use anyhow::{Context, Result};
use chrono::Utc;
use polymarket_bot::config::Config;
use dashmap::DashMap;
use std::collections::HashMap;
use std::str::FromStr as _;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use polymarket_bot::types::{BotState, Coin, Period, Round, TuiEvent, strategy_status_key};

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Load config
    let config = Arc::new(Config::from_env().context("Failed to load config")?);

    // Create dry_run flag as Arc<AtomicBool>
    let dry_run = Arc::new(AtomicBool::new(config.dry_run));

    // Check if we're in debug mode (RUST_LOG=debug)
    let rust_log_env = std::env::var("RUST_LOG").unwrap_or_else(|_| config.rust_log.clone());
    let is_debug_mode = rust_log_env.to_lowercase() == "debug";

    // TUI event channel (dashboard consumes events) - only needed if not in debug mode
    let tui_tx_rx = if !is_debug_mode {
        Some(mpsc::channel(256))
    } else {
        None
    };
    let main_shutdown = CancellationToken::new();

    // Initialize logging: file (JSON) + stdout (debug mode) or file only (TUI mode)
    let file_appender = tracing_appender::rolling::daily(".", &config.log_file);
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.rust_log));
    
    if is_debug_mode {
        // Debug mode: skip TUI, use stdout for logs
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().with_writer(non_blocking).json())
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::stdout)
                    .with_ansi(true)
                    .with_target(true)
                    .with_level(true),
            )
            .init();
    } else {
        // Normal mode: TUI enabled - route all tracing to file to avoid interfering with ratatui alternate screen
        let log_file = std::fs::File::create("polymarket-bot.log")
            .context("Failed to create log file")?;
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().with_writer(non_blocking).json())
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(log_file)
                    .with_ansi(false)
                    .with_target(true)
                    .with_level(true),
            )
            .init();
    }

    info!("Starting Polymarket trading bot");

    // Load state from state.json if present
    let _loaded_state = std::fs::read_to_string("state.json")
        .ok()
        .and_then(|s| serde_json::from_str::<BotState>(&s).ok());

    // Initialize execution engine
    let execution = Arc::new(
        polymarket_bot::execution::ExecutionEngine::new(&config.private_key, config.signature_type, config.funder_address.clone(), dry_run.clone())
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
            use polymarket_client_sdk::auth::LocalSigner;
            let signer = LocalSigner::from_str(&config.private_key).unwrap();
            signer.address()
        });

    // Initialize balance manager with watch channel
    let balance = Arc::new(polymarket_bot::balance::BalanceManager::new(
        funder_address,
        config.polygon_rpc_url.clone(),
    ));
    let balance_receiver = balance.receiver();

    // Refresh balance synchronously to get starting balance
    if let Err(e) = balance.refresh_balance().await {
        error!(error = %e, "Failed to refresh initial balance");
    }
    // Wait a moment for the watch channel to update
    sleep(Duration::from_millis(100)).await;
    let start_balance = *balance_receiver.borrow();
    info!(starting_balance = %start_balance, "Captured starting balance for P&L calculation");
    
    // Initialize risk manager (use initial balance from watch channel)
    let risk = Arc::new(polymarket_bot::risk::RiskManager::new(config.clone(), start_balance));
    let session_start = Utc::now();

    // Initialize orderbook manager
    let orderbook = Arc::new(polymarket_bot::orderbook::OrderbookManager::new());

    // Initialize redemption manager
    let redemption = Arc::new(polymarket_bot::redemption::RedemptionManager::new(
        execution.clone(),
        config.polygon_rpc_url.clone(),
        funder_address,
        dry_run.clone(),
    ));

    // Spawn TUI (when user presses q, TUI exits and we cancel main_shutdown)
    // Skip TUI in debug mode
    let tui_tx = if let Some((tx, rx)) = tui_tx_rx {
        let main_shutdown_for_tui = main_shutdown.clone();
        let dry_run_for_tui = dry_run.clone();
        tokio::spawn(async move {
            if let Err(e) = polymarket_bot::tui::run_tui(rx, main_shutdown_for_tui.clone(), dry_run_for_tui).await {
                error!(error = %e, "TUI error");
            }
            main_shutdown_for_tui.cancel();
        });
        Some(tx)
    } else {
        None
    };

    // Spawn balance → TUI updater (BalanceUpdate, PnlUpdate, SessionStats)
    // Skip in debug mode
    if let Some(ref tui_tx_ref) = tui_tx {
        let balance_receiver_tui = balance_receiver.clone();
        let tui_tx_balance = tui_tx_ref.clone();
        let shutdown_balance = main_shutdown.clone();
        tokio::spawn(async move {
        let daily_start = start_balance;
        while !shutdown_balance.is_cancelled() {
            let current = *balance_receiver_tui.borrow();
            let pnl = current - daily_start;
            let pnl_pct = if daily_start > rust_decimal::Decimal::ZERO {
                (pnl / daily_start)
                    .to_string()
                    .parse::<f64>()
                    .unwrap_or(0.0)
                    * 100.0
            } else {
                0.0
            };
            let _ = tui_tx_balance.try_send(TuiEvent::BalanceUpdate(current));
            let _ = tui_tx_balance.try_send(TuiEvent::PnlUpdate(pnl));
            let _ = tui_tx_balance.try_send(TuiEvent::SessionStats {
                trades: 0,
                rounds: 0,
                win_rate_pct: 0.0,
                pnl_pct,
                uptime_secs: (Utc::now() - session_start).num_seconds().max(0) as u64,
                avg_pair_cost: None,
                rebates: rust_decimal::Decimal::ZERO,
            });
            sleep(Duration::from_secs(5)).await;
        }
        });
    }

    // Spawn spot feeds for each coin
    let mut spot_feeds = HashMap::new();
    let main_shutdown_clone = main_shutdown.clone();
    for coin in &config.coins {
        let mut feed = polymarket_bot::spot_feed::SpotFeed::new(*coin);
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
    let active_rounds: DashMap<(Coin, Period, i64), Round> = DashMap::new();
    let mut round_tasks: HashMap<(Coin, Period, i64), Vec<tokio::task::JoinHandle<()>>> =
        HashMap::new();
    let main_shutdown_for_signal = main_shutdown.clone();
    let orderbook_for_tui = orderbook.clone();
    let tui_tx_rounds = tui_tx.clone();
    
    // Shared strategy status map for TUI
    let strategy_status: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let strategy_status_for_tui = strategy_status.clone();

    // Shared sniper lock - only one sniper can trade at a time
    let sniper_lock: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

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
        debug!(timestamp = now, "Main loop tick");

        // Discover and manage rounds
        for coin in &config.coins {
            for period in &config.periods {
                let period_secs = period.as_seconds();
                let round_start = (now / period_secs) * period_secs;
                let round_end = round_start + period_secs;
                let elapsed_pct = (now - round_start) as f64 / period_secs as f64;

                let round_key = (*coin, *period, round_start);
                let slug = polymarket_bot::market_discovery::generate_slug(*coin, *period, round_start);

                debug!(
                    coin = ?coin,
                    period = ?period,
                    round_start = round_start,
                    slug = %slug,
                    "Checking market"
                );

                // Check if we need to fetch this round
                if !active_rounds.contains_key(&round_key) {
                        match polymarket_bot::market_discovery::fetch_market(*coin, *period, round_start).await {
                        Ok(market) => {
                            info!(
                                coin = ?coin,
                                period = ?period,
                                round_start = round_start,
                                slug = %slug,
                                "Market discovery succeeded"
                            );
                            let round = Round {
                                market: market.clone(),
                                opening_price: None,
                                up_fee_rate_bps: None,
                                down_fee_rate_bps: None,
                            };

                            active_rounds.insert(round_key, round.clone());

                            // Initialize orderbooks with prices from Gamma API
                            orderbook.initialize_orderbook(
                                market.up_token_id.clone(),
                                market.up_best_bid,
                                market.up_best_ask,
                            );
                            orderbook.initialize_orderbook(
                                market.down_token_id.clone(),
                                market.down_best_bid,
                                market.down_best_ask,
                            );

                            // Subscribe to orderbooks for live updates
                            orderbook.subscribe(market.up_token_id.clone())?;
                            orderbook.subscribe(market.down_token_id.clone())?;

                            // Get opening price from spot feed (use current price as proxy if not set)
                            let spot_receiver_opt = spot_feeds.get(coin).cloned();
                            if let Some(spot_receiver) = spot_receiver_opt {
                                let spot_state = spot_receiver.borrow().clone();
                                let opening_price = spot_state.opening_price.unwrap_or(spot_state.price);

                                let mut task_handles = Vec::new();

                                // Spawn strategies based on STRATEGY_MODE
                                use polymarket_bot::config::StrategyMode;
                                match config.strategy_mode {
                                    StrategyMode::SniperOnly => {
                                        // Only spawn sniper
                                        let sniper_config = config.clone();
                                        let sniper_execution = execution.clone();
                                        let sniper_orderbook = orderbook.clone();
                                        let sniper_spot = spot_receiver.clone();
                                        let sniper_market = market.clone();
                                        let sniper_risk = risk.clone();
                                        let sniper_shutdown = main_shutdown.clone();
                                        let sniper_status = strategy_status.clone();
                                        let sniper_balance_receiver = balance_receiver.clone();
                                        let sniper_lock_clone = sniper_lock.clone();
                                        let sniper_handle = tokio::spawn(async move {
                                            let mut sniper = polymarket_bot::sniper::Sniper::new(
                                                sniper_market,
                                                sniper_config,
                                                sniper_execution,
                                                sniper_orderbook,
                                                sniper_spot,
                                                sniper_risk,
                                                opening_price,
                                                sniper_status,
                                                sniper_balance_receiver,
                                                sniper_lock_clone,
                                            );
                                            if let Err(e) = sniper.run(sniper_shutdown).await {
                                                error!(error = %e, "Sniper error");
                                            }
                                        });
                                        task_handles.push(sniper_handle);
                                    }
                                    StrategyMode::GabagoolOnly => {
                                        // Only spawn spread capture (gabagool)
                                        let sc_config = config.clone();
                                        let sc_execution = execution.clone();
                                        let sc_orderbook = orderbook.clone();
                                        let sc_market = market.clone();
                                        let sc_shutdown = main_shutdown.clone();
                                        let sc_status = strategy_status.clone();
                                        let sc_handle = tokio::spawn(async move {
                                            let mut sc = polymarket_bot::spread_capture::SpreadCapture::new(
                                                sc_market,
                                                sc_config,
                                                sc_execution,
                                                sc_orderbook,
                                                sc_status,
                                            );
                                            if let Err(e) = sc.run(sc_shutdown).await {
                                                error!(error = %e, "Spread capture error");
                                            }
                                        });
                                        task_handles.push(sc_handle);
                                    }
                                    StrategyMode::All => {
                                        // Spawn all strategies
                                        // Spawn spread capture (gabagool)
                                        let sc_config = config.clone();
                                        let sc_execution = execution.clone();
                                        let sc_orderbook = orderbook.clone();
                                        let sc_market = market.clone();
                                        let sc_shutdown = main_shutdown.clone();
                                        let sc_status = strategy_status.clone();
                                        let sc_handle = tokio::spawn(async move {
                                            let mut sc = polymarket_bot::spread_capture::SpreadCapture::new(
                                                sc_market,
                                                sc_config,
                                                sc_execution,
                                                sc_orderbook,
                                                sc_status,
                                            );
                                            if let Err(e) = sc.run(sc_shutdown).await {
                                                error!(error = %e, "Spread capture error");
                                            }
                                        });
                                        task_handles.push(sc_handle);

                                        // Spawn momentum
                                        let mom_config = config.clone();
                                        let mom_execution = execution.clone();
                                        let mom_orderbook = orderbook.clone();
                                        let mom_spot = spot_receiver.clone();
                                        let mom_market = market.clone();
                                        let mom_risk = risk.clone();
                                        let mom_shutdown = main_shutdown.clone();
                                        let mom_status = strategy_status.clone();
                                        let mom_handle = tokio::spawn(async move {
                                            let mut mom = polymarket_bot::momentum::Momentum::new(
                                                mom_market,
                                                mom_config,
                                                mom_execution,
                                                mom_orderbook,
                                                mom_spot,
                                                mom_risk,
                                                opening_price,
                                                mom_status,
                                            );
                                            if let Err(e) = mom.run(mom_shutdown).await {
                                                error!(error = %e, "Momentum error");
                                            }
                                        });
                                        task_handles.push(mom_handle);

                                        // Spawn market maker
                                        let mm_config = config.clone();
                                        let mm_execution = execution.clone();
                                        let mm_orderbook = orderbook.clone();
                                        let mm_spot = spot_receiver.clone();
                                        let mm_market = market.clone();
                                        let mm_shutdown = main_shutdown.clone();
                                        let mm_status = strategy_status.clone();
                                        let mm_handle = tokio::spawn(async move {
                                            let mut mm = polymarket_bot::market_maker::MarketMaker::new(
                                                mm_market,
                                                mm_config,
                                                mm_execution,
                                                mm_orderbook,
                                                mm_spot,
                                                mm_status,
                                            );
                                            if let Err(e) = mm.run(mm_shutdown).await {
                                                error!(error = %e, "Market maker error");
                                            }
                                        });
                                        task_handles.push(mm_handle);

                                        // Spawn sniper (late entry)
                                        let sniper_config = config.clone();
                                        let sniper_execution = execution.clone();
                                        let sniper_orderbook = orderbook.clone();
                                        let sniper_spot = spot_receiver.clone();
                                        let sniper_market = market.clone();
                                        let sniper_risk = risk.clone();
                                        let sniper_shutdown = main_shutdown.clone();
                                        let sniper_status = strategy_status.clone();
                                        let sniper_balance_receiver = balance_receiver.clone();
                                        let sniper_lock_clone = sniper_lock.clone();
                                        let sniper_handle = tokio::spawn(async move {
                                            let mut sniper = polymarket_bot::sniper::Sniper::new(
                                                sniper_market,
                                                sniper_config,
                                                sniper_execution,
                                                sniper_orderbook,
                                                sniper_spot,
                                                sniper_risk,
                                                opening_price,
                                                sniper_status,
                                                sniper_balance_receiver,
                                                sniper_lock_clone,
                                            );
                                            if let Err(e) = sniper.run(sniper_shutdown).await {
                                                error!(error = %e, "Sniper error");
                                            }
                                        });
                                        task_handles.push(sniper_handle);
                                    }
                                }

                                round_tasks.insert(round_key, task_handles);
                            }
                        }
                        Err(e) => {
                            warn!(
                                coin = ?coin,
                                period = ?period,
                                round_start = round_start,
                                slug = %slug,
                                error = %e,
                                "Market discovery failed"
                            );
                        }
                    }
                }

                // Pre-fetch next round if < 30s remaining
                if round_end - now < 30 {
                    let next_round_start = round_start + period_secs;
                    let next_key = (*coin, *period, next_round_start);
                    let next_slug = polymarket_bot::market_discovery::generate_slug(*coin, *period, next_round_start);
                    if !active_rounds.contains_key(&next_key) {
                        debug!(
                            coin = ?coin,
                            period = ?period,
                            next_round_start = next_round_start,
                            slug = %next_slug,
                            "Pre-fetching next round"
                        );
                        match polymarket_bot::market_discovery::fetch_market(*coin, *period, next_round_start).await {
                            Ok(next_market) => {
                                debug!(
                                    coin = ?coin,
                                    period = ?period,
                                    next_round_start = next_round_start,
                                    slug = %next_slug,
                                    "Next round pre-fetch succeeded"
                                );
                                let next_round = Round {
                                    market: next_market,
                                    opening_price: None,
                                    up_fee_rate_bps: None,
                                    down_fee_rate_bps: None,
                                };
                                active_rounds.insert(next_key, next_round);
                            }
                            Err(e) => {
                                warn!(
                                    coin = ?coin,
                                    period = ?period,
                                    next_round_start = next_round_start,
                                    slug = %next_slug,
                                    error = %e,
                                    "Next round pre-fetch failed"
                                );
                            }
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

                // Send round update to TUI (prices from orderbook)
                // Always send update for all rounds, even if not discovered yet
                let (yes_price, no_price, strategy_str) = if let Some(round) = active_rounds.get(&round_key) {
                    let yes = orderbook_for_tui
                        .get_orderbook(&round.market.up_token_id)
                        .and_then(|ob| ob.best_bid())
                        .unwrap_or(rust_decimal::Decimal::ZERO);
                    let no = orderbook_for_tui
                        .get_orderbook(&round.market.down_token_id)
                        .and_then(|ob| ob.best_bid())
                        .unwrap_or(rust_decimal::Decimal::ZERO);
                    // Get strategy status from shared map - try all 4 strategies and show the active one
                    // For sniper_only mode, prioritize sniper status
                    let mut active_strategy = "—".to_string();
                    let strategy_order = if config.strategy_mode == polymarket_bot::config::StrategyMode::SniperOnly {
                        vec!["sniper"]
                    } else {
                        vec!["spread_capture", "momentum", "market_maker", "sniper"]
                    };
                    
                    for strategy_name in &strategy_order {
                        let key = strategy_status_key(*coin, *period, round_start, strategy_name);
                        if let Some(status) = strategy_status_for_tui.get(&key) {
                            let status_val = status.value().clone();
                            // For sniper, show all statuses (they already have "Snp: " prefix)
                            // For other strategies, skip "Waiting" and "Ended"
                            if strategy_name == &"sniper" || (status_val != "Ended" && status_val != "Waiting") {
                                if strategy_name == &"sniper" {
                                    // Sniper status already includes "Snp: " prefix
                                    active_strategy = status_val.clone();
                                } else {
                                    active_strategy = format!("{}: {}", 
                                        match *strategy_name {
                                            "spread_capture" => "Gab",
                                            "momentum" => "Mom",
                                            "market_maker" => "MM",
                                            "sniper" => "Snp",
                                            _ => "",
                                        },
                                        status_val
                                    );
                                }
                                break;
                            }
                        }
                    }
                    (yes, no, active_strategy)
                } else {
                    // Round not discovered yet - show waiting
                    (rust_decimal::Decimal::ZERO, rust_decimal::Decimal::ZERO, "Waiting".to_string())
                };
                
                if let Some(ref tui_tx) = tui_tx_rounds {
                    let _ = tui_tx.try_send(TuiEvent::RoundUpdate {
                        coin: *coin,
                        period: *period,
                        round_start,
                        elapsed_pct,
                        yes_price,
                        no_price,
                        strategy: strategy_str,
                        status: if now >= round_end { "Ended".to_string() } else if active_rounds.contains_key(&round_key) { "Active".to_string() } else { "Waiting".to_string() },
                    });
                }
            }
        }

        sleep(Duration::from_millis(500)).await;
    }

    Ok(())
}
