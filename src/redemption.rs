//! On-chain redemption: redeemPositions() and mergePositions() via alloy.

use crate::execution::ExecutionEngine;
use crate::market_discovery;
use crate::types::{Coin, Market, Period};
use anyhow::{Context, Result};
use chrono::Utc;
use polymarket_client_sdk::types::{Address, U256};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use std::str::FromStr as _;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

// Contract addresses (Polygon mainnet)
const CTF_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
const NEG_RISK_CTF_EXCHANGE: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";
const CONDITIONAL_TOKENS: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";

// CTF Exchange ABI (simplified - just the functions we need)
alloy::sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    contract CTFExchange {
        function redeemPositions(
            address collateralToken,
            bytes32 parentCollectionId,
            bytes32 conditionId,
            uint256[] calldata indexSets,
            uint256[] calldata amounts
        ) external;

        function mergePositions(
            address collateralToken,
            bytes32 parentCollectionId,
            bytes32 conditionId,
            uint256[] calldata indexSets,
            uint256[] calldata amounts
        ) external;
    }
}

#[derive(Debug, serde::Deserialize)]
struct GammaMarketResponse {
    #[serde(rename = "conditionID")]
    condition_id: String,
    slug: String,
    closed: Option<bool>,
    #[serde(rename = "negRisk")]
    neg_risk: bool,
}

pub struct RedemptionManager {
    execution: Arc<ExecutionEngine>,
    rpc_url: String,
    funder_address: Address,
}

impl RedemptionManager {
    pub fn new(execution: Arc<ExecutionEngine>, rpc_url: String, funder_address: Address) -> Self {
        Self {
            execution,
            rpc_url,
            funder_address,
        }
    }

    /// Check if market is resolved via Gamma API.
    async fn is_market_resolved(&self, slug: &str) -> Result<bool> {
        let url = format!("https://gamma-api.polymarket.com/markets?slug={}", slug);
        let client = reqwest::Client::new();
        let response = client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch market from Gamma API")?;

        if !response.status().is_success() {
            return Ok(false);
        }

        let markets: Vec<GammaMarketResponse> = response
            .json()
            .await
            .context("Failed to parse Gamma API response")?;

        Ok(markets.first().and_then(|m| m.closed).unwrap_or(false))
    }

    /// Redeem winning positions.
    pub async fn redeem_positions(
        &self,
        market: &Market,
        token_ids: Vec<String>,
        amounts: Vec<Decimal>,
    ) -> Result<()> {
        if !self.is_market_resolved(&market.slug).await? {
            return Ok(());
        }

        info!(
            market = %market.slug,
            token_count = token_ids.len(),
            "Redeeming positions"
        );

        // Get provider with signer from execution engine
        // Note: For Gnosis Safe proxy wallets (SIGNATURE_TYPE=2), redemption calls
        // may need to go through the Safe's execTransaction. This implementation
        // currently supports direct EOA redemption (SIGNATURE_TYPE=0) only.
        let provider = self.execution.get_provider_with_signer(&self.rpc_url)
            .context("Failed to create provider with signer")?;
        let provider = Arc::new(provider);

        // Get CTF exchange address
        let exchange_addr = if market.neg_risk {
            Address::from_str(NEG_RISK_CTF_EXCHANGE)?
        } else {
            Address::from_str(CTF_EXCHANGE)?
        };

        // USDC collateral
        let collateral = Address::from_str("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174")?;
        let condition_id = U256::from_str(&market.condition_id)?;

        // Convert token IDs to index sets (simplified: assume each token is a separate index set)
        let index_sets: Vec<U256> = token_ids
            .iter()
            .map(|tid| U256::from_str(tid).unwrap_or(U256::ZERO))
            .collect();

        let amounts_u256: Vec<U256> = amounts
            .iter()
            .map(|a| {
                // Convert Decimal to U256 (USDC has 6 decimals)
                use rust_decimal::prelude::ToPrimitive;
                let scaled = (*a * dec!(1_000_000)).to_u128().unwrap_or(0);
                U256::from(scaled)
            })
            .collect();

        // Build contract instance - with #[sol(rpc)], new() is generated
        let contract = CTFExchange::new(exchange_addr, &*provider);

        // Call redeemPositions and send the transaction
        let call = contract.redeemPositions(
            collateral,
            alloy::primitives::B256::ZERO, // parentCollectionId (typically 0)
            alloy::primitives::B256::from(condition_id.to_be_bytes::<32>()),
            index_sets,
            amounts_u256,
        );

        // Send the transaction
        let pending = call.send().await
            .context("Failed to send redeemPositions transaction")?;
        
        let receipt = pending.get_receipt().await
            .context("Failed to get transaction receipt")?;

        info!(
            market = %market.slug,
            exchange = %exchange_addr,
            condition_id = %market.condition_id,
            token_count = token_ids.len(),
            tx_hash = ?receipt.transaction_hash,
            "RedeemPositions transaction sent successfully"
        );

        Ok(())
    }

