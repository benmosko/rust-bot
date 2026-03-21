use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Coin {
    Btc,
    Eth,
    Sol,
    Xrp,
}

impl Coin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Coin::Btc => "btc",
            Coin::Eth => "eth",
            Coin::Sol => "sol",
            Coin::Xrp => "xrp",
        }
    }

    pub fn binance_symbol(&self) -> &'static str {
        match self {
            Coin::Btc => "btcusdt",
            Coin::Eth => "ethusdt",
            Coin::Sol => "solusdt",
            Coin::Xrp => "xrpusdt",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "btc" => Some(Coin::Btc),
            "eth" => Some(Coin::Eth),
            "sol" => Some(Coin::Sol),
            "xrp" => Some(Coin::Xrp),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Period {
    Five,
    Fifteen,
    Sixty,
}

impl Period {
    pub fn as_minutes(&self) -> u64 {
        match self {
            Period::Five => 5,
            Period::Fifteen => 15,
            Period::Sixty => 60,
        }
    }

    pub fn as_seconds(&self) -> i64 {
        self.as_minutes() as i64 * 60
    }

    pub fn from_minutes(minutes: u64) -> Option<Self> {
        match minutes {
            5 => Some(Period::Five),
            15 => Some(Period::Fifteen),
            60 => Some(Period::Sixty),
            _ => None,
        }
    }
}

/// YES/NO leg for sniper in-flight placement deduplication (same market; avoids duplicate posts while `place_order` awaits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SniperSide {
    Yes,
    No,
}

/// One sniper entry slot per round: blocks duplicate evaluation cycles for the same leg until round rollover.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SniperEntryKey {
    pub coin: Coin,
    pub period: Period,
    pub round_start: i64,
    pub condition_id: String,
    pub side: SniperSide,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub condition_id: String,
    pub slug: String,
    pub coin: Coin,
    pub period: Period,
    pub round_start: i64,
    pub round_end: i64,
    pub up_token_id: String,
    pub down_token_id: String,
    pub minimum_tick_size: Decimal,
    pub minimum_order_size: Decimal,
    pub neg_risk: bool,
    pub taker_base_fee: Decimal,
    pub maker_base_fee: Decimal,
    pub up_best_bid: Option<Decimal>,
    pub up_best_ask: Option<Decimal>,
    pub down_best_bid: Option<Decimal>,
    pub down_best_ask: Option<Decimal>,
    /// Polymarket/Gamma-reported Chainlink open for the round (when present). Used to fix mid-round bot starts.
    pub opening_price: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Round {
    pub market: Market,
    pub opening_price: Option<Decimal>,
    pub up_fee_rate_bps: Option<u64>,
    pub down_fee_rate_bps: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct SpotState {
    pub price: Decimal,
    pub timestamp: DateTime<Utc>,
    pub opening_price: Option<Decimal>,
    #[allow(dead_code)]
    pub opening_timestamp: Option<DateTime<Utc>>,
}

impl SpotState {
    pub fn is_stale(&self, max_age_secs: u64) -> bool {
        let age = (Utc::now() - self.timestamp).num_seconds() as u64;
        age > max_age_secs
    }
}

#[derive(Debug, Clone)]
pub struct OrderbookLevel {
    pub price: Decimal,
    #[allow(dead_code)]
    pub size: Decimal,
}

#[derive(Debug, Clone)]
pub struct OrderbookState {
    #[allow(dead_code)]
    pub token_id: String,
    pub bids: Vec<OrderbookLevel>,
    pub asks: Vec<OrderbookLevel>,
    #[allow(dead_code)]
    pub timestamp: DateTime<Utc>,
}

impl OrderbookState {
    pub fn midpoint(&self) -> Option<Decimal> {
        let best_bid = self.bids.first()?.price;
        let best_ask = self.asks.first()?.price;
        Some((best_bid + best_ask) / Decimal::from(2))
    }

    pub fn spread(&self) -> Option<Decimal> {
        let best_bid = self.bids.first()?.price;
        let best_ask = self.asks.first()?.price;
        Some(best_ask - best_bid)
    }

    pub fn best_bid(&self) -> Option<Decimal> {
        self.bids.iter().map(|l| l.price).max()
    }

    #[allow(dead_code)]
    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.iter().map(|l| l.price).min()
    }
}

