//! Strategy adapter that applies the reusable paired-MM algorithm to a market.

use std::collections::BTreeMap;

use crate::core::types::FillReport;
use crate::market_making::paired_mm::{
    AutoFillSuggestion, CapitalRecycleConfig, ConvexityOverlayConfig, HardPolicyConfig,
    LadderConfig, MergePolicyConfig, MergePolicyDecision, PairedMmEngine, PairedMmEngineConfig,
    PairedMmInput, RescueConfig,
};
use crate::market_making::pairing::pair_ledger::MergeCandidate;
use crate::market_making::pairing::rescue_engine::{RescueAction, RescueDecision};
use crate::market_making::pairing::types::{LadderLeg, PairedMarketSnapshot};
use crate::markets::MarketDescriptor;
use crate::signals::{BookSanityConfig, ReversalConfig, SideScoreConfig};
use crate::strategies::traits::{StrategyFillInput, StrategyInput, TradingStrategy};
use crate::types::{
    ClientOrderId, CoolingReason, EpochMillis, InstrumentId, IntentKind, MergeIntent, OrderIntent,
    StrategyDecision,
};

/// Race buffer in ticks used when crossing the touch on a hedge rescue.
/// Matches the live profile's `rescue.hedge_rescue_race_buffer_ticks`
/// default of 3.0. Hard-coded for now; the YAML knob is currently dead
/// config in `strategy_profile.rs:843-846` and plumbing it through
/// PairedMmStrategyConfig is a follow-up.
const HEDGE_RESCUE_RACE_BUFFER_TICKS: f64 = 3.0;

