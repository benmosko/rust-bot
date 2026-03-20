use crate::types::OrderbookState;
use anyhow::{Context, Result};
use chrono::Utc;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
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

#[derive(Debug, Deserialize)]
struct OrderbookUpdate {
    #[serde(rename = "asset_id")]
    asset_id: String,
    bids: Vec<[String; 2]>,
    asks: Vec<[String; 2]>,
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
        // CLOB WebSocket sends orderbook updates
        // Format may vary, but typically includes asset_id, bids, asks
        let update: OrderbookUpdate = serde_json::from_str(text)
            .with_context(|| format!("Failed to parse orderbook update. Message: {}", text))?;

        let bids: Vec<_> = update
            .bids
            .iter()
            .map(|b| {
                let price = b[0].parse::<Decimal>().unwrap_or(Decimal::ZERO);
                let size = b[1].parse::<Decimal>().unwrap_or(Decimal::ZERO);
                crate::types::OrderbookLevel { price, size }
            })
            .collect();

        let asks: Vec<_> = update
            .asks
            .iter()
            .map(|a| {
                let price = a[0].parse::<Decimal>().unwrap_or(Decimal::ZERO);
                let size = a[1].parse::<Decimal>().unwrap_or(Decimal::ZERO);
                crate::types::OrderbookLevel { price, size }
            })
            .collect();

        let state = OrderbookState {
            token_id: update.asset_id.clone(),
            bids,
            asks,
            timestamp: Utc::now(),
        };

        self.orderbooks.insert(update.asset_id, state);

        Ok(())
    }
}
