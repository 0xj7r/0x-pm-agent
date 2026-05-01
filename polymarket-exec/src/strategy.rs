//! Runtime strategy adapter seam.
//!
//! The legacy strategy implementations have been removed from this file.
//! Runtime still calls `crate::strategy::Strategy`; this module now adapts
//! those calls into the modular strategy implementations in `crate::strategies`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::inventory::InventorySnapshot;
use crate::market_context::MarketContextRecord;
use crate::market_making::paired_mm::{
    CapitalRecycleConfig, HardPolicyConfig, LadderConfig, MergePolicyConfig, PairCostTracker,
    PairedInventorySnapshot, PairedMarketSnapshot, RescueConfig, RunningInventoryCaps,
};
use crate::markets::{BinaryOutcomeMarket, MarketDescriptor, MarketTenor, UnderlyingAsset};
use crate::quote_engine::QuoteEngineConfig;
use crate::signals::{estimate_fair_value_with_momentum, FairValueEstimate, FairValueModel};
use crate::signals::fair_value::NoSignalReason;
use crate::strategies::pair_cost_arb::{PairCostArbStrategy, PairCostArbStrategyConfig};
use crate::strategies::paired_mm::{PairedMmStrategy, PairedMmStrategyConfig};
use crate::strategies::traits::{StrategyFillInput, StrategyInput, TradingStrategy};
use crate::types::{
    EpochMillis, FillReport, InstrumentId, MarketId, MarketSnapshot, OrderIntent, QuoteSnapshot,
    RuntimeCommand, RuntimeStatus, SuppressionScope,
};

#[derive(Debug, Clone)]
pub struct BtcRegimeSnapshot {
    pub last_price: Option<f64>,
    pub realized_vol_5m_bps: Option<f64>,
    pub realized_vol_15m_bps: Option<f64>,
    pub trade_count_5m: u64,
    pub trade_count_15m: u64,
    pub return_30s_bps: Option<f64>,
    pub return_60s_bps: Option<f64>,
    pub observed_at_ms: u64,
}

