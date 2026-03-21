//! USDC balance query via alloy: balanceOf(address) on Polygon USDC contract.

use anyhow::{Context, Result};
use alloy::providers::ProviderBuilder;
use polymarket_client_sdk::types::Address;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::str::FromStr as _;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

const USDC_POLYGON: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";

// ERC20 ABI (just balanceOf)
alloy::sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    contract ERC20 {
        function balanceOf(address account) external view returns (uint256);
    }
}

pub struct BalanceManager {
    balance_sender: watch::Sender<Decimal>,
    balance_receiver: watch::Receiver<Decimal>,
    wallet_address: Address,
    rpc_url: String,
}

/// Read USDC `balanceOf` for `wallet` on Polygon (same path as [`BalanceManager::refresh_balance`]).
pub async fn fetch_usdc_balance(rpc_url: &str, wallet: Address) -> Result<Decimal> {
    let rpc_url: url::Url = rpc_url.parse()?;
    let provider = ProviderBuilder::new().connect_http(rpc_url);
    let provider = Arc::new(provider);

    let usdc_address = Address::from_str(USDC_POLYGON)?;
    let contract = ERC20::new(usdc_address, &*provider);
    let call = contract.balanceOf(wallet);
    let result = call.call().await.context("Failed to call balanceOf")?;
    let balance_u128 = result.to::<u128>();
    Ok(Decimal::from(balance_u128) / dec!(1_000_000))
}

impl BalanceManager {
    pub fn new(wallet_address: Address, rpc_url: String) -> Self {
        let (sender, receiver) = watch::channel(dec!(0));
        Self {
            balance_sender: sender,
            balance_receiver: receiver,
            wallet_address,
            rpc_url,
        }
    }

    /// Get a receiver for balance updates.
    pub fn receiver(&self) -> watch::Receiver<Decimal> {
        self.balance_receiver.clone()
    }

    /// Get current balance (from watch channel).
    #[allow(dead_code)]
    pub async fn get_balance(&self) -> Decimal {
        *self.balance_receiver.borrow()
    }

    /// Refresh balance from on-chain USDC contract.
    pub async fn refresh_balance(&self) -> Result<()> {
        let balance_decimal = fetch_usdc_balance(&self.rpc_url, self.wallet_address).await?;

        // Update watch channel
        if self.balance_sender.send(balance_decimal).is_err() {
            error!("Balance watch channel closed");
        } else {
            info!(balance = %balance_decimal, address = %self.wallet_address, "USDC balance refreshed");
        }

        Ok(())
    }

    /// Background loop: refresh balance periodically (fills also trigger immediate refresh via PnLManager).
    pub async fn run(
        &self,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        info!("Balance manager started");

        // Initial refresh
        if let Err(e) = self.refresh_balance().await {
            error!(error = %e, "Initial balance refresh failed");
        }

        loop {
            if shutdown.is_cancelled() {
                break;
            }

            // Shorter interval so the UI stays reasonably fresh even without fill events
            sleep(Duration::from_secs(10)).await;

            if let Err(e) = self.refresh_balance().await {
                error!(error = %e, "Failed to refresh balance");
            }
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub async fn log_pnl(&self, daily_start_balance: Decimal) {
        let current = self.get_balance().await;
        let pnl = current - daily_start_balance;
        let pnl_pct = if daily_start_balance > dec!(0) {
            (pnl / daily_start_balance) * dec!(100)
        } else {
            dec!(0)
        };

        info!(
            balance = %current,
            daily_pnl = %pnl,
            daily_pnl_pct = %pnl_pct,
            "P&L snapshot"
        );
    }
}
