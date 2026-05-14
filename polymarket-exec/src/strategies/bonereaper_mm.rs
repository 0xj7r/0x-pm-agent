//! First-class `bonereaper_mm` strategy entrypoint.
//!
//! This strategy is implemented as a single strategy module (single Rust file)
//! that composes the canonical paired-core and late-bar directional behaviors.
//! The wiring stays in one place so the shape can be managed cleanly.

use std::collections::HashMap;

use crate::core::types::{ClientOrderId, EpochMillis, IntentKind, OrderIntent};
use crate::market_making::pairing::types::{LadderLeg, PairedMarketSnapshot};
use crate::markets::MarketDescriptor;
use crate::signals::BtcRegime;
use crate::strategies::core_hedge_mm::{CoreHedgeMmStrategy, CoreHedgeMmStrategyConfig};
use crate::strategies::traits::{StrategyFillInput, StrategyInput, TradingStrategy};
use crate::types::{CoolingReason, MarketId, RuntimeCommand, StrategyDecision, SuppressionScope};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FavoriteClimbConfig {
    pub enabled: bool,
    /// Minimum favorite ask to qualify as loadable favorite. Sub-90c loads
    /// are allowed only as small spot-confirmed overlays; 90c+ is the heavy
    /// late-cert regime.
    pub min_favorite_ask: f64,
    /// Maximum favorite ask to qualify (avoid zero edge near 0.99+).
    pub max_favorite_ask: f64,
    /// Only fire in the last `window_sec` of bar.
    pub window_sec: u64,
    /// Optional bar-relative start fraction.
    pub start_frac: f64,
    /// Do not load the favorite during the opening noise window.
    pub min_elapsed_sec: u64,
    /// Base clip size, used as floor before ramping.
    pub clip_usd: f64,
    /// Hard cap on directional notional per market.
    pub max_load_usd: f64,
    pub maker_improve_ticks: f64,
    pub min_order_usd: f64,
    /// Spot filter: require BTC move >= this threshold (bps) in the side of
    /// the loaded favorite.
    pub spot_filter_bps: f64,
    /// If true, skip favorite loading when spot filter does not match.
    pub require_spot_match: bool,
    /// Clip/cap multiplier in noise-dominant high-vol regimes.
    pub regime_whipsaw_multiplier: f64,
    /// Override multiplier once the favorite is 90c+ in whipsaw. This keeps
    /// sub-90 probes defensive while allowing true late-cert to express.
    pub whipsaw_true_favorite_multiplier: f64,
    /// Extra maker improvement for 85-89c favorite orders. These are still
    /// post-only, but should sit near-touch rather than passively behind.
    pub near_touch_min_favorite_ask: f64,
    pub near_touch_maker_improve_ticks: f64,
    /// Clip/cap multiplier in low-vol, low-trend regimes.
    pub regime_flat_multiplier: f64,
    /// Clip/cap multiplier when trend is present but volatility is elevated.
    pub regime_trending_volatile_multiplier: f64,
    /// Clip/cap multiplier while BTC regime is still warming up.
    pub regime_unknown_multiplier: f64,
    /// Additional multiplier when the latest short-horizon move has reversed
    /// against the favorite.
    pub reversal_multiplier: f64,
    /// Use aggressive FAK for the front late-favorite level once the favorite
    /// is sufficiently certain. Paired-core stays maker/post-only; this only
    /// applies to the directional late-cert lane where missing the fill is the
    /// dominant failure mode.
    pub taker_min_favorite_ask: f64,
    pub taker_window_sec: u64,
    /// Optional hard cutoff after which this phase is disabled.
    pub disable_after_ms: Option<EpochMillis>,
}

impl Default for FavoriteClimbConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_favorite_ask: 0.70,
            max_favorite_ask: 0.99,
            window_sec: 120,
            start_frac: 0.0,
            min_elapsed_sec: 10,
            clip_usd: 20.0,
            max_load_usd: 200.0,
            maker_improve_ticks: 0.0,
            min_order_usd: 1.0,
            spot_filter_bps: 10.0,
            require_spot_match: true,
            regime_whipsaw_multiplier: 0.25,
            whipsaw_true_favorite_multiplier: 0.70,
            near_touch_min_favorite_ask: 0.85,
            near_touch_maker_improve_ticks: 3.0,
            regime_flat_multiplier: 0.50,
            regime_trending_volatile_multiplier: 0.60,
            regime_unknown_multiplier: 0.70,
            reversal_multiplier: 0.50,
            taker_min_favorite_ask: 0.90,
            taker_window_sec: 120,
            disable_after_ms: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConvexTailConfig {
    pub enabled: bool,
    /// Maximum cheap-leg ask that qualifies.
    pub max_cheap_ask: f64,
    /// Only fire in the final `window_sec` of the bar.
    pub window_sec: u64,
    /// Optional bar-relative start fraction.
    pub start_frac: f64,
    pub clip_usd: f64,
    pub max_load_usd: f64,
    /// Maximum hedge spend as a fraction of filled late-favorite cost. Cheaper
    /// tails naturally buy more shares for the same spend, but the budget is
    /// set from exposure so the hedge remains material in choppy tape.
    pub max_favorite_exposure_fraction: f64,
    /// Hard upper bound as a fraction of the late-favorite win-upside. If the
    /// favorite wins, cheap-tail loses; this cap prevents the hedge from
    /// consuming the expected upside edge of the directional leg.
    pub max_win_edge_spend_fraction: f64,
    pub maker_improve_ticks: f64,
    pub min_order_usd: f64,
    /// Optional hard cutoff after which this phase is disabled.
    pub disable_after_ms: Option<EpochMillis>,
}

impl Default for ConvexTailConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_cheap_ask: 0.20,
            window_sec: 60,
            start_frac: 0.0,
            clip_usd: 1.25,
            max_load_usd: 8.0,
            max_favorite_exposure_fraction: 0.25,
            max_win_edge_spend_fraction: 0.50,
            maker_improve_ticks: 0.0,
            min_order_usd: 0.5,
            disable_after_ms: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReversalHedgeConfig {
    pub enabled: bool,
    /// Lower bound for the opposite-leg ask. Cheaper than this belongs to
    /// pure convex-tail insurance.
    pub min_hedge_ask: f64,
    /// Upper bound for reversal hedge. Above this the hedge erodes too much
    /// of the late-favorite edge for tinylive sizing.
    pub max_hedge_ask: f64,
    /// Only fire in the final `window_sec` of the bar.
    pub window_sec: u64,
    /// Optional bar-relative start fraction.
    pub start_frac: f64,
    pub clip_usd: f64,
    pub max_load_usd: f64,
    /// Maximum hedge spend as a fraction of filled late-favorite cost when
    /// reversal risk is present. Multiplied by the live reversal score.
    pub max_favorite_exposure_fraction: f64,
    /// Shared edge-erosion cap against the favorite's win-upside. Existing
    /// cheap-tail/reversal-hedge fills and open orders count against it.
    pub max_win_edge_spend_fraction: f64,
    pub maker_improve_ticks: f64,
    pub min_order_usd: f64,
    /// Minimum live reversal score required before this lane can fire.
    pub min_reversal_score: f64,
    pub whipsaw_score_bonus: f64,
    pub flat_score_bonus: f64,
    pub trending_volatile_score_bonus: f64,
    pub directional_smooth_score_penalty: f64,
    /// Optional hard cutoff after which this phase is disabled.
    pub disable_after_ms: Option<EpochMillis>,
}

