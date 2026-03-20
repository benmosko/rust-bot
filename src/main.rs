mod balance;
mod config;
mod execution;
mod market_discovery;
mod market_maker;
mod momentum;
mod orderbook;
mod redemption;
mod risk;
mod sniper;
mod spread_capture;
mod spot_feed;
mod types;

use anyhow::{Context, Result};
use chrono::Utc;
use config::Config;
use dashmap::DashMap;
use polymarket_client_sdk::types::Address;
use std::collections::HashMap;
use std::str::FromStr as _;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use types::{BotState, Coin, Period, Round};

#[tokio::main]
async fn main() -> Result<()> {
    // Load config
    let config = Arc::new(Config::from_env().context("Failed to load config")?);

    // Initialize logging
    let file_appender = tracing_appender::rolling::daily(".", &config.log_file);
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    
    tracing_subscriber::fmt()
        .with_env_filter(&config.rust_log)
        .with_writer(non_blocking)
        .json()
        .init();

    info!("Starting Polymarket trading bot");

    // Load state from state.json if present
    let _loaded_state = std::fs::read_to_string("state.json")
        .ok()
        .and_then(|s| serde_json::from_str::<BotState>(&s).ok());

    // Initialize execution engine
    let execution = Arc::new(
        execution::ExecutionEngine::new(&config.private_key, config.signature_type, config.funder_address.clone())
            .await
            .context("Failed to initialize execution engine")?,
    );

    // Get funder address (where USDC lives)
    use polymarket_client_sdk::types::Address;
    let funder_address = config
        .funder_address
        .as_ref()
        .map(|s| Address::from_str(s.trim()).context("Invalid FUNDER_ADDRESS"))
        .transpose()?
        .unwrap_or_else(|| {
            // Fallback: derive from signer if no funder specified
            use polymarket_client_sdk::{auth::LocalSigner, auth::Signer};
            let signer = LocalSigner::from_str(&config.private_key).unwrap();
            signer.address()
        });

    // Initialize balance manager with watch channel
    let balance = Arc::new(balance::BalanceManager::new(
        funder_address,
        config.polygon_rpc_url.clone(),
    ));
    let balance_receiver = balance.receiver();

    // Initialize risk manager (use initial balance from watch channel)
    let start_balance = *balance_receiver.borrow();
    let risk = Arc::new(risk::RiskManager::new(config.clone(), start_balance));

    // Initialize orderbook manager
    let orderbook = Arc::new(orderbook::OrderbookManager::new());

    // Initialize redemption manager
    let redemption = Arc::new(redemption::RedemptionManager::new(
        execution.clone(),
        config.polygon_rpc_url.clone(),
        funder_address,
    ));

    // Main round management loop
    let main_shutdown = CancellationToken::new();
    
    // Spawn spot feeds for each coin
    let mut spot_feeds = HashMap::new();
    let main_shutdown_clone = main_shutdown.clone();
    for coin in &config.coins {
        let mut feed = spot_feed::SpotFeed::new(*coin);
        let receiver = feed.receiver();
        spot_feeds.insert(*coin, receiver);

        let feed_coin = *coin;
        let feed_shutdown = main_shutdown_clone.clone();
        tokio::spawn(async move {
            if let Err(e) = feed.run(feed_shutdown).await {
                error!(coin = ?feed_coin, error = %e, "Spot feed error");
            }
        });
    }

    // Spawn orderbook manager
    let orderbook_clone = orderbook.clone();
    let orderbook_shutdown = main_shutdown.clone();
    tokio::spawn(async move {
        if let Err(e) = orderbook_clone.run(orderbook_shutdown).await {
            error!(error = %e, "Orderbook manager error");
        }
    });

    // Spawn redemption manager (every 15s: check resolved markets, redeem wins, merge pairs)
    let redemption_clone = redemption.clone();
    let redemption_shutdown = main_shutdown.clone();
    let redemption_coins = config.coins.clone();
    let redemption_periods = config.periods.clone();
    tokio::spawn(async move {
        if let Err(e) = redemption_clone
            .run(redemption_shutdown, &redemption_coins, &redemption_periods)
            .await
        {
            error!(error = %e, "Redemption manager error");
        }
    });

    // Spawn balance manager (refreshes every 30s, exposes via watch channel)
    let balance_clone = balance.clone();
    let balance_shutdown = main_shutdown.clone();
    let daily_start = start_balance;
    tokio::spawn(async move {
        if let Err(e) = balance_clone.run(balance_shutdown).await {
            error!(error = %e, "Balance manager error");
        }
    });

    // Spawn P&L logger (uses balance watch channel)
    let balance_receiver_clone = balance_receiver.clone();
    let pnl_shutdown = main_shutdown.clone();
    tokio::spawn(async move {
        loop {
            if pnl_shutdown.is_cancelled() {
                break;
            }
            let current = *balance_receiver_clone.borrow();
            let pnl = current - daily_start;
            let pnl_pct = if daily_start > rust_decimal::Decimal::ZERO {
                (pnl / daily_start) * rust_decimal_macros::dec!(100)
            } else {
                rust_decimal::Decimal::ZERO
            };
            info!(
                balance = %current,
                daily_pnl = %pnl,
                daily_pnl_pct = %pnl_pct,
                "P&L snapshot"
            );
            sleep(Duration::from_secs(30)).await;
        }
    });

    // Main round management loop
    let mut active_rounds: DashMap<(Coin, Period, i64), Round> = DashMap::new();
    let mut round_tasks: HashMap<(Coin, Period, i64), Vec<tokio::task::JoinHandle<()>>> =
        HashMap::new();
    let main_shutdown_for_signal = main_shutdown.clone();

    // Setup graceful shutdown
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for ctrl+c");
        info!("Shutdown signal received");
        main_shutdown_for_signal.cancel();
    });

    info!("Entering main trading loop");

    loop {
        if main_shutdown.is_cancelled() {
            info!("Shutting down...");
            
            // Cancel all orders
            if let Err(e) = execution.cancel_all().await {
                error!(error = %e, "Failed to cancel all orders");
            }

            // Save state to state.json
            let final_balance = *balance_receiver.borrow();
            let state = BotState::new(final_balance);
            if let Ok(json) = serde_json::to_string_pretty(&state) {
                if let Err(e) = std::fs::write("state.json", json) {
                    error!(error = %e, "Failed to save state");
                } else {
                    info!("State saved to state.json");
                }
            }

            break;
        }

        let now = Utc::now().timestamp();

        // Discover and manage rounds
        for coin in &config.coins {
            for period in &config.periods {
                let period_secs = period.as_seconds();
                let round_start = (now / period_secs) * period_secs;
                let round_end = round_start + period_secs;
                let elapsed_pct = (now - round_start) as f64 / period_secs as f64;

                let round_key = (*coin, *period, round_start);

                // Check if we need to fetch this round
                if !active_rounds.contains_key(&round_key) {
                    if let Ok(market) = market_discovery::fetch_market(*coin, *period, round_start).await {
                        let round = Round {
                            market: market.clone(),
                            opening_price: None,
                            up_fee_rate_bps: None,
                            down_fee_rate_bps: None,
                        };

                        active_rounds.insert(round_key, round.clone());

                        // Subscribe to orderbooks
                        orderbook.subscribe(market.up_token_id.clone())?;
                        orderbook.subscribe(market.down_token_id.clone())?;

                        // Get opening price from spot feed (use current price as proxy if not set)
                        let spot_receiver_opt = spot_feeds.get(coin).cloned();
                        if let Some(spot_receiver) = spot_receiver_opt {
                            let spot_state = spot_receiver.borrow().clone();
                            let opening_price = spot_state.opening_price.unwrap_or(spot_state.price);

                            // Spawn spread capture (gabagool)
                            let sc_config = config.clone();
                            let sc_execution = execution.clone();
                            let sc_orderbook = orderbook.clone();
                            let sc_market = market.clone();
                            let sc_shutdown = main_shutdown.clone();
                            let sc_handle = tokio::spawn(async move {
                                let mut sc = spread_capture::SpreadCapture::new(
                                    sc_market,
                                    sc_config,
                                    sc_execution,
                                    sc_orderbook,
                                );
                                if let Err(e) = sc.run(sc_shutdown).await {
                                    error!(error = %e, "Spread capture error");
                                }
                            });

                            // Spawn momentum
                            let mom_config = config.clone();
                            let mom_execution = execution.clone();
                            let mom_orderbook = orderbook.clone();
                            let mom_spot = spot_receiver.clone();
                            let mom_market = market.clone();
                            let mom_risk = risk.clone();
                            let mom_shutdown = main_shutdown.clone();
                            let mom_handle = tokio::spawn(async move {
                                let mut mom = momentum::Momentum::new(
                                    mom_market,
                                    mom_config,
                                    mom_execution,
                                    mom_orderbook,
                                    mom_spot,
                                    mom_risk,
                                    opening_price,
                                );
                                if let Err(e) = mom.run(mom_shutdown).await {
                                    error!(error = %e, "Momentum error");
                                }
                            });

                            // Spawn market maker
                            let mm_config = config.clone();
                            let mm_execution = execution.clone();
                            let mm_orderbook = orderbook.clone();
                            let mm_spot = spot_receiver.clone();
                            let mm_market = market.clone();
                            let mm_shutdown = main_shutdown.clone();
                            let mm_handle = tokio::spawn(async move {
                                let mut mm = market_maker::MarketMaker::new(
                                    mm_market,
                                    mm_config,
                                    mm_execution,
                                    mm_orderbook,
                                    mm_spot,
                                );
                                if let Err(e) = mm.run(mm_shutdown).await {
                                    error!(error = %e, "Market maker error");
                                }
                            });

                            // Spawn sniper (late entry)
                            let sniper_config = config.clone();
                            let sniper_execution = execution.clone();
                            let sniper_orderbook = orderbook.clone();
                            let sniper_spot = spot_receiver.clone();
                            let sniper_market = market.clone();
                            let sniper_risk = risk.clone();
                            let sniper_shutdown = main_shutdown.clone();
                            let sniper_handle = tokio::spawn(async move {
                                let mut sniper = sniper::Sniper::new(
                                    sniper_market,
                                    sniper_config,
                                    sniper_execution,
                                    sniper_orderbook,
                                    sniper_spot,
                                    sniper_risk,
                                    opening_price,
                                );
                                if let Err(e) = sniper.run(sniper_shutdown).await {
                                    error!(error = %e, "Sniper error");
                                }
                            });

                            round_tasks.insert(
                                round_key,
                                vec![sc_handle, mom_handle, mm_handle, sniper_handle],
                            );
                        }
                    }
                }

                // Pre-fetch next round if < 30s remaining
                if round_end - now < 30 {
                    let next_round_start = round_start + period_secs;
                    let next_key = (*coin, *period, next_round_start);
                    if !active_rounds.contains_key(&next_key) {
                        if let Ok(next_market) =
                            market_discovery::fetch_market(*coin, *period, next_round_start).await
                        {
                            let next_round = Round {
                                market: next_market,
                                opening_price: None,
                                up_fee_rate_bps: None,
                                down_fee_rate_bps: None,
                            };
                            active_rounds.insert(next_key, next_round);
                        }
                    }
                }

                // Clean up expired rounds
                if now > round_end + 60 {
                    active_rounds.remove(&round_key);
                    if let Some(handles) = round_tasks.remove(&round_key) {
                        for handle in handles {
                            handle.abort();
                        }
                    }
                }
            }
        }

        sleep(Duration::from_millis(500)).await;
    }

    Ok(())
}
