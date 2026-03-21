//! Shared position sizing for strategies that use `SNIPER_*` deploy parameters.

use anyhow::{bail, Result};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct SingleLegSizing {
    pub balance: Decimal,
    pub deploy_pct: Decimal,
    pub price: Decimal,
    pub calc_shares: Decimal,
    pub min_shares: Decimal,
    pub max_shares: Decimal,
    pub final_shares: Decimal,
}

#[derive(Debug, Clone)]
pub struct DualLegSizing {
    pub balance: Decimal,
    pub deploy_pct: Decimal,
    pub sum_px: Decimal,
    pub calc_per_leg: Decimal,
    pub min_shares: Decimal,
    pub max_per_leg: Decimal,
    pub final_per_leg: Decimal,
}

/// `max(floor(balance * capital_deploy_pct / price), min_shares)`, capped at `max_shares`.
pub fn compute_single_leg_sizing(
    balance: Decimal,
    price: Decimal,
    max_shares: Decimal,
    deploy_pct: Decimal,
    min_shares: Decimal,
) -> Result<SingleLegSizing> {
    if price.is_zero() {
        bail!("sizing: zero price");
    }

    let budget = balance * deploy_pct;
    let calc_shares = (budget / price).floor().round_dp(0);
    let final_shares = calc_shares.max(min_shares).min(max_shares);

    let cost = final_shares * price;
    if cost > balance {
        bail!(
            "Balance {} cannot afford size {} @ {} (need {})",
            balance,
            final_shares,
            price,
            cost
        );
    }

    Ok(SingleLegSizing {
        balance,
        deploy_pct,
        price,
        calc_shares,
        min_shares,
        max_shares,
        final_shares,
    })
}

/// Per-leg size when resting on both YES and NO: deploy `balance * deploy_pct` across the pair.
pub fn compute_per_leg_dual(
    balance: Decimal,
    maker_yes: Decimal,
    maker_no: Decimal,
    max_shares: Decimal,
    deploy_pct: Decimal,
    min_shares: Decimal,
) -> Result<DualLegSizing> {
    let sum_px = maker_yes + maker_no;
    if sum_px.is_zero() {
        bail!("dual sizing: zero combined maker price");
    }

    let max_per_leg = (max_shares / dec!(2)).round_dp(2);
    if max_per_leg < min_shares {
        bail!(
            "dual sizing: max_shares/2 ({}) < min_shares ({})",
            max_per_leg,
            min_shares
        );
    }

    let budget = balance * deploy_pct;
    let calc_per_leg = (budget / sum_px).floor().round_dp(0);
    let mut per_leg = calc_per_leg.max(min_shares);
    if per_leg > max_per_leg {
        bail!(
            "dual sizing: per-leg {} exceeds half of max_shares ({})",
            per_leg,
            max_per_leg
        );
    }

    let cost = per_leg * sum_px;
    if cost > balance {
        bail!(
            "Balance {} cannot afford dual per-leg {} at combined px {} (need {})",
            balance,
            per_leg,
            sum_px,
            cost
        );
    }

    let per_str = format!("{:.2}", per_leg);
    per_leg = Decimal::from_str(&per_str).unwrap_or(per_leg);
    Ok(DualLegSizing {
        balance,
        deploy_pct,
        sum_px,
        calc_per_leg,
        min_shares,
        max_per_leg,
        final_per_leg: per_leg,
    })
}
