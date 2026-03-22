use crate::config::Config;
use crate::execution::ExecutionEngine;
use crate::orderbook::OrderbookManager;
use crate::pnl::PnLManager;
use crate::strategy_sizing;
use crate::types::{
    FilledTrade, InventoryState, Market, SpotState, TuiEvent, strategy_status_key,
};
use anyhow::Result;
use chrono::{Local, Utc};
use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tokio::time::{sleep, Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

struct ActiveMmLeg {
    order_id: String,
    /// "YES" or "NO"
    side_name: String,
    limit_price: Decimal,
    order_size: Decimal,
    applied_matched: Decimal,
}

pub struct MarketMaker {
    market: Market,
    config: Arc<Config>,
    execution: Arc<ExecutionEngine>,
    orderbook: Arc<OrderbookManager>,
    spot_receiver: watch::Receiver<SpotState>,
    strategy_status: Arc<DashMap<String, String>>,
    balance_receiver: watch::Receiver<Decimal>,
    pnl_manager: Arc<PnLManager>,
    trade_history: Arc<DashMap<String, FilledTrade>>,
    tui_tx: Option<mpsc::Sender<TuiEvent>>,
    /// Filled YES / NO share counts for skew and per-side caps.
    inventory: InventoryState,
    /// Recent spot prices for rolling volatility (widen spread when range exceeds threshold).
    spot_samples: Vec<Decimal>,
}

impl MarketMaker {
    pub fn new(
        market: Market,
        config: Arc<Config>,
        execution: Arc<ExecutionEngine>,
        orderbook: Arc<OrderbookManager>,
        spot_receiver: watch::Receiver<SpotState>,
        strategy_status: Arc<DashMap<String, String>>,
        balance_receiver: watch::Receiver<Decimal>,
        pnl_manager: Arc<PnLManager>,
        trade_history: Arc<DashMap<String, FilledTrade>>,
        tui_tx: Option<mpsc::Sender<TuiEvent>>,
    ) -> Self {
        let status_key = strategy_status_key(market.coin, market.period, market.round_start, "market_maker");
        strategy_status.insert(status_key.clone(), "Watching".to_string());
        let round_start = market.round_start;
        Self {
            market,
            config,
            execution,
            orderbook,
            spot_receiver,
            strategy_status,
            balance_receiver,
            pnl_manager,
            trade_history,
            tui_tx,
            inventory: InventoryState {
                yes_shares: dec!(0),
                no_shares: dec!(0),
                round_start,
            },
            spot_samples: Vec::new(),
        }
    }

    fn order_status_indicates_fill(_status: &str, size_matched: Decimal) -> bool {
        size_matched > Decimal::ZERO
    }

    fn order_fully_filled(_status: &str, size_matched: Decimal, order_size: Decimal) -> bool {
        size_matched > Decimal::ZERO && size_matched >= order_size
    }

    fn push_spot_sample(&mut self, price: Decimal) {
        if price.is_zero() {
            return;
        }
        let cap = self.config.mm_spot_volatility_window_ticks.max(2);
        self.spot_samples.push(price);
        while self.spot_samples.len() > cap {
            self.spot_samples.remove(0);
        }
    }

    fn is_high_volatility(&self) -> bool {
        if self.spot_samples.len() < 2 {
            return false;
        }
        let min_p = self
            .spot_samples
            .iter()
            .copied()
            .min()
            .unwrap_or(Decimal::ZERO);
        let max_p = self
            .spot_samples
            .iter()
            .copied()
            .max()
            .unwrap_or(Decimal::ZERO);
        if min_p.is_zero() {
            return false;
        }
        (max_p - min_p) / min_p >= self.config.mm_spot_move_pct_threshold
    }

    fn apply_matched_from_clob(
        &mut self,
        leg: &mut ActiveMmLeg,
        is_yes: bool,
        size_matched: Decimal,
        status_key: &str,
    ) {
        let delta = size_matched - leg.applied_matched;
        if delta <= Decimal::ZERO {
            return;
        }

        leg.applied_matched = size_matched;

        if is_yes {
            self.inventory.yes_shares += delta;
        } else {
            self.inventory.no_shares += delta;
        }

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
            "{} {} {} MM {} {}@{} filled (+{})",
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
            format!(
                "MM: inv Y{} N{}",
                self.inventory.yes_shares, self.inventory.no_shares
            ),
        );
        debug!(
            market = %self.market.slug,
            side = %leg.side_name,
            yes = %self.inventory.yes_shares,
            no = %self.inventory.no_shares,
            "Market maker fill applied"
        );
    }

    async fn reconcile_after_cancel_or_missing(
        &mut self,
        leg: &mut ActiveMmLeg,
        is_yes: bool,
        status_key: &str,
    ) {
        const MAX_ATTEMPTS: u32 = 45;
        const DELAY_MS: u64 = 500;

        for attempt in 0..MAX_ATTEMPTS {
            match self.execution.get_order_status(&leg.order_id).await {
                Ok(Some((status, size_matched))) => {
                    let st = status.to_lowercase();
                    if Self::order_status_indicates_fill(&status, size_matched) {
                        self.apply_matched_from_clob(leg, is_yes, size_matched, status_key);
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
                        "MM reconcile: order not in API yet, retrying"
                    );
                }
                Err(e) => {
                    warn!(
                        market = %self.market.slug,
                        order_id = %leg.order_id,
                        error = %e,
                        attempt,
                        "MM reconcile: get_order_status error, retrying"
                    );
                }
            }
            sleep(Duration::from_millis(DELAY_MS)).await;
        }
    }

    async fn poll_active_leg(
        &mut self,
        leg_slot: &mut Option<ActiveMmLeg>,
        is_yes: bool,
        status_key: &str,
    ) {
        let Some(mut leg) = leg_slot.take() else {
            return;
        };

        match self.execution.get_order_status(&leg.order_id).await {
            Ok(Some((status, size_matched))) => {
                let st = status.to_lowercase();
                if size_matched > leg.applied_matched {
                    self.apply_matched_from_clob(&mut leg, is_yes, size_matched, status_key);
                }
                if st == "cancelled" {
                    return;
                }
                if Self::order_fully_filled(&status, size_matched, leg.order_size) {
                    return;
                }
                *leg_slot = Some(leg);
            }
            Ok(None) => {
                self.reconcile_after_cancel_or_missing(&mut leg, is_yes, status_key).await;
            }
            Err(e) => {
                warn!(
                    market = %self.market.slug,
                    order_id = %leg.order_id,
                    error = %e,
                    "MM: get_order_status failed, will retry next tick"
                );
                *leg_slot = Some(leg);
            }
        }
    }

    async fn cancel_round_cleanup(
        &mut self,
        active_yes: &mut Option<ActiveMmLeg>,
        active_no: &mut Option<ActiveMmLeg>,
        status_key: &str,
    ) {
        if let Some(mut leg) = active_yes.take() {
            let _ = self.execution.cancel_order(&leg.order_id).await;
            self.reconcile_after_cancel_or_missing(&mut leg, true, status_key).await;
        }
        if let Some(mut leg) = active_no.take() {
            let _ = self.execution.cancel_order(&leg.order_id).await;
            self.reconcile_after_cancel_or_missing(&mut leg, false, status_key).await;
        }
    }

    fn dry_run_instant_fill(
        &mut self,
        order_id: &str,
        side_name: &str,
        is_yes: bool,
        limit_price: Decimal,
        order_size: Decimal,
        status_key: &str,
    ) {
        let mut leg = ActiveMmLeg {
            order_id: order_id.to_string(),
            side_name: side_name.to_string(),
            limit_price,
            order_size,
            applied_matched: Decimal::ZERO,
        };
        self.apply_matched_from_clob(&mut leg, is_yes, order_size, status_key);
    }

    pub async fn run(&mut self, shutdown: CancellationToken) -> Result<()> {
        info!(
            market = %self.market.slug,
            round_start = self.market.round_start,
            "Starting market maker"
        );

        let status_key = strategy_status_key(
            self.market.coin,
            self.market.period,
            self.market.round_start,
            "market_maker",
        );
        let mut last_spot_price = dec!(0);
        let mut last_cancel_replace_time = Instant::now();
        let mut volatility_pause_until: Option<Instant> = None;
        let mut active_yes: Option<ActiveMmLeg> = None;
        let mut active_no: Option<ActiveMmLeg> = None;

        loop {
            if shutdown.is_cancelled() {
                info!(market = %self.market.slug, "Market maker shutting down");
                self.cancel_round_cleanup(&mut active_yes, &mut active_no, &status_key)
                    .await;
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
                self.cancel_round_cleanup(&mut active_yes, &mut active_no, &status_key)
                    .await;
                break;
            }

            // Check volatility pause
            if let Some(pause_until) = volatility_pause_until {
                if Instant::now() < pause_until {
                    self.strategy_status
                        .insert(status_key.clone(), "Volatility pause".to_string());
                    sleep(Duration::from_millis(100)).await;
                    continue;
                } else {
                    volatility_pause_until = None;
                }
            }

            // Get current spot price
            let spot_state = self.spot_receiver.borrow().clone();
            if spot_state.is_stale(3) {
                self.strategy_status
                    .insert(status_key.clone(), "Stale spot".to_string());
                warn!(market = %self.market.slug, "Spot data is stale, skipping update");
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            let spot_price = spot_state.price;

            // Wait for valid spot price before proceeding
            if spot_price.is_zero() {
                self.strategy_status
                    .insert(status_key.clone(), "Waiting for price".to_string());
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            self.poll_active_leg(&mut active_yes, true, &status_key).await;
            self.poll_active_leg(&mut active_no, false, &status_key).await;

            // Check for volatility spike (single-tick circuit breaker)
            if !last_spot_price.is_zero() {
                let price_change_pct = (spot_price - last_spot_price).abs() / last_spot_price;
                if price_change_pct > dec!(0.01) {
                    warn!(
                        market = %self.market.slug,
                        price_change_pct = %price_change_pct,
                        "Volatility spike detected, pausing"
                    );
                    self.cancel_round_cleanup(&mut active_yes, &mut active_no, &status_key)
                        .await;
                    volatility_pause_until = Some(Instant::now() + Duration::from_secs(10));
                    last_spot_price = spot_price;
                    continue;
                }
            }

            self.push_spot_sample(spot_price);

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
                self.strategy_status
                    .insert(status_key.clone(), "Waiting for orderbook".to_string());
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            let up_mid = up_midpoint.unwrap();
            let down_mid = down_midpoint.unwrap();

            // Calculate spread based on rolling-spot volatility window
            let half_spread = if self.is_high_volatility() {
                self.config.mm_volatility_spread
            } else {
                self.config.mm_half_spread
            };

            // Adjust spread if inventory is imbalanced
            let imbalance = self.inventory.imbalance();
            let max_imbalance = self.config.mm_inventory_imbalance_limit;

            let (yes_spread, no_spread) = if imbalance > max_imbalance {
                if self.inventory.yes_shares > self.inventory.no_shares {
                    (half_spread * dec!(2), half_spread / dec!(2))
                } else {
                    (half_spread / dec!(2), half_spread * dec!(2))
                }
            } else {
                (half_spread, half_spread)
            };

            // Calculate order prices
            let yes_buy_price = (up_mid - yes_spread).round_dp(2);
            let no_buy_price = (down_mid - no_spread).round_dp(2);

            let quote_yes = self.inventory.yes_shares < self.config.mm_max_inventory_per_side;
            let quote_no = self.inventory.no_shares < self.config.mm_max_inventory_per_side;

            // Check if we need to cancel and replace
            let should_update = last_spot_price != spot_price
                || last_cancel_replace_time.elapsed() > Duration::from_millis(5000);

            if should_update {
                let cancel_replace_start = Instant::now();
                self.strategy_status
                    .insert(status_key.clone(), "Updating quotes".to_string());

                self.cancel_round_cleanup(&mut active_yes, &mut active_no, &status_key)
                    .await;

                let balance = *self.balance_receiver.borrow();
                if balance < dec!(5) {
                    warn!(
                        market = %self.market.slug,
                        balance = %balance,
                        "MM: USDC balance too low (< $5), skipping quotes"
                    );
                    self.strategy_status
                        .insert(status_key.clone(), "Low balance".to_string());
                    last_spot_price = spot_price;
                    last_cancel_replace_time = Instant::now();
                    sleep(Duration::from_millis(100)).await;
                    continue;
                }

                if !quote_yes && !quote_no {
                    self.strategy_status.insert(
                        status_key.clone(),
                        format!(
                            "MM: max inventory (Y≤{} N≤{})",
                            self.config.mm_max_inventory_per_side,
                            self.config.mm_max_inventory_per_side
                        ),
                    );
                    last_spot_price = spot_price;
                    last_cancel_replace_time = Instant::now();
                    sleep(Duration::from_millis(100)).await;
                    continue;
                }

                // Per-side size: dual when quoting both; single when capped on one side.
                let (yes_size, no_size) = if quote_yes && quote_no {
                    match strategy_sizing::compute_per_leg_dual(
                        balance,
                        yes_buy_price,
                        no_buy_price,
                        self.config.sniper_capital_deploy_pct,
                        self.config.sniper_min_shares,
                    ) {
                        Ok(d) => {
                            let s = d.final_per_leg.round_dp(2);
                            (s, s)
                        }
                        Err(e) => {
                            warn!(
                                market = %self.market.slug,
                                error = %e,
                                "MM: dual sizing failed, skipping quotes"
                            );
                            (Decimal::ZERO, Decimal::ZERO)
                        }
                    }
                } else if quote_yes {
                    match strategy_sizing::compute_single_leg_sizing(
                        balance,
                        yes_buy_price,
                        self.config.sniper_capital_deploy_pct,
                        self.config.sniper_min_shares,
                    ) {
                        Ok(s) => {
                            let sz = s.final_shares.round_dp(2);
                            (sz, Decimal::ZERO)
                        }
                        Err(e) => {
                            warn!(
                                market = %self.market.slug,
                                error = %e,
                                "MM: YES sizing failed, skipping"
                            );
                            (Decimal::ZERO, Decimal::ZERO)
                        }
                    }
                } else {
                    match strategy_sizing::compute_single_leg_sizing(
                        balance,
                        no_buy_price,
                        self.config.sniper_capital_deploy_pct,
                        self.config.sniper_min_shares,
                    ) {
                        Ok(s) => {
                            let sz = s.final_shares.round_dp(2);
                            (Decimal::ZERO, sz)
                        }
                        Err(e) => {
                            warn!(
                                market = %self.market.slug,
                                error = %e,
                                "MM: NO sizing failed, skipping"
                            );
                            (Decimal::ZERO, Decimal::ZERO)
                        }
                    }
                };

                let mut placed_any = false;

                if quote_yes
                    && yes_size >= self.market.minimum_order_size
                    && yes_size >= self.config.sniper_min_shares
                    && yes_buy_price * yes_size <= balance
                {
                    match self
                        .execution
                        .place_order(
                            &self.market.up_token_id,
                            polymarket_client_sdk::clob::types::Side::Buy,
                            yes_buy_price,
                            yes_size,
                            self.market.minimum_tick_size,
                        )
                        .await
                    {
                        Ok(order_id) => {
                            placed_any = true;
                            if self.config.dry_run {
                                self.dry_run_instant_fill(
                                    &order_id,
                                    "YES",
                                    true,
                                    yes_buy_price,
                                    yes_size,
                                    &status_key,
                                );
                            } else {
                                active_yes = Some(ActiveMmLeg {
                                    order_id,
                                    side_name: "YES".to_string(),
                                    limit_price: yes_buy_price,
                                    order_size: yes_size,
                                    applied_matched: Decimal::ZERO,
                                });
                            }
                        }
                        Err(e) => {
                            error!(market = %self.market.slug, error = %e, "Failed to place YES order");
                        }
                    }
                } else if quote_yes {
                    warn!(
                        market = %self.market.slug,
                        yes_size = %yes_size,
                        "MM: YES quote skipped (size or balance)"
                    );
                }

                let balance_after_yes = *self.balance_receiver.borrow();

                if quote_no
                    && no_size >= self.market.minimum_order_size
                    && no_size >= self.config.sniper_min_shares
                    && no_buy_price * no_size <= balance_after_yes
                {
                    match self
                        .execution
                        .place_order(
                            &self.market.down_token_id,
                            polymarket_client_sdk::clob::types::Side::Buy,
                            no_buy_price,
                            no_size,
                            self.market.minimum_tick_size,
                        )
                        .await
                    {
                        Ok(order_id) => {
                            placed_any = true;
                            if self.config.dry_run {
                                self.dry_run_instant_fill(
                                    &order_id,
                                    "NO",
                                    false,
                                    no_buy_price,
                                    no_size,
                                    &status_key,
                                );
                            } else {
                                active_no = Some(ActiveMmLeg {
                                    order_id,
                                    side_name: "NO".to_string(),
                                    limit_price: no_buy_price,
                                    order_size: no_size,
                                    applied_matched: Decimal::ZERO,
                                });
                            }
                        }
                        Err(e) => {
                            error!(market = %self.market.slug, error = %e, "Failed to place NO order");
                        }
                    }
                } else if quote_no {
                    warn!(
                        market = %self.market.slug,
                        no_size = %no_size,
                        "MM: NO quote skipped (size or balance)"
                    );
                }

                if placed_any {
                    self.strategy_status
                        .insert(status_key.clone(), "MM: active".to_string());
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

        self.cancel_round_cleanup(&mut active_yes, &mut active_no, &status_key)
            .await;

        Ok(())
    }
}
