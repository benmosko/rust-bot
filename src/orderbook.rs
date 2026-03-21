use crate::types::OrderbookState;
use anyhow::Result;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use futures_util::StreamExt;
use polymarket_client_sdk::clob::ws::{BookUpdate, Client as WsClient};
use polymarket_client_sdk::types::U256;
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use log::{error, info};
use tracing::debug;

const STALENESS_SECS: u64 = 15;

fn unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Remove float/string parse artifacts on CLOB WebSocket prices (e.g. 0.8499999999999999 → 0.85).
fn clean_ws_price(price: Decimal) -> Decimal {
    rust_decimal::Decimal::from_str(&format!("{:.4}", price)).unwrap_or(price)
}

#[derive(Debug, Clone)]
pub enum OrderbookCommand {
    Subscribe(String),
    #[allow(dead_code)]
    Unsubscribe(String),
}

/// Event emitted when best_ask crosses the sniper entry threshold (see `Config::sniper_entry_min_best_ask`)
#[derive(Debug, Clone)]
pub struct PriceThresholdEvent {
    pub token_id: String,
    pub best_ask: Decimal,
    pub timestamp: std::time::Instant,
}

pub struct OrderbookManager {
    orderbooks: Arc<DashMap<String, OrderbookState>>,
    command_tx: broadcast::Sender<OrderbookCommand>,
    /// Fires after every CLOB book update (token_id) — for WebSocket-driven strategies (e.g. spread capture).
    book_update_tx: broadcast::Sender<String>,
    /// Broadcast channel for price threshold events (best_ask >= sniper entry minimum)
    price_event_tx: broadcast::Sender<PriceThresholdEvent>,
    /// Minimum best_ask to emit threshold events (matches sniper entry gate)
    sniper_entry_min_best_ask: Decimal,
    /// Last time any WebSocket message was received (Unix timestamp). Used for staleness check.
    last_message_timestamp: Arc<AtomicU64>,
    /// Per-token cancel tokens for SDK orderbook stream tasks
    active_stream_cancels: Arc<DashMap<String, CancellationToken>>,
}

impl OrderbookManager {
    pub fn new(sniper_entry_min_best_ask: Decimal) -> Self {
        let (command_tx, _) = broadcast::channel(1000);
        let (book_update_tx, _) = broadcast::channel(16384);
        let (price_event_tx, _) = broadcast::channel(10000); // Large buffer for high-frequency events
        Self {
            orderbooks: Arc::new(DashMap::new()),
            command_tx,
            book_update_tx,
            price_event_tx,
            sniper_entry_min_best_ask,
            last_message_timestamp: Arc::new(AtomicU64::new(0)),
            active_stream_cancels: Arc::new(DashMap::new()),
        }
    }

    /// Subscribe to notifications after each orderbook snapshot update for a token (WebSocket-driven).
    pub fn subscribe_book_updates(&self) -> broadcast::Receiver<String> {
        self.book_update_tx.subscribe()
    }

    /// Subscribe to price threshold events when best_ask reaches the sniper entry minimum.
    /// Returns a receiver that will get events immediately when prices cross the threshold
    pub fn subscribe_price_events(&self) -> broadcast::Receiver<PriceThresholdEvent> {
        self.price_event_tx.subscribe()
    }

    /// Emit a price event when best_ask is at or above the sniper entry threshold.
    /// Called immediately after orderbook updates for sub-2ms detection latency
    fn check_and_emit_price_threshold(&self, token_id: &str) {
        let threshold = self.sniper_entry_min_best_ask;
        if let Some(orderbook) = self.orderbooks.get(token_id) {
            if let Some(best_ask) = orderbook.best_ask() {
                if best_ask >= threshold {
                    let event = PriceThresholdEvent {
                        token_id: token_id.to_string(),
                        best_ask,
                        timestamp: std::time::Instant::now(),
                    };
                    // Non-blocking send - if receiver is slow, drop the event (we'll get another one soon)
                    let _ = self.price_event_tx.send(event);
                }
            }
        }
    }

    pub fn get_orderbook(&self, token_id: &str) -> Option<OrderbookState> {
        self.orderbooks.get(token_id).map(|r| r.clone())
    }

    pub fn subscribe(&self, token_id: String) -> anyhow::Result<()> {
        self.command_tx
            .send(OrderbookCommand::Subscribe(token_id))
            .map_err(|e| anyhow::anyhow!("Failed to send subscribe command: {}", e))?;
        Ok(())
    }

    /// Initialize orderbook with initial prices from Gamma API
    /// This ensures prices are available immediately before WebSocket updates arrive
    pub fn initialize_orderbook(&self, token_id: String, best_bid: Option<Decimal>, best_ask: Option<Decimal>) {
        let mut bids = Vec::new();
        let mut asks = Vec::new();

        if let Some(bid) = best_bid {
            bids.push(crate::types::OrderbookLevel {
                price: bid,
                size: Decimal::ZERO,
            });
        }

        if let Some(ask) = best_ask {
            asks.push(crate::types::OrderbookLevel {
                price: ask,
                size: Decimal::ZERO,
            });
        }

        let state = crate::types::OrderbookState {
            token_id: token_id.clone(),
            bids,
            asks,
            timestamp: chrono::Utc::now(),
        };

        self.orderbooks.insert(token_id, state);
    }

    fn parse_asset_id(token_id: &str) -> Result<U256> {
        U256::from_str(token_id).map_err(|e| anyhow::anyhow!("invalid asset id {token_id}: {e}"))
    }