/// Hedged pair cost: (yes_cost + no_cost) / min(yes_qty, no_qty) — cost per matched share pair.
/// For average-price gabagool metrics, see spread capture (yes_avg + no_avg).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairCost {
    pub yes_qty: Decimal,
    pub yes_cost: Decimal,
    pub no_qty: Decimal,
    pub no_cost: Decimal,
    pub pair_cost: Decimal,
}

impl PairCost {
    pub fn new(yes_qty: Decimal, yes_cost: Decimal, no_qty: Decimal, no_cost: Decimal) -> Self {
        let min_qty = yes_qty.min(no_qty);
        let pair_cost = if min_qty > Decimal::ZERO {
            (yes_cost + no_cost) / min_qty
        } else {
            Decimal::ZERO
        };
        Self {
            yes_qty,
            yes_cost,
            no_qty,
            no_cost,
            pair_cost,
        }
    }

    #[allow(dead_code)]
    pub fn imbalance_pct(&self) -> Decimal {
        let max_qty = self.yes_qty.max(self.no_qty);
        if max_qty <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        (self.yes_qty - self.no_qty).abs() / max_qty
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryState {
    pub yes_shares: Decimal,
    pub no_shares: Decimal,
    pub round_start: i64,
}

impl InventoryState {
    pub fn imbalance(&self) -> Decimal {
        if self.yes_shares > self.no_shares {
            self.yes_shares - self.no_shares
        } else {
            self.no_shares - self.yes_shares
        }
    }

    #[allow(dead_code)]
    pub fn total_shares(&self) -> Decimal {
        self.yes_shares + self.no_shares
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub token_id: String,
    pub shares: Decimal,
    pub round_start: i64,
    pub entry_price: Decimal,
    pub side: String, // "Up" or "Down"
}

#[derive(Debug, Clone)]
pub struct OrderState {
    pub order_id: String,
    #[allow(dead_code)]
    pub token_id: String,
    #[allow(dead_code)]
    pub side: String, // "BUY" or "SELL"
    #[allow(dead_code)]
    pub price: Decimal,
    #[allow(dead_code)]
    pub size: Decimal,
    #[allow(dead_code)]
    pub filled: Decimal,
    pub status: String, // "OPEN", "FILLED", "CANCELLED"
    #[allow(dead_code)]
    pub placed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct TradeResult {
    pub round_start: i64,
    pub coin: String,
    pub period: u64,
    pub entry_price: Decimal,
    pub exit_price: Decimal,
    pub shares: Decimal,
    pub pnl: Decimal,
    pub mode: String, // "market_maker" or "sniper"
    pub timestamp: DateTime<Utc>,
}

/// Filled trade entry for P&L tracking and redemption manager
#[derive(Debug, Clone)]
pub struct FilledTrade {
    pub slug: String,
    pub side: String, // "YES" or "NO" / "Up" or "Down"
    pub price: Decimal,
    pub size_matched: Decimal,
    pub condition_id: String,
    pub timestamp: DateTime<Utc>,
}

/// Open position entry - tracks cost until resolution
#[derive(Debug, Clone)]
pub struct OpenPosition {
    pub slug: String,
    pub condition_id: String,
    pub side: String, // "YES" or "NO"
    pub cost: Decimal, // price × size_matched
    pub size_matched: Decimal,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotState {
    pub positions: Vec<Position>,
    pub inventory: HashMap<String, InventoryState>, // key: "coin_period_round_start"
    pub daily_pnl: Decimal,
    pub daily_start_balance: Decimal,
    pub last_updated: DateTime<Utc>,
}

impl BotState {
    pub fn new(start_balance: Decimal) -> Self {
        Self {
            positions: Vec::new(),
            inventory: HashMap::new(),
            daily_pnl: Decimal::ZERO,
            daily_start_balance: start_balance,
            last_updated: Utc::now(),
        }
    }

    #[allow(dead_code)]
    pub fn inventory_key(coin: Coin, period: Period, round_start: i64) -> String {
        format!("{:?}_{:?}_{}", coin, period, round_start)
    }
}

#[derive(Debug, Clone)]
pub struct FeeRate {
    #[allow(dead_code)]
    pub token_id: String,
    #[allow(dead_code)]
    pub fee_rate_bps: u64,
    #[allow(dead_code)]
    pub fetched_at: DateTime<Utc>,
}

impl FeeRate {
    pub fn is_stale(&self, max_age_secs: u64) -> bool {
        let age = (Utc::now() - self.fetched_at).num_seconds() as u64;
        age > max_age_secs
    }
}

// ---- Round history (sniper positions → TUI) ----

/// Settlement state for one sniper position row in Round History.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundHistoryStatus {
    /// Sniper filled; hedge not yet filled (or round not yet settled for unhedged).
    Open,
    /// Both legs filled; P&L from locked pair cost.
    Hedged,
    /// Unhedged winner after round resolution.
    Won,
    /// Unhedged loser after round resolution.
    Lost,
}

/// One row: sniper leg (and optional hedge) for the Round History TUI panel.
#[derive(Debug, Clone)]
pub struct RoundHistoryEntry {
    pub id: String,
    pub fill_time: DateTime<Utc>,
    pub coin: Coin,
    pub period: Period,
    /// Display "UP" or "DOWN" (sniper / primary leg).
    pub side_label: String,
    pub entry_price: Decimal,
    pub hedge_price: Option<Decimal>,
    /// Filled hedge size when hedged (for `min(sniper, hedge)` P&L).
    pub hedge_shares: Option<Decimal>,
    pub pair_cost: Option<Decimal>,
    /// Sniper (primary) filled shares.
    pub shares: Decimal,
    /// `None` while status is OPEN and not yet valued; set for HEDGED and after Gamma resolution for WON/LOST.
    pub pnl: Option<Decimal>,
    pub status: RoundHistoryStatus,
    pub condition_id: String,
    pub round_start: i64,
    pub round_end: i64,
}

impl RoundHistoryEntry {
    /// Unhedged win after resolution: `(1 - entry_price) * shares`.
    pub fn pnl_if_resolved_won(&self) -> Decimal {
        (Decimal::ONE - self.entry_price) * self.shares
    }

    /// Unhedged loss after resolution: `-(entry_price * shares)`.
    pub fn pnl_if_resolved_lost(&self) -> Decimal {
        -(self.entry_price * self.shares)
    }

    /// Unhedged win: `(1 - entry) * shares`. Unhedged loss: `-entry * shares`.
    pub fn pnl_unhedged_if_resolved(&self, up_won: bool) -> Decimal {
        let pos_up = self.side_label == "UP";
        let won = (up_won && pos_up) || (!up_won && !pos_up);
        if won {
            self.pnl_if_resolved_won()
        } else {
            self.pnl_if_resolved_lost()
        }
    }

    /// Session P&L: sum of `pnl` where set (hedged arb or resolved unhedged). OPEN rows contribute nothing.
    pub fn sum_session_pnl(entries: &[RoundHistoryEntry]) -> Decimal {
        entries.iter().filter_map(|e| e.pnl).sum()
    }

    /// Resolved positions: entries with realized P&L (hedged or post-resolution).
    pub fn resolved_trades_count(entries: &[RoundHistoryEntry]) -> u64 {
        entries.iter().filter(|e| e.pnl.is_some()).count() as u64
    }

    /// Win rate: (HEDGED + WON) / resolved count × 100. LOST counts as resolved but not a win.
    pub fn session_win_rate_pct(entries: &[RoundHistoryEntry]) -> f64 {
        let mut resolved = 0usize;
        let mut wins = 0usize;
        for e in entries {
            if e.pnl.is_none() {
                continue;
            }
            resolved += 1;
            if matches!(e.status, RoundHistoryStatus::Hedged | RoundHistoryStatus::Won) {
                wins += 1;
            }
        }
        if resolved == 0 {
            0.0
        } else {
            (wins as f64 / resolved as f64) * 100.0
        }
    }
}

// ---- TUI dashboard state and events ----

/// Events sent from bot modules to the TUI over a single mpsc channel.
#[derive(Debug, Clone)]
pub enum TuiEvent {
    BalanceUpdate(Decimal),
    RoundUpdate {
        coin: Coin,
        period: Period,
        round_start: i64,
        elapsed_pct: f64,
        yes_price: Decimal,
        no_price: Decimal,
        strategy: String,
        status: String,
    },
    #[allow(dead_code)]
    SpreadUpdate {
        round_key: String,
        pair_cost: Decimal,
        imbalance_pct: f64,
        est_profit: Decimal,
    },
    /// Formatted log line for the trade log panel.
    TradeLog(String),
    /// Cumulative P&L (e.g. daily).
    PnlUpdate(Decimal),
    /// Session stats: uptime, rebates, etc. Header P&L / win / trades are derived from `round_history` in the TUI.
    SessionStats {
        rounds: u64,
        uptime_secs: u64,
        avg_pair_cost: Option<Decimal>,
        rebates: Decimal,
        /// Starting wallet balance for session return % (header: session P&L / this).
        session_start_balance: Decimal,
    },
    /// Invested amount (open positions value).
    #[allow(dead_code)]
    InvestedUpdate(Decimal),
    /// Open positions count.
    #[allow(dead_code)]
    OpenPosUpdate(usize),
    /// Pause/resume display (actual pause is handled in main).
    #[allow(dead_code)]
    Paused(bool),
}

/// One row in the Active Rounds table.
#[derive(Debug, Clone)]
pub struct TuiRoundRow {
    pub coin: Coin,
    pub period: Period,
    pub round_start: i64,
    pub elapsed_pct: f64,
    pub yes_price: Decimal,
    pub no_price: Decimal,
    pub strategy: String,
    pub status: String,
}

/// One row in the Spread Capture table.
#[derive(Debug, Clone)]
pub struct TuiSpreadRow {
    pub round_key: String,
    pub pair_cost: Decimal,
    pub imbalance_pct: f64,
    pub est_profit: Decimal,
}

/// All dashboard state consumed by the TUI.
#[derive(Debug, Clone)]
pub struct TuiData {
    /// Rows shown in Active Rounds (from `COINS` × `PERIODS`); avoids fake Waiting rows for markets the bot does not track.
    pub market_slots: Vec<(Coin, Period)>,
    /// Session start time for computing uptime on every render (Instant::now() - session_start).
    pub session_start: std::time::Instant,
    pub balance: Decimal,
    pub invested: Decimal,
    pub open_pos: usize,
    pub rounds: u64,
    pub avg_pair_cost: Option<Decimal>,
    pub rebates: Decimal,
    /// Used with event-driven session P&L from round history to show return vs starting balance.
    pub session_start_balance: Decimal,
    pub active_rounds: Vec<TuiRoundRow>,
    pub spread_captures: Vec<TuiSpreadRow>,
    pub pnl_history: Vec<f64>,
    pub trade_log: Vec<String>,
    pub paused: bool,
}

impl Default for TuiData {
    fn default() -> Self {
        Self {
            market_slots: Vec::new(),
            session_start: std::time::Instant::now(),
            balance: Decimal::ZERO,
            invested: Decimal::ZERO,
            open_pos: 0,
            rounds: 0,
            avg_pair_cost: None,
            rebates: Decimal::ZERO,
            session_start_balance: Decimal::ZERO,
            active_rounds: Vec::new(),
            spread_captures: Vec::new(),
            pnl_history: Vec::new(),
            trade_log: Vec::new(),
            paused: false,
        }
    }
}

/// Helper function to generate strategy status key
pub fn strategy_status_key(coin: Coin, period: Period, round_start: i64, strategy_name: &str) -> String {
    format!("{}_{}_{}_{}", coin.as_str(), period.as_minutes(), round_start, strategy_name)
}
