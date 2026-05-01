//! EV-gated rescue decisions for stranded one-sided inventory.

use crate::market_making::paired_mm::types::LadderLeg;
use crate::types::OrderIntent;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RescueConfig {
    pub min_edge_bps: f64,
    pub max_rescue_qty: f64,
    pub allow_sell_fallback: bool,
    pub require_no_guaranteed_loss: bool,
}

impl Default for RescueConfig {
    fn default() -> Self {
        Self {
            min_edge_bps: 25.0,
            max_rescue_qty: 50.0,
            allow_sell_fallback: true,
            require_no_guaranteed_loss: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RescueInputs {
    pub leg: LadderLeg,
    pub stranded_qty: f64,
    pub avg_cost: f64,
    pub fair_win_prob: f64,
    pub best_exit_bid: Option<f64>,
    pub opposite_best_ask: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RescueAction {
    Hold,
    BuyOppositeForMerge,
    SellStrandedLeg,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RescueDecision {
    pub action: RescueAction,
    pub qty: f64,
    pub hold_value_per_share: f64,
    pub rescue_value_per_share: Option<f64>,
    pub delta_vs_hold_per_share: Option<f64>,
    pub reason: String,
}

/// Adapter seam for turning an EV rescue decision into venue-ready intents.
///
/// The EV brain is pure and market-agnostic. Concrete strategies own venue
/// details such as depth walking, IOC/FAK limit padding, tick alignment, and
/// reduce-only semantics.
pub trait RescueIntentBuilder {
    fn build_rescue_intents(&self, decision: &RescueDecision) -> Vec<OrderIntent>;
}

pub fn choose_rescue(inputs: RescueInputs, config: RescueConfig) -> RescueDecision {
    if inputs.stranded_qty <= 1e-9 {
        return RescueDecision {
            action: RescueAction::Hold,
            qty: 0.0,
            hold_value_per_share: inputs.fair_win_prob.clamp(0.0, 1.0),
            rescue_value_per_share: None,
            delta_vs_hold_per_share: None,
            reason: "no stranded quantity".to_string(),
        };
    }

    let hold_value = inputs.fair_win_prob.clamp(0.0, 1.0);
    let edge = (config.min_edge_bps / 10_000.0).max(0.0);

    let merge_rescue_value = inputs.opposite_best_ask.and_then(|ask| {
        if ask <= 0.0 || ask >= 1.0 || !ask.is_finite() {
            return None;
        }
        if config.require_no_guaranteed_loss && inputs.avg_cost + ask > 1.0 - edge {
            return None;
        }
        Some(1.0 - ask)
    });

    let sell_value = if config.allow_sell_fallback {
        inputs
            .best_exit_bid
            .filter(|bid| bid.is_finite() && *bid > 0.0 && *bid < 1.0)
    } else {
        None
    };

    let (action, rescue_value) = match (merge_rescue_value, sell_value) {
        (Some(merge), Some(sell)) if sell > merge => (RescueAction::SellStrandedLeg, sell),
        (Some(merge), _) => (RescueAction::BuyOppositeForMerge, merge),
        (None, Some(sell)) => (RescueAction::SellStrandedLeg, sell),
        (None, None) => {
            return RescueDecision {
                action: RescueAction::Hold,
                qty: 0.0,
                hold_value_per_share: hold_value,
                rescue_value_per_share: None,
                delta_vs_hold_per_share: None,
                reason: "no viable rescue path".to_string(),
            };
        }
    };

    let rescue_delta = rescue_value - hold_value;
    if rescue_delta <= edge {
        return RescueDecision {
            action: RescueAction::Hold,
            qty: 0.0,
            hold_value_per_share: hold_value,
            rescue_value_per_share: Some(rescue_value),
            delta_vs_hold_per_share: Some(rescue_delta),
            reason: format!(
                "hold beats rescue hold={hold_value:.4} rescue={rescue_value:.4} edge={edge:.4}"
            ),
        };
    }

    RescueDecision {
        action,
        qty: inputs.stranded_qty.min(config.max_rescue_qty).max(0.0),
        hold_value_per_share: hold_value,
        rescue_value_per_share: Some(rescue_value),
        delta_vs_hold_per_share: Some(rescue_delta),
        reason: format!(
            "rescue beats hold hold={hold_value:.4} rescue={rescue_value:.4} delta={rescue_delta:.4} edge={edge:.4}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sell_rescue_fires_when_exit_bid_beats_model_hold_value() {
        let decision = choose_rescue(
            RescueInputs {
                leg: LadderLeg::Yes,
                stranded_qty: 10.0,
                avg_cost: 0.70,
                fair_win_prob: 0.20,
                best_exit_bid: Some(0.35),
                opposite_best_ask: None,
            },
            RescueConfig {
                min_edge_bps: 50.0,
                max_rescue_qty: 10.0,
                allow_sell_fallback: true,
                require_no_guaranteed_loss: true,
            },
        );
        assert_eq!(decision.action, RescueAction::SellStrandedLeg);
        assert_eq!(decision.qty, 10.0);
        assert!(decision.delta_vs_hold_per_share.unwrap() > 0.0);
    }

    #[test]
    fn hold_when_model_value_beats_exit_bid() {
        let decision = choose_rescue(
            RescueInputs {
                leg: LadderLeg::No,
                stranded_qty: 10.0,
                avg_cost: 0.30,
                fair_win_prob: 0.70,
                best_exit_bid: Some(0.40),
                opposite_best_ask: None,
            },
            RescueConfig::default(),
        );
        assert_eq!(decision.action, RescueAction::Hold);
        assert_eq!(decision.qty, 0.0);
    }

    #[test]
    fn buy_opposite_requires_projected_pair_cost_edge() {
        let decision = choose_rescue(
            RescueInputs {
                leg: LadderLeg::Yes,
                stranded_qty: 10.0,
                avg_cost: 0.45,
                fair_win_prob: 0.30,
                best_exit_bid: None,
                opposite_best_ask: Some(0.40),
            },
            RescueConfig {
                min_edge_bps: 50.0,
                max_rescue_qty: 10.0,
                allow_sell_fallback: false,
                require_no_guaranteed_loss: true,
            },
        );
        assert_eq!(decision.action, RescueAction::BuyOppositeForMerge);
    }
}
