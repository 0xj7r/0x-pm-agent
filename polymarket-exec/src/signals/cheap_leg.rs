//! Cheap-leg signal composition.
//!
//! This module does not place orders. It turns fair value, BTC momentum,
//! book-pressure, and projected pair-cost facts into an auditable signal that
//! strategies can log first, then later gate on.

use crate::market_making::pairing::pair_cost_tracker::Leg;

use super::{FairValueEstimate, MomentumSignal, OrderBookPressureSignal, SignalDirection};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CheapLegConfig {
    pub min_edge_bps: f64,
    pub max_pair_cost: f64,
    pub max_momentum_against: f64,
    pub max_pressure_against: f64,
}

impl Default for CheapLegConfig {
    fn default() -> Self {
        Self {
            min_edge_bps: 35.0,
            max_pair_cost: 0.99,
            max_momentum_against: 0.60,
            max_pressure_against: 0.60,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CheapLegCandidate {
    pub leg: Leg,
    pub mid_price: f64,
    pub fair_price: f64,
    pub projected_pair_cost: f64,
}

impl CheapLegCandidate {
    pub fn edge_bps(&self) -> f64 {
        (self.fair_price - self.mid_price) * 10_000.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CheapLegSignal {
    Buy {
        leg: Leg,
        edge_bps: f64,
        projected_pair_cost: f64,
        reason: String,
    },
    Wait {
        reason: String,
    },
}

impl CheapLegSignal {
    pub fn reason(&self) -> &str {
        match self {
            Self::Buy { reason, .. } | Self::Wait { reason } => reason,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CheapLegSignalEngine {
    config: CheapLegConfig,
}

impl CheapLegSignalEngine {
    pub fn new(config: CheapLegConfig) -> Self {
        Self { config }
    }

    pub fn decide(
        &self,
        fair_value: &FairValueEstimate,
        momentum: &MomentumSignal,
        pressure: &OrderBookPressureSignal,
        yes_projected_pair_cost: Option<f64>,
        yes_mid: Option<f64>,
        no_projected_pair_cost: Option<f64>,
        no_mid: Option<f64>,
    ) -> CheapLegSignal {
        let mut candidates = Vec::with_capacity(2);
        if let (Some(projected_pair_cost), Some(mid_price)) = (yes_projected_pair_cost, yes_mid) {
            candidates.push(CheapLegCandidate {
                leg: Leg::Yes,
                mid_price,
                fair_price: fair_value.p_up,
                projected_pair_cost,
            });
        }
        if let (Some(projected_pair_cost), Some(mid_price)) = (no_projected_pair_cost, no_mid) {
            candidates.push(CheapLegCandidate {
                leg: Leg::No,
                mid_price,
                fair_price: fair_value.p_down,
                projected_pair_cost,
            });
        }
        candidates.sort_by(|left, right| {
            right
                .edge_bps()
                .partial_cmp(&left.edge_bps())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        for candidate in candidates {
            let edge_bps = candidate.edge_bps();
            if !edge_bps.is_finite() || edge_bps < self.config.min_edge_bps {
                continue;
            }
            if candidate.projected_pair_cost > self.config.max_pair_cost {
                continue;
            }
            if momentum_against(candidate.leg, momentum) > self.config.max_momentum_against {
                return CheapLegSignal::Wait {
                    reason: format!(
                        "cheap-leg signal wait: momentum against leg={:?} strength={:.4}",
                        candidate.leg, momentum.strength
                    ),
                };
            }
            if pressure_against(candidate.leg, pressure) > self.config.max_pressure_against {
                return CheapLegSignal::Wait {
                    reason: format!(
                        "cheap-leg signal wait: book pressure against leg={:?} imbalance={:.4}",
                        candidate.leg, pressure.imbalance
                    ),
                };
            }
            return CheapLegSignal::Buy {
                leg: candidate.leg,
                edge_bps,
                projected_pair_cost: candidate.projected_pair_cost,
                reason: format!(
                    "cheap-leg signal buy leg={:?} edge_bps={edge_bps:.2} projected_pair_cost={:.4}",
                    candidate.leg, candidate.projected_pair_cost
                ),
            };
        }

        CheapLegSignal::Wait {
            reason: "cheap-leg signal wait: no candidate passed edge/pair-cost gates".to_string(),
        }
    }
}

impl Default for CheapLegSignalEngine {
    fn default() -> Self {
        Self::new(CheapLegConfig::default())
    }
}

fn momentum_against(leg: Leg, momentum: &MomentumSignal) -> f64 {
    match (leg, momentum.direction) {
        (Leg::Yes, SignalDirection::Down) | (Leg::No, SignalDirection::Up) => momentum.strength,
        _ => 0.0,
    }
}

fn pressure_against(leg: Leg, pressure: &OrderBookPressureSignal) -> f64 {
    match (leg, pressure.direction) {
        (Leg::Yes, SignalDirection::Down) | (Leg::No, SignalDirection::Up) => {
            pressure.imbalance.abs()
        }
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::{FairValueModel, MomentumSignal, OrderBookPressureSignal};

    fn fair(p_up: f64) -> FairValueEstimate {
        FairValueEstimate {
            p_up,
            p_down: 1.0 - p_up,
            log_moneyness: 0.0,
            sigma_remaining: 0.0,
            time_remaining_s: 120.0,
            model: FairValueModel::BsmBinary,
        }
    }

    #[test]
    fn buys_highest_edge_leg_when_pair_cost_allows() {
        let signal = CheapLegSignalEngine::default().decide(
            &fair(0.60),
            &MomentumSignal::default(),
            &OrderBookPressureSignal::default(),
            Some(0.96),
            Some(0.54),
            Some(0.98),
            Some(0.42),
        );

        assert!(matches!(
            signal,
            CheapLegSignal::Buy {
                leg: Leg::Yes,
                projected_pair_cost: 0.96,
                ..
            }
        ));
    }

    #[test]
    fn waits_when_strong_btc_momentum_is_against_candidate() {
        let signal = CheapLegSignalEngine::default().decide(
            &fair(0.60),
            &MomentumSignal {
                direction: SignalDirection::Down,
                score: -1.0,
                strength: 1.0,
                ..Default::default()
            },
            &OrderBookPressureSignal::default(),
            Some(0.96),
            Some(0.54),
            None,
            None,
        );

        assert!(matches!(signal, CheapLegSignal::Wait { .. }));
        assert!(signal.reason().contains("momentum against"));
    }
}
