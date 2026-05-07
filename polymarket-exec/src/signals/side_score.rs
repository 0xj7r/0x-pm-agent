//! Composite side score for paired-MM diagnostics and overlay sizing.
//!
//! This combines fair value, BTC momentum, order-book pressure, terminal
//! timing, reversal risk, and book sanity into a bounded side preference.
//! It does not emit orders and should not become a hidden gate.

use crate::market_making::pairing::types::{LadderLeg, PairedMarketSnapshot};

use super::{
    BookSanitySignal, FairValueEstimate, MomentumSignal, OrderBookPressureSignal, ReversalSignal,
    SignalDirection,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SideScoreConfig {
    pub fair_value_weight: f64,
    pub momentum_weight: f64,
    pub orderflow_weight: f64,
    pub terminal_timing_weight: f64,
    pub reversal_risk_weight: f64,
    pub book_sanity_weight: f64,
    pub max_late_convex_tilt: f64,
}

impl Default for SideScoreConfig {
    fn default() -> Self {
        Self {
            fair_value_weight: 0.35,
            momentum_weight: 0.25,
            orderflow_weight: 0.20,
            terminal_timing_weight: 0.15,
            reversal_risk_weight: 0.15,
            book_sanity_weight: 0.10,
            max_late_convex_tilt: 0.80,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SideScoreLeg {
    pub score: f64,
    pub fair_value_component: f64,
    pub momentum_component: f64,
    pub orderflow_component: f64,
    pub terminal_timing_component: f64,
    pub reversal_component: f64,
    pub book_sanity_penalty: f64,
    pub late_convex_scale: f64,
}

impl Default for SideScoreLeg {
    fn default() -> Self {
        Self {
            score: 0.0,
            fair_value_component: 0.0,
            momentum_component: 0.0,
            orderflow_component: 0.0,
            terminal_timing_component: 0.0,
            reversal_component: 0.0,
            book_sanity_penalty: 0.0,
            late_convex_scale: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SideScoreSignal {
    pub yes: SideScoreLeg,
    pub no: SideScoreLeg,
    pub favorite_leg: Option<LadderLeg>,
    pub confidence: f64,
}

impl Default for SideScoreSignal {
    fn default() -> Self {
        Self {
            yes: SideScoreLeg::default(),
            no: SideScoreLeg::default(),
            favorite_leg: None,
            confidence: 0.0,
        }
    }
}

impl SideScoreSignal {
    #[allow(clippy::too_many_arguments)]
    pub fn compute(
        fair_value: &FairValueEstimate,
        snapshot: &PairedMarketSnapshot,
        momentum: &MomentumSignal,
        order_book_pressure: &OrderBookPressureSignal,
        reversal: &ReversalSignal,
        book_sanity: &BookSanitySignal,
        remaining_ms: u64,
        window_ms: u64,
        config: SideScoreConfig,
    ) -> Self {
        let yes = leg_score(
            LadderLeg::Yes,
            fair_value,
            snapshot,
            momentum,
            order_book_pressure,
            reversal,
            book_sanity,
            remaining_ms,
            window_ms,
            config,
        );
        let no = leg_score(
            LadderLeg::No,
            fair_value,
            snapshot,
            momentum,
            order_book_pressure,
            reversal,
            book_sanity,
            remaining_ms,
            window_ms,
            config,
        );
        let confidence = (yes.score - no.score).abs().clamp(0.0, 1.0);
        let favorite_leg = if yes.score > no.score {
            Some(LadderLeg::Yes)
        } else if no.score > yes.score {
            Some(LadderLeg::No)
        } else {
            None
        };
        Self {
            yes,
            no,
            favorite_leg,
            confidence,
        }
    }

    pub fn leg(self, leg: LadderLeg) -> SideScoreLeg {
        match leg {
            LadderLeg::Yes => self.yes,
            LadderLeg::No => self.no,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn leg_score(
    leg: LadderLeg,
    fair_value: &FairValueEstimate,
    snapshot: &PairedMarketSnapshot,
    momentum: &MomentumSignal,
    order_book_pressure: &OrderBookPressureSignal,
    reversal: &ReversalSignal,
    book_sanity: &BookSanitySignal,
    remaining_ms: u64,
    window_ms: u64,
    config: SideScoreConfig,
) -> SideScoreLeg {
    let fair_value_component = fair_value_component(leg, fair_value, snapshot);
    let momentum_component = directional_component(leg, momentum.direction, momentum.strength);
    let orderflow_component = directional_component(
        leg,
        order_book_pressure.direction,
        order_book_pressure.imbalance.abs().clamp(0.0, 1.0),
    );
    let terminal_timing_component =
        terminal_timing_component(leg, fair_value, remaining_ms, window_ms);
    let reversal_component = reversal.support_for_leg(leg);
    let book_sanity_penalty = book_sanity.penalty_for_leg(leg).clamp(0.0, 1.0);
    let weight_sum = config.fair_value_weight.max(0.0)
        + config.momentum_weight.max(0.0)
        + config.orderflow_weight.max(0.0)
        + config.terminal_timing_weight.max(0.0)
        + config.reversal_risk_weight.max(0.0)
        + config.book_sanity_weight.max(0.0);
    let raw_score = if weight_sum > 0.0 {
        ((config.fair_value_weight.max(0.0) * fair_value_component)
            + (config.momentum_weight.max(0.0) * momentum_component)
            + (config.orderflow_weight.max(0.0) * orderflow_component)
            + (config.terminal_timing_weight.max(0.0) * terminal_timing_component)
            + (config.reversal_risk_weight.max(0.0) * reversal_component)
            - (config.book_sanity_weight.max(0.0) * book_sanity_penalty))
            / weight_sum
    } else {
        0.0
    };
    let score = raw_score.clamp(-1.0, 1.0);
    let convex_tilt = score.clamp(-config.max_late_convex_tilt, config.max_late_convex_tilt);

    SideScoreLeg {
        score,
        fair_value_component,
        momentum_component,
        orderflow_component,
        terminal_timing_component,
        reversal_component,
        book_sanity_penalty,
        late_convex_scale: (1.0 + convex_tilt).clamp(0.10, 2.5),
    }
}

fn fair_value_component(
    leg: LadderLeg,
    fair_value: &FairValueEstimate,
    snapshot: &PairedMarketSnapshot,
) -> f64 {
    let (prob, mid) = match leg {
        LadderLeg::Yes => (fair_value.p_up, snapshot.yes_quote.mid_price()),
        LadderLeg::No => (fair_value.p_down, snapshot.no_quote.mid_price()),
    };
    if !prob.is_finite() {
        return 0.0;
    }
    let reference = mid.unwrap_or(0.5).clamp(0.01, 0.99);
    ((prob - reference) / 0.25).clamp(-1.0, 1.0)
}

fn directional_component(leg: LadderLeg, direction: SignalDirection, strength: f64) -> f64 {
    let strength = strength.clamp(0.0, 1.0);
    match (leg, direction) {
        (LadderLeg::Yes, SignalDirection::Up) | (LadderLeg::No, SignalDirection::Down) => strength,
        (LadderLeg::Yes, SignalDirection::Down) | (LadderLeg::No, SignalDirection::Up) => -strength,
        _ => 0.0,
    }
}

fn terminal_timing_component(
    leg: LadderLeg,
    fair_value: &FairValueEstimate,
    remaining_ms: u64,
    window_ms: u64,
) -> f64 {
    let prob = match leg {
        LadderLeg::Yes => fair_value.p_up,
        LadderLeg::No => fair_value.p_down,
    };
    if !prob.is_finite() || window_ms == 0 {
        return 0.0;
    }
    let late_frac =
        (1.0 - (remaining_ms as f64 / window_ms as f64).clamp(0.0, 1.0)).clamp(0.0, 1.0);
    ((prob - 0.5) * 2.0 * late_frac).clamp(-1.0, 1.0)
}
