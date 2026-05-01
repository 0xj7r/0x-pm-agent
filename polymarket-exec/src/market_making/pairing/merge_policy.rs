//! Merge-first policy for paired inventory.
//!
//! Merge/redeem is the cleanest exit for paired YES/NO inventory. This policy
//! decides whether a merge is worth emitting now based on paired quantity,
//! notional, expected gain, gas, and batching thresholds.

use crate::market_making::pairing::pair_ledger::MergeCandidate;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MergePolicyConfig {
    pub min_merge_notional_usd: f64,
    pub min_expected_gain_usd: f64,
    pub max_gas_fee_usd: f64,
    pub batch_qty_threshold: f64,
}

impl Default for MergePolicyConfig {
    fn default() -> Self {
        Self {
            min_merge_notional_usd: 2.0,
            min_expected_gain_usd: 0.0,
            max_gas_fee_usd: 1.0,
            batch_qty_threshold: 3.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MergePolicyDecision {
    MergeNow { quantity: f64, reason: String },
    Wait { reason: String },
}

pub fn choose_merge(
    candidate: Option<&MergeCandidate>,
    config: MergePolicyConfig,
) -> MergePolicyDecision {
    let Some(candidate) = candidate else {
        return MergePolicyDecision::Wait {
            reason: "merge wait: no paired inventory".to_string(),
        };
    };

    if candidate.paired_qty <= 1e-9 {
        return MergePolicyDecision::Wait {
            reason: "merge wait: paired quantity is zero".to_string(),
        };
    }

    if candidate.expected_cash_usd < config.min_merge_notional_usd {
        return MergePolicyDecision::Wait {
            reason: format!(
                "merge wait: notional {:.4} below min {:.4}",
                candidate.expected_cash_usd, config.min_merge_notional_usd
            ),
        };
    }

    if candidate.expected_gas_usd > config.max_gas_fee_usd
        && candidate.paired_qty < config.batch_qty_threshold
    {
        return MergePolicyDecision::Wait {
            reason: format!(
                "merge wait: gas {:.4} above max {:.4} and qty {:.4} below batch threshold {:.4}",
                candidate.expected_gas_usd,
                config.max_gas_fee_usd,
                candidate.paired_qty,
                config.batch_qty_threshold
            ),
        };
    }

    if candidate.expected_net_gain_usd < config.min_expected_gain_usd {
        return MergePolicyDecision::Wait {
            reason: format!(
                "merge wait: expected gain {:.4} below min {:.4}",
                candidate.expected_net_gain_usd, config.min_expected_gain_usd
            ),
        };
    }

    MergePolicyDecision::MergeNow {
        quantity: candidate.paired_qty,
        reason: format!(
            "merge now: qty {:.4} expected_net_gain {:.4}",
            candidate.paired_qty, candidate.expected_net_gain_usd
        ),
    }
}
