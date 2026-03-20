use crate::config::Config;
use crate::execution::ExecutionEngine;
use crate::orderbook::OrderbookManager;
use crate::types::{InventoryState, Market, OrderState, strategy_status_key};
use anyhow::Result;
use chrono::Utc;
use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::watch;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

pub struct MarketMaker {
    market: Market,
    config: Arc<Config>,
    execution: Arc<ExecutionEngine>,
    orderbook: Arc<OrderbookManager>,
    spot_receiver: watch::Receiver<crate::types::SpotState>,
    inventory: Arc<tokio::sync::RwLock<InventoryState>>,
    open_orders: Arc<tokio::sync::RwLock<Vec<OrderState>>>,
    strategy_status: Arc<DashMap<String, String>>,
}

impl MarketMaker {
    pub fn new(
        market: Market,
        config: Arc<Config>,
        execution: Arc<ExecutionEngine>,
        orderbook: Arc<OrderbookManager>,
        spot_receiver: watch::Receiver<crate::types::SpotState>,
        strategy_status: Arc<DashMap<String, String>>,
    ) -> Self {
        let status_key = strategy_status_key(market.coin, market.period, market.round_start, "market_maker");
        strategy_status.insert(status_key.clone(), "Watching".to_string());
        Self {
            market: market.clone(),
            config: config.clone(),
            execution,
            orderbook,
            spot_receiver,
            inventory: Arc::new(tokio::sync::RwLock::new(InventoryState {
                yes_shares: dec!(0),
                no_shares: dec!(0),
                round_start: market.round_start,
            })),
            open_orders: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            strategy_status,
        }
    }

