use crate::types::OrderbookState;
use anyhow::{Context, Result};
use chrono::Utc;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

const CLOB_WS_BASE: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/";

#[derive(Debug, Clone)]
pub enum OrderbookCommand {
    Subscribe(String),
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
                    error!(error = %e, "Orderbook connection error, reconnecting");
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
            .context("Failed to connect to CLOB WebSocket")?;

        info!("Connected to CLOB WebSocket");

        let (mut write, mut read) = ws_stream.split();

        // Re-subscribe to all tokens on reconnect
        for token_id in subscribed_tokens.iter() {
            let sub_msg = serde_json::json!({
                "type": "subscribe",
                "channel": "market",
                "assets_id": token_id
            });
            write
                .send(Message::Text(sub_msg.to_string()))
                .await
                .context("Failed to send subscription")?;
        }

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    break;
                }
                cmd = command_rx.recv() => {
                    match cmd {
                        Ok(OrderbookCommand::Subscribe(token_id)) => {
                            if subscribed_tokens.insert(token_id.clone()) {
                                let sub_msg = serde_json::json!({
                                    "type": "subscribe",
                                    "channel": "market",
                                    "assets_id": token_id
                                });
                                if let Err(e) = write.send(Message::Text(sub_msg.to_string())).await {
                                    error!(error = %e, "Failed to send subscription");
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
                            if let Err(e) = self.handle_message(&text).await {
                                error!(error = %e, "Error handling orderbook message");
                            }
                        }
                        Some(Ok(Message::Close(_))) => {
                            warn!("Orderbook WebSocket closed by server");
                            return Err(anyhow::anyhow!("WebSocket closed"));
                        }
                        Some(Err(e)) => {
                            error!(error = %e, "Orderbook WebSocket error");
                            return Err(e.into());
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
            .context("Failed to parse orderbook update")?;

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