impl Default for BtcRegimeSnapshot {
    fn default() -> Self {
        Self {
            last_price: None,
            realized_vol_5m_bps: None,
            realized_vol_15m_bps: None,
            trade_count_5m: 0,
            trade_count_15m: 0,
            return_30s_bps: None,
            return_60s_bps: None,
            observed_at_ms: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum StrategyDecisionSuppressionKind {
    SoftPause,
    HardRiskOff,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StrategyDecision {
    Noop { notes: Vec<String> },
    QuoteSet { intents: Vec<OrderIntent>, notes: Vec<String> },
    Reactive { intents: Vec<OrderIntent>, notes: Vec<String> },
    Commands { commands: Vec<RuntimeCommand>, notes: Vec<String> },
    Suppress {
        kind: StrategyDecisionSuppressionKind,
        notes: Vec<String>,
    },
}

impl StrategyDecision {
    pub fn none() -> Self {
        Self::Noop { notes: Vec::new() }
    }

    pub fn single(intent: OrderIntent) -> Self {
        Self::QuoteSet {
            intents: vec![intent],
            notes: Vec::new(),
        }
    }

    pub fn quote_set(intents: Vec<OrderIntent>, notes: Vec<String>) -> Self {
        Self::QuoteSet { intents, notes }
    }

    pub fn reactive(intents: Vec<OrderIntent>, notes: Vec<String>) -> Self {
        Self::Reactive { intents, notes }
    }

    pub fn commands(commands: Vec<RuntimeCommand>, notes: Vec<String>) -> Self {
        Self::Commands { commands, notes }
    }

    pub fn suppress(kind: StrategyDecisionSuppressionKind, notes: Vec<String>) -> Self {
        Self::Suppress { kind, notes }
    }

    pub fn intents(&self) -> &[OrderIntent] {
        match self {
            Self::QuoteSet { intents, .. } | Self::Reactive { intents, .. } => intents,
            Self::Noop { .. } | Self::Commands { .. } | Self::Suppress { .. } => &[],
        }
    }

    pub fn notes(&self) -> Vec<String> {
        match self {
            Self::Noop { notes }
            | Self::QuoteSet { notes, .. }
            | Self::Reactive { notes, .. }
            | Self::Commands { notes, .. }
            | Self::Suppress { notes, .. } => notes.clone(),
        }
    }

    pub fn suppress_kind(&self) -> Option<&StrategyDecisionSuppressionKind> {
        match self {
            Self::Suppress { kind, .. } => Some(kind),
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Noop { notes } => notes.is_empty(),
            Self::QuoteSet { intents, notes } | Self::Reactive { intents, notes } => {
                intents.is_empty() && notes.is_empty()
            }
            Self::Commands { commands, notes } => commands.is_empty() && notes.is_empty(),
            Self::Suppress { .. } => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct StrategyContext {
    pub now_ms: EpochMillis,
    pub runtime_status: RuntimeStatus,
    pub inventory: InventorySnapshot,
    pub open_orders_total: usize,
    pub open_orders_for_market: usize,
    pub market_context: Option<MarketContextRecord>,
    pub btc_regime: crate::signals::BtcRegimeSnapshot,
    pub venue_rules: Option<VenueMarketRules>,
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct VenueMarketRules {
    pub minimum_order_size: f64,
    pub minimum_tick_size: f64,
    pub neg_risk: bool,
}

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
        let raw = fs::read_to_string(path)?;
        match path
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
        config.pair_cost_threshold = self.pair_cost.threshold.unwrap_or(config.pair_cost_threshold);
        config.high_vol_pair_cost_threshold = self
            .pair_cost
            .high_vol_threshold
            .unwrap_or(config.high_vol_pair_cost_threshold);
        config.min_merge_usd = self
            .pair_cost
            .min_merge_usd
            .unwrap_or(config.min_merge_usd);
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
        config.max_excess_usd = self.convexity.max_excess_usd.unwrap_or(config.max_excess_usd);
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
            min_imbalance_qty: config.capital_recycle.min_imbalance_qty,
            max_buy_qty: config.capital_recycle.max_buy_qty,
            max_buy_notional_usd: self
                .rescue
                .hedge_rescue_clip_usd
                .unwrap_or(config.capital_recycle.max_buy_notional_usd),
            min_time_remaining_ms: 120_000,
            max_light_side_spread: config.capital_recycle.max_light_side_spread,
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
        config.normal_spacing_ticks = self
            .quote
            .entry_ladder_spacing_ticks
            .unwrap_or(config.normal_spacing_ticks);
        config.base_clip_usd = self.quote.base_clip_usd.unwrap_or(config.base_clip_usd);
        config.max_clip_usd = self.quote.max_clip_usd.unwrap_or(config.max_clip_usd);
        config.fractional_kelly = self
            .clip_sizing
            .fractional_kelly
            .unwrap_or(config.fractional_kelly);
        config.stoikov.gamma = self
            .quote
            .inventory_skew_bps
            .map(|bps| (bps / 10_000.0).max(0.0001))
            .unwrap_or(config.stoikov.gamma);
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
pub struct SignalsSection {
    pub fair_value: FairValueSection,
    pub cheap_leg: CheapLegSection,
    pub vol_regime: VolRegimeSection,
    pub late_window: LateWindowSection,
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
pub struct HybridMmSection {
    pub enabled: Option<bool>,
    pub capital_pct: Option<f64>,
    pub base_spread_bps: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RescueSection {
    pub hedge_rescue_clip_usd: Option<f64>,
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

pub trait Strategy {
    fn name(&self) -> &str;

    fn on_start(&mut self, _context: &StrategyContext) -> StrategyDecision {
        StrategyDecision::none()
    }

    fn on_market_snapshot(
        &mut self,
        _context: &StrategyContext,
        _snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        StrategyDecision::none()
    }

    fn on_fill(&mut self, _context: &StrategyContext, _fill: &FillReport) -> StrategyDecision {
        StrategyDecision::none()
    }

    fn checkpoint_state(&self) -> Option<serde_json::Value> {
        None
    }

    fn restore_checkpoint_state(
        &mut self,
        _state: &serde_json::Value,
    ) -> std::result::Result<(), String> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct NoopStrategy;

impl Strategy for NoopStrategy {
    fn name(&self) -> &str {
        "noop"
    }
}

#[derive(Debug)]
pub enum StrategyMode {
    Hybrid(HybridStrategy),
    Noop(NoopStrategy),
}

impl StrategyMode {
    pub fn try_from_name(
        name: &str,
        profile: Option<&StrategyProfile>,
    ) -> std::result::Result<Self, String> {
        let requested = parse_strategy_names(name);
        if requested.is_empty() {
            return Err("at least one strategy must be configured".to_string());
        }
        if requested.iter().all(|name| name == "noop") {
            return Ok(Self::Noop(NoopStrategy));
        }
        Ok(Self::Hybrid(HybridStrategy::new(profile, &requested)?))
    }

    pub fn taker_fee_coeff(&self) -> f64 {
        match self {
            Self::Hybrid(_) => 0.072,
            Self::Noop(_) => 0.0,
        }
    }
}

impl Strategy for StrategyMode {
    fn name(&self) -> &str {
        match self {
            Self::Hybrid(strategy) => strategy.name(),
            Self::Noop(strategy) => strategy.name(),
        }
    }

    fn on_start(&mut self, context: &StrategyContext) -> StrategyDecision {
        match self {
            Self::Hybrid(strategy) => strategy.on_start(context),
            Self::Noop(strategy) => strategy.on_start(context),
        }
    }

    fn on_market_snapshot(
        &mut self,
        context: &StrategyContext,
        snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        match self {
            Self::Hybrid(strategy) => strategy.on_market_snapshot(context, snapshot),
            Self::Noop(strategy) => strategy.on_market_snapshot(context, snapshot),
        }
    }

    fn on_fill(&mut self, context: &StrategyContext, fill: &FillReport) -> StrategyDecision {
        match self {
            Self::Hybrid(strategy) => strategy.on_fill(context, fill),
            Self::Noop(strategy) => strategy.on_fill(context, fill),
        }
    }
}

fn parse_strategy_names(raw: &str) -> Vec<String> {
    raw.split([',', '+'])
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .flat_map(|name| match name {
            "hybrid" | "pair_cost_hybrid" | "btc_5m_hybrid" => {
                vec!["pair_cost_arb".to_string(), "paired_mm".to_string()]
            }
            "btc_5m_pair_cost_arb" => vec!["pair_cost_arb".to_string()],
            "btc_5m_mm" => vec!["paired_mm".to_string()],
            other => vec![other.to_string()],
        })
        .collect()
}

#[derive(Debug)]
pub struct HybridStrategy {
    name: String,
    pair_cost_arb: Option<PairCostArbStrategy>,
    paired_mm: Option<PairedMmStrategy>,
    quotes_by_market: HashMap<MarketId, HashMap<InstrumentId, QuoteSnapshot>>,
    momentum_weight: f64,
}

impl HybridStrategy {
    fn new(
        profile: Option<&StrategyProfile>,
        requested: &[String],
    ) -> std::result::Result<Self, String> {
        let default_profile = StrategyProfile::default();
        let profile = profile.unwrap_or(&default_profile);
        let mut pair_cost_arb = None;
        let mut paired_mm = None;
        for name in requested {
            match name.as_str() {
                "pair_cost_arb" => {
                    pair_cost_arb =
                        Some(PairCostArbStrategy::new(profile.pair_cost_arb_config()));
                }
                "paired_mm" => {
                    paired_mm = Some(PairedMmStrategy::new(profile.paired_mm_config()));
                }
                "noop" => {}
                other => return Err(format!("unsupported strategy '{other}'")),
            }
        }
        if pair_cost_arb.is_none() && paired_mm.is_none() {
            return Err("configured strategy set contains no active strategy".to_string());
        }
        let name = requested.join(",");
        Ok(Self {
            name,
            pair_cost_arb,
            paired_mm,
            quotes_by_market: HashMap::new(),
            momentum_weight: profile.momentum_weight(),
        })
    }

    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn update_quote_cache(&mut self, snapshot: &MarketSnapshot) {
        self.quotes_by_market
            .entry(snapshot.market_id.clone())
            .or_default()
            .insert(snapshot.instrument_id.clone(), snapshot.quote.clone());
    }

    fn build_input(
        &self,
        context: &StrategyContext,
        market_id: &MarketId,
    ) -> Option<StrategyInput<BinaryOutcomeMarket>> {
        let quotes = self.quotes_by_market.get(market_id)?;
        if quotes.len() < 2 {
            return None;
        }
        let (yes_id, no_id) = infer_yes_no_ids(context.market_context.as_ref(), quotes)?;
        let yes_quote = quotes.get(&yes_id)?.clone();
        let no_quote = quotes.get(&no_id)?.clone();
        let mut market = BinaryOutcomeMarket::btc_5m(market_id.clone(), yes_id.clone(), no_id.clone());
        if let Some(record) = context.market_context.as_ref() {
            market = market.with_context(record);
            market.tenor = infer_tenor(record).unwrap_or(MarketTenor::Minutes(5));
        }
        if let Some(rules) = context.venue_rules {
            if rules.minimum_tick_size.is_finite() && rules.minimum_tick_size > 0.0 {
                market.tick_size = rules.minimum_tick_size;
            }
            if rules.minimum_order_size.is_finite() && rules.minimum_order_size > 0.0 {
                market.min_order_size = rules.minimum_order_size;
            }
        }
        let snapshot = PairedMarketSnapshot {
            market_id: market_id.clone(),
            yes_instrument_id: yes_id.clone(),
            no_instrument_id: no_id.clone(),
            yes_quote,
            no_quote,
        };
        let inventory = paired_inventory_from_context(&context.inventory, market_id, &yes_id, &no_id);
        let pair_cost = PairCostTracker::from_inventory(&inventory);
        let fair_value = fair_value_from_context(context, &market, self.momentum_weight);
        Some(StrategyInput {
            market,
            snapshot,
            inventory,
            pair_cost,
            fair_value,
            btc_regime: context.btc_regime.clone(),
            now_ms: context.now_ms,
        })
    }

    fn convert(decision: crate::types::StrategyDecision) -> StrategyDecision {
        match decision {
            crate::types::StrategyDecision::QuoteSet { intents, notes } => {
                StrategyDecision::quote_set(intents, notes)
            }
            crate::types::StrategyDecision::CapitalRecycle { intents, notes }
            | crate::types::StrategyDecision::Rescue { intents, notes } => {
                StrategyDecision::reactive(intents, notes)
            }
            crate::types::StrategyDecision::Merge { intent, notes } => {
                StrategyDecision::commands(vec![RuntimeCommand::Merge(intent)], notes)
            }
            crate::types::StrategyDecision::Suppress {
                scope,
                reason,
                notes,
                ..
            } => {
                let kind = match scope {
                    SuppressionScope::AllActions => StrategyDecisionSuppressionKind::HardRiskOff,
                    SuppressionScope::PairedOnly | SuppressionScope::AllEntry => {
                        StrategyDecisionSuppressionKind::SoftPause
                    }
                };
                let mut notes = notes;
                notes.push(format!("strategy suppressed: {reason:?} scope={scope:?}"));
                StrategyDecision::suppress(kind, notes)
            }
            crate::types::StrategyDecision::Noop { notes } => StrategyDecision::Noop { notes },
        }
    }

    fn combine(decisions: Vec<StrategyDecision>) -> StrategyDecision {
        let mut notes = Vec::new();
        let mut quote_intents = Vec::new();
        let mut reactive_intents = Vec::new();
        let mut commands = Vec::new();
        let mut hard_suppressed = false;
        let mut soft_suppressed = false;

        for decision in decisions {
            match decision {
                StrategyDecision::Noop { notes: decision_notes } => notes.extend(decision_notes),
                StrategyDecision::QuoteSet {
                    intents,
                    notes: decision_notes,
                } => {
                    quote_intents.extend(intents);
                    notes.extend(decision_notes);
                }
                StrategyDecision::Reactive {
                    intents,
                    notes: decision_notes,
                } => {
                    reactive_intents.extend(intents);
                    notes.extend(decision_notes);
                }
                StrategyDecision::Commands {
                    commands: decision_commands,
                    notes: decision_notes,
                } => {
                    commands.extend(decision_commands);
                    notes.extend(decision_notes);
                }
                StrategyDecision::Suppress {
                    kind,
                    notes: decision_notes,
                } => {
                    match kind {
                        StrategyDecisionSuppressionKind::HardRiskOff => hard_suppressed = true,
                        StrategyDecisionSuppressionKind::SoftPause => soft_suppressed = true,
                    }
                    notes.extend(decision_notes);
                }
            }
        }

        if hard_suppressed {
            return StrategyDecision::suppress(StrategyDecisionSuppressionKind::HardRiskOff, notes);
        }
        if !commands.is_empty() {
            return StrategyDecision::commands(commands, notes);
        }
        if !reactive_intents.is_empty() {
            reactive_intents.extend(quote_intents);
            return StrategyDecision::reactive(reactive_intents, notes);
        }
        if !quote_intents.is_empty() {
            return StrategyDecision::quote_set(quote_intents, notes);
        }
        if soft_suppressed {
            return StrategyDecision::suppress(StrategyDecisionSuppressionKind::SoftPause, notes);
        }
        StrategyDecision::Noop { notes }
    }
}

impl Strategy for HybridStrategy {
    fn name(&self) -> &str {
        self.name()
    }

    fn on_start(&mut self, _context: &StrategyContext) -> StrategyDecision {
        StrategyDecision::Noop {
            notes: vec![format!("strategy adapter active: {}", self.name)],
        }
    }

    fn on_market_snapshot(
        &mut self,
        context: &StrategyContext,
        snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        self.update_quote_cache(snapshot);
        if context.runtime_status != RuntimeStatus::Running {
            return StrategyDecision::none();
        }
        let Some(input) = self.build_input(context, &snapshot.market_id) else {
            return StrategyDecision::Noop {
                notes: vec!["strategy adapter waiting for paired yes/no books".to_string()],
            };
        };
        let mut decisions = Vec::new();
        if let Some(strategy) = self.pair_cost_arb.as_mut() {
            decisions.push(Self::convert(strategy.on_tick(input.clone())));
        }
        if let Some(strategy) = self.paired_mm.as_mut() {
            decisions.push(Self::convert(strategy.on_tick(input)));
        }
        Self::combine(decisions)
    }

    fn on_fill(&mut self, context: &StrategyContext, fill: &FillReport) -> StrategyDecision {
        let Some(input) = self.build_input(context, &fill.market_id) else {
            return StrategyDecision::none();
        };
        let fill_input = StrategyFillInput {
            market: input.market,
            snapshot: input.snapshot,
            fair_value: input.fair_value,
            fill: fill.clone(),
        };
        let mut decisions = Vec::new();
        if let Some(strategy) = self.paired_mm.as_mut() {
            decisions.push(Self::convert(strategy.on_fill(fill_input)));
        }
        Self::combine(decisions)
    }
}

fn infer_yes_no_ids(
    context: Option<&MarketContextRecord>,
    quotes: &HashMap<InstrumentId, QuoteSnapshot>,
) -> Option<(InstrumentId, InstrumentId)> {
    if let Some(record) = context {
        if record.instrument_ids.len() >= 2 {
            let yes = InstrumentId::from(record.instrument_ids[0].clone());
            let no = InstrumentId::from(record.instrument_ids[1].clone());
            if quotes.contains_key(&yes) && quotes.contains_key(&no) {
                return Some((yes, no));
            }
        }
    }
    let mut ids = quotes.keys().cloned().collect::<Vec<_>>();
    ids.sort();
    Some((ids.first()?.clone(), ids.get(1)?.clone()))
}

fn infer_tenor(record: &MarketContextRecord) -> Option<MarketTenor> {
    let start = record.event_start_time_ms?;
    let end = record.event_end_time_ms?;
    let minutes = end.saturating_sub(start) / 60_000;
    Some(MarketTenor::Minutes(minutes.max(1).min(u16::MAX as u64) as u16))
}

fn paired_inventory_from_context(
    inventory: &InventorySnapshot,
    market_id: &MarketId,
    yes_id: &InstrumentId,
    no_id: &InstrumentId,
) -> PairedInventorySnapshot {
    let mut paired = PairedInventorySnapshot {
        free_cash_usd: inventory.free_cash_usd,
        equity_usd: inventory.total_cash_usd + inventory.gross_exposure_usd,
        ..Default::default()
    };
    for position in inventory.positions.iter().filter(|p| &p.market_id == market_id) {
        if &position.instrument_id == yes_id {
            paired.yes_qty = position.quantity.max(0.0);
            paired.yes_avg_cost = position.avg_price.max(0.0);
        } else if &position.instrument_id == no_id {
            paired.no_qty = position.quantity.max(0.0);
            paired.no_avg_cost = position.avg_price.max(0.0);
        }
    }
    paired
}

fn fair_value_from_context(
    context: &StrategyContext,
    market: &BinaryOutcomeMarket,
    momentum_weight: f64,
) -> FairValueEstimate {
    let no_signal = |reason| FairValueEstimate {
        p_up: 0.5,
        p_down: 0.5,
        log_moneyness: f64::NAN,
        sigma_remaining: f64::NAN,
        time_remaining_s: market.time_remaining_fraction(context.now_ms) * 300.0,
        model: FairValueModel::NoSignal(reason),
    };
    let Some(spot) = context.btc_regime.last_price else {
        return no_signal(NoSignalReason::SpotInvalid);
    };
    let Some(strike) = market.price_to_beat else {
        return no_signal(NoSignalReason::StrikeInvalid);
    };
    let tau = market.time_remaining_fraction(context.now_ms);
    let Some(vol_bps) = context.btc_regime.realized_vol_5m_bps else {
        return no_signal(NoSignalReason::VolInvalid);
    };
    let sigma_return = vol_bps / 10_000.0;
    let momentum_return = context.btc_regime.return_60s_bps.unwrap_or(0.0) / 10_000.0
        * momentum_weight.clamp(0.0, 5.0);
    estimate_fair_value_with_momentum(spot, strike, tau, sigma_return, momentum_return)
}

#[allow(dead_code)]
fn _underlying_for_context(_record: Option<&MarketContextRecord>) -> UnderlyingAsset {
    UnderlyingAsset::Btc
}
