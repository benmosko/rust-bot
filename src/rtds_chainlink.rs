//! Polymarket Real-Time Data Socket (RTDS) — Chainlink crypto prices.
//! Same stream Polymarket documents for crypto market data; use for resolution vs Gamma when possible.
//!
//! Endpoint: `wss://ws-live-data.polymarket.com` — see Polymarket RTDS docs.

use crate::types::{Coin, Period};
use anyhow::{Context, Result};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Deserialize;
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

const RTDS_WS_URL: &str = "wss://ws-live-data.polymarket.com";
/// Retain Chainlink prints for late round registration and boundary lookup.
/// Keep ≥ 2× longest market period (15m) so buffer fallback can still see boundary ticks.
const PRICE_HISTORY_RETAIN_SECS: i64 = 30 * 60;

/// Outcome for a crypto UP/DOWN round from Chainlink open vs close (same rules as Polymarket).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainlinkOutcome {
    /// `close_price > open_price` (price moved up).
    UpWon,
    /// `close_price < open_price` (price moved down).
    DownWon,
    /// `close_price == open_price` (no movement; neither UP nor DOWN wins).
    Tie,
}

#[derive(Debug, Clone)]
struct RoundChainlink {
    round_end: i64,
    /// First Chainlink print at/after round start while in-window (see `on_tick`).
    open: Option<Decimal>,
    /// First print at/after `round_end` (boundary = next round open); prefer same rule as `lookup_first_ge`.
    close: Option<Decimal>,
}

/// Shared state: latest Chainlink USD per coin + per-round open/close for settlement inference.
#[derive(Debug)]
pub struct ChainlinkTracker {
    rounds: DashMap<(Coin, Period, i64), RoundChainlink>,
    latest: DashMap<Coin, Decimal>,
    /// Rolling window of `(payload_timestamp_secs, price)` per coin for boundary lookup.
    price_history: DashMap<Coin, VecDeque<(i64, Decimal)>>,
}

