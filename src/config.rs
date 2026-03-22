use crate::types::{Coin, Period};
use anyhow::{Context, Result};
use rust_decimal::Decimal;
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    // Wallet
    pub private_key: String,
    pub signature_type: u8,
    pub funder_address: Option<String>,
    pub polygon_rpc_url: String,

    // Spread Capture (gabagool) — accumulate YES/NO; target yes_avg + no_avg <= max
    /// Max best_ask on the **first** buy of the round only (no inventory yet). Later buys use only pair-cost + imbalance.
    pub sc_cheap_side_max_ask: Decimal,
    /// Max allowed sum of average YES price + average NO price after a contemplated buy (when both legs exist).
    pub sc_max_pair_cost: Decimal,
    /// Prefer balanced inventory: do not buy YES if yes_qty > no_qty + this (and symmetric for NO).
    pub sc_max_qty_imbalance: Decimal,
    /// Max distinct market-rounds with spread-capture inventory at once (first-leg gate).
    pub sc_max_active_markets: u32,
    /// Shares per FAK tap (DCA size).
    pub sc_fak_order_size: Decimal,
    /// Minimum time between FAK orders on the same side (YES / NO).
    pub sc_order_cooldown_ms: u64,
    /// If true, spread capture runs only on 15m markets (5m left to sniper).
    pub sc_prefer_15m: bool,
    #[allow(dead_code)]
    pub sc_max_imbalance_pct: Decimal,

    // Momentum
    pub mom_min_spot_move_pct: Decimal,
    pub mom_late_entry_min_bid: Decimal,
    pub mom_preemptive_cancel_ms: u64,

    // Market Making
    pub mm_half_spread: Decimal,
    pub mm_volatility_spread: Decimal,
    pub mm_max_inventory_per_side: Decimal,
    pub mm_inventory_imbalance_limit: Decimal,
    pub mm_stop_before_end_secs: u64,
    /// Rolling window of spot samples (loop ticks) for volatility spread widening.
    pub mm_spot_volatility_window_ticks: usize,
    /// If (max-min)/min of spot in the window >= this, use `mm_volatility_spread`.
    pub mm_spot_move_pct_threshold: Decimal,

    // Sniper (legacy / momentum late entry)
    pub snipe_min_bid: Decimal,
    pub snipe_max_spread: Decimal,
    pub snipe_min_elapsed_pct: f64,
    #[allow(dead_code)]
    pub snipe_position_size_pct: Decimal,
    #[allow(dead_code)]
    pub snipe_max_replacements: u32,
    #[allow(dead_code)]
    pub snipe_no_crossover_secs: u64,
    /// Fraction of USDC balance to size each sniper entry (e.g. 0.01 = 1%).
    pub sniper_capital_deploy_pct: Decimal,
    /// Minimum shares per sniper order after sizing (floor then apply this floor).
    pub sniper_min_shares: Decimal,
    /// Minimum best_ask on a side before sniper may enter (event-driven threshold and entry gate).
    pub sniper_entry_min_best_ask: Decimal,
    /// Max confirmed primary sniper GTC fills **per coin** per `(period, round_start)`; then cancel rests on that market.
    pub sniper_max_fills_per_round: u32,
    /// Seconds before round end when sniper may activate for rounds ≤ 5 minutes long (see [`Config::sniper_entry_window_secs`]).
    pub sniper_entry_window_5m: u64,
    /// Seconds before round end when sniper may activate for rounds longer than 5 minutes.
    pub sniper_entry_window_15m: u64,
    /// If true, when Polymarket **best_ask** on a leg is ≥ [`sniper_entry_min_best_ask`] but Binance spot is still
    /// within a small **relative** distance of the round's Binance start (`opening_price` in sniper), skip until
    /// CEX has moved enough (avoids buying a "hot" PM leg while spot is still flat vs round open).
    pub sniper_min_distance_from_open_filter: bool,
    /// If true, block when Binance looks like it's mean-reverting back toward round open vs 10s ago.
    pub sniper_momentum_reversal_filter: bool,
    /// If true, block when Binance spot slope over ~30s trends against the entry side (YES/NO).
    pub sniper_slope_filter: bool,

    // Risk
    #[allow(dead_code)]
    pub daily_loss_limit_pct: Decimal,
    #[allow(dead_code)]
    pub circuit_breaker_consecutive_losses: u32,
    #[allow(dead_code)]
    pub circuit_breaker_pause_secs: u64,
    #[allow(dead_code)]
    pub max_round_exposure_pct: Decimal,
    #[allow(dead_code)]
    pub volatility_pause_threshold_pct: Decimal,
    #[allow(dead_code)]
    pub volatility_pause_secs: u64,

    // Markets
    pub coins: Vec<Coin>,
    pub periods: Vec<Period>,

    // Performance
    pub cancel_replace_target_ms: u64,
    pub cancel_replace_hard_limit_ms: u64,

    // Logging
    pub rust_log: String,
    pub log_file: String,

    // Strategy Mode
    pub strategy_mode: StrategyMode,

    // Dry Run
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyMode {
    SniperOnly,
    GabagoolOnly,
    All,
}

