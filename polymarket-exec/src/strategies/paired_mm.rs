//! Strategy adapter that applies the reusable paired-MM algorithm to a market.

use std::collections::BTreeMap;

use crate::market_making::paired_mm::{
    AutoFillSuggestion, CapitalRecycleConfig, ConvexityOverlayConfig, HardPolicyConfig,
    LadderConfig, MergePolicyConfig, MergePolicyDecision, PairedMmEngine, PairedMmEngineConfig,
    PairedMmInput, RescueConfig,
};
use crate::market_making::pairing::pair_ledger::MergeCandidate;
use crate::markets::MarketDescriptor;
use crate::signals::{BookSanityConfig, ReversalConfig, SideScoreConfig};
use crate::strategies::traits::{StrategyFillInput, StrategyInput, TradingStrategy};
use crate::types::{ClientOrderId, CoolingReason, InstrumentId, MergeIntent, StrategyDecision};

#[derive(Clone, Debug, PartialEq)]
pub struct PairedMmStrategyConfig {
    pub ladder: LadderConfig,
    pub rescue: RescueConfig,
    pub merge: MergePolicyConfig,
    pub capital_recycle: CapitalRecycleConfig,
    pub hard_policy: HardPolicyConfig,
    pub convexity_overlay: ConvexityOverlayConfig,
    pub reversal: ReversalConfig,
    pub book_sanity: BookSanityConfig,
    pub side_score: SideScoreConfig,
}

