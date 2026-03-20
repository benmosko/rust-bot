//! All SDK order operations behind a single Mutex for nonce safety.
//! Fee rate is queried per market (SDK embeds it in EIP-712 signature).

use anyhow::{Context, Result};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::LocalSigner as AlloyLocalSigner;
use alloy::signers::Signer as AlloySigner;
use chrono::Utc;
use dashmap::DashMap;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::clob::types::{OrderType, Side, SignatureType};
use polymarket_client_sdk::clob::{Client, Config};
use polymarket_client_sdk::types::{Address, U256};
use polymarket_client_sdk::POLYGON;
use polymarket_client_sdk::auth::LocalSigner;
use rust_decimal::Decimal;
use std::str::FromStr as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;
use tracing::info;

use crate::types::FeeRate;

/// Calculate the number of decimal places from a tick_size.
/// For example, 0.01 → 2, 0.001 → 3, 0.1 → 1
fn decimal_places_from_tick_size(tick_size: Decimal) -> u32 {
    tick_size.to_string().split('.').nth(1).map(|s| s.len()).unwrap_or(2) as u32
}

/// Round price to tick_size precision and size to 2 decimal places.
/// Price: 0.01→2dp, 0.001→3dp, 0.0001→4dp (based on tick_size)
/// Size: always 2 decimal places (e.g., 8.16 shares is valid)
fn round_price_and_size(price: Decimal, tick_size: Decimal, size: Decimal) -> (Decimal, Decimal) {
    // Round price to tick_size precision (0.01→2dp, 0.001→3dp, 0.0001→4dp)
    let price_decimals = decimal_places_from_tick_size(tick_size);
    let rounded_price = price.round_dp(price_decimals);
    
    // Round size to 2 decimal places (always 2dp for size)
    let rounded_size = size.round_dp(2);
    
    (rounded_price, rounded_size)
}

const CLOB_API_BASE: &str = "https://clob.polymarket.com";

type AuthenticatedClient = Client<Authenticated<Normal>>;

/// Concrete signer type from alloy (secp256k1).
type ConcreteSigner = LocalSigner<ecdsa::SigningKey<k256::Secp256k1>>;

pub struct ExecutionEngine {
    client: Arc<Mutex<AuthenticatedClient>>,
    signer: Arc<ConcreteSigner>,
    #[allow(dead_code)]
    private_key: String, // Store private key to create alloy signer for on-chain transactions
    signature_type: u8,
    #[allow(dead_code)]
    fee_rates: Arc<DashMap<String, FeeRate>>,
    #[allow(dead_code)]
    http_client: reqwest::Client,
    dry_run: Arc<AtomicBool>,
}

impl ExecutionEngine {
    pub async fn new(
        private_key: &str,
        signature_type: u8,
        funder_address: Option<String>,
        dry_run: Arc<AtomicBool>,
    ) -> Result<Self> {
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key")?;
        let signer = signer.with_chain_id(Some(POLYGON));
        let signer = Arc::new(signer);

        let mut auth_builder =
            Client::new(CLOB_API_BASE, Config::default()).context("Failed to create CLOB client")?
                .authentication_builder(signer.as_ref());

        if signature_type == 2 {
            auth_builder = auth_builder.signature_type(SignatureType::GnosisSafe);
            if let Some(ref addr) = funder_address {
                let funder = Address::from_str(addr.trim()).context("Invalid FUNDER_ADDRESS")?;
                auth_builder = auth_builder.funder(funder);
            }
        }

        let authenticated_client = auth_builder
            .authenticate()
            .await
            .context("Failed to authenticate CLOB client")?;

        Ok(Self {
            client: Arc::new(Mutex::new(authenticated_client)),
            signer,
            private_key: private_key.to_string(),
            signature_type,
            fee_rates: Arc::new(DashMap::new()),
            http_client: reqwest::Client::new(),
            dry_run,
        })
    }

    /// Query fee rate for token (used for logging/cache). SDK fetches it automatically in order build.
    #[allow(dead_code)]
    pub async fn get_fee_rate(&self, token_id: &str) -> Result<u64> {
        if let Some(fee_rate) = self.fee_rates.get(token_id) {
            if !fee_rate.is_stale(60) {
                return Ok(fee_rate.fee_rate_bps);
            }
        }

        let url = format!("{}/fee-rate?tokenID={}", CLOB_API_BASE, token_id);
        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch fee rate")?;

        if !response.status().is_success() {
            anyhow::bail!("Fee rate API returned status: {}", response.status());
        }

        #[derive(serde::Deserialize)]
        struct FeeRateResponse {
            #[serde(rename = "feeRateBps")]
            fee_rate_bps: u64,
        }

        let fee_data: FeeRateResponse = response
            .json()
            .await
            .context("Failed to parse fee rate response")?;

        self.fee_rates.insert(
            token_id.to_string(),
            FeeRate {
                token_id: token_id.to_string(),
                fee_rate_bps: fee_data.fee_rate_bps,
                fetched_at: Utc::now(),
            },
        );

        Ok(fee_data.fee_rate_bps)
    }

