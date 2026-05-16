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

const BINARY_MARKET_TICK_SIZE: f64 = 0.01;
const LATE_FAV_REARM_STABLE_BARS: u32 = 3;
const LATE_FAV_REARM_MAX_PATH_RISK: f64 = 0.35;
const LATE_FAV_REARM_TTL_MS: u64 = 180_000;

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
            window_sec: 300,
            start_frac: 0.10,
            min_elapsed_sec: 75,
            clip_usd: 20.0,
            max_load_usd: 200.0,
            maker_improve_ticks: 1.0,
            min_order_usd: 1.0,
            spot_filter_bps: 10.0,
            require_spot_match: true,
            regime_whipsaw_multiplier: 0.40,
            whipsaw_true_favorite_multiplier: 0.70,
            near_touch_min_favorite_ask: 0.85,
            near_touch_maker_improve_ticks: 3.0,
            regime_flat_multiplier: 0.70,
            regime_trending_volatile_multiplier: 0.75,
            regime_unknown_multiplier: 0.70,
            reversal_multiplier: 0.55,
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
    /// Deprecated live-tuning field kept for profile compatibility. Convex
    /// tail timing is intentionally exposure-gated, not time-gated: once the
    /// late-favorite sleeve is being loaded and the cheap leg is eligible, the
    /// tail can fire immediately.
    pub window_sec: u64,
    /// Optional bar-relative start fraction.
    pub start_frac: f64,
    pub clip_usd: f64,
    pub max_load_usd: f64,
    /// Target fraction of filled late-favorite loss-at-risk to offset if the
    /// favorite loses. This is payoff-aware: cheap tail share count is derived
    /// from `favorite_loss / (1 - tail_price)`, then converted back into
    /// allowed notional.
    pub max_favorite_exposure_fraction: f64,
    /// Hard upper bound as a fraction of late-favorite win-upside when the
    /// combined favorite+tail pair is not EV-positive. If the pair cost is
    /// positive-EV, this cap does not bind; the hedge is then useful paired
    /// inventory, not pure insurance drag.
    pub max_win_edge_spend_fraction: f64,
    /// Hard upper bound as a fraction of late-favorite spend. This keeps
    /// cheap-tail as a small dollar-budget insurance sleeve instead of a
    /// share-count hedge that competes with the late-favorite edge.
    pub max_late_fav_spend_fraction: f64,
    /// Ultra-cheap tail threshold where a small dollar budget buys materially
    /// different convexity than ordinary 5-10c tail.
    pub ultra_cheap_max_ask: f64,
    /// Minimum favorite ask required before the ultra-cheap tail budget can
    /// expand. This avoids sizing tail early when the book has not actually
    /// become high-cert/barbell.
    pub ultra_cheap_min_favorite_ask: f64,
    /// Expanded spend cap for ultra-cheap tail, still expressed as a fraction
    /// of late-favorite spend so dollar budget remains bounded.
    pub ultra_cheap_max_late_fav_spend_fraction: f64,
    pub maker_improve_ticks: f64,
    pub min_order_usd: f64,
    /// Optional hard cutoff after which this phase is disabled.
    pub disable_after_ms: Option<EpochMillis>,
}

impl Default for ConvexTailConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_cheap_ask: 0.10,
            window_sec: 60,
            start_frac: 0.0,
            clip_usd: 1.25,
            max_load_usd: 8.0,
            max_favorite_exposure_fraction: 0.55,
            max_win_edge_spend_fraction: 0.45,
            max_late_fav_spend_fraction: 0.025,
            ultra_cheap_max_ask: 0.03,
            ultra_cheap_min_favorite_ask: 0.90,
            ultra_cheap_max_late_fav_spend_fraction: 0.075,
            maker_improve_ticks: 1.0,
            min_order_usd: 1.0,
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
    /// Target fraction of filled late-favorite loss-at-risk to offset when
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
struct DirectionalConviction {
    score: f64,
    barbell: bool,
    btc_confirms: bool,
    regime: Option<BtcRegime>,
    path_reversal_risk: f64,
    favorite_ask: f64,
    cheap_ask: f64,
    recent_bps: f64,
    strongest_bps: f64,
    spot_vs_strike_bps: Option<f64>,
    model_favorite: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarketPosture {
    PairedCore,
    CenterOnly,
    BarbellDirectional,
    WhipsawHedge,
    NoFreshCore,
}

impl MarketPosture {
    fn suppresses_broad_paired_core(self) -> bool {
        matches!(
            self,
            Self::BarbellDirectional | Self::WhipsawHedge | Self::NoFreshCore
        )
    }

    fn merge(self, observed: Self) -> Self {
        use MarketPosture::*;
        match (self, observed) {
            (BarbellDirectional, _) | (_, BarbellDirectional) => BarbellDirectional,
            (NoFreshCore, _) | (_, NoFreshCore) => NoFreshCore,
            (WhipsawHedge, _) | (_, WhipsawHedge) => WhipsawHedge,
            (CenterOnly, _) | (_, CenterOnly) => CenterOnly,
            (PairedCore, PairedCore) => PairedCore,
        }
    }
}

impl DirectionalConviction {
    fn late_favorite_multiplier(self) -> f64 {
        if self.barbell && self.btc_confirms {
            (0.90 + 1.10 * self.score).clamp(0.60, 2.00)
        } else if self.barbell {
            (0.55 + 0.70 * self.score).clamp(0.35, 1.25)
        } else {
            (0.35 + 0.65 * self.score).clamp(0.25, 1.00)
        }
    }

    fn hedge_uncertainty_boost(self) -> f64 {
        if self.barbell && !self.btc_confirms {
            0.50
        } else {
            (1.0 - self.score).clamp(0.0, 0.50)
        }
    }
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
    market_postures: HashMap<MarketId, MarketPosture>,
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

#[derive(Clone, Copy, Debug, PartialEq)]
struct LateFavBundleState {
    dominant_fav_leg: LadderLeg,
    current_fav_leg: LadderLeg,
    tail_leg: LadderLeg,
    side_flip: bool,
    fav_filled_qty: f64,
    fav_filled_spend_usd: f64,
    tail_filled_qty: f64,
    tail_filled_spend_usd: f64,
    working_fav_spend_usd: f64,
    working_tail_spend_usd: f64,
}

#[derive(Clone, Debug, PartialEq)]
struct BundleOrderGate {
    allowed: bool,
    reason: String,
}

#[derive(Clone, Copy, Debug, Default)]
struct LateFavRearmState {
    last_favorite_leg: Option<LadderLeg>,
    stable_bars: u32,
    rearm_ready: bool,
    last_seen_ms: EpochMillis,
}

#[derive(Clone, Debug)]
pub struct LateFavoriteStrategy {
    config: LateFavoriteStrategyConfig,
    reserved_directional_notional: HashMap<String, ReservedNotional>,
    late_fav_rearm_state: HashMap<MarketId, LateFavRearmState>,
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
            late_fav_rearm_state: HashMap::new(),
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

    fn prune_rearm_state(&mut self, now_ms: EpochMillis) {
        self.late_fav_rearm_state
            .retain(|_, state| now_ms.saturating_sub(state.last_seen_ms) <= LATE_FAV_REARM_TTL_MS);
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

    fn update_late_fav_rearm_state(
        &mut self,
        market_id: &MarketId,
        favorite_leg: LadderLeg,
        path_reversal_risk: f64,
        now_ms: EpochMillis,
    ) -> (bool, u32) {
        let entry = self
            .late_fav_rearm_state
            .entry(market_id.clone())
            .or_insert(LateFavRearmState::default());
        entry.last_seen_ms = now_ms;

        if !path_reversal_risk.is_finite() || path_reversal_risk > LATE_FAV_REARM_MAX_PATH_RISK {
            entry.stable_bars = 0;
            entry.rearm_ready = false;
            entry.last_favorite_leg = Some(favorite_leg);
            return (false, entry.stable_bars);
        }

        match entry.last_favorite_leg {
            Some(previous) if previous == favorite_leg => {
                entry.stable_bars = entry.stable_bars.saturating_add(1);
            }
            _ => {
                entry.last_favorite_leg = Some(favorite_leg);
                entry.stable_bars = 1;
                entry.rearm_ready = false;
            }
        }

        entry.last_favorite_leg = Some(favorite_leg);
        entry.rearm_ready = entry.stable_bars >= LATE_FAV_REARM_STABLE_BARS;
        (entry.rearm_ready, entry.stable_bars)
    }

    fn bundle_state<M: MarketDescriptor>(
        &self,
        input: &StrategyInput<M>,
        legs: &LegQuotes,
    ) -> LateFavBundleState {
        let yes_fav_spend = directional_favorite_leg_spend_usd(self, input, LadderLeg::Yes);
        let no_fav_spend = directional_favorite_leg_spend_usd(self, input, LadderLeg::No);

        let dominant_fav_leg = if yes_fav_spend <= 1e-9 && no_fav_spend <= 1e-9 {
            legs.favorite_leg
        } else if yes_fav_spend >= no_fav_spend {
            LadderLeg::Yes
        } else {
            LadderLeg::No
        };
        let tail_leg = opposite_leg(dominant_fav_leg);
        let fav_filled_qty = inventory_qty_for_leg(input.late_fav_inventory, dominant_fav_leg)
            + stranded_paired_core_qty_for_leg(input.paired_core_inventory, dominant_fav_leg);
        let fav_filled_spend_usd =
            filled_inventory_spend_usd(input.late_fav_inventory, dominant_fav_leg)
                + stranded_paired_core_spend_usd(input.paired_core_inventory, dominant_fav_leg);
        let tail_filled_qty = inventory_qty_for_leg(input.cheap_tail_inventory, tail_leg)
            + stranded_paired_core_qty_for_leg(input.paired_core_inventory, tail_leg);
        let tail_filled_spend_usd =
            filled_inventory_spend_usd(input.cheap_tail_inventory, tail_leg)
                + stranded_paired_core_spend_usd(input.paired_core_inventory, tail_leg);
        let working_fav_spend_usd =
            directional_favorite_working_spend_usd(self, input, dominant_fav_leg);
        let working_tail_spend_usd = directional_tail_working_spend_usd(input, tail_leg);

        LateFavBundleState {
            dominant_fav_leg,
            current_fav_leg: legs.favorite_leg,
            tail_leg,
            side_flip: dominant_fav_leg != legs.favorite_leg,
            fav_filled_qty,
            fav_filled_spend_usd,
            tail_filled_qty,
            tail_filled_spend_usd,
            working_fav_spend_usd,
            working_tail_spend_usd,
        }
    }
}

fn directional_favorite_leg_spend_usd<M: MarketDescriptor>(
    strategy: &LateFavoriteStrategy,
    input: &StrategyInput<M>,
    leg: LadderLeg,
) -> f64 {
    filled_inventory_spend_usd(input.late_fav_inventory, leg)
        + stranded_paired_core_spend_usd(input.paired_core_inventory, leg)
        + directional_favorite_working_spend_usd(strategy, input, leg)
}

fn directional_favorite_working_spend_usd<M: MarketDescriptor>(
    strategy: &LateFavoriteStrategy,
    input: &StrategyInput<M>,
    leg: LadderLeg,
) -> f64 {
    open_order_notional_for_leg(input.open_late_fav_order_exposure, leg)
        + strategy.reserved_notional(input.market.market_id(), leg)
}

fn directional_tail_working_spend_usd<M: MarketDescriptor>(
    input: &StrategyInput<M>,
    leg: LadderLeg,
) -> f64 {
    open_order_notional_for_leg(input.open_convex_order_exposure, leg)
}

impl LateFavBundleState {
    fn fav_committed_spend_usd(self) -> f64 {
        self.fav_filled_spend_usd + self.working_fav_spend_usd
    }

    fn tail_coverage_ratio(self) -> f64 {
        let loss_at_risk = self.fav_committed_spend_usd();
        if loss_at_risk <= 0.0 {
            return 1.0;
        }
        (self.tail_filled_qty / loss_at_risk).clamp(0.0, 10.0)
    }

    fn side_flip_exposure_debt_usd(self) -> f64 {
        if self.side_flip {
            self.fav_filled_spend_usd + self.working_fav_spend_usd
        } else {
            0.0
        }
    }

    fn tail_coverage_ratio_after_favorite_add(self, proposed_fav_spend: f64) -> f64 {
        let loss_at_risk = self.fav_committed_spend_usd() + proposed_fav_spend.max(0.0);
        if loss_at_risk <= 0.0 {
            return 1.0;
        }
        (self.tail_filled_qty / loss_at_risk).clamp(0.0, 10.0)
    }

    fn payoff_if_fav_wins_after(self, proposed_fav_qty: f64, proposed_fav_spend: f64) -> f64 {
        self.fav_filled_qty + proposed_fav_qty
            - self.fav_filled_spend_usd
            - self.tail_filled_spend_usd
            - self.working_fav_spend_usd
            - self.working_tail_spend_usd
            - proposed_fav_spend
    }

    fn payoff_if_tail_wins(self) -> f64 {
        self.tail_filled_qty
            - self.fav_filled_spend_usd
            - self.tail_filled_spend_usd
            - self.working_fav_spend_usd
            - self.working_tail_spend_usd
    }

    fn payoff_if_tail_wins_after_favorite_add(self, proposed_fav_spend_usd: f64) -> f64 {
        self.payoff_if_tail_wins() - proposed_fav_spend_usd
    }

