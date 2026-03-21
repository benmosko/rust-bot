//! On-chain redemption: redeemPositions() and mergePositions() via alloy.
//! Supports both direct EOA (SIGNATURE_TYPE=0) and Gnosis Safe proxy (SIGNATURE_TYPE=2).

use crate::execution::ExecutionEngine;
use crate::pnl::PnLManager;
use crate::types::{Coin, Market, Period, TuiEvent};
use anyhow::{Context, Result};
use alloy::primitives::{B256, Bytes, U256 as AlloyU256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::LocalSigner as AlloyLocalSigner;
use alloy::signers::Signer as AlloySigner;
use polymarket_client_sdk::types::{Address, U256};
use polymarket_client_sdk::POLYGON;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::str::FromStr as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};
use tracing::info;
use chrono::Local;

// Contract addresses (Polygon mainnet)
#[allow(dead_code)]
const CTF_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
#[allow(dead_code)]
const NEG_RISK_CTF_EXCHANGE: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";
// Conditional Tokens contract (CTF) - used for direct redemption
const CONDITIONAL_TOKENS: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";
// Multicall3 - batch CTF.redeemPositions in one Safe tx (poly_claim uses this)
const MULTICALL3: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";
const USDC_COLLATERAL: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
// Uniswap V3 and POL funding (poly_claim auto-funds POL when low)
#[allow(dead_code)]
const UNISWAP_V3_ROUTER: &str = "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45";
#[allow(dead_code)]
const WPOL_ADDR: &str = "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270";
const POL_THRESHOLD: f64 = 3.0; // Fund when POL < 3.0
#[allow(dead_code)]
const USDC_SWAP_AMOUNT: u64 = 4_000_000; // $4 USDC (6 decimals)
#[allow(dead_code)]
const POOL_FEE: u32 = 3000; // 0.3% fee tier
#[allow(dead_code)]
const GAS_APPROVE: u64 = 120_000;
#[allow(dead_code)]
const GAS_SWAP: u64 = 300_000;

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

// Conditional Tokens contract ABI (4-parameter redeemPositions - no amounts)
alloy::sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    contract ConditionalTokens {
        function redeemPositions(
            address collateralToken,
            bytes32 parentCollectionId,
            bytes32 conditionId,
            uint256[] calldata indexSets
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

// Multicall3 ABI (poly_claim batches redeems via aggregate3)
alloy::sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    contract Multicall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct Result {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls) external returns (Result[] memory returnData);
    }
}

// ERC1155 balanceOf for CTF (poly_claim skips when balance 0 or partial)
alloy::sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    contract ERC1155 {
        function balanceOf(address account, uint256 id) external view returns (uint256);
    }
}

// ERC20 for USDC (approve, balanceOf, allowance)
alloy::sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    contract ERC20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

