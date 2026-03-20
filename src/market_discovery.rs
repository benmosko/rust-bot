use crate::types::{Coin, Market, Period};
use anyhow::{Context, Result};
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const GAMMA_API_BASE: &str = "https://gamma-api.polymarket.com";

#[derive(Debug, Deserialize)]
struct GammaMarketResponse {
    #[serde(rename = "conditionID")]
    condition_id: String,
    slug: String,
    tokens: Vec<GammaToken>,
    #[serde(rename = "negRisk")]
    neg_risk: bool,
    #[serde(rename = "minimumTickSize")]
    minimum_tick_size: String,
    #[serde(rename = "takerBaseFee")]
    taker_base_fee: String,
    #[serde(rename = "makerBaseFee")]
    maker_base_fee: String,
}

#[derive(Debug, Deserialize)]
struct GammaToken {
    #[serde(rename = "tokenID")]
    token_id: String,
    outcome: String,
    price: Option<String>,
}

pub fn generate_slug(coin: Coin, period: Period, round_start: i64) -> String {
    format!(
        "{}-updown-{}m-{}",
        coin.as_str(),
        period.as_minutes(),
        round_start
    )
}

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
    let url = format!("{}/markets?slug={}", GAMMA_API_BASE, slug);

    let client = reqwest::Client::new();
    let response = client
        .get(&url)
        .send()
        .await
        .context("Failed to fetch market from Gamma API")?;

    if !response.status().is_success() {
        anyhow::bail!("Gamma API returned status: {}", response.status());
    }

    let markets: Vec<GammaMarketResponse> = response
        .json()
        .await
        .context("Failed to parse Gamma API response")?;

    let market_data = markets
        .first()
        .context(format!("No market found for slug: {}", slug))?;

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

    let taker_base_fee = market_data
        .taker_base_fee
        .parse::<Decimal>()
        .context("Failed to parse taker_base_fee")?;

    let maker_base_fee = market_data
        .maker_base_fee
        .parse::<Decimal>()
        .context("Failed to parse maker_base_fee")?;

    // Minimum order size is typically 1 USDC
    let minimum_order_size = dec!(1.0);

    let round_end = calculate_round_end(period, round_start);

    Ok(Market {
        condition_id: market_data.condition_id.clone(),
        slug: market_data.slug.clone(),
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

pub async fn pre_fetch_next_round(
    coin: Coin,
    period: Period,
    current_round_start: i64,
) -> Result<Market> {
    let next_round_start = current_round_start + period.as_seconds();
    fetch_market(coin, period, next_round_start).await
}
