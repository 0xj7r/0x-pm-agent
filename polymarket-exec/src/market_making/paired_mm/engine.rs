//! Deep paired-MM interface.
//!
//! Callers should prefer this facade over wiring ladder/rescue modules by hand.
//! It encodes the intended decision order:
//! 1. close/flatten decisions are considered first,
//! 2. entry ladders are generated only when hard risk state permits,
//! 3. all output remains proposed intents for the runtime hard risk boundary.

use crate::market_making::paired_mm::fill_automation::{
    AutoFillConfig, AutoFillDecision, AutoFillState,
};
use crate::market_making::paired_mm::ladder_builder::{
    build_ladder, LadderBuildResult, LadderConfig,
};
use crate::market_making::pairing::capital_recycler::{
    choose_capital_recycle, CapitalRecycleConfig, CapitalRecycleDecision,
};
use crate::market_making::pairing::merge_policy::{
    choose_merge, MergePolicyConfig, MergePolicyDecision,
};
use crate::market_making::pairing::pair_cost_tracker::PairCostTracker;
use crate::market_making::pairing::pair_ledger::MergeCandidate;
use crate::market_making::pairing::rescue_engine::{
    choose_rescue, RescueAction, RescueConfig, RescueDecision, RescueInputs,
};
use crate::market_making::pairing::risk_policy::{
    evaluate_hard_policy, HardPolicyAction, HardPolicyConfig, HardPolicyDecision,
};
use crate::market_making::pairing::types::{PairedInventorySnapshot, PairedMarketSnapshot};
use crate::markets::MarketDescriptor;
use crate::signals::{BtcRegimeSnapshot, FairValueEstimate};
use crate::types::{
    ClientOrderId, CoolingReason, EpochMillis, FillReport, IntentKind, MmQuoteKind, OrderIntent,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConvexityOverlayConfig {
    pub enabled: bool,
    pub late_window_sec: u64,
    pub convex_p_threshold: f64,
    pub max_excess_usd: f64,
    pub clip_usd: f64,
    pub maker_safety_ticks: f64,
}

impl Default for ConvexityOverlayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            late_window_sec: 120,
            convex_p_threshold: 0.72,
            max_excess_usd: 40.0,
            clip_usd: 4.0,
            maker_safety_ticks: 2.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PairedMmEngineConfig {
    pub ladder: LadderConfig,
    pub rescue: RescueConfig,
    pub merge: MergePolicyConfig,
    pub capital_recycle: CapitalRecycleConfig,
    pub hard_policy: HardPolicyConfig,
    pub auto_fill: AutoFillConfig,
    pub convexity_overlay: ConvexityOverlayConfig,
}

impl Default for PairedMmEngineConfig {
    fn default() -> Self {
        Self {
            ladder: LadderConfig::default(),
            rescue: RescueConfig::default(),
            merge: MergePolicyConfig::default(),
            capital_recycle: CapitalRecycleConfig::default(),
            hard_policy: HardPolicyConfig::default(),
            auto_fill: AutoFillConfig::default(),
            convexity_overlay: ConvexityOverlayConfig::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PairedMmInput<M> {
    pub market: M,
    pub snapshot: PairedMarketSnapshot,
    pub inventory: PairedInventorySnapshot,
    pub pair_cost: PairCostTracker,
    pub fair_value: FairValueEstimate,
    pub btc_regime: BtcRegimeSnapshot,
    pub merge_candidate: Option<MergeCandidate>,
    pub rescue: Option<RescueInputs>,
    pub now_ms: EpochMillis,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PairedMmDecision {
    pub ladder: LadderBuildResult,
    pub hard_policy: HardPolicyDecision,
    pub merge: MergePolicyDecision,
    pub capital_recycle: CapitalRecycleDecision,
    pub rescue: Option<RescueDecision>,
    pub convex_overlay: Option<OrderIntent>,
    pub notes: Vec<String>,
}

impl PairedMmDecision {
    pub fn should_emit_merge(&self) -> bool {
        matches!(self.merge, MergePolicyDecision::MergeNow { .. })
    }

    pub fn capital_recycle_intent(&self) -> Option<&crate::types::OrderIntent> {
        match &self.capital_recycle {
            CapitalRecycleDecision::BuyLightSide { intent, .. } => Some(intent),
            CapitalRecycleDecision::Wait { .. } => None,
        }
    }

    pub fn should_emit_rescue(&self) -> bool {
        self.rescue
            .as_ref()
            .is_some_and(|decision| decision.qty > 1e-9 && decision.action != RescueAction::Hold)
    }

    pub fn paired_entry_allowed(&self) -> bool {
        matches!(self.hard_policy.action, HardPolicyAction::Allow)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PairedMmEngine {
    config: PairedMmEngineConfig,
    fill_state: AutoFillState,
}

impl PairedMmEngine {
    pub fn new(config: PairedMmEngineConfig) -> Self {
        Self {
            config,
            fill_state: AutoFillState::new(),
        }
    }

    pub fn config(&self) -> &PairedMmEngineConfig {
        &self.config
    }

    pub fn fill_state(&self) -> &AutoFillState {
        &self.fill_state
    }

    pub fn on_fill<M>(
        &mut self,
        market: &M,
        fill: &FillReport,
        snapshot: &PairedMarketSnapshot,
        fair_value: &FairValueEstimate,
    ) -> AutoFillDecision
    where
        M: MarketDescriptor,
    {
        self.fill_state
            .on_fill(market, fill, snapshot, fair_value, self.config.auto_fill)
    }

    pub fn decide<M>(&self, input: &PairedMmInput<M>) -> PairedMmDecision
    where
        M: MarketDescriptor,
    {
        let hard_policy = evaluate_hard_policy(
            &input.inventory,
            input.market.time_remaining_ms(input.now_ms),
            &input.btc_regime,
            self.config.hard_policy,
        );
        let merge = choose_merge(input.merge_candidate.as_ref(), self.config.merge);
        let capital_recycle = choose_capital_recycle(
            &input.market,
            &input.snapshot,
            &input.inventory,
            self.config.capital_recycle,
            input.now_ms,
        );

        let rescue = input
            .rescue
            .map(|rescue_inputs| choose_rescue(rescue_inputs, self.config.rescue));

        let ladder = match &hard_policy.action {
            HardPolicyAction::Allow => build_ladder(
                &input.market,
                &input.snapshot,
                &input.inventory,
                &input.fair_value,
                &input.btc_regime,
                &input.pair_cost,
                &self.config.ladder,
                input.now_ms,
            ),
            HardPolicyAction::SuppressPaired { .. } | HardPolicyAction::ForceFlatten { .. } => {
                let mut result = build_ladder(
                    &input.market,
                    &input.snapshot,
                    &input.inventory,
                    &input.fair_value,
                    &input.btc_regime,
                    &input.pair_cost,
                    &self.config.ladder,
                    input.now_ms,
                );
                result.intents.clear();
                result
            }
        };
        let convex_overlay = if matches!(hard_policy.action, HardPolicyAction::Allow) {
            choose_convex_overlay(
                &input.market,
                &input.snapshot,
                &input.inventory,
                &input.fair_value,
                self.config.convexity_overlay,
                input.now_ms,
            )
        } else {
            None
        };

        let mut notes = hard_policy.notes.clone();
        notes.extend(ladder.diagnostics.notes.clone());
        match &merge {
            MergePolicyDecision::MergeNow { reason, .. } | MergePolicyDecision::Wait { reason } => {
                notes.push(reason.clone());
            }
        }
        match &capital_recycle {
            CapitalRecycleDecision::BuyLightSide { reason, .. }
            | CapitalRecycleDecision::Wait { reason } => notes.push(reason.clone()),
        }
        if let Some(rescue) = &rescue {
            notes.push(format!(
                "paired-mm rescue action={:?} qty={:.4} reason={}",
                rescue.action, rescue.qty, rescue.reason
            ));
        }
        if let Some(intent) = &convex_overlay {
            notes.push(format!(
                "paired-mm decision_label=winner_side_tilt mode=convex_tilt intent={} price={:.4} qty={:.4} notional={:.4}",
                intent.client_order_id,
                intent.limit_price,
                intent.quantity,
                intent.notional_usd()
            ));
        }
        if let HardPolicyAction::ForceFlatten { .. } = hard_policy.action {
            if rescue.as_ref().is_none_or(|decision| decision.qty <= 1e-9)
                && matches!(merge, MergePolicyDecision::Wait { .. })
            {
                notes.push(
                    "force-flatten active but no merge/rescue action is currently viable"
                        .to_string(),
                );
            }
        }

        PairedMmDecision {
            ladder,
            hard_policy,
            merge,
            capital_recycle,
            rescue,
            convex_overlay,
            notes,
        }
    }

    pub fn suppression_reason(decision: &PairedMmDecision) -> Option<CoolingReason> {
        match &decision.hard_policy.action {
            HardPolicyAction::Allow => None,
            HardPolicyAction::SuppressPaired { reason, .. }
            | HardPolicyAction::ForceFlatten { reason } => Some(reason.clone()),
        }
    }
}

fn choose_convex_overlay<M: MarketDescriptor>(
    market: &M,
    snapshot: &PairedMarketSnapshot,
    inventory: &PairedInventorySnapshot,
    fair_value: &FairValueEstimate,
    config: ConvexityOverlayConfig,
    now_ms: EpochMillis,
) -> Option<OrderIntent> {
    if !config.enabled {
        return None;
    }
    let remaining_ms = market
        .time_remaining_ms(now_ms)
        .unwrap_or(market.window_ms());
    if remaining_ms > config.late_window_sec.saturating_mul(1_000) {
        return None;
    }

    let (leg_tag, instrument_id, quote, win_prob, current_qty, opposite_qty, avg_cost) =
        if fair_value.p_up >= config.convex_p_threshold {
            (
                "yes",
                market.yes_instrument_id().clone(),
                &snapshot.yes_quote,
                fair_value.p_up,
                inventory.yes_qty,
                inventory.no_qty,
                inventory.yes_avg_cost,
            )
        } else if fair_value.p_down >= config.convex_p_threshold {
            (
                "no",
                market.no_instrument_id().clone(),
                &snapshot.no_quote,
                fair_value.p_down,
                inventory.no_qty,
                inventory.yes_qty,
                inventory.no_avg_cost,
            )
        } else {
            return None;
        };

    let tick_size = market.tick_size().max(0.0001);
    let best_ask = quote.best_ask.as_ref()?.price;
    let best_bid = quote
        .best_bid
        .as_ref()
        .map(|level| level.price)
        .unwrap_or(tick_size);
    if !best_ask.is_finite() || best_ask <= tick_size || best_ask >= 1.0 {
        return None;
    }

    let maker_cap = best_ask - tick_size * config.maker_safety_ticks.max(1.0);
    let limit_price = (best_bid + tick_size)
        .min(maker_cap)
        .clamp(tick_size, 1.0 - tick_size);
    if !limit_price.is_finite() || limit_price >= best_ask {
        return None;
    }

    let excess_qty = (current_qty - opposite_qty).max(0.0);
    let cost_basis = if avg_cost.is_finite() && avg_cost > 0.0 {
        avg_cost
    } else {
        limit_price
    };
    let current_excess_usd = excess_qty * cost_basis;
    let remaining_excess_budget = config.max_excess_usd - current_excess_usd;
    if remaining_excess_budget <= 0.0 {
        return None;
    }

    let clip_usd = config.clip_usd.min(remaining_excess_budget);
    let quantity = (clip_usd / limit_price.max(tick_size)).max(market.min_order_size());
    let notional = quantity * limit_price;
    if notional > remaining_excess_budget + 1e-9 {
        return None;
    }

    let mut intent = OrderIntent::new_buy(
        ClientOrderId::from(format!(
            "paired-mm-convex:{}:{}:{}",
            market.market_id(),
            leg_tag,
            now_ms
        )),
        market.market_id().clone(),
        instrument_id,
        limit_price,
        quantity,
        format!(
            "paired-mm winner-side convex tilt leg={leg_tag} p_win={win_prob:.4} excess_usd={current_excess_usd:.4} max_excess_usd={:.4}",
            config.max_excess_usd
        ),
        now_ms,
    );
    intent.quote_level_tag = Some(format!(
        "mm-convex-accum:winner-side-tilt:{leg_tag}:{:?}",
        MmQuoteKind::ConvexAccumulation
    ));
    intent.kind = IntentKind::Entry;
    Some(intent)
}