impl Default for PairedMmStrategyConfig {
    fn default() -> Self {
        let mut rescue = RescueConfig::default();
        rescue.allow_sell_fallback = false;
        Self {
            ladder: LadderConfig::default(),
            rescue,
            merge: MergePolicyConfig::default(),
            capital_recycle: CapitalRecycleConfig::default(),
            hard_policy: HardPolicyConfig::default(),
            convexity_overlay: ConvexityOverlayConfig::default(),
            reversal: ReversalConfig::default(),
            book_sanity: BookSanityConfig::default(),
            side_score: SideScoreConfig::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PairedMmStrategy {
    engine: PairedMmEngine,
    last_capital_recycle_at_ms: BTreeMap<String, u64>,
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
                convexity_overlay: config.convexity_overlay,
                reversal: config.reversal,
                book_sanity: config.book_sanity,
                side_score: config.side_score,
            }),
            last_capital_recycle_at_ms: BTreeMap::new(),
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
        let now_ms = input.now_ms;
        let btc_regime = input.btc_regime.regime();
        let vol_5m_bps = input.btc_regime.realized_vol_5m_bps;
        let ret180_bps = input.btc_regime.return_180s_bps;
        let imbalance_qty = input.inventory.side_imbalance_qty();
        let yes_instrument_id = input.market.yes_instrument_id().clone();
        let no_instrument_id = input.market.no_instrument_id().clone();
        let repair_mode = RepairMode::from_inventory(
            input.inventory.yes_qty,
            input.inventory.no_qty,
            input.market.min_order_size().max(1.0),
            &yes_instrument_id,
            &no_instrument_id,
        );
        let recycle_only_threshold = self
            .engine
            .config()
            .capital_recycle
            .min_imbalance_qty
            .max(input.market.min_order_size());
        let recycle_only = imbalance_qty >= recycle_only_threshold;
        let merge_candidate = merge_candidate_from_input(&input);
        let decision = self.engine.decide(&PairedMmInput {
            market: input.market,
            snapshot: input.snapshot,
            inventory: input.inventory,
            open_convex_order_exposure: input.open_convex_order_exposure,
            pair_cost: input.pair_cost,
            fair_value: input.fair_value,
            btc_regime: input.btc_regime,
            momentum: input.momentum.clone(),
            order_book_pressure: input.order_book_pressure.clone(),
            merge_candidate: merge_candidate.clone(),
            rescue: None,
            now_ms: input.now_ms,
        });

        let mut notes = decision.notes.clone();
        notes.extend(decision.ladder.diagnostics.notes.clone());
        notes.push(format!(
            "paired-mm ladder regime={:?} btc_regime={:?} vol_5m_bps={:?} ret180_bps={:?} momentum_dir={:?} momentum_score={:.4} pressure_dir={:?} pressure_imbalance={:.4} thin_book={} depth={} spacing_ticks={:.2} yes_res={:.4} no_res={:.4} yes_signal_scale={:.3} no_signal_scale={:.3} side_favorite={:?} side_confidence={:.4} yes_side_score={:.4} no_side_score={:.4} reversal_prob={:.4} book_penalty_yes={:.4} book_penalty_no={:.4}",
            decision.ladder.diagnostics.regime,
            btc_regime,
            vol_5m_bps,
            ret180_bps,
            input.momentum.direction,
            input.momentum.score,
            input.order_book_pressure.direction,
            input.order_book_pressure.imbalance,
            input.order_book_pressure.thin_book,
            decision.ladder.diagnostics.depth,
            decision.ladder.diagnostics.spacing_ticks,
            decision.ladder.diagnostics.yes_reservation,
            decision.ladder.diagnostics.no_reservation,
            decision.ladder.diagnostics.yes_signal_scale,
            decision.ladder.diagnostics.no_signal_scale,
            decision.side_score.favorite_leg,
            decision.side_score.confidence,
            decision.ladder.diagnostics.yes_side_score,
            decision.ladder.diagnostics.no_side_score,
            decision.reversal.probability,
            decision.book_sanity.yes.penalty,
            decision.book_sanity.no.penalty
        ));

        if let (Some(candidate), MergePolicyDecision::MergeNow { quantity, reason }) =
            (merge_candidate, decision.merge.clone())
        {
            notes.push(format!(
                "paired-mm decision_label=merge mode=merge_first quantity={quantity:.4} reason={reason}"
            ));
            return StrategyDecision::Merge {
                intent: MergeIntent {
                    command_id: ClientOrderId::new(format!(
                        "paired-mm-merge:{}:{}",
                        candidate.market_id, now_ms
                    )),
                    market_id: candidate.market_id,
                    condition_id: None,
                    yes_instrument_id: candidate
                        .yes_instrument_id
                        .expect("merge candidate carries YES instrument"),
                    no_instrument_id: candidate
                        .no_instrument_id
                        .expect("merge candidate carries NO instrument"),
                    quantity,
                    expected_cash_usd: quantity,
                    expected_cost_usd: candidate.expected_cost_usd,
                    expected_fee_usd: candidate.expected_fee_usd,
                    expected_gas_usd: candidate.expected_gas_usd,
                    reason,
                    created_at_ms: now_ms,
                },
                notes,
            };
        }

        if let Some(intent) = decision.capital_recycle_intent().cloned() {
            let recycle_key = format!("{}:{}", intent.market_id, intent.instrument_id);
            let cooldown_ms = self.engine.config().capital_recycle.cooldown_ms;
            let in_cooldown = cooldown_ms > 0
                && self
                    .last_capital_recycle_at_ms
                    .get(&recycle_key)
                    .is_some_and(|last| now_ms.saturating_sub(*last) < cooldown_ms);
            if in_cooldown {
                notes.push(format!(
                    "paired-mm capital recycle cooldown active key={recycle_key} cooldown_ms={cooldown_ms}"
                ));
            } else {
                self.last_capital_recycle_at_ms
                    .insert(recycle_key, now_ms);
                notes.push("paired-mm decision_label=cheap_leg_recycle mode=cheap_leg_mode capital recycle emitted".to_string());
                return StrategyDecision::capital_recycle(vec![intent], notes);
            }
        }

        if let Some(reason) = PairedMmEngine::suppression_reason(&decision) {
            return StrategyDecision::suppress(reason, false, notes);
        }

        let mut intents = decision.ladder.intents;
        if let Some(repair_mode) = &repair_mode {
            let before = intents.len();
            intents.retain(|intent| intent.instrument_id == repair_mode.light_instrument_id);
            let removed = before.saturating_sub(intents.len());
            notes.push(format!(
                "paired-mm decision_label=light_side_repair mode=repair_first heavy_leg={} light_leg={} imbalance_qty={imbalance_qty:.4} recycle_threshold={recycle_only_threshold:.4} removed_heavy_leg_quotes={removed}",
                repair_mode.heavy_leg,
                repair_mode.light_leg
            ));
        }
        if !intents.is_empty() {
            if repair_mode.is_some() {
                notes.push(
                    "paired-mm decision_label=light_side_repair mode=repair_first light-side repair ladder emitted"
                        .to_string(),
                );
            } else {
                notes.push("paired-mm decision_label=paired_entry mode=paired_entry_mode paired ladder emitted".to_string());
            }
        }
        for intent in decision.convex_overlay {
            if repair_mode
                .as_ref()
                .is_none_or(|mode| intent.instrument_id != mode.heavy_instrument_id)
            {
                intents.push(intent);
            } else {
                notes.push(
                    "paired-mm convex overlay suppressed: would add to heavy leg during repair-first mode"
                        .to_string(),
                );
            }
        }

        if intents.is_empty() && repair_mode.is_some() {
            notes.push(format!(
                "paired-mm repair-first: no viable light-side quote; suppressing fresh entry imbalance_qty={imbalance_qty:.4} threshold={recycle_only_threshold:.4}"
            ));
            return StrategyDecision::suppress(CoolingReason::SideImbalanceCap, false, notes);
        }

        if recycle_only && repair_mode.is_none() {
            notes.push(format!(
                "paired-mm recycle-only: suppressing fresh paired-entry ladder imbalance_qty={imbalance_qty:.4} threshold={recycle_only_threshold:.4}"
            ));
            return StrategyDecision::suppress(CoolingReason::SideImbalanceCap, false, notes);
        }

        StrategyDecision::quote_set(intents, notes)
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

fn merge_candidate_from_input<M>(input: &StrategyInput<M>) -> Option<MergeCandidate>
where
    M: MarketDescriptor,
{
    let paired_qty = input.inventory.yes_qty.min(input.inventory.no_qty);
    if paired_qty <= 1e-9 {
        return None;
    }
    let yes_avg_cost = input.inventory.yes_avg_cost.max(0.0);
    let no_avg_cost = input.inventory.no_avg_cost.max(0.0);
    let expected_cost_usd = paired_qty * yes_avg_cost + paired_qty * no_avg_cost;
    let expected_cash_usd = paired_qty;
    let expected_fee_usd = 0.0;
    let expected_gas_usd = 0.0;
    let stranded_yes_qty = (input.inventory.yes_qty - paired_qty).max(0.0);
    let stranded_no_qty = (input.inventory.no_qty - paired_qty).max(0.0);
    Some(MergeCandidate {
        market_id: input.market.market_id().clone(),
        yes_instrument_id: Some(input.market.yes_instrument_id().clone()),
        no_instrument_id: Some(input.market.no_instrument_id().clone()),
        paired_qty,
        stranded_yes_qty,
        stranded_no_qty,
        stranded_yes_cost_usd: stranded_yes_qty * yes_avg_cost,
        stranded_no_cost_usd: stranded_no_qty * no_avg_cost,
        expected_cash_usd,
        expected_cost_usd,
        expected_gas_usd,
        expected_fee_usd,
        expected_net_gain_usd: expected_cash_usd
            - expected_cost_usd
            - expected_fee_usd
            - expected_gas_usd,
        last_merge_at_ms: None,
        observed_at_ms: input.now_ms,
    })
}

#[derive(Clone, Debug, PartialEq)]
struct RepairMode {
    heavy_leg: &'static str,
    light_leg: &'static str,
    heavy_instrument_id: InstrumentId,
    light_instrument_id: InstrumentId,
}

impl RepairMode {
    fn from_inventory(
        yes_qty: f64,
        no_qty: f64,
        min_meaningful_qty: f64,
        yes_instrument_id: &InstrumentId,
        no_instrument_id: &InstrumentId,
    ) -> Option<Self> {
        let imbalance_qty = (yes_qty - no_qty).abs();
        if imbalance_qty < min_meaningful_qty.max(1e-9) {
            return None;
        }
        if yes_qty > no_qty {
            Some(Self {
                heavy_leg: "yes",
                light_leg: "no",
                heavy_instrument_id: yes_instrument_id.clone(),
                light_instrument_id: no_instrument_id.clone(),
            })
        } else if no_qty > yes_qty {
            Some(Self {
                heavy_leg: "no",
                light_leg: "yes",
                heavy_instrument_id: no_instrument_id.clone(),
                light_instrument_id: yes_instrument_id.clone(),
            })
        } else {
            None
        }
    }
}