impl StrategyMode {
    pub fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "sniper_only" => Ok(StrategyMode::SniperOnly),
            "gabagool_only" => Ok(StrategyMode::GabagoolOnly),
            "all" => Ok(StrategyMode::All),
            _ => anyhow::bail!("Invalid STRATEGY_MODE: {} (must be sniper_only, gabagool_only, or all)", s),
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        // Load .env first, then polymarket_keys.env (which takes precedence)
        dotenv::dotenv().ok();
        dotenv::from_filename("polymarket_keys.env").ok();

        // Support both PK and POLYMARKET_PRIVATE_KEY
        let private_key = env::var("PK")
            .or_else(|_| env::var("POLYMARKET_PRIVATE_KEY"))
            .context("PK or POLYMARKET_PRIVATE_KEY must be set")?;

        // Support both SIG_TYPE and SIGNATURE_TYPE
        let signature_type = env::var("SIG_TYPE")
            .or_else(|_| env::var("SIGNATURE_TYPE"))
            .unwrap_or_else(|_| "0".to_string())
            .parse::<u8>()
            .context("SIG_TYPE or SIGNATURE_TYPE must be 0 or 2")?;

        // Support both FUNDER and FUNDER_ADDRESS
        let funder_address = env::var("FUNDER")
            .or_else(|_| env::var("FUNDER_ADDRESS"))
            .ok();

        // Polygon RPC URL
        let polygon_rpc_url = env::var("POLYGON_RPC_URL")
            .unwrap_or_else(|_| "https://polygon-rpc.com".to_string());

        // Spread Capture (gabagool)
        let sc_cheap_side_max_ask = env::var("SC_CHEAP_SIDE_MAX_ASK")
            .unwrap_or_else(|_| "0.45".to_string())
            .parse::<Decimal>()
            .context("SC_CHEAP_SIDE_MAX_ASK must be a decimal")?;
        let sc_max_pair_cost = env::var("SC_MAX_PAIR_COST")
            .unwrap_or_else(|_| "0.98".to_string())
            .parse::<Decimal>()
            .context("SC_MAX_PAIR_COST must be a decimal")?;
        let sc_max_qty_imbalance = env::var("SC_MAX_QTY_IMBALANCE")
            .unwrap_or_else(|_| "10".to_string())
            .parse::<Decimal>()
            .context("SC_MAX_QTY_IMBALANCE must be a decimal")?;
        let sc_max_active_markets = env::var("SC_MAX_ACTIVE_MARKETS")
            .unwrap_or_else(|_| "2".to_string())
            .parse::<u32>()
            .context("SC_MAX_ACTIVE_MARKETS must be a positive integer")?;
        let sc_fak_order_size = env::var("SC_FAK_ORDER_SIZE")
            .unwrap_or_else(|_| "5".to_string())
            .parse::<Decimal>()
            .context("SC_FAK_ORDER_SIZE must be a decimal")?;
        let sc_order_cooldown_ms = env::var("SC_ORDER_COOLDOWN_MS")
            .unwrap_or_else(|_| "500".to_string())
            .parse::<u64>()
            .context("SC_ORDER_COOLDOWN_MS must be a number")?;
        let sc_prefer_15m = env::var("SC_PREFER_15M")
            .unwrap_or_else(|_| "true".to_string())
            .parse::<bool>()
            .unwrap_or(true);
        let sc_max_imbalance_pct = env::var("SC_MAX_IMBALANCE_PCT")
            .unwrap_or_else(|_| "0.10".to_string())
            .parse::<Decimal>()
            .context("SC_MAX_IMBALANCE_PCT must be a decimal")?;

