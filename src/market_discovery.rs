use crate::types::{Coin, Market, Period};
use anyhow::{Context, Result};
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Deserialize;
use tracing::{debug, warn};

const GAMMA_API_BASE: &str = "https://gamma-api.polymarket.com";

#[derive(Debug, Deserialize)]
struct GammaEventResponse {
    id: String,
    slug: String,
    markets: Vec<GammaMarketResponse>,
}

#[derive(Debug, Deserialize)]
struct GammaMarketResponse {
    #[serde(rename = "conditionId")]
    condition_id: String,
    tokens: Vec<GammaToken>,
    neg_risk: bool,
    minimum_tick_size: String,
    #[serde(rename = "takerBaseFee")]
    taker_base_fee: Option<String>,
    #[serde(rename = "makerBaseFee")]
    maker_base_fee: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GammaToken {
    #[serde(rename = "token_id")]
    token_id: String,
    outcome: String,
    #[allow(dead_code)]
    price: Option<f64>,
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
        .context("Failed to parse Gamma API response")?;

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
            .context("Failed to parse Gamma API fallback response")?
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

    let up_token = market_data
        .tokens
        .iter()
        .find(|t| t.outcome == "Up")
        .context("No Up token found")?;

    let down_token = market_data
        .tokens
        .iter()
        .find(|t| t.outcome == "Down")
        .context("No Down token found")?;

    let minimum_tick_size = market_data
        .minimum_tick_size
        .parse::<Decimal>()
        .context("Failed to parse minimum_tick_size")?;

    // Handle optional fee fields - use defaults if not present
    let taker_base_fee = market_data
        .taker_base_fee
        .as_ref()
        .map(|s| s.parse::<Decimal>())
        .transpose()
        .context("Failed to parse taker_base_fee")?
        .unwrap_or(dec!(0.0));

    let maker_base_fee = market_data
        .maker_base_fee
        .as_ref()
        .map(|s| s.parse::<Decimal>())
        .transpose()
        .context("Failed to parse maker_base_fee")?
        .unwrap_or(dec!(0.0));

    // Minimum order size is typically 1 USDC
    let minimum_order_size = dec!(1.0);

    let round_end = calculate_round_end(period, round_start);

    Ok(Market {
        condition_id: market_data.condition_id.clone(),
        slug: event.slug.clone(),
        coin,
        period,
        round_start,
        round_end,
        up_token_id: up_token.token_id.clone(),
        down_token_id: down_token.token_id.clone(),
        minimum_tick_size,
        minimum_order_size,
        neg_risk: market_data.neg_risk,
        taker_base_fee,
        maker_base_fee,
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
