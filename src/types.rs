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
    pub size: Decimal,
}

#[derive(Debug, Clone)]
pub struct OrderbookState {
    pub token_id: String,
    pub bids: Vec<OrderbookLevel>,
    pub asks: Vec<OrderbookLevel>,
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
        self.bids.first().map(|l| l.price)
    }

    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.first().map(|l| l.price)
    }
}

/// Pair cost for spread capture (gabagool): (yes_cost + no_cost) / min(yes_qty, no_qty).
/// Profit guaranteed when pair_cost < 1.0.
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
    pub token_id: String,
    pub side: String, // "BUY" or "SELL"
    pub price: Decimal,
    pub size: Decimal,
    pub filled: Decimal,
    pub status: String, // "OPEN", "FILLED", "CANCELLED"
    pub placed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

    pub fn inventory_key(coin: Coin, period: Period, round_start: i64) -> String {
        format!("{:?}_{:?}_{}", coin, period, round_start)
    }
}

#[derive(Debug, Clone)]
pub struct FeeRate {
    pub token_id: String,
    pub fee_rate_bps: u64,
    pub fetched_at: DateTime<Utc>,
}

impl FeeRate {
    pub fn is_stale(&self, max_age_secs: u64) -> bool {
        let age = (Utc::now() - self.fetched_at).num_seconds() as u64;
        age > max_age_secs
    }
}
