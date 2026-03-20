use crate::types::{Coin, SpotState};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Deserialize;
use std::collections::VecDeque;
use tokio::sync::watch;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

const BINANCE_WS_BASE: &str = "wss://stream.binance.com:9443/ws";

#[derive(Debug, Deserialize)]
struct BinanceKline {
    #[serde(rename = "k")]
    kline: BinanceKlineData,
}

#[derive(Debug, Deserialize)]
struct BinanceKlineData {
    #[serde(rename = "c")]
    close: String,
    #[serde(rename = "T")]
    close_time: i64,
}

pub struct SpotFeed {
    coin: Coin,
    sender: watch::Sender<SpotState>,
    receiver: watch::Receiver<SpotState>,
}

impl SpotFeed {
    pub fn new(coin: Coin) -> Self {
        let (sender, receiver) = watch::channel(SpotState {
            price: dec!(0),
            timestamp: Utc::now(),
            opening_price: None,
            opening_timestamp: None,
        });
        Self {
            coin,
            sender,
            receiver,
        }
    }

    pub fn receiver(&self) -> watch::Receiver<SpotState> {
        self.receiver.clone()
    }

    pub async fn run(
        &mut self,
        mut shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        let symbol = self.coin.binance_symbol();
        let url = format!("{}/{}@kline_1m", BINANCE_WS_BASE, symbol);

        info!(coin = ?self.coin, "Starting spot feed");

        let mut reconnect_delay = Duration::from_secs(1);
        let max_delay = Duration::from_secs(30);
        let mut recent_prices: VecDeque<(Decimal, DateTime<Utc>)> = VecDeque::with_capacity(60);

        loop {
            if shutdown.is_cancelled() {
                info!(coin = ?self.coin, "Spot feed shutting down");
                break;
            }

            match self.connect_and_run(&url, &mut recent_prices, &mut shutdown).await {
                Ok(_) => break,
                Err(e) => {
                    error!(coin = ?self.coin, error = %e, "Spot feed connection error, reconnecting");
                    sleep(reconnect_delay).await;
                    reconnect_delay = (reconnect_delay * 2).min(max_delay);
                }
            }
        }

        Ok(())
    }

    async fn connect_and_run(
        &mut self,
        url: &str,
        recent_prices: &mut VecDeque<(Decimal, DateTime<Utc>)>,
        shutdown: &mut tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        let (ws_stream, _) = connect_async(url)
            .await
            .context("Failed to connect to Binance WebSocket")?;

        info!(coin = ?self.coin, "Connected to Binance WebSocket");

        let (mut _write, mut read) = ws_stream.split();

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    break;
                }
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Err(e) = self.handle_message(&text, recent_prices).await {
                                error!(coin = ?self.coin, error = %e, "Error handling message");
                            }
                        }
                        Some(Ok(Message::Close(_))) => {
                            warn!(coin = ?self.coin, "WebSocket closed by server");
                            return Err(anyhow::anyhow!("WebSocket closed"));
                        }
                        Some(Err(e)) => {
                            error!(coin = ?self.coin, error = %e, "WebSocket error");
                            return Err(e.into());
                        }
                        None => {
                            warn!(coin = ?self.coin, "WebSocket stream ended");
                            return Err(anyhow::anyhow!("WebSocket stream ended"));
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_message(
        &mut self,
        text: &str,
        recent_prices: &mut VecDeque<(Decimal, DateTime<Utc>)>,
    ) -> Result<()> {
        let kline: BinanceKline = serde_json::from_str(text)
            .context("Failed to parse Binance kline message")?;

        let price = kline.kline.close.parse::<Decimal>()
            .context("Failed to parse price")?;

        let timestamp = DateTime::from_timestamp(
            kline.kline.close_time / 1000,
            ((kline.kline.close_time % 1000) * 1_000_000) as u32,
        )
        .context("Invalid timestamp")?;

        recent_prices.push_back((price, timestamp));
        if recent_prices.len() > 60 {
            recent_prices.pop_front();
        }

        let mut state = self.sender.borrow().clone();
        state.price = price;
        state.timestamp = timestamp;

        // Set opening price if this is the start of a new round
        // This is handled by the round manager, but we track it here for staleness checks
        self.sender.send(state)?;

        Ok(())
    }

    #[allow(dead_code)]
    pub fn set_opening_price(&mut self, price: Decimal, round_start: i64) -> Result<()> {
        let mut state = self.sender.borrow().clone();
        state.opening_price = Some(price);
        state.opening_timestamp = Some(DateTime::from_timestamp(round_start, 0)
            .context("Invalid round_start timestamp")?);
        self.sender.send(state)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn get_recent_prices(
        &self,
        recent_prices: &VecDeque<(Decimal, DateTime<Utc>)>,
        seconds: u64,
    ) -> Vec<Decimal> {
        let cutoff = Utc::now() - chrono::Duration::seconds(seconds as i64);
        recent_prices
            .iter()
            .filter(|(_, ts)| *ts >= cutoff)
            .map(|(p, _)| *p)
            .collect()
    }
}
