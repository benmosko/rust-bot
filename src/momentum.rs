//! Strategy 2: Momentum / Directional (Binance signal following).
//! Four modes: early momentum, mid-round, late confirmation, preemptive cancel.

use crate::config::Config;
use crate::execution::ExecutionEngine;
use crate::orderbook::OrderbookManager;
use crate::pnl::PnLManager;
use crate::risk::RiskManager;
use crate::strategy_sizing;
use crate::types::{FilledTrade, Market, SpotState, TuiEvent, strategy_status_key};
use anyhow::{Context, Result};
use chrono::{Local, Utc};
use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tokio::time::{sleep, Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

struct ActiveMomentumLeg {
    order_id: String,
    /// "YES" (Up) or "NO" (Down) — matches PnL / trade history with other strategies.
    side_name: String,
    limit_price: Decimal,
    order_size: Decimal,
    applied_matched: Decimal,
    placed_at: Instant,
}

pub struct Momentum {
    market: Market,
    config: Arc<Config>,
    execution: Arc<ExecutionEngine>,
    orderbook: Arc<OrderbookManager>,
    spot_receiver: watch::Receiver<SpotState>,
    risk: Arc<RiskManager>,
    opening_price: Decimal,
    strategy_status: Arc<DashMap<String, String>>,
    balance_receiver: watch::Receiver<Decimal>,
    pnl_manager: Arc<PnLManager>,
    trade_history: Arc<DashMap<String, FilledTrade>>,
    tui_tx: Option<mpsc::Sender<TuiEvent>>,
    active_leg: Option<ActiveMomentumLeg>,
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
        balance_receiver: watch::Receiver<Decimal>,
        pnl_manager: Arc<PnLManager>,
        trade_history: Arc<DashMap<String, FilledTrade>>,
        tui_tx: Option<mpsc::Sender<TuiEvent>>,
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
            strategy_status,
            balance_receiver,
            pnl_manager,
            trade_history,
            tui_tx,
            active_leg: None,
        }
    }

    fn order_status_indicates_fill(_status: &str, size_matched: Decimal) -> bool {
        size_matched > Decimal::ZERO
    }

    fn order_fully_filled(_status: &str, size_matched: Decimal, order_size: Decimal) -> bool {
        size_matched > Decimal::ZERO && size_matched >= order_size
    }

    fn apply_matched_from_clob(
        &mut self,
        leg: &mut ActiveMomentumLeg,
        size_matched: Decimal,
        status_key: &str,
    ) {
        let delta = size_matched - leg.applied_matched;
        if delta <= Decimal::ZERO {
            return;
        }

        leg.applied_matched = size_matched;

        self.pnl_manager.on_fill(
            self.market.slug.clone(),
            self.market.condition_id.clone(),
            leg.side_name.clone(),
            leg.limit_price,
            delta,
        );

        let side_display = if leg.side_name == "YES" { "UP" } else { "DOWN" };
        let fill_time = Local::now().format("%H:%M:%S").to_string();
        let log_msg = format!(
            "{} {} {} {} {}@{} filled (+{})",
            fill_time,
            self.market.coin.as_str(),
            format!("{}m", self.market.period.as_minutes()),
            side_display,
            size_matched,
            leg.limit_price,
            delta
        );
        if let Some(ref tx) = self.tui_tx {
            let _ = tx.try_send(TuiEvent::TradeLog(log_msg));
        }

        if let Some(mut existing) = self.trade_history.get_mut(&leg.order_id) {
            existing.size_matched = size_matched;
        } else {
            self.trade_history.insert(
                leg.order_id.clone(),
                FilledTrade {
                    slug: self.market.slug.clone(),
                    side: leg.side_name.clone(),
                    price: leg.limit_price,
                    size_matched,
                    condition_id: self.market.condition_id.clone(),
                    timestamp: Utc::now(),
                },
            );
        }

        self.strategy_status.insert(
            status_key.to_string(),
            format!("Momentum: {} filled", side_display),
        );
        debug!(
            market = %self.market.slug,
            side = %leg.side_name,
            size_matched = %size_matched,
            delta = %delta,
            "Momentum fill applied"
        );
    }

    async fn reconcile_after_cancel_or_missing(&mut self, leg: &mut ActiveMomentumLeg, status_key: &str) {
        const MAX_ATTEMPTS: u32 = 45;
        const DELAY_MS: u64 = 500;

        for attempt in 0..MAX_ATTEMPTS {
            match self.execution.get_order_status(&leg.order_id).await {
                Ok(Some((status, size_matched))) => {
                    let st = status.to_lowercase();
                    if Self::order_status_indicates_fill(&status, size_matched) {
                        self.apply_matched_from_clob(leg, size_matched, status_key);
                        return;
                    }
                    if st == "cancelled" && size_matched == Decimal::ZERO {
                        return;
                    }
                }
                Ok(None) => {
                    debug!(
                        market = %self.market.slug,
                        order_id = %leg.order_id,
                        attempt,
                        "Momentum reconcile: order not in API yet, retrying"
                    );
                }
                Err(e) => {
                    warn!(
                        market = %self.market.slug,
                        order_id = %leg.order_id,
                        error = %e,
                        attempt,
                        "Momentum reconcile: get_order_status error, retrying"
                    );
                }
            }
            sleep(Duration::from_millis(DELAY_MS)).await;
        }
    }

    async fn poll_active_leg(&mut self, status_key: &str) {
        let Some(mut leg) = self.active_leg.take() else {
            return;
        };

        match self.execution.get_order_status(&leg.order_id).await {
            Ok(Some((status, size_matched))) => {
                let st = status.to_lowercase();
                if size_matched > leg.applied_matched {
                    self.apply_matched_from_clob(&mut leg, size_matched, status_key);
                }
                if st == "cancelled" {
                    return;
                }
                if Self::order_fully_filled(&status, size_matched, leg.order_size) {
                    return;
                }
                self.active_leg = Some(leg);
            }
            Ok(None) => {
                self.reconcile_after_cancel_or_missing(&mut leg, status_key).await;
            }
            Err(e) => {
                warn!(
                    market = %self.market.slug,
                    order_id = %leg.order_id,
                    error = %e,
                    "Momentum: get_order_status failed, will retry next tick"
                );
                self.active_leg = Some(leg);
            }
        }
    }

    async fn cancel_round_cleanup(&mut self, status_key: &str) {
        if let Some(mut leg) = self.active_leg.take() {
            let _ = self.execution.cancel_order(&leg.order_id).await;
            self.reconcile_after_cancel_or_missing(&mut leg, status_key).await;
        }
    }

    fn dry_run_instant_fill(
        &mut self,
        order_id: &str,
        side_name: &str,
        limit_price: Decimal,
        order_size: Decimal,
        status_key: &str,
    ) {
        let mut leg = ActiveMomentumLeg {
            order_id: order_id.to_string(),
            side_name: side_name.to_string(),
            limit_price,
            order_size,
            applied_matched: Decimal::ZERO,
            placed_at: Instant::now(),
        };
        self.apply_matched_from_clob(&mut leg, order_size, status_key);
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
    async fn check_preemptive_cancel(&mut self, status_key: &str) -> Result<()> {
        let Some(leg) = self.active_leg.as_ref() else {
            return Ok(());
        };
        if leg.placed_at.elapsed() < Duration::from_millis(self.config.mom_preemptive_cancel_ms) {
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
        let order_was_up = leg.side_name == "YES";
        if current_up != order_was_up {
            let order_id = leg.order_id.clone();
            if let Err(e) = self.execution.cancel_order(&order_id).await {
                error!(order_id = %order_id, error = %e, "Preemptive cancel failed");
            } else {
                info!(order_id = %order_id, side = %leg.side_name, "Preemptive cancel (Binance reversed)");
                if let Some(mut l) = self.active_leg.take() {
                    self.reconcile_after_cancel_or_missing(&mut l, status_key).await;
                }
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
        let tick = self.market.minimum_tick_size;

        while !shutdown.is_cancelled() {
            let now = Utc::now().timestamp();
            let time_remaining = self.market.round_end - now;
            let period_secs = self.market.period.as_seconds();
            let elapsed_pct = (now - self.market.round_start) as f64 / period_secs as f64;

            if time_remaining <= 0 {
                info!(market = %self.market.slug, "Round ended, momentum cleanup");
                self.cancel_round_cleanup(&status_key).await;
                self.strategy_status.insert(status_key.clone(), "Ended".to_string());
                break;
            }

            self.poll_active_leg(&status_key).await;

            self.check_preemptive_cancel(&status_key).await?;

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
                self.market.up_token_id.clone()
            } else {
                self.market.down_token_id.clone()
            };

            let ob = self.orderbook.get_orderbook(&token_id).context("No orderbook")?;
            let best_bid = ob.best_bid().context("No best bid")?;
            if time_remaining <= 30 && best_bid < self.config.mom_late_entry_min_bid {
                self.strategy_status.insert(status_key.clone(), "Waiting for price".to_string());
                sleep(Duration::from_millis(200)).await;
                continue;
            }

            let order_price = (best_bid + tick).min(dec!(0.99));

            let balance = *self.balance_receiver.borrow();
            if balance < dec!(5) {
                warn!(market = %self.market.slug, balance = %balance, "Momentum: USDC balance too low (< $5), skipping order");
                self.strategy_status.insert(status_key.clone(), "Low balance".to_string());
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            let sized = match strategy_sizing::compute_single_leg_sizing(
                balance,
                order_price,
                self.config.sniper_max_shares,
                self.config.sniper_capital_deploy_pct,
                self.config.sniper_min_shares,
            ) {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        market = %self.market.slug,
                        error = %e,
                        "Momentum: insufficient USDC or invalid sizing, skipping order"
                    );
                    self.strategy_status.insert(status_key.clone(), "Can't afford".to_string());
                    sleep(Duration::from_millis(500)).await;
                    continue;
                }
            };

            let order_size = sized.final_shares.round_dp(2);
            if order_size < self.config.sniper_min_shares {
                self.strategy_status.insert(status_key.clone(), "Too small".to_string());
                sleep(Duration::from_millis(200)).await;
                continue;
            }
            if order_size < self.market.minimum_order_size {
                warn!(
                    market = %self.market.slug,
                    order_size = %order_size,
                    min = %self.market.minimum_order_size,
                    "Momentum: order size below market minimum, skipping"
                );
                sleep(Duration::from_millis(200)).await;
                continue;
            }

            let cost = order_price * order_size;
            if cost > balance {
                warn!(
                    market = %self.market.slug,
                    cost = %cost,
                    balance = %balance,
                    "Momentum: insufficient USDC for order, skipping"
                );
                self.strategy_status.insert(status_key.clone(), "Low balance".to_string());
                sleep(Duration::from_millis(200)).await;
                continue;
            }

            if let Some(mut leg) = self.active_leg.take() {
                let _ = self.execution.cancel_order(&leg.order_id).await;
                self.reconcile_after_cancel_or_missing(&mut leg, &status_key).await;
            }

            let side_yes_no = if is_up { "YES" } else { "NO" };

            self.strategy_status.insert(status_key.clone(), format!("Entry signal: {}", side_str));
            match self
                .execution
                .place_order(
                    &token_id,
                    polymarket_client_sdk::clob::types::Side::Buy,
                    order_price,
                    order_size,
                    self.market.minimum_tick_size,
                )
                .await
            {
                Ok(oid) => {
                    if self.config.dry_run {
                        self.dry_run_instant_fill(&oid, side_yes_no, order_price, order_size, &status_key);
                    } else {
                        self.active_leg = Some(ActiveMomentumLeg {
                            order_id: oid.clone(),
                            side_name: side_yes_no.to_string(),
                            limit_price: order_price,
                            order_size,
                            applied_matched: Decimal::ZERO,
                            placed_at: Instant::now(),
                        });
                    }
                    self.strategy_status.insert(status_key.clone(), format!("Momentum: {} order", side_str));
                    info!(
                        market = %self.market.slug,
                        side = %side_str,
                        price = %order_price,
                        order_id = %oid,
                        size = %order_size,
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

        self.cancel_round_cleanup(&status_key).await;

        Ok(())
    }
}