        // Momentum
        let mom_min_spot_move_pct = env::var("MOM_MIN_SPOT_MOVE_PCT")
            .unwrap_or_else(|_| "0.003".to_string())
            .parse::<Decimal>()
            .context("MOM_MIN_SPOT_MOVE_PCT must be a decimal")?;
        let mom_late_entry_min_bid = env::var("MOM_LATE_ENTRY_MIN_BID")
            .unwrap_or_else(|_| "0.85".to_string())
            .parse::<Decimal>()
            .context("MOM_LATE_ENTRY_MIN_BID must be a decimal")?;
        let mom_preemptive_cancel_ms = env::var("MOM_PREEMPTIVE_CANCEL_MS")
            .unwrap_or_else(|_| "100".to_string())
            .parse::<u64>()
            .context("MOM_PREEMPTIVE_CANCEL_MS must be a number")?;

        // Market Making
        let mm_half_spread = env::var("MM_HALF_SPREAD")
            .unwrap_or_else(|_| "0.02".to_string())
            .parse::<Decimal>()
            .context("MM_HALF_SPREAD must be a decimal")?;

        let mm_volatility_spread = env::var("MM_VOLATILITY_SPREAD")
            .unwrap_or_else(|_| "0.04".to_string())
            .parse::<Decimal>()
            .context("MM_VOLATILITY_SPREAD must be a decimal")?;

        let mm_max_inventory_per_side = env::var("MM_MAX_INVENTORY_PER_SIDE")
            .unwrap_or_else(|_| "500".to_string())
            .parse::<Decimal>()
            .context("MM_MAX_INVENTORY_PER_SIDE must be a decimal")?;

        let mm_inventory_imbalance_limit = env::var("MM_INVENTORY_IMBALANCE_LIMIT")
            .unwrap_or_else(|_| "50".to_string())
            .parse::<Decimal>()
            .context("MM_INVENTORY_IMBALANCE_LIMIT must be a decimal")?;

        let mm_stop_before_end_secs = env::var("MM_STOP_BEFORE_END_SECS")
            .unwrap_or_else(|_| "30".to_string())
            .parse::<u64>()
            .context("MM_STOP_BEFORE_END_SECS must be a number")?;

        let mm_spot_volatility_window_ticks = env::var("MM_SPOT_VOL_WINDOW_TICKS")
            .unwrap_or_else(|_| "30".to_string())
            .parse::<usize>()
            .context("MM_SPOT_VOL_WINDOW_TICKS must be a number")?;

        let mm_spot_move_pct_threshold = env::var("MM_SPOT_MOVE_PCT")
            .unwrap_or_else(|_| "0.01".to_string())
            .parse::<Decimal>()
            .context("MM_SPOT_MOVE_PCT must be a decimal")?;

        // Sniper
        let snipe_min_bid = env::var("SNIPE_MIN_BID")
            .unwrap_or_else(|_| "0.90".to_string())
            .parse::<Decimal>()
            .context("SNIPE_MIN_BID must be a decimal")?;

        let snipe_max_spread = env::var("SNIPE_MAX_SPREAD")
            .unwrap_or_else(|_| "0.03".to_string())
            .parse::<Decimal>()
            .context("SNIPE_MAX_SPREAD must be a decimal")?;

        let snipe_min_elapsed_pct = env::var("SNIPE_MIN_ELAPSED_PCT")
            .unwrap_or_else(|_| "0.70".to_string())
            .parse::<f64>()
            .context("SNIPE_MIN_ELAPSED_PCT must be a float")?;

        let snipe_position_size_pct = env::var("SNIPE_POSITION_SIZE_PCT")
            .unwrap_or_else(|_| "0.90".to_string())
            .parse::<Decimal>()
            .context("SNIPE_POSITION_SIZE_PCT must be a decimal")?;

        let snipe_max_replacements = env::var("SNIPE_MAX_REPLACEMENTS")
            .unwrap_or_else(|_| "3".to_string())
            .parse::<u32>()
            .context("SNIPE_MAX_REPLACEMENTS must be a number")?;

        let snipe_no_crossover_secs = env::var("SNIPE_NO_CROSSOVER_SECS")
            .unwrap_or_else(|_| "45".to_string())
            .parse::<u64>()
            .context("SNIPE_NO_CROSSOVER_SECS must be a number")?;

