//! Hard paired-MM policy before entry generation.
//!
//! This policy decides whether paired entry is allowed, suppressed, or whether
//! the engine should focus entirely on flattening because end-of-bar risk is
//! too high.

use crate::market_making::pairing::types::PairedInventorySnapshot;
use crate::signals::BtcRegimeSnapshot;
use crate::types::CoolingReason;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HardPolicyConfig {
    pub max_gross_cost_usd: f64,
    pub max_side_imbalance_qty: f64,
    pub end_of_bar_flatten_ms: u64,
    pub max_realized_vol_5m_bps: f64,
    pub max_abs_return_60s_bps: f64,
}

impl Default for HardPolicyConfig {
    fn default() -> Self {
        Self {
            max_gross_cost_usd: 150.0,
            max_side_imbalance_qty: 150.0,
            end_of_bar_flatten_ms: 30_000,
            max_realized_vol_5m_bps: 120.0,
            max_abs_return_60s_bps: 200.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum HardPolicyAction {
    Allow,
    SuppressPaired {
        reason: CoolingReason,
        preserve_quotes: bool,
    },
    ForceFlatten {
        reason: CoolingReason,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct HardPolicyDecision {
    pub action: HardPolicyAction,
    pub notes: Vec<String>,
}

pub fn evaluate_hard_policy(
    inventory: &PairedInventorySnapshot,
    remaining_ms: Option<u64>,
    btc_regime: &BtcRegimeSnapshot,
    config: HardPolicyConfig,
) -> HardPolicyDecision {
    if btc_regime
        .realized_vol_5m_bps
        .is_some_and(|vol| vol.is_finite() && vol > config.max_realized_vol_5m_bps)
    {
        return HardPolicyDecision {
            action: HardPolicyAction::SuppressPaired {
                reason: CoolingReason::BtcTrending,
                preserve_quotes: true,
            },
            notes: vec![format!(
                "paired entry suppressed: realized_vol_5m_bps={:?} above max {:.4}",
                btc_regime.realized_vol_5m_bps, config.max_realized_vol_5m_bps
            )],
        };
    }

    if btc_regime
        .return_60s_bps
        .is_some_and(|ret| ret.is_finite() && ret.abs() > config.max_abs_return_60s_bps)
    {
        return HardPolicyDecision {
            action: HardPolicyAction::SuppressPaired {
                reason: CoolingReason::BtcTrending,
                preserve_quotes: true,
            },
            notes: vec![format!(
                "paired entry suppressed: abs_return_60s_bps={:?} above max {:.4}",
                btc_regime.return_60s_bps, config.max_abs_return_60s_bps
            )],
        };
    }

    let gross_cost_usd = inventory.gross_cost_usd();
    if gross_cost_usd >= config.max_gross_cost_usd {
        return HardPolicyDecision {
            action: HardPolicyAction::SuppressPaired {
                reason: CoolingReason::GrossCostCap,
                preserve_quotes: false,
            },
            notes: vec![format!(
                "gross cost cap reached gross={gross_cost_usd:.4} cap={:.4}",
                config.max_gross_cost_usd
            )],
        };
    }

    let side_imbalance_qty = inventory.side_imbalance_qty();
    if side_imbalance_qty >= config.max_side_imbalance_qty {
        return HardPolicyDecision {
            action: HardPolicyAction::SuppressPaired {
                reason: CoolingReason::SideImbalanceCap,
                preserve_quotes: false,
            },
            notes: vec![format!(
                "side imbalance cap reached imbalance={side_imbalance_qty:.4} cap={:.4}",
                config.max_side_imbalance_qty
            )],
        };
    }

    if remaining_ms.is_some_and(|ms| ms <= config.end_of_bar_flatten_ms)
        && side_imbalance_qty > 1e-9
    {
        return HardPolicyDecision {
            action: HardPolicyAction::ForceFlatten {
                reason: CoolingReason::EndOfBar,
            },
            notes: vec![format!(
                "end-of-bar flatten window remaining_ms={remaining_ms:?} imbalance={side_imbalance_qty:.4}"
            )],
        };
    }

    HardPolicyDecision {
        action: HardPolicyAction::Allow,
        notes: Vec::new(),
    }
}
