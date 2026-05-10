//! Deep paired-MM interface.
//!
//! Callers should prefer this facade over wiring ladder/rescue modules by hand.
//! It encodes the intended decision order:
//! 1. close/flatten decisions are considered first,
//! 2. entry ladders are generated only when hard risk state permits,
//! 3. all output remains proposed intents for the runtime hard risk boundary.

use std::sync::atomic::AtomicU64;

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
use crate::market_making::pairing::types::{
    LadderLeg, PairedInventorySnapshot, PairedMarketSnapshot,
};
use crate::markets::MarketDescriptor;
use crate::signals::{
    BookSanityConfig, BookSanitySignal, BtcRegime, BtcRegimeSnapshot, FairValueEstimate,
    MomentumSignal, OrderBookPressureSignal, ReversalConfig, ReversalSignal, SideScoreConfig,
    SideScoreSignal,
};
use crate::strategies::traits::PairedOpenOrderExposure;
use crate::types::{
    ClientOrderId, CoolingReason, EpochMillis, FillReport, IntentKind, MmQuoteKind, OrderIntent,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConvexityOverlayConfig {
    pub enabled: bool,
    pub suppress_ladder_when_package_fires: bool,
    pub start_frac: f64,
    pub capital_pct: f64,
    pub fractional_kelly: f64,
    pub max_loss_usd: f64,
    pub max_book_take_pct: f64,
    pub min_order_usd: f64,
    pub max_active_orders_per_leg: usize,
    pub late_window_sec: u64,
    pub convex_p_threshold: f64,
    pub maker_safety_ticks: f64,
    pub min_favorite_edge_bps: f64,
    pub tail_enabled: bool,
    pub max_favorite_win_loss_usd: f64,
    pub min_tail_win_profit_usd: f64,
    pub min_tail_payoff_multiple: f64,
}

impl Default for ConvexityOverlayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            suppress_ladder_when_package_fires: true,
            start_frac: 0.60,
            capital_pct: 0.10,
            fractional_kelly: 0.15,
            max_loss_usd: 40.0,
            max_book_take_pct: 0.20,
            min_order_usd: 0.25,
            max_active_orders_per_leg: 3,
            late_window_sec: 120,
            convex_p_threshold: 0.72,
            maker_safety_ticks: 2.0,
            min_favorite_edge_bps: 25.0,
            tail_enabled: true,
            max_favorite_win_loss_usd: 8.0,
            min_tail_win_profit_usd: 10.0,
            min_tail_payoff_multiple: 20.0,
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
    pub reversal: ReversalConfig,
    pub book_sanity: BookSanityConfig,
    pub side_score: SideScoreConfig,
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
            reversal: ReversalConfig::default(),
            book_sanity: BookSanityConfig::default(),
            side_score: SideScoreConfig::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PairedMmInput<M> {
    pub market: M,
    pub snapshot: PairedMarketSnapshot,
    pub inventory: PairedInventorySnapshot,
    pub open_convex_order_exposure: PairedOpenOrderExposure,
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
    pub reversal: ReversalSignal,
    pub book_sanity: BookSanitySignal,
    pub side_score: SideScoreSignal,
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
        let reversal = ReversalSignal::compute(
            &input.fair_value,
            &input.momentum,
            &input.order_book_pressure,
            self.config.reversal,
        );
        let book_sanity =
            BookSanitySignal::compute(&input.snapshot, input.now_ms, self.config.book_sanity);
        let remaining_ms = input
            .market
            .time_remaining_ms(input.now_ms)
            .unwrap_or(input.market.window_ms());
        let side_score = SideScoreSignal::compute(
            &input.fair_value,
            &input.snapshot,
            &input.momentum,
            &input.order_book_pressure,
            &reversal,
            &book_sanity,
            remaining_ms,
            input.market.window_ms(),
            self.config.side_score,
        );

        let mut ladder = match &hard_policy.action {
            HardPolicyAction::Allow => build_ladder(
                &input.market,
                &input.snapshot,
                &input.inventory,
                &input.fair_value,
                &input.btc_regime,
                &input.momentum,
                &input.order_book_pressure,
                &side_score,
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
                    &side_score,
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
                &input.open_convex_order_exposure,
                &input.fair_value,
                &input.btc_regime,
                &input.order_book_pressure,
                &side_score,
                self.config.convexity_overlay,
                input.now_ms,
            )
        } else {
            Vec::new()
        };
        if self
            .config
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
            let decision_label = intent
                .quote_level_tag
                .as_deref()
                .map(|tag| {
                    if tag.contains("ultra-cheap-tail") {
                        "cheap_tail_convexity"
                    } else if tag.contains("late-favorite") {
                        "late_favorite_loading"
                    } else {
                        "late_asymmetric_convex"
                    }
                })
                .unwrap_or("late_asymmetric_convex");
            notes.push(format!(
                "paired-mm decision_label={decision_label} mode=convex_tilt intent={} price={:.4} qty={:.4} notional={:.4} reason={}",
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
            reversal,
            book_sanity,
            side_score,
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

/// Diagnostic counters for late-asymmetric-convex gate rejections. These
/// are global so the rayon-parallel replay can write into them without
/// threading state through every strategy call. Dumped via
/// `print_convex_overlay_gate_counts` at the end of a run so we can see
/// which gate is dropping the late-favourite path on real data.
static CONVEX_GATE_DISABLED: AtomicU64 = AtomicU64::new(0);
static CONVEX_GATE_OUTSIDE_LATE_WINDOW: AtomicU64 = AtomicU64::new(0);
static CONVEX_GATE_PROB_BELOW_THRESHOLD: AtomicU64 = AtomicU64::new(0);
static CONVEX_GATE_FAVORITE_OPEN_FULL: AtomicU64 = AtomicU64::new(0);
static CONVEX_GATE_BAD_PASSIVE_PRICE: AtomicU64 = AtomicU64::new(0);
static CONVEX_GATE_EDGE_TOO_THIN: AtomicU64 = AtomicU64::new(0);
static CONVEX_GATE_PLAN_REJECTED: AtomicU64 = AtomicU64::new(0);
static CONVEX_GATE_PASSED: AtomicU64 = AtomicU64::new(0);
/// Within-late-window p_max histogram (max(p_up, p_down) at decision time).
/// Granularity: 0.05 buckets from 0.50 upward. Tells us whether fair-value
/// is producing actionable directional signal in the convex eligibility
/// window without flooding stderr with per-event prints.
static CONVEX_PMAX_50_55: AtomicU64 = AtomicU64::new(0);
static CONVEX_PMAX_55_60: AtomicU64 = AtomicU64::new(0);
static CONVEX_PMAX_60_65: AtomicU64 = AtomicU64::new(0);
static CONVEX_PMAX_65_70: AtomicU64 = AtomicU64::new(0);
static CONVEX_PMAX_70_75: AtomicU64 = AtomicU64::new(0);
static CONVEX_PMAX_75_80: AtomicU64 = AtomicU64::new(0);
static CONVEX_PMAX_80_85: AtomicU64 = AtomicU64::new(0);
static CONVEX_PMAX_85_90: AtomicU64 = AtomicU64::new(0);
static CONVEX_PMAX_90_PLUS: AtomicU64 = AtomicU64::new(0);
/// Counts of fair-value model branches reached during late-window evaluation.
static CONVEX_FAIR_NOSIGNAL: AtomicU64 = AtomicU64::new(0);
static CONVEX_FAIR_BSM: AtomicU64 = AtomicU64::new(0);
static CONVEX_FAIR_STEP: AtomicU64 = AtomicU64::new(0);
/// NoSignal reason buckets: which input was missing/invalid when fair-value
/// fell back. Spot = BTC last price; Strike = market.price_to_beat;
/// Vol = regime.realized_vol_5m_bps; Time = remaining seconds.
static CONVEX_NOSIGNAL_SPOT: AtomicU64 = AtomicU64::new(0);
static CONVEX_NOSIGNAL_STRIKE: AtomicU64 = AtomicU64::new(0);
static CONVEX_NOSIGNAL_VOL: AtomicU64 = AtomicU64::new(0);
static CONVEX_NOSIGNAL_TIME: AtomicU64 = AtomicU64::new(0);

/// PriceToBeat delivery counters: did the synthesised PriceToBeat events
/// reach the registered market and update its strike?
pub static PRICE_TO_BEAT_DELIVERED: AtomicU64 = AtomicU64::new(0);
pub static PRICE_TO_BEAT_NO_MARKET: AtomicU64 = AtomicU64::new(0);

pub fn print_convex_overlay_gate_counts() {
    let load = |a: &AtomicU64| a.load(std::sync::atomic::Ordering::Relaxed);
    eprintln!(
        "convex_overlay_gates disabled={} outside_late_window={} prob_below_threshold={} favorite_open_full={} bad_passive_price={} edge_too_thin={} plan_rejected={} passed={}",
        load(&CONVEX_GATE_DISABLED),
        load(&CONVEX_GATE_OUTSIDE_LATE_WINDOW),
        load(&CONVEX_GATE_PROB_BELOW_THRESHOLD),
        load(&CONVEX_GATE_FAVORITE_OPEN_FULL),
        load(&CONVEX_GATE_BAD_PASSIVE_PRICE),
        load(&CONVEX_GATE_EDGE_TOO_THIN),
        load(&CONVEX_GATE_PLAN_REJECTED),
        load(&CONVEX_GATE_PASSED),
    );
    eprintln!(
        "convex_late_window_pmax_buckets 0.50-0.55={} 0.55-0.60={} 0.60-0.65={} 0.65-0.70={} 0.70-0.75={} 0.75-0.80={} 0.80-0.85={} 0.85-0.90={} 0.90+={}",
        load(&CONVEX_PMAX_50_55),
        load(&CONVEX_PMAX_55_60),
        load(&CONVEX_PMAX_60_65),
        load(&CONVEX_PMAX_65_70),
        load(&CONVEX_PMAX_70_75),
        load(&CONVEX_PMAX_75_80),
        load(&CONVEX_PMAX_80_85),
        load(&CONVEX_PMAX_85_90),
        load(&CONVEX_PMAX_90_PLUS),
    );
    eprintln!(
        "convex_late_window_fair_value_model nosignal={} bsm={} step={}",
        load(&CONVEX_FAIR_NOSIGNAL),
        load(&CONVEX_FAIR_BSM),
        load(&CONVEX_FAIR_STEP),
    );
    eprintln!(
        "convex_late_window_nosignal_reasons spot={} strike={} vol={} time={}",
        load(&CONVEX_NOSIGNAL_SPOT),
        load(&CONVEX_NOSIGNAL_STRIKE),
        load(&CONVEX_NOSIGNAL_VOL),
        load(&CONVEX_NOSIGNAL_TIME),
    );
    eprintln!(
        "price_to_beat delivered={} no_market_match={}",
        load(&PRICE_TO_BEAT_DELIVERED),
        load(&PRICE_TO_BEAT_NO_MARKET),
    );
}

fn record_convex_pmax(p_max: f64) {
    let bucket = if p_max < 0.55 {
        &CONVEX_PMAX_50_55
    } else if p_max < 0.60 {
        &CONVEX_PMAX_55_60
    } else if p_max < 0.65 {
        &CONVEX_PMAX_60_65
    } else if p_max < 0.70 {
        &CONVEX_PMAX_65_70
    } else if p_max < 0.75 {
        &CONVEX_PMAX_70_75
    } else if p_max < 0.80 {
        &CONVEX_PMAX_75_80
    } else if p_max < 0.85 {
        &CONVEX_PMAX_80_85
    } else if p_max < 0.90 {
        &CONVEX_PMAX_85_90
    } else {
        &CONVEX_PMAX_90_PLUS
    };
    bucket.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

fn choose_convex_overlay<M: MarketDescriptor>(
    market: &M,
    snapshot: &PairedMarketSnapshot,
    inventory: &PairedInventorySnapshot,
    open_convex_order_exposure: &PairedOpenOrderExposure,
    fair_value: &FairValueEstimate,
    btc_regime: &BtcRegimeSnapshot,
    order_book_pressure: &OrderBookPressureSignal,
    side_score: &SideScoreSignal,
    config: ConvexityOverlayConfig,
    now_ms: EpochMillis,
) -> Vec<OrderIntent> {
    if !config.enabled {
        CONVEX_GATE_DISABLED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Vec::new();
    }
    let remaining_ms = market
        .time_remaining_ms(now_ms)
        .unwrap_or(market.window_ms());
    let start_frac_late_ms = ((1.0 - config.start_frac.clamp(0.0, 0.99))
        * market.window_ms() as f64)
        .round()
        .max(0.0) as u64;
    let late_threshold_ms = config
        .late_window_sec
        .saturating_mul(1_000)
        .max(start_frac_late_ms);
    if remaining_ms > late_threshold_ms {
        CONVEX_GATE_OUTSIDE_LATE_WINDOW.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Vec::new();
    }

    let p_max = fair_value.p_up.max(fair_value.p_down);
    record_convex_pmax(p_max);
    match fair_value.model {
        crate::signals::FairValueModel::NoSignal(reason) => {
            CONVEX_FAIR_NOSIGNAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let bucket = match reason {
                crate::signals::fair_value::NoSignalReason::SpotInvalid => &CONVEX_NOSIGNAL_SPOT,
                crate::signals::fair_value::NoSignalReason::StrikeInvalid => &CONVEX_NOSIGNAL_STRIKE,
                crate::signals::fair_value::NoSignalReason::VolInvalid => &CONVEX_NOSIGNAL_VOL,
                crate::signals::fair_value::NoSignalReason::TimeRemainingInvalid => &CONVEX_NOSIGNAL_TIME,
            };
            bucket.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        crate::signals::FairValueModel::BsmBinary => {
            CONVEX_FAIR_BSM.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        crate::signals::FairValueModel::StepFunctionDecided => {
            CONVEX_FAIR_STEP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    let (favorite, tail) = if fair_value.p_up >= config.convex_p_threshold {
        (
            ConvexLeg::yes(
                market,
                snapshot,
                inventory,
                open_convex_order_exposure,
                fair_value,
            ),
            ConvexLeg::no(
                market,
                snapshot,
                inventory,
                open_convex_order_exposure,
                fair_value,
            ),
        )
    } else if fair_value.p_down >= config.convex_p_threshold {
        (
            ConvexLeg::no(
                market,
                snapshot,
                inventory,
                open_convex_order_exposure,
                fair_value,
            ),
            ConvexLeg::yes(
                market,
                snapshot,
                inventory,
                open_convex_order_exposure,
                fair_value,
            ),
        )
    } else {
        CONVEX_GATE_PROB_BELOW_THRESHOLD.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Vec::new();
    };

    let max_active_per_leg = config.max_active_orders_per_leg.max(1);
    if favorite.open_count >= max_active_per_leg {
        CONVEX_GATE_FAVORITE_OPEN_FULL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Vec::new();
    }
    let pressure_bias = apply_side_score_to_pressure_bias(
        convex_pressure_bias(favorite.tag, order_book_pressure),
        favorite.leg,
        tail.leg,
        side_score,
    );

    let tick_size = market.tick_size().max(0.0001);
    let Some((favorite_limit_price, favorite_best_ask)) =
        passive_buy_price(favorite.quote, tick_size, config.maker_safety_ticks)
    else {
        CONVEX_GATE_BAD_PASSIVE_PRICE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Vec::new();
    };
    let favorite_edge = favorite.win_prob - favorite_limit_price;
    if favorite_edge * 10_000.0 < config.min_favorite_edge_bps.max(0.0) {
        CONVEX_GATE_EDGE_TOO_THIN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Vec::new();
    }

    let effective_favorite_qty = favorite.effective_qty();
    let effective_tail_qty = tail.effective_qty();
    let existing_cost = favorite.existing_cost_usd() + tail.existing_cost_usd();
    let existing_ev = favorite.win_prob * effective_favorite_qty
        + tail.win_prob * effective_tail_qty
        - existing_cost;
    // Budget gate uses only convex-attributed open orders, not paired-MM
    // inventory. Otherwise paired-MM accumulating favorite-side fills (which
    // it does aggressively under whale-tuned configs) suppresses convex
    // before it ever has a chance to fire. The `existing_cost` above (full
    // position) is correct for the EV calculation; this gate just needs to
    // know what convex itself has already spent.
    let convex_attributed_cost =
        favorite.convex_attributed_cost_usd() + tail.convex_attributed_cost_usd();
    let total_budget = config.max_loss_usd.max(0.0);
    let remaining_excess_budget = total_budget - convex_attributed_cost;
    if remaining_excess_budget <= 0.0 {
        CONVEX_GATE_PLAN_REJECTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Vec::new();
    }

    let favorite_depth_usd = favorite.depth_usd_or(config.min_order_usd);
    let tail_depth_usd = tail.depth_usd_or(config.min_order_usd);

    let maybe_tail_quote = if config.tail_enabled && tail.open_count < max_active_per_leg {
        passive_buy_price(tail.quote, tick_size, config.maker_safety_ticks)
    } else {
        None
    };
    let tail_price_cap = (1.0 / config.min_tail_payoff_multiple.max(1.0)).max(tick_size);
    let maybe_package_plan = maybe_tail_quote.and_then(|(tail_limit_price, tail_best_ask)| {
        if tail_best_ask > tail_price_cap {
            return None;
        }
        choose_late_asymmetric_package(
            favorite.win_prob,
            tail.win_prob,
            favorite_limit_price,
            tail_limit_price,
            effective_favorite_qty,
            effective_tail_qty,
            existing_cost,
            existing_ev,
            remaining_excess_budget,
            favorite_depth_usd,
            tail_depth_usd,
            market.min_order_size(),
            btc_regime.regime(),
            pressure_bias,
            config,
        )
        .map(|plan| (plan, tail_limit_price, tail_best_ask))
    });

    if let Some((plan, tail_limit_price, tail_best_ask)) = maybe_package_plan {
        let favorite_tag = favorite.tag;
        let tail_tag = tail.tag;
        let favorite_prob = favorite.win_prob;
        let tail_prob = tail.win_prob;
        let favorite_leg = favorite.leg;
        let tail_leg = tail.leg;
        let mut intents = Vec::with_capacity(2);
        let package_pair_id = format!(
            "paired-mm-convex-package:{}:{}:{}:{}",
            market.market_id(),
            favorite_tag,
            tail_tag,
            now_ms
        );
        let mut favorite_intent = OrderIntent::new_buy(
            ClientOrderId::from(format!(
                "paired-mm-convex:{}:{}:{}",
                market.market_id(),
                favorite_tag,
                now_ms
            )),
            market.market_id().clone(),
            favorite.instrument_id.clone(),
            favorite_limit_price,
            plan.favorite_qty,
            format!(
                "paired-mm late asymmetric package favorite leg={favorite_tag} p_win={favorite_prob:.4} price={favorite_limit_price:.4} ask={favorite_best_ask:.4} edge_bps={:.2} package_ev_delta={:.4} pnl_if_favorite={:.4} pnl_if_tail={:.4} favorite_notional={:.4} tail_notional={:.4} tail_payoff_multiple={:.2} pressure={} side_score_favorite={:.4} side_score_tail={:.4} reversal_prob={:.4} book_penalty_favorite={:.4} regime={}",
                favorite_edge * 10_000.0,
                plan.ev_delta,
                plan.pnl_if_favorite,
                plan.pnl_if_tail,
                plan.favorite_notional,
                plan.tail_notional,
                plan.tail_payoff_multiple,
                pressure_bias.label,
                side_score.leg(favorite_leg).score,
                side_score.leg(tail_leg).score,
                side_score.leg(favorite_leg).reversal_component.abs(),
                side_score.leg(favorite_leg).book_sanity_penalty,
                plan.regime_label
            ),
            now_ms,
        );
        favorite_intent.quote_level_tag = Some(format!(
            "mm-convex-accum:late-favorite:{favorite_tag}:{:?}",
            MmQuoteKind::ConvexAccumulation
        ));
        favorite_intent.kind = IntentKind::Entry;
        favorite_intent.pair_id = Some(package_pair_id.clone());
        intents.push(favorite_intent);

        let mut tail_intent = OrderIntent::new_buy(
            ClientOrderId::from(format!(
                "paired-mm-tail:{}:{}:{}",
                market.market_id(),
                tail_tag,
                now_ms
            )),
            market.market_id().clone(),
            tail.instrument_id.clone(),
            tail_limit_price,
            plan.tail_qty,
            format!(
                "paired-mm late asymmetric package ultra-cheap tail leg={tail_tag} p_win={tail_prob:.4} price={tail_limit_price:.4} ask={tail_best_ask:.4} package_ev_delta={:.4} pnl_if_favorite={:.4} pnl_if_tail={:.4} favorite_notional={:.4} tail_notional={:.4} tail_payoff_multiple={:.2} pressure={} side_score_favorite={:.4} side_score_tail={:.4} reversal_prob={:.4} book_penalty_tail={:.4} regime={}",
                plan.ev_delta,
                plan.pnl_if_favorite,
                plan.pnl_if_tail,
                plan.favorite_notional,
                plan.tail_notional,
                plan.tail_payoff_multiple,
                pressure_bias.label,
                side_score.leg(favorite_leg).score,
                side_score.leg(tail_leg).score,
                side_score.leg(favorite_leg).reversal_component.abs(),
                side_score.leg(tail_leg).book_sanity_penalty,
                plan.regime_label
            ),
            now_ms,
        );
        tail_intent.quote_level_tag = Some(format!(
            "mm-convex-accum:ultra-cheap-tail:{tail_tag}:{:?}",
            MmQuoteKind::ConvexAccumulation
        ));
        tail_intent.kind = IntentKind::Entry;
        tail_intent.pair_id = Some(package_pair_id);
        intents.push(tail_intent);

        return intents;
    }

    let Some(favorite_plan) = choose_late_favorite_only(
        favorite.win_prob,
        tail.win_prob,
        favorite_limit_price,
        effective_favorite_qty,
        effective_tail_qty,
        existing_cost,
        existing_ev,
        remaining_excess_budget,
        favorite_depth_usd,
        market.min_order_size(),
        btc_regime.regime(),
        pressure_bias,
        config,
    ) else {
        return Vec::new();
    };

    let favorite_tag = favorite.tag;
    let favorite_prob = favorite.win_prob;
    let favorite_leg = favorite.leg;
    let mut favorite_intent = OrderIntent::new_buy(
        ClientOrderId::from(format!(
            "paired-mm-convex:{}:{}:{}",
            market.market_id(),
            favorite_tag,
            now_ms
        )),
        market.market_id().clone(),
        favorite.instrument_id.clone(),
        favorite_limit_price,
        favorite_plan.qty,
        format!(
            "paired-mm late favorite-only load leg={favorite_tag} p_win={favorite_prob:.4} price={favorite_limit_price:.4} ask={favorite_best_ask:.4} edge_bps={:.2} ev_delta={:.4} pnl_if_favorite={:.4} pnl_if_other={:.4} notional={:.4} pressure={} side_score={:.4} reversal_prob={:.4} book_penalty={:.4} regime={}",
            favorite_edge * 10_000.0,
            favorite_plan.ev_delta,
            favorite_plan.pnl_if_favorite,
            favorite_plan.pnl_if_other,
            favorite_plan.notional,
            pressure_bias.label,
            side_score.leg(favorite_leg).score,
            side_score.leg(favorite_leg).reversal_component.abs(),
            side_score.leg(favorite_leg).book_sanity_penalty,
            favorite_plan.regime_label
        ),
        now_ms,
    );
    favorite_intent.quote_level_tag = Some(format!(
        "mm-convex-accum:late-favorite-only:{favorite_tag}:{:?}",
        MmQuoteKind::ConvexAccumulation
    ));
    favorite_intent.kind = IntentKind::Entry;
    vec![favorite_intent]
}

struct ConvexLeg<'a> {
    tag: &'static str,
    leg: LadderLeg,
    instrument_id: crate::types::InstrumentId,
    quote: &'a crate::types::QuoteSnapshot,
    win_prob: f64,
    current_qty: f64,
    open_qty: f64,
    open_notional: f64,
    open_count: usize,
    avg_cost: f64,
}

impl<'a> ConvexLeg<'a> {
    fn yes<M: MarketDescriptor>(
        market: &M,
        snapshot: &'a PairedMarketSnapshot,
        inventory: &PairedInventorySnapshot,
        exposure: &PairedOpenOrderExposure,
        fair_value: &FairValueEstimate,
    ) -> Self {
        Self {
            tag: "yes",
            leg: LadderLeg::Yes,
            instrument_id: market.yes_instrument_id().clone(),
            quote: &snapshot.yes_quote,
            win_prob: fair_value.p_up,
            current_qty: inventory.yes_qty,
            open_qty: exposure.yes_qty,
            open_notional: exposure.yes_notional_usd,
            open_count: exposure.yes_count,
            avg_cost: inventory.yes_avg_cost,
        }
    }

    fn no<M: MarketDescriptor>(
        market: &M,
        snapshot: &'a PairedMarketSnapshot,
        inventory: &PairedInventorySnapshot,
        exposure: &PairedOpenOrderExposure,
        fair_value: &FairValueEstimate,
    ) -> Self {
        Self {
            tag: "no",
            leg: LadderLeg::No,
            instrument_id: market.no_instrument_id().clone(),
            quote: &snapshot.no_quote,
            win_prob: fair_value.p_down,
            current_qty: inventory.no_qty,
            open_qty: exposure.no_qty,
            open_notional: exposure.no_notional_usd,
            open_count: exposure.no_count,
            avg_cost: inventory.no_avg_cost,
        }
    }

    fn effective_qty(&self) -> f64 {
        self.current_qty.max(0.0) + self.open_qty.max(0.0)
    }

    /// Total cost basis on this leg: full inventory + open orders. Used for
    /// EV calculation where existing position genuinely changes the expected
    /// payoff of adding more.
    fn existing_cost_usd(&self) -> f64 {
        self.current_qty.max(0.0) * self.avg_cost.max(0.0) + self.open_notional.max(0.0)
    }

    /// Cost the convex overlay itself has consumed (open convex orders only).
    /// Used for the convex budget gate so paired-MM inventory accumulated
    /// independently does not crowd convex out of its dedicated budget.
    fn convex_attributed_cost_usd(&self) -> f64 {
        self.open_notional.max(0.0)
    }

    fn depth_usd_or(&self, fallback: f64) -> f64 {
        self.quote
            .best_ask
            .as_ref()
            .map(|level| level.price * level.quantity)
            .unwrap_or(fallback)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LateFavoriteOnlyPlan {
    qty: f64,
    notional: f64,
    pnl_if_favorite: f64,
    pnl_if_other: f64,
    ev_delta: f64,
    regime_label: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ConvexPressureBias {
    favorite_scale: f64,
    tail_scale: f64,
    label: &'static str,
}

const FAVORITE_SCALE_MIN: f64 = 0.50;
const FAVORITE_SCALE_MAX: f64 = 1.50;
const TAIL_SCALE_MIN: f64 = 0.60;
const TAIL_SCALE_MAX: f64 = 1.25;

fn convex_pressure_bias(
    favorite_tag: &str,
    pressure: &OrderBookPressureSignal,
) -> ConvexPressureBias {
    let strength = pressure.imbalance.abs().clamp(0.0, 1.0);
    let pressure_favorite_tag = match pressure.direction {
        crate::signals::SignalDirection::Up => Some("yes"),
        crate::signals::SignalDirection::Down => Some("no"),
        crate::signals::SignalDirection::Neutral => None,
    };
    let mut bias = match pressure_favorite_tag {
        Some(tag) if tag == favorite_tag => ConvexPressureBias {
            favorite_scale: 1.0 + 0.50 * strength,
            tail_scale: 1.0 - 0.20 * strength,
            label: "supports_favorite",
        },
        Some(_) => ConvexPressureBias {
            favorite_scale: 1.0 - 0.50 * strength,
            tail_scale: 1.0 + 0.20 * strength,
            label: "opposes_favorite",
        },
        None => ConvexPressureBias {
            favorite_scale: 1.0,
            tail_scale: 1.0,
            label: "neutral",
        },
    };
    if pressure.thin_book {
        bias.favorite_scale *= 0.75;
        bias.tail_scale *= 0.75;
        bias.label = "thin_book";
    }
    bias.favorite_scale = bias
        .favorite_scale
        .clamp(FAVORITE_SCALE_MIN, FAVORITE_SCALE_MAX);
    bias.tail_scale = bias.tail_scale.clamp(TAIL_SCALE_MIN, TAIL_SCALE_MAX);
    bias
}

fn apply_side_score_to_pressure_bias(
    mut pressure_bias: ConvexPressureBias,
    favorite_leg: LadderLeg,
    tail_leg: LadderLeg,
    side_score: &SideScoreSignal,
) -> ConvexPressureBias {
    pressure_bias.favorite_scale = (pressure_bias.favorite_scale
        * side_score.leg(favorite_leg).late_convex_scale)
        .clamp(FAVORITE_SCALE_MIN, FAVORITE_SCALE_MAX);
    pressure_bias.tail_scale = (pressure_bias.tail_scale
        * side_score.leg(tail_leg).late_convex_scale)
        .clamp(TAIL_SCALE_MIN, TAIL_SCALE_MAX);
    pressure_bias
}

#[allow(clippy::too_many_arguments)]
fn choose_late_favorite_only(
    favorite_prob: f64,
    tail_prob: f64,
    favorite_price: f64,
    existing_favorite_qty: f64,
    existing_tail_qty: f64,
    existing_cost: f64,
    existing_ev: f64,
    remaining_budget_usd: f64,
    favorite_depth_usd: f64,
    min_order_size: f64,
    regime: Option<BtcRegime>,
    pressure_bias: ConvexPressureBias,
    config: ConvexityOverlayConfig,
) -> Option<LateFavoriteOnlyPlan> {
    if remaining_budget_usd <= 0.0 || favorite_price <= 0.0 || favorite_price >= 1.0 {
        return None;
    }

    let regime_label = regime
        .map(|regime| regime_label(regime))
        .unwrap_or("unknown");
    let book_take = config.max_book_take_pct.clamp(0.01, 1.0);
    let depth_cap = (favorite_depth_usd * book_take).max(config.min_order_usd);
    let risk_cap = config
        .max_favorite_win_loss_usd
        .max(config.min_order_usd)
        .min(config.max_loss_usd.max(0.0))
        .min(remaining_budget_usd);
    let terminal_confidence = ((favorite_prob - config.convex_p_threshold)
        / (1.0 - config.convex_p_threshold).max(1e-9))
    .clamp(0.0, 1.0);
    let edge = (favorite_prob - favorite_price).max(0.0);
    let kelly = edge / (1.0 - favorite_price).max(1e-9);
    let capital_budget =
        (config.max_loss_usd.max(0.0) * config.capital_pct.max(0.0)).max(config.min_order_usd);
    let mut notional = (capital_budget * config.fractional_kelly.clamp(0.0, 1.0))
        .max(config.min_order_usd)
        .max(capital_budget * terminal_confidence * config.fractional_kelly.clamp(0.0, 1.0))
        .max(capital_budget * kelly * config.fractional_kelly.clamp(0.0, 1.0))
        * pressure_bias.favorite_scale;
    notional = notional.min(depth_cap).min(risk_cap);
    if matches!(regime, Some(BtcRegime::Whipsaw)) {
        notional *= 0.75;
    }
    if notional + 1e-9 < config.min_order_usd {
        return None;
    }
    let qty = (notional / favorite_price).max(min_order_size);
    notional = qty * favorite_price;
    if notional > risk_cap + 1e-9 {
        return None;
    }
    let total_favorite_qty = existing_favorite_qty + qty;
    let total_cost = existing_cost + notional;
    let pnl_if_favorite = total_favorite_qty - total_cost;
    let pnl_if_other = existing_tail_qty - total_cost;
    let total_ev = favorite_prob * total_favorite_qty + tail_prob * existing_tail_qty - total_cost;
    let ev_delta = total_ev - existing_ev;
    if pnl_if_other + config.max_favorite_win_loss_usd < -1e-9 {
        return None;
    }

    Some(LateFavoriteOnlyPlan {
        qty,
        notional,
        pnl_if_favorite,
        pnl_if_other,
        ev_delta,
        regime_label,
    })
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
    remaining_budget_usd: f64,
    favorite_depth_usd: f64,
    tail_depth_usd: f64,
    min_order_size: f64,
    regime: Option<BtcRegime>,
    pressure_bias: ConvexPressureBias,
    config: ConvexityOverlayConfig,
) -> Option<LateAsymmetricPackagePlan> {
    if remaining_budget_usd <= 0.0 || favorite_price <= 0.0 || tail_price <= 0.0 {
        return None;
    }

    let regime_label = regime
        .map(|regime| regime_label(regime))
        .unwrap_or("unknown");
    let tail_stranded = existing_tail_qty > existing_favorite_qty + min_order_size.max(1e-9);
    let favorite_stranded = existing_favorite_qty > existing_tail_qty + min_order_size.max(1e-9);
    let risk_budget = config.max_loss_usd.max(0.0);
    let remaining_budget_usd = remaining_budget_usd.min(risk_budget).max(0.0);
    let fractional_kelly = config.fractional_kelly.clamp(0.0, 1.0);
    let capital_budget = (risk_budget * config.capital_pct.max(0.0)).max(config.min_order_usd);
    let favorite_edge = (favorite_prob - favorite_price).max(0.0);
    let favorite_kelly = if favorite_price < 1.0 {
        favorite_edge / (1.0 - favorite_price).max(1e-9)
    } else {
        0.0
    };
    let tail_edge = (tail_prob - tail_price).max(0.0);
    let tail_kelly = if tail_price < 1.0 {
        tail_edge / (1.0 - tail_price).max(1e-9)
    } else {
        0.0
    };
    let tail_payoff_at_price = 1.0 / tail_price.max(1e-9);
    if tail_payoff_at_price + 1e-9 < config.min_tail_payoff_multiple {
        return None;
    }

    let book_take = config.max_book_take_pct.clamp(0.01, 1.0);
    let favorite_depth_cap = (favorite_depth_usd * book_take).max(config.min_order_usd);
    let tail_depth_cap = (tail_depth_usd * book_take).max(config.min_order_usd);

    let structural_tail_notional =
        (risk_budget / config.min_tail_payoff_multiple.max(1.0)).max(config.min_order_usd);
    let per_leg_floor = config
        .min_order_usd
        .max(min_order_size * favorite_price)
        .max(min_order_size * tail_price)
        .min(remaining_budget_usd * 0.5);
    let terminal_confidence = ((favorite_prob - config.convex_p_threshold)
        / (1.0 - config.convex_p_threshold).max(1e-9))
    .clamp(0.0, 1.0);
    let favorite_edge_budget = capital_budget * fractional_kelly * favorite_kelly.max(0.0);
    let favorite_confidence_budget = capital_budget * fractional_kelly * terminal_confidence;
    let mut favorite_notional = favorite_edge_budget
        .max(favorite_confidence_budget)
        .max(per_leg_floor)
        * pressure_bias.favorite_scale;
    favorite_notional = favorite_notional
        .min(favorite_depth_cap)
        .min((remaining_budget_usd - per_leg_floor).max(0.0));

    if matches!(regime, Some(BtcRegime::Whipsaw)) {
        favorite_notional *= 0.90;
    }

    if tail_stranded {
        let share_deficit_cost =
            (existing_tail_qty - existing_favorite_qty).max(0.0) * favorite_price;
        favorite_notional = favorite_notional
            .max(share_deficit_cost.min(remaining_budget_usd - per_leg_floor))
            .min(favorite_depth_cap);
    } else if favorite_stranded {
        favorite_notional = favorite_notional.min(per_leg_floor.max(config.min_order_usd));
    }

    let projected_favorite_qty = existing_favorite_qty + favorite_notional / favorite_price;
    let coverage =
        ((tail_payoff_at_price / config.min_tail_payoff_multiple.max(1.0)) - 1.0).clamp(0.25, 1.0);
    let target_tail_qty = if tail_stranded {
        existing_tail_qty + per_leg_floor / tail_price
    } else {
        projected_favorite_qty * coverage
    };
    let coverage_tail_qty = (target_tail_qty - existing_tail_qty).max(0.0);
    let tail_profit_qty = (existing_cost + favorite_notional + config.min_tail_win_profit_usd
        - existing_tail_qty)
        .max(0.0)
        / (1.0 - tail_price).max(1e-9);
    let tail_edge_notional = capital_budget * fractional_kelly * tail_kelly.max(0.0);
    let desired_tail_notional = structural_tail_notional
        .max(coverage_tail_qty * tail_price)
        .max(tail_profit_qty * tail_price)
        .max(tail_edge_notional)
        .max(per_leg_floor)
        * pressure_bias.tail_scale;
    let tail_notional = desired_tail_notional
        .min(tail_depth_cap)
        .min((remaining_budget_usd - favorite_notional).max(0.0));
    if favorite_notional < config.min_order_usd && tail_notional < config.min_order_usd {
        return None;
    }

    let favorite_qty = if favorite_notional + 1e-9 >= config.min_order_usd {
        (favorite_notional / favorite_price).max(min_order_size)
    } else {
        0.0
    };
    let tail_qty = if tail_notional + 1e-9 >= config.min_order_usd {
        (tail_notional / tail_price).max(min_order_size)
    } else {
        0.0
    };
    if favorite_qty <= 0.0 || tail_qty <= 0.0 {
        return None;
    }

    let favorite_notional = favorite_qty * favorite_price;
    let tail_notional = tail_qty * tail_price;
    let new_cost = favorite_notional + tail_notional;
    if new_cost > remaining_budget_usd + 1e-9 {
        return None;
    }
    let total_favorite_qty = existing_favorite_qty + favorite_qty;
    let total_tail_qty = existing_tail_qty + tail_qty;
    let total_cost = existing_cost + new_cost;
    let pnl_if_favorite = total_favorite_qty - total_cost;
    let pnl_if_tail = total_tail_qty - total_cost;
    let total_ev = favorite_prob * total_favorite_qty + tail_prob * total_tail_qty - total_cost;
    let ev_delta = total_ev - existing_ev;
    if pnl_if_favorite + config.max_favorite_win_loss_usd < -1e-9 {
        return None;
    }
    if pnl_if_tail + 1e-9 < config.min_tail_win_profit_usd {
        return None;
    }
    let tail_payoff_multiple = total_tail_qty / total_cost.max(1e-9);
    if tail_payoff_multiple + 1e-9 < config.min_tail_payoff_multiple {
        return None;
    }

    Some(LateAsymmetricPackagePlan {
        favorite_qty,
        tail_qty,
        favorite_notional,
        tail_notional,
        pnl_if_favorite,
        pnl_if_tail,
        ev_delta,
        tail_payoff_multiple,
        regime_label,
    })
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
    use crate::markets::BinaryOutcomeMarket;
    use crate::types::{BookLevel, InstrumentId, MarketId, QuoteSnapshot};

    fn package_config() -> ConvexityOverlayConfig {
        ConvexityOverlayConfig {
            max_loss_usd: 40.0,
            capital_pct: 1.0,
            fractional_kelly: 0.25,
            min_order_usd: 0.10,
            max_book_take_pct: 1.0,
            max_favorite_win_loss_usd: 100.0,
            min_tail_win_profit_usd: 0.0,
            min_tail_payoff_multiple: 1.0,
            ..ConvexityOverlayConfig::default()
        }
    }

    fn quote(bid: f64, ask: f64, ask_qty: f64) -> QuoteSnapshot {
        QuoteSnapshot {
            best_bid: Some(BookLevel::new(bid, ask_qty)),
            best_ask: Some(BookLevel::new(ask, ask_qty)),
            bid_levels: vec![BookLevel::new(bid, ask_qty)],
            ask_levels: vec![BookLevel::new(ask, ask_qty)],
            depth_observed_at_ms: Some(250_000),
            last_trade_price: Some(ask),
            taker_buy_qty_60s: 0.0,
            taker_sell_qty_60s: 0.0,
            observed_at_ms: 250_000,
        }
    }

    fn late_market() -> BinaryOutcomeMarket {
        let mut market = BinaryOutcomeMarket::btc_5m(
            MarketId::from("market"),
            InstrumentId::from("yes"),
            InstrumentId::from("no"),
        );
        market.min_order_size = 0.1;
        market.event_start_ms = Some(0);
        market.event_end_ms = Some(300_000);
        market
    }

    fn late_snapshot() -> PairedMarketSnapshot {
        PairedMarketSnapshot {
            market_id: MarketId::from("market"),
            yes_instrument_id: InstrumentId::from("yes"),
            no_instrument_id: InstrumentId::from("no"),
            yes_quote: quote(0.98, 0.99, 10_000.0),
            no_quote: quote(0.01, 0.02, 10_000.0),
        }
    }

    fn neutral_pressure_bias() -> ConvexPressureBias {
        ConvexPressureBias {
            favorite_scale: 1.0,
            tail_scale: 1.0,
            label: "neutral",
        }
    }

    #[test]
    fn late_package_loads_99c_favorite_and_buys_1c_tail_shares() {
        let mut config = package_config();
        config.fractional_kelly = 0.06;
        config.min_tail_payoff_multiple = 20.0;
        config.min_tail_win_profit_usd = 10.0;

        let plan = choose_late_asymmetric_package(
            0.985,
            0.015,
            0.99,
            0.01,
            0.0,
            0.0,
            0.0,
            0.0,
            40.0,
            10_000.0,
            10_000.0,
            0.1,
            Some(BtcRegime::DirectionalSmooth),
            neutral_pressure_bias(),
            config,
        )
        .expect("99c/1c late package should be viable");

        assert!(
            plan.favorite_notional > plan.tail_notional,
            "favorite should get the dollar allocation, got favorite=${:.4} tail=${:.4}",
            plan.favorite_notional,
            plan.tail_notional
        );
        assert!(
            plan.tail_qty > plan.favorite_qty,
            "cheap tail should dominate share count, got favorite_qty={:.4} tail_qty={:.4}",
            plan.favorite_qty,
            plan.tail_qty
        );
        assert!(plan.tail_payoff_multiple >= 20.0);
        assert!(plan.pnl_if_tail >= 10.0);
    }

    #[test]
    fn late_convex_overlay_counts_open_convex_orders_against_rebuy_budget() {
        let mut config = package_config();
        config.enabled = true;
        config.fractional_kelly = 0.06;
        config.max_loss_usd = 40.0;
        config.min_tail_payoff_multiple = 20.0;
        config.min_tail_win_profit_usd = 10.0;

        let market = late_market();
        let snapshot = late_snapshot();
        let fair_value = FairValueEstimate {
            p_up: 0.985,
            p_down: 0.015,
            log_moneyness: 0.0,
            sigma_remaining: 0.0,
            time_remaining_s: 50.0,
            model: crate::signals::FairValueModel::NoSignal(
                crate::signals::fair_value::NoSignalReason::SpotInvalid,
            ),
        };
        let without_open_orders = choose_convex_overlay(
            &market,
            &snapshot,
            &PairedInventorySnapshot::default(),
            &PairedOpenOrderExposure::default(),
            &fair_value,
            &BtcRegimeSnapshot::default(),
            &OrderBookPressureSignal::default(),
            &SideScoreSignal::default(),
            config,
            250_000,
        );
        assert!(
            !without_open_orders.is_empty(),
            "baseline late convex package should fire with unused budget"
        );
        assert_eq!(without_open_orders.len(), 2);
        let package_pair_id = without_open_orders[0]
            .pair_id
            .as_ref()
            .expect("favorite package leg should carry pair_id");
        assert_eq!(
            without_open_orders[1].pair_id.as_ref(),
            Some(package_pair_id),
            "late asymmetric package legs must share pair_id"
        );

        let saturated_open_orders = PairedOpenOrderExposure {
            yes_qty: 20.0 / 0.98,
            yes_notional_usd: 20.0,
            yes_count: 3,
            no_qty: 20.0 / 0.01,
            no_notional_usd: 20.0,
            no_count: 3,
        };
        let with_open_orders = choose_convex_overlay(
            &market,
            &snapshot,
            &PairedInventorySnapshot::default(),
            &saturated_open_orders,
            &fair_value,
            &BtcRegimeSnapshot::default(),
            &OrderBookPressureSignal::default(),
            &SideScoreSignal::default(),
            config,
            250_000,
        );
        assert!(
            with_open_orders.is_empty(),
            "active open convex orders should consume package budget before rebuying"
        );
    }

    #[test]
    fn late_convex_overlay_ignores_paired_mm_inventory_in_budget_check() {
        // Whale-v1 backtest finding: paired-MM accumulated favorite-side
        // inventory aggressively, and the prior `existing_cost` formula
        // included that full position cost. Convex saw remaining_excess_budget
        // <= 0 and silently suppressed itself. This test pins the new
        // behavior: paired-MM-attributed inventory does not eat the convex
        // budget; only convex's own open orders do.
        let mut config = package_config();
        config.enabled = true;
        config.fractional_kelly = 0.06;
        config.max_loss_usd = 40.0;
        config.min_tail_payoff_multiple = 20.0;
        config.min_tail_win_profit_usd = 10.0;

        let market = late_market();
        let snapshot = late_snapshot();
        let fair_value = FairValueEstimate {
            p_up: 0.985,
            p_down: 0.015,
            log_moneyness: 0.0,
            sigma_remaining: 0.0,
            time_remaining_s: 50.0,
            model: crate::signals::FairValueModel::NoSignal(
                crate::signals::fair_value::NoSignalReason::SpotInvalid,
            ),
        };
        // Paired-MM has accumulated ~$50 of favorite-side inventory through
        // its market making. Under the buggy formula this would consume the
        // entire $40 convex budget and suppress every package. The new
        // formula uses convex-attributed open orders only (zero here), so
        // convex should fire.
        let modest_paired_inventory = PairedInventorySnapshot {
            yes_qty: 50.0 / 0.985,
            yes_avg_cost: 0.985,
            ..PairedInventorySnapshot::default()
        };
        let no_open_convex_orders = PairedOpenOrderExposure::default();

        let intents = choose_convex_overlay(
            &market,
            &snapshot,
            &modest_paired_inventory,
            &no_open_convex_orders,
            &fair_value,
            &BtcRegimeSnapshot::default(),
            &OrderBookPressureSignal::default(),
            &SideScoreSignal::default(),
            config,
            250_000,
        );
        assert!(
            !intents.is_empty(),
            "paired-MM inventory must not consume convex budget; \
             convex package should still fire when its own open orders are zero"
        );
    }

    #[test]
    fn late_package_keeps_tail_share_coverage_at_96c_2c() {
        let mut config = package_config();
        config.fractional_kelly = 0.06;
        config.min_tail_payoff_multiple = 20.0;
        config.min_tail_win_profit_usd = 10.0;

        let plan = choose_late_asymmetric_package(
            0.96,
            0.04,
            0.96,
            0.02,
            0.0,
            0.0,
            0.0,
            0.0,
            40.0,
            10_000.0,
            10_000.0,
            0.1,
            Some(BtcRegime::DirectionalSmooth),
            neutral_pressure_bias(),
            config,
        )
        .expect("96c/2c late package should be viable");

        assert!(plan.favorite_notional > plan.tail_notional);
        assert!(plan.tail_qty > plan.favorite_qty);
        assert!(plan.tail_payoff_multiple >= 20.0);
    }

    #[test]
    fn side_score_adjusted_convex_pressure_bias_stays_inside_pressure_bounds() {
        let side_score = SideScoreSignal {
            yes: crate::signals::SideScoreLeg {
                late_convex_scale: 2.50,
                ..Default::default()
            },
            no: crate::signals::SideScoreLeg {
                late_convex_scale: 0.10,
                ..Default::default()
            },
            favorite_leg: Some(LadderLeg::Yes),
            confidence: 1.0,
        };
        let pressure_bias = ConvexPressureBias {
            favorite_scale: FAVORITE_SCALE_MAX,
            tail_scale: TAIL_SCALE_MIN,
            label: "test",
        };
        let adjusted = apply_side_score_to_pressure_bias(
            pressure_bias,
            LadderLeg::Yes,
            LadderLeg::No,
            &side_score,
        );

        assert_eq!(adjusted.favorite_scale, FAVORITE_SCALE_MAX);
        assert_eq!(adjusted.tail_scale, TAIL_SCALE_MIN);
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
            100.0,
            100.0,
            0.1,
            Some(BtcRegime::Whipsaw),
            neutral_pressure_bias(),
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
            100.0,
            100.0,
            0.1,
            Some(BtcRegime::Whipsaw),
            neutral_pressure_bias(),
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
            100.0,
            100.0,
            0.1,
            Some(BtcRegime::DirectionalSmooth),
            neutral_pressure_bias(),
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