        let sniper_capital_deploy_pct = env::var("SNIPER_CAPITAL_DEPLOY_PCT")
            .unwrap_or_else(|_| "0.01".to_string())
            .parse::<Decimal>()
            .context("SNIPER_CAPITAL_DEPLOY_PCT must be a decimal")?;

        let sniper_min_shares = env::var("SNIPER_MIN_SHARES")
            .unwrap_or_else(|_| "6".to_string())
            .parse::<Decimal>()
            .context("SNIPER_MIN_SHARES must be a decimal")?;

        let sniper_entry_min_best_ask = env::var("SNIPER_ENTRY_MIN_BEST_ASK")
            .unwrap_or_else(|_| "0.96".to_string())
            .parse::<Decimal>()
            .context("SNIPER_ENTRY_MIN_BEST_ASK must be a decimal")?;

        let sniper_max_fills_per_round = env::var("SNIPER_MAX_FILLS_PER_ROUND")
            .unwrap_or_else(|_| "2".to_string())
            .parse::<u32>()
            .context("SNIPER_MAX_FILLS_PER_ROUND must be a positive integer")?;

        let sniper_entry_window_5m = env::var("SNIPER_ENTRY_WINDOW_5M")
            .unwrap_or_else(|_| "130".to_string())
            .parse::<u64>()
            .context("SNIPER_ENTRY_WINDOW_5M must be a positive integer (seconds)")?;

        let sniper_entry_window_15m = env::var("SNIPER_ENTRY_WINDOW_15M")
            .unwrap_or_else(|_| "300".to_string())
            .parse::<u64>()
            .context("SNIPER_ENTRY_WINDOW_15M must be a positive integer (seconds)")?;

        let sniper_min_distance_from_open_filter = env::var("SNIPER_MIN_DISTANCE_FROM_OPEN_FILTER")
            .unwrap_or_else(|_| "false".to_string())
            .parse::<bool>()
            .unwrap_or(false);
        let sniper_momentum_reversal_filter = env::var("SNIPER_MOMENTUM_REVERSAL_FILTER")
            .unwrap_or_else(|_| "false".to_string())
            .parse::<bool>()
            .unwrap_or(false);
        let sniper_slope_filter = env::var("SNIPER_SLOPE_FILTER")
            .unwrap_or_else(|_| "true".to_string())
            .parse::<bool>()
            .unwrap_or(true);

        // Risk
        let daily_loss_limit_pct = env::var("DAILY_LOSS_LIMIT_PCT")
            .unwrap_or_else(|_| "0.20".to_string())
            .parse::<Decimal>()
            .context("DAILY_LOSS_LIMIT_PCT must be a decimal")?;

        let circuit_breaker_consecutive_losses = env::var("CIRCUIT_BREAKER_LOSSES")
            .or_else(|_| env::var("CIRCUIT_BREAKER_CONSECUTIVE_LOSSES"))
            .unwrap_or_else(|_| "3".to_string())
            .parse::<u32>()
            .context("CIRCUIT_BREAKER_LOSSES must be a number")?;

        let circuit_breaker_pause_secs = env::var("CIRCUIT_BREAKER_PAUSE_SECS")
            .unwrap_or_else(|_| "1800".to_string())
            .parse::<u64>()
            .context("CIRCUIT_BREAKER_PAUSE_SECS must be a number")?;

        let max_round_exposure_pct = env::var("MAX_ROUND_EXPOSURE_PCT")
            .unwrap_or_else(|_| "0.40".to_string())
            .parse::<Decimal>()
            .context("MAX_ROUND_EXPOSURE_PCT must be a decimal")?;

        let volatility_pause_threshold_pct = env::var("VOLATILITY_PAUSE_THRESHOLD_PCT")
            .unwrap_or_else(|_| "0.03".to_string())
            .parse::<Decimal>()
            .context("VOLATILITY_PAUSE_THRESHOLD_PCT must be a decimal")?;

        let volatility_pause_secs = env::var("VOLATILITY_PAUSE_SECS")
            .unwrap_or_else(|_| "300".to_string())
            .parse::<u64>()
            .context("VOLATILITY_PAUSE_SECS must be a number")?;

