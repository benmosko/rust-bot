//! Strategy 1: Spread Capture / Dual-Side Arbitrage (gabagool strategy).
//! Buy YES when YES dips, NO when NO dips; keep pair cost < target; balance quantities.

use crate::config::Config;
use crate::execution::ExecutionEngine;
use crate::orderbook::OrderbookManager;
use crate::types::{Market, PairCost, strategy_status_key};
use anyhow::Result;
use chrono::Utc;
use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub struct SpreadCapture {
    market: Market,
    config: Arc<Config>,
    execution: Arc<ExecutionEngine>,
    orderbook: Arc<OrderbookManager>,
    strategy_status: Arc<DashMap<String, String>>,
    yes_qty: Decimal,
    yes_cost: Decimal,
    no_qty: Decimal,
    no_cost: Decimal,
    last_yes_bid: Decimal,
    last_no_bid: Decimal,
}

impl SpreadCapture {
    pub fn new(
        market: Market,
        config: Arc<Config>,
        execution: Arc<ExecutionEngine>,
        orderbook: Arc<OrderbookManager>,
        strategy_status: Arc<DashMap<String, String>>,
    ) -> Self {
        let status_key = strategy_status_key(market.coin, market.period, market.round_start, "spread_capture");
        strategy_status.insert(status_key.clone(), "Watching".to_string());
        Self {
            market,
            config: config.clone(),
            execution,
            orderbook,
            strategy_status,
            yes_qty: dec!(0),
            yes_cost: dec!(0),
            no_qty: dec!(0),
            no_cost: dec!(0),
            last_yes_bid: dec!(0),
            last_no_bid: dec!(0),
        }
    }

    fn current_pair_cost(&self) -> PairCost {
        PairCost::new(self.yes_qty, self.yes_cost, self.no_qty, self.no_cost)
    }

    /// Simulate pair cost if we add a buy: (yes_cost + no_cost + price * size) / min(new_yes_qty, new_no_qty).
    fn simulated_pair_cost_after_buy_yes(&self, price: Decimal, size: Decimal) -> Decimal {
        let new_yes_qty = self.yes_qty + size;
        let new_yes_cost = self.yes_cost + price * size;
        let min_qty = new_yes_qty.min(self.no_qty);
        if min_qty <= dec!(0) {
            return dec!(1.0);
        }
        (new_yes_cost + self.no_cost) / min_qty
    }

    fn simulated_pair_cost_after_buy_no(&self, price: Decimal, size: Decimal) -> Decimal {
        let new_no_qty = self.no_qty + size;
        let new_no_cost = self.no_cost + price * size;
        let min_qty = self.yes_qty.min(new_no_qty);
        if min_qty <= dec!(0) {
            return dec!(1.0);
        }
        (self.yes_cost + new_no_cost) / min_qty
    }

    fn imbalance_pct(&self) -> Decimal {
        let max_qty = self.yes_qty.max(self.no_qty);
        if max_qty <= dec!(0) {
            return Decimal::ZERO;
        }
        (self.yes_qty - self.no_qty).abs() / max_qty
    }

