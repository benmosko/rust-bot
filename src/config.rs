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

    // Spread Capture (gabagool)
    pub sc_target_pair_cost: Decimal,
    pub sc_max_pair_cost: Decimal,
    pub sc_max_imbalance_pct: Decimal,
    pub sc_min_price_dip: Decimal,

    // Momentum
    pub mom_min_spot_move_pct: Decimal,
    pub mom_late_entry_min_bid: Decimal,
    pub mom_preemptive_cancel_ms: u64,

    // Market Making
    pub mm_half_spread: Decimal,
    pub mm_volatility_spread: Decimal,
    #[allow(dead_code)]
    pub mm_max_inventory_per_side: Decimal,
    pub mm_inventory_imbalance_limit: Decimal,
    pub mm_stop_before_end_secs: u64,

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

    // Risk
    #[allow(dead_code)]
    pub max_concurrent_rounds: usize,
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
        let sc_target_pair_cost = env::var("SC_TARGET_PAIR_COST")
            .unwrap_or_else(|_| "0.97".to_string())
            .parse::<Decimal>()
            .context("SC_TARGET_PAIR_COST must be a decimal")?;
        let sc_max_pair_cost = env::var("SC_MAX_PAIR_COST")
            .unwrap_or_else(|_| "0.99".to_string())
            .parse::<Decimal>()
            .context("SC_MAX_PAIR_COST must be a decimal")?;
        let sc_max_imbalance_pct = env::var("SC_MAX_IMBALANCE_PCT")
            .unwrap_or_else(|_| "0.10".to_string())
            .parse::<Decimal>()
            .context("SC_MAX_IMBALANCE_PCT must be a decimal")?;
        let sc_min_price_dip = env::var("SC_MIN_PRICE_DIP")
            .unwrap_or_else(|_| "0.02".to_string())
            .parse::<Decimal>()
            .context("SC_MIN_PRICE_DIP must be a decimal")?;

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

        // Risk
        let max_concurrent_rounds = env::var("MAX_CONCURRENT_ROUNDS")
            .unwrap_or_else(|_| "4".to_string())
            .parse::<usize>()
            .context("MAX_CONCURRENT_ROUNDS must be a number")?;

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
        let log_file = env::var("LOG_FILE").unwrap_or_else(|_| "polymarket-bot.log".to_string());

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
            sc_target_pair_cost,
            sc_max_pair_cost,
            sc_max_imbalance_pct,
            sc_min_price_dip,
            mom_min_spot_move_pct,
            mom_late_entry_min_bid,
            mom_preemptive_cancel_ms,
            mm_half_spread,
            mm_volatility_spread,
            mm_max_inventory_per_side,
            mm_inventory_imbalance_limit,
            mm_stop_before_end_secs,
            snipe_min_bid,
            snipe_max_spread,
            snipe_min_elapsed_pct,
            snipe_position_size_pct,
            snipe_max_replacements,
            snipe_no_crossover_secs,
            max_concurrent_rounds,
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
}
