//! Agreement layer between the book-picked favorite and short-horizon signals.
//!
//! The output is intentionally a sizing/risk overlay, not a standalone
//! direction picker. Positive component scores support the favorite leg already
//! selected by the book; negative scores warn that flow, depth, or BTC momentum
//! are leaning against it.

use crate::market_making::pairing::types::LadderLeg;

use super::{MomentumSignal, OrderBookPressureSignal};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BookModelAgreement {
    pub flow_score: f64,
    pub depth_score: f64,
    pub momentum_score: f64,
    pub toxicity_score: f64,
    pub agreement: f64,
    pub signal_confidence: f64,
}

impl Default for BookModelAgreement {
    fn default() -> Self {
        Self {
            flow_score: 0.0,
            depth_score: 0.0,
            momentum_score: 0.0,
            toxicity_score: 0.0,
            agreement: 1.0,
            signal_confidence: 0.0,
        }
    }
}

impl BookModelAgreement {
    pub fn compute(
        favorite_leg: LadderLeg,
        pressure: &OrderBookPressureSignal,
        momentum: &MomentumSignal,
    ) -> Self {
        let flow = flow_score_for_favorite(favorite_leg, pressure);
        let depth = depth_score_for_favorite(favorite_leg, pressure);
        let momentum_score = momentum_score_for_favorite(favorite_leg, momentum);

        let mut weighted = 0.0;
        let mut weight_sum = 0.0;
        if let Some(score) = flow {
            weighted += score * 0.40;
            weight_sum += 0.40;
        }
        if let Some(score) = depth {
            weighted += score * 0.35;
            weight_sum += 0.35;
        }
        if let Some(score) = momentum_score {
            weighted += score * 0.25;
            weight_sum += 0.25;
        }

        if weight_sum <= 0.0 {
            return Self::default();
        }

        let signed_agreement = (weighted / weight_sum).clamp(-1.0, 1.0);
        let agreement = ((signed_agreement + 1.0) * 0.5).clamp(0.0, 1.0);
        let signal_confidence = weight_sum.clamp(0.0, 1.0);
        let component_min = [flow, depth, momentum_score]
            .into_iter()
            .flatten()
            .fold(1.0_f64, f64::min);
        let component_max = [flow, depth, momentum_score]
            .into_iter()
            .flatten()
            .fold(-1.0_f64, f64::max);
        let dispersion = ((component_max - component_min) * 0.5).clamp(0.0, 1.0);
        let against = (-signed_agreement).clamp(0.0, 1.0);
        let thin_book_component = if pressure.thin_book { 0.15 } else { 0.0 };
        let toxicity_score = (0.65 * against + 0.20 * dispersion + thin_book_component)
            .clamp(0.0, 1.0)
            * signal_confidence;

        Self {
            flow_score: flow.unwrap_or(0.0),
            depth_score: depth.unwrap_or(0.0),
            momentum_score: momentum_score.unwrap_or(0.0),
            toxicity_score,
            agreement,
            signal_confidence,
        }
    }

    pub fn kelly_multiplier(&self) -> f64 {
        if self.signal_confidence <= 0.0 {
            return 1.0;
        }
        ((1.0 - self.toxicity_score * 0.70) * (0.60 + 0.40 * self.agreement)).clamp(0.05, 1.0)
    }

    pub fn path_reversal_adder(&self) -> f64 {
        if self.signal_confidence <= 0.0 {
            return 0.0;
        }
        (self.toxicity_score * 0.35 + (1.0 - self.agreement) * 0.25).clamp(0.0, 0.45)
    }
}

fn flow_score_for_favorite(
    favorite_leg: LadderLeg,
    pressure: &OrderBookPressureSignal,
) -> Option<f64> {
    let yes_net = pressure.yes_taker_buy_qty_60s - pressure.yes_taker_sell_qty_60s;
    let no_net = pressure.no_taker_buy_qty_60s - pressure.no_taker_sell_qty_60s;
    let total = pressure.yes_taker_buy_qty_60s
        + pressure.yes_taker_sell_qty_60s
        + pressure.no_taker_buy_qty_60s
        + pressure.no_taker_sell_qty_60s;
    if !total.is_finite() || total < 1.0 {
        return None;
    }
    let signed = match favorite_leg {
        LadderLeg::Yes => yes_net - no_net,
        LadderLeg::No => no_net - yes_net,
    };
    Some((signed / total.max(1e-9)).clamp(-1.0, 1.0))
}

