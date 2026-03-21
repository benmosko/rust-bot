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
use polymarket_client_sdk::clob::types::response::PostOrderResponse;
use polymarket_client_sdk::clob::types::{OrderStatusType, OrderType, Side, SignatureType};
use polymarket_client_sdk::clob::{Client, Config};
use polymarket_client_sdk::types::{Address, U256};
use polymarket_client_sdk::POLYGON;
use polymarket_client_sdk::auth::LocalSigner;
use rust_decimal::Decimal;
use std::str::FromStr as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::{error, info};

use crate::types::FeeRate;

/// Calculate the number of decimal places from a tick_size.
/// For example, 0.01 → 2, 0.001 → 3, 0.1 → 1
fn decimal_places_from_tick_size(tick_size: Decimal) -> u32 {
    tick_size.to_string().split('.').nth(1).map(|s| s.len()).unwrap_or(2) as u32
}

/// Round price to tick_size precision and size to 2 decimal places.
/// Price: 0.01→2dp, 0.001→3dp, 0.0001→4dp (based on tick_size)
/// Size: always 2 decimal places (e.g., 8.16 shares is valid)
/// Uses string roundtrip to remove floating point artifacts.
fn round_price_and_size(price: Decimal, tick_size: Decimal, size: Decimal) -> (Decimal, Decimal) {
    // Round price to tick_size precision (0.01→2dp, 0.001→3dp, 0.0001→4dp)
    let price_decimals = decimal_places_from_tick_size(tick_size);
    let rounded_price = price.round_dp(price_decimals);
    
    // Force clean decimal by round-tripping through string with fixed precision
    let price_str = format!("{:.prec$}", rounded_price, prec = price_decimals as usize);
    let clean_price = Decimal::from_str(&price_str)
        .unwrap_or(rounded_price); // Fallback to rounded if parse fails
    
    // Round size to 2 decimal places (always 2dp for size)
    let rounded_size = size.round_dp(2);
    
    // Force clean decimal by round-tripping through string with fixed precision (2dp for size)
    let size_str = format!("{:.2}", rounded_size);
    let clean_size = Decimal::from_str(&size_str)
        .unwrap_or(rounded_size); // Fallback to rounded if parse fails
    
    (clean_price, clean_size)
}

const CLOB_API_BASE: &str = "https://clob.polymarket.com";

/// Walks `std::error::Error::source` and SDK [`polymarket_client_sdk::error::Error`] to surface
/// HTTP status and raw API response body (when the SDK used [`polymarket_client_sdk::error::Status`]).
pub fn format_order_error_for_logs(e: &anyhow::Error) -> String {
    use polymarket_client_sdk::error::Error as PmErr;
    use polymarket_client_sdk::error::Status as PmStatus;
    use std::error::Error as StdError;

    let mut parts = vec![format!("anyhow_chain={:#}", e)];
    let mut cur: Option<&dyn StdError> = Some(e.as_ref());
    while let Some(err) = cur {
        if let Some(pm) = err.downcast_ref::<PmErr>() {
            parts.push(format!("pm_sdk={pm:?}"));
            if let Some(st) = pm.downcast_ref::<PmStatus>() {
                parts.push(format!(
                    "raw_api status={} {} {} body={}",
                    st.status_code, st.method, st.path, st.message
                ));
            }
        } else if let Some(st) = err.downcast_ref::<PmStatus>() {
            parts.push(format!(
                "raw_api status={} {} {} body={}",
                st.status_code, st.method, st.path, st.message
            ));
        }
        cur = err.source();
    }
    parts.join(" | ")
}

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
    /// When true, new orders are rejected (Telegram `/pause`). Cancels still run.
    trading_paused: Arc<AtomicBool>,
}

