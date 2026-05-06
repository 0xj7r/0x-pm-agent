//! YAML/JSON strategy profile schema and conversion helpers.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::core::risk::RiskLimits;
use crate::market_making::paired_mm::{
    CapitalRecycleConfig, ConvexityOverlayConfig, HardPolicyConfig, LadderConfig,
    MergePolicyConfig, RescueConfig, RunningInventoryCaps,
};
use crate::quote_engine::QuoteEngineConfig;
use crate::signals::{BookSanityConfig, ReversalConfig, SideScoreConfig};
use crate::strategies::pair_cost_arb::PairCostArbStrategyConfig;
use crate::strategies::paired_mm::PairedMmStrategyConfig;

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
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
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

    pub fn pair_cost_arb_config(&self) -> PairCostArbStrategyConfig {
        let mut config = PairCostArbStrategyConfig::default();
        config.pair_cost_threshold = self
            .pair_cost
            .threshold
            .unwrap_or(config.pair_cost_threshold);
        config.high_vol_pair_cost_threshold = self
            .pair_cost
            .high_vol_threshold
            .unwrap_or(config.high_vol_pair_cost_threshold);
        config.min_merge_usd = self.pair_cost.min_merge_usd.unwrap_or(config.min_merge_usd);
        config.merge_gas_cost_usd = self
            .rescue
            .merge_gas_cost_usd
            .unwrap_or(config.merge_gas_cost_usd);
        config.high_vol_atr_threshold = self
            .signals
            .vol_regime
            .high_vol_atr_threshold
            .unwrap_or(config.high_vol_atr_threshold);
        config.pause_in_extreme_vol = self
            .signals
            .vol_regime
            .extreme_vol_pause
            .unwrap_or(config.pause_in_extreme_vol);
        config.price_deviation_bps = self
            .signals
            .cheap_leg
            .price_deviation_bps
            .unwrap_or(config.price_deviation_bps);
        config.max_fresh_entry_price = self
            .signals
            .cheap_leg
            .max_entry_price
            .unwrap_or(config.max_fresh_entry_price)
            .clamp(0.0, 0.99);
        config.min_edge_bps = self.pair_cost.min_edge_bps.unwrap_or(config.min_edge_bps);
        config.base_clip_usd = self
            .clip_sizing
            .base_clip_usd
            .unwrap_or(config.base_clip_usd);
        config.min_clip_usd = self.clip_sizing.min_clip_usd.unwrap_or(config.min_clip_usd);
        config.max_clip_usd = self.clip_sizing.max_clip_usd.unwrap_or(config.max_clip_usd);
        config.fractional_kelly = self
            .clip_sizing
            .fractional_kelly
            .unwrap_or(config.fractional_kelly);
        config.capital_scale_factor = self
            .clip_sizing
            .capital_scale_factor
            .unwrap_or(config.capital_scale_factor);
        config.max_excess_usd = self
            .convexity
            .max_excess_usd
            .unwrap_or(config.max_excess_usd);
        config.late_window_sec = self
            .convexity
            .late_window_sec
            .unwrap_or(config.late_window_sec);
        config.convex_p_threshold = self
            .convexity
            .convex_p_threshold
            .or(self.signals.late_window.p_threshold)
            .unwrap_or(config.convex_p_threshold);
        config.avoid_rehedging_when_convex = self
            .convexity
            .avoid_rehedging_when_convex
            .unwrap_or(config.avoid_rehedging_when_convex);
        config.allow_extra_clip_on_winner = self
            .convexity
            .allow_extra_clip_on_winner
            .unwrap_or(config.allow_extra_clip_on_winner);
        config.enable_buy_light_side_rebalance = self
            .operational
            .enable_buy_light_side_rebalance
            .unwrap_or(config.enable_buy_light_side_rebalance);
        config.recycle_min_imbalance_qty = self
            .operational
            .recycle_min_imbalance_qty
            .unwrap_or(config.recycle_min_imbalance_qty);
        config.recycle_min_time_remaining_ms = self
            .operational
            .recycle_min_time_remaining_ms
            .unwrap_or(config.recycle_min_time_remaining_ms);
        config.recycle_max_light_side_spread = self
            .operational
            .recycle_max_light_side_spread
            .unwrap_or(config.recycle_max_light_side_spread);
        config.rescue_enabled = self.rescue.enabled.unwrap_or(config.rescue_enabled);
        config.rescue_late_window_sec = self
            .rescue
            .late_window_sec
            .unwrap_or(config.rescue_late_window_sec);
        config.rescue_min_excess_usd = self
            .rescue
            .min_excess_usd
            .unwrap_or(config.rescue_min_excess_usd);
        config.rescue_hold_threshold = self
            .rescue
            .hold_threshold
            .unwrap_or(config.rescue_hold_threshold);
        config.rescue_threshold = self
            .rescue
            .rescue_threshold
            .unwrap_or(config.rescue_threshold);
        config.rescue_max_fraction = self
            .rescue
            .max_rescue_fraction
            .unwrap_or(config.rescue_max_fraction);
        config.rescue_rehedge_pair_cost_threshold = self
            .rescue
            .rehedge_pair_cost_threshold
            .unwrap_or(config.rescue_rehedge_pair_cost_threshold);
        config
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
            allow_sell_fallback: self
                .rescue
                .sell_unwind_enabled
                .unwrap_or(config.rescue.allow_sell_fallback),
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
        config.convexity_overlay = ConvexityOverlayConfig {
            enabled: self
                .convexity_overlay
                .enabled
                .unwrap_or(config.convexity_overlay.enabled),
            suppress_ladder_when_package_fires: self
                .convexity_overlay
                .suppress_ladder_when_package_fires
                .unwrap_or(config.convexity_overlay.suppress_ladder_when_package_fires),
            start_frac: self
                .convexity_overlay
                .start_frac
                .unwrap_or(config.convexity_overlay.start_frac),
            capital_pct: self
                .convexity_overlay
                .capital_pct
                .unwrap_or(config.convexity_overlay.capital_pct),
            fractional_kelly: self
                .convexity_overlay
                .fractional_kelly
                .unwrap_or(config.convexity_overlay.fractional_kelly),
            max_loss_usd: self
                .convexity_overlay
                .max_loss_usd
                .or(self.convexity_overlay.max_excess_usd)
                .or(self.convexity.max_excess_usd)
                .unwrap_or(config.convexity_overlay.max_loss_usd),
            max_book_take_pct: self
                .convexity_overlay
                .max_book_take_pct
                .unwrap_or(config.convexity_overlay.max_book_take_pct),
            min_order_usd: self
                .convexity_overlay
                .min_order_usd
                .unwrap_or(config.convexity_overlay.min_order_usd),
            max_active_orders_per_leg: self
                .convexity_overlay
                .max_active_orders_per_leg
                .unwrap_or(config.convexity_overlay.max_active_orders_per_leg),
            late_window_sec: self
                .convexity_overlay
                .late_window_sec
                .or(self.convexity.late_window_sec)
                .unwrap_or(config.convexity_overlay.late_window_sec),
            convex_p_threshold: self
                .convexity_overlay
                .convex_p_threshold
                .or(self.convexity.convex_p_threshold)
                .unwrap_or(config.convexity_overlay.convex_p_threshold),
            maker_safety_ticks: self
                .convexity_overlay
                .maker_safety_ticks
                .or(self.quote.maker_safety_ticks)
                .unwrap_or(config.convexity_overlay.maker_safety_ticks),
            min_favorite_edge_bps: self
                .convexity_overlay
                .min_favorite_edge_bps
                .unwrap_or(config.convexity_overlay.min_favorite_edge_bps),
            tail_enabled: self
                .convexity_overlay
                .tail_enabled
                .unwrap_or(config.convexity_overlay.tail_enabled),
            max_favorite_win_loss_usd: self
                .convexity_overlay
                .max_favorite_win_loss_usd
                .unwrap_or(config.convexity_overlay.max_favorite_win_loss_usd),
            min_tail_win_profit_usd: self
                .convexity_overlay
                .min_tail_win_profit_usd
                .unwrap_or(config.convexity_overlay.min_tail_win_profit_usd),
            min_tail_payoff_multiple: self
                .convexity_overlay
                .min_tail_payoff_multiple
                .unwrap_or(config.convexity_overlay.min_tail_payoff_multiple),
        };
        config.reversal = ReversalConfig {
            enabled: self
                .signals
                .reversal
                .enabled
                .unwrap_or(config.reversal.enabled),
            deceleration_weight: self
                .signals
                .reversal
                .deceleration_weight
                .unwrap_or(config.reversal.deceleration_weight),
            momentum_flip_weight: self
                .signals
                .reversal
                .momentum_flip_weight
                .unwrap_or(config.reversal.momentum_flip_weight),
            distance_weight: self
                .signals
                .reversal
                .distance_weight
                .unwrap_or(config.reversal.distance_weight),
            orderflow_weight: self
                .signals
                .reversal
                .orderflow_weight
                .unwrap_or(config.reversal.orderflow_weight),
            strong_momentum_bps: self
                .signals
                .reversal
                .strong_momentum_bps
                .unwrap_or(config.reversal.strong_momentum_bps),
            flip_deadband_bps: self
                .signals
                .reversal
                .flip_deadband_bps
                .unwrap_or(config.reversal.flip_deadband_bps),
        };
        config.book_sanity = BookSanityConfig {
            max_spread: self
                .signals
                .book_sanity
                .max_spread
                .or(self.quote.max_spread)
                .unwrap_or(config.book_sanity.max_spread),
            min_top_depth_notional_usd: self
                .signals
                .book_sanity
                .min_top_depth_notional_usd
                .or(self.quote.min_top_depth_notional_usd)
                .unwrap_or(config.book_sanity.min_top_depth_notional_usd),
            max_staleness_ms: self
                .signals
                .book_sanity
                .max_staleness_ms
                .unwrap_or(config.book_sanity.max_staleness_ms),
            max_queue_depth_usd: self
                .signals
                .book_sanity
                .max_queue_depth_usd
                .unwrap_or(config.book_sanity.max_queue_depth_usd),
            max_projected_pair_cost: self
                .signals
                .book_sanity
                .max_projected_pair_cost
                .unwrap_or(config.book_sanity.max_projected_pair_cost),
        };
        config.side_score = SideScoreConfig {
            fair_value_weight: self
                .signals
                .side_score
                .fair_value_weight
                .unwrap_or(config.side_score.fair_value_weight),
            momentum_weight: self
                .signals
                .side_score
                .momentum_weight
                .unwrap_or(config.side_score.momentum_weight),
            orderflow_weight: self
                .signals
                .side_score
                .orderflow_weight
                .unwrap_or(config.side_score.orderflow_weight),
            terminal_timing_weight: self
                .signals
                .side_score
                .terminal_timing_weight
                .unwrap_or(config.side_score.terminal_timing_weight),
            reversal_risk_weight: self
                .signals
                .side_score
                .reversal_risk_weight
                .unwrap_or(config.side_score.reversal_risk_weight),
            book_sanity_weight: self
                .signals
                .side_score
                .book_sanity_weight
                .unwrap_or(config.side_score.book_sanity_weight),
            max_late_convex_tilt: self
                .signals
                .side_score
                .max_late_convex_tilt
                .unwrap_or(config.side_score.max_late_convex_tilt),
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
            .join("config/strategies/btc_5m_paired_mm.live.yaml");
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
            .join("config/strategies/btc_5m_paired_mm.live.yaml");
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
