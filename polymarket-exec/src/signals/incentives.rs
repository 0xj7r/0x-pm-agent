//! Polymarket liquidity reward / rebate context.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IncentiveSignal {
    pub min_incentive_size: Option<f64>,
    pub max_incentive_spread: Option<f64>,
    pub maker_rebate_bps: Option<f64>,
    pub estimated_reward_edge_bps: Option<f64>,
}

impl IncentiveSignal {
    pub fn min_reward_quantity(self) -> Option<f64> {
        self.min_incentive_size
            .filter(|value| value.is_finite() && *value > 0.0)
    }

    pub fn reward_edge(self) -> f64 {
        let rebate = self.maker_rebate_bps.unwrap_or_default().max(0.0);
        let reward = self.estimated_reward_edge_bps.unwrap_or_default().max(0.0);
        rebate + reward
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RewardScoringState {
    pub scoring_orders: u32,
    pub non_scoring_orders: u32,
    pub two_sided_score_eligible: bool,
    pub observed_at_ms: u64,
}
