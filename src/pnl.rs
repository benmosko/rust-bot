//! P&L tracking manager: tracks invested, open positions, realized P&L, trades, and win rate.
//! P&L is only calculated when positions resolve (redeemed/claimed), not when orders fill.

use crate::balance::BalanceManager;
use crate::types::{OpenPosition, TuiEvent};
use chrono::Utc;
use dashmap::DashMap;
use rust_decimal::Decimal;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::debug;

/// P&L state manager - tracks all P&L related metrics
pub struct PnLManager {
    // Open positions: key is condition_id + side (to uniquely identify a position)
    open_positions: Arc<DashMap<String, OpenPosition>>,
    /// Cost basis for positions dropped from `open_positions` when a round slot ends (invested already
    /// reduced). Used so `on_resolve` after redeem still computes P&L without double-counting invested.
    pending_redeem_costs: Arc<DashMap<String, Decimal>>,
    // P&L state
    invested: Arc<dashmap::DashMap<(), Decimal>>, // Total cost of open positions
    pnl: Arc<dashmap::DashMap<(), Decimal>>, // Realized P&L from resolved positions only
    trades: Arc<dashmap::DashMap<(), u64>>, // Count of resolved positions
    wins: Arc<dashmap::DashMap<(), u64>>, // Count of winning trades
    tui_tx: Option<mpsc::Sender<TuiEvent>>,
    /// When set, triggers an on-chain USDC refresh after fills/resolves so TUI Balance stays in sync.
    balance: Option<Arc<BalanceManager>>,
}

impl PnLManager {
    pub fn new(tui_tx: Option<mpsc::Sender<TuiEvent>>, balance: Option<Arc<BalanceManager>>) -> Self {
        Self {
            open_positions: Arc::new(DashMap::new()),
            pending_redeem_costs: Arc::new(DashMap::new()),
            invested: Arc::new(dashmap::DashMap::new()),
            pnl: Arc::new(dashmap::DashMap::new()),
            trades: Arc::new(dashmap::DashMap::new()),
            wins: Arc::new(dashmap::DashMap::new()),
            tui_tx,
            balance,
        }
    }

    fn schedule_balance_refresh(&self) {
        let Some(b) = self.balance.clone() else {
            return;
        };
        tokio::spawn(async move {
            if let Err(e) = b.refresh_balance().await {
                debug!(error = %e, "Post-trade balance refresh failed");
            }
        });
    }

    /// Get a reference to open_positions for strategies to add positions
    pub fn open_positions(&self) -> Arc<DashMap<String, OpenPosition>> {
        self.open_positions.clone()
    }

    /// When an order FILLS: Move cost from "available" to "invested". Do NOT touch P&L yet.
    /// Returns the position key for later resolution
    pub fn on_fill(
        &self,
        slug: String,
        condition_id: String,
        side: String,
        fill_price: Decimal,
        fill_size: Decimal,
    ) -> String {
        let cost = fill_price * fill_size;
        
        // Skip if cost is zero (no actual investment made)
        if cost.is_zero() {
            tracing::warn!(
                slug = %slug,
                condition_id = %condition_id,
                side = %side,
                fill_price = %fill_price,
                fill_size = %fill_size,
                "Skipping P&L tracking for fill with zero cost"
            );
            return format!("{}:{}", condition_id, side);
        }
        
        // Add or merge open position (multiple fills on the same side accumulate).
        let position_key = format!("{}:{}", condition_id, side);
        if let Some(mut existing) = self.open_positions.get_mut(&position_key) {
            existing.cost += cost;
            existing.size_matched += fill_size;
        } else {
            let position = OpenPosition {
                slug: slug.clone(),
                condition_id: condition_id.clone(),
                side: side.clone(),
                cost,
                size_matched: fill_size,
                timestamp: Utc::now(),
            };
            self.open_positions.insert(position_key.clone(), position);
        }

        // Update invested: Invested += fill_price × fill_size
        *self.invested.entry(()).or_insert(Decimal::ZERO) += cost;

        // Update open_pos count
        let open_pos_count = self.open_positions.len();
        
        // Send updates to TUI
        if let Some(ref tx) = self.tui_tx {
            let _ = tx.try_send(TuiEvent::InvestedUpdate(self.invested.get(&()).map(|v| *v.value()).unwrap_or(Decimal::ZERO)));
            let _ = tx.try_send(TuiEvent::OpenPosUpdate(open_pos_count));
        }

        self.schedule_balance_refresh();

        position_key
    }

