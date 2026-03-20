//! On-chain redemption: redeemPositions() and mergePositions() via alloy.
//! Supports both direct EOA (SIGNATURE_TYPE=0) and Gnosis Safe proxy (SIGNATURE_TYPE=2).

use crate::execution::ExecutionEngine;
use crate::market_discovery;
use crate::types::{Coin, Market, Period};
use anyhow::{Context, Result};
use alloy::primitives::{B256, Bytes, U256 as AlloyU256};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::LocalSigner as AlloyLocalSigner;
use alloy::signers::Signer as AlloySigner;
use chrono::Utc;
use polymarket_client_sdk::types::{Address, U256};
use polymarket_client_sdk::POLYGON;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use std::str::FromStr as _;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::info;

// Contract addresses (Polygon mainnet)
#[allow(dead_code)]
const CTF_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
#[allow(dead_code)]
const NEG_RISK_CTF_EXCHANGE: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";
#[allow(dead_code)]
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

// Gnosis Safe ABI
alloy::sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface GnosisSafe {
        function execTransaction(
            address to,
            uint256 value,
            bytes calldata data,
            uint8 operation,
            uint256 safeTxGas,
            uint256 baseGas,
            uint256 gasPrice,
            address gasToken,
            address refundReceiver,
            bytes memory signatures
        ) external payable returns (bool success);

        function nonce() external view returns (uint256);
    }
}

#[derive(Debug, serde::Deserialize)]
struct GammaMarketResponse {
    #[serde(rename = "conditionID")]
    #[allow(dead_code)]
    condition_id: String,
    #[allow(dead_code)]
    slug: String,
    closed: Option<bool>,
    #[serde(rename = "negRisk")]
    #[allow(dead_code)]
    neg_risk: bool,
}

pub struct RedemptionManager {
    #[allow(dead_code)]
    execution: Arc<ExecutionEngine>,
    #[allow(dead_code)]
    rpc_url: String,
    #[allow(dead_code)]
    funder_address: Address,
    #[allow(dead_code)]
    signature_type: u8,
}

impl RedemptionManager {
    pub fn new(execution: Arc<ExecutionEngine>, rpc_url: String, funder_address: Address) -> Self {
        // Get signature_type from execution engine
        let signature_type = execution.signature_type();
        Self {
            execution,
            rpc_url,
            funder_address,
            signature_type,
        }
    }

