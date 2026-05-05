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
use crate::signals::{
    BtcRegime, BtcRegimeSnapshot, FairValueEstimate, MomentumSignal, OrderBookPressureSignal,
};
use crate::types::{
    ClientOrderId, CoolingReason, EpochMillis, FillReport, IntentKind, MmQuoteKind, OrderIntent,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConvexityOverlayConfig {
    pub enabled: bool,
    pub suppress_ladder_when_package_fires: bool,
    pub late_window_sec: u64,
    pub convex_p_threshold: f64,
    pub max_excess_usd: f64,
    pub clip_usd: f64,
    pub maker_safety_ticks: f64,
    pub min_favorite_edge_bps: f64,
    pub tail_enabled: bool,
    pub tail_max_price: f64,
    pub tail_clip_usd: f64,
    pub min_combo_ev_usd: f64,
    pub ev_gate_enabled: bool,
    pub package_budget_usd: f64,
    pub favorite_notional_min_pct: f64,
    pub favorite_notional_max_pct: f64,
    pub whipsaw_favorite_notional_min_pct: f64,
    pub whipsaw_favorite_notional_max_pct: f64,
    pub loser_stranded_favorite_notional_min_pct: f64,
    pub loser_stranded_favorite_notional_max_pct: f64,
    pub winner_stranded_favorite_notional_min_pct: f64,
    pub winner_stranded_favorite_notional_max_pct: f64,
    pub max_favorite_win_loss_usd: f64,
    pub min_tail_win_profit_usd: f64,
    pub min_tail_payoff_multiple: f64,
    pub ev_delta_weight: f64,
    pub favorite_loss_reduction_weight: f64,
    pub tail_convexity_weight: f64,
    pub imbalance_penalty_weight: f64,
}

impl Default for ConvexityOverlayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            suppress_ladder_when_package_fires: true,
            late_window_sec: 120,
            convex_p_threshold: 0.72,
            max_excess_usd: 40.0,
            clip_usd: 4.0,
            maker_safety_ticks: 2.0,
            min_favorite_edge_bps: 25.0,
            tail_enabled: true,
            tail_max_price: 0.02,
            tail_clip_usd: 2.0,
            min_combo_ev_usd: 0.0,
            ev_gate_enabled: false,
            package_budget_usd: 15.0,
            favorite_notional_min_pct: 0.80,
            favorite_notional_max_pct: 0.97,
            whipsaw_favorite_notional_min_pct: 0.50,
            whipsaw_favorite_notional_max_pct: 0.70,
            loser_stranded_favorite_notional_min_pct: 0.75,
            loser_stranded_favorite_notional_max_pct: 0.90,
            winner_stranded_favorite_notional_min_pct: 0.45,
            winner_stranded_favorite_notional_max_pct: 0.65,
            max_favorite_win_loss_usd: 8.0,
            min_tail_win_profit_usd: 10.0,
            min_tail_payoff_multiple: 4.0,
            ev_delta_weight: 0.0,
            favorite_loss_reduction_weight: 0.0,
            tail_convexity_weight: 1.0,
            imbalance_penalty_weight: 0.0,
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
    pub momentum: MomentumSignal,
    pub order_book_pressure: OrderBookPressureSignal,
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
    pub convex_overlay: Vec<OrderIntent>,
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

        let rescue_inputs = input.rescue.or_else(|| {
            derive_tick_rescue_inputs(
                &input.snapshot,
                &input.inventory,
                &input.fair_value,
                self.config.capital_recycle.min_imbalance_qty,
            )
        });
        let rescue =
            rescue_inputs.map(|rescue_inputs| choose_rescue(rescue_inputs, self.config.rescue));

        let mut ladder = match &hard_policy.action {
            HardPolicyAction::Allow => build_ladder(
                &input.market,
                &input.snapshot,
                &input.inventory,
                &input.fair_value,
                &input.btc_regime,
                &input.momentum,
                &input.order_book_pressure,
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
                    &input.momentum,
                    &input.order_book_pressure,
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
                &input.btc_regime,
                self.config.convexity_overlay,
                input.now_ms,
            )
        } else {
            Vec::new()
        };
        if self.config
            .convexity_overlay
            .suppress_ladder_when_package_fires
            && !convex_overlay.is_empty()
        {
            ladder.intents.clear();
            ladder.diagnostics.notes.push(
                "paired-mm normal ladder suppressed because late asymmetric package fired"
                    .to_string(),
            );
        }

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
        for intent in &convex_overlay {
            notes.push(format!(
                "paired-mm decision_label=late_asymmetric_convex mode=convex_tilt intent={} price={:.4} qty={:.4} notional={:.4} reason={}",
                intent.client_order_id,
                intent.limit_price,
                intent.quantity,
                intent.notional_usd(),
                intent.reason
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

fn derive_tick_rescue_inputs(
    snapshot: &PairedMarketSnapshot,
    inventory: &PairedInventorySnapshot,
    fair_value: &FairValueEstimate,
    min_imbalance_qty: f64,
) -> Option<RescueInputs> {
    let stranded_qty = inventory.side_imbalance_qty();
    if stranded_qty < min_imbalance_qty.max(0.0).max(1e-9) {
        return None;
    }

    if inventory.yes_qty > inventory.no_qty {
        Some(RescueInputs {
            leg: crate::market_making::pairing::types::LadderLeg::Yes,
            stranded_qty,
            avg_cost: inventory.yes_avg_cost,
            fair_win_prob: fair_value.p_up,
            best_exit_bid: snapshot
                .yes_quote
                .best_bid
                .as_ref()
                .map(|level| level.price),
            opposite_best_ask: snapshot.no_quote.best_ask.as_ref().map(|level| level.price),
        })
    } else if inventory.no_qty > inventory.yes_qty {
        Some(RescueInputs {
            leg: crate::market_making::pairing::types::LadderLeg::No,
            stranded_qty,
            avg_cost: inventory.no_avg_cost,
            fair_win_prob: fair_value.p_down,
            best_exit_bid: snapshot.no_quote.best_bid.as_ref().map(|level| level.price),
            opposite_best_ask: snapshot
                .yes_quote
                .best_ask
                .as_ref()
                .map(|level| level.price),
        })
    } else {
        None
    }
}

fn choose_convex_overlay<M: MarketDescriptor>(
    market: &M,
    snapshot: &PairedMarketSnapshot,
    inventory: &PairedInventorySnapshot,
    fair_value: &FairValueEstimate,
    btc_regime: &BtcRegimeSnapshot,
    config: ConvexityOverlayConfig,
    now_ms: EpochMillis,
) -> Vec<OrderIntent> {
    if !config.enabled {
        return Vec::new();
    }
    let remaining_ms = market
        .time_remaining_ms(now_ms)
        .unwrap_or(market.window_ms());
    if remaining_ms > config.late_window_sec.saturating_mul(1_000) {
        return Vec::new();
    }

    let (
        favorite_tag,
        favorite_instrument_id,
        favorite_quote,
        favorite_prob,
        favorite_current_qty,
        _opposite_qty,
        favorite_avg_cost,
        tail_tag,
        tail_instrument_id,
        tail_quote,
        tail_prob,
        tail_current_qty,
        tail_avg_cost,
    ) =
        if fair_value.p_up >= config.convex_p_threshold {
            (
                "yes",
                market.yes_instrument_id().clone(),
                &snapshot.yes_quote,
                fair_value.p_up,
                inventory.yes_qty,
                inventory.no_qty,
                inventory.yes_avg_cost,
                "no",
                market.no_instrument_id().clone(),
                &snapshot.no_quote,
                fair_value.p_down,
                inventory.no_qty,
                inventory.no_avg_cost,
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
                "yes",
                market.yes_instrument_id().clone(),
                &snapshot.yes_quote,
                fair_value.p_up,
                inventory.yes_qty,
                inventory.yes_avg_cost,
            )
        } else {
            return Vec::new();
        };

    let tick_size = market.tick_size().max(0.0001);
    let Some((favorite_limit_price, favorite_best_ask)) =
        passive_buy_price(favorite_quote, tick_size, config.maker_safety_ticks)
    else {
        return Vec::new();
    };
    let favorite_edge = favorite_prob - favorite_limit_price;
    if favorite_edge * 10_000.0 < config.min_favorite_edge_bps.max(0.0) {
        return Vec::new();
    }

    if !config.tail_enabled {
        return Vec::new();
    }
    let Some((tail_limit_price, tail_best_ask)) =
        passive_buy_price(tail_quote, tick_size, config.maker_safety_ticks)
    else {
        return Vec::new();
    };
    if tail_best_ask > config.tail_max_price.max(tick_size) {
        return Vec::new();
    }

    let existing_favorite_cost = favorite_current_qty.max(0.0) * favorite_avg_cost.max(0.0);
    let existing_tail_cost = tail_current_qty.max(0.0) * tail_avg_cost.max(0.0);
    let existing_cost = existing_favorite_cost + existing_tail_cost;
    let existing_ev = favorite_prob * favorite_current_qty.max(0.0)
        + tail_prob * tail_current_qty.max(0.0)
        - existing_cost;
    let remaining_excess_budget = config.max_excess_usd - existing_cost;
    if remaining_excess_budget <= 0.0 {
        return Vec::new();
    }

    let budget = config.package_budget_usd.min(remaining_excess_budget).max(0.0);
    let Some(plan) = choose_late_asymmetric_package(
        favorite_prob,
        tail_prob,
        favorite_limit_price,
        tail_limit_price,
        favorite_current_qty.max(0.0),
        tail_current_qty.max(0.0),
        existing_cost,
        existing_ev,
        budget,
        market.min_order_size(),
        btc_regime.regime(),
        config,
    ) else {
        return Vec::new();
    };

    let mut intents = Vec::with_capacity(2);
    let mut favorite_intent = OrderIntent::new_buy(
        ClientOrderId::from(format!(
            "paired-mm-convex:{}:{}:{}",
            market.market_id(),
            favorite_tag,
            now_ms
        )),
        market.market_id().clone(),
        favorite_instrument_id,
        favorite_limit_price,
        plan.favorite_qty,
        format!(
            "paired-mm late asymmetric package favorite leg={favorite_tag} p_win={favorite_prob:.4} price={favorite_limit_price:.4} ask={favorite_best_ask:.4} edge_bps={:.2} package_ev_delta={:.4} pnl_if_favorite={:.4} pnl_if_tail={:.4} favorite_notional={:.4} tail_notional={:.4} tail_payoff_multiple={:.2} regime={}",
            favorite_edge * 10_000.0,
            plan.ev_delta,
            plan.pnl_if_favorite,
            plan.pnl_if_tail,
            plan.favorite_notional,
            plan.tail_notional,
            plan.tail_payoff_multiple,
            plan.regime_label
        ),
        now_ms,
    );
    favorite_intent.quote_level_tag = Some(format!(
        "mm-convex-accum:late-favorite:{favorite_tag}:{:?}",
        MmQuoteKind::ConvexAccumulation
    ));
    favorite_intent.kind = IntentKind::Entry;
    intents.push(favorite_intent);

    let mut tail_intent = OrderIntent::new_buy(
        ClientOrderId::from(format!(
            "paired-mm-tail:{}:{}:{}",
            market.market_id(),
            tail_tag,
            now_ms
        )),
        market.market_id().clone(),
        tail_instrument_id,
        tail_limit_price,
        plan.tail_qty,
        format!(
            "paired-mm late asymmetric package ultra-cheap tail leg={tail_tag} p_win={tail_prob:.4} price={tail_limit_price:.4} ask={tail_best_ask:.4} package_ev_delta={:.4} pnl_if_favorite={:.4} pnl_if_tail={:.4} favorite_notional={:.4} tail_notional={:.4} tail_payoff_multiple={:.2} regime={}",
            plan.ev_delta,
            plan.pnl_if_favorite,
            plan.pnl_if_tail,
            plan.favorite_notional,
            plan.tail_notional,
            plan.tail_payoff_multiple,
            plan.regime_label
        ),
        now_ms,
    );
    tail_intent.quote_level_tag = Some(format!(
        "mm-convex-accum:ultra-cheap-tail:{tail_tag}:{:?}",
        MmQuoteKind::ConvexAccumulation
    ));
    tail_intent.kind = IntentKind::Entry;
    intents.push(tail_intent);

    intents
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LateAsymmetricPackagePlan {
    favorite_qty: f64,
    tail_qty: f64,
    favorite_notional: f64,
    tail_notional: f64,
    pnl_if_favorite: f64,
    pnl_if_tail: f64,
    ev_delta: f64,
    tail_payoff_multiple: f64,
    regime_label: &'static str,
}

#[allow(clippy::too_many_arguments)]
fn choose_late_asymmetric_package(
    favorite_prob: f64,
    tail_prob: f64,
    favorite_price: f64,
    tail_price: f64,
    existing_favorite_qty: f64,
    existing_tail_qty: f64,
    existing_cost: f64,
    existing_ev: f64,
    budget_usd: f64,
    min_order_size: f64,
    regime: Option<BtcRegime>,
    config: ConvexityOverlayConfig,
) -> Option<LateAsymmetricPackagePlan> {
    if budget_usd <= 0.0 || favorite_price <= 0.0 || tail_price <= 0.0 {
        return None;
    }

    let regime_label = regime.map(|regime| regime_label(regime)).unwrap_or("unknown");
    let (mut configured_min_pct, mut configured_max_pct) = match regime {
        Some(BtcRegime::Whipsaw) => (
            config.whipsaw_favorite_notional_min_pct,
            config.whipsaw_favorite_notional_max_pct,
        ),
        _ => (
            config.favorite_notional_min_pct,
            config.favorite_notional_max_pct,
        ),
    };
    let tail_stranded = existing_tail_qty > existing_favorite_qty + min_order_size.max(1e-9);
    let favorite_stranded = existing_favorite_qty > existing_tail_qty + min_order_size.max(1e-9);
    if tail_stranded {
        configured_min_pct = configured_min_pct.max(config.loser_stranded_favorite_notional_min_pct);
        configured_max_pct = configured_max_pct.max(config.loser_stranded_favorite_notional_max_pct);
    } else if favorite_stranded {
        configured_min_pct = configured_min_pct.min(config.winner_stranded_favorite_notional_min_pct);
        configured_max_pct = configured_max_pct.min(config.winner_stranded_favorite_notional_max_pct);
    }
    let min_pct = configured_min_pct.clamp(0.01, 0.99);
    let max_pct = configured_max_pct.clamp(min_pct, 0.995);
    let steps = 8;
    let mut best: Option<LateAsymmetricPackagePlan> = None;

    for step in 0..=steps {
        let pct = min_pct + (max_pct - min_pct) * (step as f64 / steps as f64);
        let favorite_target_notional = budget_usd * pct;
        let tail_target_notional = budget_usd - favorite_target_notional;
        if favorite_target_notional <= 0.0 || tail_target_notional <= 0.0 {
            continue;
        }

        let favorite_qty = (favorite_target_notional / favorite_price).max(min_order_size);
        let tail_qty = (tail_target_notional / tail_price).max(min_order_size);
        let favorite_notional = favorite_qty * favorite_price;
        let tail_notional = tail_qty * tail_price;
        let new_cost = favorite_notional + tail_notional;
        if new_cost > budget_usd + 1e-9 {
            continue;
        }

        let total_favorite_qty = existing_favorite_qty + favorite_qty;
        let total_tail_qty = existing_tail_qty + tail_qty;
        let total_cost = existing_cost + new_cost;
        let pnl_if_favorite = total_favorite_qty - total_cost;
        let pnl_if_tail = total_tail_qty - total_cost;
        let total_ev = favorite_prob * total_favorite_qty + tail_prob * total_tail_qty - total_cost;
        let ev_delta = total_ev - existing_ev;
        if config.ev_gate_enabled && ev_delta + 1e-9 < config.min_combo_ev_usd {
            continue;
        }
        if pnl_if_favorite + config.max_favorite_win_loss_usd < -1e-9 {
            continue;
        }
        if pnl_if_tail + 1e-9 < config.min_tail_win_profit_usd {
            continue;
        }
        let tail_payoff_multiple = total_tail_qty / total_cost.max(1e-9);
        if tail_payoff_multiple + 1e-9 < config.min_tail_payoff_multiple {
            continue;
        }

        let candidate = LateAsymmetricPackagePlan {
            favorite_qty,
            tail_qty,
            favorite_notional,
            tail_notional,
            pnl_if_favorite,
            pnl_if_tail,
            ev_delta,
            tail_payoff_multiple,
            regime_label,
        };
    if best
            .as_ref()
            .is_none_or(|best| {
                let candidate_imbalance_delta = (candidate.favorite_notional - candidate.tail_notional).abs();
                let best_imbalance_delta = (best.favorite_notional - best.tail_notional).abs();
                let stranded_score = |candidate: &LateAsymmetricPackagePlan| -> f64 {
                    if tail_stranded {
                        candidate.pnl_if_favorite
                    } else if favorite_stranded {
                        candidate.pnl_if_tail
                    } else {
                        candidate.tail_payoff_multiple
                    }
                };
                let candidate_score = stranded_score(&candidate)
                    + config.ev_delta_weight * candidate.ev_delta
                    + config.favorite_loss_reduction_weight * candidate.pnl_if_favorite
                    + config.tail_convexity_weight * candidate.tail_payoff_multiple
                    - config.imbalance_penalty_weight * candidate_imbalance_delta;
                let best_score = stranded_score(best)
                    + config.ev_delta_weight * best.ev_delta
                    + config.favorite_loss_reduction_weight * best.pnl_if_favorite
                    + config.tail_convexity_weight * best.tail_payoff_multiple
                    - config.imbalance_penalty_weight * best_imbalance_delta;

                if tail_stranded {
                    candidate_score > best_score
                } else if favorite_stranded {
                    candidate_score > best_score
                } else {
                    candidate_score > best_score
                }
            })
        {
            best = Some(candidate);
        }
    }

    best
}

fn regime_label(regime: BtcRegime) -> &'static str {
    match regime {
        BtcRegime::Flat => "flat",
        BtcRegime::Whipsaw => "whipsaw",
        BtcRegime::DirectionalSmooth => "directional_smooth",
        BtcRegime::TrendingVolatile => "trending_volatile",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package_config() -> ConvexityOverlayConfig {
        ConvexityOverlayConfig {
            ev_gate_enabled: false,
            package_budget_usd: 10.0,
            favorite_notional_min_pct: 0.60,
            favorite_notional_max_pct: 0.80,
            whipsaw_favorite_notional_min_pct: 0.45,
            whipsaw_favorite_notional_max_pct: 0.65,
            max_favorite_win_loss_usd: 100.0,
            min_tail_win_profit_usd: 0.0,
            min_tail_payoff_multiple: 1.0,
            ..ConvexityOverlayConfig::default()
        }
    }

    #[test]
    fn late_package_shifts_favorite_heavy_when_tail_is_stranded_loser() {
        let plan = choose_late_asymmetric_package(
            0.90,
            0.10,
            0.90,
            0.02,
            0.0,
            100.0,
            50.0,
            -40.0,
            10.0,
            0.1,
            Some(BtcRegime::Whipsaw),
            package_config(),
        )
        .expect("tail-stranded package should be viable");

        let favorite_pct = plan.favorite_notional / (plan.favorite_notional + plan.tail_notional);
        assert!(
            favorite_pct >= 0.75,
            "tail stranded loser should force favorite-heavy allocation, got {favorite_pct:.4}"
        );
        assert!(
            plan.favorite_notional > plan.tail_notional,
            "loser-stranded case should prioritize favourite dollars"
        );
    }

    #[test]
    fn late_package_shifts_tail_heavy_when_favorite_is_already_stranded() {
        let plan = choose_late_asymmetric_package(
            0.90,
            0.10,
            0.90,
            0.02,
            100.0,
            0.0,
            50.0,
            40.0,
            10.0,
            0.1,
            Some(BtcRegime::Whipsaw),
            package_config(),
        )
        .expect("favorite-stranded package should be viable");

        let favorite_pct = plan.favorite_notional / (plan.favorite_notional + plan.tail_notional);
        assert!(
            favorite_pct <= 0.65,
            "favorite stranded should shift allocation toward cheap tail, got {favorite_pct:.4}"
        );
        assert!(
            plan.tail_qty > plan.favorite_qty,
            "cheap tail should accumulate more shares than favorite"
        );
    }

    #[test]
    fn late_package_can_ignore_negative_model_ev_when_structural_bounds_pass() {
        let mut config = package_config();
        config.min_tail_payoff_multiple = 4.0;
        config.min_tail_win_profit_usd = 10.0;
        config.max_favorite_win_loss_usd = 20.0;

        let plan = choose_late_asymmetric_package(
            0.98,
            0.005,
            0.99,
            0.01,
            0.0,
            0.0,
            0.0,
            0.0,
            10.0,
            0.1,
            Some(BtcRegime::DirectionalSmooth),
            config,
        )
        .expect("structurally convex package should not require positive model EV");

        assert!(
            plan.ev_delta < 0.0,
            "test setup should demonstrate EV gate is advisory, got {:.4}",
            plan.ev_delta
        );
        assert!(plan.tail_payoff_multiple >= 4.0);
        assert!(plan.pnl_if_tail >= 10.0);
    }
}

fn passive_buy_price(
    quote: &crate::types::QuoteSnapshot,
    tick_size: f64,
    maker_safety_ticks: f64,
) -> Option<(f64, f64)> {
    let best_ask = quote.best_ask.as_ref()?.price;
    let best_bid = quote
        .best_bid
        .as_ref()
        .map(|level| level.price)
        .unwrap_or(tick_size);
    if !best_ask.is_finite() || best_ask <= tick_size || best_ask >= 1.0 {
        return None;
    }

    let maker_cap = best_ask - tick_size * maker_safety_ticks.max(1.0);
    let limit_price = (best_bid + tick_size)
        .min(maker_cap)
        .clamp(tick_size, 1.0 - tick_size);
    if !limit_price.is_finite() || limit_price >= best_ask {
        return None;
    }
    Some((limit_price, best_ask))
}
