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
use std::sync::atomic::{AtomicBool, Ordering};
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
    strategy_status: Arc<DashMap<String, String>>,
    balance_receiver: watch::Receiver<Decimal>,
    sniper_lock: Arc<AtomicBool>,
    last_order_price: Option<Decimal>,
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
        sniper_lock: Arc<AtomicBool>,
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
            sniper_lock,
            last_order_price: None,
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

        let status_key = strategy_status_key(self.market.coin, self.market.period, self.market.round_start, "sniper");
        let mut order_id: Option<String> = None;

        loop {
            if shutdown.is_cancelled() {
                info!(market = %self.market.slug, "Sniper shutting down");
                if let Some(oid) = order_id {
                    let _ = self.execution.cancel_order(&oid).await;
                    // Release lock when order is cancelled
                    self.sniper_lock.store(false, Ordering::Relaxed);
                }
                break;
            }

            let now = Utc::now().timestamp();
            let time_remaining = self.market.round_end - now;

            // Only activate in final 60 seconds of round
            // For 5m rounds: 4:00-5:00 (240-300s), for 15m: 14:00-15:00 (840-900s)
            if time_remaining > 60 {
                self.strategy_status.insert(status_key.clone(), "Snp: Too early".to_string());
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            // Check if sniper is paused
            if self.risk.is_sniper_paused().await {
                self.strategy_status.insert(status_key.clone(), "Snp: Paused".to_string());
                warn!(market = %self.market.slug, "Sniper is paused");
                sleep(Duration::from_secs(10)).await;
                continue;
            }

            // Check balance - skip if < $5
            let balance = *self.balance_receiver.borrow();
            if balance < dec!(5) {
                self.strategy_status.insert(status_key.clone(), "Snp: Low balance".to_string());
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            // Check sniper lock - if another sniper is trading, skip
            if self.sniper_lock.load(Ordering::Relaxed) {
                self.strategy_status.insert(status_key.clone(), "Snp: Locked".to_string());
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            // Run entry gates
            match self.evaluate_entry_gates().await {
                Ok(Some((side_name, token_id, best_ask))) => {
                    let tick_size = self.market.minimum_tick_size;
                    
                    // Calculate price_decimals from tick_size (e.g., 0.01 → 2)
                    let price_decimals = tick_size.to_string().split('.').nth(1).map(|s| s.len()).unwrap_or(2) as u32;
                    
                    // Calculate maker price: best_ask - tick_size (one tick below to sit as maker)
                    // Round IMMEDIATELY after subtraction to avoid floating point artifacts
                    // Never place orders at $1.00 - if best_ask is $1.00 or higher, place at $0.99
                    // If best_ask - tick_size would be >= $1.00, also place at $0.99
                    let maker_price = if best_ask >= dec!(1.0) {
                        dec!(0.99)
                    } else {
                        let calculated = (best_ask - tick_size).round_dp(price_decimals);
                        // If calculated price is >= $1.00, use $0.99 instead
                        if calculated >= dec!(1.0) {
                            dec!(0.99)
                        } else {
                            calculated
                        }
                    };

                    // Double-check: never place orders at $1.00
                    if maker_price >= dec!(1.0) {
                        self.strategy_status.insert(status_key.clone(), "Snp: Price too high".to_string());
                        sleep(Duration::from_millis(500)).await;
                        continue;
                    }

                    // Only place order if maker_price >= 0.94
                    if maker_price < dec!(0.94) {
                        self.strategy_status.insert(status_key.clone(), "Snp: Price too low".to_string());
                        sleep(Duration::from_millis(500)).await;
                        continue;
                    }

                    // Check if we need to update the order (only if price changed by more than 1 tick)
                    if let Some(last_price) = self.last_order_price {
                        // If the maker price hasn't changed, do nothing
                        if maker_price == last_price {
                            sleep(Duration::from_millis(500)).await;
                            continue;
                        }
                    }

                    // Cancel any existing order before placing new one
                    if let Some(oid) = order_id.take() {
                        let _ = self.execution.cancel_order(&oid).await;
                        // Release lock when cancelling
                        self.sniper_lock.store(false, Ordering::Relaxed);
                    }

                    // Try to acquire lock
                    if self.sniper_lock.compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed).is_err() {
                        // Another sniper got the lock, skip
                        self.strategy_status.insert(status_key.clone(), "Snp: Locked".to_string());
                        sleep(Duration::from_millis(500)).await;
                        continue;
                    }

                    // Calculate position size: entire available balance
                    let order_size = self.calculate_position_size(maker_price).await?;
                    
                    // Minimum order size: 5 shares (Polymarket minimum)
                    if order_size < dec!(5) {
                        // Release lock if order too small
                        self.sniper_lock.store(false, Ordering::Relaxed);
                        self.strategy_status.insert(status_key.clone(), "Snp: Too small".to_string());
                        warn!(
                            market = %self.market.slug,
                            order_size = %order_size,
                            "Order size below minimum (5 shares)"
                        );
                        sleep(Duration::from_millis(500)).await;
                        continue;
                    }

                    // Place maker BUY order at maker_price (one tick below best_ask)
                    self.strategy_status.insert(status_key.clone(), "Snp: Entry!".to_string());

                    match self
                        .execution
                        .place_order(
                            &token_id,
                            polymarket_client_sdk::clob::types::Side::Buy,
                            maker_price,
                            order_size,
                            tick_size,
                        )
                        .await
                    {
                        Ok(oid) => {
                            order_id = Some(oid.clone());
                            self.last_order_price = Some(maker_price);
                            self.strategy_status.insert(status_key.clone(), "Snp: Filled".to_string());
                            info!(
                                market = %self.market.slug,
                                side = %side_name,
                                best_ask = %best_ask,
                                maker_price = %maker_price,
                                size = %order_size,
                                order_id = %oid,
                                "Sniper order placed at maker price (one tick below best_ask)"
                            );
                        }
                        Err(e) => {
                            // Release lock on error
                            self.sniper_lock.store(false, Ordering::Relaxed);
                            self.strategy_status.insert(status_key.clone(), "Snp: Failed".to_string());
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
                    self.strategy_status.insert(status_key.clone(), "Snp: Waiting".to_string());
                }
                Err(e) => {
                    self.strategy_status.insert(status_key.clone(), "Snp: Error".to_string());
                    error!(market = %self.market.slug, error = %e, "Error evaluating entry gates");
                }
            }

            // If round ended, break
            if time_remaining <= 0 {
                info!(
                    market = %self.market.slug,
                    "Round ended, sniper holding position"
                );
                // Release lock when round ends
                if order_id.is_some() {
                    self.sniper_lock.store(false, Ordering::Relaxed);
                }
                break;
            }

            sleep(Duration::from_millis(500)).await;
        }

        Ok(())
    }

    async fn evaluate_entry_gates(
        &self,
    ) -> Result<Option<(String, String, Decimal)>> {
        // Check both sides (YES and NO) to find one where best_ask >= $0.94
        let yes_orderbook = self
            .orderbook
            .get_orderbook(&self.market.up_token_id)
            .context("No YES orderbook data")?;
        
        let no_orderbook = self
            .orderbook
            .get_orderbook(&self.market.down_token_id)
            .context("No NO orderbook data")?;

        // Check YES side
        if let Some(best_ask_yes) = yes_orderbook.best_ask() {
            if best_ask_yes >= dec!(0.94) {
                return Ok(Some(("YES".to_string(), self.market.up_token_id.clone(), best_ask_yes)));
            }
        }

        // Check NO side
        if let Some(best_ask_no) = no_orderbook.best_ask() {
            if best_ask_no >= dec!(0.94) {
                return Ok(Some(("NO".to_string(), self.market.down_token_id.clone(), best_ask_no)));
            }
        }

        // Neither side meets the condition
        Ok(None)
    }

    async fn calculate_position_size(&self, price: Decimal) -> Result<Decimal> {
        // Get current balance
        let balance = *self.balance_receiver.borrow();
        
        // Maximum position size: entire available balance
        // Calculate how many shares we can buy with the full balance
        let shares = balance / price;
        
        // Round down to avoid exceeding balance, then round to 2 decimal places
        let shares_rounded = shares.floor().round_dp(2);
        
        Ok(shares_rounded)
    }
}
