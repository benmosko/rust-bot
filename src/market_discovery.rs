use crate::rtds_chainlink::{ChainlinkOutcome, ChainlinkTracker};
use crate::strategy_log::StrategyLogger;
use crate::telegram::TelegramBot;
use crate::types::{Coin, Market, Period, RoundHistoryEntry, RoundHistoryStatus, SpotState};
use anyhow::{Context, Result};
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Deserialize;
use serde_json;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use rust_decimal::prelude::ToPrimitive;

const GAMMA_API_BASE: &str = "https://gamma-api.polymarket.com";

#[derive(Debug, Deserialize)]
struct GammaEventResponse {
    id: String,
    slug: String,
    markets: Vec<GammaMarketResponse>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct GammaMarketResponse {
    #[serde(rename = "conditionId")]
    condition_id: String,
    #[serde(rename = "clobTokenIds")]
    clob_token_ids: String, // JSON string containing array
    outcomes: String, // JSON string containing array
    #[serde(rename = "outcomePrices")]
    outcome_prices: String, // JSON string containing array
    neg_risk: bool,
    #[serde(rename = "orderPriceMinTickSize")]
    order_price_min_tick_size: Option<f64>,
    #[serde(rename = "orderMinSize")]
    order_min_size: Option<f64>,
    #[serde(rename = "bestBid")]
    best_bid: Option<f64>,
    #[serde(rename = "bestAsk")]
    best_ask: Option<f64>,
    #[serde(rename = "lastTradePrice")]
    last_trade_price: Option<f64>,
    #[serde(rename = "feesEnabled")]
    fees_enabled: Option<bool>,
    #[serde(rename = "acceptingOrders")]
    accepting_orders: Option<bool>,
    #[serde(rename = "takerBaseFee")]
    taker_base_fee: Option<u64>,
    #[serde(rename = "makerBaseFee")]
    maker_base_fee: Option<u64>,
    /// Chainlink reference open at round start (when Gamma provides it).
    #[serde(rename = "openingPrice")]
    opening_price: Option<f64>,
    #[serde(flatten)]
    #[allow(dead_code)] // Intentionally unused - catches unknown fields to prevent deserialization errors
    extra: HashMap<String, serde_json::Value>,
}

impl Default for GammaMarketResponse {
    fn default() -> Self {
        Self {
            condition_id: String::new(),
            clob_token_ids: String::new(),
            outcomes: String::new(),
            outcome_prices: String::new(),
            neg_risk: false,
            order_price_min_tick_size: None,
            order_min_size: None,
            best_bid: None,
            best_ask: None,
            last_trade_price: None,
            fees_enabled: None,
            accepting_orders: None,
            taker_base_fee: None,
            maker_base_fee: None,
            opening_price: None,
            extra: HashMap::new(),
        }
    }
}

fn decimal_from_gamma_json_value(v: &serde_json::Value) -> Option<Decimal> {
    match v {
        serde_json::Value::Number(n) => n.as_f64().and_then(Decimal::from_f64_retain),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Alternate keys sometimes present on Gamma `extra` when not deserialized into struct fields.
fn gamma_opening_price_from_extra(extra: &HashMap<String, serde_json::Value>) -> Option<Decimal> {
    const KEYS: &[&str] = &[
        "openingPrice",
        "opening_price",
        "oraclePrice",
        "oracle_price",
        "priceToBeat",
        "cryptoOpeningPrice",
    ];
    for k in KEYS {
        if let Some(v) = extra.get(*k) {
            if let Some(d) = decimal_from_gamma_json_value(v) {
                return Some(d);
            }
        }
    }
    None
}

pub fn generate_slug(coin: Coin, period: Period, round_start: i64) -> String {
    format!(
        "{}-updown-{}m-{}",
        coin.as_str(),
        period.as_minutes(),
        round_start
    )
}

#[allow(dead_code)]
pub fn calculate_round_start(period: Period, timestamp: i64) -> i64 {
    let period_secs = period.as_seconds();
    (timestamp / period_secs) * period_secs
}

pub fn calculate_round_end(period: Period, round_start: i64) -> i64 {
    round_start + period.as_seconds()
}

pub async fn fetch_market(
    client: &reqwest::Client,
    coin: Coin,
    period: Period,
    round_start: i64,
) -> Result<Market> {
    let slug = generate_slug(coin, period, round_start);

    // Try with original slug first
    let url = format!("{}/events?slug={}", GAMMA_API_BASE, slug);
    debug!(url = %url, "Calling Gamma API for market discovery");

    let response = client
        .get(&url)
        .send()
        .await
        .context("Failed to fetch event from Gamma API")?;

    let status = response.status();
    if !status.is_success() {
        let response_body = response.text().await.unwrap_or_else(|_| "Failed to read response body".to_string());
        warn!(
            url = %url,
            status = %status,
            response_body = %response_body,
            "Gamma API returned error status"
        );
        anyhow::bail!("Gamma API returned status: {}", status);
    }

    let response_body = response.text().await.context("Failed to read response body")?;
    debug!(url = %url, response_body = %response_body, "Gamma API response received");

    let events: Vec<GammaEventResponse> = serde_json::from_str(&response_body)
        .map_err(|e| {
            error!("Parse error for {}: {}", slug, e);
            anyhow::anyhow!("Failed to parse Gamma API response: {}", e)
        })?;

    // If empty, try with lowercased slug as fallback
    let events = if events.is_empty() {
        let lowercased_slug = slug.to_lowercase();
        let fallback_url = format!("{}/events?slug={}", GAMMA_API_BASE, lowercased_slug);
        debug!(url = %fallback_url, "Trying fallback with lowercased slug");

        let fallback_response = client
            .get(&fallback_url)
            .send()
            .await
            .context("Failed to fetch event from Gamma API (fallback)")?;

        let fallback_status = fallback_response.status();
        if !fallback_status.is_success() {
            let fallback_body = fallback_response.text().await.unwrap_or_else(|_| "Failed to read response body".to_string());
            warn!(
                url = %fallback_url,
                status = %fallback_status,
                response_body = %fallback_body,
                "Gamma API fallback returned error status"
            );
            anyhow::bail!("Gamma API fallback returned status: {}", fallback_status);
        }

        let fallback_body = fallback_response.text().await.context("Failed to read response body (fallback)")?;
        debug!(url = %fallback_url, response_body = %fallback_body, "Gamma API fallback response received");

        serde_json::from_str(&fallback_body)
            .map_err(|e| {
                error!("Parse error for {}: {}", slug, e);
                anyhow::anyhow!("Failed to parse Gamma API fallback response: {}", e)
            })?
    } else {
        events
    };

    if events.is_empty() {
        warn!(
            slug = %slug,
            "Gamma API returned empty results for both original and lowercased slug"
        );
        anyhow::bail!("No event found for slug: {}", slug);
    }

    let event = events
        .first()
        .expect("events should not be empty after check");

    if event.markets.is_empty() {
        warn!(
            slug = %slug,
            event_id = %event.id,
            "Event has no markets"
        );
        anyhow::bail!("Event {} has no markets", event.id);
    }

    let market_data = event.markets
        .first()
        .expect("markets should not be empty after check");

    // Parse JSON strings to get arrays
    let clob_token_ids: Vec<String> = serde_json::from_str(&market_data.clob_token_ids)
        .map_err(|e| {
            error!("Parse error for {}: {}", slug, e);
            anyhow::anyhow!("Failed to parse clobTokenIds as JSON array: {}", e)
        })?;
    
    let outcomes: Vec<String> = serde_json::from_str(&market_data.outcomes)
        .map_err(|e| {
            error!("Parse error for {}: {}", slug, e);
            anyhow::anyhow!("Failed to parse outcomes as JSON array: {}", e)
        })?;
    
    let _outcome_prices: Vec<String> = serde_json::from_str(&market_data.outcome_prices)
        .map_err(|e| {
            error!("Parse error for {}: {}", slug, e);
            anyhow::anyhow!("Failed to parse outcomePrices as JSON array: {}", e)
        })?;

    // Match outcomes to token IDs by index
    if outcomes.len() != clob_token_ids.len() {
        anyhow::bail!(
            "Mismatch: outcomes length ({}) != clobTokenIds length ({})",
            outcomes.len(),
            clob_token_ids.len()
        );
    }

    // Find Up and Down token IDs by matching outcomes
    let up_token_id = outcomes
        .iter()
        .position(|o| o == "Up")
        .and_then(|idx| clob_token_ids.get(idx))
        .context("No Up token found")?
        .clone();

    let down_token_id = outcomes
        .iter()
        .position(|o| o == "Down")
        .and_then(|idx| clob_token_ids.get(idx))
        .context("No Down token found")?
        .clone();

    // DEBUG: Log token mapping
    info!(
        "Token mapping: outcomes={:?}, clobTokenIds={:?}, up_token_id={}, down_token_id={}",
        outcomes, clob_token_ids, up_token_id, down_token_id
    );

    // Parse minimum tick size - construct from string to avoid f64 floating point artifacts
    // All our crypto up/down markets use tick_size 0.01, so hardcode for safety
    let minimum_tick_size = Decimal::from_str("0.01")
        .unwrap_or_else(|_| dec!(0.01)); // Fallback to macro if string parse fails

    // Parse minimum order size - use orderMinSize if available, otherwise default to 1.0
    let minimum_order_size = market_data
        .order_min_size
        .and_then(|f| Decimal::from_f64_retain(f))
        .unwrap_or_else(|| dec!(1.0));

    // Parse best bid/ask from Gamma API - these are for the market overall
    // We'll need to determine which applies to up vs down tokens
    // For now, store them as up token prices (can be refined later if needed)
    let up_best_bid = market_data
        .best_bid
        .and_then(|f| Decimal::from_f64_retain(f));

    let up_best_ask = market_data
        .best_ask
        .and_then(|f| Decimal::from_f64_retain(f));

    // For down token, if we have best_bid/best_ask, we can derive:
    // down_best_bid = 1 - up_best_ask (if up_best_ask exists)
    // down_best_ask = 1 - up_best_bid (if up_best_bid exists)
    let down_best_bid = up_best_ask.map(|ask| Decimal::from(1) - ask);
    let down_best_ask = up_best_bid.map(|bid| Decimal::from(1) - bid);

    let _last_trade_price = market_data
        .last_trade_price
        .and_then(|f| Decimal::from_f64_retain(f));

    let _fees_enabled = market_data.fees_enabled;
    let _accepting_orders = market_data.accepting_orders;

    // Handle optional fee fields - use defaults if not present
    // Convert u64 directly to Decimal
    let taker_base_fee = market_data
        .taker_base_fee
        .map(Decimal::from)
        .unwrap_or(dec!(0.0));

    let maker_base_fee = market_data
        .maker_base_fee
        .map(Decimal::from)
        .unwrap_or(dec!(0.0));

    let round_end = calculate_round_end(period, round_start);

    let opening_price = market_data
        .opening_price
        .and_then(|f| Decimal::from_f64_retain(f))
        .or_else(|| gamma_opening_price_from_extra(&market_data.extra));

    Ok(Market {
        condition_id: market_data.condition_id.clone(),
        slug: event.slug.clone(),
        coin,
        period,
        round_start,
        round_end,
        up_token_id,
        down_token_id,
        minimum_tick_size,
        minimum_order_size,
        neg_risk: market_data.neg_risk,
        taker_base_fee,
        maker_base_fee,
        up_best_bid,
        up_best_ask,
        down_best_bid,
        down_best_ask,
        opening_price,
    })
}

#[allow(dead_code)]
pub async fn discover_active_rounds(
    coins: &[Coin],
    periods: &[Period],
) -> Result<Vec<(Coin, Period, i64)>> {
    let now = Utc::now().timestamp();
    let mut rounds = Vec::new();

    for coin in coins {
        for period in periods {
            let round_start = calculate_round_start(*period, now);
            rounds.push((*coin, *period, round_start));
        }
    }

    Ok(rounds)
}

#[allow(dead_code)]
pub async fn pre_fetch_next_round(
    client: &reqwest::Client,
    coin: Coin,
    period: Period,
    current_round_start: i64,
) -> Result<Market> {
    let next_round_start = current_round_start + period.as_seconds();
    fetch_market(client, coin, period, next_round_start).await
}

/// Parsed Gamma `/markets?slug=…` row: resolution state for winner inference.
#[derive(Debug, Clone)]
pub struct GammaMarketResolution {
    pub closed: bool,
    /// `true` = outcome index 0 (Up / YES) won; `false` = index 1 (Down / NO) won.
    pub up_won: Option<bool>,
}

fn gamma_outcome_prices_as_f64(m: &serde_json::Value) -> Vec<f64> {
    let raw: Option<Vec<serde_json::Value>> = match m.get("outcomePrices") {
        Some(serde_json::Value::String(s)) => serde_json::from_str(s).ok(),
        Some(serde_json::Value::Array(a)) => Some(a.clone()),
        _ => None,
    };
    raw.unwrap_or_default()
        .iter()
        .filter_map(|v| match v {
            serde_json::Value::String(s) => s.parse::<f64>().ok(),
            serde_json::Value::Number(n) => n.as_f64(),
            _ => None,
        })
        .collect()
}

fn gamma_outcome_labels_as_strings(m: &serde_json::Value) -> Vec<String> {
    let raw: Option<Vec<serde_json::Value>> = match m.get("outcomes") {
        Some(serde_json::Value::String(s)) => serde_json::from_str(s).ok(),
        Some(serde_json::Value::Array(a)) => Some(a.clone()),
        _ => None,
    };
    raw.unwrap_or_default()
        .iter()
        .filter_map(|v| match v {
            serde_json::Value::String(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// Infer winner from resolved outcome prices (long decimals, not exactly `"1"`).
/// Pairs each price with the corresponding `outcomes` label when present so we do not assume
/// index 0 is always "Up" (Gamma order can differ by market).
/// Returns `None` if resolution data is not ready yet or ambiguous.
fn infer_up_won_from_prices_and_labels(prices: &[f64], labels: &[String]) -> Option<bool> {
    if prices.len() < 2 {
        return None;
    }
    let (up_i, down_i) = if labels.len() >= 2 && labels.len() == prices.len() {
        let mut up_idx = None;
        let mut down_idx = None;
        for (i, label) in labels.iter().enumerate() {
            let l = label.to_lowercase();
            if l.contains("up") || l == "yes" {
                up_idx = Some(i);
            }
            if l.contains("down") || l == "no" {
                down_idx = Some(i);
            }
        }
        match (up_idx, down_idx) {
            (Some(u), Some(d)) => (u, d),
            _ => (0, 1),
        }
    } else {
        (0, 1)
    };
    let p_up = *prices.get(up_i)?;
    let p_down = *prices.get(down_i)?;
    if p_up == 0.0 && p_down == 0.0 {
        return None;
    }
    // Strict band: same outcome Polymarket shows when a side is effectively settled (claimable).
    // `closed` in JSON often lags these prices — we finalize on prices alone so you see WON/LOST ~within 1m.
    if p_up > 0.95 && p_down < 0.05 {
        return Some(true);
    }
    if p_down > 0.95 && p_up < 0.05 {
        return Some(false);
    }
    None
}

/// GET `https://gamma-api.polymarket.com/markets?slug={slug}` — uses `outcomePrices` and `closed`.
/// Slug uniquely identifies the crypto UP/DOWN market; `condition_id` queries return unrelated rows.
pub async fn fetch_market_resolution_by_slug(
    client: &reqwest::Client,
    slug: &str,
) -> Result<Option<GammaMarketResolution>> {
    let url = format!("{}/markets", GAMMA_API_BASE);
    let response = client
        .get(&url)
        .query(&[("slug", slug)])
        .send()
        .await
        .context("Gamma markets fetch failed")?;

    if !response.status().is_success() {
        warn!(
            status = %response.status(),
            slug = %slug,
            "Gamma markets resolution fetch non-success"
        );
        return Ok(None);
    }

    let v: serde_json::Value = response
        .json()
        .await
        .context("Gamma markets JSON parse failed")?;

    let arr: &[serde_json::Value] = match &v {
        serde_json::Value::Array(a) => a.as_slice(),
        _ => &[],
    };
    let m = match arr.first() {
        Some(x) => x,
        None => return Ok(None),
    };

    let closed = m.get("closed").and_then(|x| x.as_bool()).unwrap_or(false);

    let outcome_prices_raw = m.get("outcomePrices");
    let outcomes_raw = m.get("outcomes");
    info!(
        "Gamma raw response: slug={}, closed={}, outcomePrices={:?}, outcomes={:?}",
        slug, closed, outcome_prices_raw, outcomes_raw
    );

    let prices = gamma_outcome_prices_as_f64(m);
    let labels = gamma_outcome_labels_as_strings(m);
    let up_won = infer_up_won_from_prices_and_labels(&prices, &labels);

    Ok(Some(GammaMarketResolution { closed, up_won }))
}

/// Chainlink-style resolution: UP / DOWN / tie (no movement). Gamma only gives a binary winner.
fn apply_chainlink_outcome_to_open_round_history(
    round_history: &Arc<Mutex<Vec<RoundHistoryEntry>>>,
    condition_id: &str,
    round_start: i64,
    outcome: ChainlinkOutcome,
) {
    let mut g = match round_history.lock() {
        Ok(g) => g,
        Err(e) => {
            error!(error = %e, "round_history mutex poisoned; skip resolution P&L update");
            return;
        }
    };
    for e in g.iter_mut() {
        if e.condition_id != condition_id || e.round_start != round_start {
            continue;
        }
        if e.status != RoundHistoryStatus::Open {
            continue;
        }
        let won = match outcome {
            ChainlinkOutcome::UpWon => e.side_label == "UP",
            ChainlinkOutcome::DownWon => e.side_label == "DOWN",
            ChainlinkOutcome::Tie => false,
        };
        // Explicit realized P&L for unhedged legs (must match TUI session sum).
        let pnl = if won {
            e.pnl_if_resolved_won()
        } else {
            e.pnl_if_resolved_lost()
        };
        e.pnl = Some(pnl);
        e.status = if won {
            RoundHistoryStatus::Won
        } else {
            RoundHistoryStatus::Lost
        };
        info!(
            "Round resolved: coin={}, side={}, outcome={:?}, status={:?}, pnl={:?}",
            e.coin.as_str(),
            e.side_label,
            outcome,
            e.status,
            e.pnl,
        );
    }
}

/// Apply settlement outcome, persist telemetry, Telegram notifications, and drop Chainlink round state.
fn finalize_open_round_resolution(
    outcome: ChainlinkOutcome,
    tracker: Option<&Arc<ChainlinkTracker>>,
    round_history: &Arc<Mutex<Vec<RoundHistoryEntry>>>,
    coin: Coin,
    period: Period,
    round_start: i64,
    condition_id: &str,
    slug: &str,
    strategy_logger: &Option<Arc<Mutex<StrategyLogger>>>,
    spot_feeds: &Option<Arc<HashMap<Coin, watch::Receiver<SpotState>>>>,
    telegram: &Arc<TelegramBot>,
    source: &'static str,
) {
    let prices_opt = tracker.and_then(|t| t.settlement_prices_for_round(coin, period, round_start));
    let chainlink_close_f = prices_opt.as_ref().and_then(|(_, c)| c.to_f64()).unwrap_or(0.0);
    let chainlink_open_f = prices_opt
        .as_ref()
        .and_then(|(o, _)| o.to_f64())
        .unwrap_or(0.0);
    if prices_opt.is_none() && source == "chainlink" {
        warn!(
            coin = ?coin,
            period = ?period,
            round_start,
            "strategy_log: settlement_prices_for_round missing after Chainlink resolve"
        );
    }

    apply_chainlink_outcome_to_open_round_history(round_history, condition_id, round_start, outcome);

    if let Ok(rh) = round_history.lock() {
        for e in rh.iter() {
            if e.condition_id != condition_id || e.round_start != round_start {
                continue;
            }
            if e.status != RoundHistoryStatus::Won
                && e.status != RoundHistoryStatus::Lost
                && e.status != RoundHistoryStatus::Hedged
            {
                continue;
            }
            let position_outcome = if e.status == RoundHistoryStatus::Hedged {
                "HEDGED"
            } else if matches!(outcome, ChainlinkOutcome::Tie) {
                "TIE"
            } else {
                let matches_winning_side = match outcome {
                    ChainlinkOutcome::UpWon => e.side_label == "UP",
                    ChainlinkOutcome::DownWon => e.side_label == "DOWN",
                    ChainlinkOutcome::Tie => unreachable!(),
                };
                if matches_winning_side {
                    "WON"
                } else {
                    "LOST"
                }
            };
            let pnl_f = e.pnl.and_then(|d| d.to_f64()).unwrap_or(0.0);
            if let (Some(sl), Some(spot_arc)) = (strategy_logger, spot_feeds) {
                let binance_spot_close = spot_arc
                    .get(&coin)
                    .map(|r| r.borrow().price)
                    .and_then(|d| d.to_f64());
                if let Ok(guard) = sl.lock() {
                    if let Err(err) = guard.log_resolution(
                        e.coin.as_str(),
                        &format!("{}m", e.period.as_minutes()),
                        e.round_start,
                        &e.side_label,
                        chainlink_close_f,
                        position_outcome,
                        pnl_f,
                        binance_spot_close,
                    ) {
                        warn!(error = %err, "strategy_log log_resolution failed");
                    }
                }
            }
            let entry_pf = e.entry_price.to_f64().unwrap_or(0.0);
            let time_left_u = (e.round_end - e.fill_time.timestamp()).max(0) as u64;
            telegram.send_round_resolution(
                e.coin.as_str(),
                &format!("{}m", e.period.as_minutes()),
                &e.side_label,
                entry_pf,
                position_outcome,
                pnl_f,
                chainlink_open_f,
                chainlink_close_f,
                time_left_u,
            );
        }
    }
    if let Some(t) = tracker {
        t.remove_round(coin, period, round_start);
    }
    info!(
        condition_id = %condition_id,
        slug = %slug,
        round_start,
        ?outcome,
        source,
        "Round history: OPEN rows settled (notifications sent)"
    );
}

/// Fixed **3s** between Gamma polls from `round_end` until resolved (simple backoff).
fn gamma_poll_interval_secs(_round_start: i64, _period: Period) -> u64 {
    3
}

/// Settle OPEN rows from **Gamma only** (same source as the app: `outcomePrices` + `closed`).
/// Finalizes as soon as [`infer_up_won_from_prices_and_labels`] returns `Some` — often **before** `closed=true`.
/// [`gamma_poll_interval_secs`] polls every **3s** after `round_end` until outcome prices are decisive.
pub async fn resolve_round_history_open_entries_chainlink_or_gamma(
    tracker: Option<Arc<ChainlinkTracker>>,
    client: &reqwest::Client,
    round_history: &Arc<Mutex<Vec<RoundHistoryEntry>>>,
    coin: Coin,
    period: Period,
    condition_id: &str,
    slug: &str,
    round_start: i64,
    shutdown: CancellationToken,
    strategy_logger: Option<Arc<Mutex<StrategyLogger>>>,
    spot_feeds: Option<Arc<HashMap<Coin, watch::Receiver<SpotState>>>>,
    telegram: Arc<TelegramBot>,
) {
    let has_open = round_history
        .lock()
        .map(|g| {
            g.iter().any(|e| {
                e.condition_id == condition_id
                    && e.round_start == round_start
                    && e.status == RoundHistoryStatus::Open
            })
        })
        .unwrap_or(false);
    if !has_open {
        info!(
            condition_id = %condition_id,
            slug = %slug,
            round_start,
            "resolution: no OPEN entries, skipping"
        );
        return;
    }

    match fetch_market_resolution_by_slug(client, slug).await {
        Ok(Some(gamma_res)) => {
            if let Some(up_won) = gamma_res.up_won {
                info!(
                    slug = %slug,
                    closed = gamma_res.closed,
                    up_won,
                    "Gamma: decisive outcomePrices (finalize; closed flag may still lag)"
                );
                let outcome = if up_won {
                    ChainlinkOutcome::UpWon
                } else {
                    ChainlinkOutcome::DownWon
                };
                finalize_open_round_resolution(
                    outcome,
                    tracker.as_ref(),
                    round_history,
                    coin,
                    period,
                    round_start,
                    condition_id,
                    slug,
                    &strategy_logger,
                    &spot_feeds,
                    &telegram,
                    "gamma",
                );
                return;
            }
            resolve_round_history_open_entries_gamma(
                client,
                round_history,
                condition_id,
                slug,
                round_start,
                shutdown,
                tracker,
                coin,
                period,
                strategy_logger,
                spot_feeds,
                telegram,
            )
            .await;
            return;
        }
        Err(e) => {
            warn!(
                error = %e,
                slug = %slug,
                "Gamma resolution fetch failed; polling Gamma"
            );
        }
        Ok(None) => {}
    }

    resolve_round_history_open_entries_gamma(
        client,
        round_history,
        condition_id,
        slug,
        round_start,
        shutdown,
        tracker,
        coin,
        period,
        strategy_logger,
        spot_feeds,
        telegram,
    )
    .await;
}

/// Poll Gamma until outcome prices are decisive (see [`infer_up_won_from_prices_and_labels`]).
/// Interval: **3s** (see [`gamma_poll_interval_secs`]).
/// Stops on `shutdown` or once all matching OPEN rows are settled.
pub async fn resolve_round_history_open_entries_gamma(
    client: &reqwest::Client,
    round_history: &Arc<Mutex<Vec<RoundHistoryEntry>>>,
    condition_id: &str,
    slug: &str,
    round_start: i64,
    shutdown: CancellationToken,
    tracker: Option<Arc<ChainlinkTracker>>,
    coin: Coin,
    period: Period,
    strategy_logger: Option<Arc<Mutex<StrategyLogger>>>,
    spot_feeds: Option<Arc<HashMap<Coin, watch::Receiver<SpotState>>>>,
    telegram: Arc<TelegramBot>,
) {
    info!(
        condition_id = %condition_id,
        slug = %slug,
        round_start,
        "Gamma resolution task started"
    );
    let has_open = round_history
        .lock()
        .map(|g| {
            g.iter()
                .any(|e| {
                    e.condition_id == condition_id
                        && e.round_start == round_start
                        && e.status == RoundHistoryStatus::Open
                })
        })
        .unwrap_or(false);
    if !has_open {
        info!(
            condition_id = %condition_id,
            slug = %slug,
            round_start,
            "Gamma resolution: no OPEN entries, skipping"
        );
        return;
    }

    loop {
        if shutdown.is_cancelled() {
            return;
        }

        match fetch_market_resolution_by_slug(client, slug).await {
            Ok(Some(res)) => {
                info!(
                    condition_id = %condition_id,
                    slug = %slug,
                    closed = res.closed,
                    outcome = ?res.up_won,
                    "Gamma resolution result"
                );
                if let Some(up_won) = res.up_won {
                    let outcome = if up_won {
                        ChainlinkOutcome::UpWon
                    } else {
                        ChainlinkOutcome::DownWon
                    };
                    finalize_open_round_resolution(
                        outcome,
                        tracker.as_ref(),
                        round_history,
                        coin,
                        period,
                        round_start,
                        condition_id,
                        slug,
                        &strategy_logger,
                        &spot_feeds,
                        &telegram,
                        "gamma_poll",
                    );
                    return;
                }
                debug!(
                    condition_id = %condition_id,
                    slug = %slug,
                    closed = res.closed,
                    "Gamma market not fully resolved yet"
                );
            }
            Ok(None) => {
                debug!(
                    condition_id = %condition_id,
                    slug = %slug,
                    "Gamma markets: empty response"
                );
            }
            Err(e) => {
                warn!(
                    error = %e,
                    condition_id = %condition_id,
                    slug = %slug,
                    "Gamma resolution fetch error"
                );
            }
        }

        let wait = gamma_poll_interval_secs(round_start, period);
        debug!(
            condition_id = %condition_id,
            slug = %slug,
            wait_secs = wait,
            "Gamma resolution: sleep before next poll"
        );
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = sleep(Duration::from_secs(wait)) => {}
        }
    }
}
