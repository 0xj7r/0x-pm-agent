//! Shared core/hedge ladder logic modeled on the `unlawful-shear` whale.
//!
//! Per `docs/research/unlawful-shear-microstructure-spec.md` the whale
//! does NOT run symmetric paired-MM around mid. Instead, on each bar:
//!
//! 1. Classifies legs by ask price: cheap (~0.29-0.31) and expensive
//!    (~0.66-0.70). The expensive leg is treated as the favorite.
//! 2. Builds the expensive leg as core (main exposure, ~67% of capital,
//!    ~$13 avg clip).
//! 3. Builds the cheap leg as hedge (~33% of capital, ~$5 avg clip).
//! 4. Continuously rebalances toward a target hedge ratio (~0.47
//!    cheap-to-expensive notional).
//! 5. Merges paired inventory aggressively to recycle capital (median
//!    first merge ~30s after entry).
//!
//! P1 (this revision): leg classifier + core/hedge ladder pricing only.
//! Merge planner integration and salvage are deferred.

use std::collections::HashMap;

use crate::core::types::{ClientOrderId, EpochMillis, IntentKind, OrderIntent, QuoteSnapshot};
use crate::market_making::pairing::types::{LadderLeg, PairedMarketSnapshot};
use crate::markets::MarketDescriptor;
use crate::signals::BtcRegime;
use crate::strategies::traits::{StrategyFillInput, StrategyInput, TradingStrategy};
use crate::types::{CoolingReason, MarketId, MergeIntent, StrategyDecision, SuppressionScope};

const PAIRED_CORE_CENTER_PROBE_MAX_LEVELS: usize = 3;
const PAIRED_CORE_CENTER_PROBE_SPAN: f64 = 0.16;
const PAIRED_CORE_CENTER_PROBE_MIN_PRICE: f64 = 0.42;
const PAIRED_CORE_CENTER_PROBE_MAX_PRICE: f64 = 0.58;
const PAIRED_CORE_REPAIR_PAIR_COST_LIMIT: f64 = 0.99;
const PAIRED_CORE_MERGE_PAIR_COST_LIMIT: f64 = 0.99;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoreHedgeMmConfig {
    pub enabled: bool,
    /// Maximum ask price for the cheap leg to be considered a valid hedge.
    pub cheap_leg_max_price: f64,
    /// Inclusive ask-price band for the expensive leg to be considered the
    /// favorite/core target.
    pub expensive_leg_min_price: f64,
    pub expensive_leg_max_price: f64,
    /// Minimum gap between the two ask prices required to engage.
    pub min_price_gap: f64,
    /// Per-bar capital target spread across both legs.
    pub bar_capital_usd: f64,
    /// Target cheap-to-expensive notional ratio. Whale sits at ~0.47.
    pub target_hedge_ratio: f64,
    /// Broad mergeable maker-ladder levels per side. When > 1, emit a
    /// symmetric two-leg ladder. The ladder is mergeable inventory, but it is
    /// not treated as a strict quote-pair package: fills can be recycled with
    /// any opposite-side mergeable inventory if realised pair cost is positive.
    pub ladder_levels: usize,
    /// Total price span covered by the paired ladder on each leg.
    pub ladder_span: f64,
    /// Fallback ladder center when a leg lacks a visible midpoint.
    pub center_price: f64,
    /// Per-level share clip for the canonical paired ladder.
    pub clip_shares: f64,
    /// Maximum projected one-sided paired-core imbalance, including
    /// pending/working paired-core orders. This prevents a full ladder on
    /// one leg from becoming accidental directional inventory before the
    /// other leg fills.
    pub max_unpaired_core_qty: f64,
    /// Price band for mergeable maker-ladder rungs. Rungs outside this band
    /// belong to explicit directional lanes, not mergeable paired-core.
    pub ladder_min_price: f64,
    pub ladder_max_price: f64,
    /// Safety mode for live paired-core: clamp any configured ladder to a tiny
    /// center probe instead of broad resting levels.
    pub center_probe_only: bool,
    /// Hard per-share pair-cost ceiling for one-sided mate repair. The
    /// implementation also caps this at 0.99 so config cannot loosen it.
    pub repair_pair_cost_limit: f64,
    /// Conservative per-share fee/slippage buffer added to filled_avg + mate ask.
    pub repair_fee_buffer: f64,
    /// Per-clip size by leg.
    pub core_clip_usd: f64,
    pub hedge_clip_usd: f64,
    /// Pricing offset from best_bid (in ticks) when posting a maker buy.
    /// 0 = join queue at best_bid; positive = improve.
    pub maker_improve_ticks: f64,
    /// Floor on per-order USD notional.
    pub min_order_usd: f64,
    /// Minimum paired inventory (min(yes_qty, no_qty)) that triggers a
    /// merge intent on the next tick. Whale data (493 paired windows)
    /// shows first merge ~30s after entry, so we want a low threshold.
    pub merge_min_qty: f64,
    /// Maximum paired quantity to merge in a single batch.
    pub merge_batch_cap: f64,
    /// Disable merge planning after this epoch-ms timestamp (redeem-only mode).
    pub disable_merge_after_ms: Option<EpochMillis>,
    /// Disable new broad paired-core quote placement after this elapsed bar age.
    /// Late-window exposure belongs to late-favorite plus cheap-tail/reversal
    /// lanes; existing paired inventory may still merge/redeem and mate-repair
    /// can clean up pre-existing imbalance.
    pub disable_after_elapsed_ms: Option<u64>,
    /// Once paired-core has created one-sided filled inventory, stop emitting
    /// fresh core quotes for that market. Mate repair and merge planning still
    /// run before this guard; this only prevents the probe ladder from
    /// compounding adverse-selection inventory.
    pub stop_fresh_quotes_on_unpaired_fill: bool,
    /// Depth-aware paired-core filter. When enabled, each paired ladder level
    /// must have symmetric visible bid-side queue/depth on both legs before it
    /// is emitted. This avoids posting a "pair" where one side is trivially
    /// fillable and the mate is buried behind materially more queue.
    pub book_sanity_enabled: bool,
    pub book_sanity_max_spread: f64,
    pub book_sanity_min_top_depth_usd: f64,
    pub book_sanity_max_queue_ahead_usd: f64,
    pub book_sanity_max_queue_imbalance_ratio: f64,
    pub book_sanity_max_projected_pair_cost: f64,
}

impl Default for CoreHedgeMmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cheap_leg_max_price: 0.38,
            expensive_leg_min_price: 0.52,
            expensive_leg_max_price: 0.92,
            min_price_gap: 0.12,
            bar_capital_usd: 50.0,
            target_hedge_ratio: 0.47,
            ladder_levels: 0,
            ladder_span: 0.42,
            center_price: 0.50,
            clip_shares: 20.0,
            max_unpaired_core_qty: 10.0,
            ladder_min_price: 0.0,
            ladder_max_price: 1.0,
            center_probe_only: false,
            repair_pair_cost_limit: PAIRED_CORE_REPAIR_PAIR_COST_LIMIT,
            repair_fee_buffer: 0.0,
            core_clip_usd: 13.0,
            hedge_clip_usd: 5.0,
            maker_improve_ticks: 0.0,
            min_order_usd: 1.0,
            merge_min_qty: 1.0,
            merge_batch_cap: f64::INFINITY,
            disable_merge_after_ms: None,
            disable_after_elapsed_ms: None,
            stop_fresh_quotes_on_unpaired_fill: false,
            book_sanity_enabled: false,
            book_sanity_max_spread: 0.08,
            book_sanity_min_top_depth_usd: 3.0,
            book_sanity_max_queue_ahead_usd: 200.0,
            book_sanity_max_queue_imbalance_ratio: 3.0,
            book_sanity_max_projected_pair_cost: 0.995,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoreHedgeMmStrategyConfig {
    pub core_hedge: CoreHedgeMmConfig,
}

impl Default for CoreHedgeMmStrategyConfig {
    fn default() -> Self {
        Self {
            core_hedge: CoreHedgeMmConfig::default(),
        }
    }
}

/// Tracks the last (price, qty) we emitted per (market, leg, tag) so we
/// only re-emit when those change. Without this, the strategy fires a
/// new intent every tick (~1 Hz), each with a fresh CoID. The replay
/// fill simulator treats every replace as a fresh `cumulative_trade_
/// through` counter — meaning we reset our queue-burn estimate every
/// second instead of accumulating it over minutes like a real maker.
/// That artifact lets the simulator fill us on tiny trade-through
/// events that wouldn't reach a long-resting maker.
type LastEmitKey = (MarketId, LadderLeg, String);

#[derive(Clone, Debug)]
pub struct CoreHedgeMmStrategy {
    config: CoreHedgeMmStrategyConfig,
    last_emit: HashMap<LastEmitKey, (f64, f64)>,
}