impl ChainlinkTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            rounds: DashMap::new(),
            latest: DashMap::new(),
            price_history: DashMap::new(),
        })
    }

    fn push_price_history(&self, coin: Coin, ts: i64, price: Decimal) {
        let mut q = self
            .price_history
            .entry(coin)
            .or_insert_with(VecDeque::new);
        q.push_back((ts, price));
        let cutoff = ts.saturating_sub(PRICE_HISTORY_RETAIN_SECS);
        while let Some(&(front_ts, _)) = q.front() {
            if front_ts < cutoff {
                q.pop_front();
            } else {
                break;
            }
        }
    }

    /// First chronological timestamp at/after `t`, then the **last** print at that timestamp
    /// (multiple Chainlink updates can share a second; use the latest quote at the boundary).
    fn lookup_first_ge(&self, coin: Coin, t: i64) -> Option<Decimal> {
        let q = self.price_history.get(&coin)?;
        let mut boundary_ts: Option<i64> = None;
        for (ts, _) in q.iter() {
            if *ts >= t {
                boundary_ts = Some(match boundary_ts {
                    None => *ts,
                    Some(b) => b.min(*ts),
                });
            }
        }
        let bt = boundary_ts?;
        q.iter()
            .rev()
            .find(|(ts, _)| *ts == bt)
            .map(|(_, p)| *p)
    }

    /// Call when a round becomes active (same moment as inserting `active_rounds`).
    ///
    /// Open and the previous round's close use the same boundary price: the first Chainlink print
    /// at/after `round_start`, taken from the rolling buffer when available so late registration
    /// still sees the correct open.
    pub fn register_round(&self, coin: Coin, period: Period, round_start: i64, round_end: i64) {
        let open = self.lookup_first_ge(coin, round_start);
        self.rounds.insert(
            (coin, period, round_start),
            RoundChainlink {
                round_end,
                open,
                close: None,
            },
        );
        if let Some(p) = open {
            self.set_previous_round_close_to_boundary_price(coin, period, round_start, p);
        }
        debug!(
            coin = ?coin,
            period = ?period,
            round_start,
            round_end,
            "Chainlink tracker: round registered"
        );
    }

    /// Close of round N = open of round N+1 (same boundary timestamp).
    fn set_previous_round_close_to_boundary_price(
        &self,
        coin: Coin,
        period: Period,
        round_start: i64,
        boundary_price: Decimal,
    ) {
        let prev_start = round_start.saturating_sub(period.as_seconds());
        if prev_start >= round_start {
            return;
        }
        let Some(mut prev) = self.rounds.get_mut(&(coin, period, prev_start)) else {
            return;
        };
        prev.close = Some(boundary_price);
        info!(
            coin = ?coin,
            period = ?period,
            prev_round_start = prev_start,
            close = %boundary_price,
            "Chainlink closing price for previous round (same as next round open)"
        );
    }

    pub fn remove_round(&self, coin: Coin, period: Period, round_start: i64) {
        self.rounds.remove(&(coin, period, round_start));
    }

    /// Apply one Chainlink tick: update history, `latest`, and any registered rounds for this coin.
    /// `tick_secs` should be the payload timestamp when present, else wall-clock seconds.
    pub fn on_tick(&self, coin: Coin, value: Decimal, tick_secs: i64) {
        self.push_price_history(coin, tick_secs, value);
        self.latest.insert(coin, value);

        for mut ent in self.rounds.iter_mut() {
            let (c, p, round_start) = *ent.key();
            if c != coin {
                continue;
            }
            let st = ent.value_mut();
            // Opening: first buffered price at/after round start (buffer includes this tick).
            // Refine when the same-second boundary gets a newer quote after `register_round`.
            if tick_secs >= round_start && tick_secs < st.round_end {
                let open_price = self
                    .lookup_first_ge(coin, round_start)
                    .unwrap_or(value);
                let first_set = st.open.is_none();
                if first_set || st.open != Some(open_price) {
                    st.open = Some(open_price);
                    if first_set {
                        info!(
                            coin = ?coin,
                            round_start,
                            open = %open_price,
                            "Chainlink opening price captured"
                        );
                    }
                    drop(ent);
                    self.set_previous_round_close_to_boundary_price(coin, p, round_start, open_price);
                    continue;
                }
            }
            // Closing: first tick at/after round end if we never got a successor round's open
            // (e.g. bot stopped before the next round was registered). Use the same boundary rule as
            // `lookup_first_ge` (last quote at the boundary second), not the raw first tick value.
            if tick_secs >= st.round_end && st.close.is_none() {
                st.close = self
                    .lookup_first_ge(coin, st.round_end)
                    .or(Some(value));
            }
        }
    }

    /// Resolve using **stored** open/close from boundary-time capture (`on_tick` / `register_round`).
    /// Falls back to `lookup_first_ge` on the rolling buffer only when a boundary was missed
    /// (e.g. bot started mid-round). Buffer alone at resolution time is unreliable (rotation).
    ///
    /// Rules: UP wins iff `close > open`; DOWN wins iff `close < open`; if equal, neither wins.
    /// Returns `None` if data is missing or round not finished (caller falls back to Gamma).
    pub fn try_resolve_outcome(
        &self,
        coin: Coin,
        period: Period,
        round_start: i64,
    ) -> Option<ChainlinkOutcome> {
        let key = (coin, period, round_start);
        let now = chrono::Utc::now().timestamp();

        let round_end = self.rounds.get(&key)?.round_end;
        if now < round_end {
            return None;
        }

        let close_was_stored = self
            .rounds
            .get(&key)
            .map(|e| e.close.is_some())
            .unwrap_or(false);

        if let Some(mut e) = self.rounds.get_mut(&key) {
            if now >= e.round_end && e.close.is_none() {
                if let Some(c) = self.lookup_first_ge(coin, e.round_end) {
                    e.close = Some(c);
                }
            }
        }

        let entry = self.rounds.get(&key)?;

        let (open, open_source) = match entry.open {
            Some(o) => (o, "stored"),
            None => (
                self.lookup_first_ge(coin, round_start)?,
                "buffer_lookup",
            ),
        };

        let (close, close_source) = if close_was_stored {
            (entry.close?, "stored")
        } else {
            let c = entry
                .close
                .or_else(|| self.lookup_first_ge(coin, round_end))?;
            (c, "buffer_lookup")
        };

        let outcome = match close.cmp(&open) {
            Ordering::Greater => ChainlinkOutcome::UpWon,
            Ordering::Less => ChainlinkOutcome::DownWon,
            Ordering::Equal => ChainlinkOutcome::Tie,
        };
        info!(
            coin = ?coin,
            period = ?period,
            round_start,
            round_end,
            open = %open,
            close = %close,
            open_source,
            close_source,
            ?outcome,
            "Chainlink resolution"
        );
        Some(outcome)
    }

    /// Chainlink open price for a registered round (if captured).
    pub fn chainlink_open_for_round(
        &self,
        coin: Coin,
        period: Period,
        round_start: i64,
    ) -> Option<Decimal> {
        self.rounds
            .get(&(coin, period, round_start))
            .and_then(|r| r.open)
    }

    /// Open and close used for settlement logging (call after [`Self::try_resolve_outcome`] succeeds, before [`Self::remove_round`]).
    pub fn settlement_prices_for_round(
        &self,
        coin: Coin,
        period: Period,
        round_start: i64,
    ) -> Option<(Decimal, Decimal)> {
        let key = (coin, period, round_start);
        let entry = self.rounds.get(&key)?;
        let open = entry.open.or_else(|| self.lookup_first_ge(coin, round_start))?;
        let close = entry
            .close
            .or_else(|| self.lookup_first_ge(coin, entry.round_end))?;
        Some((open, close))
    }
}