    /// Build inner calldata for redeemPositions or mergePositions call.
    #[allow(dead_code)]
    fn build_inner_calldata(
        &self,
        _exchange_addr: Address,
        function_name: &str,
        collateral: Address,
        condition_id: U256,
        index_sets: Vec<U256>,
        amounts: Vec<U256>,
    ) -> Result<Bytes> {
        // Convert U256 to AlloyU256
        let index_sets_alloy: Vec<AlloyU256> = index_sets
            .iter()
            .map(|u| {
                let limbs = u.as_limbs();
                AlloyU256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]])
            })
            .collect();
        let amounts_alloy: Vec<AlloyU256> = amounts
            .iter()
            .map(|u| {
                let limbs = u.as_limbs();
                AlloyU256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]])
            })
            .collect();
        
        // Encode the function call using alloy's SolCall trait
        let calldata = match function_name {
            "redeemPositions" => {
                use alloy::sol_types::SolCall;
                let call_data = CTFExchange::redeemPositionsCall {
                    collateralToken: collateral.into(),
                    parentCollectionId: B256::ZERO,
                    conditionId: B256::from(condition_id.to_be_bytes::<32>()),
                    indexSets: index_sets_alloy,
                    amounts: amounts_alloy,
                };
                call_data.abi_encode()
            }
            "mergePositions" => {
                use alloy::sol_types::SolCall;
                let call_data = CTFExchange::mergePositionsCall {
                    collateralToken: collateral.into(),
                    parentCollectionId: B256::ZERO,
                    conditionId: B256::from(condition_id.to_be_bytes::<32>()),
                    indexSets: index_sets_alloy,
                    amounts: amounts_alloy,
                };
                call_data.abi_encode()
            }
            _ => anyhow::bail!("Unknown function: {}", function_name),
        };
        
        Ok(Bytes::from(calldata))
    }

    /// Sign a Gnosis Safe transaction using EIP-712.
    #[allow(dead_code)]
    async fn sign_safe_transaction(
        &self,
        safe_address: Address,
        to: Address,
        data: Bytes,
        nonce: AlloyU256,
    ) -> Result<Bytes> {
        use alloy::primitives::keccak256;
        
        // EIP-712 domain separator for Gnosis Safe
        // Domain: { chainId: 137, verifyingContract: safe_address }
        let chain_id = AlloyU256::from(137u64);
        
        // EIP712Domain type hash: keccak256("EIP712Domain(uint256 chainId,address verifyingContract)")
        let domain_type_hash = keccak256(b"EIP712Domain(uint256 chainId,address verifyingContract)");
        
        // Encode domain separator: encode(domainTypeHash, chainId, verifyingContract)
        let mut domain_data = Vec::with_capacity(32 + 32 + 32);
        domain_data.extend_from_slice(domain_type_hash.as_slice());
        domain_data.extend_from_slice(&chain_id.to_be_bytes::<32>());
        domain_data.extend_from_slice(safe_address.as_slice());
        let domain_separator = keccak256(domain_data);
        
        // SafeTx type hash: keccak256("SafeTx(address to,uint256 value,bytes data,uint8 operation,uint256 safeTxGas,uint256 baseGas,uint256 gasPrice,address gasToken,address refundReceiver,uint256 nonce)")
        let safe_tx_type_hash = keccak256(
            b"SafeTx(address to,uint256 value,bytes data,uint8 operation,uint256 safeTxGas,uint256 baseGas,uint256 gasPrice,address gasToken,address refundReceiver,uint256 nonce)"
        );
        
        // Hash the data bytes
        let data_hash = keccak256(&data);
        
        // Encode SafeTx struct: encode(typeHash, to, value, dataHash, operation, safeTxGas, baseGas, gasPrice, gasToken, refundReceiver, nonce)
        let mut safe_tx_data = Vec::with_capacity(32 * 11);
        safe_tx_data.extend_from_slice(safe_tx_type_hash.as_slice());
        safe_tx_data.extend_from_slice(to.as_slice());
        safe_tx_data.extend_from_slice(&AlloyU256::ZERO.to_be_bytes::<32>()); // value
        safe_tx_data.extend_from_slice(data_hash.as_slice()); // data hash
        safe_tx_data.extend_from_slice(&[0u8; 31]); // operation (uint8, padded to 32 bytes)
        safe_tx_data.push(0u8); // operation = 0 (Call)
        safe_tx_data.extend_from_slice(&AlloyU256::ZERO.to_be_bytes::<32>()); // safeTxGas
        safe_tx_data.extend_from_slice(&AlloyU256::ZERO.to_be_bytes::<32>()); // baseGas
        safe_tx_data.extend_from_slice(&AlloyU256::ZERO.to_be_bytes::<32>()); // gasPrice
        safe_tx_data.extend_from_slice(Address::ZERO.as_slice()); // gasToken
        safe_tx_data.extend_from_slice(Address::ZERO.as_slice()); // refundReceiver
        safe_tx_data.extend_from_slice(&nonce.to_be_bytes::<32>()); // nonce
        let safe_tx_hash = keccak256(safe_tx_data);
        
        // EIP-712 message: keccak256("\x19\x01" || domain_separator || safe_tx_hash)
        let mut message = Vec::with_capacity(2 + 32 + 32);
        message.extend_from_slice(b"\x19\x01");
        message.extend_from_slice(domain_separator.as_slice());
        message.extend_from_slice(safe_tx_hash.as_slice());
        let message_hash = keccak256(message);
        
        // Sign with EOA signer
        let alloy_signer = AlloyLocalSigner::from_str(&self.execution.private_key())
            .context("Failed to create alloy signer")?
            .with_chain_id(Some(POLYGON));
        
        let signature = alloy_signer.sign_hash(&message_hash)
            .await
            .context("Failed to sign Safe transaction")?;
        
        // Pack signature as r ++ s ++ v (65 bytes)
        // For Gnosis Safe, v should be 0 or 1 (recovery id)
        let mut packed = Vec::with_capacity(65);
        packed.extend_from_slice(&signature.r().to_be_bytes::<32>());
        packed.extend_from_slice(&signature.s().to_be_bytes::<32>());
        // Safe expects v to be 0 or 1 (recovery id)
        // signature.v() returns a bool (true = odd, false = even)
        // For ECDSA, recovery id is 0 for even y, 1 for odd y
        let recovery_id = if signature.v() { 1u8 } else { 0u8 };
        packed.push(recovery_id);
        
        Ok(Bytes::from(packed))
    }

    /// Execute a transaction through Gnosis Safe execTransaction.
    #[allow(dead_code)]
    async fn exec_safe_transaction(
        &self,
        safe_address: Address,
        to: Address,
        data: Bytes,
    ) -> Result<()> {
        // Get provider
        let rpc_url: url::Url = self.rpc_url.parse()
            .context("Invalid RPC URL")?;
        let provider = ProviderBuilder::new()
            .connect_http(rpc_url);
        let provider = Arc::new(provider);
        
        // Get current nonce
        let safe_contract = GnosisSafe::new(safe_address, &*provider);
        let nonce_result = safe_contract.nonce().call().await
            .context("Failed to get Safe nonce")?;
        // nonce() returns a Uint directly, not a struct
        let nonce = nonce_result;
        
        // Sign the transaction
        let signatures = self.sign_safe_transaction(safe_address, to, data.clone(), nonce).await
            .context("Failed to sign Safe transaction")?;
        
        // Create alloy signer for sending the transaction
        let alloy_signer = AlloyLocalSigner::from_str(&self.execution.private_key())
            .context("Failed to create alloy signer")?
            .with_chain_id(Some(POLYGON));
        
        let provider_with_signer = ProviderBuilder::new()
            .wallet(alloy_signer)
            .connect_http(self.rpc_url.parse::<url::Url>()?);
        let provider_with_signer = Arc::new(provider_with_signer);
        
        // Call execTransaction
        let safe_contract = GnosisSafe::new(safe_address, &*provider_with_signer);
        let call = safe_contract.execTransaction(
            to,
            AlloyU256::ZERO, // value
            data,
            0u8, // operation (0 = Call)
            AlloyU256::ZERO, // safeTxGas
            AlloyU256::ZERO, // baseGas
            AlloyU256::ZERO, // gasPrice
            Address::ZERO, // gasToken
            Address::ZERO, // refundReceiver
            signatures,
        );
        
        let pending_tx = call.send().await
            .context("Failed to send execTransaction")?;
        
        let receipt = pending_tx.get_receipt().await
            .context("Failed to get transaction receipt")?;
        
        info!(
            safe_address = %safe_address,
            to = %to,
            tx_hash = ?receipt.transaction_hash,
            "Safe execTransaction sent successfully"
        );
        
        Ok(())
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
    #[allow(dead_code)]
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
            signature_type = self.signature_type,
            "Redeeming positions"
        );

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

        // Branch on signature type
        if self.signature_type == 2 {
            // Gnosis Safe path: use execTransaction
            let inner_calldata = self.build_inner_calldata(
                exchange_addr,
                "redeemPositions",
                collateral,
                condition_id,
                index_sets,
                amounts_u256,
            )?;
            
            self.exec_safe_transaction(self.funder_address, exchange_addr, inner_calldata).await
                .context("Failed to execute Safe transaction for redeemPositions")?;
        } else {
            // Direct EOA path
            let provider = self.execution.get_provider_with_signer(&self.rpc_url)
                .context("Failed to create provider with signer")?;
            let provider = Arc::new(provider);

            // Build contract instance - with #[sol(rpc)], new() is generated
            let contract = CTFExchange::new(exchange_addr, &*provider);

            // Convert U256 to AlloyU256 for the contract call
            let index_sets_alloy: Vec<AlloyU256> = index_sets
                .iter()
                .map(|u| {
                    let limbs = u.as_limbs();
                    AlloyU256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]])
                })
                .collect();
            let amounts_alloy: Vec<AlloyU256> = amounts_u256
                .iter()
                .map(|u| {
                    let limbs = u.as_limbs();
                    AlloyU256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]])
                })
                .collect();

            // Call redeemPositions and send the transaction
            let call = contract.redeemPositions(
                collateral,
                B256::ZERO, // parentCollectionId (typically 0)
                B256::from(condition_id.to_be_bytes::<32>()),
                index_sets_alloy,
                amounts_alloy,
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
        }

        Ok(())
    }

    /// Merge YES+NO positions (1 YES + 1 NO = 1 USDC).
    #[allow(dead_code)]
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
            signature_type = self.signature_type,
            "Merging YES+NO positions"
        );

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

        // Branch on signature type
        if self.signature_type == 2 {
            // Gnosis Safe path: use execTransaction
            let inner_calldata = self.build_inner_calldata(
                exchange_addr,
                "mergePositions",
                collateral,
                condition_id,
                index_sets,
                amounts_u256,
            )?;
            
            self.exec_safe_transaction(self.funder_address, exchange_addr, inner_calldata).await
                .context("Failed to execute Safe transaction for mergePositions")?;
        } else {
            // Direct EOA path
            let provider = self.execution.get_provider_with_signer(&self.rpc_url)
                .context("Failed to create provider with signer")?;
            let provider = Arc::new(provider);

            let contract = CTFExchange::new(exchange_addr, &*provider);

            // Convert U256 to AlloyU256 for the contract call
            let index_sets_alloy: Vec<AlloyU256> = index_sets
                .iter()
                .map(|u| {
                    let limbs = u.as_limbs();
                    AlloyU256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]])
                })
                .collect();
            let amounts_alloy: Vec<AlloyU256> = amounts_u256
                .iter()
                .map(|u| {
                    let limbs = u.as_limbs();
                    AlloyU256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]])
                })
                .collect();

            let call = contract.mergePositions(
                collateral,
                B256::ZERO,
                B256::from(condition_id.to_be_bytes::<32>()),
                index_sets_alloy,
                amounts_alloy,
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
        }

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