impl ExecutionEngine {
    pub async fn new(
        private_key: &str,
        signature_type: u8,
        funder_address: Option<String>,
        dry_run: Arc<AtomicBool>,
        trading_paused: Arc<AtomicBool>,
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
            trading_paused,
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

    /// Limit order (GTC, FAK, FOK, …). All order ops go through this Mutex for nonce safety.
    /// For BUY, `post_order` returns `making_amount` ≈ USDC spent and `taking_amount` ≈ shares received.
    pub async fn place_limit_order(
        &self,
        token_id: &str,
        side: Side,
        price: Decimal,
        size: Decimal,
        tick_size: Decimal,
        order_type: OrderType,
    ) -> Result<PostOrderResponse> {
        let e0 = Instant::now();

        let (rounded_price, rounded_size) = round_price_and_size(price, tick_size, size);

        if self.trading_paused.load(Ordering::Relaxed) {
            anyhow::bail!("Trading paused (use Telegram /resume)");
        }

        if self.dry_run.load(Ordering::Relaxed) {
            let (making_amount, taking_amount) = if side == Side::Buy {
                (
                    (rounded_size * rounded_price).round_dp(6),
                    rounded_size,
                )
            } else {
                (
                    rounded_size,
                    (rounded_size * rounded_price).round_dp(6),
                )
            };
            info!(
                side = ?side,
                order_type = ?order_type,
                price = %rounded_price,
                size = %rounded_size,
                making = %making_amount,
                taking = %taking_amount,
                market = %token_id,
                "DRY RUN: would place limit order"
            );
            return Ok(
                PostOrderResponse::builder()
                    .making_amount(making_amount)
                    .taking_amount(taking_amount)
                    .order_id(format!("dry_run_order_{}", Utc::now().timestamp_millis()))
                    .status(OrderStatusType::Matched)
                    .success(true)
                    .build(),
            );
        }

        let token_id_u256 = U256::from_str(token_id).context("Invalid token_id for order")?;

        let size_sdk = polymarket_client_sdk::types::Decimal::from_str_exact(rounded_size.to_string().as_str())
            .context("Invalid size")?;

        let client = self.client.lock().await;

        let order = client
            .limit_order()
            .token_id(token_id_u256)
            .size(size_sdk)
            .price(
                polymarket_client_sdk::types::Decimal::from_str_exact(rounded_price.to_string().as_str())
                    .context("Invalid price")?,
            )
            .side(side)
            .order_type(order_type.clone())
            .build()
            .await
            .context("Failed to build order")?;

        let e1 = Instant::now();

        let signed_order = client
            .sign(self.signer.as_ref(), order)
            .await
            .context("Failed to sign order")?;

        let e2 = Instant::now();

        let response = match client.post_order(signed_order).await {
            Ok(r) => r,
            Err(sdk_err) => {
                let ae = anyhow::Error::new(sdk_err).context("Failed to post order");
                error!(
                    token_id = %token_id,
                    side = ?side,
                    price = %rounded_price,
                    size = %rounded_size,
                    order_type = ?order_type,
                    err = ?ae,
                    detail = %format_order_error_for_logs(&ae),
                    "Gab: post_order failed (raw API body in `detail` when present)"
                );
                return Err(ae);
            }
        };

        let e3 = Instant::now();

        info!(
            token_id = %token_id,
            "EXEC LATENCY: build={}us, sign={}us, http_post={}ms, TOTAL={}ms",
            e1.duration_since(e0).as_micros(),
            e2.duration_since(e1).as_micros(),
            e3.duration_since(e2).as_millis(),
            e3.duration_since(e0).as_millis(),
        );

        info!(
            token_id = %token_id,
            side = ?side,
            order_type = ?order_type,
            price = %rounded_price,
            size = %rounded_size,
            order_id = %response.order_id,
            success = response.success,
            making = %response.making_amount,
            taking = %response.taking_amount,
            "Order placed"
        );

        Ok(response)
    }

    /// GTC limit order; returns order id only (backward compatible).
    pub async fn place_order(
        &self,
        token_id: &str,
        side: Side,
        price: Decimal,
        size: Decimal,
        tick_size: Decimal,
    ) -> Result<String> {
        let r = self
            .place_limit_order(token_id, side, price, size, tick_size, OrderType::GTC)
            .await?;
        Ok(r.order_id)
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

    /// Get order status and filled size using the SDK's order() method.
    /// Returns Option<(status, size_matched)> with `size_matched` from the CLOB (no string round-trip).
    /// Returns None if the order doesn't exist.
    pub async fn get_order_status(&self, order_id: &str) -> Result<Option<(String, Decimal)>> {
        if self.dry_run.load(Ordering::Relaxed) {
            // In dry run, return a fake status
            return Ok(Some(("live".to_string(), Decimal::ZERO)));
        }

        let client = self.client.lock().await;
        match client.order(order_id).await {
            Ok(order) => {
                let status = order.status.to_string();
                let size_matched = order.size_matched;
                drop(client);
                Ok(Some((status, size_matched)))
            }
            Err(e) => {
                drop(client);
                // If order not found, return None
                let err_str = e.to_string();
                if err_str.contains("not found") || err_str.contains("404") {
                    Ok(None)
                } else {
                    Err(e.into())
                }
            }
        }
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
