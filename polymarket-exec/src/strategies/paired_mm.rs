//! Strategy adapter that applies the reusable paired-MM algorithm to a market.

use crate::market_making::paired_mm::{
    AutoFillSuggestion, CapitalRecycleConfig, HardPolicyConfig, LadderConfig, MergePolicyConfig,
    PairedMmEngine, PairedMmEngineConfig, PairedMmInput, RescueConfig,
};
use crate::markets::MarketDescriptor;
use crate::strategies::traits::{StrategyFillInput, StrategyInput, TradingStrategy};
use crate::types::StrategyDecision;

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
            "paired-mm ladder regime={:?} depth={} spacing_ticks={:.2} yes_res={:.4} no_res={:.4}",
            decision.ladder.diagnostics.regime,
            decision.ladder.diagnostics.depth,
            decision.ladder.diagnostics.spacing_ticks,
            decision.ladder.diagnostics.yes_reservation,
            decision.ladder.diagnostics.no_reservation
        ));

        if let Some(intent) = decision.capital_recycle_intent().cloned() {
            return StrategyDecision::capital_recycle(vec![intent], notes);
        }

        if decision.should_emit_rescue() {
            return StrategyDecision::rescue(
                Vec::new(),
                notes
                    .into_iter()
                    .chain(std::iter::once(
                        "paired-mm rescue selected; concrete rescue intent builder not wired"
                            .to_string(),
                    ))
                    .collect(),
            );
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