    fn gate_favorite_add(
        self,
        cfg: &ConvexTailConfig,
        proposed_leg: LadderLeg,
        proposed_price: f64,
        proposed_qty: f64,
        proposed_spend_usd: f64,
        regime: Option<BtcRegime>,
        path_reversal_risk: f64,
        late_fav_rearm_ready: bool,
    ) -> BundleOrderGate {
        if self.side_flip && proposed_leg != self.dominant_fav_leg && !late_fav_rearm_ready {
            return BundleOrderGate {
                allowed: false,
                reason: format!(
                    "bundle blocks side-flip favorite loading dominant={:?} current={:?} rearm_ready={} px={:.4} max_tail_px={:.4} fav_spend={:.2} tail_coverage={:.2}",
                    self.dominant_fav_leg,
                    self.current_fav_leg,
                    late_fav_rearm_ready,
                    proposed_price,
                    cfg.max_cheap_ask,
                    self.fav_committed_spend_usd(),
                    self.tail_coverage_ratio(),
                ),
            };
        }

        if self.side_flip && proposed_leg != self.dominant_fav_leg && proposed_price > cfg.ultra_cheap_max_ask {
            let tail_payoff_after = self.payoff_if_tail_wins_after_favorite_add(proposed_spend_usd);
            if tail_payoff_after < 0.0 {
                return BundleOrderGate {
                    allowed: false,
                    reason: format!(
                        "bundle blocks side-flip favorite add: tail-win payoff would be negative payoff={tail_payoff_after:.2} fav_spend={:.2} tail_spend={:.2} proposed_spend={proposed_spend_usd:.2}",
                        self.fav_committed_spend_usd(),
                        self.tail_filled_spend_usd,
                    ),
                };
            }
        }

        if proposed_leg == self.dominant_fav_leg {
            let payoff_if_fav_wins =
                self.payoff_if_fav_wins_after(proposed_qty, proposed_spend_usd);
            if payoff_if_fav_wins < -1e-9 {
                return BundleOrderGate {
                    allowed: false,
                    reason: format!(
                        "bundle blocks favorite add: favorite-win payoff would be negative payoff={payoff_if_fav_wins:.2} fav_committed={:.2} fav_filled={:.2} tail_spend={:.2} proposed_spend={proposed_spend_usd:.2}",
                        self.fav_committed_spend_usd(),
                        self.fav_filled_spend_usd,
                        self.tail_filled_spend_usd,
                    ),
                };
            }

            let coverage_target = bundle_tail_coverage_target(cfg, regime, path_reversal_risk);
            let reversal_prone = matches!(
                regime,
                Some(BtcRegime::Whipsaw | BtcRegime::TrendingVolatile)
            ) || path_reversal_risk >= 0.35;
            if reversal_prone
                && self.fav_filled_spend_usd >= cfg.clip_usd.max(cfg.min_order_usd)
                && self.tail_coverage_ratio_after_favorite_add(proposed_spend_usd)
                    < coverage_target * 0.50
            {
                return BundleOrderGate {
                    allowed: false,
                    reason: format!(
                        "bundle blocks favorite add: filled tail coverage {:.2} after proposed {:.2} below required {:.2} in reversal-prone regime={:?} path_reversal={:.2}",
                        self.tail_coverage_ratio(),
                        self.tail_coverage_ratio_after_favorite_add(proposed_spend_usd),
                        coverage_target,
                        regime,
                        path_reversal_risk,
                    ),
                };
            }
        }

        BundleOrderGate {
            allowed: true,
            reason: format!(
                "bundle allows favorite add fav_leg={:?} tail_leg={:?} side_flip={} fav_spend={:.2} tail_spend={:.2} tail_coverage={:.2}",
                self.dominant_fav_leg,
                self.tail_leg,
                self.side_flip,
                self.fav_committed_spend_usd(),
                self.tail_filled_spend_usd,
                self.tail_coverage_ratio(),
            ),
        }
    }
}

fn read_legs(snapshot: &PairedMarketSnapshot) -> Option<LegQuotes> {
    let yes_ask = snapshot.yes_quote.best_ask.as_ref()?.price;
    let no_ask = snapshot.no_quote.best_ask.as_ref()?.price;
    let yes_bid = snapshot
        .yes_quote
        .best_bid
        .as_ref()
        .map(|bid| bid.price)
        .unwrap_or(0.0);
    let no_bid = snapshot
        .no_quote
        .best_bid
        .as_ref()
        .map(|bid| bid.price)
        .unwrap_or(0.0);

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

fn opposite_leg(leg: LadderLeg) -> LadderLeg {
    match leg {
        LadderLeg::Yes => LadderLeg::No,
        LadderLeg::No => LadderLeg::Yes,
    }
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
    let coid = if aggressive_taker {
        // Aggressive FAK/IOC attempts are one-shot liquidity takes. Keep them
        // unique so a later attempt cannot be mistaken for the same venue
        // order after the previous one filled, killed, or no-matched.
        ClientOrderId::from(format!(
            "late-fav:{}:{}:{:?}:{}",
            tag,
            market.market_id(),
            leg,
            now_ms,
        ))
    } else {
        // Passive late-favorite probes need to rest. Do not include now_ms here:
        // otherwise every strategy tick creates a new desired id and the runtime
        // cancels the previous maker quote as a mixed-strategy reaction before it
        // has had a fair chance to fill.
        ClientOrderId::from(format!(
            "late-fav:{}:{}:{:?}:maker",
            tag,
            market.market_id(),
            leg,
        ))
    };
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
    aggressive_taker: bool,
    reason: String,
    now_ms: EpochMillis,
) -> OrderIntent {
    let instrument_id = match leg {
        LadderLeg::Yes => market.yes_instrument_id().clone(),
        LadderLeg::No => market.no_instrument_id().clone(),
    };
    let coid = if aggressive_taker {
        // FAK/IOC attempts are one-shot. Keep these unique so repeated
        // attempts after a no-match rejection are real fresh liquidity takes.
        ClientOrderId::from(format!(
            "cheap-tail:{}:{}:{:?}:{}",
            tag,
            market.market_id(),
            leg,
            now_ms,
        ))
    } else {
        // Passive cheap-tail fallback needs time to rest. Do not include
        // now_ms, otherwise each tick replaces the fallback quote before it
        // can get hit.
        ClientOrderId::from(format!(
            "cheap-tail:{}:{}:{:?}:maker",
            tag,
            market.market_id(),
            leg,
        ))
    };
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
        format!("cheap-tail-taker:{tag}")
    } else {
        format!("cheap-tail:{tag}")
    });
    intent
}

fn build_reversal_hedge_intent<M: MarketDescriptor>(
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
    intent.quote_level_tag = Some(if aggressive_taker {
        format!("reversal-hedge-taker:{tag}")
    } else {
        format!("cheap-tail:reversal-hedge:{tag}")
    });
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

fn effective_favorite_hedge_basis_qty(
    filled_late_fav_qty: f64,
    favorite_avg_price: f64,
    favorite_total_qty: f64,
    other_total_qty: f64,
    working_late_fav_usd: f64,
    reserved_late_fav_usd: f64,
) -> f64 {
    if favorite_avg_price <= 0.0 || !favorite_avg_price.is_finite() {
        return filled_late_fav_qty.max(0.0);
    }
    let filled_late_fav_usd = filled_late_fav_qty.max(0.0) * favorite_avg_price;
    let working_favorite_usd = working_late_fav_usd.max(reserved_late_fav_usd).max(0.0);
    let net_directional_usd = directional_exposure_usd(
        favorite_total_qty.max(0.0),
        other_total_qty.max(0.0),
        favorite_avg_price,
    );
    filled_late_fav_usd
        .max(working_favorite_usd)
        .max(net_directional_usd)
        / favorite_avg_price
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

fn inventory_avg_cost_for_leg(
    inventory: crate::market_making::pairing::types::PairedInventorySnapshot,
    leg: LadderLeg,
) -> f64 {
    match leg {
        LadderLeg::Yes => inventory.yes_avg_cost,
        LadderLeg::No => inventory.no_avg_cost,
    }
    .max(0.0)
}

fn filled_inventory_spend_usd(
    inventory: crate::market_making::pairing::types::PairedInventorySnapshot,
    leg: LadderLeg,
) -> f64 {
    inventory_qty_for_leg(inventory, leg) * inventory_avg_cost_for_leg(inventory, leg)
}

fn stranded_paired_core_qty_for_leg(
    inventory: crate::market_making::pairing::types::PairedInventorySnapshot,
    leg: LadderLeg,
) -> f64 {
    match leg {
        LadderLeg::Yes => (inventory.yes_qty - inventory.no_qty).max(0.0),
        LadderLeg::No => (inventory.no_qty - inventory.yes_qty).max(0.0),
    }
}

fn stranded_paired_core_spend_usd(
    inventory: crate::market_making::pairing::types::PairedInventorySnapshot,
    leg: LadderLeg,
) -> f64 {
    stranded_paired_core_qty_for_leg(inventory, leg) * inventory_avg_cost_for_leg(inventory, leg)
}

fn inventory_avg_cost_or(
    inventory: crate::market_making::pairing::types::PairedInventorySnapshot,
    leg: LadderLeg,
    fallback_price: f64,
) -> f64 {
    let avg_cost = inventory_avg_cost_for_leg(inventory, leg);
    if avg_cost > 0.0 {
        avg_cost
    } else {
        fallback_price
    }
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
    let price_scale = favorite_load_price_scale(
        favorite_ask,
        cfg.min_favorite_ask,
        cfg.taker_min_favorite_ask,
    );
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
    let lower_probe_upper_ask = cfg.min_favorite_ask
        + (cfg.near_touch_min_favorite_ask - cfg.min_favorite_ask) * (2.0 / 3.0);
    if legs.favorite_ask < lower_probe_upper_ask {
        multiplier *= 0.75;
    }
    multiplier.clamp(0.20, 1.0)
}

fn late_favorite_regime_multiplier(
    regime: &crate::signals::BtcRegimeSnapshot,
    favorite_leg: LadderLeg,
    cfg: &FavoriteClimbConfig,
    favorite_ask: f64,
) -> f64 {
    let base = match regime.regime() {
        Some(BtcRegime::DirectionalSmooth) => 1.0,
        Some(BtcRegime::TrendingVolatile) => cfg.regime_trending_volatile_multiplier,
        Some(BtcRegime::Whipsaw) if favorite_ask >= cfg.taker_min_favorite_ask => {
            cfg.whipsaw_true_favorite_multiplier
        }
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

fn favorite_load_price_scale(
    favorite_ask: f64,
    min_favorite_ask: f64,
    true_favorite_min_ask: f64,
) -> f64 {
    if favorite_ask >= true_favorite_min_ask {
        return 1.0;
    }
    let floor = min_favorite_ask.clamp(0.01, true_favorite_min_ask - 0.01);
    let span = (true_favorite_min_ask - floor).max(0.01);
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

fn directional_conviction<M: MarketDescriptor>(
    input: &StrategyInput<M>,
    legs: &LegQuotes,
    cfg: &FavoriteClimbConfig,
) -> DirectionalConviction {
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
    let spot_vs_strike = spot_vs_strike_bps(
        &input.market,
        input.btc_regime.last_price,
        legs.favorite_leg,
    );
    let model_favorite = favorite_probability(
        legs.favorite_leg,
        input.fair_value.p_up,
        input.fair_value.p_down,
    );
    let barbell = is_directional_barbell_favorite(cfg, legs.favorite_ask, legs.cheap_ask);
    let btc_confirms = strongest >= threshold * 0.75
        || spot_vs_strike
            .map(|m| m >= (threshold * 0.40).max(4.0))
            .unwrap_or(false);
    let book_score = if legs.favorite_ask >= cfg.taker_min_favorite_ask {
        1.0
    } else {
        ((legs.favorite_ask - cfg.near_touch_min_favorite_ask)
            / (cfg.taker_min_favorite_ask - cfg.near_touch_min_favorite_ask).max(0.01))
        .clamp(0.0, 1.0)
    };
    let cheap_score =
        ((cfg.max_favorite_ask - legs.cheap_ask) / cfg.max_favorite_ask.max(0.01)).clamp(0.0, 1.0);
    let spot_score = (strongest / (threshold * 1.50)).clamp(0.0, 1.0);
    let strike_score = spot_vs_strike
        .map(|m| (m / (threshold * 1.25)).clamp(0.0, 1.0))
        .unwrap_or(0.0);
    let model_score = ((model_favorite - legs.favorite_ask + 0.05) / 0.10).clamp(0.0, 1.0);
    let regime_score = match input.btc_regime.regime() {
        Some(BtcRegime::DirectionalSmooth) => 1.0,
        Some(BtcRegime::TrendingVolatile) => 0.80,
        Some(BtcRegime::Whipsaw) => 0.45,
        Some(BtcRegime::Flat) => 0.25,
        None => 0.50,
    };
    let path_reversal_risk = path_reversal_risk_score(input, legs);
    let reversal_scale = (1.0 - path_reversal_risk * 0.65).clamp(0.20, 1.0);
    let raw_score = 0.30 * book_score
        + 0.15 * cheap_score
        + 0.20 * spot_score
        + 0.15 * strike_score
        + 0.10 * model_score
        + 0.10 * regime_score;
    let score = (raw_score * reversal_scale).clamp(0.0, 1.0);

    DirectionalConviction {
        score,
        barbell,
        btc_confirms,
        regime: input.btc_regime.regime(),
        path_reversal_risk,
        favorite_ask: legs.favorite_ask,
        cheap_ask: legs.cheap_ask,
        recent_bps: recent,
        strongest_bps: strongest,
        spot_vs_strike_bps: spot_vs_strike,
        model_favorite,
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

fn favorite_load_levels(cfg: &FavoriteClimbConfig, favorite_ask: f64, remaining_ms: u64) -> usize {
    if favorite_ask >= cfg.taker_min_favorite_ask && remaining_ms <= 120_000 {
        5
    } else if favorite_ask >= cfg.taker_min_favorite_ask {
        4
    } else if favorite_ask >= 0.80 {
        3
    } else if favorite_ask >= 0.75 {
        2
    } else {
        1
    }
}

fn is_pre_standard_late_favorite_window(cfg: &FavoriteClimbConfig, remaining_ms: u64) -> bool {
    remaining_ms > cfg.taker_window_sec.saturating_mul(1_000)
}

fn is_directional_barbell_favorite(
    cfg: &FavoriteClimbConfig,
    favorite_ask: f64,
    cheap_ask: f64,
) -> bool {
    favorite_ask >= cfg.near_touch_min_favorite_ask
        && favorite_ask + cheap_ask <= 1.0 + 2.0 * BINARY_MARKET_TICK_SIZE + 1e-9
}

fn early_barbell_late_favorite_blocked(
    cfg: &FavoriteClimbConfig,
    whipsaw: bool,
    path_reversal_risk: f64,
    favorite_ask: f64,
    strongest: f64,
    threshold: f64,
    model_favorite: f64,
) -> bool {
    let whipsaw_reversal_limit = (1.0 - cfg.regime_whipsaw_multiplier).clamp(0.35, 0.75);
    if whipsaw
        && path_reversal_risk >= whipsaw_reversal_limit
        && favorite_ask < cfg.taker_min_favorite_ask + 3.0 * BINARY_MARKET_TICK_SIZE
    {
        return true;
    }

    strongest < threshold * cfg.regime_unknown_multiplier.clamp(0.35, 1.0)
        && model_favorite < cfg.taker_min_favorite_ask
}

fn late_favorite_timing_scale(
    cfg: &FavoriteClimbConfig,
    pre_standard_late_window: bool,
    early_late: bool,
) -> f64 {
    if pre_standard_late_window {
        cfg.regime_unknown_multiplier.clamp(0.35, 1.0)
    } else if early_late {
        cfg.regime_trending_volatile_multiplier.clamp(0.35, 1.0)
    } else {
        1.0
    }
}

fn late_favorite_max_levels(
    pre_standard_late_window: bool,
    early_late: bool,
    whipsaw: bool,
) -> usize {
    if pre_standard_late_window && whipsaw {
        1
    } else if pre_standard_late_window {
        2
    } else if early_late && whipsaw {
        2
    } else if early_late {
        3
    } else {
        5
    }
}

fn late_favorite_clip_ceiling_multiplier(
    conviction: DirectionalConviction,
    entry_policy: &FavoriteEntryPolicy,
) -> f64 {
    if !entry_policy.allow_taker {
        return 1.0;
    }
    if conviction.barbell && conviction.btc_confirms {
        2.0
    } else if conviction.barbell {
        1.5
    } else if conviction.favorite_ask >= 0.90 && conviction.score >= 0.70 {
        1.25
    } else {
        1.0
    }
}

fn late_favorite_high_cert_price_taper(favorite_ask: f64) -> f64 {
    if favorite_ask < 0.95 {
        return 1.0;
    }
    if favorite_ask >= 0.99 {
        return 0.18;
    }
    let progress = ((favorite_ask - 0.95) / 0.04).clamp(0.0, 1.0);
    1.0 - progress * 0.82
}

fn late_favorite_high_cert_max_levels(favorite_ask: f64, base_levels: usize) -> usize {
    if favorite_ask >= 0.99 {
        base_levels.min(1)
    } else if favorite_ask >= 0.97 {
        base_levels.min(2)
    } else if favorite_ask >= 0.95 {
        base_levels.min(3)
    } else {
        base_levels
    }
}

fn favorite_entry_policy<M: MarketDescriptor>(
    input: &StrategyInput<M>,
    legs: &LegQuotes,
    cfg: &FavoriteClimbConfig,
    elapsed_ms: u64,
    remaining_ms: u64,
) -> Option<FavoriteEntryPolicy> {
    let elapsed_sec = elapsed_ms / 1_000;
    if legs.favorite_ask < cfg.min_favorite_ask || legs.favorite_ask > cfg.max_favorite_ask {
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
    let conviction = directional_conviction(input, legs, cfg);
    let pre_standard_late_window = is_pre_standard_late_favorite_window(cfg, remaining_ms);
    let directional_barbell = conviction.barbell;

    if pre_standard_late_window && !directional_barbell {
        return None;
    }
    if pre_standard_late_window && directional_barbell && conviction.score < 0.35 {
        return None;
    }

    if recent < -threshold * 0.35 {
        return None;
    }
    if legs.favorite_ask < cfg.taker_min_favorite_ask && path_reversal_risk >= 0.50 {
        return None;
    }
    if early_late
        && path_reversal_risk >= 0.75
        && !(directional_barbell && legs.favorite_ask >= cfg.taker_min_favorite_ask)
    {
        return None;
    }
    if pre_standard_late_window
        && early_barbell_late_favorite_blocked(
            cfg,
            whipsaw,
            path_reversal_risk,
            legs.favorite_ask,
            strongest,
            threshold,
            model_favorite,
        )
    {
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
            min_price: cfg.min_favorite_ask,
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

    if legs.favorite_ask < cfg.taker_min_favorite_ask {
        let clean_directional_persistence = !whipsaw
            && path_reversal_risk <= 0.20
            && strongest >= threshold * 1.50
            && recent >= threshold * 0.25
            && model_favorite >= 0.88;
        let directional_probe_below_near_touch = directional_barbell
            && legs.favorite_ask >= cfg.min_favorite_ask
            && legs.favorite_ask < cfg.near_touch_min_favorite_ask
            && clean_directional_persistence;
        let barbell_sub90 = directional_barbell
            && ((legs.favorite_ask >= cfg.near_touch_min_favorite_ask && conviction.score >= 0.35)
                || directional_probe_below_near_touch);
        if early_late
            && !barbell_sub90
            && if flat_regime {
                strongest < threshold * 1.15 || model_favorite < 0.86
            } else {
                !clean_regime || strongest < threshold || model_favorite < 0.82
            }
        {
            return None;
        }
        if whipsaw
            && if barbell_sub90 {
                model_favorite < 0.86 || strongest < threshold * 0.75
            } else {
                model_favorite < 0.88 || strongest < threshold * 1.50
            }
        {
            return None;
        }
        let reversal_scale = (1.0 - 0.50 * path_reversal_risk).clamp(0.50, 1.0);
        let whipsaw_scale = if whipsaw && barbell_sub90 {
            0.75
        } else if whipsaw {
            0.50
        } else {
            1.0
        };
        let flat_scale = if flat_regime && early_late { 0.80 } else { 1.0 };
        let near_touch_maker = legs.favorite_ask >= cfg.min_favorite_ask
            && path_reversal_risk <= 0.40
            && (directional_probe_below_near_touch
                || (legs.favorite_ask >= cfg.near_touch_min_favorite_ask
                    && (barbell_sub90
                        || clean_directional_persistence
                        || strongest >= threshold * 1.15)));
        let base_scale = if barbell_sub90 {
            if early_late {
                0.75
            } else {
                1.0
            }
        } else if early_late {
            0.45
        } else {
            0.75
        };
        let conviction_scale = if barbell_sub90 {
            (0.90 + 0.80 * conviction.score).clamp(0.90, 1.60)
        } else {
            1.0
        };
        let probe_scale = if directional_probe_below_near_touch {
            0.50
        } else {
            1.0
        };
        return Some(FavoriteEntryPolicy {
            min_price: if directional_probe_below_near_touch {
                cfg.min_favorite_ask
            } else {
                cfg.near_touch_min_favorite_ask
            },
            max_levels: if directional_probe_below_near_touch {
                2
            } else if barbell_sub90 && !whipsaw {
                4
            } else if barbell_sub90 {
                3
            } else if whipsaw {
                1
            } else if early_late {
                2
            } else {
                3
            },
            clip_multiplier: base_scale
                * conviction_scale
                * reversal_scale
                * whipsaw_scale
                * flat_scale
                * probe_scale,
            cap_multiplier: base_scale
                * conviction_scale
                * reversal_scale
                * whipsaw_scale
                * flat_scale
                * probe_scale,
            allow_taker: (clean_directional_persistence || barbell_sub90)
                && legs.favorite_ask >= cfg.near_touch_min_favorite_ask
                && (elapsed_sec >= 180 || barbell_sub90),
            near_touch_maker,
            path_reversal_risk,
            label: if directional_probe_below_near_touch {
                "maker_probe_below_near_touch_directional_barbell"
            } else if clean_directional_persistence
                && legs.favorite_ask >= cfg.near_touch_min_favorite_ask
            {
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

    let reversal_scale = (1.0 - 0.55 * path_reversal_risk).clamp(0.35, 1.0);
    let timing_scale = late_favorite_timing_scale(cfg, pre_standard_late_window, early_late);
    let conviction_scale = conviction.late_favorite_multiplier();
    let high_cert_taper = late_favorite_high_cert_price_taper(legs.favorite_ask);
    let max_levels = late_favorite_high_cert_max_levels(
        legs.favorite_ask,
        late_favorite_max_levels(pre_standard_late_window, early_late, whipsaw),
    );
    Some(FavoriteEntryPolicy {
        min_price: if early_late {
            cfg.taker_min_favorite_ask
        } else {
            cfg.near_touch_min_favorite_ask
        },
        max_levels,
        clip_multiplier: timing_scale * reversal_scale * conviction_scale * high_cert_taper,
        cap_multiplier: timing_scale * reversal_scale * conviction_scale * high_cert_taper,
        allow_taker: true,
        near_touch_maker: true,
        path_reversal_risk,
        label: if pre_standard_late_window && whipsaw {
            "true_late_fav_90_plus_barbell_whipsaw_early"
        } else if pre_standard_late_window {
            "true_late_fav_90_plus_barbell_early"
        } else if early_late && whipsaw {
            "true_late_fav_90_plus_whipsaw_early"
        } else if early_late {
            "true_late_fav_90_plus_early"
        } else {
            "true_late_fav_90_plus"
        },
    })
}

fn should_override_favorite_signal_for_barbell(
    legs: &LegQuotes,
    climb_cfg: &FavoriteClimbConfig,
    tail_cfg: &ConvexTailConfig,
    conviction: DirectionalConviction,
) -> bool {
    conviction.barbell
        && legs.favorite_ask >= climb_cfg.taker_min_favorite_ask
        && legs.cheap_ask <= tail_cfg.max_cheap_ask
        && conviction.model_favorite >= climb_cfg.near_touch_min_favorite_ask
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
    favorite_avg_price: f64,
    favorite_ask: f64,
    cheap_ask: f64,
    regime: Option<BtcRegime>,
    path_reversal_risk: f64,
    directional_uncertainty_boost: f64,
    late_fav_filled_qty: f64,
    favorite_avg_filled_price: f64,
) -> f64 {
    if late_fav_qty <= 0.0
        || favorite_avg_price <= 0.0
        || favorite_avg_price >= 1.0
        || cheap_ask <= 0.0
        || cheap_ask >= 1.0
    {
        return 0.0;
    }
    let favorite_loss_at_risk = late_fav_qty * favorite_avg_price;
    // Favorability for tail spend should be constrained by realized upside
    // whenever possible, but if only working/in-flight favorite exposure is
    // present we still apply upside-aware limits to avoid overloading the
    // sleeve while it is not yet filled.
    let realized_favorite_win_upside = if late_fav_filled_qty > 0.0
        && favorite_avg_filled_price > 0.0
        && favorite_avg_filled_price < 1.0
    {
        late_fav_filled_qty * (1.0 - favorite_avg_filled_price)
    } else {
        0.0
    };
    let expected_favorite_win_upside = if late_fav_qty > 0.0
        && favorite_avg_price > 0.0
        && favorite_avg_price < 1.0
    {
        late_fav_qty * (1.0 - favorite_avg_price)
    } else {
        0.0
    };
    let favorite_win_upside = realized_favorite_win_upside.max(expected_favorite_win_upside);
    let coverage_fraction = (cheap_tail_coverage_fraction(cfg, regime)
        * (1.0
            + path_reversal_risk.clamp(0.0, 1.0)
            + directional_uncertainty_boost.clamp(0.0, 0.75)))
    .clamp(0.0, 1.0);
    let target_tail_shares =
        (favorite_loss_at_risk * coverage_fraction) / (1.0 - cheap_ask).max(0.01);
    let hedge_notional = target_tail_shares * cheap_ask;
    // Cheap-tail protects the favorite sleeve, but it must not convert the
    // bundle into a position that loses when the favorite wins. Cap tail spend
    // by a configured fraction of the favorite-side remaining upside in every
    // regime; hard-reversal sizing can increase desired coverage, not erase the
    // favorite payoff.
    let fractional_edge_cap = favorite_win_upside * cfg.max_win_edge_spend_fraction.max(0.0);
    let min_positive_payoff_tail_cap = if favorite_win_upside >= cfg.min_order_usd {
        cfg.min_order_usd
    } else {
        0.0
    };
    let edge_erosion_cap = fractional_edge_cap.max(min_positive_payoff_tail_cap);
    let high_cert_favorite = favorite_ask >= cfg.ultra_cheap_min_favorite_ask
        || favorite_avg_price >= cfg.ultra_cheap_min_favorite_ask;
    let late_fav_spend_fraction = if cheap_ask <= cfg.ultra_cheap_max_ask && high_cert_favorite {
        cfg.ultra_cheap_max_late_fav_spend_fraction
    } else {
        cfg.max_late_fav_spend_fraction
    };
    let fractional_late_fav_budget_cap =
        favorite_loss_at_risk * late_fav_spend_fraction.max(0.0);
    let late_fav_budget_cap = fractional_late_fav_budget_cap.max(min_positive_payoff_tail_cap);

    let desired_tail_notional =
        if hedge_notional > 0.0 && min_positive_payoff_tail_cap >= cfg.min_order_usd {
            hedge_notional.max(cfg.min_order_usd)
        } else {
            hedge_notional
        };

    cfg.max_load_usd
        .min(desired_tail_notional)
        .min(edge_erosion_cap)
        .min(late_fav_budget_cap)
}

fn unbundled_ultra_cheap_tail_cap_usd(
    cfg: &ConvexTailConfig,
    late_fav_filled_qty: f64,
    favorite_ask: f64,
    cheap_ask: f64,
) -> f64 {
    // Gate on *filled* late-fav qty, not working/reserved. We now require a
    // confirmed late-favorite anchor before firing the standalone lottery lane;
    // without a filled anchor, this lane becomes pure speculation and can
    // materially over-index on cheap tails.
    if late_fav_filled_qty <= 0.0
        || cheap_ask <= 0.0
        || cheap_ask > cfg.ultra_cheap_max_ask
        || favorite_ask < cfg.ultra_cheap_min_favorite_ask
    {
        return 0.0;
    }

    let cap = cfg.clip_usd.min(cfg.max_load_usd);
    if cap >= cfg.min_order_usd {
        cap
    } else {
        0.0
    }
}

fn reversal_hedge_cap_usd(
    cfg: &ReversalHedgeConfig,
    late_fav_qty: f64,
    favorite_avg_price: f64,
    hedge_ask: f64,
    reversal_score: f64,
) -> f64 {
    if late_fav_qty <= 0.0
        || favorite_avg_price <= 0.0
        || favorite_avg_price >= 1.0
        || hedge_ask <= 0.0
        || hedge_ask >= 1.0
        || reversal_score <= 0.0
    {
        return 0.0;
    }
    let favorite_loss_at_risk = late_fav_qty * favorite_avg_price;
    let favorite_win_upside = late_fav_qty * (1.0 - favorite_avg_price);
    let coverage_fraction = (cfg.max_favorite_exposure_fraction.max(0.0)
        * reversal_score.clamp(0.0, 1.0))
    .clamp(0.0, 1.0);
    let target_hedge_shares =
        (favorite_loss_at_risk * coverage_fraction) / (1.0 - hedge_ask).max(0.01);
    let hedge_notional = target_hedge_shares * hedge_ask;
    let pair_cost_is_positive_ev = favorite_avg_price + hedge_ask <= 1.0 + 1e-9;
    let edge_erosion_cap = if pair_cost_is_positive_ev {
        cfg.max_load_usd
    } else {
        favorite_win_upside * cfg.max_win_edge_spend_fraction.max(0.0)
    };

    cfg.max_load_usd.min(hedge_notional).min(edge_erosion_cap)
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

fn bundle_tail_coverage_target(
    cfg: &ConvexTailConfig,
    regime: Option<BtcRegime>,
    path_reversal_risk: f64,
) -> f64 {
    (cheap_tail_coverage_fraction(cfg, regime) * (1.0 + path_reversal_risk.clamp(0.0, 0.75)))
        .clamp(0.20, 1.0)
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

fn cheap_tail_ladder_load_usd(
    cfg: &ConvexTailConfig,
    favorite_ask: f64,
    cheap_ask: f64,
    remaining_load_usd: f64,
) -> f64 {
    if remaining_load_usd < cfg.min_order_usd {
        return 0.0;
    }

    let clip_multiple = if cheap_ask <= cfg.ultra_cheap_max_ask
        && favorite_ask >= cfg.ultra_cheap_min_favorite_ask
    {
        6.0
    } else if cheap_ask <= cfg.max_cheap_ask {
        3.0
    } else {
        1.0
    };

    remaining_load_usd
        .min(cfg.max_load_usd)
        .min(cfg.clip_usd * clip_multiple)
        .max(cfg.min_order_usd)
}

fn cheap_tail_ladder_step_ticks(cfg: &ConvexTailConfig, cheap_ask: f64) -> f64 {
    if cheap_ask <= cfg.ultra_cheap_max_ask {
        1.0
    } else if cheap_ask <= cfg.max_cheap_ask {
        2.0
    } else {
        1.0
    }
}

fn should_use_aggressive_cheap_tail(
    cfg: &ConvexTailConfig,
    favorite_ask: f64,
    favorite_avg_price: f64,
    cheap_ask: f64,
    regime: Option<BtcRegime>,
    path_reversal_risk: f64,
) -> bool {
    if cheap_ask <= 0.0 || cheap_ask > cfg.max_cheap_ask {
        return false;
    }
    // Cheap and ultra-cheap convex tails are always taker. Maker rungs at 1-7c
    // sit unfilled because the resting book at those prices is thin and the
    // touch moves on us before queue position pays off. We want the "tiny
    // dollars, huge shares" Bonereaper shape, which only materializes when
    // we actually cross the spread.
    if cheap_ask <= cfg.max_cheap_ask {
        return true;
    }
    let pair_cost_is_positive_ev =
        favorite_avg_price > 0.0 && favorite_avg_price + cheap_ask <= 1.0 + 1e-9;
    let high_cert_favorite = favorite_ask >= cfg.ultra_cheap_min_favorite_ask
        || favorite_avg_price >= cfg.ultra_cheap_min_favorite_ask;
    if high_cert_favorite && cheap_ask <= cfg.ultra_cheap_max_ask {
        return true;
    }
    pair_cost_is_positive_ev
        || matches!(
            regime,
            Some(BtcRegime::Whipsaw | BtcRegime::TrendingVolatile)
        )
        || path_reversal_risk >= cfg.max_favorite_exposure_fraction.clamp(0.25, 0.75)
}

fn should_use_aggressive_reversal_hedge(
    cfg: &ReversalHedgeConfig,
    favorite_avg_price: f64,
    hedge_ask: f64,
    reversal_score: f64,
) -> bool {
    let pair_cost_is_positive_ev =
        favorite_avg_price > 0.0 && favorite_avg_price + hedge_ask <= 1.0 + 1e-9;
    pair_cost_is_positive_ev || reversal_score >= (cfg.min_reversal_score + 0.15).min(0.95)
}

fn is_repair_like_paired_core_intent(intent: &OrderIntent) -> bool {
    intent.reason.contains("repair")
        || intent
            .quote_level_tag
            .as_deref()
            .is_some_and(|tag| tag.contains("repair"))
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
        self.prune_rearm_state(input.now_ms);
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
        let directional_conviction = directional_conviction(&input, &legs, &climb_cfg);
        let (late_fav_rearm_ready, late_fav_stable_bars) = self.update_late_fav_rearm_state(
            input.market.market_id(),
            legs.favorite_leg,
            directional_conviction.path_reversal_risk,
            input.now_ms,
        );
        notes.push(format!(
            "directional_conviction score={:.2} barbell={} btc_confirms={} regime={:?} favorite_ask={:.4} cheap_ask={:.4} recent_bps={:.2} strongest_bps={:.2} spot_vs_strike_bps={:?} model_favorite={:.4} path_reversal_risk={:.2}",
            directional_conviction.score,
            directional_conviction.barbell,
            directional_conviction.btc_confirms,
            directional_conviction.regime,
            directional_conviction.favorite_ask,
            directional_conviction.cheap_ask,
            directional_conviction.recent_bps,
            directional_conviction.strongest_bps,
            directional_conviction.spot_vs_strike_bps,
            directional_conviction.model_favorite,
            directional_conviction.path_reversal_risk,
        ));
        notes.push(format!(
            "late_fav_rearm ready={} stable_bars={} max_path_risk={:.2}",
            late_fav_rearm_ready,
            late_fav_stable_bars,
            LATE_FAV_REARM_MAX_PATH_RISK,
        ));
        let bundle_state = self.bundle_state(&input, &legs);
        notes.push(format!(
            "late_fav_bundle dominant={:?} current={:?} tail={:?} side_flip={} fav_committed={:.2} fav_filled={:.2} fav_qty={:.2} tail_spend={:.2} tail_qty={:.2} working_fav={:.2} working_tail={:.2} tail_coverage={:.2} fav_win_payoff_now={:.2} tail_win_payoff_now={:.2}",
            bundle_state.dominant_fav_leg,
            bundle_state.current_fav_leg,
            bundle_state.tail_leg,
            bundle_state.side_flip,
            bundle_state.fav_committed_spend_usd(),
            bundle_state.fav_filled_spend_usd,
            bundle_state.fav_filled_qty,
            bundle_state.tail_filled_spend_usd,
            bundle_state.tail_filled_qty,
            bundle_state.working_fav_spend_usd,
            bundle_state.working_tail_spend_usd,
            bundle_state.tail_coverage_ratio(),
            bundle_state.payoff_if_fav_wins_after(0.0, 0.0),
            bundle_state.payoff_if_tail_wins(),
        ));
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
        let regime_multiplier = late_favorite_regime_multiplier(
            &input.btc_regime,
            legs.favorite_leg,
            &climb_cfg,
            legs.favorite_ask,
        );
        if climb_cfg.enabled
            && climb_enabled
            && remaining_ms <= climb_window_ms
            && elapsed_ms >= climb_cfg.min_elapsed_sec.saturating_mul(1_000)
            && legs.favorite_ask >= climb_cfg.min_favorite_ask
            && legs.favorite_ask <= climb_cfg.max_favorite_ask
        {
            let (direction_ok, signal_note) = favorite_direction_signal(&input, &legs, &climb_cfg);

            let market_structure_override = should_override_favorite_signal_for_barbell(
                &legs,
                &climb_cfg,
                &tail_cfg,
                directional_conviction,
            );
            let hard_barbell_favorite_entry = market_structure_override
                && legs.favorite_ask >= climb_cfg.taker_min_favorite_ask
                && is_directional_barbell_favorite(&climb_cfg, legs.favorite_ask, legs.cheap_ask);
            let policy_elapsed_ms = if hard_barbell_favorite_entry {
                elapsed_ms.max(climb_cfg.min_elapsed_sec.saturating_mul(1_000))
            } else {
                elapsed_ms
            };
            let policy_remaining_ms = if hard_barbell_favorite_entry {
                remaining_ms.min(climb_cfg.taker_window_sec.saturating_mul(1_000))
            } else {
                remaining_ms
            };

            let entry_policy = if !direction_ok && !market_structure_override {
                notes.push(format!(
                    "late_favorite blocked by favorite signal {signal_note}",
                ));
                None
            } else if let Some(entry_policy) = favorite_entry_policy(
                &input,
                &legs,
                &climb_cfg,
                policy_elapsed_ms,
                policy_remaining_ms,
            ) {
                if market_structure_override && !direction_ok {
                    notes.push(format!(
                        "late_favorite using barbell market-structure override despite signal miss {signal_note}",
                    ));
                } else {
                    notes.push(signal_note);
                }
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
                    "late_favorite blocked by entry tier ask={:.4} elapsed_ms={} policy_elapsed_ms={} regime={:?}",
                    legs.favorite_ask,
                    elapsed_ms,
                    policy_elapsed_ms,
                    input.btc_regime.regime()
                ));
                None
            };
            if let Some(entry_policy) = entry_policy {
                let favorite_qty =
                    inventory_qty_for_leg(input.late_fav_inventory, legs.favorite_leg);
                let other_qty = inventory_qty_for_leg(input.cheap_tail_inventory, legs.cheap_leg);
                let net_directional_exposure_usd =
                    directional_exposure_usd(favorite_qty, other_qty, legs.favorite_ask);
                let side_flip_debt_usd =
                    if bundle_state.current_fav_leg != bundle_state.dominant_fav_leg {
                        bundle_state.side_flip_exposure_debt_usd()
                    } else {
                        0.0
                    };
                let current_exposure_usd =
                    directional_favorite_leg_spend_usd(self, &input, legs.favorite_leg)
                        .max(net_directional_exposure_usd)
                        + side_flip_debt_usd;
                if side_flip_debt_usd > 0.0 {
                    notes.push(format!(
                        "late_favorite side_flip_debt_usd {:.2} added to budget for re-arm control",
                        side_flip_debt_usd
                    ));
                }
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
                        let sizing_remaining_ms = if hard_barbell_favorite_entry {
                            policy_remaining_ms
                        } else {
                            remaining_ms
                        };
                        let raw_clip = reactive_climb_clip_usd(
                            &climb_cfg,
                            legs.favorite_ask,
                            elapsed_ms,
                            sizing_remaining_ms,
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
                            && legs.favorite_ask >= climb_cfg.near_touch_min_favorite_ask
                            && legs.favorite_ask < climb_cfg.taker_min_favorite_ask
                            && (remaining_ms <= climb_cfg.taker_window_sec.saturating_mul(1_000)
                                || is_directional_barbell_favorite(
                                    &climb_cfg,
                                    legs.favorite_ask,
                                    legs.cheap_ask,
                                ));
                        let high_cert_favorite =
                            legs.favorite_ask >= climb_cfg.taker_min_favorite_ask;
                        let use_aggressive_taker = entry_policy.allow_taker
                            && (high_cert_favorite
                                || should_use_aggressive_favorite_taker(
                                    &climb_cfg,
                                    legs.favorite_ask,
                                    sizing_remaining_ms,
                                )
                                || use_sub90_fak
                                || directional_conviction.barbell);
                        let level_count = (if use_aggressive_taker {
                            favorite_load_levels(&climb_cfg, legs.favorite_ask, sizing_remaining_ms)
                        } else if legs.favorite_ask < climb_cfg.taker_min_favorite_ask {
                            favorite_load_levels(&climb_cfg, legs.favorite_ask, sizing_remaining_ms)
                                .max(3)
                        } else {
                            favorite_load_levels(&climb_cfg, legs.favorite_ask, sizing_remaining_ms)
                        })
                        .min(entry_policy.max_levels);
                        // Late-favorite is an active payoff sleeve, not passive
                        // inventory discovery. Once the entry policy has opted
                        // into taker execution, do not leave lower passive
                        // maker rungs behind: those create tiny scrap fills
                        // when the real trade was to consume available
                        // high-cert liquidity now.
                        let aggressive_all_levels = use_aggressive_taker;
                        let clip_ceiling = climb_cfg.clip_usd
                            * late_favorite_clip_ceiling_multiplier(
                                directional_conviction,
                                &entry_policy,
                            )
                            * late_favorite_high_cert_price_taper(legs.favorite_ask);
                        for level in 0..level_count {
                            if load_left < climb_cfg.min_order_usd {
                                break;
                            }
                            let aggressive_taker =
                                use_aggressive_taker && (level == 0 || aggressive_all_levels);
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
                                .min(clip_ceiling)
                                .min(load_left)
                                .max(climb_cfg.min_order_usd);
                            let qty = (clip / px).max(input.market.min_order_size());
                            let bundle_gate = bundle_state.gate_favorite_add(
                                &tail_cfg,
                                legs.favorite_leg,
                                px,
                                qty,
                                clip,
                                input.btc_regime.regime(),
                                entry_policy.path_reversal_risk,
                                late_fav_rearm_ready,
                            );
                            if !bundle_gate.allowed {
                                notes.push(bundle_gate.reason);
                                break;
                            }
                            let reason = format!(
                                "late_favorite climb leg={:?} level={} mode={} px={:.4} ask={:.4} entry_policy={} price_scale={:.2} confidence_multiplier={:.2} regime_multiplier={:.2} clip_usd={:.2} cumulative={:.2}/{:.2} elapsed_ms={elapsed_ms} remaining_ms={remaining_ms}; {}",
                                legs.favorite_leg,
                                level,
                                if aggressive_taker { "taker_fak" } else { "maker_post_only" },
                                px,
                                legs.favorite_ask,
                                entry_policy.label,
                                favorite_load_price_scale(
                                    legs.favorite_ask,
                                    climb_cfg.min_favorite_ask,
                                    climb_cfg.taker_min_favorite_ask,
                                ),
                                confidence_multiplier,
                                regime_multiplier,
                                clip,
                                current_exposure_usd + (remaining_load - load_left),
                                adjusted_max_load_usd,
                                bundle_gate.reason,
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

        if tail_cfg.enabled && tail_enabled && legs.cheap_ask <= tail_cfg.max_cheap_ask {
            let late_fav_filled_qty =
                inventory_qty_for_leg(input.late_fav_inventory, legs.favorite_leg)
                    + stranded_paired_core_qty_for_leg(
                        input.paired_core_inventory,
                        legs.favorite_leg,
                    );
            let favorite_filled_spend_usd =
                filled_inventory_spend_usd(input.late_fav_inventory, legs.favorite_leg)
                    + stranded_paired_core_spend_usd(
                        input.paired_core_inventory,
                        legs.favorite_leg,
                    );
            let favorite_avg_price = if late_fav_filled_qty > 0.0 {
                favorite_filled_spend_usd / late_fav_filled_qty
            } else {
                inventory_avg_cost_or(
                    input.late_fav_inventory,
                    legs.favorite_leg,
                    legs.favorite_ask,
                )
            };
            let favorite_total_qty = late_fav_filled_qty;
            let other_total_qty = inventory_qty_for_leg(input.cheap_tail_inventory, legs.cheap_leg)
                + stranded_paired_core_qty_for_leg(input.paired_core_inventory, legs.cheap_leg);
            let working_late_fav_usd =
                open_order_notional_for_leg(input.open_late_fav_order_exposure, legs.favorite_leg);
            let reserved_late_fav_usd =
                self.reserved_notional(input.market.market_id(), legs.favorite_leg);
            let effective_late_fav_qty = effective_favorite_hedge_basis_qty(
                late_fav_filled_qty,
                favorite_avg_price,
                favorite_total_qty,
                other_total_qty,
                working_late_fav_usd,
                reserved_late_fav_usd,
            );
            let favorite_exposure_usd = late_fav_filled_qty * favorite_avg_price;
            let cheap_tail_filled_qty = other_total_qty;
            let cheap_tail_filled_spend_usd =
                filled_inventory_spend_usd(input.cheap_tail_inventory, legs.cheap_leg)
                    + stranded_paired_core_spend_usd(input.paired_core_inventory, legs.cheap_leg);
            let cheap_tail_avg_price = if cheap_tail_filled_qty > 0.0 {
                cheap_tail_filled_spend_usd / cheap_tail_filled_qty
            } else {
                inventory_avg_cost_or(input.cheap_tail_inventory, legs.cheap_leg, legs.cheap_ask)
            };
            let current_exposure_usd = (cheap_tail_filled_qty * cheap_tail_avg_price)
                + directional_tail_working_spend_usd(&input, legs.cheap_leg);
            let path_reversal_risk = path_reversal_risk_score(&input, &legs);
            let favorite_avg_filled_price = if late_fav_filled_qty > 0.0 {
                favorite_filled_spend_usd / late_fav_filled_qty
            } else {
                0.0
            };
            let bundled_tail_cap_usd = cheap_tail_cap_usd(
                &tail_cfg,
                late_fav_filled_qty,
                favorite_avg_price,
                legs.favorite_ask,
                legs.cheap_ask,
                input.btc_regime.regime(),
                path_reversal_risk,
                directional_conviction.hedge_uncertainty_boost(),
                late_fav_filled_qty,
                favorite_avg_filled_price,
            );
            let unbundled_tail_cap_usd = unbundled_ultra_cheap_tail_cap_usd(
                &tail_cfg,
                late_fav_filled_qty,
                legs.favorite_ask,
                legs.cheap_ask,
            );
            let tail_cap_usd = bundled_tail_cap_usd.max(unbundled_tail_cap_usd);
            let remaining_load = (tail_cap_usd - current_exposure_usd).max(0.0);
            if remaining_load >= tail_cfg.min_order_usd {
                if let Some(base_px) = maker_limit_price(
                    legs.cheap_bid,
                    legs.cheap_ask,
                    tick,
                    tail_cfg.maker_improve_ticks,
                ) {
                    let total_clip = cheap_tail_ladder_load_usd(
                        &tail_cfg,
                        legs.favorite_ask,
                        legs.cheap_ask,
                        remaining_load,
                    );
                    let level_count = cheap_tail_ladder_levels(
                        legs.cheap_ask,
                        total_clip,
                        tail_cfg.min_order_usd,
                    );
                    let mut load_left = total_clip;
                    let coverage_target = bundle_tail_coverage_target(
                        &tail_cfg,
                        input.btc_regime.regime(),
                        path_reversal_risk,
                    );
                    let coverage_deficit_forces_taker = bundle_state.fav_committed_spend_usd()
                        >= tail_cfg.max_load_usd.min(30.0)
                        && bundle_state.tail_coverage_ratio() < coverage_target
                        && legs.cheap_ask <= tail_cfg.max_cheap_ask
                        && (legs.cheap_ask <= tail_cfg.ultra_cheap_max_ask
                            || favorite_avg_price + legs.cheap_ask <= 1.0 + 1e-9);
                    let use_aggressive_taker = coverage_deficit_forces_taker
                        || should_use_aggressive_cheap_tail(
                            &tail_cfg,
                            legs.favorite_ask,
                            favorite_avg_price,
                            legs.cheap_ask,
                            input.btc_regime.regime(),
                            path_reversal_risk,
                        );
                    // Bonereaper's "tiny dollars, huge shares" shape needs
                    // ultra-cheap exposure to exist, not just a no-match FAK.
                    // Use the front level as FAK, then leave remaining
                    // ultra-cheap levels resting at touch as maker fallback.
                    // If visible liquidity exists, the FAK can take it. If the
                    // venue says no orders match, the fallback can still rest.
                    let use_ultra_cheap_maker_fallback = use_aggressive_taker
                        && legs.cheap_ask <= tail_cfg.ultra_cheap_max_ask;
                    let aggressive_all_levels =
                        use_aggressive_taker && !use_ultra_cheap_maker_fallback;
                    let ladder_step_ticks = cheap_tail_ladder_step_ticks(&tail_cfg, legs.cheap_ask);
                    for level in 0..level_count {
                        if load_left < tail_cfg.min_order_usd {
                            break;
                        }
                        let remaining_levels = (level_count - level).max(1) as f64;
                        let clip = (load_left / remaining_levels)
                            .max(tail_cfg.min_order_usd)
                            .min(load_left);
                        let aggressive_taker =
                            use_aggressive_taker && (level == 0 || aggressive_all_levels);
                        let px = if aggressive_taker {
                            legs.cheap_ask
                        } else if use_ultra_cheap_maker_fallback {
                            legs.cheap_ask
                        } else {
                            base_px - tick * ladder_step_ticks * level as f64
                        };
                        if px <= 0.0 || px > legs.cheap_ask {
                            continue;
                        }
                        let qty = (clip / px).max(input.market.min_order_size());
                        let reason = format!(
                            "cheap_tail leg={:?} level={} mode={} px={:.4} ask={:.4} clip_usd={:.2} cumulative={:.2}/{:.2} bundled_cap={:.2} unbundled_ultra_cap={:.2} favorite_exposure={:.2} favorite_avg={:.4} hedge_ratio={:.2} favorite_win_upside={:.2} regime_multiplier={:.2} path_reversal_risk={:.2} coverage_target={:.2} coverage_deficit_forces_taker={} working_late_fav_usd={:.2} reserved_late_fav_usd={:.2} remaining_ms={remaining_ms}",
                            legs.cheap_leg,
                            level,
                            if aggressive_taker { "taker_ioc" } else { "maker_post_only" },
                            px,
                            legs.cheap_ask,
                            clip,
                            current_exposure_usd + (total_clip - load_left),
                            tail_cap_usd,
                            bundled_tail_cap_usd,
                            unbundled_tail_cap_usd,
                            favorite_exposure_usd,
                            favorite_avg_price,
                            cheap_tail_coverage_fraction(&tail_cfg, input.btc_regime.regime()),
                            effective_late_fav_qty * (1.0 - favorite_avg_price).max(0.0),
                            regime_multiplier,
                            path_reversal_risk,
                            coverage_target,
                            coverage_deficit_forces_taker,
                            working_late_fav_usd,
                            reserved_late_fav_usd,
                        );
                        notes.push(reason.clone());
                        intents.push(build_cheap_tail_intent(
                            &input.market,
                            legs.cheap_leg,
                            px,
                            qty,
                            &format!("{level}"),
                            aggressive_taker,
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
            let late_fav_filled_qty =
                inventory_qty_for_leg(input.late_fav_inventory, legs.favorite_leg);
            let favorite_avg_price = inventory_avg_cost_or(
                input.late_fav_inventory,
                legs.favorite_leg,
                legs.favorite_ask,
            );
            let favorite_total_qty =
                inventory_qty_for_leg(input.late_fav_inventory, legs.favorite_leg);
            let other_total_qty = inventory_qty_for_leg(input.cheap_tail_inventory, legs.cheap_leg);
            let working_late_fav_usd =
                open_order_notional_for_leg(input.open_late_fav_order_exposure, legs.favorite_leg);
            let reserved_late_fav_usd =
                self.reserved_notional(input.market.market_id(), legs.favorite_leg);
            let effective_late_fav_qty = effective_favorite_hedge_basis_qty(
                late_fav_filled_qty,
                favorite_avg_price,
                favorite_total_qty,
                other_total_qty,
                working_late_fav_usd,
                reserved_late_fav_usd,
            );
            let favorite_exposure_usd = effective_late_fav_qty * favorite_avg_price;
            let hedge_filled_qty =
                inventory_qty_for_leg(input.cheap_tail_inventory, legs.cheap_leg);
            let hedge_avg_price =
                inventory_avg_cost_or(input.cheap_tail_inventory, legs.cheap_leg, legs.cheap_ask);
            let current_hedge_usd = (hedge_filled_qty * hedge_avg_price)
                + directional_tail_working_spend_usd(&input, legs.cheap_leg);
            let reversal_score = reversal_hedge_score(&input, &legs, &reversal_cfg);
            let hedge_cap_usd = reversal_hedge_cap_usd(
                &reversal_cfg,
                effective_late_fav_qty,
                favorite_avg_price,
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
                    let use_aggressive_taker = should_use_aggressive_reversal_hedge(
                        &reversal_cfg,
                        favorite_avg_price,
                        legs.cheap_ask,
                        reversal_score,
                    );
                    let aggressive_all_levels =
                        use_aggressive_taker && directional_conviction.barbell;
                    for level in 0..level_count {
                        if load_left < reversal_cfg.min_order_usd {
                            break;
                        }
                        let remaining_levels = (level_count - level).max(1) as f64;
                        let clip = (load_left / remaining_levels)
                            .max(reversal_cfg.min_order_usd)
                            .min(load_left);
                        let aggressive_taker =
                            use_aggressive_taker && (level == 0 || aggressive_all_levels);
                        let px = if aggressive_taker {
                            legs.cheap_ask
                        } else {
                            base_px - tick * level as f64
                        };
                        if px <= 0.0 || px > legs.cheap_ask {
                            continue;
                        }
                        let qty = (clip / px).max(input.market.min_order_size());
                        let reason = format!(
                            "reversal_hedge leg={:?} level={} mode={} px={:.4} ask={:.4} clip_usd={:.2} cumulative={:.2}/{:.2} favorite_exposure={:.2} favorite_avg={:.4} favorite_win_upside={:.2} reversal_score={:.2} working_late_fav_usd={:.2} reserved_late_fav_usd={:.2} remaining_ms={remaining_ms}",
                            legs.cheap_leg,
                            level,
                            if aggressive_taker { "taker_ioc" } else { "maker_post_only" },
                            px,
                            legs.cheap_ask,
                            clip,
                            current_hedge_usd + (total_clip - load_left),
                            hedge_cap_usd,
                            favorite_exposure_usd,
                            favorite_avg_price,
                            effective_late_fav_qty * (1.0 - favorite_avg_price).max(0.0),
                            reversal_score,
                            working_late_fav_usd,
                            reserved_late_fav_usd,
                        );
                        notes.push(reason.clone());
                        intents.push(build_reversal_hedge_intent(
                            &input.market,
                            legs.cheap_leg,
                            px,
                            qty,
                            &format!("{level}"),
                            aggressive_taker,
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
            market_postures: HashMap::new(),
        }
    }

    pub fn config(&self) -> &BonereaperMmStrategyConfig {
        &self.config
    }

    fn combine(
        a: StrategyDecision,
        b: StrategyDecision,
        suppress_broad_paired_core: bool,
    ) -> StrategyDecision {
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
            let dropped_broad_paired =
                push_allowed_quote_intents(&mut reactive_intents, quote_intents);
            note_dropped_broad_paired(
                &mut notes,
                dropped_broad_paired,
                "directional sleeves active",
            );
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
            if suppress_broad_paired_core {
                let original_len = quote_intents.len();
                quote_intents.retain(|intent| !is_broad_paired_core_intent(intent));
                note_dropped_broad_paired(
                    &mut notes,
                    original_len.saturating_sub(quote_intents.len()),
                    "latched market posture suppresses fresh broad paired-core",
                );
            }
            if quote_intents.is_empty() {
                return if runtime_commands.is_empty() {
                    StrategyDecision::Noop { notes }
                } else {
                    StrategyDecision::Mixed {
                        intents: Vec::new(),
                        commands: runtime_commands,
                        notes,
                    }
                };
            }
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
        if suppress_broad_paired_core {
            notes.push(
                "paired_core broad stopped: latched market posture suppresses resting broad paired-core"
                    .to_string(),
            );
            return StrategyDecision::Suppress {
                scope: SuppressionScope::PairedOnly,
                reason: CoolingReason::Other("paired_core_broad_stopped".to_string()),
                preserve_quotes: false,
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

fn push_allowed_quote_intents(
    reactive_intents: &mut Vec<OrderIntent>,
    quote_intents: Vec<OrderIntent>,
) -> usize {
    let mut dropped_broad_paired = 0usize;
    for intent in quote_intents {
        if is_broad_paired_core_intent(&intent) {
            dropped_broad_paired += 1;
            continue;
        }
        reactive_intents.push(intent);
    }
    dropped_broad_paired
}

fn is_broad_paired_core_intent(intent: &OrderIntent) -> bool {
    intent
        .quote_level_tag
        .as_deref()
        .is_some_and(|tag| tag.starts_with("paired-core:"))
        && !is_repair_like_paired_core_intent(intent)
}

fn note_dropped_broad_paired(notes: &mut Vec<String>, dropped_broad_paired: usize, reason: &str) {
    if dropped_broad_paired > 0 {
        notes.push(format!(
            "bonereaper dropped {dropped_broad_paired} broad paired-core intents: {reason}"
        ));
    }
}

fn observe_market_posture<M: MarketDescriptor>(
    input: &StrategyInput<M>,
    cfg: &LateFavoriteStrategyConfig,
) -> MarketPosture {
    let Some(legs) = read_legs(&input.snapshot) else {
        return MarketPosture::PairedCore;
    };
    let climb_cfg = cfg.favorite_climb;
    let conviction = directional_conviction(input, &legs, &climb_cfg);
    if conviction.barbell {
        return MarketPosture::BarbellDirectional;
    }

    let elapsed = input
        .market
        .time_remaining_ms(input.now_ms)
        .map(|remaining_ms| elapsed_ms(&input.market, input.now_ms, remaining_ms))
        .unwrap_or(0);
    let path_reversal_risk = conviction.path_reversal_risk;
    let market_price_path_separating = legs.favorite_ask
        >= (climb_cfg.near_touch_min_favorite_ask - 0.10).max(climb_cfg.min_favorite_ask)
        && legs.cheap_ask <= (cfg.convex_tail.max_cheap_ask + 0.05).min(0.35);
    let favorite_is_separating =
        market_price_path_separating || legs.favorite_ask >= 0.75 || legs.cheap_ask <= 0.25;
    let broad_mid_market = legs.favorite_ask <= 0.70 && legs.cheap_ask >= 0.30;
    let momentum_persistent = input.momentum.strength >= 0.50
        && input
            .momentum
            .latest_window_return_bps
            .map(|ret| signed_for_favorite(legs.favorite_leg, ret) > 0.0)
            .unwrap_or(false);

    if market_price_path_separating {
        return if path_reversal_risk >= 0.45 {
            MarketPosture::WhipsawHedge
        } else {
            MarketPosture::NoFreshCore
        };
    }

    match input.btc_regime.regime() {
        Some(BtcRegime::Whipsaw) if path_reversal_risk >= 0.55 => MarketPosture::WhipsawHedge,
        Some(BtcRegime::Whipsaw) if elapsed >= 90_000 && !broad_mid_market => {
            MarketPosture::WhipsawHedge
        }
        Some(BtcRegime::Whipsaw) => MarketPosture::CenterOnly,
        Some(BtcRegime::TrendingVolatile)
            if favorite_is_separating || path_reversal_risk >= 0.30 =>
        {
            MarketPosture::NoFreshCore
        }
        Some(BtcRegime::DirectionalSmooth) if favorite_is_separating && momentum_persistent => {
            MarketPosture::NoFreshCore
        }
        Some(BtcRegime::DirectionalSmooth | BtcRegime::TrendingVolatile) if !broad_mid_market => {
            MarketPosture::CenterOnly
        }
        _ => MarketPosture::PairedCore,
    }
}

fn add_posture_note(
    decision: &mut StrategyDecision,
    latched_posture: MarketPosture,
    observed_posture: MarketPosture,
) {
    let note = format!(
        "bonereaper market_posture observed={observed_posture:?} latched={latched_posture:?} suppress_broad_paired_core={}",
        latched_posture.suppresses_broad_paired_core()
    );
    match decision {
        StrategyDecision::Noop { notes }
        | StrategyDecision::QuoteSet { notes, .. }
        | StrategyDecision::CapitalRecycle { notes, .. }
        | StrategyDecision::Rescue { notes, .. }
        | StrategyDecision::Merge { notes, .. }
        | StrategyDecision::Mixed { notes, .. }
        | StrategyDecision::Suppress { notes, .. } => notes.push(note),
    }
}

fn log_market_classification<M: MarketDescriptor>(
    input: &StrategyInput<M>,
    observed_posture: MarketPosture,
    latched_posture: MarketPosture,
    cfg: &LateFavoriteStrategyConfig,
) {
    let Some(legs) = read_legs(&input.snapshot) else {
        tracing::info!(
            market_id = ?input.market.market_id(),
            btc_regime = ?input.btc_regime.regime(),
            observed_posture = ?observed_posture,
            latched_posture = ?latched_posture,
            suppress_broad_paired_core = latched_posture.suppresses_broad_paired_core(),
            "bonereaper classification no_quotes"
        );
        return;
    };

    let conviction = directional_conviction(input, &legs, &cfg.favorite_climb);
    let market_path = if legs.favorite_ask >= 0.90 && legs.cheap_ask <= 0.10 {
        "exhausted_barbell"
    } else if legs.favorite_ask >= 0.75 || legs.cheap_ask <= 0.25 {
        if conviction.path_reversal_risk >= 0.45 {
            "directional_whipsaw"
        } else {
            "directional_separating"
        }
    } else if legs.favorite_ask <= 0.60 && legs.cheap_ask >= 0.40 {
        "centered_oscillation"
    } else {
        "transition"
    };
    let late_fav_skip_reason = if legs.favorite_ask < cfg.favorite_climb.min_favorite_ask {
        "favorite_ask_below_min"
    } else if !conviction.btc_confirms {
        "btc_model_not_confirming"
    } else if !latched_posture.suppresses_broad_paired_core()
        && !conviction.barbell
        && legs.favorite_ask < cfg.favorite_climb.near_touch_min_favorite_ask
    {
        "paired_core_mid_market"
    } else if conviction.path_reversal_risk >= 0.50
        && legs.favorite_ask < cfg.favorite_climb.taker_min_favorite_ask
    {
        "reversal_risk_sub90"
    } else {
        "eligible_or_blocked_deeper"
    };
    let cheap_tail_skip_reason = if legs.cheap_ask > cfg.convex_tail.max_cheap_ask {
        "cheap_ask_above_max"
    } else if !latched_posture.suppresses_broad_paired_core()
        && !conviction.barbell
        && legs.cheap_ask > cfg.convex_tail.ultra_cheap_max_ask
    {
        "paired_core_not_barbell"
    } else if legs.cheap_ask > cfg.convex_tail.ultra_cheap_max_ask
        && legs.favorite_ask < cfg.favorite_climb.taker_min_favorite_ask
    {
        "not_ultra_without_high_cert_fav"
    } else {
        "eligible_or_blocked_deeper"
    };
    tracing::info!(
        market_id = ?input.market.market_id(),
        btc_regime = ?input.btc_regime.regime(),
        observed_posture = ?observed_posture,
        latched_posture = ?latched_posture,
        suppress_broad_paired_core = latched_posture.suppresses_broad_paired_core(),
        market_path = market_path,
        favorite_leg = ?legs.favorite_leg,
        favorite_ask = legs.favorite_ask,
        cheap_leg = ?legs.cheap_leg,
        cheap_ask = legs.cheap_ask,
        btc_vol_5m_bps = ?input.btc_regime.realized_vol_5m_bps,
        btc_ret_30s_bps = ?input.btc_regime.return_30s_bps,
        btc_ret_60s_bps = ?input.btc_regime.return_60s_bps,
        btc_ret_120s_bps = ?input.btc_regime.return_120s_bps,
        btc_ret_180s_bps = ?input.btc_regime.return_180s_bps,
        momentum_strength = input.momentum.strength,
        momentum_latest_bps = ?input.momentum.latest_window_return_bps,
        barbell = conviction.barbell,
        btc_confirms = conviction.btc_confirms,
        conviction_score = conviction.score,
        model_favorite = conviction.model_favorite,
        path_reversal_risk = conviction.path_reversal_risk,
        late_fav_skip_reason = late_fav_skip_reason,
        cheap_tail_skip_reason = cheap_tail_skip_reason,
        "bonereaper classification"
    );
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
    use crate::markets::BinaryOutcomeMarket;
    use crate::signals::{FairValueEstimate, FairValueModel, MomentumSignal, SignalDirection};
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

    fn test_market() -> BinaryOutcomeMarket {
        let mut market = BinaryOutcomeMarket::btc_5m("m".into(), "yes".into(), "no".into());
        market.price_to_beat = Some(100.0);
        market.event_start_ms = Some(0);
        market.event_end_ms = Some(300_000);
        market
    }

    fn fair_value(p_up: f64) -> FairValueEstimate {
        FairValueEstimate {
            p_up,
            p_down: 1.0 - p_up,
            log_moneyness: 0.0,
            sigma_remaining: 0.01,
            time_remaining_s: 120.0,
            model: FairValueModel::BsmBinary,
        }
    }

    fn strategy_input(
        snapshot: PairedMarketSnapshot,
        btc_regime: crate::signals::BtcRegimeSnapshot,
        momentum: MomentumSignal,
        p_up: f64,
        now_ms: EpochMillis,
    ) -> StrategyInput<BinaryOutcomeMarket> {
        StrategyInput {
            market: test_market(),
            snapshot,
            inventory: Default::default(),
            paired_core_inventory: Default::default(),
            late_fav_inventory: Default::default(),
            cheap_tail_inventory: Default::default(),
            open_convex_order_exposure: Default::default(),
            open_late_fav_order_exposure: Default::default(),
            open_paired_core_order_exposure: Default::default(),
            pair_cost: Default::default(),
            fair_value: fair_value(p_up),
            btc_regime,
            momentum,
            order_book_pressure: Default::default(),
            now_ms,
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
    fn late_fav_bundle_uses_bot_lane_inventory_not_wallet_inventory() {
        let strategy = LateFavoriteStrategy::new(LateFavoriteStrategyConfig::default());
        let snapshot = snap(0.92, 0.93, 0.06, 0.07);
        let legs = read_legs(&snapshot).unwrap();
        let mut input = strategy_input(
            snapshot,
            crate::signals::BtcRegimeSnapshot::default(),
            MomentumSignal::default(),
            0.93,
            120_000,
        );
        input.inventory = crate::market_making::pairing::types::PairedInventorySnapshot {
            yes_qty: 500.0,
            yes_avg_cost: 0.93,
            no_qty: 300.0,
            no_avg_cost: 0.04,
            free_cash_usd: 0.0,
            equity_usd: 0.0,
        };

        let bundle = strategy.bundle_state(&input, &legs);
        assert_eq!(bundle.dominant_fav_leg, LadderLeg::Yes);
        assert_eq!(bundle.fav_filled_qty, 0.0);
        assert_eq!(bundle.fav_filled_spend_usd, 0.0);
        assert_eq!(bundle.tail_filled_qty, 0.0);
        assert_eq!(bundle.tail_filled_spend_usd, 0.0);

        input.late_fav_inventory = crate::market_making::pairing::types::PairedInventorySnapshot {
            yes_qty: 100.0,
            yes_avg_cost: 0.90,
            no_qty: 0.0,
            no_avg_cost: 0.0,
            free_cash_usd: 0.0,
            equity_usd: 0.0,
        };
        input.cheap_tail_inventory =
            crate::market_making::pairing::types::PairedInventorySnapshot {
                yes_qty: 0.0,
                yes_avg_cost: 0.0,
                no_qty: 25.0,
                no_avg_cost: 0.04,
                free_cash_usd: 0.0,
                equity_usd: 0.0,
            };

        let bundle = strategy.bundle_state(&input, &legs);
        assert_eq!(bundle.dominant_fav_leg, LadderLeg::Yes);
        assert!((bundle.fav_filled_qty - 100.0).abs() < 1e-9);
        assert!((bundle.fav_filled_spend_usd - 90.0).abs() < 1e-9);
        assert!((bundle.tail_filled_qty - 25.0).abs() < 1e-9);
        assert!((bundle.tail_filled_spend_usd - 1.0).abs() < 1e-9);
    }

    #[test]
    fn late_fav_bundle_reclassifies_unmatched_paired_core_inventory() {
        let strategy = LateFavoriteStrategy::new(LateFavoriteStrategyConfig::default());
        let snapshot = snap(0.92, 0.93, 0.06, 0.07);
        let legs = read_legs(&snapshot).unwrap();
        let mut input = strategy_input(
            snapshot,
            crate::signals::BtcRegimeSnapshot::default(),
            MomentumSignal::default(),
            0.93,
            120_000,
        );

        input.late_fav_inventory = crate::market_making::pairing::types::PairedInventorySnapshot {
            yes_qty: 100.0,
            yes_avg_cost: 0.90,
            no_qty: 0.0,
            no_avg_cost: 0.0,
            free_cash_usd: 0.0,
            equity_usd: 0.0,
        };
        input.paired_core_inventory =
            crate::market_making::pairing::types::PairedInventorySnapshot {
                yes_qty: 170.0,
                yes_avg_cost: 0.40,
                no_qty: 120.0,
                no_avg_cost: 0.38,
                free_cash_usd: 0.0,
                equity_usd: 0.0,
            };

        let bundle = strategy.bundle_state(&input, &legs);
        assert_eq!(bundle.dominant_fav_leg, LadderLeg::Yes);
        assert!((bundle.fav_filled_qty - 150.0).abs() < 1e-9);
        assert!((bundle.fav_filled_spend_usd - 110.0).abs() < 1e-9);
        assert!((bundle.tail_filled_qty - 0.0).abs() < 1e-9);
        assert!((bundle.tail_filled_spend_usd - 0.0).abs() < 1e-9);

        input.paired_core_inventory =
            crate::market_making::pairing::types::PairedInventorySnapshot {
                yes_qty: 120.0,
                yes_avg_cost: 0.40,
                no_qty: 170.0,
                no_avg_cost: 0.08,
                free_cash_usd: 0.0,
                equity_usd: 0.0,
            };

        let bundle = strategy.bundle_state(&input, &legs);
        assert_eq!(bundle.dominant_fav_leg, LadderLeg::Yes);
        assert!((bundle.fav_filled_qty - 100.0).abs() < 1e-9);
        assert!((bundle.fav_filled_spend_usd - 90.0).abs() < 1e-9);
        assert!((bundle.tail_filled_qty - 50.0).abs() < 1e-9);
        assert!((bundle.tail_filled_spend_usd - 4.0).abs() < 1e-9);
    }

    #[test]
    fn combine_suppresses_resting_paired_core_when_broad_core_is_stopped() {
        let decision = BonereaperMmStrategy::combine(
            StrategyDecision::Noop { notes: Vec::new() },
            StrategyDecision::Noop { notes: Vec::new() },
            true,
        );

        match decision {
            StrategyDecision::Suppress {
                scope,
                preserve_quotes,
                notes,
                ..
            } => {
                assert_eq!(scope, SuppressionScope::PairedOnly);
                assert!(!preserve_quotes);
                assert!(notes
                    .iter()
                    .any(|note| note.contains("paired_core broad stopped")));
            }
            other => panic!("expected paired-only suppression, got {other:?}"),
        }
    }

    #[test]
    fn market_posture_latches_escalation_and_does_not_deescalate_intrabar() {
        assert_eq!(
            MarketPosture::BarbellDirectional.merge(MarketPosture::PairedCore),
            MarketPosture::BarbellDirectional
        );
        assert_eq!(
            MarketPosture::WhipsawHedge.merge(MarketPosture::CenterOnly),
            MarketPosture::WhipsawHedge
        );
        assert_eq!(
            MarketPosture::CenterOnly.merge(MarketPosture::PairedCore),
            MarketPosture::CenterOnly
        );
    }

    #[test]
    fn observe_market_posture_identifies_barbell_directional_before_late_window() {
        let input = strategy_input(
            snap(0.95, 0.96, 0.03, 0.04),
            crate::signals::BtcRegimeSnapshot {
                last_price: Some(101.0),
                realized_vol_5m_bps: Some(12.0),
                return_30s_bps: Some(4.0),
                return_60s_bps: Some(12.0),
                return_120s_bps: Some(18.0),
                return_180s_bps: Some(24.0),
                observed_at_ms: 60_000,
                ..Default::default()
            },
            MomentumSignal {
                direction: SignalDirection::Up,
                strength: 1.0,
                latest_window_return_bps: Some(20.0),
                ..Default::default()
            },
            0.97,
            60_000,
        );

        assert_eq!(
            observe_market_posture(&input, &LateFavoriteStrategyConfig::default()),
            MarketPosture::BarbellDirectional
        );
    }

    #[test]
    fn observe_market_posture_keeps_whipsaw_mid_as_center_only() {
        let input = strategy_input(
            snap(0.46, 0.51, 0.49, 0.54),
            crate::signals::BtcRegimeSnapshot {
                realized_vol_5m_bps: Some(12.0),
                return_30s_bps: Some(-1.0),
                return_60s_bps: Some(-2.0),
                return_120s_bps: Some(-3.0),
                return_180s_bps: Some(-4.0),
                observed_at_ms: 60_000,
                ..Default::default()
            },
            MomentumSignal::default(),
            0.50,
            60_000,
        );

        assert_eq!(
            observe_market_posture(&input, &LateFavoriteStrategyConfig::default()),
            MarketPosture::CenterOnly
        );
    }

    #[test]
    fn observe_market_posture_suppresses_core_on_market_price_path_separation() {
        let input = strategy_input(
            snap(0.76, 0.78, 0.21, 0.23),
            crate::signals::BtcRegimeSnapshot {
                last_price: Some(101.0),
                realized_vol_5m_bps: Some(2.0),
                return_30s_bps: Some(-2.0),
                return_60s_bps: Some(1.0),
                return_120s_bps: Some(1.0),
                return_180s_bps: Some(1.0),
                observed_at_ms: 120_000,
                ..Default::default()
            },
            MomentumSignal {
                direction: SignalDirection::Neutral,
                strength: 0.05,
                latest_window_return_bps: Some(0.0),
                ..Default::default()
            },
            0.95,
            120_000,
        );

        assert_eq!(
            observe_market_posture(&input, &LateFavoriteStrategyConfig::default()),
            MarketPosture::WhipsawHedge
        );
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
        assert!(
            favorite_load_price_scale(0.70, 0.70, 0.90)
                < favorite_load_price_scale(0.80, 0.70, 0.90)
        );
        assert!(
            favorite_load_price_scale(0.80, 0.70, 0.90)
                < favorite_load_price_scale(0.90, 0.70, 0.90)
        );
        assert_eq!(favorite_load_price_scale(0.90, 0.70, 0.90), 1.0);
    }

    #[test]
    fn early_barbell_late_favorite_uses_existing_policy_inputs() {
        let cfg = FavoriteClimbConfig {
            taker_min_favorite_ask: 0.90,
            taker_window_sec: 120,
            regime_whipsaw_multiplier: 0.45,
            regime_unknown_multiplier: 0.80,
            ..FavoriteClimbConfig::default()
        };

        assert!(is_pre_standard_late_favorite_window(&cfg, 121_000));
        assert!(!is_pre_standard_late_favorite_window(&cfg, 120_000));
        assert!(is_directional_barbell_favorite(&cfg, 0.90, 0.12));
        assert!(is_directional_barbell_favorite(&cfg, 0.89, 0.12));
        assert!(!is_directional_barbell_favorite(&cfg, 0.84, 0.12));
        assert!(!is_directional_barbell_favorite(&cfg, 0.90, 0.13));
        assert!(early_barbell_late_favorite_blocked(
            &cfg, true, 0.55, 0.92, 10.0, 10.0, 0.95
        ));
        assert!(!early_barbell_late_favorite_blocked(
            &cfg, true, 0.55, 0.93, 10.0, 10.0, 0.95
        ));
    }

    #[test]
    fn true_favorite_timing_scale_is_regime_posture_driven() {
        let cfg = FavoriteClimbConfig {
            regime_unknown_multiplier: 0.31,
            regime_trending_volatile_multiplier: 0.62,
            ..FavoriteClimbConfig::default()
        };

        assert_eq!(late_favorite_timing_scale(&cfg, true, true), 0.35);
        assert_eq!(late_favorite_timing_scale(&cfg, false, true), 0.62);
        assert_eq!(late_favorite_timing_scale(&cfg, false, false), 1.0);
    }

    #[test]
    fn high_cert_late_favorite_tapers_size_as_upside_collapses() {
        assert_eq!(late_favorite_high_cert_price_taper(0.94), 1.0);
        assert_eq!(late_favorite_high_cert_price_taper(0.95), 1.0);
        assert!((late_favorite_high_cert_price_taper(0.97) - 0.59).abs() < 1e-9);
        assert_eq!(late_favorite_high_cert_price_taper(0.99), 0.18);

        assert_eq!(late_favorite_high_cert_max_levels(0.94, 5), 5);
        assert_eq!(late_favorite_high_cert_max_levels(0.95, 5), 3);
        assert_eq!(late_favorite_high_cert_max_levels(0.97, 5), 2);
        assert_eq!(late_favorite_high_cert_max_levels(0.99, 5), 1);
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
            max_late_fav_spend_fraction: 1.0,
            ..ConvexTailConfig::default()
        };

        // 100 shares loaded at 95c has $95 loss-at-risk. At 1c tail, a 50%
        // hedge only needs roughly 48 tail shares, below venue min. If the
        // favorite-win path can afford it, the bundle planner promotes this
        // to one venue-min convex-tail order.
        assert!(
            (cheap_tail_cap_usd(
                &cfg,
                100.0,
                0.95,
                0.95,
                0.01,
                Some(BtcRegime::Whipsaw),
                0.0,
                0.0,
                100.0,
                0.95,
            ) - cfg.min_order_usd)
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
            max_late_fav_spend_fraction: 1.0,
            ..ConvexTailConfig::default()
        };

        // 100 shares at 95c risks $95. In whipsaw, coverage target is doubled
        // from 25% to 50%. At 5c tail, 50 shares offsets half the loss.
        assert!(
            (cheap_tail_cap_usd(
                &cfg,
                100.0,
                0.95,
                0.95,
                0.05,
                Some(BtcRegime::Whipsaw),
                0.0,
                0.0,
                100.0,
                0.95,
            ) - 2.5)
                .abs()
                < 1e-9
        );

        // At 25c the same hedge requires much more spend, and because pair
        // cost is no longer positive-EV the favorite-upside erosion cap binds.
        assert!(
            (cheap_tail_cap_usd(
                &cfg,
                100.0,
                0.95,
                0.95,
                0.25,
                Some(BtcRegime::Whipsaw),
                0.0,
                0.0,
                100.0,
                0.95,
            ) - 2.5)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn cheap_tail_cap_preserves_favorite_win_payoff_in_hard_reversal() {
        let cfg = ConvexTailConfig {
            max_load_usd: 100.0,
            max_favorite_exposure_fraction: 0.25,
            max_win_edge_spend_fraction: 0.50,
            max_late_fav_spend_fraction: 1.0,
            ..ConvexTailConfig::default()
        };

        // Even in hard reversal, cheap-tail is insurance around a favorite
        // sleeve. It cannot spend more than the configured fraction of the
        // favorite-win upside, otherwise the bundle becomes structurally
        // negative when the favorite wins.
        assert!(
            (cheap_tail_cap_usd(
                &cfg,
                100.0,
                0.95,
                0.95,
                0.20,
                Some(BtcRegime::Whipsaw),
                0.75,
                0.0,
                100.0,
                0.95,
            ) - 2.5)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn cheap_tail_cap_allows_min_order_when_favorite_win_payoff_survives() {
        let cfg = ConvexTailConfig {
            min_order_usd: 1.0,
            max_load_usd: 30.0,
            max_favorite_exposure_fraction: 0.55,
            max_win_edge_spend_fraction: 0.45,
            max_late_fav_spend_fraction: 0.025,
            ..ConvexTailConfig::default()
        };

        // A 95c $30 late-fav clip only has about $1.58 favorite-win upside.
        // The old fractional edge cap blocked the venue-min tail order, even
        // though a $1 tail still leaves the favorite-win path positive.
        assert!(
            (cheap_tail_cap_usd(
                &cfg,
                31.9148936170213,
                0.94,
                0.95,
                0.06,
                Some(BtcRegime::DirectionalSmooth),
                0.0,
                0.0,
                31.9148936170213,
                0.94,
            ) - cfg.min_order_usd)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn ultra_cheap_tail_budget_expands_only_when_favorite_is_high_cert() {
        let cfg = ConvexTailConfig {
            max_load_usd: 100.0,
            max_favorite_exposure_fraction: 1.0,
            max_win_edge_spend_fraction: 10.0,
            max_late_fav_spend_fraction: 0.025,
            ultra_cheap_max_ask: 0.03,
            ultra_cheap_min_favorite_ask: 0.90,
            ultra_cheap_max_late_fav_spend_fraction: 0.075,
            ..ConvexTailConfig::default()
        };

        let below_high_cert = cheap_tail_cap_usd(
            &cfg,
            100.0,
            0.85,
            0.89,
            0.03,
            Some(BtcRegime::Whipsaw),
            0.0,
            0.0,
            100.0,
            0.85,
        );
        let high_cert_ultra_cheap = cheap_tail_cap_usd(
            &cfg,
            100.0,
            0.85,
            0.90,
            0.03,
            Some(BtcRegime::Whipsaw),
            0.0,
            0.0,
            100.0,
            0.85,
        );
        let high_cert_not_ultra_cheap = cheap_tail_cap_usd(
            &cfg,
            100.0,
            0.85,
            0.90,
            0.05,
            Some(BtcRegime::Whipsaw),
            0.0,
            0.0,
            100.0,
            0.85,
        );

        assert!(high_cert_ultra_cheap > below_high_cert);
        assert!((below_high_cert - 85.0 * 0.025).abs() < 1e-9);
        assert!((high_cert_not_ultra_cheap - 85.0 * 0.025).abs() < 1e-9);
    }

    #[test]
    fn unbundled_ultra_cheap_tail_requires_late_fav_fill() {
        let cfg = ConvexTailConfig {
            clip_usd: 3.0,
            max_load_usd: 30.0,
            min_order_usd: 1.0,
            ultra_cheap_max_ask: 0.03,
            ultra_cheap_min_favorite_ask: 0.90,
            ..ConvexTailConfig::default()
        };

        assert_eq!(
            unbundled_ultra_cheap_tail_cap_usd(&cfg, 0.0, 0.92, 0.02),
            0.0
        );
        assert_eq!(
            unbundled_ultra_cheap_tail_cap_usd(&cfg, 10.0, 0.92, 0.02),
            3.0
        );
        assert_eq!(
            unbundled_ultra_cheap_tail_cap_usd(&cfg, 0.0, 0.89, 0.02),
            0.0
        );
        assert_eq!(
            unbundled_ultra_cheap_tail_cap_usd(&cfg, 0.0, 0.92, 0.04),
            0.0
        );
    }

    #[test]
    fn cheap_tail_cap_is_lower_in_directional_smooth_regime() {
        let cfg = ConvexTailConfig {
            max_load_usd: 100.0,
            max_favorite_exposure_fraction: 0.25,
            max_win_edge_spend_fraction: 10.0,
            max_late_fav_spend_fraction: 1.0,
            ..ConvexTailConfig::default()
        };

        assert!(
            cheap_tail_cap_usd(
                &cfg,
                100.0,
                0.95,
                0.95,
                0.05,
                Some(BtcRegime::DirectionalSmooth),
                0.0,
                0.0,
                100.0,
                0.95,
            ) < cheap_tail_cap_usd(
                &cfg,
                100.0,
                0.95,
                0.95,
                0.05,
                Some(BtcRegime::Whipsaw),
                0.0,
                0.0,
                100.0,
                0.95,
            )
        );
    }

    #[test]
    fn cheap_tail_cap_increases_when_directional_conviction_is_uncertain() {
        let cfg = ConvexTailConfig {
            max_load_usd: 100.0,
            max_favorite_exposure_fraction: 0.25,
            max_win_edge_spend_fraction: 10.0,
            max_late_fav_spend_fraction: 1.0,
            ..ConvexTailConfig::default()
        };

        let confident = cheap_tail_cap_usd(
            &cfg,
            100.0,
            0.90,
            0.90,
            0.05,
            Some(BtcRegime::TrendingVolatile),
            0.0,
            0.0,
            100.0,
            0.90,
        );
        let uncertain = cheap_tail_cap_usd(
            &cfg,
            100.0,
            0.90,
            0.90,
            0.05,
            Some(BtcRegime::TrendingVolatile),
            0.0,
            0.50,
            100.0,
            0.90,
        );

        assert!(uncertain > confident);
    }

    #[test]
    fn cheap_tail_ladder_load_spends_multiple_clips_when_cap_allows() {
        let cfg = ConvexTailConfig {
            clip_usd: 3.0,
            max_load_usd: 30.0,
            min_order_usd: 1.0,
            max_cheap_ask: 0.10,
            ultra_cheap_max_ask: 0.03,
            ultra_cheap_min_favorite_ask: 0.90,
            ..ConvexTailConfig::default()
        };

        assert_eq!(cheap_tail_ladder_load_usd(&cfg, 0.91, 0.08, 20.0), 9.0);
        assert_eq!(cheap_tail_ladder_load_usd(&cfg, 0.91, 0.02, 20.0), 18.0);
        assert_eq!(cheap_tail_ladder_load_usd(&cfg, 0.91, 0.08, 0.50), 0.0);
    }

    #[test]
    fn cheap_tail_ladder_down_is_wider_for_non_ultra_tail() {
        let cfg = ConvexTailConfig {
            max_cheap_ask: 0.10,
            ultra_cheap_max_ask: 0.03,
            ..ConvexTailConfig::default()
        };

        assert_eq!(cheap_tail_ladder_step_ticks(&cfg, 0.02), 1.0);
        assert_eq!(cheap_tail_ladder_step_ticks(&cfg, 0.08), 2.0);
    }

    #[test]
    fn directional_conviction_multipliers_route_barbell_risk() {
        let confirmed = DirectionalConviction {
            score: 0.80,
            barbell: true,
            btc_confirms: true,
            regime: Some(BtcRegime::DirectionalSmooth),
            path_reversal_risk: 0.10,
            favorite_ask: 0.92,
            cheap_ask: 0.08,
            recent_bps: 8.0,
            strongest_bps: 18.0,
            spot_vs_strike_bps: Some(12.0),
            model_favorite: 0.95,
        };
        let unclear = DirectionalConviction {
            btc_confirms: false,
            score: 0.35,
            ..confirmed
        };

        assert!(confirmed.late_favorite_multiplier() > unclear.late_favorite_multiplier());
        assert!(unclear.hedge_uncertainty_boost() > confirmed.hedge_uncertainty_boost());
    }

    #[test]
    fn cheap_tail_is_taker_inside_classification_band() {
        // New structural rule: cheap-tail in the convex band (<= max_cheap_ask)
        // is always taker. Maker rungs at 1-7c sat unfilled because the resting
        // book is too thin and the touch moves before queue position pays off.
        let cfg = ConvexTailConfig {
            max_cheap_ask: 0.10,
            max_favorite_exposure_fraction: 0.50,
            ..ConvexTailConfig::default()
        };

        // In-band: taker.
        assert!(should_use_aggressive_cheap_tail(
            &cfg,
            0.93,
            0.93,
            0.06,
            Some(BtcRegime::DirectionalSmooth),
            0.0,
        ));
        // In-band at the band edge: still taker.
        assert!(should_use_aggressive_cheap_tail(
            &cfg,
            0.93,
            0.93,
            0.10,
            Some(BtcRegime::DirectionalSmooth),
            0.0,
        ));
    }

    #[test]
    fn cheap_tail_outside_band_is_not_treated_as_convex() {
        // Above max_cheap_ask the price band is no longer convex tail; it is
        // either reversal hedge or skipped entirely. Aggressive-taker must not
        // engage there, regardless of regime or favorite certainty.
        let cfg = ConvexTailConfig {
            max_cheap_ask: 0.10,
            ultra_cheap_max_ask: 0.03,
            ultra_cheap_min_favorite_ask: 0.90,
            ..ConvexTailConfig::default()
        };

        assert!(!should_use_aggressive_cheap_tail(
            &cfg,
            0.86,
            0.86,
            0.16,
            Some(BtcRegime::DirectionalSmooth),
            0.0,
        ));
        // Ultra-cheap edge: in-band, taker.
        assert!(should_use_aggressive_cheap_tail(
            &cfg,
            0.95,
            0.95,
            0.03,
            Some(BtcRegime::DirectionalSmooth),
            0.0,
        ));
    }

    #[test]
    fn bundle_gate_blocks_expensive_side_flip_repair() {
        let cfg = ConvexTailConfig {
            max_cheap_ask: 0.10,
            ..ConvexTailConfig::default()
        };
        let bundle = LateFavBundleState {
            dominant_fav_leg: LadderLeg::No,
            current_fav_leg: LadderLeg::Yes,
            tail_leg: LadderLeg::Yes,
            side_flip: true,
            fav_filled_qty: 100.0,
            fav_filled_spend_usd: 92.0,
            tail_filled_qty: 0.0,
            tail_filled_spend_usd: 0.0,
            working_fav_spend_usd: 0.0,
            working_tail_spend_usd: 0.0,
        };

        let gate = bundle.gate_favorite_add(
            &cfg,
            LadderLeg::Yes,
            0.81,
            12.0,
            9.72,
            Some(BtcRegime::Whipsaw),
            0.60,
            false,
        );

        assert!(!gate.allowed);
        assert!(gate.reason.contains("side-flip"));
    }

    #[test]
    fn bundle_gate_allows_cheap_tail_like_side_flip_repair() {
        let cfg = ConvexTailConfig {
            max_cheap_ask: 0.10,
            ultra_cheap_max_ask: 0.05,
            ..ConvexTailConfig::default()
        };
        let bundle = LateFavBundleState {
            dominant_fav_leg: LadderLeg::No,
            current_fav_leg: LadderLeg::Yes,
            tail_leg: LadderLeg::Yes,
            side_flip: true,
            fav_filled_qty: 100.0,
            fav_filled_spend_usd: 92.0,
            tail_filled_qty: 0.0,
            tail_filled_spend_usd: 0.0,
            working_fav_spend_usd: 0.0,
            working_tail_spend_usd: 0.0,
        };

        let gate = bundle.gate_favorite_add(
            &cfg,
            LadderLeg::Yes,
            0.05,
            100.0,
            4.0,
            Some(BtcRegime::Whipsaw),
            0.60,
            true,
        );

        assert!(gate.allowed);
    }

    #[test]
    fn bundle_gate_blocks_side_flip_fav_add_when_tail_outcome_turns_negative() {
        let cfg = ConvexTailConfig {
            max_cheap_ask: 0.10,
            ultra_cheap_max_ask: 0.03,
            ..ConvexTailConfig::default()
        };
        let bundle = LateFavBundleState {
            dominant_fav_leg: LadderLeg::No,
            current_fav_leg: LadderLeg::Yes,
            tail_leg: LadderLeg::Yes,
            side_flip: true,
            fav_filled_qty: 100.0,
            fav_filled_spend_usd: 92.0,
            tail_filled_qty: 1.0,
            tail_filled_spend_usd: 0.02,
            working_fav_spend_usd: 0.0,
            working_tail_spend_usd: 0.0,
        };

        let gate = bundle.gate_favorite_add(
            &cfg,
            LadderLeg::Yes,
            0.10,
            100.0,
            4.0,
            Some(BtcRegime::Whipsaw),
            0.60,
            true,
        );

        assert!(!gate.allowed);
        assert!(gate.reason.contains("tail-win payoff would be negative"));
    }

    #[test]
    fn bundle_gate_blocks_same_side_favorite_when_tail_coverage_is_missing_in_whipsaw() {
        let cfg = ConvexTailConfig {
            clip_usd: 3.0,
            min_order_usd: 1.0,
            max_favorite_exposure_fraction: 0.55,
            ..ConvexTailConfig::default()
        };
        let bundle = LateFavBundleState {
            dominant_fav_leg: LadderLeg::Yes,
            current_fav_leg: LadderLeg::Yes,
            tail_leg: LadderLeg::No,
            side_flip: false,
            fav_filled_qty: 100.0,
            fav_filled_spend_usd: 90.0,
            tail_filled_qty: 5.0,
            tail_filled_spend_usd: 0.25,
            working_fav_spend_usd: 0.0,
            working_tail_spend_usd: 0.0,
        };

        let gate = bundle.gate_favorite_add(
            &cfg,
            LadderLeg::Yes,
            0.92,
            30.0,
            27.60,
            Some(BtcRegime::Whipsaw),
            0.55,
            false,
        );

        assert!(!gate.allowed);
        assert!(gate.reason.contains("tail coverage"));
    }

    #[test]
    fn bundle_gate_blocks_favorite_add_when_tail_drag_makes_fav_win_negative() {
        let cfg = ConvexTailConfig::default();
        let bundle = LateFavBundleState {
            dominant_fav_leg: LadderLeg::Yes,
            current_fav_leg: LadderLeg::Yes,
            tail_leg: LadderLeg::No,
            side_flip: false,
            fav_filled_qty: 0.0,
            fav_filled_spend_usd: 0.0,
            tail_filled_qty: 200.0,
            tail_filled_spend_usd: 16.0,
            working_fav_spend_usd: 0.0,
            working_tail_spend_usd: 0.0,
        };

        let gate = bundle.gate_favorite_add(
            &cfg,
            LadderLeg::Yes,
            0.90,
            5.0,
            4.50,
            Some(BtcRegime::DirectionalSmooth),
            0.10,
            false,
        );

        assert!(!gate.allowed);
        assert!(gate.reason.contains("favorite-win payoff"));
    }

    #[test]
    fn bundle_payoff_and_coverage_charge_working_exposure() {
        let cfg = ConvexTailConfig {
            clip_usd: 3.0,
            min_order_usd: 1.0,
            max_favorite_exposure_fraction: 0.55,
            ..ConvexTailConfig::default()
        };
        let bundle = LateFavBundleState {
            dominant_fav_leg: LadderLeg::Yes,
            current_fav_leg: LadderLeg::Yes,
            tail_leg: LadderLeg::No,
            side_flip: false,
            fav_filled_qty: 100.0,
            fav_filled_spend_usd: 90.0,
            tail_filled_qty: 60.0,
            tail_filled_spend_usd: 3.0,
            working_fav_spend_usd: 20.0,
            working_tail_spend_usd: 6.0,
        };

        assert!((bundle.tail_coverage_ratio() - (60.0 / 110.0)).abs() < 1e-9);
        assert!((bundle.payoff_if_fav_wins_after(0.0, 0.0) + 19.0).abs() < 1e-9);
        assert!((bundle.payoff_if_tail_wins() + 59.0).abs() < 1e-9);

        let gate = bundle.gate_favorite_add(
            &cfg,
            LadderLeg::Yes,
            0.92,
            10.0,
            9.20,
            Some(BtcRegime::Whipsaw),
            0.55,
            false,
        );

        assert!(!gate.allowed);
        assert!(gate.reason.contains("favorite-win payoff"));
    }

    #[test]
    fn live_profile_cheap_tail_regime_shape_is_loaded_from_yaml() {
        let profile = crate::strategy_profile::StrategyProfile::load(std::path::Path::new(
            "config/strategies/whale_bonereaper_strategy.live.yaml",
        ))
        .expect("load live bonereaper profile");
        let late_favorite = profile.bonereaper_mm_config().late_favorite;
        let cfg = late_favorite.convex_tail;
        let climb = late_favorite.favorite_climb;

        assert!(cfg.enabled);
        assert_eq!(cfg.max_cheap_ask, 0.04);
        assert_eq!(cfg.clip_usd, 3.0);
        assert_eq!(cfg.max_load_usd, 30.0);
        assert_eq!(cfg.max_favorite_exposure_fraction, 0.55);
        assert_eq!(cfg.max_win_edge_spend_fraction, 0.30);
        assert_eq!(cfg.max_late_fav_spend_fraction, 0.04);
        assert_eq!(cfg.ultra_cheap_max_ask, 0.04);
        assert_eq!(cfg.maker_improve_ticks, 0.0);
        assert_eq!(cfg.ultra_cheap_min_favorite_ask, 0.90);
        assert_eq!(cfg.ultra_cheap_max_late_fav_spend_fraction, 0.075);
        assert_eq!(climb.clip_usd, 45.0);
        assert_eq!(climb.max_load_usd, 450.0);
        assert_eq!(climb.min_order_usd, 10.0);
        assert_eq!(climb.taker_min_favorite_ask, 0.90);
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
            whipsaw_true_favorite_multiplier: 0.75,
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
            (late_favorite_regime_multiplier(&regime, LadderLeg::Yes, &cfg, 0.85) - 0.125).abs()
                < 1e-9
        );
        assert!(
            (late_favorite_regime_multiplier(&regime, LadderLeg::Yes, &cfg, 0.95) - 0.375).abs()
                < 1e-9
        );
    }
}

impl<M: MarketDescriptor + Clone> TradingStrategy<M> for BonereaperMmStrategy {
    fn name(&self) -> &'static str {
        "bonereaper_mm"
    }

    fn on_tick(&mut self, input: StrategyInput<M>) -> StrategyDecision {
        let market_id = input.market.market_id().clone();
        let observed_posture = observe_market_posture(&input, &self.config.late_favorite);
        let posture = self
            .market_postures
            .entry(market_id)
            .and_modify(|latched| *latched = latched.merge(observed_posture))
            .or_insert(observed_posture);
        log_market_classification(
            &input,
            observed_posture,
            *posture,
            &self.config.late_favorite,
        );
        let core_decision = self.paired_core.on_tick(input.clone());
        let late_decision = self.late_favorite.on_tick(input);
        let mut decision = Self::combine(
            core_decision,
            late_decision,
            posture.suppresses_broad_paired_core(),
        );
        add_posture_note(&mut decision, *posture, observed_posture);
        decision
    }

    fn on_fill(&mut self, input: StrategyFillInput<M>) -> StrategyDecision {
        let core_decision = self.paired_core.on_fill(input.clone());
        let late_decision = self.late_favorite.on_fill(input);
        Self::combine(core_decision, late_decision, false)
    }
}