/// Translate a rescue brain decision into venue-ready aggressive Close
/// intents. Returns an empty Vec if the decision is `Hold`, qty is below
/// the float floor, or the snapshot lacks the side we need to lift/hit.
///
/// Without this builder the on-fill rescue path emits an empty intent
/// vector and the simulator never sees the rescue, so stranded one-sided
/// inventory just sits and resolves on the losing side. That accounts for
/// the bulk of `expired_losing_cost_by_path[paired_mm]` we observed in
/// the whale-v1 21-day backtest ($2,756 / 21d).
fn build_rescue_intents<M: MarketDescriptor>(
    rescue: &RescueDecision,
    fill: &FillReport,
    snapshot: &PairedMarketSnapshot,
    market: &M,
    now_ms: EpochMillis,
) -> Vec<OrderIntent> {
    let qty = rescue.qty.max(0.0);
    if qty < 1e-9 {
        return Vec::new();
    }
    let tick = market.tick_size().max(0.0001);

    // The leg the fill just landed on is the "stranded" side because
    // it has accumulated more inventory than the opposite leg.
    let stranded_leg = if fill.instrument_id == *market.yes_instrument_id() {
        LadderLeg::Yes
    } else if fill.instrument_id == *market.no_instrument_id() {
        LadderLeg::No
    } else {
        return Vec::new();
    };

    match rescue.action {
        RescueAction::Hold => Vec::new(),
        RescueAction::BuyOppositeForMerge => {
            let (opposite_id, opposite_quote) = match stranded_leg {
                LadderLeg::Yes => (market.no_instrument_id().clone(), &snapshot.no_quote),
                LadderLeg::No => (market.yes_instrument_id().clone(), &snapshot.yes_quote),
            };
            let Some(best_ask) = opposite_quote.best_ask.as_ref() else {
                return Vec::new();
            };
            // Cross the touch by `race_buffer_ticks` so the fill simulator
            // walks opposing depth instead of resting passive.
            let limit_price =
                (best_ask.price + HEDGE_RESCUE_RACE_BUFFER_TICKS * tick).clamp(tick, 0.999);
            let coid = ClientOrderId::from(format!(
                "mm-hedge-rescue:merge:{}:{:?}:{}",
                market.market_id(),
                stranded_leg,
                now_ms,
            ));
            let mut intent = OrderIntent::new_buy(
                coid,
                market.market_id().clone(),
                opposite_id,
                limit_price,
                qty,
                format!(
                    "hedge_rescue buy_opposite_for_merge stranded_leg={:?} qty={:.4} ask={:.4} reason={}",
                    stranded_leg, qty, best_ask.price, rescue.reason
                ),
                now_ms,
            );
            intent.kind = IntentKind::Close;
            intent.quote_level_tag = Some(format!("mm-hedge-rescue:merge:{:?}", stranded_leg));
            vec![intent]
        }
        RescueAction::SellStrandedLeg => {
            let (stranded_id, stranded_quote) = match stranded_leg {
                LadderLeg::Yes => (market.yes_instrument_id().clone(), &snapshot.yes_quote),
                LadderLeg::No => (market.no_instrument_id().clone(), &snapshot.no_quote),
            };
            let Some(best_bid) = stranded_quote.best_bid.as_ref() else {
                return Vec::new();
            };
            let limit_price =
                (best_bid.price - HEDGE_RESCUE_RACE_BUFFER_TICKS * tick).clamp(tick, 0.999);
            let coid = ClientOrderId::from(format!(
                "mm-hedge-rescue:sell:{}:{:?}:{}",
                market.market_id(),
                stranded_leg,
                now_ms,
            ));
            let mut intent = OrderIntent::new_sell(
                coid,
                market.market_id().clone(),
                stranded_id,
                limit_price,
                qty,
                format!(
                    "hedge_rescue sell_stranded_leg leg={:?} qty={:.4} bid={:.4} reason={}",
                    stranded_leg, qty, best_bid.price, rescue.reason
                ),
                now_ms,
            );
            intent.kind = IntentKind::Close;
            intent.quote_level_tag = Some(format!("mm-hedge-rescue:sell:{:?}", stranded_leg));
            vec![intent]
        }
    }
}

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
                self.last_capital_recycle_at_ms.insert(recycle_key, now_ms);
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
                let now_ms = input.fill.observed_at_ms;
                // Phase 1 (early/mid bar): paired_mm + rescue actively unwind
                // asymmetric fills to keep inventory neutral.
                // Phase 2 (late bar): paired_mm hands off to the directional
                // lanes (late-favorite + cheap-tail). Rescue must not fire in
                // Phase 2 because it would unwind the directional positions
                // those lanes are intentionally accumulating.
                //
                // Boundary uses the same `convexity_overlay.late_window_sec`
                // that gates convex on_tick so both lanes share one truth.
                let late_threshold_ms = self
                    .engine
                    .config()
                    .convexity_overlay
                    .late_window_sec
                    .saturating_mul(1_000);
                let remaining_ms = input
                    .market
                    .time_remaining_ms(now_ms)
                    .unwrap_or(input.market.window_ms());
                if remaining_ms <= late_threshold_ms {
                    notes.push(format!(
                        "hedge_rescue suppressed (Phase 2 directional): remaining_ms={} <= late_threshold_ms={} action={:?} qty={:.4} reason={}",
                        remaining_ms,
                        late_threshold_ms,
                        rescue.action,
                        rescue.qty,
                        rescue.reason
                    ));
                    return StrategyDecision::Noop { notes };
                }
                let intents = build_rescue_intents(
                    &rescue,
                    &input.fill,
                    &input.snapshot,
                    &input.market,
                    now_ms,
                );
                notes.push(format!(
                    "paired-mm hedge_rescue (Phase 1) action={:?} qty={:.4} intents_built={} reason={}",
                    rescue.action,
                    rescue.qty,
                    intents.len(),
                    rescue.reason
                ));
                StrategyDecision::rescue(intents, notes)
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

#[cfg(test)]
mod hedge_rescue_tests {
    use super::*;
    use crate::core::types::{BookLevel, FillLiquidity, QuoteSnapshot, TradeSide};
    use crate::markets::descriptor::BinaryOutcomeMarket;
    use crate::types::{InstrumentId, MarketId};

    fn quote(bid: f64, ask: f64) -> QuoteSnapshot {
        QuoteSnapshot {
            best_bid: Some(BookLevel::new(bid, 100.0)),
            best_ask: Some(BookLevel::new(ask, 100.0)),
            bid_levels: vec![BookLevel::new(bid, 100.0)],
            ask_levels: vec![BookLevel::new(ask, 100.0)],
            depth_observed_at_ms: Some(0),
            last_trade_price: None,
            taker_buy_qty_60s: 0.0,
            taker_sell_qty_60s: 0.0,
            observed_at_ms: 0,
        }
    }

    fn fixtures() -> (BinaryOutcomeMarket, PairedMarketSnapshot) {
        let market = BinaryOutcomeMarket::btc_5m(
            MarketId::from("m"),
            InstrumentId::from("yes"),
            InstrumentId::from("no"),
        );
        let snapshot = PairedMarketSnapshot {
            market_id: MarketId::from("m"),
            yes_instrument_id: InstrumentId::from("yes"),
            no_instrument_id: InstrumentId::from("no"),
            yes_quote: quote(0.40, 0.45),
            no_quote: quote(0.55, 0.60),
        };
        (market, snapshot)
    }