    /// Merge YES+NO positions (1 YES + 1 NO = 1 USDC).
    pub async fn merge_positions(
        &self,
        market: &Market,
        yes_shares: Decimal,
        no_shares: Decimal,
    ) -> Result<()> {
        if yes_shares <= Decimal::ZERO || no_shares <= Decimal::ZERO {
            return Ok(());
        }

        let merge_amount = yes_shares.min(no_shares);

        info!(
            market = %market.slug,
            yes_shares = %yes_shares,
            no_shares = %no_shares,
            merge_amount = %merge_amount,
            "Merging YES+NO positions"
        );

        // Get provider with signer from execution engine
        // Note: For Gnosis Safe proxy wallets (SIGNATURE_TYPE=2), redemption calls
        // may need to go through the Safe's execTransaction. This implementation
        // currently supports direct EOA redemption (SIGNATURE_TYPE=0) only.
        let provider = self.execution.get_provider_with_signer(&self.rpc_url)
            .context("Failed to create provider with signer")?;
        let provider = Arc::new(provider);

        // Get CTF exchange address
        let exchange_addr = if market.neg_risk {
            Address::from_str(NEG_RISK_CTF_EXCHANGE)?
        } else {
            Address::from_str(CTF_EXCHANGE)?
        };

        let collateral = Address::from_str("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174")?;
        let condition_id = U256::from_str(&market.condition_id)?;

        // Index sets: YES is typically 1, NO is typically 2 (or derive from token IDs)
        // For simplicity, we'll use placeholder values - in production, derive from actual token structure
        let index_sets = vec![U256::from(1), U256::from(2)];
        let amounts_u256 = vec![
            U256::from((merge_amount * dec!(1_000_000)).to_u128().unwrap_or(0)),
            U256::from((merge_amount * dec!(1_000_000)).to_u128().unwrap_or(0)),
        ];

        let contract = CTFExchange::new(exchange_addr, &*provider);

        let call = contract.mergePositions(
            collateral,
            alloy::primitives::B256::ZERO,
            alloy::primitives::B256::from(condition_id.to_be_bytes::<32>()),
            index_sets,
            amounts_u256,
        );

        // Send the transaction
        let pending = call.send().await
            .context("Failed to send mergePositions transaction")?;
        
        let receipt = pending.get_receipt().await
            .context("Failed to get transaction receipt")?;

        info!(
            market = %market.slug,
            exchange = %exchange_addr,
            condition_id = %market.condition_id,
            merge_amount = %merge_amount,
            tx_hash = ?receipt.transaction_hash,
            "MergePositions transaction sent successfully"
        );

        Ok(())
    }

    /// Background loop: check resolved markets every 15s and redeem/merge.
    pub async fn run(
        &self,
        shutdown: tokio_util::sync::CancellationToken,
        coins: &[Coin],
        periods: &[Period],
    ) -> Result<()> {
        info!("Redemption manager started");

        loop {
            if shutdown.is_cancelled() {
                break;
            }

            let now = Utc::now().timestamp();

            // Check recent rounds (last 24 hours)
            for coin in coins {
                for period in periods {
                    let period_secs = period.as_seconds();
                    // Check last 10 rounds
                    for i in 0..10 {
                        let round_start = ((now / period_secs) - i) * period_secs;
                        let slug = market_discovery::generate_slug(*coin, *period, round_start);

                        if let Ok(resolved) = self.is_market_resolved(&slug).await {
                            if resolved {
                                // In production: query positions for this market and redeem/merge
                                // For now, just log
                                info!(
                                    slug = %slug,
                                    "Market resolved, would check positions and redeem"
                                );
                            }
                        }
                    }
                }
            }

            sleep(Duration::from_secs(15)).await;
        }

        Ok(())
    }
}
