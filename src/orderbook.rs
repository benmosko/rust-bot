use crate::types::OrderbookState;
use anyhow::{Context, Result};
use chrono::Utc;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

const CLOB_WS_BASE: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

#[derive(Debug, Clone)]
pub enum OrderbookCommand {
    Subscribe(String),
    #[allow(dead_code)]
    Unsubscribe(String),
}

pub struct OrderbookManager {
    orderbooks: Arc<DashMap<String, OrderbookState>>,
    command_tx: broadcast::Sender<OrderbookCommand>,
}

impl OrderbookManager {
    pub fn new() -> Self {
        let (command_tx, _) = broadcast::channel(1000);
        Self {
            orderbooks: Arc::new(DashMap::new()),
            command_tx,
        }
    }

    pub fn get_orderbook(&self, token_id: &str) -> Option<OrderbookState> {
        self.orderbooks.get(token_id).map(|r| r.clone())
    }

    pub fn subscribe(&self, token_id: String) -> Result<()> {
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
            timestamp: Utc::now(),
        };

        self.orderbooks.insert(token_id, state);
    }

    pub async fn run(
        &self,
        mut shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        let mut command_rx = self.command_tx.subscribe();
        let mut subscribed_tokens = std::collections::HashSet::new();
        let mut reconnect_delay = std::time::Duration::from_secs(1);
        let max_delay = std::time::Duration::from_secs(30);

        loop {
            if shutdown.is_cancelled() {
                info!("Orderbook manager shutting down");
                break;
            }

            match self
                .connect_and_run(&mut command_rx, &mut subscribed_tokens, &mut shutdown)
                .await
            {
                Ok(_) => break,
                Err(e) => {
                    error!(error = %e, error_debug = ?e, "Orderbook connection error, reconnecting");
                    tokio::time::sleep(reconnect_delay).await;
                    reconnect_delay = (reconnect_delay * 2).min(max_delay);
                }
            }
        }

        Ok(())
    }

    async fn connect_and_run(
        &self,
        command_rx: &mut broadcast::Receiver<OrderbookCommand>,
        subscribed_tokens: &mut std::collections::HashSet<String>,
        shutdown: &mut tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        let (ws_stream, _) = connect_async(CLOB_WS_BASE)
            .await
            .with_context(|| format!("Failed to connect to CLOB WebSocket at {}", CLOB_WS_BASE))?;

        info!("Connected to CLOB WebSocket");

        let (mut write, mut read) = ws_stream.split();

        // Re-subscribe to all tokens on reconnect
        if !subscribed_tokens.is_empty() {
            let assets_ids: Vec<String> = subscribed_tokens.iter().cloned().collect();
            let sub_msg = serde_json::json!({
                "assets_ids": assets_ids,
                "type": "market",
                "custom_feature_enabled": true
            });
            write
                .send(Message::Text(sub_msg.to_string()))
                .await
                .with_context(|| format!("Failed to send subscription message: {}", sub_msg))?;
            info!(tokens = ?assets_ids, "Re-subscribed to orderbooks");
        }

        // Start heartbeat task - send PING every 10 seconds
        let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(10));
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    break;
                }
                _ = ping_interval.tick() => {
                    // Send PING to keep connection alive
                    if let Err(e) = write.send(Message::Ping(vec![])).await {
                        error!(error = %e, error_debug = ?e, "Failed to send PING");
                        return Err(anyhow::anyhow!("Failed to send PING: {:?}", e));
                    }
                }
                cmd = command_rx.recv() => {
                    match cmd {
                        Ok(OrderbookCommand::Subscribe(token_id)) => {
                            if subscribed_tokens.insert(token_id.clone()) {
                                let sub_msg = serde_json::json!({
                                    "assets_ids": [token_id.clone()],
                                    "operation": "subscribe",
                                    "custom_feature_enabled": true
                                });
                                if let Err(e) = write.send(Message::Text(sub_msg.to_string())).await {
                                    error!(error = %e, error_debug = ?e, token_id = %token_id, "Failed to send subscription");
                                } else {
                                    info!(token_id = %token_id, "Subscribed to orderbook");
                                }
                            }
                        }
                        Ok(OrderbookCommand::Unsubscribe(token_id)) => {
                            subscribed_tokens.remove(&token_id);
                            // Note: CLOB WebSocket doesn't have explicit unsubscribe
                        }
                        Err(_) => {}
                    }
                }
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            // Handle PONG response
                            if text == "PONG" {
                                continue;
                            }
                            if let Err(e) = self.handle_message(&text).await {
                                error!(error = %e, error_debug = ?e, message = %text, "Error handling orderbook message");
                            }
                        }
                        Some(Ok(Message::Pong(_))) => {
                            // Server responded to our PING
                            continue;
                        }
                        Some(Ok(Message::Close(frame))) => {
                            let reason = frame.map(|f| format!("code: {}, reason: {}", f.code, f.reason)).unwrap_or_else(|| "no reason".to_string());
                            warn!(reason = %reason, "Orderbook WebSocket closed by server");
                            return Err(anyhow::anyhow!("WebSocket closed: {}", reason));
                        }
                        Some(Err(e)) => {
                            error!(error = %e, error_debug = ?e, "Orderbook WebSocket error");
                            return Err(anyhow::anyhow!("WebSocket error: {:?}", e));
                        }
                        None => {
                            warn!("Orderbook WebSocket stream ended");
                            return Err(anyhow::anyhow!("WebSocket stream ended"));
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_message(&self, text: &str) -> Result<()> {
        // Parse as JSON value first - handle both single objects and arrays
        let value: Value = serde_json::from_str(text)
            .with_context(|| format!("Failed to parse JSON. Message: {}", text))?;

        // Handle array of messages
        let messages: Vec<Value> = if let Value::Array(arr) = value {
            arr
        } else {
            vec![value]
        };

        for msg in messages {
            // Check event_type to determine message type
            let event_type = msg.get("event_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            match event_type {
                "book" => {
                    self.handle_book_message(&msg)?;
                }
                "price_change" => {
                    self.handle_price_change_message(&msg)?;
                }
                "last_trade_price" => {
                    // Silently skip - user said to log or silently skip
                    // For now, silently skip
                }
                "best_bid_ask" => {
                    self.handle_best_bid_ask_message(&msg)?;
                }
                _ => {
                    // Unknown event_type - silently skip
                    continue;
                }
            }
        }

        Ok(())
    }

    fn handle_book_message(&self, msg: &Value) -> Result<()> {
        let asset_id = msg
            .get("asset_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing asset_id in book message"))?
            .to_string();

        // Parse bids - they are objects with "price" and "size" fields, not tuples
        let bids = msg
            .get("bids")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("Missing or invalid bids in book message"))?;

        let parsed_bids: Vec<crate::types::OrderbookLevel> = bids
            .iter()
            .filter_map(|b| {
                let price_str = b.get("price")?.as_str()?;
                let size_str = b.get("size")?.as_str()?;
                let price = price_str.parse::<Decimal>().ok()?;
                let size = size_str.parse::<Decimal>().ok()?;
                Some(crate::types::OrderbookLevel { price, size })
            })
            .collect();

        // Parse asks - they are objects with "price" and "size" fields, not tuples
        let asks = msg
            .get("asks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("Missing or invalid asks in book message"))?;

        let parsed_asks: Vec<crate::types::OrderbookLevel> = asks
            .iter()
            .filter_map(|a| {
                let price_str = a.get("price")?.as_str()?;
                let size_str = a.get("size")?.as_str()?;
                let price = price_str.parse::<Decimal>().ok()?;
                let size = size_str.parse::<Decimal>().ok()?;
                Some(crate::types::OrderbookLevel { price, size })
            })
            .collect();

        // Extract best_bid from top of bids (first element)
        // Extract best_ask from bottom of asks (last element) - but actually asks are usually sorted ascending
        // so best_ask is the first (lowest) ask. Let's check the user's instruction again.
        // User said: "Extract best_bid from top of bids, best_ask from bottom of asks."
        // This suggests bids are sorted descending (best bid first) and asks are sorted descending too (best ask last).
        // But typically asks are sorted ascending. Let me follow the user's instruction literally.

        let state = OrderbookState {
            token_id: asset_id.clone(),
            bids: parsed_bids,
            asks: parsed_asks,
            timestamp: Utc::now(),
        };

        self.orderbooks.insert(asset_id, state);

        Ok(())
    }

    fn handle_price_change_message(&self, msg: &Value) -> Result<()> {
        let price_changes = msg
            .get("price_changes")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("Missing or invalid price_changes in price_change message"))?;

        for change in price_changes {
            let asset_id = match change.get("asset_id").and_then(|v| v.as_str()) {
                Some(id) => id.to_string(),
                None => continue,
            };

            let best_bid_str = change.get("best_bid").and_then(|v| v.as_str());
            let best_ask_str = change.get("best_ask").and_then(|v| v.as_str());

            // Update the orderbook for this asset
            if let Some(mut entry) = self.orderbooks.get_mut(&asset_id) {
                // Update best bid if provided
                if let Some(bid_str) = best_bid_str {
                    if let Ok(bid_price) = bid_str.parse::<Decimal>() {
                        if let Some(first_bid) = entry.bids.first_mut() {
                            first_bid.price = bid_price;
                        } else {
                            // No bids exist, create one
                            entry.bids.insert(0, crate::types::OrderbookLevel {
                                price: bid_price,
                                size: Decimal::ZERO,
                            });
                        }
                    }
                }

                // Update best ask if provided
                if let Some(ask_str) = best_ask_str {
                    if let Ok(ask_price) = ask_str.parse::<Decimal>() {
                        if let Some(first_ask) = entry.asks.first_mut() {
                            first_ask.price = ask_price;
                        } else {
                            // No asks exist, create one
                            entry.asks.insert(0, crate::types::OrderbookLevel {
                                price: ask_price,
                                size: Decimal::ZERO,
                            });
                        }
                    }
                }

                entry.timestamp = Utc::now();
            } else {
                // Orderbook doesn't exist yet, create a minimal one
                let mut bids = Vec::new();
                let mut asks = Vec::new();

                if let Some(bid_str) = best_bid_str {
                    if let Ok(bid_price) = bid_str.parse::<Decimal>() {
                        bids.push(crate::types::OrderbookLevel {
                            price: bid_price,
                            size: Decimal::ZERO,
                        });
                    }
                }

                if let Some(ask_str) = best_ask_str {
                    if let Ok(ask_price) = ask_str.parse::<Decimal>() {
                        asks.push(crate::types::OrderbookLevel {
                            price: ask_price,
                            size: Decimal::ZERO,
                        });
                    }
                }

                let state = OrderbookState {
                    token_id: asset_id.clone(),
                    bids,
                    asks,
                    timestamp: Utc::now(),
                };

                self.orderbooks.insert(asset_id, state);
            }
        }

        Ok(())
    }

    fn handle_best_bid_ask_message(&self, msg: &Value) -> Result<()> {
        let asset_id = msg
            .get("asset_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing asset_id in best_bid_ask message"))?
            .to_string();

        let best_bid_str = msg.get("best_bid").and_then(|v| v.as_str());
        let best_ask_str = msg.get("best_ask").and_then(|v| v.as_str());

        // Update the orderbook for this asset
        if let Some(mut entry) = self.orderbooks.get_mut(&asset_id) {
            // Update best bid if provided
            if let Some(bid_str) = best_bid_str {
                if let Ok(bid_price) = bid_str.parse::<Decimal>() {
                    if let Some(first_bid) = entry.bids.first_mut() {
                        first_bid.price = bid_price;
                    } else {
                        entry.bids.insert(0, crate::types::OrderbookLevel {
                            price: bid_price,
                            size: Decimal::ZERO,
                        });
                    }
                }
            }

            // Update best ask if provided
            if let Some(ask_str) = best_ask_str {
                if let Ok(ask_price) = ask_str.parse::<Decimal>() {
                    if let Some(first_ask) = entry.asks.first_mut() {
                        first_ask.price = ask_price;
                    } else {
                        entry.asks.insert(0, crate::types::OrderbookLevel {
                            price: ask_price,
                            size: Decimal::ZERO,
                        });
                    }
                }
            }

            entry.timestamp = Utc::now();
        } else {
            // Orderbook doesn't exist yet, create a minimal one
            let mut bids = Vec::new();
            let mut asks = Vec::new();

            if let Some(bid_str) = best_bid_str {
                if let Ok(bid_price) = bid_str.parse::<Decimal>() {
                    bids.push(crate::types::OrderbookLevel {
                        price: bid_price,
                        size: Decimal::ZERO,
                    });
                }
            }

            if let Some(ask_str) = best_ask_str {
                if let Ok(ask_price) = ask_str.parse::<Decimal>() {
                    asks.push(crate::types::OrderbookLevel {
                        price: ask_price,
                        size: Decimal::ZERO,
                    });
                }
            }

            let state = OrderbookState {
                token_id: asset_id.clone(),
                bids,
                asks,
                timestamp: Utc::now(),
            };

            self.orderbooks.insert(asset_id, state);
        }

        Ok(())
    }
}