    fn fill(instrument: &str) -> FillReport {
        FillReport {
            order_id: None,
            client_order_id: Some(ClientOrderId::from("paired-mm:m:yes:l1:1")),
            market_id: MarketId::from("m"),
            instrument_id: InstrumentId::from(instrument),
            side: TradeSide::Buy,
            price: 0.40,
            quantity: 5.0,
            fee_usd: 0.0,
            liquidity: FillLiquidity::Maker,
            close_method: None,
            observed_at_ms: 1_000,
        }
    }

    #[test]
    fn build_rescue_intents_empty_for_hold_action() {
        let (market, snapshot) = fixtures();
        let decision = RescueDecision {
            action: RescueAction::Hold,
            qty: 0.0,
            hold_value_per_share: 0.5,
            rescue_value_per_share: None,
            delta_vs_hold_per_share: None,
            reason: "no opposite arm".into(),
        };
        let intents = build_rescue_intents(&decision, &fill("yes"), &snapshot, &market, 1_000);
        assert!(intents.is_empty());
    }

    #[test]
    fn build_rescue_intents_buy_opposite_for_merge_yes_stranded() {
        // Yes leg over-filled. Rescue brain says buy NO at the touch + race
        // buffer to create a mergeable pair.
        let (market, snapshot) = fixtures();
        let decision = RescueDecision {
            action: RescueAction::BuyOppositeForMerge,
            qty: 5.0,
            hold_value_per_share: 0.40,
            rescue_value_per_share: Some(0.40),
            delta_vs_hold_per_share: Some(0.05),
            reason: "merge cheaper than holding".into(),
        };
        let intents = build_rescue_intents(&decision, &fill("yes"), &snapshot, &market, 1_000);
        assert_eq!(intents.len(), 1);
        let intent = &intents[0];
        assert_eq!(intent.side, TradeSide::Buy);
        assert_eq!(intent.kind, IntentKind::Close);
        assert_eq!(intent.instrument_id, InstrumentId::from("no"));
        let no_ask = 0.60;
        let tick = market.tick_size();
        let expected = (no_ask + 3.0 * tick).clamp(tick, 0.999);
        assert!(
            (intent.limit_price - expected).abs() < 1e-9,
            "price should cross opposite ask + race_buffer"
        );
    }

    #[test]
    fn build_rescue_intents_sell_stranded_leg_no_stranded() {
        // No leg over-filled. Sell NO at best_bid - race_buffer to take.
        let (market, snapshot) = fixtures();
        let decision = RescueDecision {
            action: RescueAction::SellStrandedLeg,
            qty: 7.0,
            hold_value_per_share: 0.55,
            rescue_value_per_share: Some(0.55),
            delta_vs_hold_per_share: Some(0.0),
            reason: "exit on bid".into(),
        };
        let intents = build_rescue_intents(&decision, &fill("no"), &snapshot, &market, 2_000);
        assert_eq!(intents.len(), 1);
        let intent = &intents[0];
        assert_eq!(intent.side, TradeSide::Sell);
        assert_eq!(intent.kind, IntentKind::Close);
        assert!(intent.reduce_only);
        assert_eq!(intent.instrument_id, InstrumentId::from("no"));
        let no_bid = 0.55;
        let tick = market.tick_size();
        let expected = (no_bid - 3.0 * tick).clamp(tick, 0.999);
        assert!(
            (intent.limit_price - expected).abs() < 1e-9,
            "price should hit own bid - race_buffer"
        );
    }

    #[test]
    fn build_rescue_intents_empty_when_opposite_book_empty() {
        let (market, mut snapshot) = fixtures();
        snapshot.no_quote.best_ask = None;
        let decision = RescueDecision {
            action: RescueAction::BuyOppositeForMerge,
            qty: 5.0,
            hold_value_per_share: 0.40,
            rescue_value_per_share: None,
            delta_vs_hold_per_share: None,
            reason: "no liquidity".into(),
        };
        let intents = build_rescue_intents(&decision, &fill("yes"), &snapshot, &market, 1_000);
        assert!(
            intents.is_empty(),
            "must skip rescue when opposite leg has no asks to lift"
        );
    }
}