impl Default for ReversalHedgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_hedge_ask: 0.20,
            max_hedge_ask: 0.45,
            window_sec: 120,
            start_frac: 0.0,
            clip_usd: 2.0,
            max_load_usd: 8.0,
            max_favorite_exposure_fraction: 0.30,
            max_win_edge_spend_fraction: 0.65,
            maker_improve_ticks: 0.0,
            min_order_usd: 1.0,
            min_reversal_score: 0.45,
            whipsaw_score_bonus: 0.25,
            flat_score_bonus: 0.15,
            trending_volatile_score_bonus: 0.10,
            directional_smooth_score_penalty: 0.20,
            disable_after_ms: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct FavoriteEntryPolicy {
    min_price: f64,
    max_levels: usize,
    clip_multiplier: f64,
    cap_multiplier: f64,
    allow_taker: bool,
    near_touch_maker: bool,
    path_reversal_risk: f64,
    label: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LateFavoriteStrategyConfig {
    pub favorite_climb: FavoriteClimbConfig,
    pub convex_tail: ConvexTailConfig,
    pub reversal_hedge: ReversalHedgeConfig,
}

impl Default for LateFavoriteStrategyConfig {
    fn default() -> Self {
        Self {
            favorite_climb: FavoriteClimbConfig::default(),
            convex_tail: ConvexTailConfig::default(),
            reversal_hedge: ReversalHedgeConfig::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BonereaperMmStrategyConfig {
    pub core_hedge: CoreHedgeMmStrategyConfig,
    pub late_favorite: LateFavoriteStrategyConfig,
}

#[derive(Clone, Debug)]
pub struct BonereaperMmStrategy {
    paired_core: CoreHedgeMmStrategy,
    late_favorite: LateFavoriteStrategy,
    config: BonereaperMmStrategyConfig,
}

impl Default for BonereaperMmStrategyConfig {
    fn default() -> Self {
        Self {
            core_hedge: CoreHedgeMmStrategyConfig::default(),
            late_favorite: LateFavoriteStrategyConfig::default(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct LegQuotes {
    favorite_leg: LadderLeg,
    cheap_leg: LadderLeg,
    favorite_ask: f64,
    favorite_bid: f64,
    cheap_ask: f64,
    cheap_bid: f64,
}

#[derive(Clone, Debug)]
pub struct LateFavoriteStrategy {
    config: LateFavoriteStrategyConfig,
    reserved_directional_notional: HashMap<String, ReservedNotional>,
}

#[derive(Clone, Copy, Debug)]
struct ReservedNotional {
    notional_usd: f64,
    updated_at_ms: EpochMillis,
}

impl LateFavoriteStrategy {
    pub fn new(config: LateFavoriteStrategyConfig) -> Self {
        Self {
            config,
            reserved_directional_notional: HashMap::new(),
        }
    }

    fn reservation_key(market_id: &MarketId, leg: LadderLeg) -> String {
        format!("{market_id}:{leg:?}")
    }

    fn prune_reservations(&mut self, now_ms: EpochMillis) {
        const RESERVATION_TTL_MS: u64 = 20_000;
        self.reserved_directional_notional.retain(|_, reserved| {
            now_ms.saturating_sub(reserved.updated_at_ms) <= RESERVATION_TTL_MS
        });
    }

    fn reserved_notional(&self, market_id: &MarketId, leg: LadderLeg) -> f64 {
        self.reserved_directional_notional
            .get(&Self::reservation_key(market_id, leg))
            .map(|reserved| reserved.notional_usd.max(0.0))
            .unwrap_or(0.0)
    }

    fn reserve_notional(
        &mut self,
        market_id: &MarketId,
        leg: LadderLeg,
        notional_usd: f64,
        now_ms: EpochMillis,
    ) {
        if notional_usd <= 0.0 || !notional_usd.is_finite() {
            return;
        }
        let key = Self::reservation_key(market_id, leg);
        let entry = self
            .reserved_directional_notional
            .entry(key)
            .or_insert(ReservedNotional {
                notional_usd: 0.0,
                updated_at_ms: now_ms,
            });
        entry.notional_usd += notional_usd;
        entry.updated_at_ms = now_ms;
    }
}

fn read_legs(snapshot: &PairedMarketSnapshot) -> Option<LegQuotes> {
    let yes_ask = snapshot.yes_quote.best_ask.as_ref()?.price;
    let no_ask = snapshot.no_quote.best_ask.as_ref()?.price;
    let yes_bid = snapshot.yes_quote.best_bid.as_ref()?.price;
    let no_bid = snapshot.no_quote.best_bid.as_ref()?.price;

    let (favorite_leg, cheap_leg, favorite_ask, favorite_bid, cheap_ask, cheap_bid) =
        if yes_ask >= no_ask {
            (
                LadderLeg::Yes,
                LadderLeg::No,
                yes_ask,
                yes_bid,
                no_ask,
                no_bid,
            )
        } else {
            (
                LadderLeg::No,
                LadderLeg::Yes,
                no_ask,
                no_bid,
                yes_ask,
                yes_bid,
            )
        };

    Some(LegQuotes {
        favorite_leg,
        cheap_leg,
        favorite_ask,
        favorite_bid,
        cheap_ask,
        cheap_bid,
    })
}

fn maker_limit_price(bid: f64, ask: f64, tick: f64, improve_ticks: f64) -> Option<f64> {
    let mut px = bid + improve_ticks * tick;
    let max_passive = (ask - tick).max(tick);
    if px > max_passive {
        px = max_passive;
    }
    if px <= 0.0 || px >= 1.0 {
        return None;
    }
    Some(px)
}

fn late_favorite_maker_base_price(
    bid: f64,
    ask: f64,
    tick: f64,
    cfg: &FavoriteClimbConfig,
    entry_policy: &FavoriteEntryPolicy,
) -> Option<f64> {
    if !entry_policy.near_touch_maker {
        return maker_limit_price(bid, ask, tick, cfg.maker_improve_ticks);
    }
    let passive_px = maker_limit_price(bid, ask, tick, cfg.maker_improve_ticks)?;
    let near_touch_ticks = if ask >= cfg.near_touch_min_favorite_ask {
        1.0
    } else {
        2.0
    };
    let near_touch_px = ask - tick * near_touch_ticks;
    let max_passive = (ask - tick).max(tick);
    let px = passive_px.max(near_touch_px).min(max_passive);
    (px > 0.0 && px < ask && px < 1.0).then_some(px)
}

fn build_late_favorite_intent<M: MarketDescriptor>(
    market: &M,
    leg: LadderLeg,
    limit_price: f64,
    qty: f64,
    tag: &str,
    aggressive_taker: bool,
    reason: String,
    now_ms: EpochMillis,
) -> OrderIntent {
    let instrument_id = match leg {
        LadderLeg::Yes => market.yes_instrument_id().clone(),
        LadderLeg::No => market.no_instrument_id().clone(),
    };
    let coid = ClientOrderId::from(format!(
        "late-fav:{}:{}:{:?}:{}",
        tag,
        market.market_id(),
        leg,
        now_ms,
    ));
    let mut intent = OrderIntent::new_buy(
        coid,
        market.market_id().clone(),
        instrument_id,
        limit_price,
        qty,
        reason,
        now_ms,
    );
    intent.kind = IntentKind::Entry;
    intent.quote_level_tag = Some(if aggressive_taker {
        format!("late-fav-taker-{tag}")
    } else {
        format!("late-fav-{tag}")
    });
    intent
}

fn build_cheap_tail_intent<M: MarketDescriptor>(
    market: &M,
    leg: LadderLeg,
    limit_price: f64,
    qty: f64,
    tag: &str,
    reason: String,
    now_ms: EpochMillis,
) -> OrderIntent {
    let instrument_id = match leg {
        LadderLeg::Yes => market.yes_instrument_id().clone(),
        LadderLeg::No => market.no_instrument_id().clone(),
    };
    let coid = ClientOrderId::from(format!(
        "cheap-tail:{}:{}:{:?}:{}",
        tag,
        market.market_id(),
        leg,
        now_ms,
    ));
    let mut intent = OrderIntent::new_buy(
        coid,
        market.market_id().clone(),
        instrument_id,
        limit_price,
        qty,
        reason,
        now_ms,
    );
    intent.kind = IntentKind::Entry;
    intent.quote_level_tag = Some(format!("cheap-tail:{tag}"));
    intent
}

fn build_reversal_hedge_intent<M: MarketDescriptor>(
    market: &M,
    leg: LadderLeg,
    limit_price: f64,
    qty: f64,
    tag: &str,
    reason: String,
    now_ms: EpochMillis,
) -> OrderIntent {
    let instrument_id = match leg {
        LadderLeg::Yes => market.yes_instrument_id().clone(),
        LadderLeg::No => market.no_instrument_id().clone(),
    };
    let coid = ClientOrderId::from(format!(
        "reversal-hedge:{}:{}:{:?}:{}",
        tag,
        market.market_id(),
        leg,
        now_ms,
    ));
    let mut intent = OrderIntent::new_buy(
        coid,
        market.market_id().clone(),
        instrument_id,
        limit_price,
        qty,
        reason,
        now_ms,
    );
    intent.kind = IntentKind::Entry;
    // Keep the hedge in the existing cheap-tail inventory/accounting bucket
    // so reversal hedge and pure convex tail spend a shared hedge budget.
    intent.quote_level_tag = Some(format!("cheap-tail:reversal-hedge:{tag}"));
    intent
}

fn phase_window_ms(window_sec: u64, start_frac: f64, bar_window_ms: u64) -> u64 {
    let absolute_ms = window_sec.saturating_mul(1_000);
    let fractional_ms = if start_frac.is_finite() && start_frac > 0.0 && start_frac < 1.0 {
        ((1.0 - start_frac) * bar_window_ms as f64).round() as u64
    } else {
        0
    };
    absolute_ms.max(fractional_ms)
}

fn elapsed_ms<M: MarketDescriptor>(market: &M, now_ms: EpochMillis, remaining_ms: u64) -> u64 {
    market
        .event_start_ms()
        .map(|start_ms| now_ms.saturating_sub(start_ms))
        .unwrap_or_else(|| market.window_ms().saturating_sub(remaining_ms))
}

fn directional_exposure_usd(favorite_qty: f64, other_qty: f64, px: f64) -> f64 {
    let unmatched = (favorite_qty - other_qty).max(0.0);
    unmatched * px.max(0.0)
}

fn open_order_notional_for_leg(
    exposure: crate::strategies::traits::PairedOpenOrderExposure,
    leg: LadderLeg,
) -> f64 {
    match leg {
        LadderLeg::Yes => exposure.yes_notional_usd,
        LadderLeg::No => exposure.no_notional_usd,
    }
    .max(0.0)
}

fn inventory_qty_for_leg(
    inventory: crate::market_making::pairing::types::PairedInventorySnapshot,
    leg: LadderLeg,
) -> f64 {
    match leg {
        LadderLeg::Yes => inventory.yes_qty,
        LadderLeg::No => inventory.no_qty,
    }
    .max(0.0)
}

fn reactive_climb_clip_usd(
    cfg: &FavoriteClimbConfig,
    favorite_ask: f64,
    elapsed_ms: u64,
    remaining_ms: u64,
    bar_window_ms: u64,
) -> f64 {
    let remaining_sec = (remaining_ms as f64) / 1000.0;
    let elapsed_sec = (elapsed_ms as f64) / 1000.0;
    let min_elapsed_sec = cfg.min_elapsed_sec.max(1) as f64;
    let progress = if bar_window_ms > 0 {
        (elapsed_ms as f64 / bar_window_ms as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let raw_ramp = if elapsed_sec <= min_elapsed_sec {
        0.0
    } else if remaining_sec > 120.0 {
        // Mid-bar reactive load: the BTC move has already happened, but
        // there is still reversal risk, so keep this below the late-cert ramp.
        (cfg.clip_usd * (0.25 + 0.75 * progress)).max(cfg.min_order_usd)
    } else if remaining_sec > 60.0 {
        100.0
    } else if remaining_sec > 30.0 {
        130.0
    } else if remaining_sec > 15.0 {
        200.0
    } else if remaining_sec > 5.0 {
        240.0
    } else {
        250.0
    };
    let price_scale = favorite_load_price_scale(favorite_ask, cfg.min_favorite_ask);
    let ramp = raw_ramp * price_scale;
    ramp.max(cfg.min_order_usd)
}

fn favorite_momentum_clip_multiplier<M: MarketDescriptor>(
    input: &StrategyInput<M>,
    legs: &LegQuotes,
    cfg: &FavoriteClimbConfig,
) -> f64 {
    let threshold = cfg.spot_filter_bps.abs().max(1.0);
    let side_30 = input
        .btc_regime
        .return_30s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_60 = input
        .btc_regime
        .return_60s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_120 = input
        .btc_regime
        .return_120s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_180 = input
        .btc_regime
        .return_180s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let strongest = [side_60, side_120, side_180]
        .into_iter()
        .flatten()
        .fold(0.0_f64, f64::max);
    let recent = side_30.unwrap_or(strongest);
    let mut multiplier = (strongest / threshold).clamp(0.35, 1.0);
    if recent < threshold * 0.25 {
        multiplier *= 0.60;
    }
    if recent < 0.0 {
        multiplier *= 0.50;
    }
    if legs.favorite_ask < 0.80 {
        multiplier *= 0.75;
    }
    multiplier.clamp(0.20, 1.0)
}

fn late_favorite_regime_multiplier(
    regime: &crate::signals::BtcRegimeSnapshot,
    favorite_leg: LadderLeg,
    cfg: &FavoriteClimbConfig,
) -> f64 {
    let base = match regime.regime() {
        Some(BtcRegime::DirectionalSmooth) => 1.0,
        Some(BtcRegime::TrendingVolatile) => cfg.regime_trending_volatile_multiplier,
        Some(BtcRegime::Whipsaw) => cfg.regime_whipsaw_multiplier,
        Some(BtcRegime::Flat) => cfg.regime_flat_multiplier,
        None => cfg.regime_unknown_multiplier,
    };
    let threshold = cfg.spot_filter_bps.abs().max(1.0);
    let recent = regime
        .return_30s_bps
        .map(|r| signed_for_favorite(favorite_leg, r));
    let medium = regime
        .return_120s_bps
        .map(|r| signed_for_favorite(favorite_leg, r));
    let long = regime
        .return_180s_bps
        .map(|r| signed_for_favorite(favorite_leg, r));
    let previous_support = [medium, long].into_iter().flatten().fold(0.0_f64, f64::max);
    let reversal = recent
        .map(|r| r < -threshold * 0.35 || (r < 0.0 && previous_support >= threshold * 0.50))
        .unwrap_or(false);
    let reversal_multiplier = if reversal {
        cfg.reversal_multiplier
    } else {
        1.0
    };
    (base * reversal_multiplier).clamp(0.05, 1.0)
}

fn favorite_load_price_scale(favorite_ask: f64, min_favorite_ask: f64) -> f64 {
    if favorite_ask >= 0.90 {
        return 1.0;
    }
    let floor = min_favorite_ask.clamp(0.01, 0.89);
    let span = (0.90 - floor).max(0.01);
    let progress = ((favorite_ask - floor) / span).clamp(0.0, 1.0);
    0.15 + 0.85 * progress
}

fn signed_for_favorite(leg: LadderLeg, value_bps: f64) -> f64 {
    match leg {
        LadderLeg::Yes => value_bps,
        LadderLeg::No => -value_bps,
    }
}

fn favorite_probability(leg: LadderLeg, p_up: f64, p_down: f64) -> f64 {
    match leg {
        LadderLeg::Yes => p_up,
        LadderLeg::No => p_down,
    }
}

fn spot_vs_strike_bps<M: MarketDescriptor>(
    market: &M,
    spot: Option<f64>,
    leg: LadderLeg,
) -> Option<f64> {
    let spot = spot.filter(|v| v.is_finite() && *v > 0.0)?;
    let strike = market
        .price_to_beat()
        .filter(|v| v.is_finite() && *v > 0.0)?;
    Some(signed_for_favorite(
        leg,
        ((spot - strike) / strike) * 10_000.0,
    ))
}

fn favorite_direction_signal<M: MarketDescriptor>(
    input: &StrategyInput<M>,
    legs: &LegQuotes,
    cfg: &FavoriteClimbConfig,
) -> (bool, String) {
    if !cfg.require_spot_match {
        return (true, "spot gate disabled".to_string());
    }

    let threshold = cfg.spot_filter_bps.abs();
    let side_30 = input
        .btc_regime
        .return_30s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_60 = input
        .btc_regime
        .return_60s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_120 = input
        .btc_regime
        .return_120s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_180 = input
        .btc_regime
        .return_180s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_strike = spot_vs_strike_bps(
        &input.market,
        input.btc_regime.last_price,
        legs.favorite_leg,
    );
    let model_favorite = favorite_probability(
        legs.favorite_leg,
        input.fair_value.p_up,
        input.fair_value.p_down,
    );

    let momentum_ok = [side_60, side_120, side_180]
        .into_iter()
        .flatten()
        .any(|r| r >= threshold * 0.75);
    let no_sharp_reversal = side_30.map(|r| r >= -threshold * 0.35).unwrap_or(true);
    let strike_ok = side_strike
        .map(|m| m >= (threshold * 0.40).max(4.0))
        .unwrap_or(false)
        && no_sharp_reversal
        && [side_60, side_120, side_180]
            .into_iter()
            .flatten()
            .any(|r| r >= threshold * 0.35);
    let model_ok = model_favorite >= (legs.favorite_ask + 0.03).min(0.98)
        && no_sharp_reversal
        && (side_strike.map(|m| m >= 0.5).unwrap_or(false)
            || [side_60, side_120, side_180]
                .into_iter()
                .flatten()
                .any(|r| r >= threshold * 0.50));

    let ok = momentum_ok || strike_ok || model_ok;
    (
        ok,
        format!(
            "favorite_signal favorite={:?} ok={} ask={:.4} side_strike_bps={:?} side_30s_bps={:?} side_60s_bps={:?} side_120s_bps={:?} side_180s_bps={:?} model_favorite={:.4} threshold_bps={:.2} momentum_ok={} strike_ok={} model_ok={}",
            legs.favorite_leg,
            ok,
            legs.favorite_ask,
            side_strike,
            side_30,
            side_60,
            side_120,
            side_180,
            model_favorite,
            threshold,
            momentum_ok,
            strike_ok,
            model_ok,
        ),
    )
}

fn favorite_load_levels(favorite_ask: f64, remaining_ms: u64) -> usize {
    if favorite_ask >= 0.90 && remaining_ms <= 120_000 {
        5
    } else if favorite_ask >= 0.90 {
        4
    } else if favorite_ask >= 0.80 {
        3
    } else if favorite_ask >= 0.75 {
        2
    } else {
        1
    }
}

fn favorite_entry_policy<M: MarketDescriptor>(
    input: &StrategyInput<M>,
    legs: &LegQuotes,
    cfg: &FavoriteClimbConfig,
    elapsed_ms: u64,
) -> Option<FavoriteEntryPolicy> {
    let elapsed_sec = elapsed_ms / 1_000;
    if elapsed_sec < 180 || legs.favorite_ask < 0.70 || legs.favorite_ask > cfg.max_favorite_ask {
        return None;
    }

    let threshold = cfg.spot_filter_bps.abs().max(1.0);
    let side_30 = input
        .btc_regime
        .return_30s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_60 = input
        .btc_regime
        .return_60s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_120 = input
        .btc_regime
        .return_120s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_180 = input
        .btc_regime
        .return_180s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let strongest = [side_60, side_120, side_180]
        .into_iter()
        .flatten()
        .fold(0.0_f64, f64::max);
    let recent = side_30.unwrap_or(strongest);
    let early_late = elapsed_sec < 230;
    let whipsaw = matches!(input.btc_regime.regime(), Some(BtcRegime::Whipsaw));
    let clean_regime = matches!(
        input.btc_regime.regime(),
        Some(BtcRegime::DirectionalSmooth | BtcRegime::TrendingVolatile)
    );
    let flat_regime = matches!(input.btc_regime.regime(), Some(BtcRegime::Flat));
    let model_favorite = favorite_probability(
        legs.favorite_leg,
        input.fair_value.p_up,
        input.fair_value.p_down,
    );
    let path_reversal_risk = path_reversal_risk_score(input, legs);

    if recent < -threshold * 0.35 {
        return None;
    }
    if legs.favorite_ask < 0.90 && path_reversal_risk >= 0.50 {
        return None;
    }
    if early_late && path_reversal_risk >= 0.75 {
        return None;
    }

    if legs.favorite_ask < 0.80 {
        if whipsaw
            || path_reversal_risk >= 0.35
            || strongest < threshold * 1.25
            || model_favorite < 0.86
        {
            return None;
        }
        return Some(FavoriteEntryPolicy {
            min_price: 0.70,
            max_levels: 1,
            clip_multiplier: if early_late { 0.20 } else { 0.35 },
            cap_multiplier: if early_late { 0.20 } else { 0.35 },
            allow_taker: false,
            near_touch_maker: false,
            path_reversal_risk,
            label: if early_late {
                "probe_70_79_early"
            } else {
                "probe_70_79_late"
            },
        });
    }

    if legs.favorite_ask < 0.90 {
        if early_late
            && if flat_regime {
                strongest < threshold * 1.15 || model_favorite < 0.86
            } else {
                !clean_regime || strongest < threshold || model_favorite < 0.82
            }
        {
            return None;
        }
        if whipsaw && (model_favorite < 0.88 || strongest < threshold * 1.50) {
            return None;
        }
        let reversal_scale = (1.0 - 0.50 * path_reversal_risk).clamp(0.50, 1.0);
        let whipsaw_scale = if whipsaw { 0.50 } else { 1.0 };
        let flat_scale = if flat_regime && early_late { 0.80 } else { 1.0 };
        let clean_directional_persistence = !whipsaw
            && path_reversal_risk <= 0.20
            && strongest >= threshold * 1.50
            && recent >= threshold * 0.25
            && model_favorite >= 0.88;
        let near_touch_maker = legs.favorite_ask >= 0.85
            && !whipsaw
            && path_reversal_risk <= 0.35
            && (clean_directional_persistence || strongest >= threshold * 1.15);
        return Some(FavoriteEntryPolicy {
            min_price: 0.80,
            max_levels: if whipsaw {
                1
            } else if early_late {
                2
            } else {
                3
            },
            clip_multiplier: (if early_late { 0.40 } else { 0.70 })
                * reversal_scale
                * whipsaw_scale
                * flat_scale,
            cap_multiplier: (if early_late { 0.40 } else { 0.70 })
                * reversal_scale
                * whipsaw_scale
                * flat_scale,
            allow_taker: clean_directional_persistence
                && legs.favorite_ask >= 0.85
                && elapsed_sec >= 210,
            near_touch_maker,
            path_reversal_risk,
            label: if clean_directional_persistence && legs.favorite_ask >= 0.85 {
                "maker_ladder_80_89_clean_persistent"
            } else if whipsaw {
                "maker_ladder_80_89_whipsaw"
            } else if flat_regime && early_late {
                "maker_ladder_80_89_flat_early"
            } else if early_late {
                "maker_ladder_80_89_early"
            } else {
                "maker_ladder_80_89_late"
            },
        });
    }

    let whipsaw_scale = if whipsaw {
        cfg.whipsaw_true_favorite_multiplier.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let reversal_scale = (1.0 - 0.55 * path_reversal_risk).clamp(0.35, 1.0);
    Some(FavoriteEntryPolicy {
        min_price: if early_late { 0.90 } else { 0.85 },
        max_levels: if early_late && whipsaw {
            2
        } else if early_late {
            3
        } else {
            5
        },
        clip_multiplier: (if early_late { 0.60 } else { 1.0 }) * reversal_scale * whipsaw_scale,
        cap_multiplier: (if early_late { 0.60 } else { 1.0 }) * reversal_scale * whipsaw_scale,
        allow_taker: true,
        near_touch_maker: true,
        path_reversal_risk,
        label: if early_late && whipsaw {
            "true_late_fav_90_plus_whipsaw_early"
        } else if early_late {
            "true_late_fav_90_plus_early"
        } else {
            "true_late_fav_90_plus"
        },
    })
}

fn path_reversal_risk_score<M: MarketDescriptor>(
    input: &StrategyInput<M>,
    legs: &LegQuotes,
) -> f64 {
    let side_30 = input
        .btc_regime
        .return_30s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_60 = input
        .btc_regime
        .return_60s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_120 = input
        .btc_regime
        .return_120s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_180 = input
        .btc_regime
        .return_180s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_strike = spot_vs_strike_bps(
        &input.market,
        input.btc_regime.last_price,
        legs.favorite_leg,
    );
    let prior_support = [side_60, side_120, side_180]
        .into_iter()
        .flatten()
        .fold(0.0_f64, f64::max);
    let recent = side_30.unwrap_or(prior_support);
    let reversal_component = if prior_support > 0.0 && recent < prior_support * 0.25 {
        ((prior_support - recent).max(0.0) / prior_support.max(1.0)).clamp(0.0, 0.45)
    } else {
        0.0
    };
    let strike_component = side_strike
        .map(|m| {
            if prior_support >= 8.0 && m < 6.0 {
                ((6.0 - m) / 18.0).clamp(0.0, 0.25)
            } else {
                0.0
            }
        })
        .unwrap_or(0.0);
    let vol_component = input
        .btc_regime
        .realized_vol_5m_bps
        .map(|vol| ((vol - 8.0) / 24.0).clamp(0.0, 0.20))
        .unwrap_or(0.0);
    let regime_component = match input.btc_regime.regime() {
        Some(BtcRegime::Whipsaw) => 0.25,
        Some(BtcRegime::TrendingVolatile) => 0.12,
        Some(BtcRegime::Flat) => 0.10,
        Some(BtcRegime::DirectionalSmooth) | None => 0.0,
    };

    (reversal_component + strike_component + vol_component + regime_component).clamp(0.0, 1.0)
}

fn should_use_aggressive_favorite_taker(
    cfg: &FavoriteClimbConfig,
    favorite_ask: f64,
    remaining_ms: u64,
) -> bool {
    cfg.taker_window_sec > 0
        && remaining_ms <= cfg.taker_window_sec.saturating_mul(1_000)
        && favorite_ask >= cfg.taker_min_favorite_ask
        && favorite_ask < cfg.max_favorite_ask
}

fn cheap_tail_cap_usd(
    cfg: &ConvexTailConfig,
    late_fav_qty: f64,
    favorite_ask: f64,
    cheap_ask: f64,
    regime: Option<BtcRegime>,
    path_reversal_risk: f64,
) -> f64 {
    if late_fav_qty <= 0.0
        || favorite_ask <= 0.0
        || favorite_ask >= 1.0
        || cheap_ask <= 0.0
        || cheap_ask >= 1.0
    {
        return 0.0;
    }
    let favorite_notional = late_fav_qty * favorite_ask;
    let favorite_win_upside = late_fav_qty * (1.0 - favorite_ask);
    let coverage_fraction = (cheap_tail_coverage_fraction(cfg, regime)
        * (1.0 + path_reversal_risk.clamp(0.0, 1.0)))
    .clamp(0.0, 1.0);
    let exposure_budget = favorite_notional * coverage_fraction;
    let edge_erosion_cap = favorite_win_upside * cfg.max_win_edge_spend_fraction.max(0.0);

    cfg.max_load_usd.min(exposure_budget).min(edge_erosion_cap)
}

fn reversal_hedge_cap_usd(
    cfg: &ReversalHedgeConfig,
    late_fav_qty: f64,
    favorite_ask: f64,
    hedge_ask: f64,
    reversal_score: f64,
) -> f64 {
    if late_fav_qty <= 0.0
        || favorite_ask <= 0.0
        || favorite_ask >= 1.0
        || hedge_ask <= 0.0
        || hedge_ask >= 1.0
        || reversal_score <= 0.0
    {
        return 0.0;
    }
    let favorite_notional = late_fav_qty * favorite_ask;
    let favorite_win_upside = late_fav_qty * (1.0 - favorite_ask);
    let coverage_fraction = (cfg.max_favorite_exposure_fraction.max(0.0)
        * reversal_score.clamp(0.0, 1.0))
    .clamp(0.0, 1.0);
    let exposure_budget = favorite_notional * coverage_fraction;
    let edge_erosion_cap = favorite_win_upside * cfg.max_win_edge_spend_fraction.max(0.0);

    cfg.max_load_usd.min(exposure_budget).min(edge_erosion_cap)
}

fn cheap_tail_coverage_fraction(cfg: &ConvexTailConfig, regime: Option<BtcRegime>) -> f64 {
    let base = cfg.max_favorite_exposure_fraction.max(0.0);
    let regime_multiplier = match regime {
        // Choppy tape is exactly where cheap convexity is most useful: the
        // favorite signal can still be right, but late reversals are common.
        Some(BtcRegime::Whipsaw) => 2.0,
        Some(BtcRegime::Flat) => 1.5,
        Some(BtcRegime::TrendingVolatile) => 1.25,
        Some(BtcRegime::DirectionalSmooth) => 0.75,
        None => 1.0,
    };
    (base * regime_multiplier).clamp(0.0, 1.0)
}

fn reversal_hedge_score<M: MarketDescriptor>(
    input: &StrategyInput<M>,
    legs: &LegQuotes,
    cfg: &ReversalHedgeConfig,
) -> f64 {
    let side_30 = input
        .btc_regime
        .return_30s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_60 = input
        .btc_regime
        .return_60s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_120 = input
        .btc_regime
        .return_120s_bps
        .map(|r| signed_for_favorite(legs.favorite_leg, r));
    let side_strike = spot_vs_strike_bps(
        &input.market,
        input.btc_regime.last_price,
        legs.favorite_leg,
    );

    let reversal_component = side_30
        .map(|r| {
            if r < 0.0 {
                (-r / 10.0).clamp(0.0, 0.45)
            } else {
                0.0
            }
        })
        .unwrap_or(0.0);
    let momentum_decay_component = match (side_30, side_60, side_120) {
        (Some(recent), Some(medium), Some(long))
            if long > 0.0 && medium > 0.0 && recent < medium * 0.25 =>
        {
            ((medium - recent).max(0.0) / 20.0).clamp(0.0, 0.30)
        }
        _ => 0.0,
    };
    let strike_decay_component = side_strike
        .map(|m| {
            if m < 4.0 {
                ((4.0 - m) / 16.0).clamp(0.0, 0.20)
            } else {
                0.0
            }
        })
        .unwrap_or(0.0);
    let regime_component = match input.btc_regime.regime() {
        Some(BtcRegime::Whipsaw) => cfg.whipsaw_score_bonus,
        Some(BtcRegime::Flat) => cfg.flat_score_bonus,
        Some(BtcRegime::TrendingVolatile) => cfg.trending_volatile_score_bonus,
        Some(BtcRegime::DirectionalSmooth) => -cfg.directional_smooth_score_penalty,
        None => 0.0,
    };
    let path_component = path_reversal_risk_score(input, legs) * 0.50;

    (reversal_component
        + momentum_decay_component
        + strike_decay_component
        + regime_component
        + path_component)
        .clamp(0.0, 1.0)
}

fn cheap_tail_ladder_levels(cheap_ask: f64, budget_usd: f64, min_order_usd: f64) -> usize {
    if budget_usd <= 0.0 || min_order_usd <= 0.0 {
        return 0;
    }
    let budget_limited = (budget_usd / min_order_usd).floor().max(1.0) as usize;
    let desired = if cheap_ask <= 0.05 {
        3
    } else if cheap_ask <= 0.20 {
        2
    } else {
        1
    };
    desired.min(budget_limited).max(1)
}

impl<M> TradingStrategy<M> for LateFavoriteStrategy
where
    M: MarketDescriptor,
{
    fn name(&self) -> &'static str {
        "late_favorite_directional"
    }

    fn on_tick(&mut self, input: StrategyInput<M>) -> StrategyDecision {
        self.prune_reservations(input.now_ms);
        let climb_cfg = self.config.favorite_climb;
        let tail_cfg = self.config.convex_tail;
        let reversal_cfg = self.config.reversal_hedge;
        if !climb_cfg.enabled && !tail_cfg.enabled && !reversal_cfg.enabled {
            return StrategyDecision::Noop {
                notes: vec!["late_favorite all phases disabled".to_string()],
            };
        }

        let remaining_ms = input
            .market
            .time_remaining_ms(input.now_ms)
            .unwrap_or(input.market.window_ms());

        let Some(legs) = read_legs(&input.snapshot) else {
            return StrategyDecision::Noop {
                notes: vec!["late_favorite no quotes".to_string()],
            };
        };

        let tick = input.market.tick_size().max(0.0001);
        let bar_window_ms = input.market.window_ms();
        let elapsed_ms = elapsed_ms(&input.market, input.now_ms, remaining_ms);
        let mut intents = Vec::new();
        let mut notes = Vec::new();
        let climb_enabled = climb_cfg
            .disable_after_ms
            .map(|disable_ms| input.now_ms < disable_ms)
            .unwrap_or(true);
        let tail_enabled = tail_cfg
            .disable_after_ms
            .map(|disable_ms| input.now_ms < disable_ms)
            .unwrap_or(true);
        let reversal_hedge_enabled = reversal_cfg
            .disable_after_ms
            .map(|disable_ms| input.now_ms < disable_ms)
            .unwrap_or(true);
        if !climb_enabled && !tail_enabled && !reversal_hedge_enabled {
            return StrategyDecision::Noop {
                notes: vec![format!(
                    "late_favorite all phases disabled now_ms={} climb_cutoff={:?} tail_cutoff={:?} reversal_hedge_cutoff={:?}",
                    input.now_ms,
                    climb_cfg.disable_after_ms,
                    tail_cfg.disable_after_ms,
                    reversal_cfg.disable_after_ms
                )],
            };
        }

        let climb_window_ms =
            phase_window_ms(climb_cfg.window_sec, climb_cfg.start_frac, bar_window_ms);
        let regime_multiplier =
            late_favorite_regime_multiplier(&input.btc_regime, legs.favorite_leg, &climb_cfg);
        if climb_cfg.enabled
            && climb_enabled
            && remaining_ms <= climb_window_ms
            && elapsed_ms >= climb_cfg.min_elapsed_sec.saturating_mul(1_000)
            && legs.favorite_ask >= climb_cfg.min_favorite_ask
            && legs.favorite_ask <= climb_cfg.max_favorite_ask
        {
            let (direction_ok, signal_note) = favorite_direction_signal(&input, &legs, &climb_cfg);

            let entry_policy = if !direction_ok {
                notes.push(format!(
                    "late_favorite blocked by favorite signal {signal_note}",
                ));
                None
            } else if let Some(entry_policy) =
                favorite_entry_policy(&input, &legs, &climb_cfg, elapsed_ms)
            {
                notes.push(signal_note);
                notes.push(format!(
                    "late_favorite entry_policy={} min_price={:.2} max_levels={} clip_mult={:.2} cap_mult={:.2} path_reversal_risk={:.2}",
                    entry_policy.label,
                    entry_policy.min_price,
                    entry_policy.max_levels,
                    entry_policy.clip_multiplier,
                    entry_policy.cap_multiplier,
                    entry_policy.path_reversal_risk
                ));
                Some(entry_policy)
            } else {
                notes.push(format!(
                    "late_favorite blocked by entry tier ask={:.4} elapsed_ms={} regime={:?}",
                    legs.favorite_ask,
                    elapsed_ms,
                    input.btc_regime.regime()
                ));
                None
            };
            if let Some(entry_policy) = entry_policy {
                let (favorite_qty, other_qty) = match legs.favorite_leg {
                    LadderLeg::Yes => (input.inventory.yes_qty, input.inventory.no_qty),
                    LadderLeg::No => (input.inventory.no_qty, input.inventory.yes_qty),
                };
                let net_directional_exposure_usd =
                    directional_exposure_usd(favorite_qty, other_qty, legs.favorite_ask);
                let filled_late_fav_usd =
                    inventory_qty_for_leg(input.late_fav_inventory, legs.favorite_leg)
                        * legs.favorite_ask;
                let working_late_fav_usd = open_order_notional_for_leg(
                    input.open_late_fav_order_exposure,
                    legs.favorite_leg,
                );
                let reserved_late_fav_usd =
                    self.reserved_notional(input.market.market_id(), legs.favorite_leg);
                let current_exposure_usd = filled_late_fav_usd.max(net_directional_exposure_usd)
                    + working_late_fav_usd.max(reserved_late_fav_usd);
                let adjusted_max_load_usd =
                    climb_cfg.max_load_usd * regime_multiplier * entry_policy.cap_multiplier;
                let remaining_load = (adjusted_max_load_usd - current_exposure_usd).max(0.0);
                if remaining_load >= climb_cfg.min_order_usd {
                    if let Some(base_px) = late_favorite_maker_base_price(
                        legs.favorite_bid,
                        legs.favorite_ask,
                        tick,
                        &climb_cfg,
                        &entry_policy,
                    ) {
                        let raw_clip = reactive_climb_clip_usd(
                            &climb_cfg,
                            legs.favorite_ask,
                            elapsed_ms,
                            remaining_ms,
                            bar_window_ms,
                        );
                        let confidence_multiplier =
                            favorite_momentum_clip_multiplier(&input, &legs, &climb_cfg);
                        let per_level_clip = (raw_clip
                            * confidence_multiplier
                            * regime_multiplier
                            * entry_policy.clip_multiplier)
                            .max(climb_cfg.min_order_usd);
                        let mut load_left = remaining_load;
                        let use_sub90_fak = entry_policy.allow_taker
                            && legs.favorite_ask >= 0.80
                            && legs.favorite_ask < climb_cfg.taker_min_favorite_ask
                            && remaining_ms <= climb_cfg.taker_window_sec.saturating_mul(1_000);
                        let use_aggressive_taker = entry_policy.allow_taker
                            && (should_use_aggressive_favorite_taker(
                                &climb_cfg,
                                legs.favorite_ask,
                                remaining_ms,
                            ) || use_sub90_fak);
                        let level_count = (if use_aggressive_taker {
                            favorite_load_levels(legs.favorite_ask, remaining_ms)
                        } else if legs.favorite_ask < climb_cfg.taker_min_favorite_ask {
                            favorite_load_levels(legs.favorite_ask, remaining_ms).max(3)
                        } else {
                            favorite_load_levels(legs.favorite_ask, remaining_ms)
                        })
                        .min(entry_policy.max_levels);
                        for level in 0..level_count {
                            if load_left < climb_cfg.min_order_usd {
                                break;
                            }
                            let aggressive_taker = use_aggressive_taker && level == 0;
                            let px = if aggressive_taker {
                                legs.favorite_ask
                            } else {
                                base_px - tick * level as f64
                            };
                            if px + 1e-9 < entry_policy.min_price {
                                continue;
                            }
                            if px <= 0.0 || px > legs.favorite_ask {
                                continue;
                            }
                            if !aggressive_taker && px >= legs.favorite_ask {
                                continue;
                            }
                            let clip = per_level_clip
                                .min(climb_cfg.clip_usd)
                                .min(load_left)
                                .max(climb_cfg.min_order_usd);
                            let qty = (clip / px).max(input.market.min_order_size());
                            let reason = format!(
                                "late_favorite climb leg={:?} level={} mode={} px={:.4} ask={:.4} entry_policy={} price_scale={:.2} confidence_multiplier={:.2} regime_multiplier={:.2} clip_usd={:.2} cumulative={:.2}/{:.2} elapsed_ms={elapsed_ms} remaining_ms={remaining_ms}",
                                legs.favorite_leg,
                                level,
                                if aggressive_taker { "taker_fak" } else { "maker_post_only" },
                                px,
                                legs.favorite_ask,
                                entry_policy.label,
                                favorite_load_price_scale(legs.favorite_ask, climb_cfg.min_favorite_ask),
                                confidence_multiplier,
                                regime_multiplier,
                                clip,
                                current_exposure_usd + (remaining_load - load_left),
                                adjusted_max_load_usd,
                            );
                            notes.push(reason.clone());
                            intents.push(build_late_favorite_intent(
                                &input.market,
                                legs.favorite_leg,
                                px,
                                qty,
                                &format!("climb:{level}"),
                                aggressive_taker,
                                reason,
                                input.now_ms,
                            ));
                            self.reserve_notional(
                                input.market.market_id(),
                                legs.favorite_leg,
                                clip,
                                input.now_ms,
                            );
                            load_left -= clip;
                        }
                    }
                }
            }
        }

        if tail_cfg.enabled
            && tail_enabled
            && remaining_ms
                <= phase_window_ms(
                    tail_cfg.window_sec,
                    tail_cfg.start_frac,
                    input.market.window_ms(),
                )
            && legs.cheap_ask <= tail_cfg.max_cheap_ask
        {
            let late_fav_filled_qty = match legs.favorite_leg {
                LadderLeg::Yes => input.late_fav_inventory.yes_qty,
                LadderLeg::No => input.late_fav_inventory.no_qty,
            };
            let favorite_exposure_usd = late_fav_filled_qty * legs.favorite_ask;
            let cheap_tail_filled_qty = match legs.cheap_leg {
                LadderLeg::Yes => input.cheap_tail_inventory.yes_qty,
                LadderLeg::No => input.cheap_tail_inventory.no_qty,
            };
            let current_exposure_usd = (cheap_tail_filled_qty * legs.cheap_ask)
                + open_order_notional_for_leg(input.open_convex_order_exposure, legs.cheap_leg)
                + self.reserved_notional(input.market.market_id(), legs.cheap_leg);
            let tail_cap_usd = cheap_tail_cap_usd(
                &tail_cfg,
                late_fav_filled_qty,
                legs.favorite_ask,
                legs.cheap_ask,
                input.btc_regime.regime(),
                path_reversal_risk_score(&input, &legs),
            );
            let remaining_load = (tail_cap_usd - current_exposure_usd).max(0.0);
            if remaining_load >= tail_cfg.min_order_usd {
                if let Some(base_px) = maker_limit_price(
                    legs.cheap_bid,
                    legs.cheap_ask,
                    tick,
                    tail_cfg.maker_improve_ticks,
                ) {
                    let total_clip = tail_cfg
                        .clip_usd
                        .min(remaining_load)
                        .max(tail_cfg.min_order_usd);
                    let level_count = cheap_tail_ladder_levels(
                        legs.cheap_ask,
                        total_clip,
                        tail_cfg.min_order_usd,
                    );
                    let mut load_left = total_clip;
                    for level in 0..level_count {
                        if load_left < tail_cfg.min_order_usd {
                            break;
                        }
                        let remaining_levels = (level_count - level).max(1) as f64;
                        let clip = (load_left / remaining_levels)
                            .max(tail_cfg.min_order_usd)
                            .min(load_left);
                        let px = base_px - tick * level as f64;
                        if px <= 0.0 || px > legs.cheap_ask {
                            continue;
                        }
                        let qty = (clip / px).max(input.market.min_order_size());
                        let reason = format!(
                            "cheap_tail leg={:?} level={} px={:.4} ask={:.4} clip_usd={:.2} cumulative={:.2}/{:.2} favorite_exposure={:.2} favorite_win_upside={:.2} regime_multiplier={:.2} path_reversal_risk={:.2} remaining_ms={remaining_ms}",
                            legs.cheap_leg,
                            level,
                            px,
                            legs.cheap_ask,
                            clip,
                            current_exposure_usd + (total_clip - load_left),
                            tail_cap_usd,
                            favorite_exposure_usd,
                            late_fav_filled_qty * (1.0 - legs.favorite_ask).max(0.0),
                            regime_multiplier,
                            path_reversal_risk_score(&input, &legs),
                        );
                        notes.push(reason.clone());
                        intents.push(build_cheap_tail_intent(
                            &input.market,
                            legs.cheap_leg,
                            px,
                            qty,
                            &format!("{level}"),
                            reason,
                            input.now_ms,
                        ));
                        self.reserve_notional(
                            input.market.market_id(),
                            legs.cheap_leg,
                            clip,
                            input.now_ms,
                        );
                        load_left -= clip;
                    }
                }
            } else if favorite_exposure_usd < tail_cfg.min_order_usd {
                notes.push(format!(
                    "cheap_tail blocked: favorite exposure {:.2} below min {:.2}",
                    favorite_exposure_usd, tail_cfg.min_order_usd
                ));
            }
        }

        if reversal_cfg.enabled
            && reversal_hedge_enabled
            && remaining_ms
                <= phase_window_ms(
                    reversal_cfg.window_sec,
                    reversal_cfg.start_frac,
                    input.market.window_ms(),
                )
            && legs.cheap_ask >= reversal_cfg.min_hedge_ask
            && legs.cheap_ask <= reversal_cfg.max_hedge_ask
        {
            let late_fav_filled_qty = match legs.favorite_leg {
                LadderLeg::Yes => input.late_fav_inventory.yes_qty,
                LadderLeg::No => input.late_fav_inventory.no_qty,
            };
            let favorite_exposure_usd = late_fav_filled_qty * legs.favorite_ask;
            let hedge_filled_qty = match legs.cheap_leg {
                LadderLeg::Yes => input.cheap_tail_inventory.yes_qty,
                LadderLeg::No => input.cheap_tail_inventory.no_qty,
            };
            let current_hedge_usd = (hedge_filled_qty * legs.cheap_ask)
                + open_order_notional_for_leg(input.open_convex_order_exposure, legs.cheap_leg)
                + self.reserved_notional(input.market.market_id(), legs.cheap_leg);
            let reversal_score = reversal_hedge_score(&input, &legs, &reversal_cfg);
            let hedge_cap_usd = reversal_hedge_cap_usd(
                &reversal_cfg,
                late_fav_filled_qty,
                legs.favorite_ask,
                legs.cheap_ask,
                reversal_score,
            );
            let remaining_load = (hedge_cap_usd - current_hedge_usd).max(0.0);
            if reversal_score >= reversal_cfg.min_reversal_score
                && favorite_exposure_usd >= reversal_cfg.min_order_usd
                && remaining_load >= reversal_cfg.min_order_usd
            {
                if let Some(base_px) = maker_limit_price(
                    legs.cheap_bid,
                    legs.cheap_ask,
                    tick,
                    reversal_cfg.maker_improve_ticks,
                ) {
                    let total_clip = reversal_cfg
                        .clip_usd
                        .min(remaining_load)
                        .max(reversal_cfg.min_order_usd);
                    let level_count = cheap_tail_ladder_levels(
                        legs.cheap_ask,
                        total_clip,
                        reversal_cfg.min_order_usd,
                    )
                    .min(3);
                    let mut load_left = total_clip;
                    for level in 0..level_count {
                        if load_left < reversal_cfg.min_order_usd {
                            break;
                        }
                        let remaining_levels = (level_count - level).max(1) as f64;
                        let clip = (load_left / remaining_levels)
                            .max(reversal_cfg.min_order_usd)
                            .min(load_left);
                        let px = base_px - tick * level as f64;
                        if px <= 0.0 || px > legs.cheap_ask {
                            continue;
                        }
                        let qty = (clip / px).max(input.market.min_order_size());
                        let reason = format!(
                            "reversal_hedge leg={:?} level={} px={:.4} ask={:.4} clip_usd={:.2} cumulative={:.2}/{:.2} favorite_exposure={:.2} favorite_win_upside={:.2} reversal_score={:.2} remaining_ms={remaining_ms}",
                            legs.cheap_leg,
                            level,
                            px,
                            legs.cheap_ask,
                            clip,
                            current_hedge_usd + (total_clip - load_left),
                            hedge_cap_usd,
                            favorite_exposure_usd,
                            late_fav_filled_qty * (1.0 - legs.favorite_ask).max(0.0),
                            reversal_score,
                        );
                        notes.push(reason.clone());
                        intents.push(build_reversal_hedge_intent(
                            &input.market,
                            legs.cheap_leg,
                            px,
                            qty,
                            &format!("{level}"),
                            reason,
                            input.now_ms,
                        ));
                        self.reserve_notional(
                            input.market.market_id(),
                            legs.cheap_leg,
                            clip,
                            input.now_ms,
                        );
                        load_left -= clip;
                    }
                }
            } else if favorite_exposure_usd >= reversal_cfg.min_order_usd {
                notes.push(format!(
                    "reversal_hedge blocked: score={:.2}/{:.2} hedge_remaining={:.2} favorite_exposure={:.2} ask={:.4}",
                    reversal_score,
                    reversal_cfg.min_reversal_score,
                    remaining_load,
                    favorite_exposure_usd,
                    legs.cheap_ask
                ));
            }
        }

        if intents.is_empty() {
            StrategyDecision::Noop {
                notes: if notes.is_empty() {
                    vec![format!(
                        "late_favorite no fire favorite_ask={:.4} cheap_ask={:.4} elapsed_ms={} remaining_ms={}",
                        legs.favorite_ask, legs.cheap_ask, elapsed_ms, remaining_ms
                    )]
                } else {
                    notes
                },
            }
        } else {
            StrategyDecision::QuoteSet { intents, notes }
        }
    }

    fn on_fill(&mut self, _input: StrategyFillInput<M>) -> StrategyDecision {
        StrategyDecision::Noop {
            notes: vec!["late_favorite on_fill noop".to_string()],
        }
    }
}

impl BonereaperMmStrategy {
    pub fn new(config: BonereaperMmStrategyConfig) -> Self {
        Self {
            paired_core: CoreHedgeMmStrategy::new(config.core_hedge),
            late_favorite: LateFavoriteStrategy::new(config.late_favorite),
            config,
        }
    }

    pub fn config(&self) -> &BonereaperMmStrategyConfig {
        &self.config
    }

    fn combine(a: StrategyDecision, b: StrategyDecision) -> StrategyDecision {
        use StrategyDecision::*;
        let mut notes = Vec::new();
        let mut quote_intents = Vec::new();
        let mut reactive_intents = Vec::new();
        let mut runtime_commands = Vec::new();
        let mut hard_suppressed = false;
        let mut soft_suppressed = false;

        for decision in [a, b] {
            match decision {
                Noop {
                    notes: decision_notes,
                } => notes.extend(decision_notes),
                QuoteSet {
                    intents,
                    notes: decision_notes,
                } => {
                    quote_intents.extend(intents);
                    notes.extend(decision_notes);
                }
                CapitalRecycle {
                    intents,
                    notes: decision_notes,
                } => {
                    quote_intents.extend(intents);
                    notes.extend(decision_notes);
                }
                Rescue {
                    intents,
                    notes: decision_notes,
                } => {
                    reactive_intents.extend(intents);
                    notes.extend(decision_notes);
                }
                Merge {
                    intent,
                    notes: decision_notes,
                } => {
                    runtime_commands.push(RuntimeCommand::Merge(intent));
                    notes.extend(
                        decision_notes
                            .into_iter()
                            .filter(|note| !is_merge_planner_note(note)),
                    );
                }
                Mixed {
                    intents,
                    commands,
                    notes: decision_notes,
                } => {
                    reactive_intents.extend(intents);
                    notes.extend(
                        decision_notes
                            .into_iter()
                            .filter(|note| !is_merge_planner_note(note)),
                    );
                    for command in commands {
                        if matches!(command, RuntimeCommand::Merge(_)) {
                            runtime_commands.push(command);
                        } else {
                            notes.push(
                                "bonereaper ignored unsupported child runtime command".to_string(),
                            );
                        }
                    }
                }
                Suppress {
                    reason,
                    scope,
                    preserve_quotes: _,
                    notes: decision_notes,
                } => {
                    match scope {
                        SuppressionScope::AllActions => hard_suppressed = true,
                        SuppressionScope::PairedOnly | SuppressionScope::AllEntry => {
                            soft_suppressed = true;
                        }
                    }
                    notes.push(format!("bonereaper suppression reason: {reason:?}"));
                    notes.extend(decision_notes);
                }
            }
        }

        if hard_suppressed {
            return StrategyDecision::Suppress {
                scope: SuppressionScope::AllActions,
                reason: CoolingReason::Other("bonereaper_hard_suppressed".to_string()),
                preserve_quotes: false,
                notes,
            };
        }
        if !reactive_intents.is_empty() {
            reactive_intents.extend(quote_intents);
            return if runtime_commands.is_empty() {
                StrategyDecision::Rescue {
                    intents: reactive_intents,
                    notes,
                }
            } else {
                StrategyDecision::Mixed {
                    intents: reactive_intents,
                    commands: runtime_commands,
                    notes,
                }
            };
        }
        if !quote_intents.is_empty() {
            return if runtime_commands.is_empty() {
                StrategyDecision::QuoteSet {
                    intents: quote_intents,
                    notes,
                }
            } else {
                StrategyDecision::Mixed {
                    intents: quote_intents,
                    commands: runtime_commands,
                    notes,
                }
            };
        }
        if !runtime_commands.is_empty() {
            return StrategyDecision::Mixed {
                intents: Vec::new(),
                commands: runtime_commands,
                notes,
            };
        }
        if soft_suppressed {
            return StrategyDecision::Suppress {
                scope: SuppressionScope::AllEntry,
                reason: CoolingReason::Other("bonereaper_soft_suppressed".to_string()),
                preserve_quotes: false,
                notes,
            };
        }
        StrategyDecision::Noop { notes }
    }
}

fn is_merge_planner_note(note: &str) -> bool {
    note.contains("paired_core merging")
        || note.contains("paired_core merge skipped")
        || note.contains("bonereaper merge retained")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::BookLevel;
    use crate::types::QuoteSnapshot;

    fn snap(yes_bid: f64, yes_ask: f64, no_bid: f64, no_ask: f64) -> PairedMarketSnapshot {
        PairedMarketSnapshot {
            market_id: "m".into(),
            yes_instrument_id: "yes".into(),
            no_instrument_id: "no".into(),
            yes_quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new(yes_bid, 100.0)),
                best_ask: Some(BookLevel::new(yes_ask, 100.0)),
                ..QuoteSnapshot::default()
            },
            no_quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new(no_bid, 100.0)),
                best_ask: Some(BookLevel::new(no_ask, 100.0)),
                ..QuoteSnapshot::default()
            },
        }
    }

    #[test]
    fn read_legs_picks_higher_ask_as_favorite() {
        let s = snap(0.96, 0.97, 0.02, 0.03);
        let l = read_legs(&s).unwrap();
        assert_eq!(l.favorite_leg, LadderLeg::Yes);
        assert_eq!(l.cheap_leg, LadderLeg::No);
        assert!((l.favorite_ask - 0.97).abs() < 1e-9);
        assert!((l.cheap_ask - 0.03).abs() < 1e-9);
    }

    #[test]
    fn maker_limit_price_caps_below_ask() {
        let px = maker_limit_price(0.96, 0.97, 0.01, 0.0).unwrap();
        // Bid + 0 ticks = 0.96, but max_passive = ask - 1 tick = 0.96.
        assert!((px - 0.96).abs() < 1e-9);
    }

    #[test]
    fn phase_window_supports_bar_relative_late_windows() {
        assert_eq!(phase_window_ms(0, 0.90, 300_000), 30_000);
        assert_eq!(phase_window_ms(0, 0.90, 900_000), 90_000);
        assert_eq!(phase_window_ms(60, 0.0, 300_000), 60_000);
    }

    #[test]
    fn reactive_climb_clip_respects_opening_guard_and_time_ramp() {
        let cfg = FavoriteClimbConfig {
            clip_usd: 20.0,
            min_order_usd: 1.0,
            min_elapsed_sec: 10,
            min_favorite_ask: 0.70,
            ..FavoriteClimbConfig::default()
        };

        assert_eq!(
            reactive_climb_clip_usd(&cfg, 0.90, 5_000, 295_000, 300_000),
            1.0
        );
        let mid_bar = reactive_climb_clip_usd(&cfg, 0.90, 90_000, 210_000, 300_000);
        let late_bar = reactive_climb_clip_usd(&cfg, 0.90, 210_000, 90_000, 300_000);
        assert!(mid_bar > cfg.min_order_usd);
        assert!(mid_bar < cfg.clip_usd);
        assert!(late_bar >= 100.0);
    }

    #[test]
    fn favorite_load_price_scale_keeps_sub_90c_loads_smaller() {
        assert!(favorite_load_price_scale(0.70, 0.70) < favorite_load_price_scale(0.80, 0.70));
        assert!(favorite_load_price_scale(0.80, 0.70) < favorite_load_price_scale(0.90, 0.70));
        assert_eq!(favorite_load_price_scale(0.90, 0.70), 1.0);
    }

    #[test]
    fn remaining_directional_load_caps_to_unmatched_exposure() {
        let favorite_qty = 400.0;
        let other_qty = 300.0;
        let px = 0.98;

        let current_exposure = directional_exposure_usd(favorite_qty, other_qty, px);
        assert!((current_exposure - 98.0).abs() < 1e-9);
        assert!(((150.0_f64 - current_exposure).max(0.0) - 52.0).abs() < 1e-9);
        assert!(((90.0_f64 - current_exposure).max(0.0)).abs() < 1e-9);
    }

    #[test]
    fn cheap_tail_cap_is_bounded_by_favorite_win_upside() {
        let cfg = ConvexTailConfig {
            max_load_usd: 100.0,
            max_favorite_exposure_fraction: 0.25,
            max_win_edge_spend_fraction: 0.50,
            ..ConvexTailConfig::default()
        };

        // 100 shares loaded at 95c has $95 notional but only $5 win-upside.
        // Tail spend must be capped by upside budget, not raw favorite notional.
        assert!(
            (cheap_tail_cap_usd(&cfg, 100.0, 0.95, 0.01, Some(BtcRegime::Whipsaw), 0.0) - 2.5)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn cheap_tail_cap_uses_payoff_aware_reversal_coverage() {
        let cfg = ConvexTailConfig {
            max_load_usd: 100.0,
            max_favorite_exposure_fraction: 0.25,
            max_win_edge_spend_fraction: 0.50,
            ..ConvexTailConfig::default()
        };

        // 100 shares at 95c costs $95. In whipsaw, coverage target is doubled
        // from 25% to 50%. Spend is now budgeted directly from exposure,
        // then bounded by the favorite-upside erosion cap.
        assert!(
            (cheap_tail_cap_usd(&cfg, 100.0, 0.95, 0.05, Some(BtcRegime::Whipsaw), 0.0) - 2.5)
                .abs()
                < 1e-9
        );

        // At 25c, the same dollar budget buys fewer shares, but the cap is
        // still governed by exposure budget vs edge erosion, not payoff math.
        assert!(
            (cheap_tail_cap_usd(&cfg, 100.0, 0.95, 0.25, Some(BtcRegime::Whipsaw), 0.0) - 2.5)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn cheap_tail_cap_is_lower_in_directional_smooth_regime() {
        let cfg = ConvexTailConfig {
            max_load_usd: 100.0,
            max_favorite_exposure_fraction: 0.25,
            max_win_edge_spend_fraction: 10.0,
            ..ConvexTailConfig::default()
        };

        assert!(
            cheap_tail_cap_usd(
                &cfg,
                100.0,
                0.95,
                0.05,
                Some(BtcRegime::DirectionalSmooth),
                0.0,
            ) < cheap_tail_cap_usd(&cfg, 100.0, 0.95, 0.05, Some(BtcRegime::Whipsaw), 0.0)
        );
    }

    #[test]
    fn live_profile_cheap_tail_regime_shape_is_loaded_from_yaml() {
        let profile = crate::strategy_profile::StrategyProfile::load(std::path::Path::new(
            "config/strategies/whale_bonereaper_strategy.live.yaml",
        ))
        .expect("load live bonereaper profile");
        let cfg = profile.bonereaper_mm_config().late_favorite.convex_tail;

        assert!(cfg.enabled);
        assert_eq!(cfg.max_cheap_ask, 0.20);
        assert_eq!(cfg.max_load_usd, 14.0);
        assert_eq!(cfg.max_favorite_exposure_fraction, 0.50);
        assert_eq!(cfg.max_win_edge_spend_fraction, 0.80);
        assert!(
            cheap_tail_coverage_fraction(&cfg, Some(BtcRegime::DirectionalSmooth))
                < cheap_tail_coverage_fraction(&cfg, Some(BtcRegime::Whipsaw))
        );
    }

    #[test]
    fn late_favorite_regime_multiplier_penalizes_reversal_and_whipsaw() {
        let cfg = FavoriteClimbConfig {
            spot_filter_bps: 10.0,
            regime_whipsaw_multiplier: 0.25,
            reversal_multiplier: 0.50,
            ..FavoriteClimbConfig::default()
        };
        let regime = crate::signals::BtcRegimeSnapshot {
            realized_vol_5m_bps: Some(20.0),
            return_30s_bps: Some(-6.0),
            return_120s_bps: Some(8.0),
            return_180s_bps: Some(10.0),
            ..Default::default()
        };

        assert!(
            (late_favorite_regime_multiplier(&regime, LadderLeg::Yes, &cfg) - 0.125).abs() < 1e-9
        );
    }
}

impl<M: MarketDescriptor + Clone> TradingStrategy<M> for BonereaperMmStrategy {
    fn name(&self) -> &'static str {
        "bonereaper_mm"
    }

    fn on_tick(&mut self, input: StrategyInput<M>) -> StrategyDecision {
        let core_decision = self.paired_core.on_tick(input.clone());
        let late_decision = self.late_favorite.on_tick(input);
        Self::combine(core_decision, late_decision)
    }

    fn on_fill(&mut self, input: StrategyFillInput<M>) -> StrategyDecision {
        let core_decision = self.paired_core.on_fill(input.clone());
        let late_decision = self.late_favorite.on_fill(input);
        Self::combine(core_decision, late_decision)
    }
}
