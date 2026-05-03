//! Strategy adapter that applies the reusable paired-MM algorithm to a market.

use crate::market_making::paired_mm::{
    AutoFillSuggestion, CapitalRecycleConfig, HardPolicyConfig, LadderConfig, MergePolicyConfig,
    PairedMmEngine, PairedMmEngineConfig, PairedMmInput, RescueConfig,
};
use crate::markets::MarketDescriptor;
use crate::strategies::traits::{StrategyFillInput, StrategyInput, TradingStrategy};
use crate::types::{CoolingReason, StrategyDecision};

#[derive(Clone, Debug, PartialEq)]
pub struct PairedMmStrategyConfig {
    pub ladder: LadderConfig,
    pub rescue: RescueConfig,
    pub merge: MergePolicyConfig,
    pub capital_recycle: CapitalRecycleConfig,
    pub hard_policy: HardPolicyConfig,
}

impl Default for PairedMmStrategyConfig {
    fn default() -> Self {
        Self {
            ladder: LadderConfig::default(),
            rescue: RescueConfig::default(),
            merge: MergePolicyConfig::default(),
            capital_recycle: CapitalRecycleConfig::default(),
            hard_policy: HardPolicyConfig::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PairedMmStrategy {
    engine: PairedMmEngine,
}

impl PairedMmStrategy {
    pub fn new(config: PairedMmStrategyConfig) -> Self {
        Self {
            engine: PairedMmEngine::new(PairedMmEngineConfig {
                ladder: config.ladder,
                rescue: config.rescue,
                merge: config.merge,
                capital_recycle: config.capital_recycle,
                hard_policy: config.hard_policy,
                auto_fill: Default::default(),
            }),
        }
    }

    pub fn engine(&self) -> &PairedMmEngine {
        &self.engine
    }
}

impl<M> TradingStrategy<M> for PairedMmStrategy
where
    M: MarketDescriptor,
{
    fn name(&self) -> &'static str {
        "paired_mm"
    }

    fn on_tick(&mut self, input: StrategyInput<M>) -> StrategyDecision {
        let btc_regime = input.btc_regime.regime();
        let vol_5m_bps = input.btc_regime.realized_vol_5m_bps;
        let ret180_bps = input.btc_regime.return_180s_bps;
        let imbalance_qty = input.inventory.side_imbalance_qty();
        let recycle_only_threshold = self
            .engine
            .config()
            .capital_recycle
            .min_imbalance_qty
            .max(input.market.min_order_size());
        let recycle_only = imbalance_qty >= recycle_only_threshold;
        let decision = self.engine.decide(&PairedMmInput {
            market: input.market,
            snapshot: input.snapshot,
            inventory: input.inventory,
            pair_cost: input.pair_cost,
            fair_value: input.fair_value,
            btc_regime: input.btc_regime,
            merge_candidate: None,
            rescue: None,
            now_ms: input.now_ms,
        });

        let mut notes = decision.notes.clone();
        notes.extend(decision.ladder.diagnostics.notes.clone());
        notes.push(format!(
            "paired-mm ladder regime={:?} btc_regime={:?} vol_5m_bps={:?} ret180_bps={:?} depth={} spacing_ticks={:.2} yes_res={:.4} no_res={:.4}",
            decision.ladder.diagnostics.regime,
            btc_regime,
            vol_5m_bps,
            ret180_bps,
            decision.ladder.diagnostics.depth,
            decision.ladder.diagnostics.spacing_ticks,
            decision.ladder.diagnostics.yes_reservation,
            decision.ladder.diagnostics.no_reservation
        ));

        if let Some(intent) = decision.capital_recycle_intent().cloned() {
            notes.push("paired-mm capital recycle emitted".to_string());
            return StrategyDecision::capital_recycle(vec![intent], notes);
        }

        if recycle_only {
            notes.push(format!(
                "paired-mm recycle-only: suppressing fresh paired-entry ladder imbalance_qty={imbalance_qty:.4} threshold={recycle_only_threshold:.4}"
            ));
            return StrategyDecision::suppress(CoolingReason::SideImbalanceCap, false, notes);
        }

        if let Some(reason) = PairedMmEngine::suppression_reason(&decision) {
            return StrategyDecision::suppress(reason, false, notes);
        }

        StrategyDecision::quote_set(decision.ladder.intents, notes)
    }

    fn on_fill(&mut self, input: StrategyFillInput<M>) -> StrategyDecision {
        let decision = self.engine.on_fill(
            &input.market,
            &input.fill,
            &input.snapshot,
            &input.fair_value,
        );
        match decision.suggestion {
            AutoFillSuggestion::None => StrategyDecision::Noop {
                notes: decision.notes,
            },
            AutoFillSuggestion::Cooldown { reason, note } => {
                let mut notes = decision.notes;
                notes.push(note);
                StrategyDecision::suppress(reason, true, notes)
            }
            AutoFillSuggestion::Rescue {
                decision: rescue, ..
            } => {
                let mut notes = decision.notes;
                notes.push(format!(
                    "paired-mm on-fill rescue requires venue adapter action={:?} qty={:.4}",
                    rescue.action, rescue.qty
                ));
                StrategyDecision::rescue(Vec::new(), notes)
            }
        }
    }
}
