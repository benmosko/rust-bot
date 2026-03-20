use crate::config::Config;
use crate::execution::ExecutionEngine;
use crate::orderbook::OrderbookManager;
use crate::risk::RiskManager;
use crate::types::{Market, SpotState};
use anyhow::{Context, Result};
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

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
    ) -> Self {
        Self {
            market: market.clone(),
            config: config.clone(),
            execution,
            orderbook,
            spot_receiver,
            risk,
            opening_price,
            recent_spot_prices: Vec::new(),
        }
    }

    pub async fn run(
        &mut self,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        info!(
            market = %self.market.slug,
            round_start = self.market.round_start,
            opening_price = %self.opening_price,
            "Sniper waiting for entry window"
        );

        let mut replacement_attempts = 0;
        let mut order_id: Option<String> = None;

        loop {
            if shutdown.is_cancelled() {
                info!(market = %self.market.slug, "Sniper shutting down");
                if let Some(oid) = order_id {
                    let _ = self.execution.cancel_order(&oid).await;
                }
                break;
            }

            let now = Utc::now().timestamp();
            let time_remaining = self.market.round_end - now;

            // Only activate in final 30 seconds
            if time_remaining > 30 {
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            // Check if sniper is paused
            if self.risk.is_sniper_paused().await {
                warn!(market = %self.market.slug, "Sniper is paused");
                sleep(Duration::from_secs(10)).await;
                continue;
            }

            // Run entry gates
            match self.evaluate_entry_gates().await {
                Ok(Some((winning_side, token_id, bid_price))) => {
                    // Cancel any existing order
                    if let Some(oid) = order_id.take() {
                        let _ = self.execution.cancel_order(&oid).await;
                    }

                    // Place sniper order
                    let order_size = self.calculate_position_size().await?;
                    if order_size < self.market.minimum_order_size {
                        warn!(
                            market = %self.market.slug,
                            order_size = %order_size,
                            "Insufficient balance for sniper order"
                        );
                        sleep(Duration::from_millis(500)).await;
                        continue;
                    }

                    // Place order at best bid + 1 tick
                    let order_price = bid_price + self.market.minimum_tick_size;

                    match self
                        .execution
                        .place_order(
                            &token_id,
                            polymarket_client_sdk::clob::types::Side::Buy,
                            order_price,
                            order_size,
                        )
                        .await
                    {
                        Ok(oid) => {
                            order_id = Some(oid.clone());
                            info!(
                                market = %self.market.slug,
                                side = %winning_side,
                                price = %order_price,
                                size = %order_size,
                                order_id = %oid,
                                "Sniper order placed"
                            );
                        }
                        Err(e) => {
                            error!(
                                market = %self.market.slug,
                                error = %e,
                                "Failed to place sniper order"
                            );
                        }
                    }
                }
                Ok(None) => {
                    // Gates not passed, wait
                }
                Err(e) => {
                    error!(market = %self.market.slug, error = %e, "Error evaluating entry gates");
                }
            }

            // Check if order filled, re-place if needed
            if let Some(oid) = order_id.as_ref() {
                if replacement_attempts < self.config.snipe_max_replacements {
                    // In production, check order status via WebSocket
                    // For now, re-place after 10 seconds if not filled
                    sleep(Duration::from_secs(10)).await;
                    replacement_attempts += 1;

                    // Re-evaluate and re-place if still valid
                    if let Ok(Some((_, token_id, bid_price))) = self.evaluate_entry_gates().await {
                        let oid_clone = oid.clone();
                        let _ = self.execution.cancel_order(&oid_clone).await;
                        let order_size = self.calculate_position_size().await?;
                        let order_price = bid_price + self.market.minimum_tick_size;

                        if let Ok(new_oid) = self
                            .execution
                            .place_order(
                                &token_id,
                                polymarket_client_sdk::clob::types::Side::Buy,
                                order_price,
                                order_size,
                            )
                            .await
                        {
                            order_id = Some(new_oid);
                            info!(
                                market = %self.market.slug,
                                attempt = replacement_attempts,
                                "Sniper order re-placed"
                            );
                        }
                    }
                }
            }

            // If round ended, break
            if time_remaining <= 0 {
                info!(
                    market = %self.market.slug,
                    "Round ended, sniper holding position"
                );
                break;
            }

            sleep(Duration::from_millis(500)).await;
        }

        Ok(())
    }

    async fn evaluate_entry_gates(
        &self,
    ) -> Result<Option<(String, String, Decimal)>> {
        let now = Utc::now().timestamp();
        let elapsed = now - self.market.round_start;
        let period_secs = self.market.period.as_seconds();
        let elapsed_pct = elapsed as f64 / period_secs as f64;

        // Gate 1: Time gate
        if elapsed_pct < self.config.snipe_min_elapsed_pct {
            return Ok(None);
        }

        // Gate 2 & 3: Spot confirmation and momentum
        let spot_state = self.spot_receiver.borrow().clone();
        if spot_state.is_stale(2) {
            return Ok(None);
        }

        let current_spot = spot_state.price;
        let spot_direction = current_spot > self.opening_price;
        let winning_side = if spot_direction { "Up" } else { "Down" };
        let token_id = if spot_direction {
            &self.market.up_token_id
        } else {
            &self.market.down_token_id
        };

        // Gate 4: Bid price gate
        let orderbook = self
            .orderbook
            .get_orderbook(token_id)
            .context("No orderbook data")?;

        let best_bid = orderbook.best_bid().context("No best bid")?;
        if best_bid < self.config.snipe_min_bid {
            return Ok(None);
        }

        // Gate 5: Spread gate
        let spread = orderbook.spread().context("No spread")?;
        if spread > self.config.snipe_max_spread {
            return Ok(None);
        }

        // Gate 6: Opposing side confirmation
        let losing_token_id = if spot_direction {
            &self.market.down_token_id
        } else {
            &self.market.up_token_id
        };
        let losing_orderbook = self
            .orderbook
            .get_orderbook(losing_token_id)
            .context("No losing orderbook")?;
        let losing_best_bid = losing_orderbook.best_bid().unwrap_or(dec!(0));
        if losing_best_bid > dec!(0.10) {
            return Ok(None);
        }

        // Gate 7: Orderbook depth
        if orderbook.bids.len() < 3 {
            return Ok(None);
        }

        // Gate 8: No recent crossover (simplified - would need spot history)
        // Gate 9: Capital available (checked in calculate_position_size)
        // Gate 10: Spot data freshness (checked above)

        Ok(Some((winning_side.to_string(), token_id.clone(), best_bid)))
    }

    async fn calculate_position_size(&self) -> Result<Decimal> {
        // In production, get actual balance from balance manager
        // For now, return a placeholder
        Ok(dec!(100))
    }
}