    fn book_update_to_state(book: &BookUpdate) -> OrderbookState {
        let token_id = book.asset_id.to_string();
        let bids: Vec<crate::types::OrderbookLevel> = book
            .bids
            .iter()
            .map(|l| crate::types::OrderbookLevel {
                price: clean_ws_price(l.price),
                size: l.size,
            })
            .collect();
        let asks: Vec<crate::types::OrderbookLevel> = book
            .asks
            .iter()
            .map(|l| crate::types::OrderbookLevel {
                price: clean_ws_price(l.price),
                size: l.size,
            })
            .collect();
        let timestamp =
            DateTime::from_timestamp_millis(book.timestamp).unwrap_or_else(Utc::now);

        OrderbookState {
            token_id,
            bids,
            asks,
            timestamp,
        }
    }

    fn apply_book_update(&self, book: &BookUpdate) {
        self.last_message_timestamp.store(unix_secs(), Ordering::Relaxed);

        let asset_id = book.asset_id.to_string();
        let state = Self::book_update_to_state(book);
        let best_bid = state.best_bid();
        let best_ask = state.best_ask();
        self.orderbooks.insert(asset_id.clone(), state);

        debug!(
            asset_id = %asset_id,
            best_bid = ?best_bid,
            best_ask = ?best_ask,
            "Orderbook snapshot received (SDK book stream)"
        );

        self.check_and_emit_price_threshold(&asset_id);
        let _ = self.book_update_tx.send(asset_id);
    }

    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) -> Result<()> {
        let mut command_rx = self.command_tx.subscribe();

        self.last_message_timestamp.store(unix_secs(), Ordering::Relaxed);

        let client = WsClient::default();
        tracing::info!("Using Polymarket SDK CLOB WebSocket client for orderbook streams");

        let stale_self = Arc::clone(&self);
        let stale_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut staleness_interval = tokio::time::interval(Duration::from_secs(2));
            staleness_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = stale_shutdown.cancelled() => break,
                    _ = staleness_interval.tick() => {
                        let now = unix_secs();
                        let last = stale_self.last_message_timestamp.load(Ordering::Relaxed);
                        if now.saturating_sub(last) > STALENESS_SECS {
                            tracing::warn!(
                                "CLOB orderbook stream stale — no book updates for {}s (SDK may reconnect)",
                                STALENESS_SECS
                            );
                        }
                    }
                }
            }
        });

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!("Orderbook manager shutting down");
                    for entry in self.active_stream_cancels.iter() {
                        entry.value().cancel();
                    }
                    self.active_stream_cancels.clear();
                    break Ok(());
                }
                cmd = command_rx.recv() => {
                    match cmd {
                        Ok(OrderbookCommand::Subscribe(token_id)) => {
                            if self.active_stream_cancels.contains_key(&token_id) {
                                debug!(token_id = %token_id, "Already subscribed to orderbook (SDK stream)");
                                continue;
                            }

                            let asset_u256 = match Self::parse_asset_id(&token_id) {
                                Ok(id) => id,
                                Err(e) => {
                                    tracing::error!(token_id = %token_id, error = %e, "Bad token id for CLOB subscription");
                                    continue;
                                }
                            };

                            let stream = match client.subscribe_orderbook(vec![asset_u256]) {
                                Ok(s) => {
                                    info!("WS STREAM CONNECTED for asset: {}", token_id);
                                    s
                                }
                                Err(e) => {
                                    error!("WS SUBSCRIBE FAILED for {}: {:?}", token_id, e);
                                    continue;
                                }
                            };

                            let cancel = CancellationToken::new();
                            self.active_stream_cancels.insert(token_id.clone(), cancel.clone());

                            let om = Arc::clone(&self);
                            let client_task = client.clone();
                            let token_for_map = token_id.clone();
                            let cancels = Arc::clone(&self.active_stream_cancels);

                            tokio::spawn(async move {
                                let asset_id = token_for_map.clone();
                                info!("WS TASK STARTED for asset: {}", asset_id);
                                let mut stream = Box::pin(stream);
                                loop {
                                    tokio::select! {
                                        _ = cancel.cancelled() => {
                                            if let Err(e) = client_task.unsubscribe_orderbook(&[asset_u256]) {
                                                tracing::error!(error = %e, "SDK unsubscribe_orderbook failed");
                                            }
                                            cancels.remove(&token_for_map);
                                            break;
                                        }
                                        next = stream.next() => {
                                            match next {
                                                None => {
                                                    info!("WS STREAM ENDED for {} — loop exited naturally", asset_id);
                                                    if let Err(e) = client_task.unsubscribe_orderbook(&[asset_u256]) {
                                                        tracing::error!(error = %e, "SDK unsubscribe_orderbook failed");
                                                    }
                                                    cancels.remove(&token_for_map);
                                                    break;
                                                }
                                                Some(msg) => {
                                                    debug!("WS MSG RECEIVED for {}: {:?}", asset_id, msg);
                                                    match msg {
                                                        Ok(book) => {
                                                            om.apply_book_update(&book);
                                                        }
                                                        Err(e) => {
                                                            tracing::error!("SDK WS stream error for token {}: {}", token_for_map, e);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            });
                        }
                        Ok(OrderbookCommand::Unsubscribe(token_id)) => {
                            if let Some((_k, cancel)) = self.active_stream_cancels.remove(&token_id) {
                                cancel.cancel();
                            }
                        }
                        Err(_) => {}
                    }
                }
            }
        }
    }
}