fn depth_score_for_favorite(
    favorite_leg: LadderLeg,
    pressure: &OrderBookPressureSignal,
) -> Option<f64> {
    let total = pressure.yes_bid_notional
        + pressure.yes_ask_notional
        + pressure.no_bid_notional
        + pressure.no_ask_notional;
    if !total.is_finite() || total < 1.0 {
        return None;
    }
    let yes_pressure = (pressure.yes_bid_notional + pressure.no_ask_notional)
        - (pressure.no_bid_notional + pressure.yes_ask_notional);
    let signed = match favorite_leg {
        LadderLeg::Yes => yes_pressure,
        LadderLeg::No => -yes_pressure,
    };
    Some((signed / total.max(1e-9)).clamp(-1.0, 1.0))
}

fn momentum_score_for_favorite(favorite_leg: LadderLeg, momentum: &MomentumSignal) -> Option<f64> {
    if !momentum.is_ready() {
        return None;
    }
    let side = match favorite_leg {
        LadderLeg::Yes => 1.0,
        LadderLeg::No => -1.0,
    };
    let score_component = (momentum.score * side).clamp(-1.0, 1.0);
    let latest_component = momentum
        .latest_window_return_bps
        .filter(|v| v.is_finite())
        .map(|v| (v * side / 10.0).clamp(-1.0, 1.0))
        .unwrap_or(0.0);
    let accel_component = momentum
        .acceleration_bps
        .filter(|v| v.is_finite())
        .map(|v| (v * side / 10.0).clamp(-1.0, 1.0))
        .unwrap_or(0.0);

    Some(
        (0.55 * score_component + 0.30 * latest_component + 0.15 * accel_component)
            .clamp(-1.0, 1.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::SignalDirection;

    fn pressure_against_yes() -> OrderBookPressureSignal {
        OrderBookPressureSignal {
            yes_bid_notional: 10.0,
            yes_ask_notional: 120.0,
            no_bid_notional: 110.0,
            no_ask_notional: 10.0,
            yes_taker_buy_qty_60s: 2.0,
            yes_taker_sell_qty_60s: 30.0,
            no_taker_buy_qty_60s: 40.0,
            no_taker_sell_qty_60s: 1.0,
            ..Default::default()
        }
    }

    #[test]
    fn default_is_no_penalty_when_no_microstructure_signal_exists() {
        let agreement = BookModelAgreement::compute(
            LadderLeg::Yes,
            &OrderBookPressureSignal::default(),
            &MomentumSignal::default(),
        );

        assert_eq!(agreement.signal_confidence, 0.0);
        assert_eq!(agreement.kelly_multiplier(), 1.0);
        assert_eq!(agreement.path_reversal_adder(), 0.0);
    }

    #[test]
    fn adverse_flow_depth_and_momentum_reduce_kelly_and_raise_reversal_risk() {
        let agreement = BookModelAgreement::compute(
            LadderLeg::Yes,
            &pressure_against_yes(),
            &MomentumSignal {
                direction: SignalDirection::Down,
                score: -1.0,
                strength: 1.0,
                latest_window_return_bps: Some(-12.0),
                acceleration_bps: Some(-8.0),
                window_returns_bps: vec![-12.0, -4.0],
            },
        );

        assert!(agreement.agreement < 0.35, "{agreement:?}");
        assert!(agreement.toxicity_score > 0.35, "{agreement:?}");
        assert!(agreement.kelly_multiplier() < 0.55, "{agreement:?}");
        assert!(agreement.path_reversal_adder() > 0.20, "{agreement:?}");
    }

    #[test]
    fn supportive_flow_depth_and_momentum_keep_kelly_near_full_size() {
        let pressure = OrderBookPressureSignal {
            yes_bid_notional: 150.0,
            yes_ask_notional: 10.0,
            no_bid_notional: 10.0,
            no_ask_notional: 140.0,
            yes_taker_buy_qty_60s: 40.0,
            yes_taker_sell_qty_60s: 2.0,
            no_taker_buy_qty_60s: 1.0,
            no_taker_sell_qty_60s: 30.0,
            ..Default::default()
        };

        let agreement = BookModelAgreement::compute(
            LadderLeg::Yes,
            &pressure,
            &MomentumSignal {
                direction: SignalDirection::Up,
                score: 1.0,
                strength: 1.0,
                latest_window_return_bps: Some(14.0),
                acceleration_bps: Some(5.0),
                window_returns_bps: vec![14.0, 9.0],
            },
        );

        assert!(agreement.agreement > 0.85, "{agreement:?}");
        assert!(agreement.kelly_multiplier() > 0.90, "{agreement:?}");
        assert!(agreement.path_reversal_adder() < 0.05, "{agreement:?}");
    }
}