    /// Place a maker limit order. All order ops go through this Mutex for nonce safety.
    pub async fn place_order(
        &self,
        token_id: &str,
        side: Side,
        price: Decimal,
        size: Decimal,
        tick_size: Decimal,
    ) -> Result<String> {
        // Round price to tick_size precision and size to 2 decimal places
        let (rounded_price, rounded_size) = round_price_and_size(price, tick_size, size);

        if self.dry_run.load(Ordering::Relaxed) {
            info!(
                side = ?side,
                price = %rounded_price,
                original_price = %price,
                size = %rounded_size,
                original_size = %size,
                market = %token_id,
                "DRY RUN: would place order"
            );
            // Return a fake order ID
            return Ok(format!("dry_run_order_{}", Utc::now().timestamp_millis()));
        }

        let token_id_u256 = U256::from_str(token_id).context("Invalid token_id for order")?;

        let size_sdk = polymarket_client_sdk::types::Decimal::from_str_exact(rounded_size.to_string().as_str())
            .context("Invalid size")?;

        let client = self.client.lock().await;

        let order = client
            .limit_order()
            .token_id(token_id_u256)
            .size(size_sdk)
            .price(polymarket_client_sdk::types::Decimal::from_str_exact(rounded_price.to_string().as_str()).context("Invalid price")?)
            .side(side)
            .order_type(OrderType::GTC)
            .build()
            .await
            .context("Failed to build order")?;

        let signed_order = client
            .sign(self.signer.as_ref(), order)
            .await
            .context("Failed to sign order")?;

        let response = client
            .post_order(signed_order)
            .await
            .context("Failed to post order")?;

        let order_id = response.order_id.clone();

        info!(
            token_id = %token_id,
            side = ?side,
            price = %rounded_price,
            original_price = %price,
            size = %rounded_size,
            original_size = %size,
            order_id = %order_id,
            "Order placed"
        );

        Ok(order_id)
    }

    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        if self.dry_run.load(Ordering::Relaxed) {
            info!(order_id = %order_id, "DRY RUN: would cancel order");
            return Ok(());
        }

        let client = self.client.lock().await;
        client
            .cancel_order(order_id)
            .await
            .context("Failed to cancel order")?;

        info!(order_id = %order_id, "Order cancelled");
        Ok(())
    }

    pub async fn cancel_all(&self) -> Result<()> {
        if self.dry_run.load(Ordering::Relaxed) {
            info!("DRY RUN: would cancel all orders");
            return Ok(());
        }

        let client = self.client.lock().await;
        client.cancel_all_orders().await.context("Failed to cancel all orders")?;

        info!("All orders cancelled");
        Ok(())
    }

    /// Get a provider with signer for on-chain transactions.
    /// Returns a provider that can be used to send transactions.
    /// 
    /// Note: For Gnosis Safe proxy wallets (SIGNATURE_TYPE=2), redemption calls
    /// may need to go through the Safe's execTransaction. This implementation
    /// currently supports direct EOA redemption (SIGNATURE_TYPE=0) only.
    #[allow(dead_code)]
    pub fn get_provider_with_signer(
        &self,
        rpc_url: &str,
    ) -> Result<impl alloy::providers::Provider + Clone> {
        let rpc_url: url::Url = rpc_url.parse()
            .context("Invalid RPC URL")?;
        
        // Create alloy signer from private key
        let alloy_signer = AlloyLocalSigner::from_str(&self.private_key)
            .context("Failed to create alloy signer from private key")?
            .with_chain_id(Some(POLYGON));
        
        // Create provider and attach signer using wallet()
        let provider = ProviderBuilder::new()
            .wallet(alloy_signer)
            .connect_http(rpc_url);
        
        Ok(provider)
    }

    /// Get the signature type (0 = EOA, 2 = Gnosis Safe).
    pub fn signature_type(&self) -> u8 {
        self.signature_type
    }

    /// Get the private key (for creating signers in redemption).
    #[allow(dead_code)]
    pub fn private_key(&self) -> &str {
        &self.private_key
    }
}
