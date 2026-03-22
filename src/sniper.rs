use crate::config::Config;
use crate::execution::ExecutionEngine;
use crate::orderbook::OrderbookManager;
use crate::pnl::PnLManager;
use crate::risk::RiskManager;
use crate::rtds_chainlink::ChainlinkTracker;
use crate::spot_feed::{spot_price_for_momentum_reversal, SharedBinanceSpotHistory};
use crate::strategy_log::{RoundEntry, StrategyLogger};
use crate::strategy_sizing::{self, DualLegSizing, SingleLegSizing};
use crate::types::{
    Coin, FilledTrade, Market, OrderbookState, Period, RoundHistoryEntry, RoundHistoryStatus,
    SniperEntryKey, SniperSide, SpotState, TuiEvent, strategy_status_key,
};
use anyhow::{anyhow, Context, Result};
use chrono::{Local, Utc};
use dashmap::{DashMap, DashSet};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::{broadcast::error::RecvError, watch, mpsc};
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};

/// Block when Binance spot has recovered more than this fraction of its distance from round open (~10s window).
const MOMENTUM_REVERSAL_THRESHOLD: f64 = 0.4;
/// Tighter threshold when price is moving against the entry direction (UP vs DOWN).
const MOMENTUM_DIRECTIONAL_THRESHOLD: f64 = 0.3;
/// Floor for distance-from-open in `recovery_ratio` to avoid division blow-ups near zero.
const MOMENTUM_DISTANCE_FLOOR: f64 = 0.0001;
/// Block when normalized spot slope (bps/sec scaled) trends against the entry side by more than this.
const SLOPE_AGAINST_THRESHOLD: f64 = 0.02;

/// Minimum |Binance spot − round open| / open required for sniper entry, from PM conviction, time left, period, and spread.
fn min_distance_required(pm_ask: f64, pm_bid: f64, time_remaining: i64, period: Period) -> f64 {
    // Base distance depends on Polymarket's conviction level
    let base = if pm_ask >= 0.98 {
        0.0001 // 0.01% — PM very confident
    } else if pm_ask >= 0.97 {
        0.0002 // 0.02%
    } else if pm_ask >= 0.96 {
        0.0003 // 0.03%
    } else if pm_ask >= 0.95 {
        0.0005 // 0.05%
    } else {
        0.0015 // 0.15% — PM unsure, need strong CEX move
    };

    // Time scaling: more time left = need more distance (more room for reversal)
    let (window, time_mult) = match period {
        Period::Five => (130.0, 1.5),
        Period::Fifteen | Period::Sixty => (180.0, 3.0),  // was 300.0
    };
    let frac = (time_remaining as f64 / window).clamp(0.0, 1.0);
    let multiplier = 1.0 + time_mult * frac;

    let mut req = base * multiplier;

    let spread = pm_ask - pm_bid;
    if spread > 0.10 {
        // Wide spread: market uncertain. Add extra distance requirement.
        let extra = (spread - 0.10) * 0.005; // 0.5% per 1.0 of spread above 0.10
        req += extra;
    }

    req
}

struct EntryQualityBlocked {
    detail: String,
    side: String,
    pm_ask: f64,
    distance_pct: f64,
    required: f64,
    time_remaining: i64,
}

enum SniperSizingPlan {
    Single(SingleLegSizing),
    Dual(DualLegSizing),
}

/// One resting GTC sniper leg (YES and/or NO in dual mode).
struct ActiveSniperLeg {
    order_id: String,
    side_name: String,
    maker_price: Decimal,
    order_size: Decimal,
    fill_handled: bool,
    fully_filled: bool,
    alive: bool,
}

pub struct Sniper {
    market: Market,
    config: Arc<Config>,
    execution: Arc<ExecutionEngine>,
    orderbook: Arc<OrderbookManager>,
    spot_receiver: watch::Receiver<SpotState>,
    risk: Arc<RiskManager>,
    opening_price: Decimal,
    #[allow(dead_code)]
    recent_spot_prices: Vec<Decimal>,
    strategy_status: Arc<DashMap<String, String>>,
    balance_receiver: watch::Receiver<Decimal>,
    gtc_filled_count: Arc<AtomicU32>,
    active_gtc_orders: Arc<DashMap<String, (Arc<ExecutionEngine>, String, Period, i64)>>,
    last_order_price: Option<Decimal>,
    last_dual_makers: Option<(Decimal, Decimal)>,
    tui_tx: Option<mpsc::Sender<TuiEvent>>,
    trade_history: Arc<DashMap<String, FilledTrade>>, // key: order_id
    round_history: Arc<Mutex<Vec<RoundHistoryEntry>>>,
    pnl_manager: Arc<PnLManager>,
    /// Reserved before `place_order` awaits so concurrent evaluation cycles cannot duplicate the same leg.
    pending_sniper_entries: Arc<DashSet<SniperEntryKey>>,
    strategy_logger: Arc<Mutex<StrategyLogger>>,
    chainlink_tracker: Arc<ChainlinkTracker>,
    binance_spot_history: SharedBinanceSpotHistory,
    momentum_blocks: Arc<AtomicU64>,
    last_momentum_block: Arc<Mutex<Option<String>>>,
    /// Dedupe high-frequency `log_sniper_decision` rows (key → last log time).
    decision_log_throttle: Arc<Mutex<HashMap<String, Instant>>>,
}