// Uniswap V3 SwapRouter02 (for POL funding)
alloy::sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    contract SwapRouter02 {
        function multicall(uint256 deadline, bytes[] calldata data) external payable returns (bytes[] memory results);
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

// Data API Position response (matches Polymarket Data API + poly_claim)
#[derive(Debug, Clone, serde::Deserialize)]
struct DataApiPosition {
    #[serde(rename = "conditionId")]
    condition_id: String,
    slug: String,
    size: f64,
    #[serde(rename = "currentValue")]
    current_value: f64,
    #[serde(rename = "outcomeIndex")]
    outcome_index: Option<i32>,
    #[serde(rename = "negativeRisk")]
    #[allow(dead_code)]
    negative_risk: Option<bool>,
    #[allow(dead_code)]
    title: Option<String>,
    /// ERC1155 token ID for on-chain balance check (poly_claim uses this to skip already-redeemed/partial)
    asset: Option<String>,
}

/// Map Data API `outcomeIndex` to the same side strings used in [`PnLManager::on_fill`]
/// (`"YES"` / `"NO"`). Polymarket binary markets use index 0 = Yes, 1 = No (see Polymarket docs).
fn side_label_for_redeemed_position(pos: &DataApiPosition, pnl: &PnLManager) -> String {
    match pos.outcome_index {
        Some(0) => "YES".to_string(),
        Some(1) => "NO".to_string(),
        _ => pnl
            .find_position_by_condition_id(&pos.condition_id)
            .map(|p| p.side)
            .unwrap_or_else(|| "YES".to_string()),
    }
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
    dry_run: Arc<AtomicBool>,
    tui_tx: Option<mpsc::Sender<TuiEvent>>,
    pnl_manager: Arc<PnLManager>,
}

impl RedemptionManager {
    pub fn new(execution: Arc<ExecutionEngine>, rpc_url: String, funder_address: Address, dry_run: Arc<AtomicBool>, tui_tx: Option<mpsc::Sender<TuiEvent>>, pnl_manager: Arc<PnLManager>) -> Self {
        // Get signature_type from execution engine
        let signature_type = execution.signature_type();
        Self {
            execution,
            rpc_url,
            funder_address,
            signature_type,
            dry_run,
            tui_tx,
            pnl_manager,
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
    /// operation: 0 = Call, 1 = DelegateCall (e.g. for Multicall3).
    #[allow(dead_code)]
    async fn sign_safe_transaction(
        &self,
        safe_address: Address,
        to: Address,
        data: Bytes,
        operation: u8,
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
        safe_tx_data.push(operation); // 0 = Call, 1 = DelegateCall
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
    /// operation: 0 = Call, 1 = DelegateCall (e.g. for Multicall3).
    #[allow(dead_code)]
    async fn exec_safe_transaction(
        &self,
        safe_address: Address,
        to: Address,
        data: Bytes,
        operation: u8,
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
        let signatures = self.sign_safe_transaction(safe_address, to, data.clone(), operation, nonce).await
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
            operation, // 0 = Call, 1 = DelegateCall
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

    /// Fetch redeemable positions from Polymarket Data API.
    async fn fetch_redeemable_positions(&self) -> Result<Vec<DataApiPosition>> {
        let url = format!(
            "https://data-api.polymarket.com/positions?user={}&redeemable=true&sizeThreshold=0&limit=500",
            self.funder_address.to_string()
        );
        let client = reqwest::Client::new();
        let response = client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch positions from Data API")?;

        if !response.status().is_success() {
            anyhow::bail!("Data API returned status: {}", response.status());
        }

        let positions: Vec<DataApiPosition> = response
            .json()
            .await
            .context("Failed to parse Data API response")?;

        Ok(positions)
    }

    /// On-chain CTF ERC1155 balance in shares (poly_claim: skip if 0 or partial).
    /// Returns None on RPC/parse error, Some(shares) with shares = raw/1e6.
    async fn get_ctf_balance_shares(&self, owner: &str, asset_id: &str) -> Option<f64> {
        let token_id = match asset_id.parse::<u64>() {
            Ok(id) => AlloyU256::from(id),
            Err(_) => return None,
        };
        let owner_addr = match Address::from_str(owner) {
            Ok(a) => a,
            Err(_) => return None,
        };
        let ctf_addr = match Address::from_str(CONDITIONAL_TOKENS) {
            Ok(a) => a,
            Err(_) => return None,
        };
        let rpc_url = match self.rpc_url.parse::<url::Url>() {
            Ok(u) => u,
            Err(_) => return None,
        };
        let provider = ProviderBuilder::new().connect_http(rpc_url);
        let provider = Arc::new(provider);
        let erc1155 = ERC1155::new(ctf_addr, &*provider);
        let raw = match erc1155.balanceOf(owner_addr, token_id).call().await {
            Ok(b) => b,
            Err(_) => return None,
        };
        let raw_u64 = raw.to::<u64>();
        let shares = raw_u64 as f64 / 1e6;
        Some(shares)
    }

    /// Build redeemPositions calldata for one condition_id (for multicall/batch).
    fn build_redeem_calldata(&self, condition_id: &str) -> Result<Bytes> {
        let usdc_collateral = Address::from_str(USDC_COLLATERAL)?;
        let condition_id_bytes = if condition_id.starts_with("0x") {
            let hex = &condition_id[2..];
            if hex.len() != 64 {
                anyhow::bail!("Invalid conditionId length: expected 64 hex chars, got {}", hex.len());
            }
            let mut bytes = [0u8; 32];
            hex::decode_to_slice(hex, &mut bytes)
                .context("Failed to decode conditionId hex")?;
            B256::from(bytes)
        } else {
            if condition_id.len() != 64 {
                anyhow::bail!("Invalid conditionId length: expected 64 hex chars, got {}", condition_id.len());
            }
            let mut bytes = [0u8; 32];
            hex::decode_to_slice(condition_id, &mut bytes)
                .context("Failed to decode conditionId hex")?;
            B256::from(bytes)
        };
        let index_sets = vec![AlloyU256::from(1u64), AlloyU256::from(2u64)];
        use alloy::sol_types::SolCall;
        let call_data = ConditionalTokens::redeemPositionsCall {
            collateralToken: usdc_collateral.into(),
            parentCollectionId: B256::ZERO,
            conditionId: condition_id_bytes,
            indexSets: index_sets,
        };
        Ok(Bytes::from(call_data.abi_encode()))
    }

    /// Redeem a single position using Conditional Tokens contract (4-parameter version).
    async fn redeem_position_ctf(&self, condition_id: &str, slug: &str, payout: Decimal) -> Result<()> {
        if self.dry_run.load(Ordering::Relaxed) {
            info!(
                market = %slug,
                condition_id = %condition_id,
                payout = %payout,
                "DRY RUN: would redeem position"
            );
            return Ok(());
        }

        let redeem_time = Local::now().format("%H:%M:%S").to_string();
        let attempt_msg = format!("{} {} REDEEM attempt: ${:.2}", redeem_time, slug, payout);
        
        // Send redemption attempt to TUI
        if let Some(ref tx) = self.tui_tx {
            let _ = tx.try_send(TuiEvent::TradeLog(attempt_msg.clone()));
        }
        
        info!(
            market = %slug,
            condition_id = %condition_id,
            payout = %payout,
            signature_type = self.signature_type,
            "Redeeming position"
        );

        let ctf_address = Address::from_str(CONDITIONAL_TOKENS)?;
        let usdc_collateral = Address::from_str("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174")?;
        
        // Parse condition ID (should be 0x-prefixed 64 hex chars)
        let condition_id_bytes = if condition_id.starts_with("0x") {
            let hex = &condition_id[2..];
            if hex.len() != 64 {
                anyhow::bail!("Invalid conditionId length: expected 64 hex chars, got {}", hex.len());
            }
            let mut bytes = [0u8; 32];
            hex::decode_to_slice(hex, &mut bytes)
                .context("Failed to decode conditionId hex")?;
            B256::from(bytes)
        } else {
            // Try parsing as hex without 0x prefix
            if condition_id.len() != 64 {
                anyhow::bail!("Invalid conditionId length: expected 64 hex chars, got {}", condition_id.len());
            }
            let mut bytes = [0u8; 32];
            hex::decode_to_slice(condition_id, &mut bytes)
                .context("Failed to decode conditionId hex")?;
            B256::from(bytes)
        };

        // Index sets: [1, 2] redeems all winning positions (YES and NO outcomes)
        let index_sets = vec![AlloyU256::from(1u64), AlloyU256::from(2u64)];

        // Branch on signature type
        if self.signature_type == 2 {
            // Gnosis Safe path: use execTransaction
            // Build inner calldata for ConditionalTokens.redeemPositions
            use alloy::sol_types::SolCall;
            let call_data = ConditionalTokens::redeemPositionsCall {
                collateralToken: usdc_collateral.into(),
                parentCollectionId: B256::ZERO,
                conditionId: condition_id_bytes,
                indexSets: index_sets.clone(),
            };
            let inner_calldata = Bytes::from(call_data.abi_encode());
            
            self.exec_safe_transaction(self.funder_address, ctf_address, inner_calldata, 0u8).await
                .context("Failed to execute Safe transaction for redeemPositions")?;
            
            // Send success message for Gnosis Safe path
            let success_time = Local::now().format("%H:%M:%S").to_string();
            let success_msg = format!("{} {} REDEEM success: ${:.2}", success_time, slug, payout);
            
            if let Some(ref tx) = self.tui_tx {
                let _ = tx.try_send(TuiEvent::TradeLog(success_msg.clone()));
            }
        } else {
            // Direct EOA path
            let provider = self.execution.get_provider_with_signer(&self.rpc_url)
                .context("Failed to create provider with signer")?;
            let provider = Arc::new(provider);

            // Build contract instance
            let contract = ConditionalTokens::new(ctf_address, &*provider);

            // Call redeemPositions (4 parameters, no amounts)
            let call = contract.redeemPositions(
                usdc_collateral,
                B256::ZERO, // parentCollectionId (typically 0)
                condition_id_bytes,
                index_sets,
            );

            // Send the transaction
            let pending = call.send().await
                .context("Failed to send redeemPositions transaction")?;
            
            let receipt = pending.get_receipt().await
                .context("Failed to get transaction receipt")?;

            let success_time = Local::now().format("%H:%M:%S").to_string();
            let success_msg = format!("{} {} REDEEM success: ${:.2}", success_time, slug, payout);
            
            // Send redemption success to TUI
            if let Some(ref tx) = self.tui_tx {
                let _ = tx.try_send(TuiEvent::TradeLog(success_msg.clone()));
            }
            
            info!(
                market = %slug,
                condition_id = %condition_id,
                payout = %payout,
                tx_hash = ?receipt.transaction_hash,
                "Redeemed position successfully"
            );
        }

        Ok(())
    }

    /// Try to redeem multiple CIDs in one Safe tx via Multicall3 (poly_claim path).
    /// On success returns all cids as succeeded. On failure returns Err (caller can fall back to batch/single).
    async fn try_redeem_multicall_via_safe(&self, cids: &[String]) -> Result<Vec<String>> {
        if cids.is_empty() {
            return Ok(vec![]);
        }
        let ctf_address = Address::from_str(CONDITIONAL_TOKENS)?;
        let mc3_address = Address::from_str(MULTICALL3)?;
        let mut calls = Vec::with_capacity(cids.len());
        for cid in cids {
            let call_data = self.build_redeem_calldata(cid)?;
            calls.push(Multicall3::Call3 {
                target: ctf_address,
                allowFailure: true,
                callData: call_data,
            });
        }
        use alloy::sol_types::SolCall;
        let aggregate_calldata = Multicall3::aggregate3Call { calls }.abi_encode();
        self.exec_safe_transaction(
            self.funder_address,
            mc3_address,
            Bytes::from(aggregate_calldata),
            1u8, // DelegateCall
        )
        .await
        .context("Multicall redeem failed")?;
        Ok(cids.to_vec())
    }

    /// Batch redeem through Safe: build all Safe execTransaction calls with incrementing nonces,
    /// execute them in parallel (poly_claim path). Returns HashMap<cid, success>.
    /// Simplified: uses existing exec_safe_transaction but with manual nonce management.
    async fn try_redeem_batch_via_safe(&self, cids: &[String]) -> HashMap<String, bool> {
        if cids.is_empty() {
            return HashMap::new();
        }

        let rpc_url: url::Url = match self.rpc_url.parse() {
            Ok(u) => u,
            Err(_) => return cids.iter().map(|c| (c.clone(), false)).collect(),
        };
        let provider = Arc::new(ProviderBuilder::new().connect_http(rpc_url.clone()));
        let safe_contract = GnosisSafe::new(self.funder_address, &*provider);

        // Get initial Safe nonce
        let safe_nonce = match safe_contract.nonce().call().await {
            Ok(n) => n,
            Err(_) => return cids.iter().map(|c| (c.clone(), false)).collect(),
        };

        let ctf_address = match Address::from_str(CONDITIONAL_TOKENS) {
            Ok(a) => a,
            Err(_) => return cids.iter().map(|c| (c.clone(), false)).collect(),
        };
        let mut results = HashMap::new();

        // Execute all redeems sequentially with incrementing nonces
        for (i, cid) in cids.iter().enumerate() {
            let call_data = match self.build_redeem_calldata(cid) {
                Ok(cd) => cd,
                Err(_) => continue,
            };
            
            // Sequential execution (simpler than parallel, still correct)
            let nonce_i = safe_nonce + AlloyU256::from(i as u64);
            
            // Sign with nonce + i
            let signatures = match self.sign_safe_transaction(
                self.funder_address,
                ctf_address,
                call_data.clone(),
                0u8,
                nonce_i,
            ).await {
                Ok(sig) => sig,
                Err(e) => {
                    tracing::warn!(cid = %cid, error = %e, "Sign failed");
                    results.insert(cid.clone(), false);
                    continue;
                }
            };
            
            // Execute
            let alloy_signer = match AlloyLocalSigner::from_str(&self.execution.private_key())
                .map(|s| s.with_chain_id(Some(POLYGON))) {
                Ok(s) => s,
                Err(_) => {
                    results.insert(cid.clone(), false);
                    continue;
                }
            };
            
            let provider_with_signer = ProviderBuilder::new()
                .wallet(alloy_signer)
                .connect_http(self.rpc_url.parse::<url::Url>().unwrap());
            let provider_with_signer = Arc::new(provider_with_signer);
            
            let safe_contract_exec = GnosisSafe::new(self.funder_address, &*provider_with_signer);
            let call = safe_contract_exec.execTransaction(
                ctf_address,
                AlloyU256::ZERO,
                call_data,
                0u8,
                AlloyU256::ZERO,
                AlloyU256::ZERO,
                AlloyU256::ZERO,
                Address::ZERO,
                Address::ZERO,
                signatures,
            );
            
            match call.send().await {
                Ok(pending) => {
                    match pending.get_receipt().await {
                        Ok(_) => {
                            results.insert(cid.clone(), true);
                        }
                        Err(e) => {
                            let err_str: String = e.to_string().to_lowercase();
                            if err_str.contains("nonce") || err_str.contains("already") {
                                results.insert(cid.clone(), true);
                            } else {
                                tracing::warn!(cid = %cid, error = %e, "Receipt failed");
                                results.insert(cid.clone(), false);
                            }
                        }
                    }
                }
                Err(e) => {
                    let err_str: String = e.to_string().to_lowercase();
                    if err_str.contains("nonce") || err_str.contains("already") {
                        results.insert(cid.clone(), true);
                    } else {
                        tracing::warn!(cid = %cid, error = %e, "Send failed");
                        results.insert(cid.clone(), false);
                    }
                }
            }
        }

        results
    }

    /// Get POL balance for EOA (poly_claim checks this before redeeming).
    async fn get_pol_balance(&self, eoa: &str) -> Option<f64> {
        let eoa_addr = match Address::from_str(eoa) {
            Ok(a) => a,
            Err(_) => return None,
        };
        let rpc_url: url::Url = match self.rpc_url.parse() {
            Ok(u) => u,
            Err(_) => return None,
        };
        let provider = ProviderBuilder::new().connect_http(rpc_url);
        match provider.get_balance(eoa_addr.into()).await {
            Ok(bal) => Some(bal.to::<u128>() as f64 / 1e18),
            Err(_) => None,
        }
    }

    /// Auto-fund POL when balance is low (poly_claim: swaps USDC->POL via Uniswap V3).
    /// Returns true if funded or already sufficient, false on error.
    /// Note: Full Uniswap V3 swap encoding is complex; this checks balance and logs.
    async fn auto_fund_pol(&self) -> bool {
        // Get EOA address
        let alloy_signer = match AlloyLocalSigner::from_str(&self.execution.private_key())
            .map(|s| s.with_chain_id(Some(POLYGON))) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let eoa = alloy_signer.address();
        let eoa_str = eoa.to_string();

        // Check POL balance
        let pol_bal = match self.get_pol_balance(&eoa_str).await {
            Some(b) => b,
            None => {
                tracing::warn!("Could not check POL balance");
                return false;
            }
        };

        if pol_bal >= POL_THRESHOLD {
            return true; // Already sufficient
        }

        info!("EOA has {:.4} POL (< {:.1}), would fund via Uniswap V3 (not fully implemented)", pol_bal, POL_THRESHOLD);
        // TODO: Implement full Uniswap V3 SwapRouter02 multicall (exactInputSingle + unwrapWETH9)
        false
    }

    /// Redeem winning positions.
    /// 
    /// TODO: When selling or redeeming positions, query actual token balance first.
    /// Don't assume you hold exactly size_matched shares, because fees reduce the actual tokens received.
    /// Query the on-chain ERC1155 balance for each token_id before calling redeemPositions/mergePositions.
    #[allow(dead_code)]
    pub async fn redeem_positions(
        &self,
        market: &Market,
        token_ids: Vec<String>,
        amounts: Vec<Decimal>,
    ) -> Result<()> {
        if self.dry_run.load(Ordering::Relaxed) {
            info!(
                market = %market.slug,
                token_count = token_ids.len(),
                "DRY RUN: would redeem positions"
            );
            return Ok(());
        }

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
            
            self.exec_safe_transaction(self.funder_address, exchange_addr, inner_calldata, 0u8).await
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
    /// 
    /// TODO: Query actual token balances before merging. Fees reduce the actual tokens received,
    /// so don't assume you hold exactly yes_shares/no_shares. Query on-chain ERC1155 balances.
    #[allow(dead_code)]
    pub async fn merge_positions(
        &self,
        market: &Market,
        yes_shares: Decimal,
        no_shares: Decimal,
    ) -> Result<()> {
        if self.dry_run.load(Ordering::Relaxed) {
            info!(
                market = %market.slug,
                yes_shares = %yes_shares,
                no_shares = %no_shares,
                "DRY RUN: would merge positions"
            );
            return Ok(());
        }

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
            
            self.exec_safe_transaction(self.funder_address, exchange_addr, inner_calldata, 0u8).await
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

    /// Background loop: check for redeemable positions every 3s (poly_claim). Balance filter + multicall then single.
    /// Each position is retried up to 5 times before giving up.
    pub async fn run(
        &self,
        shutdown: tokio_util::sync::CancellationToken,
        _coins: &[Coin],
        _periods: &[Period],
    ) -> Result<()> {
        const POLL_INTERVAL_SECS: u64 = 3;
        const MAX_RETRIES_PER_POSITION: u32 = 5;
        let funder = self.funder_address.to_string();

        info!("Redemption manager started (polling every {}s, max {} retries per position, poly_claim flow)", POLL_INTERVAL_SECS, MAX_RETRIES_PER_POSITION);

        let mut retry_count: HashMap<String, u32> = HashMap::new();

        loop {
            if shutdown.is_cancelled() {
                break;
            }

            // Fetch redeemable positions from Data API (same API as poly_claim)
            match self.fetch_redeemable_positions().await {
                Ok(positions) => {
                    // Filter to worthwhile (currentValue >= 0.01, size >= 0.01) and not given up
                    let worthwhile: Vec<_> = positions
                        .into_iter()
                        .filter(|p| {
                            p.current_value >= 0.01
                                && p.size >= 0.01
                                && *retry_count.get(&p.condition_id).unwrap_or(&0) < MAX_RETRIES_PER_POSITION
                        })
                        .collect();

                    if worthwhile.is_empty() {
                        sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
                        continue;
                    }

                    // On-chain balance filter (poly_claim): skip if balance 0 or partial
                    let mut valid: Vec<DataApiPosition> = {
                        let mut out = Vec::with_capacity(worthwhile.len());
                        for pos in worthwhile {
                            let cid = pos.condition_id.clone();
                            let size = pos.size;
                            let has_asset = pos.asset.as_ref().map(|a| !a.is_empty()).unwrap_or(false);
                            if !has_asset {
                                out.push(pos);
                                continue;
                            }
                            let asset_id = pos.asset.as_ref().unwrap();
                            match self.get_ctf_balance_shares(&funder, asset_id).await {
                                None => {
                                    tracing::debug!(cid = %cid, "CTF balance RPC error, skipping");
                                }
                                Some(0.0) => {
                                    info!(cid = %cid, "SKIP on-chain balance=0 (already redeemed/sold)");
                                }
                                Some(chain_bal) if chain_bal < size * 0.5 => {
                                    info!(cid = %cid, chain_bal = %chain_bal, size = %size, "SKIP partial sold");
                                }
                                Some(_) => out.push(pos),
                            }
                        }
                        out
                    };

                    if valid.is_empty() {
                        sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
                        continue;
                    }

                    info!(
                        count = valid.len(),
                        total_value = valid.iter().map(|p| p.current_value).sum::<f64>(),
                        "Found redeemable positions (after balance filter)"
                    );

                    // Check POL balance and fund if needed (poly_claim does this before redeeming)
                    if self.signature_type == 2 {
                        let _ = self.auto_fund_pol().await; // Non-fatal if funding fails
                    }

                    // Safe path: try multicall first (poly_claim), then batch, then single
                    if self.signature_type == 2 && !valid.is_empty() {
                        let cids: Vec<String> = valid.iter().map(|p| p.condition_id.clone()).collect();
                        if let Ok(redeemed_cids) = self.try_redeem_multicall_via_safe(&cids).await {
                            for cid in &redeemed_cids {
                                retry_count.remove(cid);
                            }
                            for pos in &valid {
                                if redeemed_cids.contains(&pos.condition_id) {
                                    let condition_id = pos.condition_id.clone();
                                    let slug = pos.slug.clone();
                                    let payout = Decimal::try_from(pos.current_value)
                                        .unwrap_or_else(|_| Decimal::from_f64_retain(pos.current_value).unwrap_or(Decimal::ZERO));
                                    let side = side_label_for_redeemed_position(pos, &self.pnl_manager);
                                    if let Some(trade_pnl) = self.pnl_manager.on_resolve(&condition_id, &side, payout) {
                                        let win_lose = if payout > Decimal::ZERO { "WON" } else { "LOST" };
                                        let pnl_time = Local::now().format("%H:%M:%S").to_string();
                                        let pnl_msg = format!("{} {} {} P&L: ${:+.2} (payout: ${:.2})", pnl_time, slug, win_lose, trade_pnl, payout);
                                        if let Some(ref tx) = self.tui_tx {
                                            let _ = tx.try_send(TuiEvent::TradeLog(pnl_msg));
                                        }
                                    }
                                    if let Some(ref tx) = self.tui_tx {
                                        let _ = tx.try_send(TuiEvent::TradeLog(format!("{} {} REDEEM success: ${:.2}", Local::now().format("%H:%M:%S"), slug, payout)));
                                    }
                                }
                            }
                            info!(count = redeemed_cids.len(), "Multicall redeem success");
                            sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
                            continue;
                        }
                        tracing::warn!("Multicall redeem failed, trying batch");
                        // Try batch redeem (poly_claim fallback)
                        let batch_results = self.try_redeem_batch_via_safe(&cids).await;
                        let mut remaining = Vec::new();
                        for pos in &valid {
                            let cid = &pos.condition_id;
                            if let Some(&success) = batch_results.get(cid) {
                                if success {
                                    retry_count.remove(cid);
                                    let condition_id = pos.condition_id.clone();
                                    let slug = pos.slug.clone();
                                    let payout = Decimal::try_from(pos.current_value)
                                        .unwrap_or_else(|_| Decimal::from_f64_retain(pos.current_value).unwrap_or(Decimal::ZERO));
                                    let side = side_label_for_redeemed_position(pos, &self.pnl_manager);
                                    if let Some(trade_pnl) = self.pnl_manager.on_resolve(&condition_id, &side, payout) {
                                        let win_lose = if payout > Decimal::ZERO { "WON" } else { "LOST" };
                                        let pnl_time = Local::now().format("%H:%M:%S").to_string();
                                        let pnl_msg = format!("{} {} {} P&L: ${:+.2} (payout: ${:.2})", pnl_time, slug, win_lose, trade_pnl, payout);
                                        if let Some(ref tx) = self.tui_tx {
                                            let _ = tx.try_send(TuiEvent::TradeLog(pnl_msg));
                                        }
                                    }
                                    if let Some(ref tx) = self.tui_tx {
                                        let _ = tx.try_send(TuiEvent::TradeLog(format!("{} {} REDEEM success: ${:.2}", Local::now().format("%H:%M:%S"), slug, payout)));
                                    }
                                } else {
                                    remaining.push((*pos).clone());
                                }
                            } else {
                                remaining.push((*pos).clone());
                            }
                        }
                        info!(batch_succeeded = batch_results.values().filter(|&&v| v).count(), batch_failed = batch_results.values().filter(|&&v| !v).count(), "Batch redeem complete");
                        // Fall through to single redeem for remaining
                        valid = remaining;
                    }

                    // Single redeem per position (EOA or multicall/batch fallback)
                    for pos in valid {
                        let condition_id = pos.condition_id.clone();
                        let slug = pos.slug.clone();
                        let payout = Decimal::try_from(pos.current_value)
                            .unwrap_or_else(|_| Decimal::from_f64_retain(pos.current_value).unwrap_or(Decimal::ZERO));

                        let side = side_label_for_redeemed_position(&pos, &self.pnl_manager);

                        match self.redeem_position_ctf(&condition_id, &slug, payout).await {
                            Ok(()) => {
                                retry_count.remove(&condition_id);
                                // When position RESOLVES: Calculate P&L for that trade
                                // If won: P&L += payout - cost
                                // If lost: P&L += 0 - cost
                                // Invested -= cost
                                // Open Pos -= 1
                                // Trades += 1
                                // Update Win rate
                                if let Some(trade_pnl) = self.pnl_manager.on_resolve(&condition_id, &side, payout) {
                                    let win_lose = if payout > Decimal::ZERO { "WON" } else { "LOST" };
                                    let pnl_time = Local::now().format("%H:%M:%S").to_string();
                                    let pnl_msg = format!(
                                        "{} {} {} P&L: ${:+.2} (payout: ${:.2})",
                                        pnl_time, slug, win_lose, trade_pnl, payout
                                    );
                                    
                                    // Send P&L update to TUI
                                    if let Some(ref tx) = self.tui_tx {
                                        let _ = tx.try_send(TuiEvent::TradeLog(pnl_msg));
                                    }
                                    
                                    info!(
                                        market = %slug,
                                        condition_id = %condition_id,
                                        side = %side,
                                        payout = %payout,
                                        trade_pnl = %trade_pnl,
                                        "Position resolved, P&L calculated"
                                    );
                                } else {
                                    // Position not found in open positions - might have been resolved already or not tracked
                                    tracing::warn!(
                                        market = %slug,
                                        condition_id = %condition_id,
                                        "Position resolved but not found in open positions (may have been resolved already)"
                                    );
                                }

                                // Success message already sent in redeem_position_ctf
                                info!(
                                    market = %slug,
                                    payout = %payout,
                                    "Redeemed position"
                                );
                            }
                            Err(e) => {
                                let count = retry_count.entry(condition_id.clone()).or_insert(0);
                                *count = count.saturating_add(1);
                                let attempts = *count;
                                let fail_time = Local::now().format("%H:%M:%S").to_string();
                                let fail_msg = format!("{} {} REDEEM failed: ${:.2} — {:#} (attempt {}/{})", fail_time, slug, payout, e, attempts, MAX_RETRIES_PER_POSITION);

                                // Send redemption failure to TUI (full error chain)
                                if let Some(ref tx) = self.tui_tx {
                                    let _ = tx.try_send(TuiEvent::TradeLog(fail_msg));
                                }

                                if attempts >= MAX_RETRIES_PER_POSITION {
                                    tracing::warn!(
                                        market = %slug,
                                        condition_id = %condition_id,
                                        error = %e,
                                        error_chain = %format!("{:#}", e),
                                        "Failed to redeem position after {} attempts, giving up",
                                        MAX_RETRIES_PER_POSITION
                                    );
                                } else {
                                    tracing::warn!(
                                        market = %slug,
                                        condition_id = %condition_id,
                                        error_chain = %format!("{:#}", e),
                                        attempt = attempts,
                                        max = MAX_RETRIES_PER_POSITION,
                                        "Failed to redeem position, will retry next cycle"
                                    );
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Failed to fetch redeemable positions, will retry next cycle"
                    );
                }
            }

            sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
        }

        Ok(())
    }
}
