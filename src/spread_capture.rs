//! Strategy 1: Spread Capture — gabagool DCA via **FAK** limit orders at the best ask.
//!
//! Uses the SDK `limit_order()` builder (not `market_order()`). For limit orders, BUY `size` is
//! **shares** (USDC notional is `size * price`). The market-order builder is different: BUY may use
//! USDC notional or shares via `Amount::usdc` / `Amount::shares`.
//! Small repeated fills; no resting-order polling.

use crate::config::Config;
use crate::execution::ExecutionEngine;
use crate::orderbook::OrderbookManager;
use crate::pnl::PnLManager;
use crate::types::{FilledTrade, Market, TuiEvent, strategy_status_key};
use anyhow::Result;
use chrono::{Local, Utc};
use dashmap::{DashMap, DashSet};
use polymarket_client_sdk::clob::types::{OrderType, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;

/// Polymarket rejects FAK BUY when `shares × price` is below this USDC notional.
const FAK_MIN_DOLLAR_AMOUNT: Decimal = Decimal::ONE;
use std::time::Instant;
use tokio::sync::{broadcast::error::RecvError, mpsc, watch};
use tokio::time::{Duration as TokioDuration, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

fn gab_fmt_d4(d: Decimal) -> String {
    d.round_dp(4).normalize().to_string()
}

pub struct SpreadCapture {
    market: Market,
    config: Arc<Config>,
    execution: Arc<ExecutionEngine>,
    orderbook: Arc<OrderbookManager>,
    strategy_status: Arc<DashMap<String, String>>,
    balance_receiver: watch::Receiver<Decimal>,
    pnl_manager: Arc<PnLManager>,
    trade_history: Arc<DashMap<String, FilledTrade>>,
    tui_tx: Option<mpsc::Sender<TuiEvent>>,
    yes_qty: Decimal,
    yes_cost: Decimal,
    no_qty: Decimal,
    no_cost: Decimal,
    active_markets: Arc<DashSet<String>>,
    last_yes_fak: Option<Instant>,
    last_no_fak: Option<Instant>,
}

impl SpreadCapture {
    pub fn new(
        market: Market,
        config: Arc<Config>,
        execution: Arc<ExecutionEngine>,
        orderbook: Arc<OrderbookManager>,
        strategy_status: Arc<DashMap<String, String>>,
        balance_receiver: watch::Receiver<Decimal>,
        pnl_manager: Arc<PnLManager>,
        trade_history: Arc<DashMap<String, FilledTrade>>,
        tui_tx: Option<mpsc::Sender<TuiEvent>>,
        active_markets: Arc<DashSet<String>>,
    ) -> Self {
        let status_key = strategy_status_key(market.coin, market.period, market.round_start, "spread_capture");
        strategy_status.insert(status_key.clone(), "Watching".to_string());
        Self {
            market,
            config: config.clone(),
            execution,
            orderbook,
            strategy_status,
            balance_receiver,
            pnl_manager,
            trade_history,
            tui_tx,
            yes_qty: dec!(0),
            yes_cost: dec!(0),
            no_qty: dec!(0),
            no_cost: dec!(0),
            active_markets,
            last_yes_fak: None,
            last_no_fak: None,
        }
    }

    fn active_market_key(&self) -> String {
        format!("{}:{}", self.market.slug, self.market.round_start)
    }

    fn unregister_active_market(&self) {
        self.active_markets.remove(&self.active_market_key());
    }

    fn avg_sum_pair_cost(&self) -> Option<Decimal> {
        if self.yes_qty > Decimal::ZERO && self.no_qty > Decimal::ZERO {
            Some(self.yes_cost / self.yes_qty + self.no_cost / self.no_qty)
        } else {
            None
        }
    }

    fn simulated_pair_after_yes_buy(&self, ask: Decimal, shares: Decimal) -> Option<Decimal> {
        if self.no_qty.is_zero() {
            return None;
        }
        let nq = self.yes_qty + shares;
        let nc = self.yes_cost + ask * shares;
        let nya = nc / nq;
        let noa = self.no_cost / self.no_qty;
        Some(nya + noa)
    }

    fn simulated_pair_after_no_buy(&self, ask: Decimal, shares: Decimal) -> Option<Decimal> {
        if self.yes_qty.is_zero() {
            return None;
        }
        let nq = self.no_qty + shares;
        let nc = self.no_cost + ask * shares;
        let nna = nc / nq;
        let yav = self.yes_cost / self.yes_qty;
        Some(yav + nna)
    }

    fn record_buy_fill(
        &mut self,
        is_yes: bool,
        shares: Decimal,
        usdc_spent: Decimal,
        status_key: &str,
        order_id: &str,
        limit_price: Decimal,
    ) {
        if shares <= Decimal::ZERO {
            return;
        }

        if is_yes {
            self.yes_qty += shares;
            self.yes_cost += usdc_spent;
        } else {
            self.no_qty += shares;
            self.no_cost += usdc_spent;
        }

        let side_name = if is_yes { "YES" } else { "NO" };
        self.pnl_manager.on_fill(
            self.market.slug.clone(),
            self.market.condition_id.clone(),
            side_name.to_string(),
            limit_price,
            shares,
        );

        let side_display = if is_yes { "UP" } else { "DOWN" };
        let fill_time = Local::now().format("%H:%M:%S").to_string();
        let log_msg = format!(
            "{} {} {} {} {}@{} FAK (+{} sh, ${} USDC)",
            fill_time,
            self.market.coin.as_str(),
            format!("{}m", self.market.period.as_minutes()),
            side_display,
            shares,
            limit_price,
            shares,
            usdc_spent
        );
        if let Some(ref tx) = self.tui_tx {
            let _ = tx.try_send(TuiEvent::TradeLog(log_msg));
        }

        self.trade_history.insert(
            order_id.to_string(),
            FilledTrade {
                slug: self.market.slug.clone(),
                side: side_name.to_string(),
                price: limit_price,
                size_matched: shares,
                condition_id: self.market.condition_id.clone(),
                timestamp: Utc::now(),
            },
        );

        let _ = self.active_markets.insert(self.active_market_key());

        let status_line = if let Some(pc) = self.avg_sum_pair_cost() {
            format!("pair_avg ${:.3}", pc)
        } else {
            format!(
                "Y {:.2} / N {:.2}",
                self.yes_qty.round_dp(2),
                self.no_qty.round_dp(2)
            )
        };
        self.strategy_status.insert(status_key.to_string(), status_line);
    }

    fn cooldown_ok(&self, is_yes: bool) -> bool {
        let cd = std::time::Duration::from_millis(self.config.sc_order_cooldown_ms);
        let last = if is_yes {
            self.last_yes_fak
        } else {
            self.last_no_fak
        };
        last.map(|t| t.elapsed() >= cd).unwrap_or(true)
    }

    fn mark_fak_sent(&mut self, is_yes: bool) {
        let now = Instant::now();
        if is_yes {
            self.last_yes_fak = Some(now);
        } else {
            self.last_no_fak = Some(now);
        }
    }

    async fn try_fak_buy(
        &mut self,
        is_yes: bool,
        status_key: &str,
        tick: Decimal,
    ) {
        if !self.cooldown_ok(is_yes) {
            return;
        }

        let ask = if is_yes {
            self.orderbook
                .get_orderbook(&self.market.up_token_id)
                .and_then(|o| o.best_ask())
        } else {
            self.orderbook
                .get_orderbook(&self.market.down_token_id)
                .and_then(|o| o.best_ask())
        };
        let Some(ask) = ask.filter(|a| !a.is_zero()) else {
            return;
        };

        if ask > self.config.sc_cheap_side_max_ask {
            return;
        }

        let tol = self.config.sc_max_qty_imbalance;
        if is_yes && self.yes_qty > self.no_qty + tol {
            return;
        }
        if !is_yes && self.no_qty > self.yes_qty + tol {
            return;
        }

        let min_shares_for_dollar_floor = (FAK_MIN_DOLLAR_AMOUNT / ask).ceil().round_dp(2);
        let shares = self
            .config
            .sc_fak_order_size
            .max(dec!(0.01))
            .max(min_shares_for_dollar_floor)
            .round_dp(2);
        let est_cost = (shares * ask).round_dp(2);

        let flat = self.yes_qty.is_zero() && self.no_qty.is_zero();
        if flat {
            let key = self.active_market_key();
            if !self.active_markets.contains(&key) {
                let n = self.active_markets.len();
                let max = self.config.sc_max_active_markets as usize;
                if n >= max {
                    self.strategy_status
                        .insert(status_key.to_string(), "Waiting (active cap)".to_string());
                    return;
                }
            }
        }

        let balance = *self.balance_receiver.borrow();
        if est_cost > balance {
            self.strategy_status
                .insert(status_key.to_string(), "Low balance".to_string());
            return;
        }

        if let Some(np) = if is_yes {
            self.simulated_pair_after_yes_buy(ask, shares)
        } else {
            self.simulated_pair_after_no_buy(ask, shares)
        } {
            if np > self.config.sc_max_pair_cost {
                return;
            }
        }

        let token_id = if is_yes {
            self.market.up_token_id.clone()
        } else {
            self.market.down_token_id.clone()
        };

        self.strategy_status.insert(
            status_key.to_string(),
            format!(
                "FAK {} {}sh @{}",
                if is_yes { "YES" } else { "NO" },
                shares,
                gab_fmt_d4(ask)
            ),
        );

        self.mark_fak_sent(is_yes);

        let resp = match self
            .execution
            .place_limit_order(
                token_id.as_str(),
                Side::Buy,
                ask,
                shares,
                tick,
                OrderType::FAK,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                error!(
                    detail = %crate::execution::format_order_error_for_logs(&e),
                    "Gab: FAK order details — token_id={}, side={:?}, price={}, size={}, order_type={:?}, error={:?}",
                    token_id.as_str(),
                    Side::Buy,
                    ask,
                    shares,
                    OrderType::FAK,
                    e,
                );
                self.strategy_status
                    .insert(status_key.to_string(), "Order failed".to_string());
                warn!(market = %self.market.slug, error = %e, "Gab: FAK order failed");
                return;
            }
        };

        if !resp.success {
            error!(
                market = %self.market.slug,
                token_id = %token_id,
                side = ?Side::Buy,
                price = %ask,
                size = %shares,
                order_type = ?OrderType::FAK,
                resp = ?resp,
                err_msg = ?resp.error_msg,
                "Gab: FAK post rejected (HTTP OK, success=false) — full PostOrderResponse above"
            );
            warn!(
                market = %self.market.slug,
                err = ?resp.error_msg,
                "Gab: FAK post rejected"
            );
            return;
        }

        // BUY: taking_amount = shares filled, making_amount = USDC spent (per CLOB response).
        let filled_shares = resp.taking_amount;
        let usdc = resp.making_amount;
        if filled_shares > Decimal::ZERO {
            self.record_buy_fill(
                is_yes,
                filled_shares,
                usdc,
                status_key,
                &resp.order_id,
                ask,
            );
            info!(
                "Gab {}: FAK BUY {} — filled {} sh, ${} USDC, order_id={}",
                self.market.slug,
                if is_yes { "YES" } else { "NO" },
                filled_shares,
                usdc,
                resp.order_id
            );
        }
    }

    pub async fn run(&mut self, shutdown: CancellationToken) -> Result<()> {
        info!(
            market = %self.market.slug,
            round_start = self.market.round_start,
            "Spread capture (gabagool FAK DCA) started"
        );
        info!(
            "Gab config: cheap_ask<={}, pair_max={}, imbalance<={}, fak_shares={}, cooldown_ms={}, max_active_markets={}",
            self.config.sc_cheap_side_max_ask,
            self.config.sc_max_pair_cost,
            self.config.sc_max_qty_imbalance,
            self.config.sc_fak_order_size,
            self.config.sc_order_cooldown_ms,
            self.config.sc_max_active_markets,
        );

        let status_key = strategy_status_key(
            self.market.coin,
            self.market.period,
            self.market.round_start,
            "spread_capture",
        );
        let tick = self.market.minimum_tick_size;

        let mut book_rx = self.orderbook.subscribe_book_updates();
        let mut fast_tick = tokio::time::interval(TokioDuration::from_millis(2));
        fast_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            if shutdown.is_cancelled() {
                info!(market = %self.market.slug, "Spread capture shutting down");
                self.unregister_active_market();
                return Ok(());
            }

            let now = Utc::now().timestamp();
            if now >= self.market.round_end {
                info!(market = %self.market.slug, "Round ended, spread capture exit");

                if self.yes_qty > Decimal::ZERO && self.no_qty > Decimal::ZERO {
                    let yes_avg = self.yes_cost / self.yes_qty;
                    let no_avg = self.no_cost / self.no_qty;
                    let pair_avg = yes_avg + no_avg;
                    let profit_per_share = Decimal::ONE - pair_avg;
                    let hedged_qty = self.yes_qty.min(self.no_qty);
                    let excess_yes = (self.yes_qty - self.no_qty).max(Decimal::ZERO);
                    let excess_no = (self.no_qty - self.yes_qty).max(Decimal::ZERO);
                    info!(
                        "Gab {}: round summary — yes_avg={} no_avg={} pair_avg_sum={} profit/share≈{} hedged_qty={} excess_yes={} excess_no={}",
                        self.market.slug,
                        gab_fmt_d4(yes_avg),
                        gab_fmt_d4(no_avg),
                        gab_fmt_d4(pair_avg),
                        gab_fmt_d4(profit_per_share),
                        hedged_qty,
                        excess_yes,
                        excess_no,
                    );
                } else if self.yes_qty > Decimal::ZERO || self.no_qty > Decimal::ZERO {
                    info!(
                        "Gab {}: round ended with incomplete pair — directional exposure (yes_qty={}, no_qty={})",
                        self.market.slug,
                        self.yes_qty,
                        self.no_qty,
                    );
                } else {
                    info!("Gab {}: round ended flat (no inventory)", self.market.slug);
                }

                self.strategy_status
                    .insert(status_key.clone(), "Ended".to_string());
                self.unregister_active_market();
                break;
            }

            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    self.unregister_active_market();
                    return Ok(());
                }
                r = book_rx.recv() => {
                    match r {
                        Ok(_token_id) => {}
                        Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => {
                            self.unregister_active_market();
                            return Ok(());
                        }
                    }
                }
                _ = fast_tick.tick() => {}
            }

            // Prefer the side with fewer shares first.
            let yes_first = self.yes_qty <= self.no_qty;
            if yes_first {
                self.try_fak_buy(true, &status_key, tick).await;
                self.try_fak_buy(false, &status_key, tick).await;
            } else {
                self.try_fak_buy(false, &status_key, tick).await;
                self.try_fak_buy(true, &status_key, tick).await;
            }
        }

        Ok(())
    }
}
