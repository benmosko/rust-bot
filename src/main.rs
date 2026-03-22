use anyhow::{Context, Result};
use chrono::Utc;
use futures_util::stream::{self, StreamExt};
use polymarket_bot::config::Config;
use dashmap::{DashMap, DashSet};
use std::collections::HashMap;
use std::str::FromStr as _;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use polymarket_bot::orderbook::OrderbookManager;
use polymarket_bot::sniper::clear_sniper_pending_for_market;
use polymarket_bot::types::{
    BotState, Coin, Market, Period, Round, RoundHistoryEntry, SniperEntryKey, TuiEvent,
    strategy_status_key,
};
use polymarket_bot::telegram_webapp::{abbrev_strategy_line, format_round_time_range_utc, LiveRoundRow};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

/// YES/NO for TUI: **same field the sniper uses** — live CLOB `best_ask` only (what you pay to buy each outcome).
/// Do not fall back to bid or Gamma: bids/Gamma can look like a "fine" book (e.g. $0.01 / $0.99) while the
/// sniper correctly stays on `Snp: Waiting` because asks are missing or fail the ask-sum sanity check.
fn round_yes_no_display_prices(orderbook: &OrderbookManager, market: &Market) -> (Decimal, Decimal) {
    let yes_ob = orderbook.get_orderbook(&market.up_token_id);
    let no_ob = orderbook.get_orderbook(&market.down_token_id);
    let yes = yes_ob
        .as_ref()
        .and_then(|o| o.best_ask())
        .unwrap_or(Decimal::ZERO);
    let no = no_ob
        .as_ref()
        .and_then(|o| o.best_ask())
        .unwrap_or(Decimal::ZERO);
    (yes, no)
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Load config
    let config = Arc::new(Config::from_env().context("Failed to load config")?);

    let market_slots: Vec<(Coin, Period)> = config
        .coins
        .iter()
        .flat_map(|c| config.periods.iter().map(move |p| (*c, *p)))
        .collect();

    // Create dry_run flag as Arc<AtomicBool>
    let dry_run = Arc::new(AtomicBool::new(config.dry_run));
    // Telegram `/pause` sets this; new orders are blocked in the execution engine.
    let trading_paused = Arc::new(AtomicBool::new(false));

    // Check if we're in debug mode (RUST_LOG=debug)
    let rust_log_env = std::env::var("RUST_LOG").unwrap_or_else(|_| config.rust_log.clone());
    let is_debug_mode = rust_log_env.to_lowercase() == "debug";
    // Headless (e.g. supervisor child with stdout/stderr nul): skip ratatui — TUI init fails without a TTY.
    let disable_tui = std::env::var("DISABLE_TUI")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let skip_tui = is_debug_mode || disable_tui;

    // TUI event channel (dashboard consumes events) — only when we render the TUI.
    // Trade Log lines come only from explicit TuiEvent::TradeLog (strategies), not from tracing.
    let tui_tx_rx = if !skip_tui {
        // Large buffer: RoundUpdate bursts + trade log; try_send drops on full and prices go stale
        let (tx, rx) = mpsc::channel(4096);
        Some((tx, rx))
    } else {
        None
    };
    let main_shutdown = CancellationToken::new();

    // Session id (UTC) — one log file + strategy DB session column per run.
    let session_id = Utc::now().format("%Y-%m-%dT%H-%M-%S").to_string();
    let strategy_logger = Arc::new(Mutex::new(
        polymarket_bot::strategy_log::StrategyLogger::new("logs/strategy.db", &session_id)
            .context("Failed to open strategy DB")?,
    ));

    // Initialize logging: per-session file under logs/ + stdout (debug). TUI mode: file only (no tracing → Trade Log).
    std::fs::create_dir_all("logs").context("Failed to create logs directory")?;
    let log_path = format!("logs/polymarket-bot.{}.log", session_id);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed to open session log file {}", log_path))?;
    let (non_blocking, _log_guard) = tracing_appender::non_blocking(log_file);

    // Use env filter to suppress DEBUG logs from rustls/hyper/h2/reqwest/alloy
    // Only log our bot's INFO+ messages
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("polymarket_bot=info,warn"));

    if is_debug_mode {
        // Debug mode: skip TUI, use stdout for logs
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .with_target(true)
                    .with_level(true),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::stdout)
                    .with_ansi(true)
                    .with_target(true)
                    .with_level(true),
            )
            .init();
    } else {
        // Normal mode: TUI — logs go to file only; dashboard Trade Log uses TuiEvent::TradeLog only.
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .with_target(true)
                    .with_level(true),
            )
            .init();
    }

    info!("Starting Polymarket trading bot");
    info!(
        sniper_entry_window_5m_secs = config.sniper_entry_window_5m,
        sniper_entry_window_15m_secs = config.sniper_entry_window_15m,
        "Sniper entry windows: rounds ≤5m long use SNIPER_ENTRY_WINDOW_5M; longer rounds use SNIPER_ENTRY_WINDOW_15M (threshold SNIPER_ENTRY_MIN_BEST_ASK unchanged)"
    );

    // Load state from state.json if present
    let _loaded_state = std::fs::read_to_string("state.json")
        .ok()
        .and_then(|s| serde_json::from_str::<BotState>(&s).ok());

    // Initialize execution engine
    let execution = Arc::new(
        polymarket_bot::execution::ExecutionEngine::new(
            &config.private_key,
            config.signature_type,
            config.funder_address.clone(),
            dry_run.clone(),
            trading_paused.clone(),
        )
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
    
    // Safety logging on startup when live trading
    if !dry_run.load(Ordering::Relaxed) {
        use polymarket_bot::config::StrategyMode;
        let strategy_str = match config.strategy_mode {
            StrategyMode::SniperOnly => "sniper_only",
            StrategyMode::GabagoolOnly => "gabagool_only",
            StrategyMode::All => "all",
        };
        warn!(
            balance = %start_balance,
            strategy = %strategy_str,
            "LIVE TRADING ENABLED — real orders will be placed"
        );
    }
    
    // Initialize risk manager (use initial balance from watch channel)
    let risk = Arc::new(polymarket_bot::risk::RiskManager::new(config.clone(), start_balance));
    let session_start = Utc::now();
    let session_start_instant = std::time::Instant::now();

    // Initialize orderbook manager
    let orderbook = Arc::new(polymarket_bot::orderbook::OrderbookManager::new(
        config.sniper_entry_min_best_ask,
    ));

    let sc_active_markets = Arc::new(DashSet::new());

    let round_history: Arc<std::sync::Mutex<Vec<RoundHistoryEntry>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    // Spawn TUI (when user presses q, TUI exits and we cancel main_shutdown)
    // Skip TUI in debug mode or when DISABLE_TUI=1 (headless)
    let tui_tx = if let Some((tx, rx)) = tui_tx_rx {
        let main_shutdown_for_tui = main_shutdown.clone();
        let dry_run_for_tui = dry_run.clone();
        let slots_for_tui = market_slots.clone();
        let round_history_for_tui = round_history.clone();
        let session_start_instant_for_tui = session_start_instant;
        tokio::spawn(async move {
            if let Err(e) = polymarket_bot::tui::run_tui(
                rx,
                main_shutdown_for_tui.clone(),
                dry_run_for_tui,
                slots_for_tui,
                round_history_for_tui,
                session_start_instant_for_tui,
            )
            .await
            {
                error!(error = %e, "TUI error");
            }
            main_shutdown_for_tui.cancel();
        });
        Some(tx)
    } else {
        None
    };

    // Initialize P&L manager (before redemption manager and strategies)
    let pnl_manager = Arc::new(polymarket_bot::pnl::PnLManager::new(
        tui_tx.clone(),
        Some(balance.clone()),
    ));

    // Initialize redemption manager (after TUI tx is created)
    let redemption_tui_tx = tui_tx.clone();
    let redemption_pnl_manager = pnl_manager.clone();
    let redemption = Arc::new(polymarket_bot::redemption::RedemptionManager::new(
        execution.clone(),
        config.polygon_rpc_url.clone(),
        funder_address,
        dry_run.clone(),
        redemption_tui_tx,
        redemption_pnl_manager,
    ));

    // Spawn balance → TUI updater (BalanceUpdate, PnlUpdate, SessionStats)
    // Skip in debug mode
    if let Some(ref tui_tx_ref) = tui_tx {
        let balance_receiver_tui = balance_receiver.clone();
        let tui_tx_balance = tui_tx_ref.clone();
        let shutdown_balance = main_shutdown.clone();
        let pnl_manager_balance = pnl_manager.clone();
        let round_history_balance = round_history.clone();
        tokio::spawn(async move {
        let daily_start = start_balance;
        let mut balance_rx = balance_receiver_tui.clone();
        while !shutdown_balance.is_cancelled() {
            let current = *balance_rx.borrow();

            let rh = round_history_balance
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            let session_pnl = RoundHistoryEntry::sum_session_pnl(&rh);

            let (invested, _realized_pnl, open_pos, _, _) = pnl_manager_balance.get_state();

            let _ = tui_tx_balance.try_send(TuiEvent::BalanceUpdate(current));
            let _ = tui_tx_balance.try_send(TuiEvent::PnlUpdate(session_pnl));
            let _ = tui_tx_balance.try_send(TuiEvent::InvestedUpdate(invested));
            let _ = tui_tx_balance.try_send(TuiEvent::OpenPosUpdate(open_pos));
            let _ = tui_tx_balance.try_send(TuiEvent::SessionStats {
                rounds: rh.len() as u64,
                uptime_secs: (Utc::now() - session_start).num_seconds().max(0) as u64,
                avg_pair_cost: None,
                rebates: rust_decimal::Decimal::ZERO,
                session_start_balance: daily_start,
            });

            // Wake immediately when on-chain balance refreshes; also tick every 5s for stats drift
            tokio::select! {
                _ = shutdown_balance.cancelled() => break,
                _ = balance_rx.changed() => continue,
                _ = sleep(Duration::from_secs(5)) => continue,
            }
        }
        });
    }

    // Spawn spot feeds for each coin
    let binance_spot_history = polymarket_bot::spot_feed::new_shared_binance_spot_history();
    let mut spot_feeds = HashMap::new();
    let main_shutdown_clone = main_shutdown.clone();
    for coin in &config.coins {
        let mut feed =
            polymarket_bot::spot_feed::SpotFeed::new(*coin, binance_spot_history.clone());
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

    let spot_feeds_arc = Arc::new(spot_feeds.clone());

    // Polymarket RTDS Chainlink feed (settlement-aligned prices for round resolution)
    let chainlink_tracker = polymarket_bot::rtds_chainlink::ChainlinkTracker::new();
    {
        let cl = chainlink_tracker.clone();
        let rtds_shutdown = main_shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) =
                polymarket_bot::rtds_chainlink::run_rtds_chainlink_feed(cl, rtds_shutdown).await
            {
                error!(error = %e, "RTDS Chainlink feed exited");
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

    // Spawn redemption manager (every 3s, poly_claim flow: balance filter + multicall then single, max 5 retries)
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

    // Spawn balance manager (refreshes every 10s + on fill/resolve via PnLManager)
    let balance_clone = balance.clone();
    let balance_shutdown = main_shutdown.clone();
    let daily_start = start_balance;
    tokio::spawn(async move {
        if let Err(e) = balance_clone.run(balance_shutdown).await {
            error!(error = %e, "Balance manager error");
        }
    });

    // Spawn P&L logger (event-driven session P&L from round history, same as TUI header)
    let balance_receiver_clone = balance_receiver.clone();
    let pnl_shutdown = main_shutdown.clone();
    let round_history_pnl_log = round_history.clone();
    tokio::spawn(async move {
        loop {
            if pnl_shutdown.is_cancelled() {
                break;
            }
            let current = *balance_receiver_clone.borrow();
            let rh = round_history_pnl_log
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            let pnl = RoundHistoryEntry::sum_session_pnl(&rh);
            let pnl_pct = if daily_start > rust_decimal::Decimal::ZERO {
                (pnl / daily_start) * rust_decimal_macros::dec!(100)
            } else {
                rust_decimal::Decimal::ZERO
            };
            info!(
                balance = %current,
                session_pnl = %pnl,
                session_pnl_pct = %pnl_pct,
                "P&L snapshot"
            );
            sleep(Duration::from_secs(30)).await;
        }
    });

    // Main round management loop
    let active_rounds: DashMap<(Coin, Period, i64), Round> = DashMap::new();
    // Next-round markets pre-fetched in the last 30s — NOT inserted into active_rounds until
    // the round goes live, so we always run orderbook init + subscribe + strategy spawn once.
    let prefetch_buffer: DashMap<(Coin, Period, i64), Market> = DashMap::new();
    let mut round_tasks: HashMap<(Coin, Period, i64), Vec<tokio::task::JoinHandle<()>>> =
        HashMap::new();
    let main_shutdown_for_signal = main_shutdown.clone();
    let orderbook_for_tui = orderbook.clone();
    let tui_tx_rounds = tui_tx.clone();
    
    // Shared strategy status map for TUI
    let strategy_status: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let strategy_status_for_tui = strategy_status.clone();

    // Primary sniper fill count per market `(coin, period, round_start)` — each coin has its own cap (`SNIPER_MAX_FILLS_PER_ROUND`).
    let gtc_filled_by_round: Arc<DashMap<(Coin, Period, i64), Arc<AtomicU32>>> = Arc::new(DashMap::new());
    // order_id -> (execution, slug, period, round_start) for cross-market cancel on fill cap
    let active_gtc_orders: Arc<
        DashMap<String, (Arc<polymarket_bot::execution::ExecutionEngine>, String, Period, i64)>,
    > = Arc::new(DashMap::new());
    // Sniper: block duplicate legs per round before async placement completes (shared across tasks).
    let pending_sniper_entries: Arc<DashSet<SniperEntryKey>> = Arc::new(DashSet::new());
    
    // Initialize shared trade history for P&L tracking
    use polymarket_bot::types::FilledTrade;
    let trade_history: Arc<DashMap<String, FilledTrade>> = Arc::new(DashMap::new());

    // Setup graceful shutdown
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for ctrl+c");
        info!("Shutdown signal received");
        main_shutdown_for_signal.cancel();
    });

    // 5-second countdown before entering main loop when live trading
    if !dry_run.load(Ordering::Relaxed) {
        for i in (1..=5).rev() {
            warn!("Starting live trading in {}...", i);
            sleep(Duration::from_secs(1)).await;
        }
    }

    info!("Entering main trading loop");

    let gamma_http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(25))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("Failed to build Gamma HTTP client")?;

    let strategy_mode_str = match config.strategy_mode {
        polymarket_bot::config::StrategyMode::SniperOnly => "sniper_only",
        polymarket_bot::config::StrategyMode::GabagoolOnly => "gabagool_only",
        polymarket_bot::config::StrategyMode::All => "all",
    };

    let live_dashboard_rounds: Arc<Mutex<Vec<polymarket_bot::telegram_webapp::LiveRoundRow>>> =
        Arc::new(Mutex::new(Vec::new()));

    let telegram_dashboard = polymarket_bot::telegram::TelegramDashboardState::default();
    let telegram = Arc::new(polymarket_bot::telegram::TelegramBot::new(
        telegram_dashboard.clone(),
    ));
    if let Ok(bind_s) = std::env::var("TELEGRAM_WEBAPP_BIND") {
        match bind_s.parse::<std::net::SocketAddr>() {
            Ok(addr) => {
                let (tg_token, tg_chat) = polymarket_bot::telegram::telegram_auth_from_env();
                if tg_token.trim().is_empty() {
                    warn!("TELEGRAM_WEBAPP_BIND set but TELEGRAM_BOT_TOKEN is empty; Mini App HTTP not started");
                } else {
                    polymarket_bot::telegram_webapp::spawn_http_server(
                        addr,
                        tg_token,
                        tg_chat,
                        round_history.clone(),
                        trading_paused.clone(),
                        dry_run.clone(),
                        balance.clone(),
                        strategy_status.clone(),
                        live_dashboard_rounds.clone(),
                        session_start,
                        start_balance,
                        strategy_mode_str.to_string(),
                    );
                }
            }
            Err(_) => warn!(bind = %bind_s, "TELEGRAM_WEBAPP_BIND invalid; Mini App HTTP not started"),
        }
    }
    // Supervisor-spawned child: never poll Telegram here (only one getUpdates per bot token).
    if std::env::var("POLYBOT_SUPERVISOR_CHILD")
        .map(|s| s == "1")
        .unwrap_or(false)
    {
        std::env::set_var("DISABLE_TELEGRAM_POLLING", "true");
    }
    // Default: polling enabled for standalone. Supervisor sets DISABLE_TELEGRAM_POLLING=true via POLYBOT_SUPERVISOR_CHILD.
    let telegram_polling_disabled = match std::env::var("DISABLE_TELEGRAM_POLLING") {
        Ok(s) => {
            let s = s.trim().to_ascii_lowercase();
            !matches!(s.as_str(), "false" | "0" | "no" | "off")
        }
        Err(_) => false,
    };
    if telegram_polling_disabled {
        info!("Telegram polling disabled (supervisor mode)");
    } else {
        info!("Telegram polling enabled (standalone mode)");
        telegram.start_polling(
            config.polygon_rpc_url.clone(),
            "logs/strategy.db".to_string(),
            session_id.clone(),
            round_history.clone(),
            trading_paused.clone(),
            dry_run.clone(),
            execution.clone(),
            strategy_mode_str.to_string(),
            session_start,
        );
    }
    telegram.send_startup_message(&session_id);

    loop {
        if main_shutdown.is_cancelled() {
            info!("Shutting down...");

            if let Ok(g) = strategy_logger.lock() {
                if let Err(e) = g.flush() {
                    error!(error = %e, "Strategy DB flush failed");
                }
            }
            
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

        // Phase 2: parallel Gamma discovery FIRST (snipers + orderbooks before TUI/dashboard pass)
        let mut fetch_jobs: Vec<((Coin, Period, i64), i64)> = Vec::new();
        for &(coin, period) in &market_slots {
            let period_secs = period.as_seconds();
            let round_start = (now / period_secs) * period_secs;
            let round_key = (coin, period, round_start);
            if !active_rounds.contains_key(&round_key) {
                fetch_jobs.push((round_key, round_start));
            }
        }

        if !fetch_jobs.is_empty() {
            let fetch_concurrency = fetch_jobs.len().max(1);
            let http = gamma_http.clone();
            let pb = prefetch_buffer.clone();
            let results: Vec<_> = stream::iter(fetch_jobs)
                .map(|(round_key, round_start)| {
                    let http = http.clone();
                    let pb = pb.clone();
                    async move {
                        let (coin, period, _) = round_key;
                        let slug =
                            polymarket_bot::market_discovery::generate_slug(coin, period, round_start);
                        let fetch_result = if let Some((_, m)) = pb.remove(&round_key) {
                            info!(
                                coin = ?coin,
                                period = ?period,
                                round_start = round_start,
                                slug = %slug,
                                "Using prefetched market (wiring orderbook + strategies)"
                            );
                            Ok(m)
                        } else {
                            polymarket_bot::market_discovery::fetch_market(&http, coin, period, round_start)
                                .await
                        };
                        (round_key, slug, fetch_result)
                    }
                })
                .buffer_unordered(fetch_concurrency)
                .collect()
                .await;

            for (round_key, slug, fetch_result) in results {
                let (coin, period, round_start) = round_key;
                match fetch_result {
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

                        let spot_receiver_opt = spot_feeds.get(&coin).cloned();
                        let opening_price_for_chainlink = spot_receiver_opt.as_ref().and_then(|rx| {
                            let spot_state = rx.borrow();
                            spot_state.opening_price.or_else(|| {
                                if spot_state.price > Decimal::ZERO {
                                    Some(spot_state.price)
                                } else {
                                    None
                                }
                            })
                        })
                        .or(market.opening_price);

                        chainlink_tracker.register_round(
                            coin,
                            period,
                            round_start,
                            market.round_end,
                            opening_price_for_chainlink,
                        );

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

                        orderbook.subscribe(market.up_token_id.clone())?;
                        orderbook.subscribe(market.down_token_id.clone())?;

                        if let Some(spot_receiver) = spot_receiver_opt {
                            let spot_state = spot_receiver.borrow().clone();
                            let opening_price = spot_state.opening_price.unwrap_or(spot_state.price);

                            let mut task_handles = Vec::new();

                            use polymarket_bot::config::StrategyMode;
                            match config.strategy_mode {
                                StrategyMode::SniperOnly => {
                                    let sniper_config = config.clone();
                                    let sniper_execution = execution.clone();
                                    let sniper_orderbook = orderbook.clone();
                                    let sniper_spot = spot_receiver.clone();
                                    let sniper_market = market.clone();
                                    let sniper_risk = risk.clone();
                                    let sniper_shutdown = main_shutdown.clone();
                                    let sniper_status = strategy_status.clone();
                                    let sniper_balance_receiver = balance_receiver.clone();
                                    let sniper_gtc_filled_count = gtc_filled_by_round
                                        .entry((coin, period, round_start))
                                        .or_insert_with(|| Arc::new(AtomicU32::new(0)))
                                        .clone();
                                    let sniper_active_gtc_orders = active_gtc_orders.clone();
                                    let sniper_tui_tx = tui_tx.clone();
                                    let sniper_trade_history = trade_history.clone();
                                    let sniper_round_history = round_history.clone();
                                    let sniper_pnl_manager = pnl_manager.clone();
                                    let sniper_pending_entries = pending_sniper_entries.clone();
                                    let sniper_strategy_logger = strategy_logger.clone();
                                    let sniper_chainlink = chainlink_tracker.clone();
                                    let sniper_binance_history = binance_spot_history.clone();
                                    let sniper_momentum_blocks =
                                        telegram_dashboard.momentum_blocks.clone();
                                    let sniper_last_momentum_block =
                                        telegram_dashboard.last_momentum_block.clone();
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
                                            sniper_gtc_filled_count,
                                            sniper_active_gtc_orders,
                                            sniper_tui_tx,
                                            sniper_trade_history,
                                            sniper_round_history,
                                            sniper_pnl_manager,
                                            sniper_pending_entries,
                                            sniper_strategy_logger,
                                            sniper_chainlink,
                                            sniper_binance_history,
                                            sniper_momentum_blocks,
                                            sniper_last_momentum_block,
                                        );
                                        if let Err(e) = sniper.run(sniper_shutdown).await {
                                            error!(error = %e, "Sniper error");
                                        }
                                    });
                                    task_handles.push(sniper_handle);
                                }
                                StrategyMode::GabagoolOnly => {
                                    let run_sc = !config.sc_prefer_15m
                                        || period == polymarket_bot::types::Period::Fifteen;
                                    if run_sc {
                                        let sc_config = config.clone();
                                        let sc_execution = execution.clone();
                                        let sc_orderbook = orderbook.clone();
                                        let sc_market = market.clone();
                                        let sc_shutdown = main_shutdown.clone();
                                        let sc_status = strategy_status.clone();
                                        let sc_balance = balance_receiver.clone();
                                        let sc_pnl = pnl_manager.clone();
                                        let sc_trades = trade_history.clone();
                                        let sc_tui = tui_tx.clone();
                                        let sc_am = sc_active_markets.clone();
                                        let sc_handle = tokio::spawn(async move {
                                            let mut sc = polymarket_bot::spread_capture::SpreadCapture::new(
                                                sc_market,
                                                sc_config,
                                                sc_execution,
                                                sc_orderbook,
                                                sc_status,
                                                sc_balance,
                                                sc_pnl,
                                                sc_trades,
                                                sc_tui,
                                                sc_am,
                                            );
                                            if let Err(e) = sc.run(sc_shutdown).await {
                                                error!(error = %e, "Spread capture error");
                                            }
                                        });
                                        task_handles.push(sc_handle);
                                    } else {
                                        info!(
                                            coin = ?coin,
                                            period = ?period,
                                            "Spread capture skipped (SC_PREFER_15M — 5m reserved for sniper)"
                                        );
                                    }
                                }
                                StrategyMode::All => {
                                    let run_sc = !config.sc_prefer_15m
                                        || period == polymarket_bot::types::Period::Fifteen;
                                    if run_sc {
                                        let sc_config = config.clone();
                                        let sc_execution = execution.clone();
                                        let sc_orderbook = orderbook.clone();
                                        let sc_market = market.clone();
                                        let sc_shutdown = main_shutdown.clone();
                                        let sc_status = strategy_status.clone();
                                        let sc_balance = balance_receiver.clone();
                                        let sc_pnl = pnl_manager.clone();
                                        let sc_trades = trade_history.clone();
                                        let sc_tui = tui_tx.clone();
                                        let sc_am = sc_active_markets.clone();
                                        let sc_handle = tokio::spawn(async move {
                                            let mut sc = polymarket_bot::spread_capture::SpreadCapture::new(
                                                sc_market,
                                                sc_config,
                                                sc_execution,
                                                sc_orderbook,
                                                sc_status,
                                                sc_balance,
                                                sc_pnl,
                                                sc_trades,
                                                sc_tui,
                                                sc_am,
                                            );
                                            if let Err(e) = sc.run(sc_shutdown).await {
                                                error!(error = %e, "Spread capture error");
                                            }
                                        });
                                        task_handles.push(sc_handle);
                                    } else {
                                        info!(
                                            coin = ?coin,
                                            period = ?period,
                                            "Spread capture skipped (SC_PREFER_15M — 5m reserved for sniper)"
                                        );
                                    }

                                    let mom_config = config.clone();
                                    let mom_execution = execution.clone();
                                    let mom_orderbook = orderbook.clone();
                                    let mom_spot = spot_receiver.clone();
                                    let mom_market = market.clone();
                                    let mom_risk = risk.clone();
                                    let mom_shutdown = main_shutdown.clone();
                                    let mom_status = strategy_status.clone();
                                    let mom_balance = balance_receiver.clone();
                                    let mom_pnl = pnl_manager.clone();
                                    let mom_trades = trade_history.clone();
                                    let mom_tui = tui_tx.clone();
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
                                            mom_balance,
                                            mom_pnl,
                                            mom_trades,
                                            mom_tui,
                                        );
                                        if let Err(e) = mom.run(mom_shutdown).await {
                                            error!(error = %e, "Momentum error");
                                        }
                                    });
                                    task_handles.push(mom_handle);

                                    let mm_config = config.clone();
                                    let mm_execution = execution.clone();
                                    let mm_orderbook = orderbook.clone();
                                    let mm_spot = spot_receiver.clone();
                                    let mm_market = market.clone();
                                    let mm_shutdown = main_shutdown.clone();
                                    let mm_status = strategy_status.clone();
                                    let mm_balance = balance_receiver.clone();
                                    let mm_pnl = pnl_manager.clone();
                                    let mm_trades = trade_history.clone();
                                    let mm_tui = tui_tx.clone();
                                    let mm_handle = tokio::spawn(async move {
                                        let mut mm = polymarket_bot::market_maker::MarketMaker::new(
                                            mm_market,
                                            mm_config,
                                            mm_execution,
                                            mm_orderbook,
                                            mm_spot,
                                            mm_status,
                                            mm_balance,
                                            mm_pnl,
                                            mm_trades,
                                            mm_tui,
                                        );
                                        if let Err(e) = mm.run(mm_shutdown).await {
                                            error!(error = %e, "Market maker error");
                                        }
                                    });
                                    task_handles.push(mm_handle);

                                    let sniper_config = config.clone();
                                    let sniper_execution = execution.clone();
                                    let sniper_orderbook = orderbook.clone();
                                    let sniper_spot = spot_receiver.clone();
                                    let sniper_market = market.clone();
                                    let sniper_risk = risk.clone();
                                    let sniper_shutdown = main_shutdown.clone();
                                    let sniper_status = strategy_status.clone();
                                    let sniper_balance_receiver = balance_receiver.clone();
                                    let sniper_gtc_filled_count = gtc_filled_by_round
                                        .entry((coin, period, round_start))
                                        .or_insert_with(|| Arc::new(AtomicU32::new(0)))
                                        .clone();
                                    let sniper_active_gtc_orders = active_gtc_orders.clone();
                                    let sniper_tui_tx = tui_tx.clone();
                                    let sniper_trade_history = trade_history.clone();
                                    let sniper_round_history = round_history.clone();
                                    let sniper_pnl_manager = pnl_manager.clone();
                                    let sniper_pending_entries = pending_sniper_entries.clone();
                                    let sniper_strategy_logger = strategy_logger.clone();
                                    let sniper_chainlink = chainlink_tracker.clone();
                                    let sniper_binance_history = binance_spot_history.clone();
                                    let sniper_momentum_blocks =
                                        telegram_dashboard.momentum_blocks.clone();
                                    let sniper_last_momentum_block =
                                        telegram_dashboard.last_momentum_block.clone();
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
                                            sniper_gtc_filled_count,
                                            sniper_active_gtc_orders,
                                            sniper_tui_tx,
                                            sniper_trade_history,
                                            sniper_round_history,
                                            sniper_pnl_manager,
                                            sniper_pending_entries,
                                            sniper_strategy_logger,
                                            sniper_chainlink,
                                            sniper_binance_history,
                                            sniper_momentum_blocks,
                                            sniper_last_momentum_block,
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
        }

        // Phase 1: push TUI updates for every configured slot (after discovery this iteration).
        let mut live_rows: Vec<LiveRoundRow> = Vec::with_capacity(market_slots.len());
        for &(coin, period) in &market_slots {
            let period_secs = period.as_seconds();
            let round_start = (now / period_secs) * period_secs;
            let round_end = round_start + period_secs;
            let elapsed_pct = (now - round_start) as f64 / period_secs as f64;
            let round_key = (coin, period, round_start);

            let (yes_price, no_price, strategy_str) = if let Some(round) = active_rounds.get(&round_key) {
                let (yes, no) = round_yes_no_display_prices(&orderbook_for_tui, &round.market);
                let mut active_strategy = "—".to_string();
                let strategy_order = if config.strategy_mode == polymarket_bot::config::StrategyMode::SniperOnly {
                    vec!["sniper"]
                } else {
                    vec!["spread_capture", "momentum", "market_maker", "sniper"]
                };

                for strategy_name in &strategy_order {
                    let key = strategy_status_key(coin, period, round_start, strategy_name);
                    if let Some(status) = strategy_status_for_tui.get(&key) {
                        let status_val = status.value().clone();
                        if strategy_name == &"sniper" || (status_val != "Ended" && status_val != "Waiting") {
                            if strategy_name == &"sniper" {
                                active_strategy = status_val.clone();
                            } else {
                                active_strategy = format!(
                                    "{}: {}",
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
                (rust_decimal::Decimal::ZERO, rust_decimal::Decimal::ZERO, "Waiting".to_string())
            };

            let status_str = if now >= round_end {
                "Ended".to_string()
            } else if active_rounds.contains_key(&round_key) {
                "Active".to_string()
            } else {
                "Waiting".to_string()
            };

            live_rows.push(LiveRoundRow {
                coin: coin.as_str().to_uppercase(),
                period: format!("{}m", period.as_minutes()),
                market_label: format!("{} {}m", coin.as_str().to_uppercase(), period.as_minutes()),
                round_start,
                round_end,
                round_time_range_utc: format_round_time_range_utc(round_start, round_end),
                elapsed_pct,
                yes_ask: yes_price.to_f64().unwrap_or(0.0),
                no_ask: no_price.to_f64().unwrap_or(0.0),
                strategy: abbrev_strategy_line(&strategy_str),
                status: status_str.clone(),
            });

            if let Some(ref tui_tx) = tui_tx_rounds {
                let _ = tui_tx.try_send(TuiEvent::RoundUpdate {
                    coin,
                    period,
                    round_start,
                    elapsed_pct,
                    yes_price,
                    no_price,
                    strategy: strategy_str,
                    status: status_str,
                });
            }
        }
        if let Ok(mut g) = live_dashboard_rounds.lock() {
            *g = live_rows;
        }

        // Phase 3: pre-fetch next round + expire old rounds
        for &(coin, period) in &market_slots {
            let period_secs = period.as_seconds();
            let round_start = (now / period_secs) * period_secs;
            let round_end = round_start + period_secs;

            if round_end - now < 30 {
                let next_round_start = round_start + period_secs;
                let next_key = (coin, period, next_round_start);
                let next_slug = polymarket_bot::market_discovery::generate_slug(coin, period, next_round_start);
                if !active_rounds.contains_key(&next_key) && !prefetch_buffer.contains_key(&next_key) {
                    debug!(
                        coin = ?coin,
                        period = ?period,
                        next_round_start = next_round_start,
                        slug = %next_slug,
                        "Pre-fetching next round"
                    );
                    match polymarket_bot::market_discovery::fetch_market(
                        &gamma_http,
                        coin,
                        period,
                        next_round_start,
                    )
                    .await
                    {
                        Ok(next_market) => {
                            debug!(
                                coin = ?coin,
                                period = ?period,
                                next_round_start = next_round_start,
                                slug = %next_slug,
                                "Next round pre-fetch succeeded (buffered until round is live)"
                            );
                            prefetch_buffer.insert(next_key, next_market);
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

            // Cleanup the round that ended at round_start (previous period).
            // The current round runs (round_start, round_end); the previous round ran (round_start - period_secs, round_start).
            let round_to_cleanup_start = round_start - period_secs;
            let round_key_cleanup = (coin, period, round_to_cleanup_start);
            if now > round_start + 3 {
                info!(
                    round_key = ?round_key_cleanup,
                    "Round cleanup triggered for round_key"
                );
                if let Some((_, round)) = active_rounds.remove(&round_key_cleanup) {
                    let slug = round.market.slug.clone();
                    let condition_id = round.market.condition_id.clone();

                    gtc_filled_by_round.remove(&(coin, period, round_to_cleanup_start));

                    clear_sniper_pending_for_market(
                        &pending_sniper_entries,
                        coin,
                        period,
                        round_to_cleanup_start,
                        &condition_id,
                    );

                    let order_ids: Vec<String> = active_gtc_orders
                        .iter()
                        .filter(|e| {
                            let v = e.value();
                            v.1 == slug && v.2 == period && v.3 == round_to_cleanup_start
                        })
                        .map(|e| e.key().clone())
                        .collect();
                    for oid in order_ids {
                        if let Some((_, (ex, _, _, _))) = active_gtc_orders.remove(&oid) {
                            if let Err(e) = ex.cancel_order(&oid).await {
                                warn!(
                                    order_id = %oid,
                                    market = %slug,
                                    error = %e,
                                    "Failed to cancel GTC on round expiry"
                                );
                            }
                        }
                    }

                    let open_count = round_history
                        .lock()
                        .map(|g| {
                            g.iter()
                                .filter(|e| {
                                    e.condition_id == condition_id
                                        && e.round_start == round_to_cleanup_start
                                        && e.status == polymarket_bot::types::RoundHistoryStatus::Open
                                })
                                .count()
                        })
                        .unwrap_or(0);
                    info!(
                        condition_id = %condition_id,
                        round_start = round_to_cleanup_start,
                        open_entries = open_count,
                        "OPEN round_history entries for resolution (Gamma official settlement)"
                    );
                    info!(
                        condition_id = %condition_id,
                        "Spawning round-history resolution task"
                    );
                    let rh_res = round_history.clone();
                    let cid_res = condition_id.clone();
                    let slug_res = slug.clone();
                    let http_res = gamma_http.clone();
                    let shutdown_res = main_shutdown.clone();
                    let round_start_res = round_to_cleanup_start;
                    let tracker_res = chainlink_tracker.clone();
                    let coin_res = coin;
                    let period_res = period;
                    let strategy_logger_res = strategy_logger.clone();
                    let spot_feeds_res = spot_feeds_arc.clone();
                    let telegram_res = telegram.clone();
                    tokio::spawn(async move {
                        polymarket_bot::market_discovery::resolve_round_history_open_entries_chainlink_or_gamma(
                            Some(tracker_res),
                            &http_res,
                            &rh_res,
                            coin_res,
                            period_res,
                            &cid_res,
                            &slug_res,
                            round_start_res,
                            shutdown_res,
                            Some(strategy_logger_res),
                            Some(spot_feeds_res),
                            telegram_res,
                        )
                        .await;
                    });

                    pnl_manager.clear_round_positions_for_condition(&condition_id);
                }
                if let Some(handles) = round_tasks.remove(&round_key_cleanup) {
                    for handle in handles {
                        handle.abort();
                    }
                }
            }
        }

        sleep(Duration::from_millis(500)).await;
    }

    Ok(())
}