    pub async fn run(
        &mut self,
        shutdown: CancellationToken,
    ) -> Result<()> {
        info!(
            market = %self.market.slug,
            round_start = self.market.round_start,
            "Spread capture (gabagool) started"
        );

        let status_key = strategy_status_key(self.market.coin, self.market.period, self.market.round_start, "spread_capture");
        let order_size = self.market.minimum_order_size.round_dp(2); // Round size to 2 decimal places
        let tick = self.market.minimum_tick_size;

        while !shutdown.is_cancelled() {
            let now = Utc::now().timestamp();
            if now >= self.market.round_end {
                info!(market = %self.market.slug, "Round ended, spread capture exit");
                self.strategy_status.insert(status_key.clone(), "Ended".to_string());
                break;
            }

            let yes_ob = self.orderbook.get_orderbook(&self.market.up_token_id);
            let no_ob = self.orderbook.get_orderbook(&self.market.down_token_id);

            let (yes_bid, no_bid) = match (yes_ob.and_then(|o| o.best_bid()), no_ob.and_then(|o| o.best_bid())) {
                (Some(y), Some(n)) => (y, n),
                _ => {
                    self.strategy_status.insert(status_key.clone(), "Waiting for price".to_string());
                    sleep(Duration::from_millis(500)).await;
                    continue;
                }
            };

            // Guard against zero prices
            if yes_bid.is_zero() || no_bid.is_zero() {
                self.strategy_status.insert(status_key.clone(), "Waiting for price".to_string());
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            let _pair = self.current_pair_cost();
            let imbalance = self.imbalance_pct();

            // Only buy YES if: YES price dipped, adding YES keeps pair cost < max, and we're not over-weighted YES.
            let yes_dipped = self.last_yes_bid > dec!(0) && yes_bid <= self.last_yes_bid - self.config.sc_min_price_dip
                || self.last_yes_bid == dec!(0);
            let buy_yes_ok = imbalance <= self.config.sc_max_imbalance_pct
                || self.no_qty > self.yes_qty;
            let new_pair_yes = self.simulated_pair_cost_after_buy_yes(yes_bid + tick, order_size);
            if yes_dipped && buy_yes_ok && new_pair_yes < self.config.sc_max_pair_cost && new_pair_yes <= self.config.sc_target_pair_cost + dec!(0.01) {
                self.strategy_status.insert(status_key.clone(), "Entry signal: YES".to_string());
                match self
                    .execution
                    .place_order(
                        &self.market.up_token_id,
                        polymarket_client_sdk::clob::types::Side::Buy,
                        yes_bid + tick,
                        order_size,
                        tick,
                    )
                    .await
                {
                    Ok(_) => {
                        self.yes_qty += order_size;
                        self.yes_cost += (yes_bid + tick) * order_size;
                        let pair_cost = self.current_pair_cost().pair_cost;
                        self.strategy_status.insert(status_key.clone(), format!("Gabagool: pair ${:.3}", pair_cost));
                        debug!(
                            market = %self.market.slug,
                            yes_qty = %self.yes_qty,
                            pair_cost = %pair_cost,
                            "Bought YES"
                        );
                    }
                    Err(e) => {
                        self.strategy_status.insert(status_key.clone(), "Order failed".to_string());
                        warn!(market = %self.market.slug, error = %e, "Spread capture: YES order failed");
                    }
                }
            }

            // Only buy NO if: NO price dipped, adding NO keeps pair cost < max, and we're not over-weighted NO.
            let no_dipped = self.last_no_bid > dec!(0) && no_bid <= self.last_no_bid - self.config.sc_min_price_dip
                || self.last_no_bid == dec!(0);
            let buy_no_ok = imbalance <= self.config.sc_max_imbalance_pct
                || self.yes_qty > self.no_qty;
            let new_pair_no = self.simulated_pair_cost_after_buy_no(no_bid + tick, order_size);
            if no_dipped && buy_no_ok && new_pair_no < self.config.sc_max_pair_cost && new_pair_no <= self.config.sc_target_pair_cost + dec!(0.01) {
                self.strategy_status.insert(status_key.clone(), "Entry signal: NO".to_string());
                match self
                    .execution
                    .place_order(
                        &self.market.down_token_id,
                        polymarket_client_sdk::clob::types::Side::Buy,
                        no_bid + tick,
                        order_size,
                        tick,
                    )
                    .await
                {
                    Ok(_) => {
                        self.no_qty += order_size;
                        self.no_cost += (no_bid + tick) * order_size;
                        let pair_cost = self.current_pair_cost().pair_cost;
                        self.strategy_status.insert(status_key.clone(), format!("Gabagool: pair ${:.3}", pair_cost));
                        debug!(
                            market = %self.market.slug,
                            no_qty = %self.no_qty,
                            pair_cost = %pair_cost,
                            "Bought NO"
                        );
                    }
                    Err(e) => {
                        self.strategy_status.insert(status_key.clone(), "Order failed".to_string());
                        warn!(market = %self.market.slug, error = %e, "Spread capture: NO order failed");
                    }
                }
            }
            
            // Update status if watching
            if self.yes_qty.is_zero() && self.no_qty.is_zero() {
                self.strategy_status.insert(status_key.clone(), "Watching".to_string());
            }

            self.last_yes_bid = yes_bid;
            self.last_no_bid = no_bid;

            let pair_after = self.current_pair_cost();
            if pair_after.pair_cost > dec!(0) && (pair_after.yes_qty > dec!(0) || pair_after.no_qty > dec!(0)) {
                info!(
                    market = %self.market.slug,
                    pair_cost = %pair_after.pair_cost,
                    yes_qty = %pair_after.yes_qty,
                    no_qty = %pair_after.no_qty,
                    "Spread capture pair cost"
                );
            }

            sleep(Duration::from_millis(500)).await;
        }

        Ok(())
    }
}