        // Markets
        let coins_str = env::var("COINS").unwrap_or_else(|_| "btc,eth,sol,xrp".to_string());
        let coins: Result<Vec<Coin>> = coins_str
            .split(',')
            .map(|s| Coin::from_str(s.trim()).context(format!("Unknown coin: {}", s)))
            .collect();
        let coins = coins?;

        let periods_str = env::var("PERIODS").unwrap_or_else(|_| "5,15".to_string());
        let periods: Result<Vec<Period>> = periods_str
            .split(',')
            .map(|s| {
                let m = s.trim().parse::<u64>().context(format!("Invalid period: {}", s))?;
                Period::from_minutes(m)
                    .ok_or_else(|| anyhow::anyhow!("Unsupported period: {} (use 5, 15, 60)", m))
            })
            .collect();
        let periods = periods?;

        // Performance
        let cancel_replace_target_ms = env::var("CANCEL_REPLACE_TARGET_MS")
            .unwrap_or_else(|_| "100".to_string())
            .parse::<u64>()
            .context("CANCEL_REPLACE_TARGET_MS must be a number")?;

        let cancel_replace_hard_limit_ms = env::var("CANCEL_REPLACE_HARD_LIMIT_MS")
            .unwrap_or_else(|_| "200".to_string())
            .parse::<u64>()
            .context("CANCEL_REPLACE_HARD_LIMIT_MS must be a number")?;

        // Logging
        let rust_log = env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
        // Rolling log filename prefix under ./logs/ (see main.rs tracing init).
        let log_file = env::var("LOG_FILE").unwrap_or_else(|_| "bot.log".to_string());

        // Strategy Mode (default to sniper_only)
        let strategy_mode_str = env::var("STRATEGY_MODE")
            .unwrap_or_else(|_| "sniper_only".to_string());
        let strategy_mode = StrategyMode::from_str(&strategy_mode_str)
            .context("Invalid STRATEGY_MODE")?;

        // Dry Run (default to true)
        let dry_run = env::var("DRY_RUN")
            .unwrap_or_else(|_| "true".to_string())
            .parse::<bool>()
            .unwrap_or(true);

        Ok(Config {
            private_key,
            signature_type,
            funder_address,
            polygon_rpc_url,
            sc_cheap_side_max_ask,
            sc_max_pair_cost,
            sc_max_qty_imbalance,
            sc_max_active_markets,
            sc_fak_order_size,
            sc_order_cooldown_ms,
            sc_prefer_15m,
            sc_max_imbalance_pct,
            mom_min_spot_move_pct,
            mom_late_entry_min_bid,
            mom_preemptive_cancel_ms,
            mm_half_spread,
            mm_volatility_spread,
            mm_max_inventory_per_side,
            mm_inventory_imbalance_limit,
            mm_stop_before_end_secs,
            mm_spot_volatility_window_ticks,
            mm_spot_move_pct_threshold,
            snipe_min_bid,
            snipe_max_spread,
            snipe_min_elapsed_pct,
            snipe_position_size_pct,
            snipe_max_replacements,
            snipe_no_crossover_secs,
            sniper_capital_deploy_pct,
            sniper_min_shares,
            sniper_entry_min_best_ask,
            sniper_max_fills_per_round,
            sniper_entry_window_5m,
            sniper_entry_window_15m,
            sniper_min_distance_from_open_filter,
            sniper_momentum_reversal_filter,
            sniper_slope_filter,
            daily_loss_limit_pct,
            circuit_breaker_consecutive_losses,
            circuit_breaker_pause_secs,
            max_round_exposure_pct,
            volatility_pause_threshold_pct,
            volatility_pause_secs,
            coins,
            periods,
            cancel_replace_target_ms,
            cancel_replace_hard_limit_ms,
            rust_log,
            log_file,
            strategy_mode,
            dry_run,
        })
    }

    /// Seconds before round end when the sniper may start evaluating entries (still subject to
    /// `sniper_entry_min_best_ask` and other gates). Uses the 5m window when the round length is
    /// at most 5 minutes; otherwise uses the 15m (long-round) window.
    pub fn sniper_entry_window_secs(&self, round_duration_secs: i64) -> u64 {
        const FIVE_MIN_SECS: i64 = 5 * 60;
        if round_duration_secs <= FIVE_MIN_SECS {
            self.sniper_entry_window_5m
        } else {
            self.sniper_entry_window_15m
        }
    }
}