pub fn coin_from_chainlink_symbol(sym: &str) -> Option<Coin> {
    match sym.to_lowercase().as_str() {
        "btc/usd" => Some(Coin::Btc),
        "eth/usd" => Some(Coin::Eth),
        "sol/usd" => Some(Coin::Sol),
        "xrp/usd" => Some(Coin::Xrp),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct RtdsEnvelope {
    topic: String,
    #[allow(dead_code)]
    #[serde(rename = "type")]
    type_: String,
    payload: Option<RtdsPricePayload>,
}

#[derive(Debug, Deserialize)]
struct RtdsPricePayload {
    symbol: String,
    #[allow(dead_code)]
    timestamp: Option<i64>,
    value: serde_json::Value,
}

fn parse_price(v: &serde_json::Value) -> Option<Decimal> {
    match v {
        serde_json::Value::Number(n) => n.as_f64().and_then(Decimal::from_f64_retain),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

pub async fn run_rtds_chainlink_feed(
    tracker: Arc<ChainlinkTracker>,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        if shutdown.is_cancelled() {
            info!("RTDS Chainlink feed shutting down");
            return Ok(());
        }

        match run_one_connection(tracker.clone(), shutdown.clone()).await {
            Ok(()) => {
                if shutdown.is_cancelled() {
                    return Ok(());
                }
            }
            Err(e) => {
                error!(error = %e, "RTDS Chainlink connection ended; reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

async fn run_one_connection(
    tracker: Arc<ChainlinkTracker>,
    shutdown: CancellationToken,
) -> Result<()> {
    let (ws, _) = connect_async(RTDS_WS_URL)
        .await
        .context("RTDS WebSocket connect failed")?;

    info!(url = RTDS_WS_URL, "Connected to Polymarket RTDS (Chainlink)");

    let (mut write, mut read) = ws.split();

    let sub = serde_json::json!({
        "action": "subscribe",
        "subscriptions": [{
            "topic": "crypto_prices_chainlink",
            "type": "*",
            "filters": ""
        }]
    });
    write
        .send(Message::Text(sub.to_string()))
        .await
        .context("RTDS subscribe send failed")?;

    let mut ping = interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                let _ = write.send(Message::Close(None)).await;
                return Ok(());
            }
            _ = ping.tick() => {
                if let Err(e) = write.send(Message::Text("PING".into())).await {
                    warn!(error = %e, "RTDS PING failed");
                    return Err(e.into());
                }
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if text == "PONG" {
                            continue;
                        }
                        if let Err(e) = handle_rtds_text(&tracker, &text) {
                            debug!(error = %e, text = %text, "RTDS text parse skip");
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = write.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Close(_))) => {
                        anyhow::bail!("RTDS closed by server");
                    }
                    Some(Err(e)) => return Err(e.into()),
                    None => anyhow::bail!("RTDS stream ended"),
                    _ => {}
                }
            }
        }
    }
}

fn handle_rtds_text(tracker: &ChainlinkTracker, text: &str) -> Result<()> {
    let env: RtdsEnvelope = serde_json::from_str(text).context("not JSON")?;
    if env.topic != "crypto_prices_chainlink" {
        return Ok(());
    }
    let payload = match env.payload {
        Some(p) => p,
        None => return Ok(()),
    };
    let coin = match coin_from_chainlink_symbol(&payload.symbol) {
        Some(c) => c,
        None => return Ok(()),
    };
    let value = match parse_price(&payload.value) {
        Some(v) => v,
        None => return Ok(()),
    };
    if value <= dec!(0) {
        return Ok(());
    }

    let now_secs = chrono::Utc::now().timestamp();
    let tick_secs = payload.timestamp.unwrap_or(now_secs);
    tracker.on_tick(coin, value, tick_secs);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up_won_when_close_higher() {
        let t = ChainlinkTracker::new();
        let coin = Coin::Btc;
        let period = Period::Five;
        let rs = 1000;
        let re = rs + period.as_seconds();
        t.register_round(coin, period, rs, re);
        t.on_tick(coin, dec!(100), rs);
        t.on_tick(coin, dec!(101), re);
        assert_eq!(
            t.try_resolve_outcome(coin, period, rs),
            Some(ChainlinkOutcome::UpWon)
        );
    }

    #[test]
    fn down_won_when_close_lower() {
        let t = ChainlinkTracker::new();
        let coin = Coin::Eth;
        let period = Period::Five;
        let rs = 2000;
        let re = rs + period.as_seconds();
        t.register_round(coin, period, rs, re);
        t.on_tick(coin, dec!(200), rs);
        t.on_tick(coin, dec!(199), re);
        assert_eq!(
            t.try_resolve_outcome(coin, period, rs),
            Some(ChainlinkOutcome::DownWon)
        );
    }

    #[test]
    fn tie_when_open_equals_close() {
        let t = ChainlinkTracker::new();
        let coin = Coin::Btc;
        let period = Period::Five;
        let rs = 3000;
        let re = rs + period.as_seconds();
        t.register_round(coin, period, rs, re);
        t.on_tick(coin, dec!(50), rs);
        t.on_tick(coin, dec!(50), re);
        assert_eq!(
            t.try_resolve_outcome(coin, period, rs),
            Some(ChainlinkOutcome::Tie)
        );
    }

    /// Close of N must equal open of N+1; old bug kept overwriting close with later ticks.
    #[test]
    fn close_matches_next_open_not_stale_later_tick() {
        let t = ChainlinkTracker::new();
        let coin = Coin::Sol;
        let period = Period::Five;
        let rs_n = 1_000_000;
        let re_n = rs_n + period.as_seconds();
        let rs_next = re_n;

        t.register_round(coin, period, rs_n, re_n);
        t.on_tick(coin, dec!(89.648), rs_n);

        // Stale / drift: ticks after round end before next round is registered would have
        // polluted `close` under the old "keep updating" logic.
        t.on_tick(coin, dec!(80.0), re_n);
        t.on_tick(coin, dec!(81.0), re_n + 30);

        t.register_round(coin, period, rs_next, rs_next + period.as_seconds());

        // Boundary: first print in N+1 window defines both N's close and N+1's open.
        t.on_tick(coin, dec!(89.689), rs_next);

        assert_eq!(
            t.try_resolve_outcome(coin, period, rs_n),
            Some(ChainlinkOutcome::UpWon)
        );
    }

    /// After buffer rotation, both boundary lookups can return the same tick (open==close from buffer);
    /// resolution must still use stored boundary prices.
    #[test]
    fn resolution_prefers_stored_prices_when_buffer_rotated() {
        let t = ChainlinkTracker::new();
        let coin = Coin::Btc;
        let period = Period::Five;
        let rs = 10_000_000_i64;
        let re = rs + period.as_seconds();
        t.register_round(coin, period, rs, re);
        t.on_tick(coin, dec!(70298.43), rs);
        t.on_tick(coin, dec!(70354.93), re);

        // One tick past retention window drops boundary prints; buffer would give same price for both lookups.
        let stale_ts = re + PRICE_HISTORY_RETAIN_SECS + 1;
        t.on_tick(coin, dec!(70364.21), stale_ts);

        assert_eq!(
            t.try_resolve_outcome(coin, period, rs),
            Some(ChainlinkOutcome::UpWon)
        );
    }

    /// Round registered minutes after start still gets the first print at/after `round_start` from history.
    #[test]
    fn late_register_uses_buffered_open_not_first_tick_after_registration() {
        let t = ChainlinkTracker::new();
        let coin = Coin::Btc;
        let period = Period::Fifteen;
        let rs = 50_000;
        let re = rs + period.as_seconds();
        t.on_tick(coin, dec!(100), rs);
        t.on_tick(coin, dec!(999), rs + 660);

        t.register_round(coin, period, rs, re);
        t.on_tick(coin, dec!(101), re);

        assert_eq!(
            t.try_resolve_outcome(coin, period, rs),
            Some(ChainlinkOutcome::UpWon)
        );
    }
}
