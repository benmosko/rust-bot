use anyhow::{Context, Result};
use chrono::Utc;
use polymarket_client_sdk::clob::types::Side;
use rust_decimal_macros::dec;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::time::{sleep, Duration};

// Import modules from the main crate
use polymarket_bot::config::Config;
use polymarket_bot::execution::ExecutionEngine;
use polymarket_bot::market_discovery::{calculate_round_start, fetch_market};
use polymarket_bot::types::{Coin, Period};

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

fn format_address(address: &str) -> String {
    if address.len() >= 10 {
        format!("{}...{}", &address[..6], &address[address.len()-4..])
    } else {
        address.to_string()
    }
}

async fn run() -> Result<()> {
    // Initialize rustls crypto provider
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|e| anyhow::anyhow!("Failed to install rustls crypto provider: {:?}", e))?;
    
    // Step 1: Load config from .env
    println!("[1/6] Loading config from .env...");
    let config = Config::from_env().context("Failed to load config from .env")?;
    
    let proxy_display = config.funder_address.as_deref()
        .map(|addr| format_address(addr))
        .unwrap_or_else(|| "none".to_string());
    println!("[1/6] Config loaded: proxy={}", proxy_display);
    
    // Step 2: Initialize the CLOB client
    println!("[2/6] Initializing CLOB client...");
    // Force dry_run = false for this test binary - we need to place REAL orders to test signing
    let dry_run = Arc::new(AtomicBool::new(false));
    let execution = ExecutionEngine::new(
        &config.private_key,
        config.signature_type,
        config.funder_address.clone(),
        dry_run.clone(),
    )
    .await
    .context("Failed to initialize CLOB client")?;
    
    println!("[2/6] CLOB client initialized");
    
    // Step 3: Discover the current BTC 5m round via Gamma API
    println!("[3/6] Discovering BTC 5m market via Gamma API...");
    let coin = Coin::Btc;
    let period = Period::Five;
    let now = Utc::now().timestamp();
    let round_start = calculate_round_start(period, now);
    
    let market = fetch_market(coin, period, round_start)
        .await
        .context("Failed to discover BTC 5m market")?;
    
    println!(
        "[3/6] Discovered BTC 5m market: slug={}, YES token={}",
        market.slug, market.up_token_id
    );
    
    // Step 4: Place one maker BUY order for YES at $0.01 for 5 shares
    println!("[4/6] Placing order: BUY YES @ $0.01 x 5 shares...");
    let price = dec!(0.01);
    let size = dec!(5);
    
    let order_id = execution
        .place_order(&market.up_token_id, Side::Buy, price, size, market.minimum_tick_size)
        .await
        .context("Failed to place order")?;
    
    println!("[4/6] Order placed! id={}", order_id);
    
    // Step 5: Wait 3 seconds
    println!("[5/6] Waiting 3 seconds...");
    sleep(Duration::from_secs(3)).await;
    
    // Step 6: Cancel the order
    println!("[6/6] Cancelling order...");
    execution
        .cancel_order(&order_id)
        .await
        .context("Failed to cancel order")?;
    
    println!("[6/6] Order cancelled successfully!");
    
    Ok(())
}
