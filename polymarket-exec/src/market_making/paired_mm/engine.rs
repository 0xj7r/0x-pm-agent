//! Deep paired-MM interface.
//!
//! Callers should prefer this facade over wiring ladder/rescue modules by hand.
//! It encodes the intended decision order:
//! 1. close/flatten decisions are considered first,
//! 2. entry ladders are generated only when hard risk state permits,
//! 3. all output remains proposed intents for the runtime hard risk boundary.

use crate::market_making::pairing::capital_recycler::{
    choose_capital_recycle, CapitalRecycleConfig, CapitalRecycleDecision,
};
use crate::market_making::paired_mm::fill_automation::{
    AutoFillConfig, AutoFillDecision, AutoFillState,
};
use crate::market_making::paired_mm::ladder_builder::{
    build_ladder, LadderBuildResult, LadderConfig,
};
use crate::market_making::pairing::merge_policy::{
    choose_merge, MergePolicyConfig, MergePolicyDecision,
};
use crate::market_making::pairing::pair_cost_tracker::PairCostTracker;
use crate::market_making::pairing::rescue_engine::{
    choose_rescue, RescueAction, RescueConfig, RescueDecision, RescueInputs,
};
use crate::market_making::pairing::risk_policy::{
    evaluate_hard_policy, HardPolicyAction, HardPolicyConfig, HardPolicyDecision,
};
use crate::market_making::pairing::types::{PairedInventorySnapshot, PairedMarketSnapshot};
use crate::markets::MarketDescriptor;
use crate::market_making::pairing::pair_ledger::MergeCandidate;
use crate::signals::{BtcRegimeSnapshot, FairValueEstimate};
use crate::types::{CoolingReason, EpochMillis, FillReport};

#[derive(Clone, Debug, PartialEq)]
pub struct PairedMmEngineConfig {
    pub ladder: LadderConfig,
    pub rescue: RescueConfig,
    pub merge: MergePolicyConfig,
    pub capital_recycle: CapitalRecycleConfig,
    pub hard_policy: HardPolicyConfig,
    pub auto_fill: AutoFillConfig,
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