    /// When a round slot ends in the main loop: remove open-position tracking for that market's
    /// `condition_id`, reduce `invested` by those costs, and stash costs for later redeem P&L.
    pub fn clear_round_positions_for_condition(&self, condition_id: &str) {
        let keys: Vec<String> = self
            .open_positions
            .iter()
            .filter(|e| e.value().condition_id == condition_id)
            .map(|e| e.key().clone())
            .collect();

        let mut total_freed = Decimal::ZERO;
        for k in keys {
            if let Some((_, pos)) = self.open_positions.remove(&k) {
                total_freed += pos.cost;
                self.pending_redeem_costs.insert(k, pos.cost);
            }
        }

        if total_freed.is_zero() {
            return;
        }

        if let Some(mut invested_entry) = self.invested.get_mut(&()) {
            *invested_entry -= total_freed;
            if *invested_entry < Decimal::ZERO {
                *invested_entry = Decimal::ZERO;
            }
        }

        let open_pos_count = self.open_positions.len();
        if let Some(ref tx) = self.tui_tx {
            let _ = tx.try_send(TuiEvent::InvestedUpdate(
                self.invested.get(&()).map(|v| *v.value()).unwrap_or(Decimal::ZERO),
            ));
            let _ = tx.try_send(TuiEvent::OpenPosUpdate(open_pos_count));
        }
    }

    /// When a position RESOLVES (redeemed/claimed): Calculate P&L for that trade.
    /// If won: P&L += payout - cost
    /// If lost: P&L += 0 - cost
    /// Invested -= cost
    /// Open Pos -= 1
    /// Trades += 1
    /// Update Win rate
    pub fn on_resolve(&self, condition_id: &str, side: &str, payout: Decimal) -> Option<Decimal> {
        let position_key = format!("{}:{}", condition_id, side);

        let (cost, from_pending) =
            if let Some((_, position)) = self.open_positions.remove(&position_key) {
                (position.cost, false)
            } else if let Some((_, c)) = self.pending_redeem_costs.remove(&position_key) {
                (c, true)
            } else {
                return None;
            };

        // Calculate P&L
        let trade_pnl = if payout > Decimal::ZERO {
            // Won: P&L += payout - cost
            payout - cost
        } else {
            // Lost: P&L += 0 - cost
            Decimal::ZERO - cost
        };

        // Update P&L: P&L += trade_pnl
        *self.pnl.entry(()).or_insert(Decimal::ZERO) += trade_pnl;

        // Update invested: only when resolving from an open position (still counted in invested).
        // If we already cleared the round in `clear_round_positions_for_condition`, invested was reduced then.
        if !from_pending {
            if let Some(mut invested_entry) = self.invested.get_mut(&()) {
                *invested_entry -= cost;
                if *invested_entry < Decimal::ZERO {
                    *invested_entry = Decimal::ZERO; // Prevent negative invested
                }
            }
        }

        // Update trades count: Trades += 1
        *self.trades.entry(()).or_insert(0) += 1;

        // Update wins count if won
        if payout > Decimal::ZERO {
            *self.wins.entry(()).or_insert(0) += 1;
        }

        // Update open_pos count
        let open_pos_count = self.open_positions.len();

        // Send updates to TUI (SessionStats + session P&L come from balance loop: sum of round_history `pnl`)
        if let Some(ref tx) = self.tui_tx {
            let _ = tx.try_send(TuiEvent::InvestedUpdate(self.invested.get(&()).map(|v| *v.value()).unwrap_or(Decimal::ZERO)));
            let _ = tx.try_send(TuiEvent::OpenPosUpdate(open_pos_count));
        }

        self.schedule_balance_refresh();

        Some(trade_pnl)
    }

    /// Get current P&L state
    pub fn get_state(&self) -> (Decimal, Decimal, usize, u64, f64) {
        let invested = self.invested.get(&()).map(|v| *v.value()).unwrap_or(Decimal::ZERO);
        let pnl = self.pnl.get(&()).map(|v| *v.value()).unwrap_or(Decimal::ZERO);
        let open_pos = self.open_positions.len();
        let trades = self.trades.get(&()).map(|v| *v.value()).unwrap_or(0);
        let wins = self.wins.get(&()).map(|v| *v.value()).unwrap_or(0);
        let win_rate = if trades > 0 {
            (wins as f64 / trades as f64) * 100.0
        } else {
            0.0
        };
        (invested, pnl, open_pos, trades, win_rate)
    }

    /// Find open position by condition_id (for redemption manager)
    pub fn find_position_by_condition_id(&self, condition_id: &str) -> Option<OpenPosition> {
        // Search for any position with this condition_id (could be YES or NO)
        self.open_positions
            .iter()
            .find(|entry| entry.value().condition_id == condition_id)
            .map(|entry| entry.value().clone())
    }

    /// Remove position by condition_id and side (for redemption manager)
    pub fn remove_position(&self, condition_id: &str, side: &str) -> Option<OpenPosition> {
        let position_key = format!("{}:{}", condition_id, side);
        self.open_positions.remove(&position_key).map(|(_, pos)| pos)
    }
}