    pub async fn run(
        &mut self,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        info!(
            market = %self.market.slug,
            round_start = self.market.round_start,
            "Starting market maker"
        );

        let status_key = strategy_status_key(self.market.coin, self.market.period, self.market.round_start, "market_maker");
        let mut last_spot_price = dec!(0);
        let mut last_cancel_replace_time = Instant::now();
        let mut volatility_pause_until: Option<Instant> = None;

        loop {
            if shutdown.is_cancelled() {
                info!(market = %self.market.slug, "Market maker shutting down");
                self.cancel_all_orders().await?;
                break;
            }

            let now = Utc::now().timestamp();
            let time_remaining = self.market.round_end - now;

            // Stop market making before round ends
            if time_remaining <= self.config.mm_stop_before_end_secs as i64 {
                info!(
                    market = %self.market.slug,
                    time_remaining = time_remaining,
                    "Stopping market making, handing off to sniper"
                );
                self.strategy_status.insert(status_key.clone(), "Stopped".to_string());
                self.cancel_all_orders().await?;
                break;
            }

            // Check volatility pause
            if let Some(pause_until) = volatility_pause_until {
                if Instant::now() < pause_until {
                    self.strategy_status.insert(status_key.clone(), "Volatility pause".to_string());
                    sleep(Duration::from_millis(100)).await;
                    continue;
                } else {
                    volatility_pause_until = None;
                }
            }

            // Get current spot price
            let spot_state = self.spot_receiver.borrow().clone();
            if spot_state.is_stale(3) {
                self.strategy_status.insert(status_key.clone(), "Stale spot".to_string());
                warn!(market = %self.market.slug, "Spot data is stale, skipping update");
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            let spot_price = spot_state.price;

            // Wait for valid spot price before proceeding
            if spot_price.is_zero() {
                self.strategy_status.insert(status_key.clone(), "Waiting for price".to_string());
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            // Check for volatility spike
            if !last_spot_price.is_zero() {
                // Guard against division by zero
                let price_change_pct = (spot_price - last_spot_price).abs() / last_spot_price;
                if price_change_pct > dec!(0.01) {
                    // 1% move in 30 seconds
                    warn!(
                        market = %self.market.slug,
                        price_change_pct = %price_change_pct,
                        "Volatility spike detected, pausing"
                    );
                    self.cancel_all_orders().await?;
                    volatility_pause_until = Some(Instant::now() + Duration::from_secs(10));
                    last_spot_price = spot_price;
                    continue;
                }
            }

            // Get orderbook midpoints
            let up_midpoint = self
                .orderbook
                .get_orderbook(&self.market.up_token_id)
                .and_then(|ob| ob.midpoint());

            let down_midpoint = self
                .orderbook
                .get_orderbook(&self.market.down_token_id)
                .and_then(|ob| ob.midpoint());

            if up_midpoint.is_none() || down_midpoint.is_none() {
                self.strategy_status.insert(status_key.clone(), "Waiting for orderbook".to_string());
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            let up_mid = up_midpoint.unwrap();
            let down_mid = down_midpoint.unwrap();

            // Calculate spread based on volatility
            let half_spread = if self.is_high_volatility(&spot_state) {
                self.config.mm_volatility_spread
            } else {
                self.config.mm_half_spread
            };

            // Adjust spread if inventory is imbalanced
            let inventory = self.inventory.read().await;
            let imbalance = inventory.imbalance();
            let max_imbalance = self.config.mm_inventory_imbalance_limit;

            let (yes_spread, no_spread) = if imbalance > max_imbalance {
                // Skew quotes to rebalance
                if inventory.yes_shares > inventory.no_shares {
                    (half_spread * dec!(2), half_spread / dec!(2))
                } else {
                    (half_spread / dec!(2), half_spread * dec!(2))
                }
            } else {
                (half_spread, half_spread)
            };
            drop(inventory);

            // Calculate order prices
            let yes_buy_price = (up_mid - yes_spread).round_dp(2);
            let no_buy_price = (down_mid - no_spread).round_dp(2);

            // Check if we need to cancel and replace
            let should_update = last_spot_price != spot_price
                || last_cancel_replace_time.elapsed() > Duration::from_millis(5000);

            if should_update {
                let cancel_replace_start = Instant::now();
                self.strategy_status.insert(status_key.clone(), "Updating quotes".to_string());

                // Cancel existing orders
                self.cancel_all_orders().await?;

                // Place new orders
                let order_size = self.calculate_order_size().await?;

                if order_size >= self.market.minimum_order_size {
                    self.strategy_status.insert(status_key.clone(), "MM: active".to_string());
                    // Place YES BUY order
                    match self
                        .execution
                        .place_order(
                            &self.market.up_token_id,
                            polymarket_client_sdk::clob::types::Side::Buy,
                            yes_buy_price,
                            order_size,
                            self.market.minimum_tick_size,
                        )
                        .await
                    {
                        Ok(order_id) => {
                            self.open_orders.write().await.push(OrderState {
                                order_id,
                                token_id: self.market.up_token_id.clone(),
                                side: "BUY".to_string(),
                                price: yes_buy_price,
                                size: order_size,
                                filled: dec!(0),
                                status: "OPEN".to_string(),
                                placed_at: Utc::now(),
                            });
                        }
                        Err(e) => {
                            error!(market = %self.market.slug, error = %e, "Failed to place YES order");
                        }
                    }

                    // Place NO BUY order
                    match self
                        .execution
                        .place_order(
                            &self.market.down_token_id,
                            polymarket_client_sdk::clob::types::Side::Buy,
                            no_buy_price,
                            order_size,
                            self.market.minimum_tick_size,
                        )
                        .await
                    {
                        Ok(order_id) => {
                            self.open_orders.write().await.push(OrderState {
                                order_id,
                                token_id: self.market.down_token_id.clone(),
                                side: "BUY".to_string(),
                                price: no_buy_price,
                                size: order_size,
                                filled: dec!(0),
                                status: "OPEN".to_string(),
                                placed_at: Utc::now(),
                            });
                        }
                        Err(e) => {
                            error!(market = %self.market.slug, error = %e, "Failed to place NO order");
                        }
                    }
                }

                let elapsed_ms = cancel_replace_start.elapsed().as_millis() as u64;
                if elapsed_ms > self.config.cancel_replace_hard_limit_ms {
                    warn!(
                        market = %self.market.slug,
                        elapsed_ms = elapsed_ms,
                        "Cancel/replace loop exceeded hard limit"
                    );
                } else if elapsed_ms > self.config.cancel_replace_target_ms {
                    warn!(
                        market = %self.market.slug,
                        elapsed_ms = elapsed_ms,
                        "Cancel/replace loop exceeded target"
                    );
                } else {
                    info!(
                        market = %self.market.slug,
                        elapsed_ms = elapsed_ms,
                        "Cancel/replace completed"
                    );
                }

                last_cancel_replace_time = Instant::now();
            }

            last_spot_price = spot_price;
            sleep(Duration::from_millis(100)).await;
        }

        Ok(())
    }

    async fn cancel_all_orders(&self) -> Result<()> {
        let orders = self.open_orders.read().await.clone();
        for order in orders {
            if order.status == "OPEN" {
                if let Err(e) = self.execution.cancel_order(&order.order_id).await {
                    error!(order_id = %order.order_id, error = %e, "Failed to cancel order");
                }
            }
        }
        self.open_orders.write().await.clear();
        Ok(())
    }

    async fn calculate_order_size(&self) -> Result<Decimal> {
        // Use a fixed size for now, could be made configurable
        // Round to 2 decimal places (size always uses 2dp)
        Ok(dec!(100).round_dp(2))
    }

    fn is_high_volatility(&self, _spot_state: &crate::types::SpotState) -> bool {
        // Simple volatility check - in production, use rolling standard deviation
        // For now, assume low volatility
        false
    }
}
