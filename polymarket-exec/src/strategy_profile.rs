//! YAML/JSON strategy profile schema and conversion helpers.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::core::risk::RiskLimits;
use crate::market_making::paired_mm::{
    CapitalRecycleConfig, HardPolicyConfig, LadderConfig, MergePolicyConfig, RescueConfig,
    RunningInventoryCaps,
};
use crate::quote_engine::QuoteEngineConfig;
use crate::strategies::bonereaper_mm::{
    BonereaperMmStrategyConfig, ConvexTailConfig, DirectionalSizingConfig, FavoriteClimbConfig,
    LateFavoriteStrategyConfig, ReversalHedgeConfig,
};
use crate::strategies::core_hedge_mm::{CoreHedgeMmConfig, CoreHedgeMmStrategyConfig};
use crate::strategies::paired_mm::PairedMmStrategyConfig;
use crate::strategies::unlawful_mm::UnlawfulMmStrategyConfig;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StrategyProfile {
    pub strategy: Option<String>,
    pub profile_name: String,
    pub version: Option<String>,
    pub last_updated: Option<String>,
    pub description: Option<String>,
    pub quote: ProfileQuote,
    pub inventory: ProfileInventory,
    pub risk: ProfileRisk,
    pub pair: ProfilePair,
    pub fill: ProfileFill,
    pub health: ProfileHealth,
    pub pair_cost: PairCostSection,
    pub clip_sizing: ClipSizingSection,
    pub convexity: ConvexitySection,
    pub convexity_overlay: ConvexityOverlaySection,
    pub fair_value: PairedMmFairValueSection,
    pub signals: SignalsSection,
    pub hybrid_mm: HybridMmSection,
    pub rescue: RescueSection,
    pub operational: OperationalSection,
    pub core_hedge: CoreHedgeSection,
    pub paired_core: CoreHedgeSection,
    pub late_favorite: LateFavoriteSection,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LateFavoriteSection {
    pub favorite_climb: FavoriteClimbSubsection,
    pub convex_tail: ConvexTailSubsection,
    pub reversal_hedge: ReversalHedgeSubsection,
    pub sizing: DirectionalSizingSubsection,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FavoriteClimbSubsection {
    pub enabled: Option<bool>,
    pub min_favorite_ask: Option<f64>,
    pub max_favorite_ask: Option<f64>,
    pub window_sec: Option<u64>,
    pub start_frac: Option<f64>,
    pub min_elapsed_sec: Option<u64>,
    pub clip_usd: Option<f64>,
    pub max_load_usd: Option<f64>,
    pub maker_improve_ticks: Option<f64>,
    pub min_order_usd: Option<f64>,
    pub spot_filter_bps: Option<f64>,
    pub require_spot_match: Option<bool>,
    pub regime_whipsaw_multiplier: Option<f64>,
    pub whipsaw_true_favorite_multiplier: Option<f64>,
    pub near_touch_min_favorite_ask: Option<f64>,
    pub near_touch_maker_improve_ticks: Option<f64>,
    pub regime_flat_multiplier: Option<f64>,
    pub regime_trending_volatile_multiplier: Option<f64>,
    pub regime_unknown_multiplier: Option<f64>,
    pub reversal_multiplier: Option<f64>,
    pub taker_min_favorite_ask: Option<f64>,
    pub taker_window_sec: Option<u64>,
    pub disable_after_ms: Option<u64>,
    pub clip_scale: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConvexTailSubsection {
    pub enabled: Option<bool>,
    pub max_cheap_ask: Option<f64>,
    pub window_sec: Option<u64>,
    pub start_frac: Option<f64>,
    pub clip_usd: Option<f64>,
    pub max_load_usd: Option<f64>,
    pub max_favorite_exposure_fraction: Option<f64>,
    pub max_win_edge_spend_fraction: Option<f64>,
    pub max_late_fav_spend_fraction: Option<f64>,
    pub ultra_cheap_max_ask: Option<f64>,
    pub ultra_cheap_min_favorite_ask: Option<f64>,
    pub ultra_cheap_max_late_fav_spend_fraction: Option<f64>,
    pub maker_improve_ticks: Option<f64>,
    pub min_order_usd: Option<f64>,
    pub disable_after_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReversalHedgeSubsection {
    pub enabled: Option<bool>,
    pub min_hedge_ask: Option<f64>,
    pub max_hedge_ask: Option<f64>,
    pub window_sec: Option<u64>,
    pub start_frac: Option<f64>,
    pub clip_usd: Option<f64>,
    pub max_load_usd: Option<f64>,
    pub max_favorite_exposure_fraction: Option<f64>,
    pub max_win_edge_spend_fraction: Option<f64>,
    pub maker_improve_ticks: Option<f64>,
    pub min_order_usd: Option<f64>,
    pub min_reversal_score: Option<f64>,
    pub whipsaw_score_bonus: Option<f64>,
    pub flat_score_bonus: Option<f64>,
    pub trending_volatile_score_bonus: Option<f64>,
    pub directional_smooth_score_penalty: Option<f64>,
    pub disable_after_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectionalSizingSubsection {
    pub enabled: Option<bool>,
    pub fallback_bankroll_usd: Option<f64>,
    pub favorite_clip_bps: Option<f64>,
    pub favorite_clip_min_usd: Option<f64>,
    pub favorite_clip_max_usd: Option<f64>,
    pub favorite_max_load_bps: Option<f64>,
    pub favorite_max_load_min_usd: Option<f64>,
    pub favorite_max_load_max_usd: Option<f64>,
    pub tail_clip_bps: Option<f64>,
    pub tail_clip_min_usd: Option<f64>,
    pub tail_clip_max_usd: Option<f64>,
    pub tail_max_load_bps: Option<f64>,
    pub tail_max_load_min_usd: Option<f64>,
    pub tail_max_load_max_usd: Option<f64>,
    pub reversal_clip_bps: Option<f64>,
    pub reversal_clip_min_usd: Option<f64>,
    pub reversal_clip_max_usd: Option<f64>,
    pub reversal_max_load_bps: Option<f64>,
    pub reversal_max_load_min_usd: Option<f64>,
    pub reversal_max_load_max_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CoreHedgeSection {
    pub enabled: Option<bool>,
    pub ladder_levels: Option<usize>,
    pub ladder_span: Option<f64>,
    pub center_price: Option<f64>,
    pub clip_shares: Option<f64>,
    pub max_unpaired_core_qty: Option<f64>,
    pub ladder_min_price: Option<f64>,
    pub ladder_max_price: Option<f64>,
    pub center_probe_only: Option<bool>,
    pub repair_pair_cost_limit: Option<f64>,
    pub repair_fee_buffer: Option<f64>,
    pub maker_improve_ticks: Option<f64>,
    pub merge_min_qty: Option<f64>,
    pub merge_batch_cap: Option<f64>,
    pub merge_disabled_after_ms: Option<u64>,
    pub disable_after_elapsed_ms: Option<u64>,
    pub stop_fresh_quotes_on_unpaired_fill: Option<bool>,
    pub book_sanity_enabled: Option<bool>,
    pub book_sanity_max_spread: Option<f64>,
    pub book_sanity_min_top_depth_usd: Option<f64>,
    pub book_sanity_max_queue_ahead_usd: Option<f64>,
    pub book_sanity_max_queue_imbalance_ratio: Option<f64>,
    pub book_sanity_max_projected_pair_cost: Option<f64>,
    pub clip_scale: Option<f64>,
}

impl StrategyProfile {
    pub fn load(path: &Path) -> Result<Self> {
        serde_json::from_value(Self::load_value(path)?).map_err(Into::into)
    }

    pub fn load_merged(paths: &[PathBuf]) -> Result<Self> {
        let mut merged = serde_json::json!({});
        for path in paths {
            merge_json_values(&mut merged, Self::load_value(path.as_path())?);
        }
        serde_json::from_value(merged).map_err(Into::into)
    }

    fn load_value(path: &Path) -> Result<serde_json::Value> {
        let resolved_path = resolve_profile_path(path);
        let raw = fs::read_to_string(&resolved_path)?;
        match resolved_path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref()
        {
            Some("yaml") | Some("yml") => Ok(serde_yaml::from_str(&raw)?),
            Some("json") => Ok(serde_json::from_str(&raw)?),
            _ => serde_json::from_str(&raw)
                .or_else(|_| serde_yaml::from_str(&raw))
                .map_err(Into::into),
        }
    }

    pub fn quote_engine_config(&self) -> QuoteEngineConfig {
        QuoteEngineConfig {
            max_levels_per_side: self.quote.levels_per_side.unwrap_or(3),
            skew_bps: self.quote.skew_cap_bps.unwrap_or(7.5),
            stale_quote_max_age_ms: self.quote.min_quote_age_ms,
            quote_expiry_ms: self.quote.expiry_suppression_ms,
        }
    }

    /// Build the unlawful_mm config from the `core_hedge:` section. The
    /// underlying type is `CoreHedgeMmStrategyConfig` (alias), so this
    /// reuses `core_hedge_mm_config`.
    pub fn unlawful_mm_config(&self) -> UnlawfulMmStrategyConfig {
        self.core_hedge_mm_config()
    }

    /// Build the bonereaper_mm config: paired-MM core (`core_hedge:`) plus
    /// late favorite overlay (`late_favorite:`).
    pub fn bonereaper_mm_config(&self) -> BonereaperMmStrategyConfig {
        BonereaperMmStrategyConfig {
            core_hedge: self.core_hedge_mm_config(),
            late_favorite: self.late_favorite_config(),
        }
    }

    pub fn core_hedge_mm_config(&self) -> CoreHedgeMmStrategyConfig {
        let defaults = CoreHedgeMmConfig::default();
        let s = if core_hedge_section_has_overrides(&self.paired_core) {
            &self.paired_core
        } else {
            &self.core_hedge
        };
        CoreHedgeMmStrategyConfig {
            core_hedge: CoreHedgeMmConfig {
                enabled: s.enabled.unwrap_or(defaults.enabled),
                ladder_levels: s.ladder_levels.unwrap_or(defaults.ladder_levels),
                ladder_span: s.ladder_span.unwrap_or(defaults.ladder_span),
                center_price: s.center_price.unwrap_or(defaults.center_price),
                clip_shares: s.clip_shares.unwrap_or(defaults.clip_shares),
                max_unpaired_core_qty: s
                    .max_unpaired_core_qty
                    .unwrap_or(defaults.max_unpaired_core_qty),
                ladder_min_price: s.ladder_min_price.unwrap_or(defaults.ladder_min_price),
                ladder_max_price: s.ladder_max_price.unwrap_or(defaults.ladder_max_price),
                center_probe_only: s.center_probe_only.unwrap_or(defaults.center_probe_only),
                repair_pair_cost_limit: s
                    .repair_pair_cost_limit
                    .unwrap_or(defaults.repair_pair_cost_limit),
                repair_fee_buffer: s.repair_fee_buffer.unwrap_or(defaults.repair_fee_buffer),
                maker_improve_ticks: s
                    .maker_improve_ticks
                    .unwrap_or(defaults.maker_improve_ticks),
                merge_min_qty: s.merge_min_qty.unwrap_or(defaults.merge_min_qty),
                merge_batch_cap: s.merge_batch_cap.unwrap_or(defaults.merge_batch_cap),
                disable_merge_after_ms: s.merge_disabled_after_ms,
                disable_after_elapsed_ms: s.disable_after_elapsed_ms,
                stop_fresh_quotes_on_unpaired_fill: s
                    .stop_fresh_quotes_on_unpaired_fill
                    .unwrap_or(defaults.stop_fresh_quotes_on_unpaired_fill),
                book_sanity_enabled: s
                    .book_sanity_enabled
                    .unwrap_or(defaults.book_sanity_enabled),
                book_sanity_max_spread: s
                    .book_sanity_max_spread
                    .unwrap_or(defaults.book_sanity_max_spread),
                book_sanity_min_top_depth_usd: s
                    .book_sanity_min_top_depth_usd
                    .unwrap_or(defaults.book_sanity_min_top_depth_usd),
                book_sanity_max_queue_ahead_usd: s
                    .book_sanity_max_queue_ahead_usd
                    .unwrap_or(defaults.book_sanity_max_queue_ahead_usd),
                book_sanity_max_queue_imbalance_ratio: s
                    .book_sanity_max_queue_imbalance_ratio
                    .unwrap_or(defaults.book_sanity_max_queue_imbalance_ratio),
                book_sanity_max_projected_pair_cost: s
                    .book_sanity_max_projected_pair_cost
                    .unwrap_or(defaults.book_sanity_max_projected_pair_cost),
                ..defaults
            },
        }
    }

    pub fn late_favorite_config(&self) -> LateFavoriteStrategyConfig {
        let climb_def = FavoriteClimbConfig::default();
        let tail_def = ConvexTailConfig::default();
        let reversal_def = ReversalHedgeConfig::default();
        let c = &self.late_favorite.favorite_climb;
        let t = &self.late_favorite.convex_tail;
        let r = &self.late_favorite.reversal_hedge;
        let s = &self.late_favorite.sizing;
        let sizing_def = DirectionalSizingConfig::default();
        LateFavoriteStrategyConfig {
            favorite_climb: FavoriteClimbConfig {
                enabled: c.enabled.unwrap_or(climb_def.enabled),
                min_favorite_ask: c.min_favorite_ask.unwrap_or(climb_def.min_favorite_ask),
                max_favorite_ask: c.max_favorite_ask.unwrap_or(climb_def.max_favorite_ask),
                window_sec: c.window_sec.unwrap_or(climb_def.window_sec),
                start_frac: c.start_frac.unwrap_or(climb_def.start_frac),
                min_elapsed_sec: c.min_elapsed_sec.unwrap_or(climb_def.min_elapsed_sec),
                clip_usd: c.clip_usd.unwrap_or(climb_def.clip_usd),
                max_load_usd: c.max_load_usd.unwrap_or(climb_def.max_load_usd),
                maker_improve_ticks: c
                    .maker_improve_ticks
                    .unwrap_or(climb_def.maker_improve_ticks),
                min_order_usd: c.min_order_usd.unwrap_or(climb_def.min_order_usd),
                spot_filter_bps: c.spot_filter_bps.unwrap_or(climb_def.spot_filter_bps),
                require_spot_match: c.require_spot_match.unwrap_or(climb_def.require_spot_match),
                regime_whipsaw_multiplier: c
                    .regime_whipsaw_multiplier
                    .unwrap_or(climb_def.regime_whipsaw_multiplier),
                whipsaw_true_favorite_multiplier: c
                    .whipsaw_true_favorite_multiplier
                    .unwrap_or(climb_def.whipsaw_true_favorite_multiplier),
                near_touch_min_favorite_ask: c
                    .near_touch_min_favorite_ask
                    .unwrap_or(climb_def.near_touch_min_favorite_ask),
                near_touch_maker_improve_ticks: c
                    .near_touch_maker_improve_ticks
                    .unwrap_or(climb_def.near_touch_maker_improve_ticks),
                regime_flat_multiplier: c
                    .regime_flat_multiplier
                    .unwrap_or(climb_def.regime_flat_multiplier),
                regime_trending_volatile_multiplier: c
                    .regime_trending_volatile_multiplier
                    .unwrap_or(climb_def.regime_trending_volatile_multiplier),
                regime_unknown_multiplier: c
                    .regime_unknown_multiplier
                    .unwrap_or(climb_def.regime_unknown_multiplier),
                reversal_multiplier: c
                    .reversal_multiplier
                    .unwrap_or(climb_def.reversal_multiplier),
                taker_min_favorite_ask: c
                    .taker_min_favorite_ask
                    .unwrap_or(climb_def.taker_min_favorite_ask),
                taker_window_sec: c.taker_window_sec.unwrap_or(climb_def.taker_window_sec),
                disable_after_ms: c.disable_after_ms,
            },
            convex_tail: ConvexTailConfig {
                enabled: t.enabled.unwrap_or(tail_def.enabled),
                max_cheap_ask: t.max_cheap_ask.unwrap_or(tail_def.max_cheap_ask),
                window_sec: t.window_sec.unwrap_or(tail_def.window_sec),
                start_frac: t.start_frac.unwrap_or(tail_def.start_frac),
                clip_usd: t.clip_usd.unwrap_or(tail_def.clip_usd),
                max_load_usd: t.max_load_usd.unwrap_or(tail_def.max_load_usd),
                max_favorite_exposure_fraction: t
                    .max_favorite_exposure_fraction
                    .unwrap_or(tail_def.max_favorite_exposure_fraction),
                max_win_edge_spend_fraction: t
                    .max_win_edge_spend_fraction
                    .unwrap_or(tail_def.max_win_edge_spend_fraction),
                max_late_fav_spend_fraction: t
                    .max_late_fav_spend_fraction
                    .unwrap_or(tail_def.max_late_fav_spend_fraction),
                ultra_cheap_max_ask: t
                    .ultra_cheap_max_ask
                    .unwrap_or(tail_def.ultra_cheap_max_ask),
                ultra_cheap_min_favorite_ask: t
                    .ultra_cheap_min_favorite_ask
                    .unwrap_or(tail_def.ultra_cheap_min_favorite_ask),
                ultra_cheap_max_late_fav_spend_fraction: t
                    .ultra_cheap_max_late_fav_spend_fraction
                    .unwrap_or(tail_def.ultra_cheap_max_late_fav_spend_fraction),
                maker_improve_ticks: t
                    .maker_improve_ticks
                    .unwrap_or(tail_def.maker_improve_ticks),
                min_order_usd: t.min_order_usd.unwrap_or(tail_def.min_order_usd),
                disable_after_ms: t.disable_after_ms,
            },
            reversal_hedge: ReversalHedgeConfig {
                enabled: r.enabled.unwrap_or(reversal_def.enabled),
                min_hedge_ask: r.min_hedge_ask.unwrap_or(reversal_def.min_hedge_ask),
                max_hedge_ask: r.max_hedge_ask.unwrap_or(reversal_def.max_hedge_ask),
                window_sec: r.window_sec.unwrap_or(reversal_def.window_sec),
                start_frac: r.start_frac.unwrap_or(reversal_def.start_frac),
                clip_usd: r.clip_usd.unwrap_or(reversal_def.clip_usd),
                max_load_usd: r.max_load_usd.unwrap_or(reversal_def.max_load_usd),
                max_favorite_exposure_fraction: r
                    .max_favorite_exposure_fraction
                    .unwrap_or(reversal_def.max_favorite_exposure_fraction),
                max_win_edge_spend_fraction: r
                    .max_win_edge_spend_fraction
                    .unwrap_or(reversal_def.max_win_edge_spend_fraction),
                maker_improve_ticks: r
                    .maker_improve_ticks
                    .unwrap_or(reversal_def.maker_improve_ticks),
                min_order_usd: r.min_order_usd.unwrap_or(reversal_def.min_order_usd),
                min_reversal_score: r
                    .min_reversal_score
                    .unwrap_or(reversal_def.min_reversal_score),
                whipsaw_score_bonus: r
                    .whipsaw_score_bonus
                    .unwrap_or(reversal_def.whipsaw_score_bonus),
                flat_score_bonus: r.flat_score_bonus.unwrap_or(reversal_def.flat_score_bonus),
                trending_volatile_score_bonus: r
                    .trending_volatile_score_bonus
                    .unwrap_or(reversal_def.trending_volatile_score_bonus),
                directional_smooth_score_penalty: r
                    .directional_smooth_score_penalty
                    .unwrap_or(reversal_def.directional_smooth_score_penalty),
                disable_after_ms: r.disable_after_ms,
            },
            sizing: DirectionalSizingConfig {
                enabled: s.enabled.unwrap_or(sizing_def.enabled),
                fallback_bankroll_usd: s
                    .fallback_bankroll_usd
                    .unwrap_or(sizing_def.fallback_bankroll_usd),
                favorite_clip_bps: s.favorite_clip_bps.unwrap_or(sizing_def.favorite_clip_bps),
                favorite_clip_min_usd: s
                    .favorite_clip_min_usd
                    .unwrap_or(sizing_def.favorite_clip_min_usd),
                favorite_clip_max_usd: s
                    .favorite_clip_max_usd
                    .unwrap_or(sizing_def.favorite_clip_max_usd),
                favorite_max_load_bps: s
                    .favorite_max_load_bps
                    .unwrap_or(sizing_def.favorite_max_load_bps),
                favorite_max_load_min_usd: s
                    .favorite_max_load_min_usd
                    .unwrap_or(sizing_def.favorite_max_load_min_usd),
                favorite_max_load_max_usd: s
                    .favorite_max_load_max_usd
                    .unwrap_or(sizing_def.favorite_max_load_max_usd),
                tail_clip_bps: s.tail_clip_bps.unwrap_or(sizing_def.tail_clip_bps),
                tail_clip_min_usd: s
                    .tail_clip_min_usd
                    .unwrap_or(sizing_def.tail_clip_min_usd),
                tail_clip_max_usd: s
                    .tail_clip_max_usd
                    .unwrap_or(sizing_def.tail_clip_max_usd),
                tail_max_load_bps: s.tail_max_load_bps.unwrap_or(sizing_def.tail_max_load_bps),
                tail_max_load_min_usd: s
                    .tail_max_load_min_usd
                    .unwrap_or(sizing_def.tail_max_load_min_usd),
                tail_max_load_max_usd: s
                    .tail_max_load_max_usd
                    .unwrap_or(sizing_def.tail_max_load_max_usd),
                reversal_clip_bps: s
                    .reversal_clip_bps
                    .unwrap_or(sizing_def.reversal_clip_bps),
                reversal_clip_min_usd: s
                    .reversal_clip_min_usd
                    .unwrap_or(sizing_def.reversal_clip_min_usd),
                reversal_clip_max_usd: s
                    .reversal_clip_max_usd
                    .unwrap_or(sizing_def.reversal_clip_max_usd),
                reversal_max_load_bps: s
                    .reversal_max_load_bps
                    .unwrap_or(sizing_def.reversal_max_load_bps),
                reversal_max_load_min_usd: s
                    .reversal_max_load_min_usd
                    .unwrap_or(sizing_def.reversal_max_load_min_usd),
                reversal_max_load_max_usd: s
                    .reversal_max_load_max_usd
                    .unwrap_or(sizing_def.reversal_max_load_max_usd),
            },
        }
    }

    pub fn paired_mm_config(&self) -> PairedMmStrategyConfig {
        let mut config = PairedMmStrategyConfig::default();
        config.ladder = self.ladder_config();
        config.rescue = RescueConfig {
            min_edge_bps: self
                .rescue
                .hedge_rescue_edge_bps
                .unwrap_or(config.rescue.min_edge_bps),
            max_rescue_qty: self
                .rescue
                .max_rescue_qty
                .unwrap_or(config.rescue.max_rescue_qty),
            allow_sell_fallback: self.rescue.sell_unwind_enabled.unwrap_or(false),
            require_no_guaranteed_loss: true,
        };
        config.merge = MergePolicyConfig {
            min_merge_notional_usd: self
                .pair_cost
                .min_merge_usd
                .or(self.pair.min_merge_notional_usd)
                .unwrap_or(config.merge.min_merge_notional_usd),
            min_expected_gain_usd: config.merge.min_expected_gain_usd,
            max_gas_fee_usd: self
                .rescue
                .merge_gas_cost_usd
                .unwrap_or(config.merge.max_gas_fee_usd),
            batch_qty_threshold: config.merge.batch_qty_threshold,
        };
        config.capital_recycle = CapitalRecycleConfig {
            pair_cost_target: self
                .pair_cost
                .threshold
                .unwrap_or(config.capital_recycle.pair_cost_target),
            routine_pair_cost_target: self
                .operational
                .recycle_routine_pair_cost_target
                .unwrap_or(config.capital_recycle.routine_pair_cost_target),
            cash_pressure_free_cash_ratio: self
                .pair
                .merge_pressure_free_cash_ratio
                .unwrap_or(config.capital_recycle.cash_pressure_free_cash_ratio),
            min_imbalance_qty: self
                .operational
                .recycle_min_imbalance_qty
                .unwrap_or(config.capital_recycle.min_imbalance_qty),
            max_buy_qty: self
                .rescue
                .hedge_rescue_max_qty
                .unwrap_or(config.capital_recycle.max_buy_qty),
            max_buy_notional_usd: self
                .rescue
                .hedge_rescue_clip_usd
                .unwrap_or(config.capital_recycle.max_buy_notional_usd),
            min_time_remaining_ms: self
                .operational
                .recycle_min_time_remaining_ms
                .unwrap_or(config.capital_recycle.min_time_remaining_ms),
            max_light_side_spread: self
                .operational
                .recycle_max_light_side_spread
                .unwrap_or(config.capital_recycle.max_light_side_spread),
            race_buffer_ticks: self
                .rescue
                .hedge_rescue_race_buffer_ticks
                .unwrap_or(config.capital_recycle.race_buffer_ticks),
            cooldown_ms: self
                .operational
                .cooldown_ms
                .unwrap_or(config.capital_recycle.cooldown_ms),
        };
        config.hard_policy = HardPolicyConfig {
            max_gross_cost_usd: self
                .inventory
                .max_gross_cost_usd
                .or(self.inventory.max_gross_notional_usd)
                .or(self.rescue.max_gross_cost_usd)
                .unwrap_or(config.hard_policy.max_gross_cost_usd),
            max_side_imbalance_qty: self
                .inventory
                .max_side_imbalance_qty
                .unwrap_or(config.hard_policy.max_side_imbalance_qty),
            end_of_bar_flatten_ms: self
                .rescue
                .end_of_bar_flatten_ms
                .unwrap_or(config.hard_policy.end_of_bar_flatten_ms),
            max_realized_vol_5m_bps: config.hard_policy.max_realized_vol_5m_bps,
            max_abs_return_60s_bps: config.hard_policy.max_abs_return_60s_bps,
        };
        config
    }

    fn ladder_config(&self) -> LadderConfig {
        let mut config = LadderConfig::default();
        config.max_depth = self
            .quote
            .entry_ladder_levels
            .or(self.quote.levels_per_side)
            .unwrap_or(config.max_depth)
            .max(1);
        config.min_depth = config.min_depth.min(config.max_depth);
        config.low_vol_depth = config.max_depth;
        config.high_vol_depth = config.high_vol_depth.min(config.max_depth);
        config.late_bar_depth = config.late_bar_depth.min(config.max_depth);
        config.normal_spacing_ticks = self
            .quote
            .entry_ladder_spacing_ticks
            .unwrap_or(config.normal_spacing_ticks);
        config.base_clip_usd = self.quote.base_clip_usd.unwrap_or(config.base_clip_usd);
        config.min_clip_usd = self
            .quote
            .min_clip_usd
            .or(self.clip_sizing.min_clip_usd)
            .unwrap_or(config.min_clip_usd);
        config.max_clip_usd = self.quote.max_clip_usd.unwrap_or(config.max_clip_usd);
        config.entry_min_size_multiplier = self
            .quote
            .entry_min_size_multiplier
            .unwrap_or(config.entry_min_size_multiplier);
        config.max_spread = self.quote.max_spread.or(config.max_spread);
        config.max_quote_per_side_usd = self
            .quote
            .max_quote_per_side_usd
            .or(config.max_quote_per_side_usd);
        config.fractional_kelly = self
            .clip_sizing
            .fractional_kelly
            .unwrap_or(config.fractional_kelly);
        config.stoikov.gamma = self
            .quote
            .inventory_skew_bps
            .map(|bps| (bps / 10_000.0).max(0.0001))
            .unwrap_or(config.stoikov.gamma);
        config.fair_value_anchoring.max_model_divergence = self
            .fair_value
            .max_model_divergence
            .unwrap_or(config.fair_value_anchoring.max_model_divergence)
            .clamp(0.0, 0.99);
        config.fair_value_anchoring.model_influence_weight = self
            .fair_value
            .model_influence_weight
            .unwrap_or(config.fair_value_anchoring.model_influence_weight)
            .clamp(0.0, 1.0);
        config.caps = RunningInventoryCaps {
            max_gross_cost_usd: self
                .inventory
                .max_gross_cost_usd
                .or(self.inventory.max_gross_notional_usd)
                .unwrap_or(config.caps.max_gross_cost_usd),
            max_side_imbalance_qty: self
                .inventory
                .max_side_imbalance_qty
                .unwrap_or(config.caps.max_side_imbalance_qty),
            max_entry_notional_usd: self
                .inventory
                .max_order_notional_usd
                .or(self.quote.max_clip_usd)
                .unwrap_or(config.caps.max_entry_notional_usd),
        };
        config
    }

    pub fn momentum_weight(&self) -> f64 {
        self.signals.fair_value.momentum_weight.unwrap_or(0.65)
    }

    /// Build a `RiskLimits` snapshot from the profile's `inventory:`
    /// section. Each cap defaults to the corresponding `RiskLimits`
    /// default when the profile leaves it unset, mirroring the live
    /// runtime's `Runtime::new` mapping.
    pub fn risk_limits(&self) -> RiskLimits {
        let defaults = RiskLimits::default();
        let inv = &self.inventory;
        RiskLimits {
            max_order_notional_usd: inv
                .max_order_notional_usd
                .unwrap_or(defaults.max_order_notional_usd),
            max_gross_notional_usd: inv
                .max_gross_notional_usd
                .unwrap_or(defaults.max_gross_notional_usd),
            max_net_notional_per_market_usd: inv
                .max_net_notional_per_market_usd
                .unwrap_or(defaults.max_net_notional_per_market_usd),
            max_position_quantity_per_instrument: inv
                .max_position_quantity_per_instrument
                .unwrap_or(defaults.max_position_quantity_per_instrument),
            min_free_cash_usd: inv.min_free_cash_usd.unwrap_or(defaults.min_free_cash_usd),
            min_free_cash_bps: inv.min_free_cash_bps.unwrap_or(defaults.min_free_cash_bps),
            min_portfolio_equity_usd: inv
                .min_portfolio_equity_usd
                .unwrap_or(defaults.min_portfolio_equity_usd),
            min_portfolio_equity_bps: inv
                .min_portfolio_equity_bps
                .unwrap_or(defaults.min_portfolio_equity_bps),
            max_session_loss_usd: inv
                .max_session_loss_usd
                .unwrap_or(defaults.max_session_loss_usd),
            max_session_loss_bps: inv
                .max_session_loss_bps
                .unwrap_or(defaults.max_session_loss_bps),
            max_open_orders_total: inv
                .max_open_orders_total
                .unwrap_or(defaults.max_open_orders_total),
            max_open_orders_per_market: inv
                .max_open_orders_per_market
                .unwrap_or(defaults.max_open_orders_per_market),
        }
    }
}

fn core_hedge_section_has_overrides(s: &CoreHedgeSection) -> bool {
    s.enabled.is_some()
        || s.ladder_levels.is_some()
        || s.ladder_span.is_some()
        || s.center_price.is_some()
        || s.clip_shares.is_some()
        || s.ladder_min_price.is_some()
        || s.ladder_max_price.is_some()
        || s.center_probe_only.is_some()
        || s.repair_pair_cost_limit.is_some()
        || s.repair_fee_buffer.is_some()
        || s.maker_improve_ticks.is_some()
        || s.merge_min_qty.is_some()
        || s.merge_batch_cap.is_some()
        || s.merge_disabled_after_ms.is_some()
        || s.disable_after_elapsed_ms.is_some()
        || s.stop_fresh_quotes_on_unpaired_fill.is_some()
        || s.book_sanity_enabled.is_some()
        || s.book_sanity_max_spread.is_some()
        || s.book_sanity_min_top_depth_usd.is_some()
        || s.book_sanity_max_queue_ahead_usd.is_some()
        || s.book_sanity_max_queue_imbalance_ratio.is_some()
        || s.book_sanity_max_projected_pair_cost.is_some()
        || s.clip_scale.is_some()
}

fn resolve_profile_path(path: &Path) -> PathBuf {
    if path.exists() || path.is_absolute() {
        return path.to_path_buf();
    }

    let repo_root_path = Path::new("polymarket-exec").join(path);
    if repo_root_path.exists() {
        return repo_root_path;
    }

    path.to_path_buf()
}

fn merge_json_values(target: &mut serde_json::Value, incoming: serde_json::Value) {
    match (target, incoming) {
        (serde_json::Value::Object(target), serde_json::Value::Object(incoming)) => {
            for (key, value) in incoming {
                merge_json_values(target.entry(key).or_insert(serde_json::Value::Null), value);
            }
        }
        (target, incoming) => *target = incoming,
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileQuote {
    pub levels_per_side: Option<usize>,
    pub base_clip_usd: Option<f64>,
    pub min_clip_usd: Option<f64>,
    pub max_clip_usd: Option<f64>,
    pub min_edge_bps: Option<f64>,
    pub inventory_skew_bps: Option<f64>,
    pub skew_cap_bps: Option<f64>,
    pub min_quote_age_ms: Option<u64>,
    pub expiry_suppression_ms: Option<u64>,
    pub max_spread: Option<f64>,
    pub min_top_depth_notional_usd: Option<f64>,
    pub min_order_notional_usd: Option<f64>,
    pub entry_ladder_levels: Option<usize>,
    pub entry_ladder_spacing_ticks: Option<f64>,
    pub maker_safety_ticks: Option<f64>,
    pub venue_min_order_quantity: Option<f64>,
    pub entry_min_size_multiplier: Option<f64>,
    pub max_quote_per_side_usd: Option<f64>,
    pub refresh_interval_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileInventory {
    pub max_order_notional_usd: Option<f64>,
    pub max_gross_notional_usd: Option<f64>,
    pub max_net_notional_per_market_usd: Option<f64>,
    pub max_position_quantity_per_instrument: Option<f64>,
    pub min_free_cash_usd: Option<f64>,
    pub min_free_cash_bps: Option<f64>,
    pub min_portfolio_equity_usd: Option<f64>,
    pub min_portfolio_equity_bps: Option<f64>,
    pub max_session_loss_usd: Option<f64>,
    pub max_session_loss_bps: Option<f64>,
    pub max_open_orders_total: Option<usize>,
    pub max_open_orders_per_market: Option<usize>,
    pub max_stranded_leg_usd: Option<f64>,
    pub max_gross_cost_usd: Option<f64>,
    pub max_leg_cost_usd: Option<f64>,
    pub max_side_imbalance_qty: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileRisk {
    pub book_stale_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfilePair {
    pub merge_enabled: Option<bool>,
    pub pressure_merge_enabled: Option<bool>,
    pub min_merge_notional_usd: Option<f64>,
    pub merge_pressure_free_cash_ratio: Option<f64>,
    pub merge_pressure_gross_exposure_ratio: Option<f64>,
    pub merge_market_exposure_pressure_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileFill {
    pub taker_fee_bps: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileHealth {
    pub market_ws_stale_ms: Option<u64>,
    pub user_ws_stale_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PairCostSection {
    pub threshold: Option<f64>,
    pub max_entry_pair_cost: Option<f64>,
    pub high_vol_threshold: Option<f64>,
    pub min_merge_usd: Option<f64>,
    pub min_edge_bps: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipSizingSection {
    pub base_clip_usd: Option<f64>,
    pub min_clip_usd: Option<f64>,
    pub max_clip_usd: Option<f64>,
    pub fractional_kelly: Option<f64>,
    pub capital_scale_factor: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConvexitySection {
    pub late_window_sec: Option<u64>,
    pub convex_p_threshold: Option<f64>,
    pub max_excess_usd: Option<f64>,
    pub avoid_rehedging_when_convex: Option<bool>,
    pub allow_extra_clip_on_winner: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConvexityOverlaySection {
    pub enabled: Option<bool>,
    pub suppress_ladder_when_package_fires: Option<bool>,
    pub start_frac: Option<f64>,
    pub capital_pct: Option<f64>,
    pub fractional_kelly: Option<f64>,
    pub max_loss_usd: Option<f64>,
    pub max_book_take_pct: Option<f64>,
    pub min_order_usd: Option<f64>,
    pub max_active_orders_per_leg: Option<usize>,
    pub late_window_sec: Option<u64>,
    pub convex_p_threshold: Option<f64>,
    pub max_excess_usd: Option<f64>,
    pub allow_extra_clip_on_winner: Option<bool>,
    pub maker_safety_ticks: Option<f64>,
    pub min_favorite_edge_bps: Option<f64>,
    pub tail_enabled: Option<bool>,
    pub max_favorite_win_loss_usd: Option<f64>,
    pub min_tail_win_profit_usd: Option<f64>,
    pub min_tail_payoff_multiple: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PairedMmFairValueSection {
    pub max_model_divergence: Option<f64>,
    pub model_influence_weight: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SignalsSection {
    pub fair_value: FairValueSection,
    pub cheap_leg: CheapLegSection,
    pub vol_regime: VolRegimeSection,
    pub late_window: LateWindowSection,
    pub reversal: ReversalSection,
    pub book_sanity: BookSanitySection,
    pub side_score: SideScoreSection,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FairValueSection {
    pub momentum_weight: Option<f64>,
    pub vol_lookback_sec: Option<u64>,
    pub use_piecewise_approx: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CheapLegSection {
    pub price_deviation_bps: Option<f64>,
    pub max_entry_price: Option<f64>,
    pub order_flow_imbalance_threshold: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VolRegimeSection {
    pub high_vol_atr_threshold: Option<f64>,
    pub extreme_vol_pause: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LateWindowSection {
    pub enabled: Option<bool>,
    pub p_threshold: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReversalSection {
    pub enabled: Option<bool>,
    pub deceleration_weight: Option<f64>,
    pub momentum_flip_weight: Option<f64>,
    pub distance_weight: Option<f64>,
    pub orderflow_weight: Option<f64>,
    pub strong_momentum_bps: Option<f64>,
    pub flip_deadband_bps: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BookSanitySection {
    pub max_spread: Option<f64>,
    pub min_top_depth_notional_usd: Option<f64>,
    pub max_staleness_ms: Option<u64>,
    pub max_queue_depth_usd: Option<f64>,
    pub max_projected_pair_cost: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SideScoreSection {
    pub fair_value_weight: Option<f64>,
    pub momentum_weight: Option<f64>,
    pub orderflow_weight: Option<f64>,
    pub terminal_timing_weight: Option<f64>,
    pub reversal_risk_weight: Option<f64>,
    pub book_sanity_weight: Option<f64>,
    pub max_late_convex_tilt: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HybridMmSection {
    pub enabled: Option<bool>,
    pub capital_pct: Option<f64>,
    pub base_spread_bps: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RescueSection {
    pub enabled: Option<bool>,
    pub late_window_sec: Option<u64>,
    pub min_excess_usd: Option<f64>,
    pub hold_threshold: Option<f64>,
    pub rescue_threshold: Option<f64>,
    pub max_rescue_fraction: Option<f64>,
    pub rehedge_pair_cost_threshold: Option<f64>,
    pub hedge_rescue_clip_usd: Option<f64>,
    pub hedge_rescue_max_qty: Option<f64>,
    pub hedge_rescue_edge_bps: Option<f64>,
    pub hedge_rescue_race_buffer_ticks: Option<f64>,
    pub sell_unwind_enabled: Option<bool>,
    pub merge_gas_cost_usd: Option<f64>,
    pub max_gross_cost_usd: Option<f64>,
    pub max_gross_cost_bps: Option<f64>,
    pub max_rescue_qty: Option<f64>,
    pub end_of_bar_flatten_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OperationalSection {
    pub cooldown_ms: Option<u64>,
    pub asymmetric_fill_max_penalty: Option<f64>,
    pub max_exposure_usd: Option<f64>,
    pub max_session_loss_bps: Option<f64>,
    pub enable_buy_light_side_rebalance: Option<bool>,
    pub recycle_min_imbalance_qty: Option<f64>,
    pub recycle_routine_pair_cost_target: Option<f64>,
    pub recycle_min_time_remaining_ms: Option<u64>,
    pub recycle_max_light_side_spread: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paired_mm_default_disables_sell_unwind() {
        let profile = StrategyProfile::default();
        let config = profile.paired_mm_config();

        assert!(
            !config.rescue.allow_sell_fallback,
            "paired_mm must be buy-only by default unless a profile explicitly opts into sell unwind"
        );
        assert!(config.rescue.require_no_guaranteed_loss);
    }

    #[test]
    fn live_paired_mm_profile_disables_sell_unwind() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("config/strategies/archive/btc_5m_paired_mm.live.yaml");
        let profile = StrategyProfile::load(&path).expect("load live paired_mm profile");
        let config = profile.paired_mm_config();

        assert!(
            !config.rescue.allow_sell_fallback,
            "live paired_mm profile must not permit sell unwind"
        );
        assert!(config.rescue.require_no_guaranteed_loss);
    }

    #[test]
    fn live_paired_mm_profile_wires_quote_knobs_into_ladder() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("config/strategies/archive/btc_5m_paired_mm.live.yaml");
        let profile = StrategyProfile::load(&path).expect("load live paired_mm profile");
        let config = profile.paired_mm_config();

        assert_eq!(config.ladder.min_clip_usd, 0.50);
        assert_eq!(config.ladder.entry_min_size_multiplier, 1.0);
        assert_eq!(config.ladder.max_spread, Some(0.08));
    }

    #[test]
    fn paired_mm_profile_can_explicitly_opt_into_sell_unwind() {
        let profile = StrategyProfile {
            rescue: RescueSection {
                sell_unwind_enabled: Some(true),
                ..Default::default()
            },
            ..Default::default()
        };
        let config = profile.paired_mm_config();

        assert!(config.rescue.allow_sell_fallback);
    }
}