impl Sniper {
    pub fn new(
        market: Market,
        config: Arc<Config>,
        execution: Arc<ExecutionEngine>,
        orderbook: Arc<OrderbookManager>,
        spot_receiver: watch::Receiver<SpotState>,
        risk: Arc<RiskManager>,
        opening_price: Decimal,
        strategy_status: Arc<DashMap<String, String>>,
        balance_receiver: watch::Receiver<Decimal>,
        gtc_filled_count: Arc<AtomicU32>,
        active_gtc_orders: Arc<DashMap<String, (Arc<ExecutionEngine>, String, Period, i64)>>,
        tui_tx: Option<mpsc::Sender<TuiEvent>>,
        trade_history: Arc<DashMap<String, FilledTrade>>,
        round_history: Arc<Mutex<Vec<RoundHistoryEntry>>>,
        pnl_manager: Arc<PnLManager>,
        pending_sniper_entries: Arc<DashSet<SniperEntryKey>>,
        strategy_logger: Arc<Mutex<StrategyLogger>>,
        chainlink_tracker: Arc<ChainlinkTracker>,
        binance_spot_history: SharedBinanceSpotHistory,
        momentum_blocks: Arc<AtomicU64>,
        last_momentum_block: Arc<Mutex<Option<String>>>,
    ) -> Self {
        let status_key = strategy_status_key(market.coin, market.period, market.round_start, "sniper");
        strategy_status.insert(status_key.clone(), "Snp: Waiting".to_string());
        Self {
            market: market.clone(),
            config: config.clone(),
            execution,
            orderbook,
            spot_receiver,
            risk,
            opening_price,
            recent_spot_prices: Vec::new(),
            strategy_status,
            balance_receiver,
            gtc_filled_count,
            active_gtc_orders,
            last_order_price: None,
            last_dual_makers: None,
            tui_tx,
            trade_history,
            round_history,
            pnl_manager,
            pending_sniper_entries,
            strategy_logger,
            chainlink_tracker,
            binance_spot_history,
            momentum_blocks,
            last_momentum_block,
            decision_log_throttle: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Returns false if `reason_code` was logged within the last `secs` for this market round (caller should skip).
    fn decision_throttle_try_acquire(&self, reason_code: &str, secs: u64) -> bool {
        let key = format!("{}|{}|{}", self.market.slug, self.market.round_start, reason_code);
        if let Ok(mut guard) = self.decision_log_throttle.lock() {
            let now = Instant::now();
            if let Some(prev) = guard.get(&key) {
                if prev.elapsed() < Duration::from_secs(secs) {
                    return false;
                }
            }
            guard.insert(key, now);
            true
        } else {
            true
        }
    }

    /// Persist a skip/block reason to `sniper_decision_events` (optional throttle per slug+round+code).
    fn log_sniper_decision(
        &self,
        reason_code: &str,
        detail: Option<String>,
        throttle_secs: Option<u64>,
    ) {
        if let Some(secs) = throttle_secs {
            if !self.decision_throttle_try_acquire(reason_code, secs) {
                return;
            }
        }

        let now_dt = Utc::now();
        let ts = now_dt.timestamp();
        let ts_ms = now_dt.timestamp_subsec_millis() as i32;
        let yes = self
            .orderbook
            .get_orderbook(&self.market.up_token_id)
            .and_then(|o| o.best_ask().and_then(|d| d.to_f64()));
        let no = self
            .orderbook
            .get_orderbook(&self.market.down_token_id)
            .and_then(|o| o.best_ask().and_then(|d| d.to_f64()));
        let tr = (self.market.round_end - ts).max(0);
        let filled = self.gtc_filled_count.load(Ordering::Relaxed);
        let rd = (self.market.round_end - self.market.round_start).max(0);
        let entry_window_secs = self.config.sniper_entry_window_secs(rd) as i32;
        let coin = self.market.coin.as_str();
        let period = format!("{}m", self.market.period.as_minutes());

        if let Ok(g) = self.strategy_logger.lock() {
            if let Err(e) = g.log_sniper_decision(
                ts,
                ts_ms,
                coin,
                &period,
                self.market.round_start,
                self.market.round_end,
                reason_code,
                detail.as_deref(),
                yes,
                no,
                tr,
                Some(filled),
                Some(entry_window_secs),
            ) {
                warn!(market = %self.market.slug, error = %e, "log_sniper_decision failed");
            }
        }
    }

    /// Last gate before placing GTC: block fast mean reversion toward round open (Binance vs `opening_price`).
    /// Returns a short description when this leg should **not** be entered. Fails open when history is missing or ambiguous.
    fn momentum_reversal_block_detail(&self, side_name: &str) -> Option<String> {
        if !self.config.sniper_momentum_reversal_filter {
            return None;
        }
        let side_label = if side_name == "YES" { "UP" } else { "DOWN" };
        let opening_price = match self.opening_price.to_f64() {
            Some(p) if p > 0.0 => p,
            _ => return None,
        };
        let current_spot = match self.spot_receiver.borrow().price.to_f64() {
            Some(p) if p > 0.0 => p,
            _ => return None,
        };
        let spot_10s_ago = match spot_price_for_momentum_reversal(
            &self.binance_spot_history,
            self.market.coin,
        ) {
            Some(p) if p > 0.0 => p,
            _ => return None,
        };

        let current_distance = (current_spot - opening_price).abs();
        let past_distance = (spot_10s_ago - opening_price).abs();
        let past_denom = past_distance.max(MOMENTUM_DISTANCE_FLOOR);
        let recovery_ratio =
            (past_distance - current_distance.max(MOMENTUM_DISTANCE_FLOOR)) / past_denom;

        let block_strong_reversal = recovery_ratio > MOMENTUM_REVERSAL_THRESHOLD;

        let against_us = match side_name {
            "YES" => current_spot < spot_10s_ago,
            "NO" => current_spot > spot_10s_ago,
            _ => false,
        };
        let block_directional =
            against_us && recovery_ratio > MOMENTUM_DIRECTIONAL_THRESHOLD;

        if block_strong_reversal || block_directional {
            info!(
                coin = ?self.market.coin,
                side = %side_label,
                opening_price = %opening_price,
                current_spot = %current_spot,
                spot_10s_ago = %spot_10s_ago,
                recovery_ratio = %recovery_ratio,
                "Momentum reversal filter: BLOCKED entry — price recovering toward open"
            );
            let coin_u = self.market.coin.as_str().to_uppercase();
            let period_m = format!("{}m", self.market.period.as_minutes());
            return Some(format!(
                "{} {} {} (recovery {:.2})",
                coin_u, period_m, side_label, recovery_ratio
            ));
        }

        debug!(
            coin = ?self.market.coin,
            recovery_ratio = %recovery_ratio,
            "Momentum filter: OK"
        );
        None
    }

    /// |Binance spot − Binance round ref| / ref — `opening_price` is the Binance snapshot passed at sniper start (main).
    fn distance_from_open_pct(&self) -> Option<f64> {
        let binance_ref = self.opening_price.to_f64()?;
        let binance_spot = self.spot_receiver.borrow().price.to_f64()?;
        if binance_ref <= 0.0 || binance_spot <= 0.0 {
            return None;
        }
        Some((binance_spot - binance_ref).abs() / binance_ref)
    }

    /// Unified entry quality gate: PM ask, Binance distance from round open, and time remaining.
    /// Returns `Some` when this leg should not be entered. When the filter is off or distance is unavailable, returns `None` (fail open).
    fn entry_quality_eval(
        &self,
        side_name: &str,
        best_ask: Decimal,
        pm_bid: f64,
    ) -> Option<EntryQualityBlocked> {
        if !self.config.sniper_min_distance_from_open_filter {
            return None;
        }
        let distance_pct = self.distance_from_open_pct()?;
        let pm_ask = best_ask.to_f64().unwrap_or(0.0);
        let time_remaining = (self.market.round_end - Utc::now().timestamp()).max(0);
        let required = min_distance_required(pm_ask, pm_bid, time_remaining, self.market.period);
        let side_label = if side_name == "YES" {
            "UP".to_string()
        } else {
            "DOWN".to_string()
        };
        if distance_pct < required {
            let coin_u = self.market.coin.as_str().to_uppercase();
            let period_m = format!("{}m", self.market.period.as_minutes());
            let detail = format!(
                "{} {} {} pm_ask={} dist_pct={:.6} < required={:.6} tr={}s",
                coin_u, period_m, side_label, best_ask, distance_pct, required, time_remaining
            );
            return Some(EntryQualityBlocked {
                detail,
                side: side_label,
                pm_ask,
                distance_pct,
                required,
                time_remaining,
            });
        }
        debug!(
            coin = ?self.market.coin,
            side = %side_label,
            pm_ask,
            distance_pct = %distance_pct,
            required = %required,
            time_remaining,
            "Entry quality: OK"
        );
        None
    }

    /// Binance spot linear slope over the last ~30s; block if price trends against the entry side. Fail open if data is thin.
    fn spot_slope_blocks_entry(&self, side_name: &str) -> Option<String> {
        if !self.config.sniper_slope_filter {
            return None;
        }
        let chainlink_open = match self.opening_price.to_f64() {
            Some(p) if p > 0.0 => p,
            _ => return None,
        };
        let now = Utc::now().timestamp();
        let map_ref = self.binance_spot_history.get(&self.market.coin)?;
        let guard = map_ref.value().lock().ok()?;
        let q = &*guard;
        let mut xs: Vec<f64> = Vec::new();
        let mut ys: Vec<f64> = Vec::new();
        let t0 = (now - 30) as f64;
        for (ts, price) in q.iter() {
            if *ts >= now - 30 && *ts <= now && *price > 0.0 {
                xs.push(*ts as f64 - t0);
                ys.push(*price);
            }
        }
        if xs.len() < 5 {
            return None;
        }
        let n = xs.len() as f64;
        let mean_x: f64 = xs.iter().sum::<f64>() / n;
        let mean_y: f64 = ys.iter().sum::<f64>() / n;
        let mut cov = 0.0;
        let mut var_x = 0.0;
        for i in 0..xs.len() {
            let dx = xs[i] - mean_x;
            let dy = ys[i] - mean_y;
            cov += dx * dy;
            var_x += dx * dx;
        }
        if var_x <= 1e-18 {
            return None;
        }
        let slope = cov / var_x;
        let normalized_slope = slope / chainlink_open * 10000.0;

        let side_label = if side_name == "YES" {
            "UP"
        } else {
            "DOWN"
        };
        let against = match side_name {
            "YES" => normalized_slope < -SLOPE_AGAINST_THRESHOLD,
            "NO" => normalized_slope > SLOPE_AGAINST_THRESHOLD,
            _ => return None,
        };
        if against {
            return Some(format!(
                "slope {:.3} against {}",
                normalized_slope, side_label
            ));
        }
        debug!(
            coin = ?self.market.coin,
            side = %side_label,
            normalized_slope,
            "Spot slope filter: OK"
        );
        None
    }

    fn sniper_entry_key(&self, side_name: &str) -> SniperEntryKey {
        let side = match side_name {
            "YES" => SniperSide::Yes,
            "NO" => SniperSide::No,
            _ => SniperSide::Yes,
        };
        SniperEntryKey {
            coin: self.market.coin,
            period: self.market.period,
            round_start: self.market.round_start,
            condition_id: self.market.condition_id.clone(),
            side,
        }
    }

    fn remove_pending_side(&self, side_name: &str) {
        self.pending_sniper_entries
            .remove(&self.sniper_entry_key(side_name));
    }

    fn clear_pending_keys(&self, keys: &[SniperEntryKey]) {
        for k in keys {
            self.pending_sniper_entries.remove(k);
        }
    }

    #[inline]
    fn max_primary_fills_per_round(&self) -> u32 {
        self.config.sniper_max_fills_per_round.max(1)
    }

    fn strategy_log_primary_fill(&self, side_name: &str, maker_price: Decimal, size_matched: Decimal) {
        let sl = &self.strategy_logger;
        let round_duration_secs = (self.market.round_end - self.market.round_start).max(0);
        let entry_window_secs = self.config.sniper_entry_window_secs(round_duration_secs);
        let token = if side_name == "YES" {
            &self.market.up_token_id
        } else {
            &self.market.down_token_id
        };
        let (best_bid, best_ask) = self
            .orderbook
            .get_orderbook(token)
            .as_ref()
            .map(|ob| {
                (
                    ob.best_bid().and_then(|d| d.to_f64()),
                    ob.best_ask().and_then(|d| d.to_f64()),
                )
            })
            .unwrap_or((None, None));
        let binance_spot_at_entry = self.spot_receiver.borrow().price.to_f64();
        let chainlink_open = self
            .chainlink_tracker
            .chainlink_open_for_round(self.market.coin, self.market.period, self.market.round_start)
            .and_then(|d| d.to_f64());
        let time_remaining_secs =
            (self.market.round_end - Utc::now().timestamp()).max(0) as i32;
        let side_label = if side_name == "YES" { "UP" } else { "DOWN" };
        let entry = RoundEntry {
            timestamp_utc: Utc::now().to_rfc3339(),
            coin: self.market.coin.as_str().to_string(),
            period: format!("{}m", self.market.period.as_minutes()),
            side: side_label.to_string(),
            round_start: self.market.round_start,
            round_end: self.market.round_end,
            entry_price: maker_price.to_f64().unwrap_or(0.0),
            shares: size_matched.to_f64().map(|x| x as i32).unwrap_or(0),
            best_ask_at_entry: best_ask,
            best_bid_at_entry: best_bid,
            binance_spot_at_entry,
            chainlink_open,
            time_remaining_secs: Some(time_remaining_secs),
            entry_window_secs: Some(entry_window_secs as i32),
        };
        if let Ok(g) = sl.lock() {
            if let Err(e) = g.log_entry(&entry) {
                warn!(error = %e, "strategy_log log_entry failed");
            }
        }
    }

    /// Per-second Polymarket + spot snapshots into `logs/strategy.db` (`round_price_ticks` table).
    async fn round_price_tick_loop(
        shutdown: tokio_util::sync::CancellationToken,
        strategy_logger: Arc<Mutex<StrategyLogger>>,
        market: Market,
        orderbook: Arc<OrderbookManager>,
        spot_receiver: watch::Receiver<SpotState>,
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let coin = market.coin.as_str().to_string();
        let period = format!("{}m", market.period.as_minutes());

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    break;
                }
                _ = interval.tick() => {
                    let now_ts = Utc::now().timestamp();
                    if now_ts < market.round_start {
                        continue;
                    }
                    if now_ts > market.round_end {
                        break;
                    }

                    let yes_ob = orderbook.get_orderbook(&market.up_token_id);
                    let no_ob = orderbook.get_orderbook(&market.down_token_id);
                    let best_ask_yes = yes_ob.as_ref().and_then(|o| o.best_ask()).and_then(|d| d.to_f64());
                    let best_bid_yes = yes_ob.as_ref().and_then(|o| o.best_bid()).and_then(|d| d.to_f64());
                    let best_ask_no = no_ob.as_ref().and_then(|o| o.best_ask()).and_then(|d| d.to_f64());
                    let best_bid_no = no_ob.as_ref().and_then(|o| o.best_bid()).and_then(|d| d.to_f64());
                    let binance_spot = spot_receiver.borrow().price.to_f64();
                    let time_remaining_secs = (market.round_end - now_ts).max(0);

                    if let Ok(g) = strategy_logger.lock() {
                        if let Err(e) = g.log_round_price_tick(
                            now_ts,
                            &coin,
                            &period,
                            market.round_start,
                            market.round_end,
                            best_ask_yes,
                            best_ask_no,
                            best_bid_yes,
                            best_bid_no,
                            binance_spot,
                            time_remaining_secs,
                        ) {
                            warn!(
                                market = %market.slug,
                                error = %e,
                                "strategy_log log_round_price_tick failed"
                            );
                        }
                    }
                }
            }
        }
    }

    pub async fn run(
        &mut self,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        let round_duration_secs = (self.market.round_end - self.market.round_start).max(0);
        let entry_window_secs = self.config.sniper_entry_window_secs(round_duration_secs);
        info!(
            market = %self.market.slug,
            round_start = self.market.round_start,
            round_duration_secs,
            entry_window_secs,
            opening_price = %self.opening_price,
            "Sniper waiting for entry window"
        );

        let status_key = strategy_status_key(self.market.coin, self.market.period, self.market.round_start, "sniper");
        let mut active_legs: Vec<ActiveSniperLeg> = Vec::new();

        // EVENT-DRIVEN SNIPER: Sub-2ms detection latency
        // The sniper now uses event-driven triggers instead of polling:
        // 1. Subscribes to price threshold events from OrderbookManager
        // 2. Events are emitted when best_ask >= SNIPER_ENTRY_MIN_BEST_ASK (within ~1-2ms of WebSocket update)
        // 3. Sniper reacts instantly to events, eliminating polling delay
        // 4. Fallback polling (100ms) ensures we don't miss events if channel is slow
        //
        // DETECTION LATENCY BREAKDOWN:
        // - WebSocket message arrives: 0ms (network dependent, typically 5-20ms from exchange)
        // - Orderbook update + event emission: <1ms (DashMap update + broadcast send)
        // - Event reception in sniper: <1ms (broadcast channel)
        // - Total detection latency: ~2-5ms (was 500ms with polling)
        //
        // This gives us TRUE sub-2ms detection from the moment price crosses threshold
        // (excluding network latency from exchange, which we can't control)

        // Subscribe to price threshold events (best_ask >= sniper entry minimum)
        let mut price_event_rx = self.orderbook.subscribe_price_events();

        let tick_shutdown = shutdown.clone();
        let tick_logger = self.strategy_logger.clone();
        let tick_market = self.market.clone();
        let tick_orderbook = self.orderbook.clone();
        let tick_spot = self.spot_receiver.clone();
        let _price_tick_task = tokio::spawn(Self::round_price_tick_loop(
            tick_shutdown,
            tick_logger,
            tick_market,
            tick_orderbook,
            tick_spot,
        ));

        // Fallback polling interval (safety net in case events are missed)
        let mut fallback_interval = tokio::time::interval(Duration::from_millis(100));
        fallback_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let sniper_run_start = Instant::now();
        let mut heartbeat_interval = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(30),
            Duration::from_secs(30),
        );
        heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Last time we started `evaluate_entry_gates_with_orderbooks` (detect long gaps when stuck in poll / HTTP).
        let mut last_gate_eval_at: Option<Instant> = None;

        loop {
            if shutdown.is_cancelled() {
                info!(market = %self.market.slug, "Sniper shutting down");
                for leg in &active_legs {
                    let _ = self.execution.cancel_order(&leg.order_id).await;
                    self.active_gtc_orders.remove(&leg.order_id);
                }
                break;
            }

            let now = Utc::now().timestamp();
            let time_remaining = self.market.round_end - now;

            // Only activate in the configured tail of the round (period-aware via Config).
            // Live YES/NO on the TUI can look "ready" for minutes — we do not run entry gates until
            // `time_remaining <= entry_window_secs` (e.g. last 180s on 15m). Show time left until then.
            // Do not log SQLite skip reasons here.
            if time_remaining > entry_window_secs as i64 {
                let ew = entry_window_secs as i64;
                let secs_until = time_remaining - ew;
                let tail = if secs_until >= 3600 {
                    format!("{}h{}m", secs_until / 3600, (secs_until % 3600) / 60)
                } else if secs_until >= 60 {
                    format!("{}m{:02}s", secs_until / 60, secs_until % 60)
                } else {
                    format!("{secs_until}s")
                };
                self.strategy_status.insert(
                    status_key.clone(),
                    format!("Snp: Too early (sniper in {tail})"),
                );
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            // Check if sniper is paused
            if self.risk.is_sniper_paused().await {
                self.strategy_status.insert(status_key.clone(), "Snp: Paused".to_string());
                self.log_sniper_decision("paused", None, Some(10));
                warn!(market = %self.market.slug, "Sniper is paused");
                sleep(Duration::from_secs(10)).await;
                continue;
            }

            // Check balance - skip if < $5
            let balance = *self.balance_receiver.borrow();
            if balance < dec!(5) {
                self.strategy_status.insert(status_key.clone(), "Snp: Low balance".to_string());
                self.log_sniper_decision(
                    "low_balance",
                    Some(format!("balance_usdc={}", balance)),
                    Some(5),
                );
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            // EVENT-DRIVEN TRIGGER: Wait for price threshold event OR fallback timer
            // This gives us sub-2ms detection latency when prices cross the entry threshold
            let mut threshold_emit_at: Option<Instant> = None;
            let mut skip_entry_gates_this_iteration = false;
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    break;
                }
                _ = heartbeat_interval.tick() => {
                    let elapsed = sniper_run_start.elapsed().as_secs();
                    let best_ask_yes = self
                        .orderbook
                        .get_orderbook(&self.market.up_token_id)
                        .and_then(|o| o.best_ask());
                    let best_ask_no = self
                        .orderbook
                        .get_orderbook(&self.market.down_token_id)
                        .and_then(|o| o.best_ask());
                    info!(
                        market = %self.market.slug,
                        elapsed_secs = elapsed,
                        best_ask_yes = ?best_ask_yes,
                        best_ask_no = ?best_ask_no,
                        "Sniper heartbeat"
                    );
                    skip_entry_gates_this_iteration = true;
                }
                // Primary trigger: Price threshold event (best_ask >= sniper entry minimum)
                event_result = price_event_rx.recv() => {
                    match event_result {
                        Ok(event) => {
                            // Shared broadcast: every subscribed sniper receives threshold events for ALL tokens.
                            // Only record hot-path latency when the event is for our market; still run entry
                            // gates below so other markets' events do not starve us (biased select + continue
                            // would skip evaluate_entry_gates until our token happened to fire).
                            if event.token_id == self.market.up_token_id || event.token_id == self.market.down_token_id {
                                threshold_emit_at = Some(event.timestamp);
                                let event_latency = event.timestamp.elapsed();
                                debug!(
                                    market = %self.market.slug,
                                    token_id = %event.token_id,
                                    best_ask = %event.best_ask,
                                    event_latency_us = event_latency.as_micros(),
                                    "Price threshold event received - triggering entry check"
                                );
                            } else {
                                debug!(
                                    market = %self.market.slug,
                                    foreign_token_id = %event.token_id,
                                    "Price threshold event for another market (wake-up only)"
                                );
                            }
                        }
                        Err(RecvError::Lagged(_)) => {
                            // Dropped messages; orderbook state is still updated on WS path — evaluate gates.
                        }
                        Err(RecvError::Closed) => {
                            self.log_sniper_decision("price_events_channel_closed", None, None);
                            skip_entry_gates_this_iteration = true;
                        }
                    }
                }
                // Fallback: Safety timer (100ms) ensures we don't miss events
                _ = fallback_interval.tick() => {
                    // Fallback tick, check entry gates
                }
            };

            // Primary fill cap for this market (coin, period, round_start): stop new sniper GTCs.
            let filled_count_outer = self.gtc_filled_count.load(Ordering::Relaxed);
            let max_fills = self.max_primary_fills_per_round();
            let cap_reached = filled_count_outer >= max_fills;
            if cap_reached {
                self.strategy_status
                    .insert(status_key.clone(), "Snp: Capped".to_string());
            }
            if cap_reached {
                self.log_sniper_decision(
                    "fill_cap",
                    Some(format!(
                        "filled={} max={}",
                        filled_count_outer, max_fills
                    )),
                    Some(2),
                );
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            if skip_entry_gates_this_iteration {
                continue;
            }

            let now_eval = Instant::now();
            if let Some(prev) = last_gate_eval_at {
                let gap = now_eval.saturating_duration_since(prev);
                if gap > Duration::from_secs(1) {
                    warn!(
                        market = %self.market.slug,
                        gap_ms = gap.as_millis(),
                        "sniper evaluation cycle gap > 1s since last gate eval (common: inner poll loop after GTC place — fallback_interval/select! not running; or slow CLOB/reconcile)"
                    );
                }
            }
            last_gate_eval_at = Some(now_eval);

            // Run entry gates (triggered by event or fallback timer)
            let t0 = Instant::now(); // Start: entry conditions detected
            let emit_to_hot_us: Option<u128> = threshold_emit_at
                .map(|emit| emit.elapsed().as_micros());
            // Get orderbook data
            let yes_orderbook_opt = self.orderbook.get_orderbook(&self.market.up_token_id);
            let no_orderbook_opt = self.orderbook.get_orderbook(&self.market.down_token_id);
            let t1 = Instant::now(); // After orderbook read
            match self
                .evaluate_entry_gates_with_orderbooks(yes_orderbook_opt, no_orderbook_opt, Some(&status_key))
                .await
            {
                Ok(Some(sides)) => {
                    let t2 = Instant::now(); // After entry gate evaluation
                    if sides.is_empty() {
                        self.strategy_status.insert(status_key.clone(), "Snp: Waiting".to_string());
                        self.log_sniper_decision(
                            "gate_empty_sides",
                            None,
                            Some(5),
                        );
                        continue;
                    }

                    let tick_size = self.market.minimum_tick_size;

                    let mut plans: Vec<(String, String, Decimal, Decimal)> = Vec::new();
                    for (side_name, token_id, best_ask) in sides {
                        if let Some(mp) = Self::maker_price_from_best_ask(
                            best_ask,
                            tick_size,
                            self.config.sniper_entry_min_best_ask,
                        ) {
                            plans.push((side_name, token_id, best_ask, mp));
                        }
                    }
                    plans.sort_by(|a, b| match (a.0.as_str(), b.0.as_str()) {
                        ("YES", "NO") => std::cmp::Ordering::Less,
                        ("NO", "YES") => std::cmp::Ordering::Greater,
                        _ => std::cmp::Ordering::Equal,
                    });

                    if plans.is_empty() {
                        self.log_sniper_decision(
                            "maker_price_unavailable",
                            Some(format!(
                                "tick_size={} entry_min_best_ask={}",
                                tick_size, self.config.sniper_entry_min_best_ask
                            )),
                            Some(3),
                        );
                        continue;
                    }

                    // Per-side in-flight guard for this market only (another market never blocks here).
                    let mut reserved_keys: Vec<SniperEntryKey> = Vec::new();
                    let mut plans_res: Vec<(String, String, Decimal, Decimal)> = Vec::new();
                    for (side_name, token_id, best_ask, mp) in plans {
                        let k = self.sniper_entry_key(&side_name);
                        if !self.pending_sniper_entries.insert(k.clone()) {
                            debug!(
                                market = %self.market.slug,
                                side = %side_name,
                                "Sniper: side in-flight on this market; skipping this leg"
                            );
                            self.log_sniper_decision(
                                "side_in_flight",
                                Some(format!("blocked_side={side_name}")),
                                Some(2),
                            );
                            continue;
                        }
                        reserved_keys.push(k);
                        plans_res.push((side_name, token_id, best_ask, mp));
                    }
                    if plans_res.is_empty() {
                        self.strategy_status.insert(status_key.clone(), "Snp: Waiting".to_string());
                        self.log_sniper_decision("all_sides_in_flight", None, Some(2));
                        continue;
                    }
                    let plans = plans_res;

                    let dual_mode = plans.len() >= 2;

                    // REMOVED: Price check that prevented placing new orders at same price
                    // Now allows placing new orders even if one already exists at the same price

                    // Only cancel existing orders if price has changed
                    // Allow multiple orders at the same price to coexist
                    if !active_legs.is_empty() {
                        let new_price = if !dual_mode {
                            Some(plans[0].3)
                        } else {
                            None // Dual mode: keep existing orders, add new ones
                        };
                        
                        let mut legs_to_cancel = Vec::new();
                        let mut legs_to_keep = Vec::new();
                        let dual_poll_reconcile = active_legs.len() >= 2;

                        for leg in active_legs.drain(..) {
                            // Check if this leg has already filled
                            let is_filled = match self.execution.get_order_status(&leg.order_id).await {
                                Ok(Some((status, size_matched))) => {
                                    if Self::order_status_indicates_fill(&status, size_matched) {
                                        // Leg has filled, reconcile it and don't keep it
                                        let _ = self.reconcile_late_fill(
                                            &leg.order_id,
                                            &leg.side_name,
                                            leg.maker_price,
                                            &status_key,
                                            None,
                                            None,
                                            dual_poll_reconcile,
                                        ).await;
                                        true
                                    } else {
                                        false
                                    }
                                }
                                _ => false,
                            };
                            
                            if is_filled {
                                continue; // Already handled, don't keep or cancel
                            }
                            
                            // If price changed in single mode, cancel old order
                            // If price is same or dual mode, keep it (allow multiple orders)
                            if let Some(np) = new_price {
                                if leg.maker_price != np {
                                    // Price changed - cancel old order
                                    legs_to_cancel.push(leg);
                                } else {
                                    // Same price - keep it and allow new order too
                                    legs_to_keep.push(leg);
                                }
                            } else {
                                // Dual mode: keep existing orders, we'll add new ones
                                legs_to_keep.push(leg);
                            }
                        }
                        
                        // Cancel orders at different prices (single mode only)
                        for leg in &legs_to_cancel {
                            let _ = self.execution.cancel_order(&leg.order_id).await;
                            let _ = self.reconcile_late_fill(
                                &leg.order_id,
                                &leg.side_name,
                                leg.maker_price,
                                &status_key,
                                None,
                                None,
                                dual_poll_reconcile,
                            ).await;
                            self.active_gtc_orders.remove(&leg.order_id);
                        }
                        
                        // Restore legs we're keeping (same price, allow multiple)
                        active_legs = legs_to_keep;
                    }

                    // Double-check shared fill cap (other markets may have filled while we sized).
                    let filled_count = self.gtc_filled_count.load(Ordering::Relaxed);
                    if filled_count >= self.max_primary_fills_per_round() {
                        self.clear_pending_keys(&reserved_keys);
                        self.strategy_status
                            .insert(status_key.clone(), "Snp: Capped".to_string());
                        self.log_sniper_decision(
                            "fill_cap",
                            Some(format!(
                                "filled={} max={} (post-reserve)",
                                filled_count,
                                self.max_primary_fills_per_round()
                            )),
                            Some(2),
                        );
                        continue;
                    }

                    let sized_plan: Result<(Vec<Decimal>, SniperSizingPlan)> = if dual_mode {
                        let yes_p = plans
                            .iter()
                            .find(|p| p.0 == "YES")
                            .map(|p| p.3)
                            .ok_or_else(|| anyhow!("dual sniper: missing YES plan"))?;
                        let no_p = plans
                            .iter()
                            .find(|p| p.0 == "NO")
                            .map(|p| p.3)
                            .ok_or_else(|| anyhow!("dual sniper: missing NO plan"))?;
                        let d = self.calculate_per_leg_size_dual(yes_p, no_p)?;
                        let per = d.final_per_leg;
                        Ok((vec![per, per], SniperSizingPlan::Dual(d)))
                    } else {
                        let s = self.compute_single_leg_sizing(plans[0].3)?;
                        let sz = s.final_shares;
                        Ok((vec![sz], SniperSizingPlan::Single(s)))
                    };
                    let t3 = Instant::now(); // After position size calc

                    let (sized, sizing_plan) = match sized_plan {
                        Ok(sp) => sp,
                        Err(e) => {
                            self.clear_pending_keys(&reserved_keys);
                            self.strategy_status.insert(status_key.clone(), "Snp: Can't afford".to_string());
                            self.log_sniper_decision(
                                "sizing_failed",
                                Some(format!("{e:#}")),
                                None,
                            );
                            warn!(market = %self.market.slug, error = %e, "Cannot size sniper leg(s)");
                            continue;
                        }
                    };

                    let mut ok_sizes: Vec<Decimal> = Vec::new();
                    for (i, sz) in sized.iter().enumerate() {
                        let sz_str = format!("{:.2}", sz);
                        let sz = Decimal::from_str(&sz_str).unwrap_or(*sz);
                        if sz < self.config.sniper_min_shares {
                            self.strategy_status.insert(status_key.clone(), "Snp: Too small".to_string());
                            self.log_sniper_decision(
                                "order_below_min_shares",
                                Some(format!(
                                    "leg={i} size={sz} min={}",
                                    self.config.sniper_min_shares
                                )),
                                None,
                            );
                            warn!(
                                market = %self.market.slug,
                                leg_index = i,
                                order_size = %sz,
                                min_shares = %self.config.sniper_min_shares,
                                "Order size below configured minimum"
                            );
                            ok_sizes.clear();
                            break;
                        }
                        ok_sizes.push(sz);
                    }
                    if ok_sizes.len() != plans.len() {
                        self.clear_pending_keys(&reserved_keys);
                        self.log_sniper_decision(
                            "sizing_mismatch",
                            Some(format!(
                                "ok_sizes={} plans={}",
                                ok_sizes.len(),
                                plans.len()
                            )),
                            None,
                        );
                        continue;
                    }

                    // Entry quality gate (PM ask + Binance vs round open + time remaining). Dual: abort all if any leg blocks.
                    let mut entry_quality_blocks: Vec<EntryQualityBlocked> = Vec::new();
                    for (side_name, token_id, best_ask, _) in &plans {
                        let pm_bid = self
                            .orderbook
                            .get_orderbook(token_id)
                            .as_ref()
                            .and_then(|o| o.best_bid())
                            .and_then(|d| d.to_f64())
                            .unwrap_or(0.0);
                        if let Some(b) = self.entry_quality_eval(side_name, *best_ask, pm_bid) {
                            entry_quality_blocks.push(b);
                        }
                    }
                    if !entry_quality_blocks.is_empty() {
                        if self.decision_throttle_try_acquire("entry_quality_block", 5) {
                            for b in &entry_quality_blocks {
                                info!(
                                    coin = ?self.market.coin,
                                    side = %b.side,
                                    pm_ask = b.pm_ask,
                                    distance_pct = %b.distance_pct,
                                    required = %b.required,
                                    time_remaining = b.time_remaining,
                                    "Entry quality: BLOCKED"
                                );
                            }
                            let combined = entry_quality_blocks
                                .iter()
                                .map(|b| b.detail.as_str())
                                .collect::<Vec<_>>()
                                .join(" | ");
                            self.log_sniper_decision(
                                "entry_quality_block",
                                Some(combined),
                                None,
                            );
                        }
                        self.strategy_status
                            .insert(status_key.clone(), "Snp: Entry quality".to_string());
                        self.clear_pending_keys(&reserved_keys);
                        continue;
                    }

                    // Spot slope gate: block if Binance spot trend in the last ~30s is against the entry side. Dual: abort all if any leg blocks.
                    let mut slope_block_details: Vec<(String, String)> = Vec::new();
                    for (side_name, _, _, _) in &plans {
                        if let Some(detail) = self.spot_slope_blocks_entry(side_name) {
                            slope_block_details.push((side_name.clone(), detail));
                        }
                    }
                    if !slope_block_details.is_empty() {
                        if self.decision_throttle_try_acquire("spot_slope_block", 5) {
                            for (side_name, detail) in &slope_block_details {
                                let side_label = if side_name == "YES" { "UP" } else { "DOWN" };
                                info!(
                                    coin = ?self.market.coin,
                                    side = %side_label,
                                    detail = %detail,
                                    threshold = SLOPE_AGAINST_THRESHOLD,
                                    "Spot slope filter: BLOCKED — price trending against entry"
                                );
                            }
                            let combined = slope_block_details
                                .iter()
                                .map(|(_, d)| d.as_str())
                                .collect::<Vec<_>>()
                                .join(" | ");
                            self.log_sniper_decision("spot_slope_block", Some(combined), None);
                        }
                        self.strategy_status
                            .insert(status_key.clone(), "Snp: Spot slope".to_string());
                        self.clear_pending_keys(&reserved_keys);
                        continue;
                    }

                    // Momentum reversal gate (Binance vs round open). Dual: abort all if any leg blocks.
                    if plans.len() == 2 {
                        let d1 = self.momentum_reversal_block_detail(&plans[0].0);
                        let d2 = self.momentum_reversal_block_detail(&plans[1].0);
                        if d1.is_some() || d2.is_some() {
                            let detail = match (d1, d2) {
                                (Some(a), Some(b)) => Some(format!("{a} | {b}")),
                                (Some(a), None) => Some(a),
                                (None, Some(b)) => Some(b),
                                _ => None,
                            };
                            self.log_sniper_decision(
                                "momentum_reversal_block",
                                detail.clone(),
                                None,
                            );
                            self.strategy_status
                                .insert(status_key.clone(), "Snp: Mom reversal".to_string());
                            self.momentum_blocks.fetch_add(1, Ordering::Relaxed);
                            if let Ok(mut g) = self.last_momentum_block.lock() {
                                *g = detail;
                            }
                            self.clear_pending_keys(&reserved_keys);
                            continue;
                        }
                    } else if let Some(detail) =
                        self.momentum_reversal_block_detail(&plans[0].0)
                    {
                        self.log_sniper_decision(
                            "momentum_reversal_block",
                            Some(detail.clone()),
                            None,
                        );
                        self.strategy_status
                            .insert(status_key.clone(), "Snp: Mom reversal".to_string());
                        self.momentum_blocks.fetch_add(1, Ordering::Relaxed);
                        if let Ok(mut g) = self.last_momentum_block.lock() {
                            *g = Some(detail);
                        }
                        self.clear_pending_keys(&reserved_keys);
                        continue;
                    }

                    self.strategy_status.insert(status_key.clone(), "Snp: Entry!".to_string());

                    let mut placed: Vec<ActiveSniperLeg> = Vec::new();
                    let mut place_err: Option<anyhow::Error> = None;

                    for (i, (side_name, token_id, best_ask, maker_price)) in plans.iter().enumerate() {
                        let order_size = ok_sizes[i];
                        match &sizing_plan {
                            SniperSizingPlan::Single(s) => {
                                info!(
                                    "SIZING: balance={}, deploy_pct={}, price={}, calc_shares={}, min_shares={}, final_shares={}",
                                    s.balance,
                                    s.deploy_pct,
                                    s.price,
                                    s.calc_shares,
                                    s.min_shares,
                                    order_size,
                                );
                            }
                            SniperSizingPlan::Dual(d) => {
                                info!(
                                    "SIZING: balance={}, deploy_pct={}, sum_px={}, calc_per_leg={}, min_shares={}, final_per_leg={}",
                                    d.balance,
                                    d.deploy_pct,
                                    d.sum_px,
                                    d.calc_per_leg,
                                    d.min_shares,
                                    order_size,
                                );
                            }
                        }
                        let place_start = Instant::now();
                        match self
                            .execution
                            .place_order(
                                token_id,
                                polymarket_client_sdk::clob::types::Side::Buy,
                                *maker_price,
                                order_size,
                                tick_size,
                            )
                            .await
                        {
                            Ok(oid) => {
                                let t4 = Instant::now(); // After order placed (HTTP response received)

                                // Full hot-path timing once (first leg); dual second leg logged separately
                                if i == 0 {
                                    info!(
                                        market = %self.market.slug,
                                        emit_to_hot_us = ?emit_to_hot_us,
                                        orderbook_read_us = t1.duration_since(t0).as_micros(),
                                        gate_eval_us = t2.duration_since(t1).as_micros(),
                                        size_calc_us = t3.duration_since(t2).as_micros(),
                                        order_place_ms = t4.duration_since(t3).as_millis(),
                                        hot_path_total_ms = t4.duration_since(t0).as_millis(),
                                        "SNIPER HOT PATH LATENCY (to first order HTTP OK)"
                                    );
                                } else if dual_mode && i == 1 {
                                    info!(
                                        market = %self.market.slug,
                                        second_leg_place_ms = t4.duration_since(place_start).as_millis(),
                                        "SNIPER: dual second leg place latency"
                                    );
                                }
                                
                                info!(
                                    market = %self.market.slug,
                                    side = %side_name,
                                    best_ask = %best_ask,
                                    maker_price = %maker_price,
                                    size = %order_size,
                                    order_id = %oid,
                                    dual_mode = dual_mode,
                                    "Sniper GTC order placed"
                                );
                                placed.push(ActiveSniperLeg {
                                    order_id: oid.clone(),
                                    side_name: side_name.clone(),
                                    maker_price: *maker_price,
                                    order_size,
                                    fill_handled: false,
                                    fully_filled: false,
                                    alive: true,
                                });
                                // Register order in global active GTC orders map
                                self.active_gtc_orders.insert(
                                    oid,
                                    (
                                        self.execution.clone(),
                                        self.market.slug.clone(),
                                        self.market.period,
                                        self.market.round_start,
                                    ),
                                );
                                if let Some(ref tx) = self.tui_tx {
                                    let side_display =
                                        if *side_name == "YES" { "UP" } else { "DOWN" };
                                    let t = Local::now().format("%H:%M:%S").to_string();
                                    let log_msg = format!(
                                        "{} {} {} {} GTC PLACED {}@{}",
                                        t,
                                        self.market.coin.as_str(),
                                        format!("{}m", self.market.period.as_minutes()),
                                        side_display,
                                        order_size,
                                        maker_price
                                    );
                                    let _ = tx.try_send(TuiEvent::TradeLog(log_msg));
                                }
                            }
                            Err(e) => {
                                place_err = Some(e.into());
                                break;
                            }
                        }
                    }

                    if let Some(e) = place_err {
                        self.log_sniper_decision("place_order_failed", Some(format!("{e:#}")), None);
                        for k in &reserved_keys {
                            self.pending_sniper_entries.remove(k);
                        }
                        let dual_reconcile = placed.len() >= 2;
                        for leg in &placed {
                            let _ = self.execution.cancel_order(&leg.order_id).await;
                            let _ = self
                                .reconcile_late_fill(
                                    &leg.order_id,
                                    &leg.side_name,
                                    leg.maker_price,
                                    &status_key,
                                    None,
                                    None,
                                    dual_reconcile,
                                )
                                .await;
                        }
                        // Remove orders from active map
                        for leg in &placed {
                            self.active_gtc_orders.remove(&leg.order_id);
                        }
                        self.strategy_status.insert(status_key.clone(), "Snp: Failed".to_string());
                        error!(market = %self.market.slug, error = %e, "Failed to place sniper order(s)");
                        // Wait for next price event (event-driven, no polling delay)
                        continue;
                    }

                    if dual_mode {
                        if let (Some(yes_p), Some(no_p)) = (
                            plans.iter().find(|p| p.0 == "YES").map(|p| p.3),
                            plans.iter().find(|p| p.0 == "NO").map(|p| p.3),
                        ) {
                            self.last_dual_makers = Some((yes_p, no_p));
                        }
                        self.last_order_price = None;
                    } else {
                        self.last_order_price = Some(plans[0].3);
                        self.last_dual_makers = None;
                    }

                    active_legs = placed;
                    self.strategy_status.insert(status_key.clone(), "Snp: Live".to_string());

                    // Does not return to `tokio::select!` above until this loop exits — `fallback_interval`
                    // and price-event wakeups do not apply while polling resting GTC status.
                    'poll: loop {
                        if shutdown.is_cancelled() {
                            break 'poll;
                        }

                        let poll_iter_start = Instant::now();
                        let now = Utc::now().timestamp();
                        let tr = self.market.round_end - now;
                        let dual_poll = active_legs.len() >= 2;

                        if tr <= 5 {
                            for leg in &active_legs {
                                let _ = self.execution.cancel_order(&leg.order_id).await;
                            }
                            let mut any_fill = false;
                            for leg in &active_legs {
                                if self
                                    .reconcile_late_fill(
                                        &leg.order_id,
                                        &leg.side_name,
                                        leg.maker_price,
                                        &status_key,
                                        None,
                                        None,
                                        dual_poll,
                                    )
                                    .await
                                    .is_some()
                                {
                                    any_fill = true;
                                } else {
                                    self.remove_pending_side(&leg.side_name);
                                }
                            }
                            if !any_fill {
                                self.strategy_status
                                    .insert(status_key.clone(), "Snp: Cancelled".to_string());
                            }
                            // Remove from active orders
                            for leg in &active_legs {
                                self.active_gtc_orders.remove(&leg.order_id);
                            }
                            active_legs.clear();
                            break 'poll;
                        }

                        for leg in &mut active_legs {
                            if !leg.alive || (dual_poll && leg.fully_filled) {
                                continue;
                            }

                            match self.execution.get_order_status(&leg.order_id).await {
                                Ok(Some((status, size_matched))) => {
                                    let st = status.to_lowercase();

                                    if dual_poll {
                                        if Self::order_fully_filled(&status, size_matched, leg.order_size)
                                            && !leg.fill_handled
                                        {
                                            if self.record_sniper_fill_if_new(
                                                &leg.order_id,
                                                &leg.side_name,
                                                leg.maker_price,
                                                size_matched,
                                                &status_key,
                                                None,
                                                true, // dual_poll_second_leg: second leg updates existing OPEN
                                            ) {
                                                leg.fill_handled = true;
                                                leg.fully_filled = true;
                                                // Increment global filled count
                                                let new_count = self.gtc_filled_count.fetch_add(1, Ordering::Relaxed) + 1;
                                                let max_f = self.max_primary_fills_per_round();
                                                if new_count < max_f {
                                                    self.strategy_status
                                                        .insert(status_key.clone(), "Snp: Live".to_string());
                                                } else if new_count == max_f {
                                                    self.cancel_all_remaining_gtc_orders().await;
                                                    self.strategy_status
                                                        .insert(status_key.clone(), "Snp: Capped".to_string());
                                                }
                                            }
                                        }
                                    } else if Self::order_status_indicates_fill(&status, size_matched) {
                                        if self.record_sniper_fill_if_new(
                                            &leg.order_id,
                                            &leg.side_name,
                                            leg.maker_price,
                                            size_matched,
                                            &status_key,
                                            None,
                                            false, // single-leg primary
                                        ) {
                                            // Increment global filled count
                                            let new_count = self.gtc_filled_count.fetch_add(1, Ordering::Relaxed) + 1;
                                            let max_f = self.max_primary_fills_per_round();
                                            if new_count >= max_f {
                                                self.cancel_all_remaining_gtc_orders().await;
                                                self.strategy_status
                                                    .insert(status_key.clone(), "Snp: Capped".to_string());
                                            }
                                        }
                                        // Remove from active orders
                                        for leg in &active_legs {
                                            self.active_gtc_orders.remove(&leg.order_id);
                                        }
                                        active_legs.clear();
                                        break 'poll;
                                    }

                                    if st == "cancelled" {
                                        let matched_sz = self
                                            .reconcile_late_fill(
                                                &leg.order_id,
                                                &leg.side_name,
                                                leg.maker_price,
                                                &status_key,
                                                None,
                                                None,
                                                dual_poll,
                                            )
                                            .await;
                                        if dual_poll {
                                            leg.alive = false;
                                            if matched_sz.is_none() {
                                                info!(
                                                    market = %self.market.slug,
                                                    order_id = %leg.order_id,
                                                    "Sniper leg cancelled externally"
                                                );
                                                self.remove_pending_side(&leg.side_name);
                                                self.active_gtc_orders.remove(&leg.order_id);
                                            }
                                        } else {
                                            if matched_sz.is_none() {
                                                info!(
                                                    market = %self.market.slug,
                                                    "Sniper order cancelled externally"
                                                );
                                                self.remove_pending_side(&leg.side_name);
                                                self.strategy_status
                                                    .insert(status_key.clone(), "Snp: Cancelled".to_string());
                                                self.active_gtc_orders.remove(&leg.order_id);
                                            }
                                            // Remove from active orders
                                            for leg in &active_legs {
                                                self.active_gtc_orders.remove(&leg.order_id);
                                            }
                                            active_legs.clear();
                                            break 'poll;
                                        }
                                    }
                                }
                                Ok(None) => {
                                    let matched_sz = self
                                        .reconcile_late_fill(
                                            &leg.order_id,
                                            &leg.side_name,
                                            leg.maker_price,
                                            &status_key,
                                            if dual_poll {
                                                Some(leg.order_size)
                                            } else {
                                                None
                                            },
                                            None,
                                            dual_poll,
                                        )
                                        .await;
                                    if dual_poll {
                                        if matched_sz.is_some() {
                                            leg.fill_handled = true;
                                            leg.fully_filled = true;
                                            // Increment global filled count
                                            let new_count = self.gtc_filled_count.fetch_add(1, Ordering::Relaxed) + 1;
                                            let max_f = self.max_primary_fills_per_round();
                                            if new_count < max_f {
                                                self.strategy_status
                                                    .insert(status_key.clone(), "Snp: Live".to_string());
                                            } else if new_count == max_f {
                                                self.cancel_all_remaining_gtc_orders().await;
                                                self.strategy_status
                                                    .insert(status_key.clone(), "Snp: Capped".to_string());
                                            }
                                        } else {
                                            warn!(
                                                market = %self.market.slug,
                                                order_id = %leg.order_id,
                                                "Sniper leg not found after reconcile"
                                            );
                                            leg.alive = false;
                                            self.remove_pending_side(&leg.side_name);
                                            self.active_gtc_orders.remove(&leg.order_id);
                                        }
                                    } else if let Some(_matched_sz) = matched_sz {
                                        let new_count = self.gtc_filled_count.fetch_add(1, Ordering::Relaxed) + 1;
                                        let max_f = self.max_primary_fills_per_round();
                                        if new_count >= max_f {
                                            self.cancel_all_remaining_gtc_orders().await;
                                            self.strategy_status
                                                .insert(status_key.clone(), "Snp: Capped".to_string());
                                        }
                                        // Remove from active orders
                                        for leg in &active_legs {
                                            self.active_gtc_orders.remove(&leg.order_id);
                                        }
                                        active_legs.clear();
                                        break 'poll;
                                    } else {
                                        warn!(
                                            market = %self.market.slug,
                                            order_id = %leg.order_id,
                                            "Order not found after reconcile window; treating as gone"
                                        );
                                        self.remove_pending_side(&leg.side_name);
                                        self.strategy_status
                                            .insert(status_key.clone(), "Snp: Not found".to_string());
                                        // Remove from active orders
                                        for leg in &active_legs {
                                            self.active_gtc_orders.remove(&leg.order_id);
                                        }
                                        active_legs.clear();
                                        break 'poll;
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        market = %self.market.slug,
                                        order_id = %leg.order_id,
                                        error = %e,
                                        "Failed to get order status, will retry"
                                    );
                                }
                            }
                        }

                        if dual_poll {
                            let full_count = active_legs.iter().filter(|l| l.fully_filled).count();
                            if full_count >= 2 {
                                for leg in &active_legs {
                                    if leg.alive && !leg.fully_filled {
                                        let _ = self.execution.cancel_order(&leg.order_id).await;
                                        let matched = self
                                            .reconcile_late_fill(
                                                &leg.order_id,
                                                &leg.side_name,
                                                leg.maker_price,
                                                &status_key,
                                                None,
                                                None,
                                                true,
                                            )
                                            .await;
                                        if matched.is_none() {
                                            self.remove_pending_side(&leg.side_name);
                                        }
                                        self.active_gtc_orders.remove(&leg.order_id);
                                    }
                                }
                                self.cancel_all_remaining_gtc_orders().await;
                                self.strategy_status.insert(
                                    status_key.clone(),
                                    "Snp: Dual+cx".to_string(),
                                );
                                // Remove all our orders from active map
                                for leg in &active_legs {
                                    self.active_gtc_orders.remove(&leg.order_id);
                                }
                                active_legs.clear();
                                break 'poll;
                            }
                        }

                        sleep(Duration::from_secs(1)).await;
                        let poll_dur = poll_iter_start.elapsed();
                        if poll_dur > Duration::from_secs(2) {
                            let tr_now = (self.market.round_end - Utc::now().timestamp()).max(0);
                            warn!(
                                market = %self.market.slug,
                                poll_iter_ms = poll_dur.as_millis(),
                                time_remaining_secs = tr_now,
                                "sniper poll iteration slow (get_order_status / cancel / reconcile)"
                            );
                        }
                    }
                }
                Ok(None) => {
                    // Status set inside `evaluate_entry_gates_with_orderbooks` (ask sum / threshold).
                }
                Err(e) => {
                    self.strategy_status.insert(status_key.clone(), "Snp: Error".to_string());
                    self.log_sniper_decision("gate_eval_error", Some(format!("{e:#}")), Some(1));
                    error!(market = %self.market.slug, error = %e, "Error evaluating entry gates");
                    let t_err = Instant::now();
                    info!(
                        market = %self.market.slug,
                        emit_to_hot_us = ?emit_to_hot_us,
                        orderbook_read_us = t1.duration_since(t0).as_micros(),
                        gate_eval_err_us = t_err.duration_since(t1).as_micros(),
                        hot_path_err_total_us = t_err.duration_since(t0).as_micros(),
                        "SNIPER HOT PATH LATENCY (gate eval error)"
                    );
                }
            }

            // If round ended, break (polling loop handles cancellation if order still exists)
            if time_remaining <= 0 {
                info!(
                    market = %self.market.slug,
                    "Round ended"
                );
                for leg in &active_legs {
                    self.active_gtc_orders.remove(&leg.order_id);
                }
                break;
            }

            // Event-driven: No sleep needed - we wait for price events or fallback timer
            // The tokio::select! above handles all waiting
        }

        clear_sniper_pending_for_market(
            &self.pending_sniper_entries,
            self.market.coin,
            self.market.period,
            self.market.round_start,
            &self.market.condition_id,
        );
        Ok(())
    }

    /// True if the CLOB reports a non-zero matched size. Do not infer fills from status alone:
    /// `MATCHED` often appears before `size_matched` is populated → false "0 shares FILLED" and wrong P&L.
    fn order_status_indicates_fill(_status: &str, size_matched: Decimal) -> bool {
        size_matched > Decimal::ZERO
    }

    /// Full fill for a leg: matched size meets or exceeds the placed order size (both from API / our placement).
    fn order_fully_filled(_status: &str, size_matched: Decimal, order_size: Decimal) -> bool {
        size_matched > Decimal::ZERO && size_matched >= order_size
    }

    fn maker_price_from_best_ask(
        best_ask: Decimal,
        tick_size: Decimal,
        entry_min_best_ask: Decimal,
    ) -> Option<Decimal> {
        let price_decimals = tick_size
            .to_string()
            .split('.')
            .nth(1)
            .map(|x| x.len())
            .unwrap_or(2) as u32;
        let min_maker_raw = (entry_min_best_ask - tick_size).max(Decimal::ZERO);
        let min_maker_str = format!("{:.prec$}", min_maker_raw, prec = price_decimals as usize);
        let min_maker = Decimal::from_str(&min_maker_str).unwrap_or(min_maker_raw);
        let maker_price = if best_ask >= dec!(1.0) {
            dec!(0.99)
        } else {
            let calculated = (best_ask - tick_size).round_dp(price_decimals);
            if calculated >= dec!(1.0) {
                dec!(0.99)
            } else {
                calculated
            }
        };
        let maker_price_str = format!("{:.prec$}", maker_price, prec = price_decimals as usize);
        let maker_price = Decimal::from_str(&maker_price_str).unwrap_or(maker_price);
        if maker_price >= dec!(1.0) || maker_price < min_maker {
            return None;
        }
        Some(maker_price)
    }

    /// Record fill, TUI, and P&L open position once per `order_id` (idempotent).
    /// `hedge_update_for`: when set, this fill is the hedge leg — update that round-history row to HEDGED.
    /// `dual_poll_second_leg`: when true and `hedge_update_for` is None, try to find an OPEN entry for
    /// this round and update it (second leg of dual YES+NO fill). Prevents duplicate RoundHistoryEntry rows.
    fn record_sniper_fill_if_new(
        &self,
        order_id: &str,
        side_name: &str,
        maker_price: Decimal,
        size_matched: Decimal,
        status_key: &str,
        hedge_update_for: Option<&str>,
        dual_poll_second_leg: bool,
    ) -> bool {
        if self.trade_history.contains_key(order_id) {
            return false;
        }

        if size_matched <= Decimal::ZERO {
            warn!(
                market = %self.market.slug,
                order_id = %order_id,
                side = %side_name,
                size_matched = %size_matched,
                "Refusing fill record: size_matched must be > 0 (wait for CLOB to report matched size)"
            );
            return false;
        }

        let side_display = if side_name == "YES" { "UP" } else { "DOWN" };
        let fill_time = Local::now().format("%H:%M:%S").to_string();
        let log_msg = if hedge_update_for.is_some() {
            format!(
                "{} {} {} hedge {} {}@{} FILLED",
                fill_time,
                self.market.coin.as_str(),
                format!("{}m", self.market.period.as_minutes()),
                side_display,
                size_matched,
                maker_price
            )
        } else {
            format!(
                "{} {} {} {} {}@{} FILLED",
                fill_time,
                self.market.coin.as_str(),
                format!("{}m", self.market.period.as_minutes()),
                side_display,
                size_matched,
                maker_price
            )
        };

        info!(
            market = %self.market.slug,
            side = %side_name,
            price = %maker_price,
            size_matched = %size_matched,
            hedge = hedge_update_for.is_some(),
            "Sniper order FILLED"
        );

        if let Some(ref tx) = self.tui_tx {
            let _ = tx.try_send(TuiEvent::TradeLog(log_msg));
        }

        let filled_trade = FilledTrade {
            slug: self.market.slug.clone(),
            side: side_name.to_string(),
            price: maker_price,
            size_matched,
            condition_id: self.market.condition_id.clone(),
            timestamp: Utc::now(),
        };
        self.trade_history.insert(order_id.to_string(), filled_trade);

        self.pnl_manager.on_fill(
            self.market.slug.clone(),
            self.market.condition_id.clone(),
            side_name.to_string(),
            maker_price,
            size_matched,
        );

        let hedge_rid: Option<String> = hedge_update_for
            .map(|s| s.to_string())
            .or_else(|| {
                if dual_poll_second_leg {
                    self.round_history.lock().ok().and_then(|g| {
                        g.iter()
                            .find(|e| {
                                e.condition_id == self.market.condition_id
                                    && e.round_start == self.market.round_start
                                    && e.status == RoundHistoryStatus::Open
                            })
                            .map(|e| e.id.clone())
                    })
                } else {
                    None
                }
            });

        if let Some(ref rid) = hedge_rid {
            if let Ok(mut g) = self.round_history.lock() {
                if let Some(entry) = g.iter_mut().find(|e| e.id == *rid) {
                    entry.hedge_price = Some(maker_price);
                    entry.hedge_shares = Some(size_matched);
                    // Per matched share pair: sum of leg prices. Realized P&L on locked pairs only.
                    let matched = entry.shares.min(size_matched);
                    let pair_cost = entry.entry_price + maker_price;
                    entry.pair_cost = Some(pair_cost);
                    entry.pnl = Some((Decimal::ONE - pair_cost) * matched);
                    entry.status = RoundHistoryStatus::Hedged;
                    debug!(
                        market = %self.market.slug,
                        entry_id = %rid,
                        hedge_price = %maker_price,
                        hedge_shares = %size_matched,
                        pair_cost = %pair_cost,
                        matched = %matched,
                        pnl = ?entry.pnl,
                        "RoundHistoryEntry UPDATED (hedge fill → HEDGED)"
                    );
                    let period_str = format!("{}m", self.market.period.as_minutes());
                    if let Ok(g) = self.strategy_logger.lock() {
                        if let Err(e) = g.log_hedge(
                            self.market.coin.as_str(),
                            &period_str,
                            self.market.round_start,
                            &entry.side_label,
                            maker_price.to_f64().unwrap_or(0.0),
                            pair_cost.to_f64().unwrap_or(0.0),
                        ) {
                            warn!(error = %e, "strategy_log log_hedge failed");
                        }
                    }
                }
            }
        } else {
            let entry_id = format!("{}:{}", self.market.condition_id, order_id);
            let side_label = if side_name == "YES" {
                "UP".to_string()
            } else {
                "DOWN".to_string()
            };
            let entry = RoundHistoryEntry {
                id: entry_id.clone(),
                fill_time: Utc::now(),
                coin: self.market.coin,
                period: self.market.period,
                side_label: side_label.clone(),
                entry_price: maker_price,
                hedge_price: None,
                hedge_shares: None,
                pair_cost: None,
                shares: size_matched,
                pnl: None,
                status: RoundHistoryStatus::Open,
                condition_id: self.market.condition_id.clone(),
                round_start: self.market.round_start,
                round_end: self.market.round_end,
            };
            if let Ok(mut g) = self.round_history.lock() {
                g.push(entry.clone());
            }
            self.strategy_log_primary_fill(side_name, maker_price, size_matched);
            debug!(
                market = %self.market.slug,
                entry_id = %entry_id,
                side = %side_label,
                price = %maker_price,
                shares = %size_matched,
                "RoundHistoryEntry PUSHED (primary fill)"
            );
        }

        // Do NOT remove pending_sniper_entries on fill. The key must persist for the round to block
        // a second order on the same market+side. Otherwise the next evaluation cycle (after we exit
        // the poll loop) would see the slot free, insert again, and place a duplicate order.
        // Keys are cleared only on round rollover via clear_sniper_pending_for_market.

        self.strategy_status
            .insert(status_key.to_string(), "Snp: Filled".to_string());
        true
    }

    /// After cancel, 404s, or slow CLOB updates, poll until we see a definitive outcome or timeout.
    /// When `require_full_size` is set, only a full fill to that size is recorded and returned.
    /// Second tuple element is whether [`record_sniper_fill_if_new`] returned true (new P&L row).
    async fn reconcile_late_fill(
        &self,
        order_id: &str,
        side_name: &str,
        maker_price: Decimal,
        status_key: &str,
        require_full_size: Option<Decimal>,
        hedge_round_history_id: Option<&str>,
        dual_poll_second_leg: bool,
    ) -> Option<(Decimal, bool)> {
        const MAX_ATTEMPTS: u32 = 45;
        const DELAY_MS: u64 = 500;

        for attempt in 0..MAX_ATTEMPTS {
            match self.execution.get_order_status(order_id).await {
                Ok(Some((status, size_matched))) => {
                    let st = status.to_lowercase();

                    let is_fill = match require_full_size {
                        Some(sz) => size_matched > Decimal::ZERO && size_matched >= sz,
                        None => Self::order_status_indicates_fill(&status, size_matched),
                    };
                    if is_fill {
                        let recorded = self.record_sniper_fill_if_new(
                            order_id,
                            side_name,
                            maker_price,
                            size_matched,
                            status_key,
                            hedge_round_history_id,
                            dual_poll_second_leg,
                        );
                        return Some((size_matched, recorded));
                    }
                    if st == "cancelled" && size_matched == Decimal::ZERO {
                        return None;
                    }
                }
                Ok(None) => {
                    tracing::debug!(
                        market = %self.market.slug,
                        order_id = %order_id,
                        attempt,
                        "Late-fill reconcile: order not in API yet, retrying"
                    );
                }
                Err(e) => {
                    warn!(
                        market = %self.market.slug,
                        order_id = %order_id,
                        error = %e,
                        attempt,
                        "Late-fill reconcile: get_order_status error, retrying"
                    );
                }
            }
            sleep(Duration::from_millis(DELAY_MS)).await;
        }
        None
    }

    #[allow(dead_code)]
    async fn evaluate_entry_gates(
        &self,
    ) -> Result<Option<Vec<(String, String, Decimal)>>> {
        // All sides where best_ask >= SNIPER_ENTRY_MIN_BEST_ASK (often both YES and NO in the final minute).
        let yes_orderbook = self
            .orderbook
            .get_orderbook(&self.market.up_token_id)
            .context("No YES orderbook data")?;
        
        let no_orderbook = self
            .orderbook
            .get_orderbook(&self.market.down_token_id)
            .context("No NO orderbook data")?;
        
        self.evaluate_entry_gates_with_orderbooks(Some(yes_orderbook), Some(no_orderbook), None)
            .await
    }

    async fn evaluate_entry_gates_with_orderbooks(
        &self,
        yes_orderbook_opt: Option<OrderbookState>,
        no_orderbook_opt: Option<OrderbookState>,
        status_key: Option<&str>,
    ) -> Result<Option<Vec<(String, String, Decimal)>>> {
        let yes_orderbook = yes_orderbook_opt.context("No YES orderbook data")?;
        let no_orderbook = no_orderbook_opt.context("No NO orderbook data")?;

        debug!(
            "Orderbook check: up_token_id={} yes_best_bid={:?} yes_best_ask={:?} | down_token_id={} no_best_bid={:?} no_best_ask={:?}",
            self.market.up_token_id,
            yes_orderbook.best_bid(),
            yes_orderbook.best_ask(),
            self.market.down_token_id,
            no_orderbook.best_bid(),
            no_orderbook.best_ask()
        );

        // Sanity check: YES ask + NO ask should sum to approximately $1.00
        let yes_best_ask = yes_orderbook.best_ask();
        let no_best_ask = no_orderbook.best_ask();
        if let (Some(yes_ask), Some(no_ask)) = (yes_best_ask, no_best_ask) {
            let sum = yes_ask + no_ask;
            if sum < dec!(0.90) || sum > dec!(1.10) {
                info!(
                    market = %self.market.slug,
                    "Orderbook sanity check FAILED: yes_ask={} + no_ask={} = {}. Skipping.",
                    yes_ask, no_ask, sum
                );
                self.log_sniper_decision(
                    "orderbook_sanity",
                    Some(format!("yes_ask={yes_ask} no_ask={no_ask} sum={sum}")),
                    Some(3),
                );
                if let Some(k) = status_key {
                    self.strategy_status
                        .insert(k.to_string(), "Snp: Ask sum bad".to_string());
                }
                return Ok(None);
            }
        }

        let threshold = self.config.sniper_entry_min_best_ask;
        let mut sides = Vec::new();
        if let Some(best_ask_yes) = yes_best_ask {
            if best_ask_yes >= threshold {
                sides.push(("YES".to_string(), self.market.up_token_id.clone(), best_ask_yes));
            }
        }
        if let Some(best_ask_no) = no_best_ask {
            if best_ask_no >= threshold {
                sides.push(("NO".to_string(), self.market.down_token_id.clone(), best_ask_no));
            }
        }

        if sides.is_empty() {
            self.log_sniper_decision(
                "no_side_above_threshold",
                Some(format!(
                    "yes_ask={yes_best_ask:?} no_ask={no_best_ask:?} min_best_ask={threshold}"
                )),
                Some(3),
            );
            if let Some(k) = status_key {
                let msg = if yes_best_ask.is_none() && no_best_ask.is_none() {
                    "Snp: No asks"
                } else {
                    "Snp: Ask < entry min"
                };
                self.strategy_status.insert(k.to_string(), msg.to_string());
            }
            Ok(None)
        } else {
            Ok(Some(sides))
        }
    }

    /// Per-leg size when resting GTC on both YES and NO: deploy `balance * capital_deploy_pct`
    /// across the pair (`per * (p_yes + p_no)`), with `per = max(floor(budget/sum_px), min_shares)`.
    fn calculate_per_leg_size_dual(
        &self,
        maker_yes: Decimal,
        maker_no: Decimal,
    ) -> Result<DualLegSizing> {
        strategy_sizing::compute_per_leg_dual(
            *self.balance_receiver.borrow(),
            maker_yes,
            maker_no,
            self.config.sniper_capital_deploy_pct,
            self.config.sniper_min_shares,
        )
    }

    /// `max(floor(balance * capital_deploy_pct / price), min_shares)` (default: 1% or `SNIPER_MIN_SHARES`, whichever yields more shares).
    fn compute_single_leg_sizing(&self, price: Decimal) -> Result<SingleLegSizing> {
        strategy_sizing::compute_single_leg_sizing(
            *self.balance_receiver.borrow(),
            price,
            self.config.sniper_capital_deploy_pct,
            self.config.sniper_min_shares,
        )
    }

    /// Cancel resting sniper-tracked GTC orders for **this** market and round (same slug / period / round_start).
    async fn cancel_all_remaining_gtc_orders(&self) {
        let period = self.market.period;
        let round_start = self.market.round_start;
        let slug = self.market.slug.clone();
        let order_ids: Vec<String> = self
            .active_gtc_orders
            .iter()
            .filter(|e| {
                let v = e.value();
                v.1 == slug && v.2 == period && v.3 == round_start
            })
            .map(|e| e.key().clone())
            .collect();

        for order_id in order_ids {
            if let Some(entry) = self.active_gtc_orders.get(&order_id) {
                let (execution, slug, _, _) = entry.value();
                let execution_clone = execution.clone();
                let slug_clone = slug.clone();
                drop(entry);
                if let Err(e) = execution_clone.cancel_order(&order_id).await {
                    warn!(
                        order_id = %order_id,
                        market = %slug_clone,
                        error = %e,
                        "Failed to cancel remaining GTC order (fill cap)"
                    );
                } else {
                    info!(
                        order_id = %order_id,
                        market = %slug_clone,
                        "Cancelled remaining GTC order after primary fill cap"
                    );
                }
                self.active_gtc_orders.remove(&order_id);
            }
        }
    }
}

/// Clear pending-entry keys for a market round (round rollover / sniper task exit).
pub fn clear_sniper_pending_for_market(
    pending: &DashSet<SniperEntryKey>,
    coin: Coin,
    period: Period,
    round_start: i64,
    condition_id: &str,
) {
    for side in [SniperSide::Yes, SniperSide::No] {
        pending.remove(&SniperEntryKey {
            coin,
            period,
            round_start,
            condition_id: condition_id.to_string(),
            side,
        });
    }
}
