//! Strategy 2: Momentum / Directional (Binance signal following).
//! Four modes: early momentum, mid-round, late confirmation, preemptive cancel.

use crate::config::Config;
use crate::execution::ExecutionEngine;
use crate::orderbook::OrderbookManager;
use crate::risk::RiskManager;
use crate::types::{Market, SpotState, strategy_status_key};
use anyhow::{Context, Result};
use chrono::Utc;
use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::time::{sleep, Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

pub struct Momentum {
    market: Market,
    config: Arc<Config>,
    execution: Arc<ExecutionEngine>,
    orderbook: Arc<OrderbookManager>,
    spot_receiver: watch::Receiver<SpotState>,
    risk: Arc<RiskManager>,
    opening_price: Decimal,
    open_order_id: Option<String>,
    open_order_side: Option<String>, // "Up" | "Down"
    open_order_placed_at: Option<Instant>,
    strategy_status: Arc<DashMap<String, String>>,
}

impl Momentum {
    pub fn new(
        market: Market,
        config: Arc<Config>,
        execution: Arc<ExecutionEngine>,
        orderbook: Arc<OrderbookManager>,
        spot_receiver: watch::Receiver<SpotState>,
        risk: Arc<RiskManager>,
        opening_price: Decimal,
        strategy_status: Arc<DashMap<String, String>>,
    ) -> Self {
        let status_key = strategy_status_key(market.coin, market.period, market.round_start, "momentum");
        strategy_status.insert(status_key.clone(), "Watching".to_string());
        Self {
            market,
            config,
            execution,
            orderbook,
            spot_receiver,
            risk,
            opening_price,
            open_order_id: None,
            open_order_side: None,
            open_order_placed_at: None,
            strategy_status,
        }
    }

    /// Early momentum: >0.3% spot move in 60s in first half of round.
    fn check_early_momentum(&self, spot_price: Decimal, opening_price: Decimal, elapsed_pct: f64) -> Option<(bool, String)> {
        if elapsed_pct > 0.5 {
            return None;
        }
        // Guard against division by zero
        if opening_price.is_zero() {
            return None;
        }
        let move_pct = (spot_price - opening_price).abs() / opening_price;
        if move_pct >= self.config.mom_min_spot_move_pct {
            let up = spot_price > opening_price;
            let side = if up { "Up" } else { "Down" };
            return Some((up, side.to_string()));
        }
        None
    }

    /// Late confirmation: final 30s, direction ~85% certain, buy at 85–95¢.
    fn check_late_confirmation(&self, spot_price: Decimal, opening_price: Decimal, time_remaining: i64) -> Option<(bool, String)> {
        if time_remaining > 30 || time_remaining <= 0 {
            return None;
        }
        // Guard against zero opening price
        if opening_price.is_zero() {
            return None;
        }
        let up = spot_price > opening_price;
        let side = if up { "Up" } else { "Down" };
        Some((up, side.to_string()))
    }

    /// Preemptive cancel: if Binance reverses while we have an open order, cancel within 100ms.
    async fn check_preemptive_cancel(&mut self) -> Result<()> {
        let (order_id, side) = match (self.open_order_id.as_ref(), self.open_order_side.as_ref()) {
            (Some(id), Some(s)) => (id.clone(), s.clone()),
            _ => return Ok(()),
        };
        let placed_at = match self.open_order_placed_at {
            Some(t) => t,
            None => return Ok(()),
        };
        if placed_at.elapsed() < Duration::from_millis(self.config.mom_preemptive_cancel_ms) {
            return Ok(());
        }
        let spot = self.spot_receiver.borrow().clone();
        if spot.is_stale(2) {
            return Ok(());
        }
        // Get effective opening price from spot feed
        let effective_opening_price = spot.opening_price.unwrap_or(spot.price);
        // Guard against division by zero
        if spot.price.is_zero() || effective_opening_price.is_zero() {
            return Ok(());
        }
        let current_up = spot.price > effective_opening_price;
        let order_was_up = side == "Up";
        if current_up != order_was_up {
            if let Err(e) = self.execution.cancel_order(&order_id).await {
                error!(order_id = %order_id, error = %e, "Preemptive cancel failed");
            } else {
                info!(order_id = %order_id, side = %side, "Preemptive cancel (Binance reversed)");
                self.open_order_id = None;
                self.open_order_side = None;
                self.open_order_placed_at = None;
            }
        }
        Ok(())
    }

    pub async fn run(
        &mut self,
        shutdown: CancellationToken,
    ) -> Result<()> {
        info!(
            market = %self.market.slug,
            round_start = self.market.round_start,
            opening_price = %self.opening_price,
            "Momentum strategy started"
        );

        let status_key = strategy_status_key(self.market.coin, self.market.period, self.market.round_start, "momentum");
        let order_size = (self.market.minimum_order_size * dec!(10)).round_dp(2); // small size for momentum, rounded to 2dp
        let tick = self.market.minimum_tick_size;

        while !shutdown.is_cancelled() {
            let now = Utc::now().timestamp();
            let time_remaining = self.market.round_end - now;
            let period_secs = self.market.period.as_seconds();
            let elapsed_pct = (now - self.market.round_start) as f64 / period_secs as f64;

            if time_remaining <= 0 {
                self.strategy_status.insert(status_key.clone(), "Ended".to_string());
                break;
            }

            self.check_preemptive_cancel().await?;

            if self.risk.is_sniper_paused().await {
                self.strategy_status.insert(status_key.clone(), "Paused".to_string());
                sleep(Duration::from_secs(5)).await;
                continue;
            }

            let spot = self.spot_receiver.borrow().clone();
            if spot.is_stale(3) {
                self.strategy_status.insert(status_key.clone(), "Stale spot".to_string());
                sleep(Duration::from_millis(500)).await;
                continue;
            }
            let spot_price = spot.price;

            // Get effective opening price from spot feed (use stored one if valid, otherwise from spot feed)
            let effective_opening_price = if !self.opening_price.is_zero() {
                self.opening_price
            } else {
                spot.opening_price.unwrap_or(spot.price)
            };

            // Wait for valid spot price and opening price before calculations
            if spot_price.is_zero() || effective_opening_price.is_zero() {
                self.strategy_status.insert(status_key.clone(), "Waiting for price".to_string());
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            // Resolve which side to buy (early or late).
            let (is_up, side_str) = if time_remaining <= 30 {
                if let Some((up, s)) = self.check_late_confirmation(spot_price, effective_opening_price, time_remaining) {
                    (up, s)
                } else {
                    self.strategy_status.insert(status_key.clone(), "Waiting".to_string());
                    sleep(Duration::from_millis(200)).await;
                    continue;
                }
            } else if let Some((up, s)) = self.check_early_momentum(spot_price, effective_opening_price, elapsed_pct) {
                (up, s)
            } else {
                self.strategy_status.insert(status_key.clone(), "Watching".to_string());
                sleep(Duration::from_millis(500)).await;
                continue;
            };

            let token_id = if is_up {
                &self.market.up_token_id
            } else {
                &self.market.down_token_id
            };

            let ob = self.orderbook.get_orderbook(token_id).context("No orderbook")?;
            let best_bid = ob.best_bid().context("No best bid")?;
            if time_remaining <= 30 && best_bid < self.config.mom_late_entry_min_bid {
                self.strategy_status.insert(status_key.clone(), "Waiting for price".to_string());
                sleep(Duration::from_millis(200)).await;
                continue;
            }

            let order_price = (best_bid + tick).min(dec!(0.99));

            if let Some(ref oid) = self.open_order_id {
                let _ = self.execution.cancel_order(oid).await;
                self.open_order_id = None;
                self.open_order_side = None;
            }

            self.strategy_status.insert(status_key.clone(), format!("Entry signal: {}", side_str));
            match self
                .execution
                .place_order(
                    token_id,
                    polymarket_client_sdk::clob::types::Side::Buy,
                    order_price,
                    order_size,
                    self.market.minimum_tick_size,
                )
                .await
            {
                Ok(oid) => {
                    self.open_order_id = Some(oid.clone());
                    self.open_order_side = Some(side_str.clone());
                    self.open_order_placed_at = Some(Instant::now());
                    self.strategy_status.insert(status_key.clone(), format!("Momentum: {} order", side_str));
                    info!(
                        market = %self.market.slug,
                        side = %side_str,
                        price = %order_price,
                        order_id = %oid,
                        "Momentum order placed"
                    );
                }
                Err(e) => {
                    self.strategy_status.insert(status_key.clone(), "Order failed".to_string());
                    warn!(market = %self.market.slug, error = %e, "Momentum order failed");
                }
            }

            sleep(Duration::from_millis(500)).await;
        }

        if let Some(oid) = self.open_order_id.take() {
            let _ = self.execution.cancel_order(&oid).await;
        }
        Ok(())
    }
}
