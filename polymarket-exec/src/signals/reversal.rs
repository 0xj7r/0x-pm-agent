//! Reversal risk signal for short-horizon BTC up/down markets.
//!
//! This is not a trade trigger. It quantifies whether the current favourite is
//! vulnerable to a late reversal, so paired-MM and convex overlays can size
//! with more context than raw momentum alone.

use crate::market_making::pairing::types::LadderLeg;

use super::{FairValueEstimate, MomentumSignal, OrderBookPressureSignal, SignalDirection};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReversalConfig {
    pub enabled: bool,
    pub deceleration_weight: f64,
    pub momentum_flip_weight: f64,
    pub distance_weight: f64,
    pub orderflow_weight: f64,
    pub strong_momentum_bps: f64,
    pub flip_deadband_bps: f64,
}

impl Default for ReversalConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            deceleration_weight: 0.35,
            momentum_flip_weight: 0.25,
            distance_weight: 0.20,
            orderflow_weight: 0.20,
            strong_momentum_bps: 8.0,
            flip_deadband_bps: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ReversalSignal {
    pub probability: f64,
    pub favorite_leg: Option<LadderLeg>,
    pub reversal_leg: Option<LadderLeg>,
    pub deceleration_score: f64,
    pub momentum_flip_score: f64,
    pub distance_to_strike_score: f64,
    pub orderflow_against_score: f64,
}

impl ReversalSignal {
    pub fn compute(
        fair_value: &FairValueEstimate,
        momentum: &MomentumSignal,
        order_book_pressure: &OrderBookPressureSignal,
        config: ReversalConfig,
    ) -> Self {
        if !config.enabled {
            return Self::default();
        }

        let favorite_leg = favorite_leg(fair_value);
        let reversal_leg = favorite_leg.map(opposite_leg);
        let deceleration_score = deceleration_score(momentum, config);
        let momentum_flip_score = momentum_flip_score(momentum, config);
        let distance_to_strike_score = distance_to_strike_score(fair_value);
        let orderflow_against_score = orderflow_against_score(favorite_leg, order_book_pressure);
        let weight_sum = config.deceleration_weight.max(0.0)
            + config.momentum_flip_weight.max(0.0)
            + config.distance_weight.max(0.0)
            + config.orderflow_weight.max(0.0);
        let probability = if weight_sum > 0.0 {
            ((config.deceleration_weight.max(0.0) * deceleration_score)
                + (config.momentum_flip_weight.max(0.0) * momentum_flip_score)
                + (config.distance_weight.max(0.0) * distance_to_strike_score)
                + (config.orderflow_weight.max(0.0) * orderflow_against_score))
                / weight_sum
        } else {
            0.0
        }
        .clamp(0.0, 1.0);

        Self {
            probability,
            favorite_leg,
            reversal_leg,
            deceleration_score,
            momentum_flip_score,
            distance_to_strike_score,
            orderflow_against_score,
        }
    }

    pub fn support_for_leg(self, leg: LadderLeg) -> f64 {
        if self.reversal_leg == Some(leg) {
            self.probability
        } else if self.favorite_leg == Some(leg) {
            -self.probability
        } else {
            0.0
        }
    }
}

fn favorite_leg(fair_value: &FairValueEstimate) -> Option<LadderLeg> {
    if !fair_value.p_up.is_finite() || !fair_value.p_down.is_finite() {
        return None;
    }
    if fair_value.p_up > fair_value.p_down {
        Some(LadderLeg::Yes)
    } else if fair_value.p_down > fair_value.p_up {
        Some(LadderLeg::No)
    } else {
        None
    }
}

fn opposite_leg(leg: LadderLeg) -> LadderLeg {
    match leg {
        LadderLeg::Yes => LadderLeg::No,
        LadderLeg::No => LadderLeg::Yes,
    }
}

fn deceleration_score(momentum: &MomentumSignal, config: ReversalConfig) -> f64 {
    let Some(current) = momentum.window_returns_bps.first().copied() else {
        return 0.0;
    };
    let Some(previous) = momentum.window_returns_bps.get(1).copied() else {
        return 0.0;
    };
    let previous_abs = previous.abs();
    if previous_abs < config.strong_momentum_bps.max(1e-9) {
        return 0.0;
    }
    ((previous_abs - current.abs()) / previous_abs).clamp(0.0, 1.0)
}

fn momentum_flip_score(momentum: &MomentumSignal, config: ReversalConfig) -> f64 {
    let Some(current) = momentum.window_returns_bps.first().copied() else {
        return 0.0;
    };
    let Some(previous) = momentum.window_returns_bps.get(1).copied() else {
        return 0.0;
    };
    let deadband = config.flip_deadband_bps.max(0.0);
    let current_dir = SignalDirection::from_signed(current, deadband);
    let previous_dir = SignalDirection::from_signed(previous, deadband);
    if current_dir != SignalDirection::Neutral
        && previous_dir != SignalDirection::Neutral
        && current_dir != previous_dir
    {
        1.0
    } else {
        0.0
    }
}

fn distance_to_strike_score(fair_value: &FairValueEstimate) -> f64 {
    if !fair_value.log_moneyness.is_finite()
        || !fair_value.sigma_remaining.is_finite()
        || fair_value.sigma_remaining <= 0.0
    {
        return 0.0;
    }
    (1.0 - (fair_value.log_moneyness.abs() / fair_value.sigma_remaining).clamp(0.0, 1.0))
        .clamp(0.0, 1.0)
}

fn orderflow_against_score(
    favorite_leg: Option<LadderLeg>,
    pressure: &OrderBookPressureSignal,
) -> f64 {
    let Some(favorite_leg) = favorite_leg else {
        return 0.0;
    };
    match pressure.pressure_leg() {
        Some(pressure_leg) if pressure_leg != favorite_leg => {
            pressure.imbalance.abs().clamp(0.0, 1.0)
        }
        _ => 0.0,
    }
}
