use crate::types::{Coin, Market, Period};
use anyhow::{Context, Result};
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Deserialize;
use serde_json;
use std::collections::HashMap;
use std::str::FromStr;
use tracing::{debug, error, warn};

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
            extra: HashMap::new(),
        }
    }
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
    coin: Coin,
    period: Period,
    round_start: i64,
) -> Result<Market> {
    let slug = generate_slug(coin, period, round_start);
    let client = reqwest::Client::new();

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
    coin: Coin,
    period: Period,
    current_round_start: i64,
) -> Result<Market> {
    let next_round_start = current_round_start + period.as_seconds();
    fetch_market(coin, period, next_round_start).await
}
