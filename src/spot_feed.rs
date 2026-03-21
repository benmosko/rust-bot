use crate::types::{Coin, SpotState};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use futures_util::StreamExt;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use serde::Deserialize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

const BINANCE_WS_BASE: &str = "wss://stream.binance.com:9443/ws";
/// Rolling Binance spot history per coin for sniper momentum-reversal filter (~60s, pruned on push).
pub type SharedBinanceSpotHistory = Arc<DashMap<Coin, Mutex<VecDeque<(i64, f64)>>>>;

pub fn new_shared_binance_spot_history() -> SharedBinanceSpotHistory {
    Arc::new(DashMap::new())
}

/// Binance spot price from ~8–15s ago for momentum reversal logic, or `None` to skip filtering (fail open).
pub fn spot_price_for_momentum_reversal(
    history: &DashMap<Coin, Mutex<VecDeque<(i64, f64)>>>,
    coin: Coin,
) -> Option<f64> {
    let now = Utc::now().timestamp();
    let map_ref = history.get(&coin)?;
    let guard = map_ref.value().lock().ok()?;
    let q = &*guard;
    for i in (0..q.len()).rev() {
        let (ts, price) = q[i];
        if ts <= now - 8 {
            let age = now - ts;
            if age > 15 {
                return None;
            }
            return Some(price);
        }
    }
    None
}

fn push_binance_tick(
    history: &DashMap<Coin, Mutex<VecDeque<(i64, f64)>>>,
    coin: Coin,
    ts_secs: i64,
    price_f64: f64,
) {
    if price_f64 <= 0.0 {
        return;
    }
    let entry = history
        .entry(coin)
        .or_insert_with(|| Mutex::new(VecDeque::new()));
    let Ok(mut guard) = entry.lock() else {
        return;
    };
    guard.push_back((ts_secs, price_f64));
    let cutoff = ts_secs.saturating_sub(60);
    while let Some(&(front_ts, _)) = guard.front() {
        if front_ts < cutoff {
            guard.pop_front();
        } else {
            break;
        }
    }
    const MAX_LEN: usize = 10_000;
    while guard.len() > MAX_LEN {
        guard.pop_front();
    }
}

#[derive(Debug, Deserialize)]
struct BinanceAggTrade {
    #[serde(rename = "p")]
    price: String,
    /// Trade time (ms).
    #[serde(rename = "T")]
    trade_time_ms: i64,
}

pub struct SpotFeed {
    coin: Coin,
    sender: watch::Sender<SpotState>,
    receiver: watch::Receiver<SpotState>,
    binance_spot_history: SharedBinanceSpotHistory,
}

impl SpotFeed {
    pub fn new(coin: Coin, binance_spot_history: SharedBinanceSpotHistory) -> Self {
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
            binance_spot_history,
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
        // aggTrade: frequent ticks (unlike kline_1m) so ~8–15s lookback is meaningful for reversal filter.
        let url = format!("{}/{}@aggTrade", BINANCE_WS_BASE, symbol);

        info!(coin = ?self.coin, "Starting spot feed");

        let mut reconnect_delay = Duration::from_secs(1);
        let max_delay = Duration::from_secs(30);

        loop {
            if shutdown.is_cancelled() {
                info!(coin = ?self.coin, "Spot feed shutting down");
                break;
            }

            match self.connect_and_run(&url, &mut shutdown).await {
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
                            if let Err(e) = self.handle_message(&text).await {
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

    async fn handle_message(&mut self, text: &str) -> Result<()> {
        let trade: BinanceAggTrade = serde_json::from_str(text)
            .context("Failed to parse Binance aggTrade message")?;

        let price = trade
            .price
            .parse::<Decimal>()
            .context("Failed to parse price")?;

        let ts_ms = trade.trade_time_ms;
        let timestamp = DateTime::from_timestamp(
            ts_ms / 1000,
            ((ts_ms % 1000) * 1_000_000) as u32,
        )
        .context("Invalid timestamp")?;

        let ts_secs = ts_ms / 1000;
        if let Some(pf) = price.to_f64() {
            push_binance_tick(&self.binance_spot_history, self.coin, ts_secs, pf);
        }

        let mut state = self.sender.borrow().clone();
        state.price = price;
        state.timestamp = timestamp;
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
}