#[derive(Clone, Debug, PartialEq)]
struct PairedCoreChopGate {
    note: String,
    suppress_outer_bundle: bool,
    suppress_repair: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum PairedCoreBroadPosture {
    Full,
    CenterOnly { reason: String, center_band: usize },
    Suppressed { reason: String },
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct EffectivePairedCoreLadder {
    levels: usize,
    half_span: f64,
    min_price: f64,
    max_price: f64,
    center_probe_only: bool,
}

impl CoreHedgeMmStrategy {
    pub fn new(config: CoreHedgeMmStrategyConfig) -> Self {
        Self {
            config,
            last_emit: HashMap::new(),
        }
    }

    pub fn config(&self) -> &CoreHedgeMmStrategyConfig {
        &self.config
    }

    /// Returns true (and records) if this leg+tag should re-emit at the
    /// given price/qty. Returns false if the prior emission was identical
    /// within tolerance — caller should skip the intent.
    fn should_emit(
        &mut self,
        market_id: &MarketId,
        leg: LadderLeg,
        tag: &str,
        price: f64,
        qty: f64,
    ) -> bool {
        let key = (market_id.clone(), leg, tag.to_string());
        let changed = self.would_emit(market_id, leg, tag, price, qty);
        if changed {
            self.last_emit.insert(key, (price, qty));
        }
        changed
    }

    fn would_emit(
        &self,
        market_id: &MarketId,
        leg: LadderLeg,
        tag: &str,
        price: f64,
        qty: f64,
    ) -> bool {
        // Re-emit thresholds. Each cancel+repost destroys our FIFO queue
        // position on Polymarket, so allow micro-drift in the intent's
        // price and qty without churning the venue order. A half-tick
        // price tolerance (5e-3 vs 1e-2 venue tick) preserves quote
        // stability when fair-value drifts within the tick. A half-share
        // qty tolerance absorbs sizing-function rounding without
        // triggering re-emits.
        const REQUOTE_PRICE_TOLERANCE: f64 = 0.005;
        const REQUOTE_QTY_TOLERANCE: f64 = 0.5;
        let key = (market_id.clone(), leg, tag.to_string());
        match self.last_emit.get(&key) {
            Some(&(prev_px, prev_qty)) => {
                (price - prev_px).abs() > REQUOTE_PRICE_TOLERANCE
                    || (qty - prev_qty).abs() > REQUOTE_QTY_TOLERANCE
            }
            None => true,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct LegGeometry {
    expensive_leg: LadderLeg,
    cheap_leg: LadderLeg,
    expensive_ask: f64,
    cheap_ask: f64,
    expensive_bid: f64,
    cheap_bid: f64,
}

fn classify_legs(
    snapshot: &PairedMarketSnapshot,
    config: &CoreHedgeMmConfig,
) -> Option<LegGeometry> {
    let yes_ask = snapshot.yes_quote.best_ask.as_ref()?.price;
    let no_ask = snapshot.no_quote.best_ask.as_ref()?.price;
    let yes_bid = snapshot.yes_quote.best_bid.as_ref()?.price;
    let no_bid = snapshot.no_quote.best_bid.as_ref()?.price;

    let (expensive_leg, cheap_leg, expensive_ask, cheap_ask, expensive_bid, cheap_bid) =
        if yes_ask >= no_ask {
            (
                LadderLeg::Yes,
                LadderLeg::No,
                yes_ask,
                no_ask,
                yes_bid,
                no_bid,
            )
        } else {
            (
                LadderLeg::No,
                LadderLeg::Yes,
                no_ask,
                yes_ask,
                no_bid,
                yes_bid,
            )
        };

    if cheap_ask > config.cheap_leg_max_price {
        return None;
    }
    if expensive_ask < config.expensive_leg_min_price
        || expensive_ask > config.expensive_leg_max_price
    {
        return None;
    }
    if (expensive_ask - cheap_ask) < config.min_price_gap {
        return None;
    }

    Some(LegGeometry {
        expensive_leg,
        cheap_leg,
        expensive_ask,
        cheap_ask,
        expensive_bid,
        cheap_bid,
    })
}

fn build_clip<M: MarketDescriptor>(
    market: &M,
    leg: LadderLeg,
    best_bid: f64,
    best_ask: f64,
    clip_usd: f64,
    tag: &str,
    improve_ticks: f64,
    min_order_usd: f64,
    now_ms: EpochMillis,
) -> Option<OrderIntent> {
    let tick = market.tick_size().max(0.0001);
    let mut limit_price = best_bid + improve_ticks * tick;
    let max_passive = (best_ask - tick).max(tick);
    if limit_price > max_passive {
        limit_price = max_passive;
    }
    if limit_price <= 0.0 || limit_price >= 1.0 {
        return None;
    }
    if clip_usd < min_order_usd {
        return None;
    }
    let qty = (clip_usd / limit_price).max(market.min_order_size());
    let instrument_id = match leg {
        LadderLeg::Yes => market.yes_instrument_id().clone(),
        LadderLeg::No => market.no_instrument_id().clone(),
    };
    let coid = ClientOrderId::from(format!(
        "paired-core:{}:{:?}:{}:{}",
        market.market_id(),
        leg,
        tag,
        now_ms,
    ));
    let mut intent = OrderIntent::new_buy(
        coid,
        market.market_id().clone(),
        instrument_id,
        limit_price,
        qty,
        format!(
            "paired_core {} leg={:?} px={:.4} sz_usd={:.2} (best_bid={:.4} best_ask={:.4})",
            tag, leg, limit_price, clip_usd, best_bid, best_ask
        ),
        now_ms,
    );
    intent.kind = IntentKind::Entry;
    intent.quote_level_tag = Some(format!("paired-core:{}", tag));
    Some(intent)
}

fn quote_mid(best_bid: f64, best_ask: f64, fallback: f64) -> f64 {
    if best_bid > 0.0 && best_ask > best_bid && best_ask < 1.0 {
        (best_bid + best_ask) / 2.0
    } else {
        fallback
    }
}

fn effective_paired_core_ladder(config: &CoreHedgeMmConfig) -> Option<EffectivePairedCoreLadder> {
    if config.ladder_levels == 0 {
        return None;
    }
    let mut levels = config.ladder_levels.max(1);
    let mut span = config.ladder_span.max(0.0);
    let mut min_price = config.ladder_min_price;
    let mut max_price = config.ladder_max_price;

    if config.center_probe_only {
        levels = levels.min(PAIRED_CORE_CENTER_PROBE_MAX_LEVELS).max(1);
        span = span.min(PAIRED_CORE_CENTER_PROBE_SPAN);
        min_price = min_price.max(PAIRED_CORE_CENTER_PROBE_MIN_PRICE);
        max_price = max_price.min(PAIRED_CORE_CENTER_PROBE_MAX_PRICE);
    }

    if min_price > max_price {
        return None;
    }

    Some(EffectivePairedCoreLadder {
        levels,
        half_span: span / 2.0,
        min_price,
        max_price,
        center_probe_only: config.center_probe_only,
    })
}

fn effective_repair_pair_cost_limit(config: &CoreHedgeMmConfig) -> f64 {
    config
        .repair_pair_cost_limit
        .min(PAIRED_CORE_REPAIR_PAIR_COST_LIMIT)
}

fn canonical_clip_shares(price: f64, base_clip: f64) -> f64 {
    let d = (price - 0.5).abs();
    let scale = if d <= 0.05 {
        1.0
    } else if d <= 0.10 {
        1.10
    } else if d <= 0.20 {
        1.25
    } else if d <= 0.30 {
        1.50
    } else if d <= 0.40 {
        1.85
    } else {
        3.0
    };
    (base_clip * scale).max(0.0)
}

fn paired_core_leg_allowed(leg: LadderLeg, yes_qty: f64, no_qty: f64, tolerance: f64) -> bool {
    match leg {
        LadderLeg::Yes => yes_qty <= no_qty + tolerance,
        LadderLeg::No => no_qty <= yes_qty + tolerance,
    }
}

fn paired_core_projected_qty<M: MarketDescriptor>(input: &StrategyInput<M>, leg: LadderLeg) -> f64 {
    match leg {
        LadderLeg::Yes => {
            input.paired_core_inventory.yes_qty + input.open_paired_core_order_exposure.yes_qty
        }
        LadderLeg::No => {
            input.paired_core_inventory.no_qty + input.open_paired_core_order_exposure.no_qty
        }
    }
    .max(0.0)
}

fn paired_core_repair_pair_cost<M: MarketDescriptor>(
    input: &StrategyInput<M>,
    leg: LadderLeg,
    opposite_ask: f64,
    fee_buffer: f64,
) -> Option<f64> {
    let other_avg = match leg {
        LadderLeg::Yes => input.paired_core_inventory.no_avg_cost,
        LadderLeg::No => input.paired_core_inventory.yes_avg_cost,
    };
    if other_avg.is_finite() && other_avg > 0.0 && opposite_ask.is_finite() && opposite_ask > 0.0
    {
        Some(other_avg + opposite_ask + fee_buffer.max(0.0))
    } else {
        None
    }
}

fn paired_core_chop_gate<M: MarketDescriptor>(
    input: &StrategyInput<M>,
    max_opening_ms: u64,
) -> Option<PairedCoreChopGate> {
    let elapsed_ms = input
        .market
        .event_start_ms()
        .map(|start_ms| input.now_ms.saturating_sub(start_ms))
        .unwrap_or(input.market.window_ms());
    let vol = input.btc_regime.realized_vol_5m_bps.unwrap_or(0.0);
    let r30 = input.btc_regime.return_30s_bps.unwrap_or(0.0);
    let r60 = input.btc_regime.return_60s_bps.unwrap_or(0.0);
    let r120 = input.btc_regime.return_120s_bps.unwrap_or(0.0);
    let r180 = input.btc_regime.return_180s_bps.unwrap_or(0.0);
    let sign_flip = (r30 > 1.0 && r120 < -1.0)
        || (r30 < -1.0 && r120 > 1.0)
        || (r60 > 1.0 && r180 < -1.0)
        || (r60 < -1.0 && r180 > 1.0);
    if matches!(input.btc_regime.regime(), Some(BtcRegime::Whipsaw)) {
        return Some(PairedCoreChopGate {
            note: format!(
                "paired_core center-only: whipsaw regime vol_5m_bps={vol:.2} r30={r30:.2} r60={r60:.2} r120={r120:.2} r180={r180:.2}"
            ),
            suppress_outer_bundle: true,
            suppress_repair: false,
        });
    }
    if elapsed_ms <= max_opening_ms && (vol >= BtcRegime::VOL_LOW_HIGH_BPS || sign_flip) {
        return Some(PairedCoreChopGate {
            note: format!(
                "paired_core center-only: opening chop elapsed_ms={elapsed_ms} vol_5m_bps={vol:.2} sign_flip={sign_flip} r30={r30:.2} r60={r60:.2} r120={r120:.2} r180={r180:.2}"
            ),
            suppress_outer_bundle: true,
            suppress_repair: true,
        });
    }
    None
}

fn paired_core_broad_posture<M: MarketDescriptor>(
    input: &StrategyInput<M>,
    elapsed_ms: u64,
    hard_cutoff_ms: Option<u64>,
    yes_ask: f64,
    no_ask: f64,
    levels: usize,
) -> PairedCoreBroadPosture {
    paired_core_broad_posture_for_regime(
        input.btc_regime.regime(),
        elapsed_ms,
        hard_cutoff_ms,
        yes_ask,
        no_ask,
        levels,
    )
}

fn paired_core_broad_posture_for_regime(
    regime: Option<BtcRegime>,
    elapsed_ms: u64,
    hard_cutoff_ms: Option<u64>,
    yes_ask: f64,
    no_ask: f64,
    levels: usize,
) -> PairedCoreBroadPosture {
    let favorite_ask = yes_ask.max(no_ask);
    let cheap_ask = yes_ask.min(no_ask);
    if paired_core_directional_barbell_book(yes_ask, no_ask) {
        return PairedCoreBroadPosture::Suppressed {
            reason: format!(
                "paired_core broad stopped: directional barbell book favorite_ask={favorite_ask:.4} cheap_ask={cheap_ask:.4} elapsed_ms={elapsed_ms}; route fresh risk exclusively to late-fav/cheap-tail"
            ),
        };
    }
    if let Some(cutoff_ms) = hard_cutoff_ms {
        if elapsed_ms >= cutoff_ms {
            return PairedCoreBroadPosture::Suppressed {
                reason: format!(
                    "paired_core broad suppressed: late-window cutoff elapsed_ms={elapsed_ms} cutoff_ms={cutoff_ms}; late-fav/cheap-tail lanes own fresh risk"
                ),
            };
        }
    }
    let center_band = paired_core_center_accumulator_band(levels, yes_ask, no_ask);
    match regime {
        Some(BtcRegime::Whipsaw) if elapsed_ms >= 90_000 && center_band > 0 => {
            PairedCoreBroadPosture::CenterOnly {
                reason: format!(
                    "paired_core center-only: whipsaw mid accumulator elapsed_ms={elapsed_ms}; keep controlled mid rungs while avoiding runaway outer ladder"
                ),
                center_band,
            }
        }
        Some(BtcRegime::Whipsaw) if elapsed_ms >= 90_000 => {
            PairedCoreBroadPosture::Suppressed {
                reason: format!(
                    "paired_core broad suppressed: whipsaw non-mid book elapsed_ms={elapsed_ms}; avoid adding inventory whose mate side may not return"
                ),
            }
        }
        Some(BtcRegime::DirectionalSmooth | BtcRegime::TrendingVolatile)
            if elapsed_ms >= 120_000 && center_band > 0 =>
        {
            PairedCoreBroadPosture::CenterOnly {
                reason: format!(
                    "paired_core center-only: directional mid accumulator regime={:?} elapsed_ms={elapsed_ms}; keep center rungs before favorite load resolves",
                    regime
                ),
                center_band,
            }
        }
        Some(BtcRegime::DirectionalSmooth | BtcRegime::TrendingVolatile)
            if elapsed_ms >= 120_000 =>
        {
            PairedCoreBroadPosture::Suppressed {
                reason: format!(
                    "paired_core broad suppressed: directional late non-mid path regime={:?} elapsed_ms={elapsed_ms}; route fresh risk to late-fav/cheap-tail",
                    regime
                ),
            }
        }
        _ => PairedCoreBroadPosture::Full,
    }
}

fn paired_core_directional_barbell_book(yes_ask: f64, no_ask: f64) -> bool {
    let favorite_ask = yes_ask.max(no_ask);
    let cheap_ask = yes_ask.min(no_ask);
    (favorite_ask >= 0.80 && cheap_ask <= 0.20) || (favorite_ask >= 0.90 && cheap_ask <= 0.10)
}

fn paired_core_center_accumulator_band(levels: usize, yes_ask: f64, no_ask: f64) -> usize {
    let favorite_ask = yes_ask.max(no_ask);
    let cheap_ask = yes_ask.min(no_ask);
    if favorite_ask <= 0.65 && cheap_ask >= 0.35 {
        3.min(levels / 2)
    } else if favorite_ask <= 0.78 && cheap_ask >= 0.22 {
        2.min(levels / 2)
    } else {
        0
    }
}

fn market_elapsed_ms<M: MarketDescriptor>(input: &StrategyInput<M>) -> u64 {
    input
        .market
        .event_start_ms()
        .map(|start_ms| input.now_ms.saturating_sub(start_ms))
        .unwrap_or(input.market.window_ms())
}

fn ladder_candidate<M: MarketDescriptor>(
    market: &M,
    leg: LadderLeg,
    best_bid: f64,
    best_ask: f64,
    levels: usize,
    half_span: f64,
    center_price: f64,
    idx: usize,
    min_price: f64,
    max_price: f64,
    clip_shares: f64,
    min_order_usd: f64,
    maker_improve_ticks: f64,
    now_ms: EpochMillis,
) -> Option<OrderIntent> {
    let tick = market.tick_size().max(0.0001);
    let mid = quote_mid(best_bid, best_ask, center_price).clamp(0.01, 0.99);
    let low = (mid - half_span).clamp(0.01, 0.99);
    let high = (mid + half_span).clamp(0.01, 0.99);
    let grid_price = if levels <= 1 {
        mid
    } else {
        let denom = (levels - 1) as f64;
        low + (high - low) * (idx as f64 / denom)
    };
    let raw_price =
        improved_ladder_price(grid_price, best_bid, best_ask, tick, maker_improve_ticks);
    if raw_price < min_price || raw_price > max_price {
        return None;
    }
    let qty = canonical_clip_shares(raw_price, clip_shares);
    let mut intent = build_ladder_level(
        market,
        leg,
        best_ask,
        raw_price,
        qty,
        &format!("ladder:{idx}"),
        min_order_usd,
        now_ms,
    )?;
    intent.pair_id = Some(format!(
        "paired-core:{}:ladder:{idx}:{now_ms}",
        market.market_id()
    ));
    Some(intent)
}

fn top_depth_notional_usd(quote: &QuoteSnapshot, bid_side: bool) -> f64 {
    let level = if bid_side {
        quote.best_bid.as_ref()
    } else {
        quote.best_ask.as_ref()
    };
    level
        .map(|level| level.price.max(0.0) * level.quantity.max(0.0))
        .unwrap_or(0.0)
}

fn queue_ahead_bid_notional_usd(quote: &QuoteSnapshot, limit_price: f64) -> f64 {
    quote
        .bid_levels
        .iter()
        .filter(|level| level.price + 1e-9 >= limit_price)
        .map(|level| level.price.max(0.0) * level.quantity.max(0.0))
        .sum()
}

fn paired_core_book_sanity(
    cfg: &CoreHedgeMmConfig,
    yes_quote: &QuoteSnapshot,
    no_quote: &QuoteSnapshot,
    yes_px: f64,
    no_px: f64,
) -> Result<String, String> {
    if !cfg.book_sanity_enabled {
        return Ok("disabled".to_string());
    }

    let yes_bid = yes_quote.best_bid.as_ref().map(|level| level.price).unwrap_or(0.0);
    let yes_ask = yes_quote.best_ask.as_ref().map(|level| level.price).unwrap_or(0.0);
    let no_bid = no_quote.best_bid.as_ref().map(|level| level.price).unwrap_or(0.0);
    let no_ask = no_quote.best_ask.as_ref().map(|level| level.price).unwrap_or(0.0);
    let yes_spread = (yes_ask - yes_bid).max(0.0);
    let no_spread = (no_ask - no_bid).max(0.0);
    let max_spread = cfg.book_sanity_max_spread.max(0.0);
    if yes_spread > max_spread || no_spread > max_spread {
        return Err(format!(
            "spread yes={yes_spread:.4} no={no_spread:.4} max={max_spread:.4}"
        ));
    }

    let yes_top_depth = top_depth_notional_usd(yes_quote, true);
    let no_top_depth = top_depth_notional_usd(no_quote, true);
    let min_top_depth = cfg.book_sanity_min_top_depth_usd.max(0.0);
    if yes_top_depth < min_top_depth || no_top_depth < min_top_depth {
        return Err(format!(
            "top_depth yes={yes_top_depth:.2} no={no_top_depth:.2} min={min_top_depth:.2}"
        ));
    }

    let pair_cost = yes_px + no_px;
    let max_pair_cost = cfg.book_sanity_max_projected_pair_cost.max(0.0);
    if max_pair_cost > 0.0 && pair_cost > max_pair_cost + 1e-9 {
        return Err(format!(
            "pair_cost={pair_cost:.4} max_projected={max_pair_cost:.4}"
        ));
    }

    let yes_queue = queue_ahead_bid_notional_usd(yes_quote, yes_px);
    let no_queue = queue_ahead_bid_notional_usd(no_quote, no_px);
    let max_queue = cfg.book_sanity_max_queue_ahead_usd.max(0.0);
    if max_queue > 0.0 && (yes_queue > max_queue || no_queue > max_queue) {
        return Err(format!(
            "queue_ahead yes={yes_queue:.2} no={no_queue:.2} max={max_queue:.2}"
        ));
    }
    let small = yes_queue.min(no_queue).max(1.0);
    let large = yes_queue.max(no_queue);
    let queue_ratio = large / small;
    let max_ratio = cfg.book_sanity_max_queue_imbalance_ratio.max(1.0);
    if queue_ratio > max_ratio {
        return Err(format!(
            "queue_imbalance yes={yes_queue:.2} no={no_queue:.2} ratio={queue_ratio:.2} max={max_ratio:.2}"
        ));
    }

    Ok(format!(
        "spread yes={yes_spread:.4} no={no_spread:.4} top_depth yes={yes_top_depth:.2} no={no_top_depth:.2} queue yes={yes_queue:.2} no={no_queue:.2} ratio={queue_ratio:.2} pair_cost={pair_cost:.4}"
    ))
}

fn improved_ladder_price(
    grid_price: f64,
    best_bid: f64,
    best_ask: f64,
    tick: f64,
    maker_improve_ticks: f64,
) -> f64 {
    let improve_ticks = maker_improve_ticks.max(0.0);
    if improve_ticks <= 0.0 || tick <= 0.0 {
        return grid_price;
    }
    let max_passive = best_ask - tick;
    if max_passive <= 0.0 {
        return grid_price;
    }
    let improved = (best_bid + tick * improve_ticks).min(max_passive);
    let near_touch_window = tick * (improve_ticks + 1.0);
    if grid_price >= best_bid - near_touch_window && improved > grid_price {
        improved
    } else {
        grid_price
    }
}

fn build_ladder_level<M: MarketDescriptor>(
    market: &M,
    leg: LadderLeg,
    best_ask: f64,
    price: f64,
    quantity: f64,
    tag: &str,
    min_order_usd: f64,
    now_ms: EpochMillis,
) -> Option<OrderIntent> {
    let tick = market.tick_size().max(0.0001);
    let max_passive = (best_ask - tick).max(tick);
    if price > max_passive + 1e-9 {
        return None;
    }
    let limit_price = price;
    if limit_price <= 0.0 || limit_price >= 1.0 {
        return None;
    }
    let mut qty = quantity.max(market.min_order_size());
    if qty * limit_price < min_order_usd {
        qty = (min_order_usd / limit_price).max(market.min_order_size());
    }
    let instrument_id = match leg {
        LadderLeg::Yes => market.yes_instrument_id().clone(),
        LadderLeg::No => market.no_instrument_id().clone(),
    };
    let coid = ClientOrderId::from(format!(
        "paired-core:{}:{:?}:{}:{}",
        market.market_id(),
        leg,
        tag,
        now_ms,
    ));
    let mut intent = OrderIntent::new_buy(
        coid,
        market.market_id().clone(),
        instrument_id,
        limit_price,
        qty,
        format!(
            "paired_core ladder leg={:?} tag={} px={:.4} qty={:.4}",
            leg, tag, limit_price, qty,
        ),
        now_ms,
    );
    intent.kind = IntentKind::Entry;
    intent.quote_level_tag = Some(format!("paired-core:{}", tag));
    Some(intent)
}

fn build_mate_repair_order<M: MarketDescriptor>(
    market: &M,
    leg: LadderLeg,
    best_bid: f64,
    best_ask: f64,
    quantity: f64,
    min_order_usd: f64,
    maker_improve_ticks: f64,
    now_ms: EpochMillis,
) -> Option<OrderIntent> {
    let tick = market.tick_size().max(0.0001);
    let max_passive = (best_ask - tick).max(tick);
    let limit_price = (best_bid.max(tick) + maker_improve_ticks.max(0.0) * tick)
        .min(max_passive)
        .clamp(tick, 0.99);
    if limit_price <= 0.0 || limit_price >= best_ask {
        return None;
    }

    let min_qty = market.min_order_size().max(0.0);
    let min_notional_qty = if min_order_usd > 0.0 {
        min_order_usd / limit_price
    } else {
        0.0
    };
    let qty = quantity.max(min_qty).max(min_notional_qty);
    if qty > quantity + 1e-9 {
        return None;
    }

    build_ladder_level(
        market,
        leg,
        best_ask,
        limit_price,
        qty,
        "repair:mate",
        min_order_usd,
        now_ms,
    )
}

impl<M> TradingStrategy<M> for CoreHedgeMmStrategy
where
    M: MarketDescriptor,
{
    fn name(&self) -> &'static str {
        "core_hedge_mm"
    }

    fn on_tick(&mut self, input: StrategyInput<M>) -> StrategyDecision {
        let cfg = self.config.core_hedge;
        if !cfg.enabled {
            return StrategyDecision::Noop {
                notes: vec!["paired_core disabled".to_string()],
            };
        }

        // Merge planner: if paired inventory exists, recycle it BEFORE
        // posting new entry intents.
        let merge_window_open = cfg
            .disable_merge_after_ms
            .is_none_or(|cutoff_ms| input.now_ms < cutoff_ms);

        if merge_window_open {
            let mut paired_qty = input
                .paired_core_inventory
                .yes_qty
                .min(input.paired_core_inventory.no_qty);
            if cfg.merge_batch_cap.is_finite() && cfg.merge_batch_cap > 0.0 {
                paired_qty = paired_qty.min(cfg.merge_batch_cap);
            }
            if paired_qty >= cfg.merge_min_qty {
                let yes_avg = input.paired_core_inventory.yes_avg_cost.max(0.0);
                let no_avg = input.paired_core_inventory.no_avg_cost.max(0.0);
                let pair_cost = yes_avg + no_avg;
                let expected_cost_usd = paired_qty * (yes_avg + no_avg);
                let expected_cash_usd = paired_qty;
                if pair_cost > PAIRED_CORE_MERGE_PAIR_COST_LIMIT + 1e-9 {
                    return StrategyDecision::Noop {
                        notes: vec![format!(
                            "paired_core merge skipped pair_cost_above_limit paired_qty={:.4} pair_cost={:.4} limit={:.4} cost={:.2} cash={:.2} expected_net={:.4}",
                            paired_qty,
                            pair_cost,
                            PAIRED_CORE_MERGE_PAIR_COST_LIMIT,
                            expected_cost_usd,
                            expected_cash_usd,
                            expected_cash_usd - expected_cost_usd,
                        )],
                    };
                }
                let merge_intent = MergeIntent {
                    command_id: ClientOrderId::new(format!(
                        "paired-core-merge:{}:{}",
                        input.market.market_id(),
                        input.now_ms,
                    )),
                    market_id: input.market.market_id().clone(),
                    condition_id: None,
                    yes_instrument_id: input.market.yes_instrument_id().clone(),
                    no_instrument_id: input.market.no_instrument_id().clone(),
                    quantity: paired_qty,
                    expected_cash_usd,
                    expected_cost_usd,
                    expected_fee_usd: 0.0,
                    expected_gas_usd: 0.0,
                    reason: format!(
                        "paired_core merge paired_qty={:.4} expected_net={:.4}",
                        paired_qty,
                        expected_cash_usd - expected_cost_usd
                    ),
                    created_at_ms: input.now_ms,
                };
                return StrategyDecision::Merge {
                    intent: merge_intent,
                    notes: vec![format!(
                        "paired_core merging paired_qty={:.4} cost={:.2} cash={:.2} batch_cap={:.4}",
                        paired_qty, expected_cost_usd, expected_cash_usd, cfg.merge_batch_cap
                    )],
                };
            }
        }

        if let Some(ladder) = effective_paired_core_ladder(&cfg) {
            let yes_bid = input.snapshot.yes_quote.best_bid.as_ref().map(|l| l.price);
            let yes_ask = input.snapshot.yes_quote.best_ask.as_ref().map(|l| l.price);
            let no_bid = input.snapshot.no_quote.best_bid.as_ref().map(|l| l.price);
            let no_ask = input.snapshot.no_quote.best_ask.as_ref().map(|l| l.price);
            let (Some(yes_bid), Some(yes_ask), Some(no_bid), Some(no_ask)) =
                (yes_bid, yes_ask, no_bid, no_ask)
            else {
                return StrategyDecision::Noop {
                    notes: vec![format!(
                        "paired_core ladder missing quotes yes_bid={yes_bid:?} yes_ask={yes_ask:?} no_bid={no_bid:?} no_ask={no_ask:?}",
                    )],
                };
            };

            let mut intents = Vec::new();
            let mut notes = Vec::new();
            let chop_note = paired_core_chop_gate(&input, 90_000);
            if let Some(note) = &chop_note {
                notes.push(note.note.clone());
            }
            let elapsed_ms = market_elapsed_ms(&input);
            let levels = ladder.levels;
            let half_span = ladder.half_span;
            let market_id = input.market.market_id().clone();
            let yes_mid = quote_mid(yes_bid, yes_ask, cfg.center_price).clamp(0.01, 0.99);
            let no_mid = quote_mid(no_bid, no_ask, cfg.center_price).clamp(0.01, 0.99);
            if ladder.center_probe_only {
                notes.push(format!(
                    "paired_core center probe active: configured_levels={} effective_levels={levels} band={:.4}-{:.4} span={:.4}",
                    cfg.ladder_levels,
                    ladder.min_price,
                    ladder.max_price,
                    half_span * 2.0,
                ));
            }
            if yes_mid < ladder.min_price
                || yes_mid > ladder.max_price
                || no_mid < ladder.min_price
                || no_mid > ladder.max_price
                || yes_ask > ladder.max_price
                || no_ask > ladder.max_price
            {
                notes.push(format!(
                    "paired_core broad ladder: book center/ask outside mergeable band but eligible rungs may remain yes_mid={yes_mid:.4} no_mid={no_mid:.4} yes_ask={yes_ask:.4} no_ask={no_ask:.4} band={:.4}-{:.4}",
                    ladder.min_price, ladder.max_price,
                ));
            }
            let imbalance_tolerance =
                (cfg.clip_shares.max(input.market.min_order_size()) * 0.25).max(0.5);
            let mut projected_yes_qty = paired_core_projected_qty(&input, LadderLeg::Yes);
            let mut projected_no_qty = paired_core_projected_qty(&input, LadderLeg::No);
            let filled_yes_qty = input.paired_core_inventory.yes_qty.max(0.0);
            let filled_no_qty = input.paired_core_inventory.no_qty.max(0.0);
            let open_yes_qty = input.open_paired_core_order_exposure.yes_qty.max(0.0);
            let open_no_qty = input.open_paired_core_order_exposure.no_qty.max(0.0);
            let max_unpaired_core_qty = cfg.max_unpaired_core_qty.max(0.0);
            notes.push(format!(
                "paired_core inventory gate core_yes={:.4} core_no={:.4} total_yes={:.4} total_no={:.4} open_yes={:.4} open_no={:.4} projected_yes={:.4} projected_no={:.4} tolerance={:.4} max_unpaired={:.4}",
                input.paired_core_inventory.yes_qty,
                input.paired_core_inventory.no_qty,
                input.inventory.yes_qty,
                input.inventory.no_qty,
                input.open_paired_core_order_exposure.yes_qty,
                input.open_paired_core_order_exposure.no_qty,
                projected_yes_qty,
                projected_no_qty,
                imbalance_tolerance,
                max_unpaired_core_qty,
            ));

            if paired_core_directional_barbell_book(yes_ask, no_ask) {
                if let PairedCoreBroadPosture::Suppressed { reason } = paired_core_broad_posture(
                    &input,
                    elapsed_ms,
                    cfg.disable_after_elapsed_ms,
                    yes_ask,
                    no_ask,
                    levels,
                ) {
                    notes.push(reason);
                }
                notes.push(
                    "paired_core repair suppressed in directional barbell; avoid quote/cancel churn and route fresh risk to late-fav/cheap-tail".to_string(),
                );
                return StrategyDecision::Suppress {
                    scope: SuppressionScope::PairedOnly,
                    reason: CoolingReason::BtcTrending,
                    preserve_quotes: false,
                    notes,
                };
            }

            let min_repair_qty = input.market.min_order_size().max(0.0);
            let filled_abs_imbalance = (filled_yes_qty - filled_no_qty).abs();
            let repair_leg = if filled_yes_qty + min_repair_qty <= filled_no_qty {
                Some(LadderLeg::Yes)
            } else if filled_no_qty + min_repair_qty <= filled_yes_qty {
                Some(LadderLeg::No)
            } else {
                None
            };

            let in_repair_mode = repair_leg.is_some();
            if let Some(leg) = repair_leg {
                if let Some(note) = chop_note.as_ref().filter(|note| note.suppress_repair) {
                    return StrategyDecision::Noop {
                        notes: vec![format!(
                            "{}; paired_core repair suppressed until market is stable",
                            note.note
                        )],
                    };
                }
                let (best_bid, best_ask) = match leg {
                    LadderLeg::Yes => (yes_bid, yes_ask),
                    LadderLeg::No => (no_bid, no_ask),
                };
                let repair_limit = effective_repair_pair_cost_limit(&cfg);
                let fee_buffer = cfg.repair_fee_buffer.max(0.0);
                let Some(pair_cost) =
                    paired_core_repair_pair_cost(&input, leg, best_ask, fee_buffer)
                else {
                    notes.push(format!(
                        "paired_core mate repair skipped leg={leg:?}: missing filled average cost; leave inventory for bundle salvage",
                    ));
                    return StrategyDecision::Suppress {
                        scope: SuppressionScope::PairedOnly,
                        reason: CoolingReason::BtcTrending,
                        preserve_quotes: false,
                        notes,
                    };
                };
                if pair_cost >= repair_limit {
                    notes.push(format!(
                        "paired_core mate repair skipped leg={leg:?}: pair_cost={pair_cost:.4} limit={repair_limit:.4} fee_buffer={fee_buffer:.4}; leave inventory for bundle salvage",
                    ));
                    return StrategyDecision::Suppress {
                        scope: SuppressionScope::PairedOnly,
                        reason: CoolingReason::BtcTrending,
                        preserve_quotes: false,
                        notes,
                    };
                }

                let (target_filled_qty, projected_repair_qty) = match leg {
                    LadderLeg::Yes => (filled_no_qty, projected_yes_qty),
                    LadderLeg::No => (filled_yes_qty, projected_no_qty),
                };
                let repair_qty = (target_filled_qty - projected_repair_qty)
                    .min(cfg.clip_shares.max(min_repair_qty))
                    .max(0.0);
                notes.push(format!(
                    "paired_core repair mode mate-only leg={leg:?} pair_cost={pair_cost:.4} limit={repair_limit:.4} projected_yes={projected_yes_qty:.4} projected_no={projected_no_qty:.4} repair_qty={repair_qty:.4}",
                ));
                if repair_qty + 1e-9 < min_repair_qty {
                    notes.push(format!(
                        "paired_core mate repair skipped leg={leg:?}: repair_qty={repair_qty:.4} below min_repair_qty={min_repair_qty:.4}; leave inventory for bundle salvage",
                    ));
                    return StrategyDecision::Suppress {
                        scope: SuppressionScope::PairedOnly,
                        reason: CoolingReason::BtcTrending,
                        preserve_quotes: false,
                        notes,
                    };
                }

                let Some(intent) = build_mate_repair_order(
                    &input.market,
                    leg,
                    best_bid,
                    best_ask,
                    repair_qty,
                    cfg.min_order_usd,
                    cfg.maker_improve_ticks,
                    input.now_ms,
                ) else {
                    notes.push(format!(
                        "paired_core mate repair skipped leg={leg:?}: no passive mate quote fit repair_qty={repair_qty:.4}; leave inventory for bundle salvage",
                    ));
                    return StrategyDecision::Suppress {
                        scope: SuppressionScope::PairedOnly,
                        reason: CoolingReason::BtcTrending,
                        preserve_quotes: false,
                        notes,
                    };
                };

                if self.should_emit(
                    &market_id,
                    leg,
                    "repair:mate",
                    intent.limit_price,
                    intent.quantity,
                ) {
                    notes.push(format!(
                        "paired_core mate repair emitted leg={leg:?} qty={:.4} px={:.4}; no broad continuation after fill",
                        intent.quantity, intent.limit_price,
                    ));
                    return StrategyDecision::QuoteSet {
                        intents: vec![intent],
                        notes,
                    };
                }

                notes.push(format!(
                    "paired_core mate repair unchanged leg={leg:?}; no broad continuation after fill",
                ));
                return StrategyDecision::Suppress {
                    scope: SuppressionScope::PairedOnly,
                    reason: CoolingReason::BtcTrending,
                    preserve_quotes: true,
                    notes,
                };
            }

            if cfg.stop_fresh_quotes_on_unpaired_fill
                && filled_abs_imbalance > 1e-9
                && filled_abs_imbalance < min_repair_qty
            {
                notes.push(format!(
                    "paired_core fresh quotes stopped after unpaired fill: filled_abs_imbalance={filled_abs_imbalance:.4} min_repair_qty={min_repair_qty:.4}; mate repair unavailable below venue minimum",
                ));
                return StrategyDecision::Suppress {
                    scope: SuppressionScope::PairedOnly,
                    reason: CoolingReason::BtcTrending,
                    preserve_quotes: false,
                    notes,
                };
            }

            let broad_posture = paired_core_broad_posture(
                &input,
                elapsed_ms,
                cfg.disable_after_elapsed_ms,
                yes_ask,
                no_ask,
                levels,
            );
            let center_only_band = match broad_posture {
                PairedCoreBroadPosture::Full => None,
                PairedCoreBroadPosture::CenterOnly {
                    reason,
                    center_band,
                } => {
                    notes.push(reason);
                    Some(center_band)
                }
                PairedCoreBroadPosture::Suppressed { reason } => {
                    notes.push(reason);
                    notes.push(format!(
                        "paired_core broad suppressed with mate-repair preserved repair_mode={in_repair_mode} repair_intents={}",
                        intents.len()
                    ));
                    return if intents.is_empty() {
                        StrategyDecision::Suppress {
                            scope: SuppressionScope::PairedOnly,
                            reason: CoolingReason::BtcTrending,
                            preserve_quotes: false,
                            notes,
                        }
                    } else {
                        StrategyDecision::QuoteSet { intents, notes }
                    };
                }
            };

            let late_fav_yes_qty = input.late_fav_inventory.yes_qty.max(0.0);
            let late_fav_no_qty = input.late_fav_inventory.no_qty.max(0.0);
            if late_fav_yes_qty.max(late_fav_no_qty) >= min_repair_qty {
                notes.push(format!(
                    "paired_core broad ladder remains active with late_fav inventory yes={late_fav_yes_qty:.4} no={late_fav_no_qty:.4}; late_fav/hedge lanes stay non-mergeable",
                ));
            }
            if open_yes_qty > 1e-9 || open_no_qty > 1e-9 {
                notes.push(format!(
                    "paired_core balanced bundle sees live paired-core orders open_yes={open_yes_qty:.4} open_no={open_no_qty:.4}; projected exposure guard will prevent worsening imbalance",
                ));
                if !in_repair_mode {
                    notes.push(format!(
                        "paired_core balanced bundle suppressed: awaiting open paired-core orders open_yes={open_yes_qty:.4} open_no={open_no_qty:.4}",
                    ));
                    notes.push(format!(
                        "paired_core ladder levels={levels} span={:.4} yes_mid={yes_mid:.4} no_mid={no_mid:.4} projected_yes_final={projected_yes_qty:.4} projected_no_final={projected_no_qty:.4}",
                        cfg.ladder_span,
                    ));
                    return StrategyDecision::Noop { notes };
                }
            }
            if in_repair_mode {
                notes.push(format!(
                    "paired_core balanced continuation mode projected_yes={projected_yes_qty:.4} projected_no={projected_no_qty:.4}",
                ));
            } else {
                notes.push(format!(
                    "paired_core balanced bundle mode projected_yes={projected_yes_qty:.4} projected_no={projected_no_qty:.4}",
                ));
            }
            for idx in 0..levels {
                if in_repair_mode {
                    if filled_abs_imbalance >= max_unpaired_core_qty + 1e-9 {
                        notes.push(format!(
                            "paired_core repair-continuation suppressed: filled_abs_imbalance={filled_abs_imbalance:.4} max_unpaired={max_unpaired_core_qty:.4}",
                        ));
                        break;
                    }
                }
                let mut active_center_band = center_only_band;
                if let Some(note) = &chop_note {
                    if note.suppress_outer_bundle && elapsed_ms <= 90_000 {
                        let opening_band =
                            paired_core_center_accumulator_band(levels, yes_ask, no_ask);
                        active_center_band = Some(
                            active_center_band
                                .map(|band| band.min(opening_band))
                                .unwrap_or(opening_band),
                        );
                    }
                }
                if let Some(center_band) = active_center_band {
                    let center_idx = levels / 2;
                    let distance_from_center = idx.abs_diff(center_idx);
                    if distance_from_center > center_band {
                        notes.push(format!(
                        "paired_core center accumulator: suppressing outer bundle idx={idx} center_idx={center_idx} center_band={center_band} elapsed_ms={elapsed_ms} yes_ask={yes_ask:.4} no_ask={no_ask:.4}",
                    ));
                        continue;
                    }
                }
                let Some(yes_intent) = ladder_candidate(
                        &input.market,
                        LadderLeg::Yes,
                        yes_bid,
                        yes_ask,
                        levels,
                        half_span,
                        cfg.center_price,
                        idx,
                        ladder.min_price,
                        ladder.max_price,
                        cfg.clip_shares,
                        cfg.min_order_usd,
                        cfg.maker_improve_ticks,
                    input.now_ms,
                ) else {
                    continue;
                };
                let Some(no_intent) = ladder_candidate(
                    &input.market,
                    LadderLeg::No,
                    no_bid,
                    no_ask,
                        levels,
                        half_span,
                        cfg.center_price,
                        idx,
                        ladder.min_price,
                        ladder.max_price,
                        cfg.clip_shares,
                        cfg.min_order_usd,
                        cfg.maker_improve_ticks,
                    input.now_ms,
                ) else {
                    continue;
                };
                if yes_intent.limit_price + no_intent.limit_price > 1.0 + 1e-9 {
                    notes.push(format!(
                        "paired_core bundle level blocked idx={idx} pair_cost={:.4}",
                        yes_intent.limit_price + no_intent.limit_price,
                    ));
                    continue;
                }
                match paired_core_book_sanity(
                    &cfg,
                    &input.snapshot.yes_quote,
                    &input.snapshot.no_quote,
                    yes_intent.limit_price,
                    no_intent.limit_price,
                ) {
                    Ok(summary) => {
                        notes.push(format!(
                            "paired_core depth sanity passed idx={idx} {summary}"
                        ));
                    }
                    Err(reason) => {
                        notes.push(format!(
                            "paired_core depth sanity blocked idx={idx}: {reason}",
                        ));
                        continue;
                    }
                }
                let current_abs_imbalance = (projected_yes_qty - projected_no_qty).abs();
                let next_yes = projected_yes_qty + yes_intent.quantity;
                let next_no = projected_no_qty + no_intent.quantity;
                let next_abs_imbalance = (next_yes - next_no).abs();
                if next_abs_imbalance > max_unpaired_core_qty + 1e-9 {
                    notes.push(format!(
                        "paired_core bundle level blocked idx={idx} next_yes={next_yes:.4} next_no={next_no:.4} max_unpaired={max_unpaired_core_qty:.4}",
                    ));
                    continue;
                }
                if in_repair_mode && next_abs_imbalance > current_abs_imbalance + 1e-9 {
                    notes.push(format!(
                        "paired_core repair-continuation bundle blocked idx={idx} current_abs_imbalance={current_abs_imbalance:.4} next_abs_imbalance={next_abs_imbalance:.4}",
                    ));
                    continue;
                }
                let tag = format!("ladder:{idx}");
                let yes_changed = self.should_emit(
                    &market_id,
                    LadderLeg::Yes,
                    &tag,
                    yes_intent.limit_price,
                    yes_intent.quantity,
                );
                let no_changed = self.should_emit(
                    &market_id,
                    LadderLeg::No,
                    &tag,
                    no_intent.limit_price,
                    no_intent.quantity,
                );
                if yes_changed || no_changed {
                    projected_yes_qty = next_yes;
                    projected_no_qty = next_no;
                    intents.push(yes_intent);
                    intents.push(no_intent);
                }
            }
            notes.push(format!(
                "paired_core ladder levels={levels} span={:.4} yes_mid={yes_mid:.4} no_mid={no_mid:.4} projected_yes_final={projected_yes_qty:.4} projected_no_final={projected_no_qty:.4}",
                half_span * 2.0,
            ));

            return if intents.is_empty() {
                StrategyDecision::Noop { notes }
            } else {
                StrategyDecision::QuoteSet { intents, notes }
            };
        }

        let Some(geom) = classify_legs(&input.snapshot, &cfg) else {
            return StrategyDecision::Noop {
                notes: vec![format!(
                    "paired_core geometry mismatch yes_ask={:?} no_ask={:?}",
                    input.snapshot.yes_quote.best_ask.as_ref().map(|l| l.price),
                    input.snapshot.no_quote.best_ask.as_ref().map(|l| l.price),
                )],
            };
        };

        // Current per-leg notional from inventory (cost basis, not market).
        let (expensive_qty, expensive_avg, cheap_qty, cheap_avg) = match geom.expensive_leg {
            LadderLeg::Yes => (
                input.paired_core_inventory.yes_qty,
                input.paired_core_inventory.yes_avg_cost,
                input.paired_core_inventory.no_qty,
                input.paired_core_inventory.no_avg_cost,
            ),
            LadderLeg::No => (
                input.paired_core_inventory.no_qty,
                input.paired_core_inventory.no_avg_cost,
                input.paired_core_inventory.yes_qty,
                input.paired_core_inventory.yes_avg_cost,
            ),
        };
        let expensive_notional = expensive_qty.max(0.0) * expensive_avg.max(0.0);
        let cheap_notional = cheap_qty.max(0.0) * cheap_avg.max(0.0);

        // Target allocation: expensive gets (1 / (1 + ratio)), cheap gets ratio / (1 + ratio).
        let ratio = cfg.target_hedge_ratio.clamp(0.0, 1.0);
        let denom = 1.0 + ratio;
        let expensive_target = cfg.bar_capital_usd / denom;
        let cheap_target = cfg.bar_capital_usd * ratio / denom;

        let expensive_gap = (expensive_target - expensive_notional).max(0.0);
        let cheap_gap = (cheap_target - cheap_notional).max(0.0);

        let mut intents = Vec::new();
        let mut notes = Vec::new();
        notes.push(format!(
            "paired_core geom expensive={:?}@{:.4} cheap={:?}@{:.4} gap={:.4}",
            geom.expensive_leg,
            geom.expensive_ask,
            geom.cheap_leg,
            geom.cheap_ask,
            geom.expensive_ask - geom.cheap_ask,
        ));
        notes.push(format!(
            "paired_core alloc expensive_notional={:.2}/{:.2} cheap_notional={:.2}/{:.2}",
            expensive_notional, expensive_target, cheap_notional, cheap_target,
        ));

        let market_id = input.market.market_id().clone();
        let cfg_min_order = cfg.min_order_usd;
        let cfg_core_clip = cfg.core_clip_usd;
        let cfg_hedge_clip = cfg.hedge_clip_usd;
        let cfg_improve = cfg.maker_improve_ticks;
        let imbalance_tolerance = input.market.min_order_size().max(0.5);
        if expensive_gap >= cfg_min_order
            && paired_core_leg_allowed(
                geom.expensive_leg,
                input.paired_core_inventory.yes_qty,
                input.paired_core_inventory.no_qty,
                imbalance_tolerance,
            )
        {
            let clip = cfg_core_clip.min(expensive_gap).max(cfg_min_order);
            if let Some(intent) = build_clip(
                &input.market,
                geom.expensive_leg,
                geom.expensive_bid,
                geom.expensive_ask,
                clip,
                "core",
                cfg_improve,
                cfg_min_order,
                input.now_ms,
            ) {
                if self.should_emit(
                    &market_id,
                    geom.expensive_leg,
                    "core",
                    intent.limit_price,
                    intent.quantity,
                ) {
                    intents.push(intent);
                }
            }
        } else if expensive_gap >= cfg_min_order {
            notes.push(format!(
                "paired_core suppressing heavier expensive leg={:?} core_yes={:.4} core_no={:.4}",
                geom.expensive_leg,
                input.paired_core_inventory.yes_qty,
                input.paired_core_inventory.no_qty,
            ));
        }

        if cheap_gap >= cfg_min_order
            && paired_core_leg_allowed(
                geom.cheap_leg,
                input.paired_core_inventory.yes_qty,
                input.paired_core_inventory.no_qty,
                imbalance_tolerance,
            )
        {
            let clip = cfg_hedge_clip.min(cheap_gap).max(cfg_min_order);
            if let Some(intent) = build_clip(
                &input.market,
                geom.cheap_leg,
                geom.cheap_bid,
                geom.cheap_ask,
                clip,
                "hedge",
                cfg_improve,
                cfg_min_order,
                input.now_ms,
            ) {
                if self.should_emit(
                    &market_id,
                    geom.cheap_leg,
                    "hedge",
                    intent.limit_price,
                    intent.quantity,
                ) {
                    intents.push(intent);
                }
            }
        } else if cheap_gap >= cfg_min_order {
            notes.push(format!(
                "paired_core suppressing heavier cheap leg={:?} core_yes={:.4} core_no={:.4}",
                geom.cheap_leg,
                input.paired_core_inventory.yes_qty,
                input.paired_core_inventory.no_qty,
            ));
        }

        if intents.is_empty() {
            StrategyDecision::Noop { notes }
        } else {
            StrategyDecision::QuoteSet { intents, notes }
        }
    }

    fn on_fill(&mut self, _input: StrategyFillInput<M>) -> StrategyDecision {
        // P2: trigger merge planner when paired_qty crosses threshold.
        StrategyDecision::Noop {
            notes: vec!["paired_core on_fill not yet wired".to_string()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::BookLevel;
    use crate::market_making::pairing::pair_cost_tracker::PairCostTracker;
    use crate::market_making::pairing::types::PairedInventorySnapshot;
    use crate::markets::BinaryOutcomeMarket;
    use crate::signals::{
        BtcRegimeSnapshot, FairValueEstimate, FairValueModel, MomentumSignal,
        OrderBookPressureSignal,
    };
    use crate::strategies::traits::PairedOpenOrderExposure;
    use crate::types::{QuoteSnapshot, StrategyDecision};

    fn snap_with_quotes(
        yes_bid: f64,
        yes_ask: f64,
        no_bid: f64,
        no_ask: f64,
    ) -> PairedMarketSnapshot {
        let yes_quote = QuoteSnapshot {
            best_bid: Some(BookLevel::new(yes_bid, 100.0)),
            best_ask: Some(BookLevel::new(yes_ask, 100.0)),
            ..QuoteSnapshot::default()
        };
        let no_quote = QuoteSnapshot {
            best_bid: Some(BookLevel::new(no_bid, 100.0)),
            best_ask: Some(BookLevel::new(no_ask, 100.0)),
            ..QuoteSnapshot::default()
        };
        PairedMarketSnapshot {
            market_id: "m1".into(),
            yes_instrument_id: "yes".into(),
            no_instrument_id: "no".into(),
            yes_quote,
            no_quote,
        }
    }

    fn test_market(snapshot: &PairedMarketSnapshot) -> BinaryOutcomeMarket {
        let mut market = BinaryOutcomeMarket::btc_5m(
            snapshot.market_id.clone(),
            snapshot.yes_instrument_id.clone(),
            snapshot.no_instrument_id.clone(),
        );
        market.event_start_ms = Some(0);
        market.event_end_ms = Some(300_000);
        market
    }

    fn neutral_fair_value() -> FairValueEstimate {
        FairValueEstimate {
            p_up: 0.5,
            p_down: 0.5,
            log_moneyness: 0.0,
            sigma_remaining: 0.0,
            time_remaining_s: 300.0,
            model: FairValueModel::BsmBinary,
        }
    }

    fn strategy_input(snapshot: PairedMarketSnapshot) -> StrategyInput<BinaryOutcomeMarket> {
        let market = test_market(&snapshot);
        StrategyInput {
            market,
            snapshot,
            inventory: PairedInventorySnapshot::default(),
            paired_core_inventory: PairedInventorySnapshot::default(),
            late_fav_inventory: PairedInventorySnapshot::default(),
            cheap_tail_inventory: PairedInventorySnapshot::default(),
            open_convex_order_exposure: PairedOpenOrderExposure::default(),
            open_late_fav_order_exposure: PairedOpenOrderExposure::default(),
            open_paired_core_order_exposure: PairedOpenOrderExposure::default(),
            open_orders: Vec::new(),
            pair_cost: PairCostTracker::default(),
            fair_value: neutral_fair_value(),
            btc_regime: BtcRegimeSnapshot::default(),
            momentum: MomentumSignal::default(),
            order_book_pressure: OrderBookPressureSignal::default(),
            now_ms: 60_000,
        }
    }

    fn live_like_paired_core_config() -> CoreHedgeMmConfig {
        CoreHedgeMmConfig {
            enabled: true,
            ladder_levels: 17,
            ladder_span: 0.60,
            center_probe_only: true,
            clip_shares: 5.0,
            max_unpaired_core_qty: 5.0,
            ladder_min_price: 0.20,
            ladder_max_price: 0.70,
            min_order_usd: 1.0,
            ..CoreHedgeMmConfig::default()
        }
    }

    #[test]
    fn classifier_picks_yes_as_favorite_when_yes_ask_is_higher() {
        let cfg = CoreHedgeMmConfig::default();
        let snap = snap_with_quotes(0.69, 0.70, 0.29, 0.30);
        let geom = classify_legs(&snap, &cfg).expect("geometry should fit");
        assert_eq!(geom.expensive_leg, LadderLeg::Yes);
        assert_eq!(geom.cheap_leg, LadderLeg::No);
        assert!((geom.expensive_ask - 0.70).abs() < 1e-9);
        assert!((geom.cheap_ask - 0.30).abs() < 1e-9);
    }

    #[test]
    fn classifier_rejects_when_gap_too_small() {
        let cfg = CoreHedgeMmConfig::default();
        let snap = snap_with_quotes(0.49, 0.50, 0.49, 0.50);
        assert!(classify_legs(&snap, &cfg).is_none());
    }

    #[test]
    fn classifier_rejects_when_cheap_leg_too_expensive() {
        let cfg = CoreHedgeMmConfig::default();
        let snap = snap_with_quotes(0.59, 0.60, 0.39, 0.40);
        assert!(classify_legs(&snap, &cfg).is_none());
    }

    #[test]
    fn barbell_suppression_does_not_kill_mid_whipsaw_presence() {
        assert!(paired_core_directional_barbell_book(0.86, 0.16));
        assert!(paired_core_directional_barbell_book(0.96, 0.04));
        assert!(!paired_core_directional_barbell_book(0.62, 0.42));
        assert!(!paired_core_directional_barbell_book(0.58, 0.46));
    }

    #[test]
    fn opening_chop_keeps_center_band_for_mid_markets() {
        assert_eq!(paired_core_center_accumulator_band(17, 0.62, 0.42), 3);
        assert_eq!(paired_core_center_accumulator_band(17, 0.74, 0.26), 2);
        assert_eq!(paired_core_center_accumulator_band(17, 0.86, 0.16), 0);
    }

    #[test]
    fn broad_ladder_maker_improve_only_moves_near_touch_prices() {
        assert!((improved_ladder_price(0.4900, 0.5000, 0.5300, 0.0100, 2.0) - 0.5200).abs() < 1e-9);
        assert!((improved_ladder_price(0.4500, 0.5000, 0.5300, 0.0100, 2.0) - 0.4500).abs() < 1e-9);
        assert!((improved_ladder_price(0.5200, 0.5000, 0.5300, 0.0100, 2.0) - 0.5200).abs() < 1e-9);
        assert!((improved_ladder_price(0.4900, 0.5000, 0.5100, 0.0100, 2.0) - 0.5000).abs() < 1e-9);
        assert!((improved_ladder_price(0.4900, 0.5000, 0.5300, 0.0100, 0.0) - 0.4900).abs() < 1e-9);
    }

    #[test]
    fn whipsaw_mid_book_uses_center_only_not_full_suppression() {
        let posture = paired_core_broad_posture_for_regime(
            Some(BtcRegime::Whipsaw),
            180_000,
            None,
            0.62,
            0.42,
            17,
        );
        assert!(matches!(
            posture,
            PairedCoreBroadPosture::CenterOnly { center_band: 3, .. }
        ));
    }

    #[test]
    fn directional_mid_book_uses_center_only_before_favorite_takeover() {
        let posture = paired_core_broad_posture_for_regime(
            Some(BtcRegime::DirectionalSmooth),
            130_000,
            None,
            0.74,
            0.26,
            17,
        );
        assert!(matches!(
            posture,
            PairedCoreBroadPosture::CenterOnly { center_band: 2, .. }
        ));
    }

    #[test]
    fn barbell_book_still_suppresses_paired_core() {
        let posture = paired_core_broad_posture_for_regime(None, 60_000, None, 0.96, 0.04, 17);
        assert!(matches!(posture, PairedCoreBroadPosture::Suppressed { .. }));
    }

    #[test]
    fn live_like_center_probe_does_not_emit_broad_ladder() {
        let config = live_like_paired_core_config();
        let mut strategy = CoreHedgeMmStrategy::new(CoreHedgeMmStrategyConfig {
            core_hedge: config,
        });
        let input = strategy_input(snap_with_quotes(0.48, 0.62, 0.48, 0.62));

        let decision = strategy.on_tick(input);

        match decision {
            StrategyDecision::QuoteSet { intents, notes } => {
                assert!(intents.len() <= PAIRED_CORE_CENTER_PROBE_MAX_LEVELS * 2);
                for pair in intents.chunks_exact(2) {
                    assert_eq!(pair[0].pair_id, pair[1].pair_id);
                    assert!(pair[0]
                        .pair_id
                        .as_deref()
                        .is_some_and(|id| id.starts_with("paired-core:")));
                }
                assert!(intents.iter().all(|intent| {
                    intent.limit_price >= PAIRED_CORE_CENTER_PROBE_MIN_PRICE - 1e-9
                        && intent.limit_price <= PAIRED_CORE_CENTER_PROBE_MAX_PRICE + 1e-9
                }));
                assert!(notes.iter().any(|note| note.contains("center probe active")));
            }
            other => panic!("expected constrained center-probe quotes, got {other:?}"),
        }
    }

    #[test]
    fn mate_repair_emits_only_when_pair_cost_is_positive() {
        let mut config = live_like_paired_core_config();
        config.repair_fee_buffer = 0.01;
        let mut positive_input = strategy_input(snap_with_quotes(0.58, 0.62, 0.38, 0.40));
        positive_input.paired_core_inventory = PairedInventorySnapshot {
            yes_qty: 5.0,
            yes_avg_cost: 0.50,
            no_qty: 0.0,
            no_avg_cost: 0.0,
            ..PairedInventorySnapshot::default()
        };
        let mut strategy = CoreHedgeMmStrategy::new(CoreHedgeMmStrategyConfig {
            core_hedge: config,
        });

        let positive_decision = strategy.on_tick(positive_input);

        match positive_decision {
            StrategyDecision::QuoteSet { intents, notes } => {
                assert_eq!(intents.len(), 1);
                assert_eq!(
                    intents[0].quote_level_tag.as_deref(),
                    Some("paired-core:repair:mate")
                );
                assert!(notes.iter().any(|note| note.contains("mate repair emitted")));
                assert!(
                    !notes
                        .iter()
                        .any(|note| note.contains("balanced bundle mode"))
                );
            }
            other => panic!("expected positive pair-cost mate repair, got {other:?}"),
        }

        let mut negative_input = strategy_input(snap_with_quotes(0.58, 0.62, 0.38, 0.40));
        negative_input.paired_core_inventory = PairedInventorySnapshot {
            yes_qty: 5.0,
            yes_avg_cost: 0.58,
            no_qty: 0.0,
            no_avg_cost: 0.0,
            ..PairedInventorySnapshot::default()
        };
        let mut strategy = CoreHedgeMmStrategy::new(CoreHedgeMmStrategyConfig {
            core_hedge: config,
        });

        let negative_decision = strategy.on_tick(negative_input);

        match negative_decision {
            StrategyDecision::Noop { notes } => {
                assert!(notes.iter().any(|note| {
                    note.contains("mate repair skipped") && note.contains("bundle salvage")
                }));
                assert!(
                    !notes
                        .iter()
                        .any(|note| note.contains("balanced bundle mode"))
                );
            }
            other => panic!("expected negative pair-cost repair skip, got {other:?}"),
        }
    }

    #[test]
    fn should_emit_suppresses_subtick_price_drift() {
        let mut strategy = CoreHedgeMmStrategy::new(CoreHedgeMmStrategyConfig::default());
        let market_id = MarketId::from("m1");
        // First emission always allowed.
        assert!(strategy.should_emit(&market_id, LadderLeg::Yes, "ladder:0", 0.42, 5.0));
        // Sub-half-tick price drift (<0.005) and sub-half-share qty drift
        // must NOT re-emit. Cancel+repost on every fair-value microchange
        // destroys FIFO queue position; keeping the existing quote in place
        // preserves time priority on the venue.
        assert!(!strategy.should_emit(&market_id, LadderLeg::Yes, "ladder:0", 0.4225, 5.1));
        assert!(!strategy.should_emit(&market_id, LadderLeg::Yes, "ladder:0", 0.4180, 4.95));
        assert!(!strategy.should_emit(&market_id, LadderLeg::Yes, "ladder:0", 0.4205, 5.3));
    }

    #[test]
    fn should_emit_requotes_on_full_tick_price_move() {
        let mut strategy = CoreHedgeMmStrategy::new(CoreHedgeMmStrategyConfig::default());
        let market_id = MarketId::from("m1");
        assert!(strategy.should_emit(&market_id, LadderLeg::Yes, "ladder:0", 0.42, 5.0));
        // Full tick (0.01) crosses the half-tick threshold: re-emit.
        assert!(strategy.should_emit(&market_id, LadderLeg::Yes, "ladder:0", 0.43, 5.0));
    }

    #[test]
    fn should_emit_requotes_on_meaningful_qty_change() {
        let mut strategy = CoreHedgeMmStrategy::new(CoreHedgeMmStrategyConfig::default());
        let market_id = MarketId::from("m1");
        assert!(strategy.should_emit(&market_id, LadderLeg::Yes, "ladder:0", 0.42, 5.0));
        // Quantity change above the half-share threshold: re-emit.
        assert!(strategy.should_emit(&market_id, LadderLeg::Yes, "ladder:0", 0.42, 6.0));
    }
}
