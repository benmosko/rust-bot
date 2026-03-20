use crate::config::Config;
use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

pub struct RiskManager {
    config: Arc<Config>,
    daily_start_balance: Arc<RwLock<Decimal>>,
    daily_pnl: Arc<RwLock<Decimal>>,
    consecutive_sniper_losses: Arc<RwLock<u32>>,
    sniper_paused_until: Arc<RwLock<Option<DateTime<Utc>>>>,
    volatility_paused_until: Arc<RwLock<Option<DateTime<Utc>>>>,
    active_rounds: Arc<RwLock<usize>>,
    recent_spot_changes: Arc<RwLock<VecDeque<(Decimal, DateTime<Utc>)>>>,
}

impl RiskManager {
    pub fn new(config: Arc<Config>, start_balance: Decimal) -> Self {
        Self {
            config: config.clone(),
            daily_start_balance: Arc::new(RwLock::new(start_balance)),
            daily_pnl: Arc::new(RwLock::new(Decimal::ZERO)),
            consecutive_sniper_losses: Arc::new(RwLock::new(0)),
            sniper_paused_until: Arc::new(RwLock::new(None)),
            volatility_paused_until: Arc::new(RwLock::new(None)),
            active_rounds: Arc::new(RwLock::new(0)),
            recent_spot_changes: Arc::new(RwLock::new(VecDeque::new())),
        }
    }

    pub async fn can_open_new_round(&self) -> bool {
        let active = *self.active_rounds.read().await;
        active < self.config.max_concurrent_rounds
    }

    pub async fn increment_active_rounds(&self) {
        let mut active = self.active_rounds.write().await;
        *active += 1;
    }

    pub async fn decrement_active_rounds(&self) {
        let mut active = self.active_rounds.write().await;
        if *active > 0 {
            *active -= 1;
        }
    }

    pub async fn check_daily_loss_limit(&self, current_balance: Decimal) -> Result<bool> {
        let start = *self.daily_start_balance.read().await;
        let pnl = current_balance - start;
        let loss_pct = if start > Decimal::ZERO {
            -pnl / start
        } else {
            Decimal::ZERO
        };

        if loss_pct > self.config.daily_loss_limit_pct {
            warn!(
                loss_pct = %loss_pct,
                daily_loss_limit = %self.config.daily_loss_limit_pct,
                "Daily loss limit exceeded"
            );
            return Ok(false);
        }

        Ok(true)
    }

    pub async fn record_sniper_loss(&self) {
        let mut losses = self.consecutive_sniper_losses.write().await;
        *losses += 1;

        if *losses >= self.config.circuit_breaker_consecutive_losses {
            let pause_until = Utc::now()
                + chrono::Duration::seconds(self.config.circuit_breaker_pause_secs as i64);
            *self.sniper_paused_until.write().await = Some(pause_until);
            warn!(
                consecutive_losses = *losses,
                pause_until = ?pause_until,
                "Sniper mode paused due to consecutive losses"
            );
        }
    }

    pub async fn record_sniper_win(&self) {
        let mut losses = self.consecutive_sniper_losses.write().await;
        *losses = 0;
    }

    pub async fn is_sniper_paused(&self) -> bool {
        let paused_until = self.sniper_paused_until.read().await;
        if let Some(until) = *paused_until {
            if Utc::now() < until {
                return true;
            } else {
                // Pause expired
                drop(paused_until);
                *self.sniper_paused_until.write().await = None;
            }
        }
        false
    }

    pub async fn check_round_exposure(&self, round_exposure: Decimal, total_balance: Decimal) -> bool {
        if total_balance <= Decimal::ZERO {
            return false;
        }
        let exposure_pct = round_exposure / total_balance;
        exposure_pct <= self.config.max_round_exposure_pct
    }

    pub async fn record_spot_change(&self, price_change_pct: Decimal) {
        let mut changes = self.recent_spot_changes.write().await;
        changes.push_back((price_change_pct, Utc::now()));
        
        // Keep only last 5 minutes
        let cutoff = Utc::now() - chrono::Duration::minutes(5);
        while changes.front().map(|(_, ts)| *ts < cutoff).unwrap_or(false) {
            changes.pop_front();
        }
    }

    pub async fn check_volatility_pause(&self) -> bool {
        let paused_until = self.volatility_paused_until.read().await;
        if let Some(until) = *paused_until {
            if Utc::now() < until {
                return true;
            } else {
                drop(paused_until);
                *self.volatility_paused_until.write().await = None;
            }
        }

        // Check recent volatility
        let changes = self.recent_spot_changes.read().await;
        if changes.len() < 2 {
            return false;
        }

        let max_change = changes
            .iter()
            .map(|(pct, _)| pct.abs())
            .max()
            .unwrap_or(Decimal::ZERO);

        if max_change > self.config.volatility_pause_threshold_pct {
            let pause_until = Utc::now()
                + chrono::Duration::seconds(self.config.volatility_pause_secs as i64);
            drop(changes);
            *self.volatility_paused_until.write().await = Some(pause_until);
            warn!(
                max_change = %max_change,
                pause_until = ?pause_until,
                "Volatility pause triggered"
            );
            return true;
        }

        false
    }

    pub async fn update_daily_pnl(&self, current_balance: Decimal) {
        let start = *self.daily_start_balance.read().await;
        let pnl = current_balance - start;
        *self.daily_pnl.write().await = pnl;
    }
}
