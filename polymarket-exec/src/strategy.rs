//! Strategy implementations and decision logic for unlawful_shear and baseline modes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::inventory::{InventorySnapshot, PositionState};
use crate::market_context::MarketContextRecord;
use crate::quote_engine::QuoteEngineConfig;
use crate::signals::UnlawfulGateConfig;
use crate::types::{
    BookLevel, ClientOrderId, EpochMillis, InstrumentId, IntentKind, MarketId, MarketSnapshot,
    OrderIntent, QuoteSnapshot, RuntimeStatus, TradeSide,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionBucket {
    Preferred,
    Neutral,
    Opportunistic,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlawfulExecutionMode {
    Standby,
    Entry,
    Manage,
    Cleanup,
    Flatten,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlawfulAggressionTier {
    Suppressed,
    Light,
    Normal,
    Press,
}

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

#[derive(Debug, Clone)]
pub struct PairedBookSignal {
    pub cheap_instrument_id: InstrumentId,
    pub expensive_instrument_id: InstrumentId,
    pub cheap_bid: Option<BookLevel>,
    pub cheap_ask: Option<BookLevel>,
    pub expensive_bid: Option<BookLevel>,
    pub expensive_ask: Option<BookLevel>,
    pub price_gap: Option<f64>,
    pub observed_at_ms: u64,
    pub books_fresh: bool,
    pub both_sides_present: bool,
    pub cheap_spread: Option<f64>,
    pub expensive_spread: Option<f64>,
    pub cheap_bid_depth_top3_qty: Option<f64>,
    pub cheap_ask_depth_top3_qty: Option<f64>,
    pub expensive_bid_depth_top3_qty: Option<f64>,
    pub expensive_ask_depth_top3_qty: Option<f64>,
    pub cheap_bid_notional_top3: Option<f64>,
    pub cheap_ask_notional_top3: Option<f64>,
    pub expensive_bid_notional_top3: Option<f64>,
    pub expensive_ask_notional_top3: Option<f64>,
    pub cheap_depth_imbalance_top3: Option<f64>,
    pub expensive_depth_imbalance_top3: Option<f64>,
    pub cheap_taker_buy_qty_60s: f64,
    pub cheap_taker_sell_qty_60s: f64,
    pub expensive_taker_buy_qty_60s: f64,
    pub expensive_taker_sell_qty_60s: f64,
}

impl PairedBookSignal {
    fn with_ids(
        cheap_instrument_id: InstrumentId,
        expensive_instrument_id: InstrumentId,
        cheap_bid: Option<BookLevel>,
        cheap_ask: Option<BookLevel>,
        expensive_bid: Option<BookLevel>,
        expensive_ask: Option<BookLevel>,
        observed_at_ms: u64,
        books_fresh: bool,
    ) -> Self {
        let price_gap = cheap_ask.as_ref().and_then(|cheap| {
            expensive_ask
                .as_ref()
                .map(|expensive| expensive.price - cheap.price)
        });
        let both_sides_present = cheap_bid.is_some()
            && cheap_ask.is_some()
            && expensive_bid.is_some()
            && expensive_ask.is_some();
        Self {
            cheap_instrument_id,
            expensive_instrument_id,
            cheap_bid,
            cheap_ask,
            expensive_bid,
            expensive_ask,
            price_gap,
            observed_at_ms,
            books_fresh,
            both_sides_present,
            cheap_spread: None,
            expensive_spread: None,
            cheap_bid_depth_top3_qty: None,
            cheap_ask_depth_top3_qty: None,
            expensive_bid_depth_top3_qty: None,
            expensive_ask_depth_top3_qty: None,
            cheap_bid_notional_top3: None,
            cheap_ask_notional_top3: None,
            expensive_bid_notional_top3: None,
            expensive_ask_notional_top3: None,
            cheap_depth_imbalance_top3: None,
            expensive_depth_imbalance_top3: None,
            cheap_taker_buy_qty_60s: 0.0,
            cheap_taker_sell_qty_60s: 0.0,
            expensive_taker_buy_qty_60s: 0.0,
            expensive_taker_sell_qty_60s: 0.0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct MarketActivitySignal {
    pub last_trade_event_count_10s: u32,
    pub last_trade_event_count_30s: u32,
    pub last_trade_event_count_60s: u32,
    pub last_trade_event_age_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct UnlawfulSignalSnapshot {
    pub session_bucket: SessionBucket,
    pub mode: UnlawfulExecutionMode,
    pub gate_reasons: Vec<String>,
    pub btc: BtcRegimeSnapshot,
    pub book: PairedBookSignal,
    pub activity: MarketActivitySignal,
    pub first_fill_ms: Option<u64>,
    pub first_merge_ms: Option<u64>,
    pub elapsed_s: Option<u64>,
    pub time_remaining_s: Option<u64>,
    pub clip_scale: f64,
}

impl Default for UnlawfulSignalSnapshot {
    fn default() -> Self {
        Self {
            session_bucket: SessionBucket::Unknown,
            mode: UnlawfulExecutionMode::Standby,
            gate_reasons: Vec::new(),
            btc: BtcRegimeSnapshot::default(),
            book: PairedBookSignal::with_ids(
                InstrumentId::from(""),
                InstrumentId::from(""),
                None,
                None,
                None,
                None,
                0,
                false,
            ),
            activity: MarketActivitySignal::default(),
            first_fill_ms: None,
            first_merge_ms: None,
            elapsed_s: None,
            time_remaining_s: None,
            clip_scale: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct GoatPairConfig {
    pub accumulate_price_max: f64,
    pub aggressive_price_max: f64,
    pub base_clip_usd: f64,
    pub aggressive_clip_usd: f64,
    pub max_gross_cost_usd: f64,
    pub completion_min_pnl_per_share: f64,
    pub max_imbalance_ratio: f64,
    pub taker_fee_coeff: f64,
}

impl GoatPairConfig {
    pub fn from_env() -> Self {
        Self {
            accumulate_price_max: parse_f64("WHALE_PAIR_ACCUMULATE_PRICE_MAX", 0.50),
            aggressive_price_max: parse_f64("WHALE_PAIR_AGGRESSIVE_PRICE_MAX", 0.10),
            base_clip_usd: parse_f64("WHALE_PAIR_BASE_CLIP_USD", 20.0),
            aggressive_clip_usd: parse_f64("WHALE_PAIR_AGGRESSIVE_CLIP_USD", 50.0),
            max_gross_cost_usd: parse_f64("WHALE_PAIR_MAX_GROSS_COST_USD", 200.0),
            completion_min_pnl_per_share: parse_f64(
                "WHALE_PAIR_COMPLETION_MIN_PNL_PER_SHARE",
                0.002,
            ),
            max_imbalance_ratio: parse_f64("WHALE_PAIR_MAX_IMBALANCE_RATIO", 3.0),
            taker_fee_coeff: parse_f64("WHALE_PAIR_TAKER_FEE_COEFF", 0.072),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Btc5mMmConfig {
    pub base_clip_usd: f64,
    pub min_clip_usd: f64,
    pub max_clip_usd: f64,
    pub liquidity_clip_fraction: f64,
    pub hedge_rescue_clip_usd: f64,
    /// Number of ticks to pad the rescue FAK limit above depth-walk price.
    /// Defeats the snapshot-to-venue latency race that otherwise kills FAK
    /// rescues with "no orders found to match" 400s. Cheap insurance:
    /// extra cost per share = ticks * tick_size, vs $0.50+ rescue gain.
    pub hedge_rescue_race_buffer_ticks: f64,
    /// Probability tilt applied to the UP leg's mid per bps of BTC return
    /// over the last 60s. Lets us bid slightly higher on the leg that's
    /// becoming favored as spot moves, instead of quoting symmetrically
    /// around the (laggy) book mid. 0.0 disables. Default kept very mild
    /// so book signal still dominates.
    pub momentum_tilt_per_bps: f64,
    /// Hard cap on the momentum tilt magnitude (in probability units).
    /// Bounds damage from bad regime data or one-off shocks.
    pub momentum_max_tilt: f64,
    pub max_gross_cost_usd: f64,
    pub max_gross_cost_bps: f64,
    pub max_leg_cost_usd: f64,
    pub max_leg_cost_bps: f64,
    pub max_entry_free_cash_bps: f64,
    pub max_rescue_free_cash_bps: f64,
    pub min_edge_bps: f64,
    pub hedge_rescue_edge_bps: f64,
    pub inventory_skew_bps: f64,
    pub max_spread: f64,
    pub min_top_depth_notional_usd: f64,
    pub min_order_notional_usd: f64,
    pub venue_min_order_quantity: f64,
    pub entry_min_size_multiplier: f64,
    pub min_order_quantity: f64,
    pub maker_price_tick: f64,
    pub maker_safety_ticks: f64,
    pub entry_ladder_levels: usize,
    pub entry_ladder_spacing_ticks: f64,
    pub cooldown_ms: u64,
    pub taker_fee_coeff: f64,
    /// Hard ceiling on per-leg paired bid price. Above this, the ladder
    /// loop breaks. Whale data shows favored-leg bids up to $0.97. Env-tunable.
    pub entry_premium_bid_cap: f64,
    /// V2 Signal 1 (order-flow imbalance): suppress paired entry when
    /// taker flow in the expensive leg is too one-sided over the last 60s.
    pub order_flow_imbalance_threshold: f64,
    /// Estimated Polygon gas cost per merge tx, in USDC. Used to penalize
    /// rescue EV. Env-tunable.
    pub merge_gas_cost_usd: f64,
}

impl Btc5mMmConfig {
    pub fn from_env() -> Self {
        let config = Self {
            base_clip_usd: parse_f64("WHALE_PAIR_BTC_5M_MM_BASE_CLIP_USD", 1.10),
            min_clip_usd: parse_f64("WHALE_PAIR_BTC_5M_MM_MIN_CLIP_USD", 0.25),
            max_clip_usd: parse_f64("WHALE_PAIR_BTC_5M_MM_MAX_CLIP_USD", 5.0),
            liquidity_clip_fraction: parse_f64(
                "WHALE_PAIR_BTC_5M_MM_LIQUIDITY_CLIP_FRACTION",
                0.02,
            ),
            hedge_rescue_clip_usd: parse_f64("WHALE_PAIR_BTC_5M_MM_HEDGE_RESCUE_CLIP_USD", 2.50),
            hedge_rescue_race_buffer_ticks: parse_f64(
                "WHALE_PAIR_BTC_5M_MM_HEDGE_RESCUE_RACE_BUFFER_TICKS",
                3.0,
            ),
            momentum_tilt_per_bps: parse_f64("WHALE_PAIR_BTC_5M_MM_MOMENTUM_TILT_PER_BPS", 0.0001),
            momentum_max_tilt: parse_f64("WHALE_PAIR_BTC_5M_MM_MOMENTUM_MAX_TILT", 0.02),
            max_gross_cost_usd: parse_f64("WHALE_PAIR_BTC_5M_MM_MAX_GROSS_COST_USD", 20.0),
            max_gross_cost_bps: parse_f64("WHALE_PAIR_BTC_5M_MM_MAX_GROSS_COST_BPS", 0.0),
            max_leg_cost_usd: parse_f64("WHALE_PAIR_BTC_5M_MM_MAX_LEG_COST_USD", 10.0),
            max_leg_cost_bps: parse_f64("WHALE_PAIR_BTC_5M_MM_MAX_LEG_COST_BPS", 0.0),
            max_entry_free_cash_bps: parse_f64(
                "WHALE_PAIR_BTC_5M_MM_MAX_ENTRY_FREE_CASH_BPS",
                6_000.0,
            ),
            max_rescue_free_cash_bps: parse_f64(
                "WHALE_PAIR_BTC_5M_MM_MAX_RESCUE_FREE_CASH_BPS",
                5_000.0,
            ),
            min_edge_bps: parse_f64("WHALE_PAIR_BTC_5M_MM_MIN_EDGE_BPS", 75.0),
            hedge_rescue_edge_bps: parse_f64("WHALE_PAIR_BTC_5M_MM_HEDGE_RESCUE_EDGE_BPS", 25.0),
            inventory_skew_bps: parse_f64("WHALE_PAIR_BTC_5M_MM_INVENTORY_SKEW_BPS", 150.0),
            max_spread: parse_f64("WHALE_PAIR_BTC_5M_MM_MAX_SPREAD", 0.08),
            min_top_depth_notional_usd: parse_f64(
                "WHALE_PAIR_BTC_5M_MM_MIN_TOP_DEPTH_NOTIONAL_USD",
                2.0,
            ),
            min_order_notional_usd: parse_f64("WHALE_PAIR_BTC_5M_MM_MIN_ORDER_NOTIONAL_USD", 1.0),
            venue_min_order_quantity: parse_f64(
                "WHALE_PAIR_BTC_5M_MM_VENUE_MIN_ORDER_QUANTITY",
                parse_f64("WHALE_PAIR_BTC_5M_MM_MIN_ORDER_QUANTITY", 5.0),
            ),
            entry_min_size_multiplier: parse_f64(
                "WHALE_PAIR_BTC_5M_MM_ENTRY_MIN_SIZE_MULTIPLIER",
                1.0,
            ),
            min_order_quantity: parse_f64("WHALE_PAIR_BTC_5M_MM_TARGET_MIN_ORDER_QUANTITY", 0.01),
            maker_price_tick: parse_f64("WHALE_PAIR_BTC_5M_MM_MAKER_PRICE_TICK", 0.01),
            maker_safety_ticks: parse_f64("WHALE_PAIR_BTC_5M_MM_MAKER_SAFETY_TICKS", 2.0),
            entry_ladder_levels: parse_usize("WHALE_PAIR_BTC_5M_MM_ENTRY_LADDER_LEVELS", 3),
            entry_ladder_spacing_ticks: parse_f64(
                "WHALE_PAIR_BTC_5M_MM_ENTRY_LADDER_SPACING_TICKS",
                1.0,
            ),
            cooldown_ms: parse_u64("WHALE_PAIR_BTC_5M_MM_COOLDOWN_MS", 1_000),
            taker_fee_coeff: parse_f64("WHALE_PAIR_TAKER_FEE_COEFF", 0.072),
            entry_premium_bid_cap: parse_f64(
                "WHALE_PAIR_BTC_5M_MM_ENTRY_PREMIUM_BID_CAP",
                0.97,
            ),
            order_flow_imbalance_threshold: parse_f64(
                "WHALE_PAIR_BTC_5M_MM_ORDER_FLOW_IMBALANCE_THRESHOLD",
                0.60,
            ),
            merge_gas_cost_usd: parse_f64(
                "WHALE_PAIR_BTC_5M_MM_MERGE_GAS_COST_USD",
                0.30,
            ),
        };
        Self {
            base_clip_usd: config.base_clip_usd.max(0.01),
            min_clip_usd: config.min_clip_usd.max(0.01),
            max_clip_usd: config.max_clip_usd.max(config.min_clip_usd.max(0.01)),
            liquidity_clip_fraction: config.liquidity_clip_fraction.clamp(0.0, 1.0),
            hedge_rescue_clip_usd: config.hedge_rescue_clip_usd.max(0.01),
            hedge_rescue_race_buffer_ticks: config.hedge_rescue_race_buffer_ticks.clamp(0.0, 20.0),
            momentum_tilt_per_bps: config.momentum_tilt_per_bps.clamp(0.0, 0.01),
            momentum_max_tilt: config.momentum_max_tilt.clamp(0.0, 0.10),
            max_gross_cost_usd: config.max_gross_cost_usd.max(0.01),
            max_gross_cost_bps: config.max_gross_cost_bps.clamp(0.0, 10_000.0),
            max_leg_cost_usd: config.max_leg_cost_usd.max(0.01),
            max_leg_cost_bps: config.max_leg_cost_bps.clamp(0.0, 10_000.0),
            max_entry_free_cash_bps: config.max_entry_free_cash_bps.clamp(0.0, 10_000.0),
            max_rescue_free_cash_bps: config.max_rescue_free_cash_bps.clamp(0.0, 10_000.0),
            min_edge_bps: config.min_edge_bps.max(0.0),
            hedge_rescue_edge_bps: config.hedge_rescue_edge_bps.max(0.0),
            inventory_skew_bps: config.inventory_skew_bps.max(0.0),
            max_spread: config.max_spread.max(0.001),
            min_top_depth_notional_usd: config.min_top_depth_notional_usd.max(0.0),
            min_order_notional_usd: config.min_order_notional_usd.max(0.01),
            venue_min_order_quantity: config.venue_min_order_quantity.max(0.01),
            entry_min_size_multiplier: config.entry_min_size_multiplier.max(1.0),
            min_order_quantity: config.min_order_quantity.max(0.01),
            maker_price_tick: config.maker_price_tick.clamp(0.001, 0.05),
            maker_safety_ticks: config.maker_safety_ticks.clamp(1.0, 10.0),
            entry_ladder_levels: config.entry_ladder_levels.clamp(1, 32),
            entry_ladder_spacing_ticks: config.entry_ladder_spacing_ticks.clamp(1.0, 10.0),
            cooldown_ms: config.cooldown_ms,
            taker_fee_coeff: config.taker_fee_coeff.max(0.0),
            entry_premium_bid_cap: config.entry_premium_bid_cap.clamp(0.50, 0.99),
            order_flow_imbalance_threshold: config
                .order_flow_imbalance_threshold
                .abs()
                .clamp(0.0, 1.0),
            merge_gas_cost_usd: config.merge_gas_cost_usd.max(0.0),
        }
    }
}

#[derive(Clone, Debug)]
pub struct UnlawfulShearConfig {
    pub cheap_hedge_price_max: f64,
    pub core_price_min: f64,
    pub core_price_max: f64,
    pub min_price_gap: f64,
    pub probe_clip_usd: f64,
    pub core_clip_usd: f64,
    pub hedge_clip_usd: f64,
    pub rebalance_clip_usd: f64,
    pub micro_clip_target_usd: f64,
    pub micro_clip_min_usd: f64,
    pub micro_clip_max_children: usize,
    pub trim_clip_fraction: f64,
    pub max_gross_cost_usd: f64,
    pub target_hedge_ratio_min: f64,
    pub target_hedge_ratio_max: f64,
    pub salvage_drawdown_ratio: f64,
    pub salvage_bid_floor: f64,
    pub max_open_orders_total: usize,
    pub taker_fee_coeff: f64,
    pub microstructure_enabled: bool,
    pub microstructure_require_depth: bool,
    pub microstructure_max_spread: f64,
    pub microstructure_min_ask_notional_top3_usd: f64,
    pub microstructure_max_clip_ask_notional_fraction: f64,
    pub microstructure_imbalance_threshold: f64,
    pub microstructure_weak_bid_scale: f64,
    pub microstructure_thin_ask_scale: f64,
    pub maker_entry_pricing: bool,

    pub regime_primary_hours_utc: Vec<u32>,
    pub regime_secondary_hours_utc: Vec<u32>,
    pub allow_extreme_offhour_override: bool,
    pub entry_window_seconds: u64,
    pub cleanup_start_seconds: u64,
    pub close_start_seconds: u64,
    pub merge_stall_seconds: u64,
    pub entry_book_max_age_ms: u64,
    pub entry_btc_signal_max_age_ms: u64,
    pub primary_min_btc_realized_vol_5m_bps: f64,
    pub primary_min_btc_realized_vol_15m_bps: f64,
    pub primary_min_btc_trade_count_5m: u64,
    pub secondary_min_btc_realized_vol_5m_bps: f64,
    pub secondary_min_btc_realized_vol_15m_bps: f64,
    pub secondary_min_btc_trade_count_5m: u64,
    pub override_min_btc_realized_vol_5m_bps: f64,
    pub override_min_btc_realized_vol_15m_bps: f64,
    pub override_min_btc_trade_count_5m: u64,
    pub entry_cheap_ask_max: f64,
    pub entry_expensive_ask_min: f64,
    pub entry_expensive_ask_max: f64,
    pub entry_price_gap_min: f64,
    pub preferred_cheap_ask_max: f64,
    pub preferred_expensive_ask_min: f64,
    pub preferred_expensive_ask_max: f64,
    pub preferred_price_gap_min: f64,
    pub hard_shock_return_30s_bps: f64,
    pub hard_shock_return_60s_bps: f64,
    pub soft_shock_return_30s_bps: f64,
    pub soft_shock_return_60s_bps: f64,
}

impl UnlawfulShearConfig {
    pub fn from_env() -> Self {
        let config = Self {
            cheap_hedge_price_max: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_CHEAP_HEDGE_PRICE_MAX",
                0.38,
            ),
            core_price_min: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_CORE_PRICE_MIN", 0.52),
            core_price_max: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_CORE_PRICE_MAX", 0.92),
            min_price_gap: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_MIN_PRICE_GAP", 0.12),
            probe_clip_usd: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_PROBE_CLIP_USD", 3.0),
            core_clip_usd: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_CORE_CLIP_USD", 20.0),
            hedge_clip_usd: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_HEDGE_CLIP_USD", 6.0),
            rebalance_clip_usd: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_REBALANCE_CLIP_USD", 12.0),
            micro_clip_target_usd: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICRO_CLIP_TARGET_USD",
                8.0,
            ),
            micro_clip_min_usd: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_MICRO_CLIP_MIN_USD", 1.0),
            micro_clip_max_children: parse_usize(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICRO_CLIP_MAX_CHILDREN",
                1,
            ),
            trim_clip_fraction: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_TRIM_CLIP_FRACTION", 0.30),
            max_gross_cost_usd: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_MAX_GROSS_COST_USD", 120.0),
            target_hedge_ratio_min: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_TARGET_HEDGE_RATIO_MIN",
                0.20,
            ),
            target_hedge_ratio_max: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_TARGET_HEDGE_RATIO_MAX",
                0.60,
            ),
            salvage_drawdown_ratio: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_SALVAGE_DRAWDOWN_RATIO",
                0.18,
            ),
            salvage_bid_floor: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_SALVAGE_BID_FLOOR", 0.05),
            max_open_orders_total: parse_usize(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MAX_OPEN_ORDERS_TOTAL",
                6,
            ),
            taker_fee_coeff: parse_f64("WHALE_PAIR_TAKER_FEE_COEFF", 0.072),
            microstructure_enabled: env::var("WHALE_PAIR_UNLAWFUL_SHEAR_MICROSTRUCTURE_ENABLED")
                .ok()
                .and_then(|raw| raw.parse::<bool>().ok())
                .unwrap_or(true),
            microstructure_require_depth: env::var(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICROSTRUCTURE_REQUIRE_DEPTH",
            )
            .ok()
            .and_then(|raw| raw.parse::<bool>().ok())
            .unwrap_or(false),
            microstructure_max_spread: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICROSTRUCTURE_MAX_SPREAD",
                0.05,
            ),
            microstructure_min_ask_notional_top3_usd: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICROSTRUCTURE_MIN_ASK_NOTIONAL_TOP3_USD",
                2.0,
            ),
            microstructure_max_clip_ask_notional_fraction: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICROSTRUCTURE_MAX_CLIP_ASK_NOTIONAL_FRACTION",
                0.10,
            ),
            microstructure_imbalance_threshold: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICROSTRUCTURE_IMBALANCE_THRESHOLD",
                0.55,
            ),
            microstructure_weak_bid_scale: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICROSTRUCTURE_WEAK_BID_SCALE",
                0.65,
            ),
            microstructure_thin_ask_scale: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICROSTRUCTURE_THIN_ASK_SCALE",
                0.85,
            ),
            maker_entry_pricing: env::var("WHALE_PAIR_UNLAWFUL_SHEAR_MAKER_ENTRY_PRICING")
                .ok()
                .and_then(|raw| raw.parse::<bool>().ok())
                .unwrap_or(false),

            regime_primary_hours_utc: vec![10, 11, 19, 22, 23],
            regime_secondary_hours_utc: vec![0, 9, 12, 20, 21],
            allow_extreme_offhour_override: env::var(
                "WHALE_PAIR_UNLAWFUL_SHEAR_ALLOW_EXTREME_OFFHOUR_OVERRIDE",
            )
            .ok()
            .and_then(|raw| raw.parse::<bool>().ok())
            .unwrap_or(false),
            entry_window_seconds: parse_u64("WHALE_PAIR_UNLAWFUL_SHEAR_ENTRY_WINDOW_SECONDS", 30),
            cleanup_start_seconds: parse_u64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_CLEANUP_START_SECONDS",
                210,
            ),
            close_start_seconds: parse_u64("WHALE_PAIR_UNLAWFUL_SHEAR_CLOSE_START_SECONDS", 270),
            merge_stall_seconds: parse_u64("WHALE_PAIR_UNLAWFUL_SHEAR_MERGE_STALL_SECONDS", 60),
            entry_book_max_age_ms: parse_u64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_ENTRY_BOOK_MAX_AGE_MS",
                1200,
            ),
            entry_btc_signal_max_age_ms: parse_u64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_ENTRY_BTC_SIGNAL_MAX_AGE_MS",
                2000,
            ),
            primary_min_btc_realized_vol_5m_bps: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_PRIMARY_MIN_BTC_REALIZED_VOL_5M_BPS",
                5.0,
            ),
            primary_min_btc_realized_vol_15m_bps: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_PRIMARY_MIN_BTC_REALIZED_VOL_15M_BPS",
                11.0,
            ),
            primary_min_btc_trade_count_5m: parse_u64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_PRIMARY_MIN_BTC_TRADE_COUNT_5M",
                5000,
            ),
            secondary_min_btc_realized_vol_5m_bps: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_SECONDARY_MIN_BTC_REALIZED_VOL_5M_BPS",
                8.0,
            ),
            secondary_min_btc_realized_vol_15m_bps: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_SECONDARY_MIN_BTC_REALIZED_VOL_15M_BPS",
                15.0,
            ),
            secondary_min_btc_trade_count_5m: parse_u64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_SECONDARY_MIN_BTC_TRADE_COUNT_5M",
                8000,
            ),
            override_min_btc_realized_vol_5m_bps: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_OVERRIDE_MIN_BTC_REALIZED_VOL_5M_BPS",
                12.0,
            ),
            override_min_btc_realized_vol_15m_bps: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_OVERRIDE_MIN_BTC_REALIZED_VOL_15M_BPS",
                20.0,
            ),
            override_min_btc_trade_count_5m: parse_u64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_OVERRIDE_MIN_BTC_TRADE_COUNT_5M",
                10000,
            ),
            entry_cheap_ask_max: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_ENTRY_CHEAP_ASK_MAX", 0.47),
            entry_expensive_ask_min: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_ENTRY_EXPENSIVE_ASK_MIN",
                0.56,
            ),
            entry_expensive_ask_max: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_ENTRY_EXPENSIVE_ASK_MAX",
                0.84,
            ),
            entry_price_gap_min: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_ENTRY_PRICE_GAP_MIN", 0.22),
            preferred_cheap_ask_max: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_PREFERRED_CHEAP_ASK_MAX",
                0.40,
            ),
            preferred_expensive_ask_min: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_PREFERRED_EXPENSIVE_ASK_MIN",
                0.62,
            ),
            preferred_expensive_ask_max: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_PREFERRED_EXPENSIVE_ASK_MAX",
                0.78,
            ),
            preferred_price_gap_min: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_PREFERRED_PRICE_GAP_MIN",
                0.35,
            ),
            hard_shock_return_30s_bps: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_HARD_SHOCK_RETURN_30S_BPS",
                25.0,
            ),
            hard_shock_return_60s_bps: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_HARD_SHOCK_RETURN_60S_BPS",
                30.0,
            ),
            soft_shock_return_30s_bps: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_SOFT_SHOCK_RETURN_30S_BPS",
                15.0,
            ),
            soft_shock_return_60s_bps: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_SOFT_SHOCK_RETURN_60S_BPS",
                20.0,
            ),
        };

        normalize_unlawful_invariants(config)
    }
}

#[derive(Debug, Default)]
struct GoatMarketState {
    asks: HashMap<InstrumentId, f64>,
    last_seq: u64,
    last_fill_ms: Option<EpochMillis>,
}

#[derive(Debug, Default)]
struct Btc5mMmMarketState {
    mode: Btc5mMmMarketMode,
    quotes: HashMap<InstrumentId, QuoteSnapshot>,
    market_mid_history: VecDeque<(EpochMillis, f64)>,
    recent_fills: VecDeque<(EpochMillis, InstrumentId, f64)>,
    asymmetric_entry_block_until_ms: Option<EpochMillis>,
    last_action_ms: Option<EpochMillis>,
    last_no_quote_note_ms: Option<EpochMillis>,
    last_cooling_note_ms: Option<EpochMillis>,
    last_cooling_note_key: Option<&'static str>,
    /// Last time we emitted an IOC hedge-rescue intent on this market.
    /// Used to throttle rescue emission so we don't drown the engine's
    /// rate limiter with 85 intents/min on every book tick.
    last_rescue_attempt_ms: Option<EpochMillis>,
    /// Tracks the currently outstanding rescue signature so repeated
    /// snapshots cannot emit unlimited IOC attempts for the same stranded
    /// exposure while the venue/result path has not acknowledged it.
    rescue_state: Option<Btc5mMmRescueState>,
    /// Last time on_fill recorded a fill on this market. Drives the
    /// post-fill entry cooldown that prevents re-stranding immediately
    /// after a merge in a trending market.
    last_fill_ms: Option<EpochMillis>,
    /// Per-bar convex accumulation count. Convex_accum is the
    /// asymmetric-payoff side bet; without a per-bar count cap, a single
    /// trending bar can fire convex 14+ times and accumulate $18 of
    /// stranded inventory. Reset when convex_bar_end_ms changes.
    convex_bids_this_bar: u32,
    /// The bar's event_end_time_ms snapshotted on the last convex bid.
    /// When this differs from the current bar's end_ms, we reset
    /// convex_bids_this_bar to 0 (new bar, fresh count).
    convex_bar_end_ms: Option<EpochMillis>,
    /// Per-bar late-bar-core accumulation count.
    late_bar_core_bids_this_bar: u32,
    /// Bar anchor used for late-bar-core count/budget resets.
    late_bar_core_bar_end_ms: Option<EpochMillis>,
    /// Per-bar late-bar-core spend cap tracker (USD notional).
    late_bar_core_spend_this_bar_usd: f64,
}

impl Btc5mMmMarketState {
    fn last_seen_ms(&self) -> EpochMillis {
        let quote_seen = self
            .quotes
            .values()
            .map(|quote| quote.observed_at_ms)
            .max()
            .unwrap_or_default();
        let mid_seen = self
            .market_mid_history
            .back()
            .map(|(ts, _)| *ts)
            .unwrap_or_default();
        let fill_seen = self
            .recent_fills
            .back()
            .map(|(ts, _, _)| *ts)
            .unwrap_or_default();
        [
            quote_seen,
            mid_seen,
            fill_seen,
            self.asymmetric_entry_block_until_ms.unwrap_or_default(),
            self.last_action_ms.unwrap_or_default(),
            self.last_rescue_attempt_ms.unwrap_or_default(),
            self.rescue_state
                .as_ref()
                .map(|state| state.last_attempt_ms)
                .unwrap_or_default(),
            self.last_fill_ms.unwrap_or_default(),
        ]
        .into_iter()
        .max()
        .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
enum Btc5mMmMarketMode {
    #[default]
    Ready,
    Cooling {
        reason: String,
        until_ms: Option<EpochMillis>,
    },
    ManagingInventory,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Btc5mMmPersistedState {
    version: u32,
    market_states: Vec<Btc5mMmPersistedMarketState>,
    recent_fill_times: Vec<EpochMillis>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Btc5mMmPersistedMarketState {
    market_id: String,
    mode: Btc5mMmMarketMode,
    market_mid_history: Vec<(EpochMillis, f64)>,
    recent_fills: Vec<(EpochMillis, String, f64)>,
    asymmetric_entry_block_until_ms: Option<EpochMillis>,
    last_action_ms: Option<EpochMillis>,
    last_no_quote_note_ms: Option<EpochMillis>,
    last_rescue_attempt_ms: Option<EpochMillis>,
    rescue_state: Option<Btc5mMmRescueState>,
    last_fill_ms: Option<EpochMillis>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Btc5mMmRescueState {
    stranded_instrument_id: String,
    lift_instrument_id: String,
    stranded_qty_bucket: u64,
    attempts: u32,
    first_attempt_ms: EpochMillis,
    last_attempt_ms: EpochMillis,
}

#[derive(Debug, Clone)]
struct Btc5mMmExposureDecision {
    rescue_qty: f64,
    hold_qty: f64,
    reason: String,
    hold_ev_per_share: f64,
    rescue_ev_per_share: Option<f64>,
    held_fair: f64,
    avg_cost: f64,
}

#[derive(Debug, Default)]
struct UnlawfulMarketState {
    quotes: HashMap<InstrumentId, QuoteSnapshot>,
    last_seq: u64,
    last_action_ms: Option<EpochMillis>,
}

#[derive(Debug, Clone, Copy)]
enum UnlawfulShearPhase {
    Early,
    Mid,
    Late,
    VeryLate,
    Unknown,
}

#[derive(Debug, Clone, Copy)]
enum UnlawfulMicrostructureLeg {
    Cheap,
    Expensive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettlementLeg {
    Up,
    Down,
    Unknown,
}

#[derive(Clone, Debug, Default)]
pub struct StrategyDecision {
    pub intents: Vec<OrderIntent>,
    pub notes: Vec<String>,
}

impl StrategyDecision {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn single(intent: OrderIntent) -> Self {
        Self {
            intents: vec![intent],
            notes: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.intents.is_empty() && self.notes.is_empty()
    }
}

fn deterministic_quote_unit(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    (value * 100_000_000.0).round() / 100_000_000.0
}

pub(crate) fn deterministic_client_order_id(
    strategy_tag: &str,
    market_id: &MarketId,
    instrument_id: &InstrumentId,
    side: TradeSide,
    reduce_only: bool,
    quote_level_tag: &str,
    price: f64,
    quantity: f64,
) -> ClientOrderId {
    let side_tag = if matches!(side, TradeSide::Buy) {
        "b"
    } else {
        "s"
    };
    let mode_tag = if reduce_only { "r" } else { "n" };
    ClientOrderId::from(format!(
        "{strategy_tag}:{market}:{instrument}:{side}:{mode}:{level}:{price:.8}:{qty:.8}",
        market = market_id.as_str(),
        instrument = instrument_id.as_str(),
        side = side_tag,
        mode = mode_tag,
        level = quote_level_tag,
        price = deterministic_quote_unit(price),
        qty = deterministic_quote_unit(quantity),
    ))
}

pub struct StrategyContext {
    pub now_ms: EpochMillis,
    pub runtime_status: RuntimeStatus,
    pub inventory: InventorySnapshot,
    pub open_orders_total: usize,
    pub open_orders_for_market: usize,
    pub market_context: Option<MarketContextRecord>,
    pub unlawful_signal: Option<UnlawfulSignalSnapshot>,
    /// BTC spot regime snapshot. Populated for every strategy.on_market_snapshot
    /// call so strategies can gate paired entries on regime (don't quote in
    /// flat-vol or strong-trending tape).
    pub btc_regime: crate::signals::BtcRegimeSnapshot,
    /// Venue-authoritative per-market rules (minimum_order_size, tick size).
    /// `None` means the runner hasn't fetched metadata yet for this market;
    /// strategy should fall back to its own defaults in that case. Once
    /// populated by `Runtime::set_venue_market_rules`, the strategy uses
    /// these values directly so we never have to keep an env knob in sync
    /// with what Polymarket actually requires.
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
    pub profile_name: String,
    pub version: Option<String>,
    pub quote: ProfileQuote,
    pub inventory: ProfileInventory,
    pub risk: ProfileRisk,
    pub fill: ProfileFill,
    pub pair: ProfilePair,
    pub economics: ProfileEconomics,
    pub health: ProfileHealth,
    pub strategies: StrategyOverrides,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl StrategyProfile {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)?;
        let profile: StrategyProfile = serde_json::from_str(&raw)?;
        Ok(profile)
    }

    pub fn quote_engine_config(&self) -> QuoteEngineConfig {
        QuoteEngineConfig {
            max_levels_per_side: self.quote.levels_per_side.unwrap_or(3),
            skew_bps: self.quote.skew_cap_bps.unwrap_or(7.5),
            stale_quote_max_age_ms: self.quote.min_quote_age_ms,
            quote_expiry_ms: self.quote.expiry_suppression_ms,
        }
    }

    pub fn unlawful_shear_config(&self) -> UnlawfulShearConfig {
        let config = UnlawfulShearConfig {
            cheap_hedge_price_max: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_CHEAP_HEDGE_PRICE_MAX",
                self.strategies.unlawful_shear.cheap_hedge_price_max,
                0.38,
            ),
            core_price_min: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_CORE_PRICE_MIN",
                self.strategies.unlawful_shear.core_price_min,
                0.52,
            ),
            core_price_max: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_CORE_PRICE_MAX",
                self.strategies.unlawful_shear.core_price_max,
                0.92,
            ),
            min_price_gap: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MIN_PRICE_GAP",
                self.strategies.unlawful_shear.min_price_gap,
                0.12,
            ),
            probe_clip_usd: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_PROBE_CLIP_USD",
                self.strategies.unlawful_shear.probe_clip_usd,
                3.0,
            ),
            core_clip_usd: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_CORE_CLIP_USD",
                self.strategies.unlawful_shear.core_clip_usd,
                20.0,
            ),
            hedge_clip_usd: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_HEDGE_CLIP_USD",
                self.strategies.unlawful_shear.hedge_clip_usd,
                6.0,
            ),
            rebalance_clip_usd: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_REBALANCE_CLIP_USD",
                self.strategies.unlawful_shear.rebalance_clip_usd,
                12.0,
            ),
            micro_clip_target_usd: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICRO_CLIP_TARGET_USD",
                self.strategies.unlawful_shear.micro_clip_target_usd,
                8.0,
            ),
            micro_clip_min_usd: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICRO_CLIP_MIN_USD",
                self.strategies.unlawful_shear.micro_clip_min_usd,
                1.0,
            ),
            micro_clip_max_children: env_or_profile_usize(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICRO_CLIP_MAX_CHILDREN",
                self.strategies.unlawful_shear.micro_clip_max_children,
                1,
            ),
            trim_clip_fraction: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_TRIM_CLIP_FRACTION",
                self.strategies.unlawful_shear.trim_clip_fraction,
                0.30,
            ),
            max_gross_cost_usd: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MAX_GROSS_COST_USD",
                self.strategies.unlawful_shear.max_gross_cost_usd,
                120.0,
            ),
            target_hedge_ratio_min: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_TARGET_HEDGE_RATIO_MIN",
                self.strategies.unlawful_shear.target_hedge_ratio_min,
                0.20,
            ),
            target_hedge_ratio_max: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_TARGET_HEDGE_RATIO_MAX",
                self.strategies.unlawful_shear.target_hedge_ratio_max,
                0.60,
            ),
            salvage_drawdown_ratio: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_SALVAGE_DRAWDOWN_RATIO",
                self.strategies.unlawful_shear.salvage_drawdown_ratio,
                0.18,
            ),
            salvage_bid_floor: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_SALVAGE_BID_FLOOR",
                self.strategies.unlawful_shear.salvage_bid_floor,
                0.05,
            ),
            max_open_orders_total: env_or_profile_usize(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MAX_OPEN_ORDERS_TOTAL",
                self.strategies.unlawful_shear.max_open_orders_total,
                6,
            ),
            taker_fee_coeff: env_or_profile_f64(
                "WHALE_PAIR_TAKER_FEE_COEFF",
                self.strategies.unlawful_shear.taker_fee_coeff,
                0.072,
            ),
            microstructure_enabled: env_or_profile_bool(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICROSTRUCTURE_ENABLED",
                self.strategies.unlawful_shear.microstructure_enabled,
                true,
            ),
            microstructure_require_depth: env_or_profile_bool(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICROSTRUCTURE_REQUIRE_DEPTH",
                self.strategies.unlawful_shear.microstructure_require_depth,
                false,
            ),
            microstructure_max_spread: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICROSTRUCTURE_MAX_SPREAD",
                self.strategies.unlawful_shear.microstructure_max_spread,
                0.05,
            ),
            microstructure_min_ask_notional_top3_usd: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICROSTRUCTURE_MIN_ASK_NOTIONAL_TOP3_USD",
                self.strategies
                    .unlawful_shear
                    .microstructure_min_ask_notional_top3_usd,
                2.0,
            ),
            microstructure_max_clip_ask_notional_fraction: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICROSTRUCTURE_MAX_CLIP_ASK_NOTIONAL_FRACTION",
                self.strategies
                    .unlawful_shear
                    .microstructure_max_clip_ask_notional_fraction,
                0.10,
            ),
            microstructure_imbalance_threshold: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICROSTRUCTURE_IMBALANCE_THRESHOLD",
                self.strategies
                    .unlawful_shear
                    .microstructure_imbalance_threshold,
                0.55,
            ),
            microstructure_weak_bid_scale: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICROSTRUCTURE_WEAK_BID_SCALE",
                self.strategies.unlawful_shear.microstructure_weak_bid_scale,
                0.65,
            ),
            microstructure_thin_ask_scale: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MICROSTRUCTURE_THIN_ASK_SCALE",
                self.strategies.unlawful_shear.microstructure_thin_ask_scale,
                0.85,
            ),
            maker_entry_pricing: env_or_profile_bool(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MAKER_ENTRY_PRICING",
                self.strategies.unlawful_shear.maker_entry_pricing,
                false,
            ),
            regime_primary_hours_utc: sorted_unique_u32_vec(
                self.strategies
                    .unlawful_shear
                    .regime_primary_hours_utc
                    .clone()
                    .unwrap_or_else(|| vec![10, 11, 19, 22, 23]),
            ),
            regime_secondary_hours_utc: sorted_unique_u32_vec(
                self.strategies
                    .unlawful_shear
                    .regime_secondary_hours_utc
                    .clone()
                    .unwrap_or_else(|| vec![10, 22]),
            ),
            allow_extreme_offhour_override: env_or_profile_bool(
                "WHALE_PAIR_UNLAWFUL_SHEAR_ALLOW_EXTREME_OFFHOUR_OVERRIDE",
                self.strategies
                    .unlawful_shear
                    .allow_extreme_offhour_override,
                false,
            ),
            entry_window_seconds: env_or_profile_u64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_ENTRY_WINDOW_SECONDS",
                self.strategies.unlawful_shear.entry_window_seconds,
                30,
            ),
            cleanup_start_seconds: env_or_profile_u64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_CLEANUP_START_SECONDS",
                self.strategies.unlawful_shear.cleanup_start_seconds,
                210,
            ),
            close_start_seconds: env_or_profile_u64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_CLOSE_START_SECONDS",
                self.strategies.unlawful_shear.close_start_seconds,
                270,
            ),
            merge_stall_seconds: env_or_profile_u64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MERGE_STALL_SECONDS",
                self.strategies.unlawful_shear.merge_stall_seconds,
                60,
            ),
            entry_book_max_age_ms: env_or_profile_u64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_ENTRY_BOOK_MAX_AGE_MS",
                self.strategies.unlawful_shear.entry_book_max_age_ms,
                1200,
            ),
            entry_btc_signal_max_age_ms: env_or_profile_u64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_ENTRY_BTC_SIGNAL_MAX_AGE_MS",
                self.strategies.unlawful_shear.entry_btc_signal_max_age_ms,
                2000,
            ),
            primary_min_btc_realized_vol_5m_bps: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_PRIMARY_MIN_BTC_REALIZED_VOL_5M_BPS",
                self.strategies
                    .unlawful_shear
                    .primary_min_btc_realized_vol_5m_bps,
                5.0,
            ),
            primary_min_btc_realized_vol_15m_bps: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_PRIMARY_MIN_BTC_REALIZED_VOL_15M_BPS",
                self.strategies
                    .unlawful_shear
                    .primary_min_btc_realized_vol_15m_bps,
                11.0,
            ),
            primary_min_btc_trade_count_5m: env_or_profile_u64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_PRIMARY_MIN_BTC_TRADE_COUNT_5M",
                self.strategies
                    .unlawful_shear
                    .primary_min_btc_trade_count_5m,
                5000,
            ),
            secondary_min_btc_realized_vol_5m_bps: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_SECONDARY_MIN_BTC_REALIZED_VOL_5M_BPS",
                self.strategies
                    .unlawful_shear
                    .secondary_min_btc_realized_vol_5m_bps,
                8.0,
            ),
            secondary_min_btc_realized_vol_15m_bps: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_SECONDARY_MIN_BTC_REALIZED_VOL_15M_BPS",
                self.strategies
                    .unlawful_shear
                    .secondary_min_btc_realized_vol_15m_bps,
                15.0,
            ),
            secondary_min_btc_trade_count_5m: env_or_profile_u64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_SECONDARY_MIN_BTC_TRADE_COUNT_5M",
                self.strategies
                    .unlawful_shear
                    .secondary_min_btc_trade_count_5m,
                8000,
            ),
            override_min_btc_realized_vol_5m_bps: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_OVERRIDE_MIN_BTC_REALIZED_VOL_5M_BPS",
                self.strategies
                    .unlawful_shear
                    .override_min_btc_realized_vol_5m_bps,
                12.0,
            ),
            override_min_btc_realized_vol_15m_bps: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_OVERRIDE_MIN_BTC_REALIZED_VOL_15M_BPS",
                self.strategies
                    .unlawful_shear
                    .override_min_btc_realized_vol_15m_bps,
                20.0,
            ),
            override_min_btc_trade_count_5m: env_or_profile_u64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_OVERRIDE_MIN_BTC_TRADE_COUNT_5M",
                self.strategies
                    .unlawful_shear
                    .override_min_btc_trade_count_5m,
                10000,
            ),
            entry_cheap_ask_max: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_ENTRY_CHEAP_ASK_MAX",
                self.strategies.unlawful_shear.entry_cheap_ask_max,
                0.47,
            ),
            entry_expensive_ask_min: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_ENTRY_EXPENSIVE_ASK_MIN",
                self.strategies.unlawful_shear.entry_expensive_ask_min,
                0.56,
            ),
            entry_expensive_ask_max: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_ENTRY_EXPENSIVE_ASK_MAX",
                self.strategies.unlawful_shear.entry_expensive_ask_max,
                0.84,
            ),
            entry_price_gap_min: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_ENTRY_PRICE_GAP_MIN",
                self.strategies.unlawful_shear.entry_price_gap_min,
                0.22,
            ),
            preferred_cheap_ask_max: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_PREFERRED_CHEAP_ASK_MAX",
                self.strategies.unlawful_shear.preferred_cheap_ask_max,
                0.40,
            ),
            preferred_expensive_ask_min: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_PREFERRED_EXPENSIVE_ASK_MIN",
                self.strategies.unlawful_shear.preferred_expensive_ask_min,
                0.62,
            ),
            preferred_expensive_ask_max: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_PREFERRED_EXPENSIVE_ASK_MAX",
                self.strategies.unlawful_shear.preferred_expensive_ask_max,
                0.78,
            ),
            preferred_price_gap_min: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_PREFERRED_PRICE_GAP_MIN",
                self.strategies.unlawful_shear.preferred_price_gap_min,
                0.35,
            ),
            hard_shock_return_30s_bps: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_HARD_SHOCK_RETURN_30S_BPS",
                self.strategies.unlawful_shear.hard_shock_return_30s_bps,
                25.0,
            ),
            hard_shock_return_60s_bps: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_HARD_SHOCK_RETURN_60S_BPS",
                self.strategies.unlawful_shear.hard_shock_return_60s_bps,
                30.0,
            ),
            soft_shock_return_30s_bps: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_SOFT_SHOCK_RETURN_30S_BPS",
                self.strategies.unlawful_shear.soft_shock_return_30s_bps,
                15.0,
            ),
            soft_shock_return_60s_bps: env_or_profile_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_SOFT_SHOCK_RETURN_60S_BPS",
                self.strategies.unlawful_shear.soft_shock_return_60s_bps,
                20.0,
            ),
        };

        normalize_unlawful_invariants(config)
    }

    pub fn goat_pair_config(&self) -> GoatPairConfig {
        GoatPairConfig {
            accumulate_price_max: env_or_profile_f64(
                "WHALE_PAIR_ACCUMULATE_PRICE_MAX",
                self.strategies.goat_pair.accumulate_price_max,
                0.50,
            ),
            aggressive_price_max: env_or_profile_f64(
                "WHALE_PAIR_AGGRESSIVE_PRICE_MAX",
                self.strategies.goat_pair.aggressive_price_max,
                0.10,
            ),
            base_clip_usd: env_or_profile_f64(
                "WHALE_PAIR_BASE_CLIP_USD",
                self.strategies.goat_pair.base_clip_usd,
                20.0,
            ),
            aggressive_clip_usd: env_or_profile_f64(
                "WHALE_PAIR_AGGRESSIVE_CLIP_USD",
                self.strategies.goat_pair.aggressive_clip_usd,
                50.0,
            ),
            max_gross_cost_usd: env_or_profile_f64(
                "WHALE_PAIR_MAX_GROSS_COST_USD",
                self.strategies.goat_pair.max_gross_cost_usd,
                200.0,
            ),
            completion_min_pnl_per_share: env_or_profile_f64(
                "WHALE_PAIR_COMPLETION_MIN_PNL_PER_SHARE",
                self.strategies.goat_pair.completion_min_pnl_per_share,
                0.002,
            ),
            max_imbalance_ratio: env_or_profile_f64(
                "WHALE_PAIR_MAX_IMBALANCE_RATIO",
                self.strategies.goat_pair.max_imbalance_ratio,
                3.0,
            ),
            taker_fee_coeff: env_or_profile_f64(
                "WHALE_PAIR_TAKER_FEE_COEFF",
                self.strategies.goat_pair.taker_fee_coeff,
                0.072,
            ),
        }
    }

    pub fn goat_pair_cooldown_ms(&self) -> u64 {
        env_or_profile_u64(
            "WHALE_PAIR_GOAT_PAIR_COOLDOWN_MS",
            self.strategies.goat_pair.cooldown_ms,
            250,
        )
    }

    pub fn unlawful_shear_cooldown_ms(&self) -> u64 {
        env_or_profile_u64(
            "WHALE_PAIR_UNLAWFUL_SHEAR_COOLDOWN_MS",
            self.strategies.unlawful_shear.cooldown_ms,
            400,
        )
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileQuote {
    pub levels_per_side: Option<usize>,
    pub base_clip_usd: Option<f64>,
    pub max_quote_per_side_usd: Option<f64>,
    pub max_clip_usd: Option<f64>,
    pub min_edge_bps: Option<f64>,
    pub inventory_skew_bps: Option<f64>,
    pub min_quote_age_ms: Option<u64>,
    pub expiry_suppression_ms: Option<u64>,
    pub refresh_interval_ms: Option<u64>,
    pub skew_cap_bps: Option<f64>,
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
    pub pair_merge_min_qty: Option<f64>,
    pub cleanup_min_qty: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileRisk {
    pub book_stale_ms: Option<u64>,
    pub user_ws_stale_ms: Option<u64>,
    pub max_consecutive_reconcile_failures: Option<usize>,
    pub disable_making_on_spot_shock_bps: Option<f64>,
    pub kill_switch_inventory_skew_usd: Option<f64>,
    pub kill_switch_open_orders_total: Option<usize>,
    pub kill_switch_quote_age_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileFill {
    pub maker_rebate_bps: Option<f64>,
    pub taker_fee_bps: Option<f64>,
    pub max_fill_notional_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfilePair {
    pub merge_min_qty: Option<f64>,
    pub cleanup_min_qty: Option<f64>,
    pub expiry_phase_cutoff_ms: Option<u64>,
    pub stale_feed_cutoff_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileEconomics {
    pub target_edge_bps: Option<f64>,
    pub max_fees_usd: Option<f64>,
    pub max_rebates_usd: Option<f64>,
    pub target_net_edge_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileHealth {
    pub market_ws_stale_ms: Option<u64>,
    pub user_ws_stale_ms: Option<u64>,
    pub execution_adapter_stale_ms: Option<u64>,
    pub health_check_interval_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StrategyOverrides {
    pub goat_pair: GoatPairProfile,
    pub unlawful_shear: UnlawfulShearProfile,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GoatPairProfile {
    pub accumulate_price_max: Option<f64>,
    pub aggressive_price_max: Option<f64>,
    pub base_clip_usd: Option<f64>,
    pub aggressive_clip_usd: Option<f64>,
    pub max_gross_cost_usd: Option<f64>,
    pub completion_min_pnl_per_share: Option<f64>,
    pub max_imbalance_ratio: Option<f64>,
    pub taker_fee_coeff: Option<f64>,
    pub cooldown_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UnlawfulShearProfile {
    pub cheap_hedge_price_max: Option<f64>,
    pub core_price_min: Option<f64>,
    pub core_price_max: Option<f64>,
    pub min_price_gap: Option<f64>,
    pub probe_clip_usd: Option<f64>,
    pub core_clip_usd: Option<f64>,
    pub hedge_clip_usd: Option<f64>,
    pub rebalance_clip_usd: Option<f64>,
    pub micro_clip_target_usd: Option<f64>,
    pub micro_clip_min_usd: Option<f64>,
    pub micro_clip_max_children: Option<usize>,
    pub trim_clip_fraction: Option<f64>,
    pub max_gross_cost_usd: Option<f64>,
    pub target_hedge_ratio_min: Option<f64>,
    pub target_hedge_ratio_max: Option<f64>,
    pub salvage_drawdown_ratio: Option<f64>,
    pub salvage_bid_floor: Option<f64>,
    pub max_open_orders_total: Option<usize>,
    pub taker_fee_coeff: Option<f64>,
    pub microstructure_enabled: Option<bool>,
    pub microstructure_require_depth: Option<bool>,
    pub microstructure_max_spread: Option<f64>,
    pub microstructure_min_ask_notional_top3_usd: Option<f64>,
    pub microstructure_max_clip_ask_notional_fraction: Option<f64>,
    pub microstructure_imbalance_threshold: Option<f64>,
    pub microstructure_weak_bid_scale: Option<f64>,
    pub microstructure_thin_ask_scale: Option<f64>,
    pub maker_entry_pricing: Option<bool>,
    pub cooldown_ms: Option<u64>,

    pub regime_primary_hours_utc: Option<Vec<u32>>,
    pub regime_secondary_hours_utc: Option<Vec<u32>>,
    pub allow_extreme_offhour_override: Option<bool>,
    pub entry_window_seconds: Option<u64>,
    pub cleanup_start_seconds: Option<u64>,
    pub close_start_seconds: Option<u64>,
    pub merge_stall_seconds: Option<u64>,
    pub entry_book_max_age_ms: Option<u64>,
    pub entry_btc_signal_max_age_ms: Option<u64>,
    pub primary_min_btc_realized_vol_5m_bps: Option<f64>,
    pub primary_min_btc_realized_vol_15m_bps: Option<f64>,
    pub primary_min_btc_trade_count_5m: Option<u64>,
    pub secondary_min_btc_realized_vol_5m_bps: Option<f64>,
    pub secondary_min_btc_realized_vol_15m_bps: Option<f64>,
    pub secondary_min_btc_trade_count_5m: Option<u64>,
    pub override_min_btc_realized_vol_5m_bps: Option<f64>,
    pub override_min_btc_realized_vol_15m_bps: Option<f64>,
    pub override_min_btc_trade_count_5m: Option<u64>,
    pub entry_cheap_ask_max: Option<f64>,
    pub entry_expensive_ask_min: Option<f64>,
    pub entry_expensive_ask_max: Option<f64>,
    pub entry_price_gap_min: Option<f64>,
    pub preferred_cheap_ask_max: Option<f64>,
    pub preferred_expensive_ask_min: Option<f64>,
    pub preferred_expensive_ask_max: Option<f64>,
    pub preferred_price_gap_min: Option<f64>,
    pub hard_shock_return_30s_bps: Option<f64>,
    pub hard_shock_return_60s_bps: Option<f64>,
    pub soft_shock_return_30s_bps: Option<f64>,
    pub soft_shock_return_60s_bps: Option<f64>,
}

pub trait Strategy {
    fn name(&self) -> &str;

    fn unlawful_gate_config(&self) -> Option<UnlawfulGateConfig> {
        None
    }

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

    fn on_fill(
        &mut self,
        _context: &StrategyContext,
        _fill: &crate::types::FillReport,
    ) -> StrategyDecision {
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

#[derive(Debug)]
pub enum StrategyMode {
    Goat(GoatPairStrategy),
    Btc5mMm(Btc5mMmStrategy),
    UnlawfulShear(UnlawfulShearStrategy),
    Bonereaper(crate::strategy_bonereaper::BonereaperStrategy),
    Noop(NoopStrategy),
}

impl StrategyMode {
    pub fn from_name(name: &str, profile: Option<&StrategyProfile>) -> Self {
        match name {
            "btc_5m_mm" => Self::Btc5mMm(Btc5mMmStrategy::with_defaults()),
            "goat_pair" => Self::Goat(GoatPairStrategy::with_profile(profile)),
            "noop" => Self::Noop(NoopStrategy),
            "unlawful_shear" => Self::UnlawfulShear(UnlawfulShearStrategy::with_profile(profile)),
            "bonereaper" => {
                Self::Bonereaper(crate::strategy_bonereaper::BonereaperStrategy::with_defaults())
            }
            _ => Self::UnlawfulShear(UnlawfulShearStrategy::with_defaults()),
        }
    }

    pub fn taker_fee_coeff(&self) -> f64 {
        match self {
            Self::Btc5mMm(strategy) => strategy.taker_fee_coeff(),
            Self::Goat(strategy) => strategy.taker_fee_coeff(),
            Self::UnlawfulShear(strategy) => strategy.taker_fee_coeff(),
            Self::Bonereaper(strategy) => strategy.taker_fee_coeff(),
            Self::Noop(_) => 0.0,
        }
    }

    pub fn unlawful_gate_config(&self) -> Option<UnlawfulGateConfig> {
        match self {
            Self::Btc5mMm(_) => None,
            Self::Goat(_) => None,
            Self::Noop(_) => None,
            Self::Bonereaper(_) => None,
            Self::UnlawfulShear(strategy) => Some(strategy.unlawful_gate_config()),
        }
    }
}

impl Strategy for StrategyMode {
    fn name(&self) -> &str {
        match self {
            Self::Btc5mMm(strategy) => strategy.name(),
            Self::Goat(strategy) => strategy.name(),
            Self::UnlawfulShear(strategy) => strategy.name(),
            Self::Bonereaper(strategy) => strategy.name(),
            Self::Noop(strategy) => strategy.name(),
        }
    }

    fn unlawful_gate_config(&self) -> Option<UnlawfulGateConfig> {
        StrategyMode::unlawful_gate_config(self)
    }

    fn on_start(&mut self, context: &StrategyContext) -> StrategyDecision {
        match self {
            Self::Btc5mMm(strategy) => strategy.on_start(context),
            Self::Goat(strategy) => strategy.on_start(context),
            Self::UnlawfulShear(strategy) => strategy.on_start(context),
            Self::Bonereaper(strategy) => strategy.on_start(context),
            Self::Noop(strategy) => strategy.on_start(context),
        }
    }

    fn on_market_snapshot(
        &mut self,
        context: &StrategyContext,
        snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        match self {
            Self::Btc5mMm(strategy) => strategy.on_market_snapshot(context, snapshot),
            Self::Goat(strategy) => strategy.on_market_snapshot(context, snapshot),
            Self::UnlawfulShear(strategy) => strategy.on_market_snapshot(context, snapshot),
            Self::Bonereaper(strategy) => strategy.on_market_snapshot(context, snapshot),
            Self::Noop(strategy) => strategy.on_market_snapshot(context, snapshot),
        }
    }

    fn on_fill(
        &mut self,
        context: &StrategyContext,
        fill: &crate::types::FillReport,
    ) -> StrategyDecision {
        match self {
            Self::Btc5mMm(strategy) => strategy.on_fill(context, fill),
            Self::Goat(strategy) => strategy.on_fill(context, fill),
            Self::UnlawfulShear(strategy) => strategy.on_fill(context, fill),
            Self::Bonereaper(strategy) => strategy.on_fill(context, fill),
            Self::Noop(strategy) => strategy.on_fill(context, fill),
        }
    }

    fn checkpoint_state(&self) -> Option<serde_json::Value> {
        match self {
            Self::Btc5mMm(strategy) => strategy.checkpoint_state(),
            Self::Goat(strategy) => strategy.checkpoint_state(),
            Self::UnlawfulShear(strategy) => strategy.checkpoint_state(),
            Self::Bonereaper(strategy) => strategy.checkpoint_state(),
            Self::Noop(strategy) => strategy.checkpoint_state(),
        }
    }

    fn restore_checkpoint_state(
        &mut self,
        state: &serde_json::Value,
    ) -> std::result::Result<(), String> {
        match self {
            Self::Btc5mMm(strategy) => strategy.restore_checkpoint_state(state),
            Self::Goat(strategy) => strategy.restore_checkpoint_state(state),
            Self::UnlawfulShear(strategy) => strategy.restore_checkpoint_state(state),
            Self::Bonereaper(strategy) => strategy.restore_checkpoint_state(state),
            Self::Noop(strategy) => strategy.restore_checkpoint_state(state),
        }
    }
}

#[derive(Debug)]
pub struct Btc5mMmStrategy {
    config: Btc5mMmConfig,
    market_states: HashMap<MarketId, Btc5mMmMarketState>,
    /// Timestamps of recent fills (any leg, any market). Pruned to the
    /// last 5 minutes. Drives fill-rate-aware sizing: when fills are coming
    /// in, scale clip up; when none, scale down. Mirrors how whales
    /// compound via frequency on rewarding tape.
    recent_fill_times: VecDeque<EpochMillis>,
}

impl Btc5mMmStrategy {
    const NO_QUOTE_NOTE_INTERVAL_MS: u64 = 15_000;
    /// 5-minute rolling window for fill-rate-aware sizing.
    const FILL_WINDOW_MS: EpochMillis = 5 * 60 * 1_000;
    /// Last-resort premium-price circuit breaker. This is not the edge model;
    /// it only prevents obviously bad single-level premium fills while
    /// market-local flow state decides most entries.
    /// Market-local repricing gate window. The MAX_MOVE threshold itself is
    /// env-tunable on `Btc5mMmConfig` (was a const; promoted 2026-04-29).
    const MARKET_MID_TREND_WINDOW_MS: u64 = 30_000;
    /// Recent own-entry fill symmetry policy. These are engine invariants, not
    /// operator tuning knobs: balanced fills can keep quoting; lopsided fills
    /// put this market into Cooling before we re-enter the same flow.
    const ASYMMETRIC_FILL_WINDOW_MS: u64 = 120_000;
    const RESCUE_INFLIGHT_TTL_MS: u64 = 15_000;
    const MAX_RESCUE_ATTEMPTS_PER_SIGNATURE: u32 = 3;
    /// Polymarket V2 rejects marketable BUY orders below $1 notional. Keep this
    /// as a protocol invariant so stale live env cannot emit invalid rescues.
    const MARKETABLE_BUY_MIN_NOTIONAL_USD: f64 = 1.0;
    const ASYMMETRIC_FILL_MODERATE_COOLDOWN_MS: u64 = 30_000;
    const ASYMMETRIC_FILL_SEVERE_COOLDOWN_MS: u64 = 90_000;
    const ASYMMETRIC_FILL_MODERATE_SYMMETRY: f64 = 0.65;
    const ASYMMETRIC_FILL_SEVERE_SYMMETRY: f64 = 0.35;
    const ASYMMETRIC_FILL_MIN_TOTAL_QTY: f64 = 10.0;
    const CONVEX_ACCUMULATION_MAX_BID: f64 = 0.45;
    const CONVEX_ACCUMULATION_MAX_AVG_COST: f64 = 0.55;
    /// Convex accumulation gets a smaller slice of the per-market budget than
    /// paired entry. Paired bidding is the rebate workhorse and should consume
    /// the configured caps; convex is the asymmetric-payoff side bet that
    /// shouldn't blow our bankroll on cheap-leg fades. With a 0.5 fraction,
    /// a $20 gross / $10 leg market budget gives convex $10 / $5 to work with.
    const CONVEX_BUDGET_FRACTION: f64 = 0.5;
    /// Convex_accum should not fire late in the bar. The cheap-leg bet only
    /// pays off if the market reverses, which needs time. Sub-60s remaining
    /// = high conviction the bar resolves as currently-leading side =
    /// near-certain loss on the cheap leg. Skip.
    const CONVEX_MIN_BAR_REMAINING_MS: u64 = 60_000;
    /// Convex_accum should not fire too early in the bar either. First 60s
    /// of a 5min bar is noise: both sides typically near $0.50 (no clear
    /// cheap side), brief spikes can momentarily push prices to <$0.45 but
    /// usually mean-revert before the bar resolves. Wait for the market to
    /// settle into a directional view before betting on its reversal.
    const CONVEX_MIN_BAR_ELAPSED_MS: u64 = 60_000;
    /// Per-bar count cap on convex accumulation bids per market. Even with
    /// dollar caps + time-in-bar gate + trend persistence gate, fire-and-
    /// forget refresh on every book tick can produce 10+ bids per bar.
    /// 4 fires per bar gives 4 chances at the asymmetric payoff while
    /// bounding cumulative damage if signals all happen to be wrong.
    const CONVEX_MAX_BIDS_PER_BAR: u32 = 4;
    /// Fractional Kelly keeps convex accumulation proportional to measured
    /// edge instead of forcing the venue minimum on every eligible tick.
    const CONVEX_FRACTIONAL_KELLY: f64 = 0.25;
    /// Absolute bankroll slice for a single convex clip. This protects small
    /// live bankrolls from turning a tiny theoretical edge into an oversized
    /// minimum-order bet.
    const CONVEX_MAX_KELLY_BANKROLL_FRACTION: f64 = 0.03;
    /// 180s rolling BTC return magnitude that flags a "persistent trend".
    /// Tuned to match the smallest spot move that consistently produces
    /// >5pp Polymarket book repricing in a single 5min bar. Below this,
    /// noise and short-term mean reversion dominate; above, the trend is
    /// real and bidding the cheap (against-trend) leg is adverse selection.
    /// Reverted 2026-04-29 from 150 → 50 after whale Apr 28 data showed
    /// cheap-leg accumulation is only 3.5% of his total notional (mostly
    /// incidental, not strategic). Loosening to 150 would have us buying
    /// losing lottery tickets when whale doesn't. Convex stays as a heavily-
    /// gated side bet, not a primary strategy.
    const CONVEX_TREND_PERSISTENCE_BPS: f64 = 50.0;
    // JUSTIFY: Asymmetric Core+Hedge V1 spec (2026-04-29): late-bar path
    // only targets venue-priced favored legs in the 0.85-0.97 band.
    const LATE_BAR_CORE_PRICE_FLOOR: f64 = 0.85;
    // V1 was 0.97; bumped 2026-04-29 to 0.98 after whale 6-day data showed
    // 1069 trades at $0.98 vs only 124 at $0.99 — natural cliff is between
    // $0.98 and $0.99. Captures ~7% more late-bar opportunities at slightly
    // lower per-share margin.
    const LATE_BAR_CORE_PRICE_CEILING: f64 = 0.98;
    // JUSTIFY: Asymmetric Core+Hedge V1 spec (2026-04-29): avoid <30s
    // remaining because maker queue/settlement race risk is too high.
    const LATE_BAR_CORE_TIME_REMAINING_MS_MIN: u64 = 30_000;
    // JUSTIFY: Asymmetric Core+Hedge V1 spec (2026-04-29): avoid early-bar
    // accumulation and keep this path focused on near-resolution convergence.
    const LATE_BAR_CORE_TIME_REMAINING_MS_MAX: u64 = 120_000;
    // JUSTIFY: Asymmetric Core+Hedge V1 spec (2026-04-29): late-bar core
    // only runs in sufficiently active bars with realized volatility support.
    const LATE_BAR_CORE_MIN_VOL_BPS: f64 = 50.0;
    // JUSTIFY: Asymmetric Core+Hedge V1 spec (2026-04-29): require minimum
    // directional confirmation from spot vs price_to_beat.
    const LATE_BAR_CORE_MOMENTUM_FLOOR_BPS: f64 = 5.0;
    // JUSTIFY: Asymmetric Core+Hedge V1 spec (2026-04-29): per-market, per-
    // bar spend cap for late-bar accumulation, isolated from convex budget.
    const LATE_BAR_CORE_BUDGET_USD: f64 = 5.0;
    // V1 was 4; bumped 2026-04-29 to 15 after whale data showed 20+ fills
    // legging through $0.85-$0.97 within ~20s windows. Capital cap
    // (LATE_BAR_CORE_BUDGET_USD) still bounds total exposure; this just
    // allows full legging behavior when budget supports it.
    const LATE_BAR_CORE_MAX_BIDS_PER_BAR: u32 = 15;
    // JUSTIFY: Asymmetric Core+Hedge V1 spec (2026-04-29): late-bar orders
    // are fill-or-expire maker quotes with short GTD lifetime.
    pub(crate) const LATE_BAR_CORE_TTL_MS: u64 = 60_000;
    const HOLD_EV_MARGIN: f64 = 0.005;
    const HOLD_MIN_EDGE: f64 = 0.005;
    const LATE_BAR_FAIR_BLEND_WINDOW_MS: u64 = 90_000;
    const LATE_BAR_HOLD_CONFIDENCE_FAIR: f64 = 0.70;
    const MARKET_STATE_TTL_MS: u64 = 6 * 60 * 60 * 1_000;
    const MAX_MARKET_STATES: usize = 512;
    const PERSISTED_STATE_VERSION: u32 = 1;
    const MAX_RECENT_FILL_TIMES: usize = 20_000;

    pub fn new(config: Btc5mMmConfig) -> Self {
        Self {
            config,
            market_states: HashMap::new(),
            recent_fill_times: VecDeque::new(),
        }
    }

    /// Trim recent_fill_times to the last FILL_WINDOW_MS.
    fn prune_fill_window(&mut self, now_ms: EpochMillis) {
        let cutoff = now_ms.saturating_sub(Self::FILL_WINDOW_MS);
        while self.recent_fill_times.front().is_some_and(|t| *t < cutoff) {
            self.recent_fill_times.pop_front();
        }
        while self.recent_fill_times.len() > Self::MAX_RECENT_FILL_TIMES {
            self.recent_fill_times.pop_front();
        }
    }

    fn prune_market_states(&mut self, context: &StrategyContext, active_market_id: &MarketId) {
        let inventory_markets = context
            .inventory
            .positions
            .iter()
            .filter(|position| position.quantity.abs() > 1e-9)
            .map(|position| position.market_id.clone())
            .collect::<HashSet<_>>();
        let cutoff = context.now_ms.saturating_sub(Self::MARKET_STATE_TTL_MS);
        self.market_states.retain(|market_id, state| {
            market_id == active_market_id
                || inventory_markets.contains(market_id)
                || state.last_seen_ms() >= cutoff
        });

        if self.market_states.len() <= Self::MAX_MARKET_STATES {
            return;
        }

        let mut removable = self
            .market_states
            .iter()
            .filter(|(market_id, _)| {
                *market_id != active_market_id && !inventory_markets.contains(*market_id)
            })
            .map(|(market_id, state)| (market_id.clone(), state.last_seen_ms()))
            .collect::<Vec<_>>();
        removable.sort_by_key(|(_, last_seen)| *last_seen);

        let overflow = self.market_states.len() - Self::MAX_MARKET_STATES;
        for (market_id, _) in removable.into_iter().take(overflow) {
            self.market_states.remove(&market_id);
        }
    }

    fn persisted_state(&self) -> Btc5mMmPersistedState {
        Btc5mMmPersistedState {
            version: Self::PERSISTED_STATE_VERSION,
            market_states: self
                .market_states
                .iter()
                .map(|(market_id, state)| Btc5mMmPersistedMarketState {
                    market_id: market_id.as_str().to_string(),
                    mode: state.mode.clone(),
                    market_mid_history: state.market_mid_history.iter().copied().collect(),
                    recent_fills: state
                        .recent_fills
                        .iter()
                        .map(|(ts, instrument_id, qty)| {
                            (*ts, instrument_id.as_str().to_string(), *qty)
                        })
                        .collect(),
                    asymmetric_entry_block_until_ms: state.asymmetric_entry_block_until_ms,
                    last_action_ms: state.last_action_ms,
                    last_no_quote_note_ms: state.last_no_quote_note_ms,
                    last_rescue_attempt_ms: state.last_rescue_attempt_ms,
                    rescue_state: state.rescue_state.clone(),
                    last_fill_ms: state.last_fill_ms,
                })
                .collect(),
            recent_fill_times: self.recent_fill_times.iter().copied().collect(),
        }
    }

    fn restore_persisted_state(&mut self, persisted: Btc5mMmPersistedState) {
        self.market_states.clear();
        for record in persisted
            .market_states
            .into_iter()
            .filter(|record| !record.market_id.trim().is_empty())
            .take(Self::MAX_MARKET_STATES)
        {
            let market_id = MarketId::from(record.market_id);
            self.market_states.insert(
                market_id,
                Btc5mMmMarketState {
                    mode: record.mode,
                    // Quotes are intentionally not persisted: after restart
                    // the engine must rebuild them from fresh venue books.
                    quotes: HashMap::new(),
                    market_mid_history: record.market_mid_history.into_iter().collect(),
                    recent_fills: record
                        .recent_fills
                        .into_iter()
                        .map(|(ts, instrument_id, qty)| {
                            (ts, InstrumentId::from(instrument_id), qty)
                        })
                        .collect(),
                    asymmetric_entry_block_until_ms: record.asymmetric_entry_block_until_ms,
                    last_action_ms: record.last_action_ms,
                    last_no_quote_note_ms: record.last_no_quote_note_ms,
                    last_cooling_note_ms: None,
                    last_cooling_note_key: None,
                    last_rescue_attempt_ms: record.last_rescue_attempt_ms,
                    rescue_state: record.rescue_state,
                    last_fill_ms: record.last_fill_ms,
                    // Per-bar convex tracking is in-memory only; new bar
                    // post-restart resets count naturally (curr_bar_end !=
                    // None at restart, persisted None means "no recent
                    // convex this bar yet").
                    convex_bids_this_bar: 0,
                    convex_bar_end_ms: None,
                    late_bar_core_bids_this_bar: 0,
                    late_bar_core_bar_end_ms: None,
                    late_bar_core_spend_this_bar_usd: 0.0,
                },
            );
        }
        self.recent_fill_times = persisted
            .recent_fill_times
            .into_iter()
            .take(Self::MAX_RECENT_FILL_TIMES)
            .collect();
    }

    /// Returns clip-scaling multiplier — currently HARDCODED to 1.0x
    /// after 2026-04-27 bleed analysis. The original 1.0x → 2.0x scaling
    /// compounded both good AND bad fill streaks (asymmetric one-sided
    /// fills are common in trending tape; scaling up made bleeds bigger).
    /// Keeping the field for future re-enable when we have separate
    /// "good fill" detection (paired both legs) vs "bad fill" (one-sided).
    fn fill_rate_clip_scale(&self, _now_ms: EpochMillis) -> f64 {
        1.0
    }

    pub fn with_defaults() -> Self {
        Self::new(Btc5mMmConfig::from_env())
    }

    pub fn taker_fee_coeff(&self) -> f64 {
        self.config.taker_fee_coeff
    }

    fn position_for(
        inventory: &InventorySnapshot,
        market_id: &MarketId,
        instrument_id: &InstrumentId,
    ) -> (f64, f64, Option<f64>) {
        inventory
            .positions
            .iter()
            .find(|position| {
                &position.market_id == market_id && &position.instrument_id == instrument_id
            })
            .map_or((0.0, 0.0, None), |position| {
                (position.quantity, position.avg_price, position.mark_price)
            })
    }

    fn gross_cost_usd(inventory: &InventorySnapshot, market_id: &MarketId) -> f64 {
        inventory
            .positions
            .iter()
            .filter(|position| &position.market_id == market_id)
            .map(|position| position.quantity.abs() * position.avg_price)
            .sum()
    }

    fn inventory_equity_usd(inventory: &InventorySnapshot) -> f64 {
        (inventory.total_cash_usd + inventory.gross_exposure_usd)
            .max(inventory.free_cash_usd)
            .max(0.0)
    }

    fn effective_cost_cap_usd(absolute_cap_usd: f64, bps_cap: f64, equity_usd: f64) -> f64 {
        let bps_cap_usd = if bps_cap > 0.0 && equity_usd > 0.0 {
            Some(equity_usd * (bps_cap / 10_000.0).clamp(0.0, 1.0))
        } else {
            None
        };
        bps_cap_usd
            .map(|cap| absolute_cap_usd.min(cap))
            .unwrap_or(absolute_cap_usd)
            .max(0.01)
    }

    fn effective_entry_caps_usd(&self, inventory: &InventorySnapshot) -> (f64, f64) {
        let equity_usd = Self::inventory_equity_usd(inventory);
        let free_cash_cap = if self.config.max_entry_free_cash_bps > 0.0 {
            inventory.free_cash_usd * (self.config.max_entry_free_cash_bps / 10_000.0)
        } else {
            f64::INFINITY
        };
        (
            Self::effective_cost_cap_usd(
                self.config.max_gross_cost_usd,
                self.config.max_gross_cost_bps,
                equity_usd,
            )
            .min(free_cash_cap)
            .max(0.01),
            Self::effective_cost_cap_usd(
                self.config.max_leg_cost_usd,
                self.config.max_leg_cost_bps,
                equity_usd,
            )
            .min(free_cash_cap)
            .max(0.01),
        )
    }

    fn rescue_free_cash_cap_usd(&self, inventory: &InventorySnapshot) -> f64 {
        if self.config.max_rescue_free_cash_bps <= 0.0 {
            return inventory.free_cash_usd.max(0.0);
        }
        (inventory.free_cash_usd * (self.config.max_rescue_free_cash_bps / 10_000.0)).max(0.0)
    }

    fn best_bid(quote: &QuoteSnapshot) -> Option<f64> {
        quote
            .best_bid
            .as_ref()
            .map(|level| level.price)
            .filter(|price| price.is_finite() && *price > 0.0)
    }

    fn best_ask(quote: &QuoteSnapshot) -> Option<f64> {
        quote
            .best_ask
            .as_ref()
            .map(|level| level.price)
            .filter(|price| price.is_finite() && *price > 0.0)
    }

    fn floor_to_tick(value: f64, tick: f64) -> f64 {
        if value <= 0.0 || tick <= 0.0 {
            return 0.0;
        }
        deterministic_quote_unit(((value + 1e-9) / tick).floor() * tick)
    }

    fn tick_size(&self, venue_rules: Option<&VenueMarketRules>) -> f64 {
        venue_rules
            .map(|r| r.minimum_tick_size)
            .filter(|t| t.is_finite() && *t > 0.0)
            .unwrap_or(self.config.maker_price_tick)
    }

    fn maker_bid_price(
        &self,
        quote: &QuoteSnapshot,
        max_bid: f64,
        venue_rules: Option<&VenueMarketRules>,
    ) -> Option<f64> {
        let best_bid = Self::best_bid(quote)?;
        let best_ask = Self::best_ask(quote)?;
        let tick = self.tick_size(venue_rules);
        let maker_cap = best_ask - tick * self.config.maker_safety_ticks;
        let price = Self::floor_to_tick(best_bid.min(max_bid).min(maker_cap), tick);
        (price >= 0.01 && price < best_ask).then_some(price)
    }

    fn maker_bid_price_at_level(
        &self,
        quote: &QuoteSnapshot,
        venue_rules: Option<&VenueMarketRules>,
        level_index: usize,
    ) -> Option<f64> {
        let best_bid = Self::best_bid(quote)?;
        let best_ask = Self::best_ask(quote)?;
        let tick = self.tick_size(venue_rules);
        // Reverted adaptive safety after 2026-04-27 bleed analysis.
        // Adaptive safety=1 on cheap leg got queue position 0 but combined
        // with extreme-book entries kept filling us on whichever leg was
        // rallying. Uniform safety_ticks is more defensive: trades a bit
        // of fill rate for not paying premium prices on rallying leg.
        let maker_cap = best_ask - tick * self.config.maker_safety_ticks;
        let level_offset = tick * self.config.entry_ladder_spacing_ticks * level_index as f64;
        let price = Self::floor_to_tick(best_bid.min(maker_cap) - level_offset, tick);
        (price >= 0.01 && price < best_ask).then_some(price)
    }

    fn top_notional(levels: &[BookLevel], take: usize) -> f64 {
        levels
            .iter()
            .take(take)
            .filter(|level| level.price.is_finite() && level.quantity.is_finite())
            .filter(|level| level.price > 0.0 && level.quantity > 0.0)
            .map(|level| level.price * level.quantity)
            .sum()
    }

    fn dynamic_bid_clip_usd(&self, quote: &QuoteSnapshot, requested_clip_usd: f64) -> f64 {
        let visible_bid_notional = Self::top_notional(&quote.bid_levels, 3);
        let liquidity_cap =
            if visible_bid_notional > 0.0 && self.config.liquidity_clip_fraction > 0.0 {
                visible_bid_notional * self.config.liquidity_clip_fraction
            } else {
                self.config.max_clip_usd
            };
        requested_clip_usd
            .min(self.config.max_clip_usd)
            .min(liquidity_cap)
            .max(self.config.min_clip_usd)
    }

    fn paired_entry_clip_usd(
        &self,
        left_quote: &QuoteSnapshot,
        right_quote: &QuoteSnapshot,
        requested_clip_usd: f64,
    ) -> f64 {
        self.dynamic_bid_clip_usd(left_quote, requested_clip_usd)
            .min(self.dynamic_bid_clip_usd(right_quote, requested_clip_usd))
    }

    fn paired_entry_quantity(
        &self,
        market_id: &MarketId,
        left_quote: &QuoteSnapshot,
        right_quote: &QuoteSnapshot,
        left_bid_price: f64,
        right_bid_price: f64,
        requested_clip_usd: f64,
    ) -> Option<f64> {
        let clip_reference_price = left_bid_price.max(right_bid_price);
        let min_notional_reference_price = left_bid_price.min(right_bid_price);
        if clip_reference_price <= 0.0
            || !clip_reference_price.is_finite()
            || min_notional_reference_price <= 0.0
            || !min_notional_reference_price.is_finite()
        {
            return None;
        }
        let clip_usd = self.paired_entry_clip_usd(left_quote, right_quote, requested_clip_usd);
        let raw_quantity = clip_usd / clip_reference_price;
        let required_quantity = self.required_order_quantity(min_notional_reference_price);
        let max_quantity = self.config.max_clip_usd / clip_reference_price;
        if required_quantity > max_quantity + 1e-9 {
            info!(
                target: "strategy.sizing",
                market = %market_id,
                requested_clip_usd,
                clip_usd,
                clip_reference_price,
                min_notional_reference_price,
                raw_quantity,
                required_quantity,
                max_quantity,
                outcome = "rejected_required_exceeds_max",
                "paired entry sizing aborted"
            );
            return None;
        }
        let final_quantity = raw_quantity
            .max(required_quantity)
            .min(max_quantity)
            .max(0.0);
        info!(
            target: "strategy.sizing",
            market = %market_id,
            requested_clip_usd,
            depth_capped_clip_usd = clip_usd,
            min_floor_qty = required_quantity,
            risk_cap_qty = max_quantity,
            raw_quantity,
            final_quantity,
            left_bid_price,
            right_bid_price,
            outcome = "sized",
            "paired entry sizing decision"
        );
        Some(final_quantity)
    }

    fn suppress_paired_by_flow_imbalance(
        &self,
        market_id: &MarketId,
        left_id: &InstrumentId,
        left_quote: &QuoteSnapshot,
        left_fair: f64,
        right_id: &InstrumentId,
        right_quote: &QuoteSnapshot,
        right_fair: f64,
    ) -> bool {
        if self.config.order_flow_imbalance_threshold <= 0.0 {
            return false;
        }
        let (expensive_leg, buy_qty_60s, sell_qty_60s) = if left_fair >= right_fair {
            (
                left_id,
                left_quote.taker_buy_qty_60s,
                left_quote.taker_sell_qty_60s,
            )
        } else {
            (
                right_id,
                right_quote.taker_buy_qty_60s,
                right_quote.taker_sell_qty_60s,
            )
        };
        let total_qty_60s = buy_qty_60s + sell_qty_60s;
        if total_qty_60s <= 1e-9 {
            return false;
        }
        let imbalance = (buy_qty_60s - sell_qty_60s) / total_qty_60s;
        if imbalance.abs() >= self.config.order_flow_imbalance_threshold {
            info!(
                target: "strategy.flow_imbalance",
                market = %market_id,
                expensive_leg = %expensive_leg,
                expensive_leg_buy_qty_60s = buy_qty_60s,
                expensive_leg_sell_qty_60s = sell_qty_60s,
                expensive_leg_imbalance = imbalance,
                threshold = self.config.order_flow_imbalance_threshold,
                left_fair,
                right_fair,
                "paired entry suppressed by order-flow imbalance signal"
            );
            return true;
        }
        false
    }

    fn required_order_quantity(&self, reference_price: f64) -> f64 {
        let min_notional_quantity = if reference_price > 0.0 {
            self.config.min_order_notional_usd / reference_price
        } else {
            f64::INFINITY
        };
        self.config
            .min_order_quantity
            .max(self.config.venue_min_order_quantity)
            .max(min_notional_quantity)
    }

    fn actionable_inventory_min_quantity(&self, venue_rules: Option<&VenueMarketRules>) -> f64 {
        venue_rules
            .map(|rules| rules.minimum_order_size)
            .filter(|quantity| quantity.is_finite() && *quantity > 1e-9)
            .unwrap_or(self.config.venue_min_order_quantity)
            .max(1e-9)
    }

    fn convex_kelly_budget_usd(&self, fair: f64, bid_price: f64, bankroll_usd: f64) -> f64 {
        if !fair.is_finite()
            || !bid_price.is_finite()
            || !bankroll_usd.is_finite()
            || fair <= bid_price
            || bid_price <= 0.0
            || bid_price >= 1.0
            || bankroll_usd <= 0.0
        {
            return 0.0;
        }
        let full_kelly_fraction = ((fair - bid_price) / (1.0 - bid_price)).clamp(0.0, 1.0);
        let fractional_kelly =
            (full_kelly_fraction * Self::CONVEX_FRACTIONAL_KELLY).clamp(0.0, 1.0);
        bankroll_usd * fractional_kelly.min(Self::CONVEX_MAX_KELLY_BANKROLL_FRACTION)
    }

    fn rescue_inflight_ttl_ms(&self) -> u64 {
        Self::RESCUE_INFLIGHT_TTL_MS.max(self.config.cooldown_ms.saturating_mul(10))
    }

    fn rescue_qty_bucket(quantity: f64) -> u64 {
        (quantity.max(0.0) * 100.0).round() as u64
    }

    fn can_emit_rescue(
        &mut self,
        market_id: &MarketId,
        _stranded_instrument_id: &InstrumentId,
        _lift_instrument_id: &InstrumentId,
        now_ms: EpochMillis,
        notes: &mut Vec<String>,
    ) -> bool {
        let ttl_ms = self.rescue_inflight_ttl_ms();
        let state = self.market_states.entry(market_id.clone()).or_default();
        let Some(rescue) = state.rescue_state.as_ref() else {
            return true;
        };
        if rescue.attempts >= Self::MAX_RESCUE_ATTEMPTS_PER_SIGNATURE {
            notes.push("rescue attempt cap reached".to_string());
            return false;
        }
        if now_ms.saturating_sub(rescue.last_attempt_ms) < ttl_ms {
            notes.push("rescue already in flight".to_string());
            return false;
        }
        true
    }

    fn record_rescue_attempt(
        &mut self,
        market_id: &MarketId,
        stranded_instrument_id: &InstrumentId,
        lift_instrument_id: &InstrumentId,
        stranded_qty: f64,
        now_ms: EpochMillis,
    ) {
        let qty_bucket = Self::rescue_qty_bucket(stranded_qty);
        let state = self.market_states.entry(market_id.clone()).or_default();
        let next_attempts = state
            .rescue_state
            .as_ref()
            .map(|rescue| rescue.attempts.saturating_add(1))
            .unwrap_or(1);
        let first_attempt_ms = state
            .rescue_state
            .as_ref()
            .map(|rescue| rescue.first_attempt_ms)
            .unwrap_or(now_ms);
        state.rescue_state = Some(Btc5mMmRescueState {
            stranded_instrument_id: stranded_instrument_id.as_str().to_string(),
            lift_instrument_id: lift_instrument_id.as_str().to_string(),
            stranded_qty_bucket: qty_bucket,
            attempts: next_attempts,
            first_attempt_ms,
            last_attempt_ms: now_ms,
        });
        state.last_rescue_attempt_ms = Some(now_ms);
    }

    fn clear_rescue_state(&mut self, market_id: &MarketId) {
        if let Some(state) = self.market_states.get_mut(market_id) {
            state.rescue_state = None;
        }
    }

    fn quote_is_usable(&self, quote: &QuoteSnapshot) -> bool {
        let (Some(bid), Some(ask)) = (Self::best_bid(quote), Self::best_ask(quote)) else {
            return false;
        };
        if ask <= bid || ask - bid > self.config.max_spread {
            return false;
        }
        let bid_depth = Self::top_notional(&quote.bid_levels, 3);
        let ask_depth = Self::top_notional(&quote.ask_levels, 3);
        bid_depth >= self.config.min_top_depth_notional_usd
            && ask_depth >= self.config.min_top_depth_notional_usd
    }

    fn no_quote_decision(
        &mut self,
        market_id: &MarketId,
        now_ms: EpochMillis,
        reason: String,
    ) -> StrategyDecision {
        let state = self.market_states.entry(market_id.clone()).or_default();
        if state
            .last_no_quote_note_ms
            .is_some_and(|last_ms| now_ms.saturating_sub(last_ms) < Self::NO_QUOTE_NOTE_INTERVAL_MS)
        {
            return StrategyDecision::none();
        }
        state.last_no_quote_note_ms = Some(now_ms);
        StrategyDecision {
            intents: Vec::new(),
            notes: vec![format!("btc-5m-mm no quote: {reason}")],
        }
    }

    fn should_log_cooling_note(
        &mut self,
        market_id: &MarketId,
        now_ms: EpochMillis,
        reason: &str,
    ) -> bool {
        let state = self.market_states.entry(market_id.clone()).or_default();
        let reason_key = Self::cooling_reason_key(reason);
        let reason_changed = state.last_cooling_note_key != Some(reason_key);
        let interval_elapsed = state.last_cooling_note_ms.is_none_or(|last_ms| {
            now_ms.saturating_sub(last_ms) >= Self::NO_QUOTE_NOTE_INTERVAL_MS
        });

        if !reason_changed && !interval_elapsed {
            return false;
        }

        state.last_cooling_note_ms = Some(now_ms);
        state.last_cooling_note_key = Some(reason_key);
        true
    }

    fn cooling_reason_key(reason: &str) -> &'static str {
        if reason.starts_with("market mid moved") {
            "market_mid_moved"
        } else if reason.starts_with("premium fair cap") {
            "premium_fair_cap"
        } else if reason.starts_with("btc regime flat") {
            "btc_regime_flat"
        } else if reason.starts_with("btc regime trending") {
            "btc_regime_trending"
        } else if reason.starts_with("asymmetric entry-fill cooldown") {
            "asymmetric_entry_fill_cooldown"
        } else {
            "other"
        }
    }

    fn cooling_allows_convex_accumulation(reason: &str) -> bool {
        reason.starts_with("market mid moved")
            || reason.starts_with("premium fair cap")
            || reason.starts_with("btc regime trending")
    }

    fn quote_health(quote: &QuoteSnapshot) -> String {
        let bid = Self::best_bid(quote)
            .map(|price| format!("{price:.4}"))
            .unwrap_or_else(|| "na".to_string());
        let ask = Self::best_ask(quote)
            .map(|price| format!("{price:.4}"))
            .unwrap_or_else(|| "na".to_string());
        let spread = match (Self::best_bid(quote), Self::best_ask(quote)) {
            (Some(bid), Some(ask)) if ask >= bid => format!("{:.4}", ask - bid),
            _ => "na".to_string(),
        };
        format!(
            "bid={bid} ask={ask} spread={spread} bid_depth3={:.2} ask_depth3={:.2}",
            Self::top_notional(&quote.bid_levels, 3),
            Self::top_notional(&quote.ask_levels, 3)
        )
    }

    fn bid_health(
        &self,
        quote: &QuoteSnapshot,
        fair: f64,
        leg_cost: f64,
        gross_cost: f64,
        edge_bps: f64,
        venue_rules: Option<&VenueMarketRules>,
    ) -> String {
        let best_bid = Self::best_bid(quote).unwrap_or(0.0);
        let max_bid = deterministic_quote_unit(
            (fair - edge_bps / 10_000.0 - self.inventory_skew(leg_cost, gross_cost))
                .clamp(0.0, 0.99),
        );
        let maker_bid = self
            .maker_bid_price(quote, max_bid, venue_rules)
            .map(|price| format!("{price:.4}"))
            .unwrap_or_else(|| "none".to_string());
        format!(
            "fair={fair:.4} best_bid={best_bid:.4} max_bid={max_bid:.4} maker_bid={maker_bid} leg_cost={leg_cost:.2}"
        )
    }

    fn fair_values(
        &self,
        left_id: &InstrumentId,
        left: &QuoteSnapshot,
        right_id: &InstrumentId,
        right: &QuoteSnapshot,
        btc_regime: &crate::signals::BtcRegimeSnapshot,
        market_context: Option<&MarketContextRecord>,
        now_ms: EpochMillis,
    ) -> Option<(f64, f64)> {
        let left_mid = (Self::best_bid(left)? + Self::best_ask(left)?) * 0.5;
        let right_mid = (Self::best_bid(right)? + Self::best_ask(right)?) * 0.5;

        // Spot momentum tilt: when BTC is moving, the leg whose outcome
        // benefits should fair higher than the book mid suggests, since
        // the book lags spot by 50-500ms. Without this we quote symmetrically
        // around stale mid and get adversely selected on every directional
        // tick. Strength is intentionally small (so book signal still
        // dominates) and capped to bound damage from bad data.
        //
        // Identify UP leg via gamma's instrument_ids ordering: index 0 is
        // the YES/UP outcome by Polymarket convention. If we can't identify,
        // skip the tilt entirely (fall back to pure book mid).
        let tilt_left =
            if let (Some(ctx), Some(return_bps)) = (market_context, btc_regime.return_60s_bps) {
                if return_bps.is_finite()
                    && ctx.instrument_ids.len() >= 2
                    && self.config.momentum_tilt_per_bps > 0.0
                {
                    let up_id = ctx.instrument_ids[0].as_str();
                    let raw = (return_bps * self.config.momentum_tilt_per_bps).clamp(
                        -self.config.momentum_max_tilt,
                        self.config.momentum_max_tilt,
                    );
                    if up_id == left_id.as_str() {
                        raw
                    } else if up_id == right_id.as_str() {
                        -raw
                    } else {
                        0.0
                    }
                } else {
                    0.0
                }
            } else {
                0.0
            };
        let left_biased = (left_mid + tilt_left).clamp(0.001, 0.999);
        let right_biased = (right_mid - tilt_left).clamp(0.001, 0.999);
        let sum = left_biased + right_biased;
        if sum.is_finite() && sum > 0.0 {
            let book_fairs = (left_biased / sum, right_biased / sum);
            if let Some((settlement_left, settlement_right, weight)) = self
                .late_bar_settlement_fairs(left_id, right_id, btc_regime, market_context, now_ms)
            {
                Some((
                    (book_fairs.0 * (1.0 - weight) + settlement_left * weight).clamp(0.001, 0.999),
                    (book_fairs.1 * (1.0 - weight) + settlement_right * weight).clamp(0.001, 0.999),
                ))
            } else {
                Some(book_fairs)
            }
        } else {
            None
        }
    }

    fn leg_is_up(
        instrument_id: &InstrumentId,
        market_context: Option<&MarketContextRecord>,
    ) -> Option<bool> {
        if let Some(ctx) = market_context {
            if ctx
                .instrument_ids
                .first()
                .is_some_and(|up_id| up_id == instrument_id.as_str())
            {
                return Some(true);
            }
            if ctx
                .instrument_ids
                .get(1)
                .is_some_and(|down_id| down_id == instrument_id.as_str())
            {
                return Some(false);
            }
        }
        let id = instrument_id.as_str().to_ascii_lowercase();
        if id.contains("up") || id.contains("long") || id.contains("bull") {
            Some(true)
        } else if id.contains("down") || id.contains("short") || id.contains("bear") {
            Some(false)
        } else {
            None
        }
    }

    fn time_remaining_ms(
        market_context: Option<&MarketContextRecord>,
        now_ms: EpochMillis,
    ) -> Option<u64> {
        market_context
            .and_then(|ctx| ctx.event_end_time_ms)
            .map(|end_ms| end_ms.saturating_sub(now_ms))
    }

    fn late_bar_settlement_fairs(
        &self,
        left_id: &InstrumentId,
        right_id: &InstrumentId,
        btc_regime: &crate::signals::BtcRegimeSnapshot,
        market_context: Option<&MarketContextRecord>,
        now_ms: EpochMillis,
    ) -> Option<(f64, f64, f64)> {
        let ctx = market_context?;
        let remaining_ms = Self::time_remaining_ms(Some(ctx), now_ms)?;
        if remaining_ms > Self::LATE_BAR_FAIR_BLEND_WINDOW_MS {
            return None;
        }
        let spot = btc_regime.last_price?;
        let price_to_beat = ctx.price_to_beat?;
        if spot <= 0.0 || price_to_beat <= 0.0 {
            return None;
        }
        let left_is_up = Self::leg_is_up(left_id, Some(ctx))?;
        let right_is_up = Self::leg_is_up(right_id, Some(ctx))?;
        if left_is_up == right_is_up {
            return None;
        }
        let distance_bps = ((spot / price_to_beat) - 1.0) * 10_000.0;
        if !distance_bps.is_finite() {
            return None;
        }
        let vol_scale = btc_regime.realized_vol_5m_bps.unwrap_or(25.0).max(5.0);
        let time_scale = ((remaining_ms as f64 / Self::LATE_BAR_FAIR_BLEND_WINDOW_MS as f64)
            .sqrt())
        .clamp(0.25, 1.0);
        let score = (distance_bps / (vol_scale * time_scale).max(5.0)).clamp(-6.0, 6.0);
        let p_up = (1.0 / (1.0 + (-score).exp())).clamp(0.02, 0.98);
        let weight = (1.0 - remaining_ms as f64 / Self::LATE_BAR_FAIR_BLEND_WINDOW_MS as f64)
            .clamp(0.0, 0.80);
        let left_fair = if left_is_up { p_up } else { 1.0 - p_up };
        let right_fair = if right_is_up { p_up } else { 1.0 - p_up };
        Some((left_fair, right_fair, weight))
    }

    fn normalized_left_mid(left: &QuoteSnapshot, right: &QuoteSnapshot) -> Option<f64> {
        let left_mid = left.mid_price()?;
        let right_mid = right.mid_price()?;
        let sum = left_mid + right_mid;
        if sum.is_finite() && sum > 0.0 {
            Some((left_mid / sum).clamp(0.001, 0.999))
        } else {
            None
        }
    }

    fn observe_market_mid_move(
        &mut self,
        market_id: &MarketId,
        now_ms: EpochMillis,
        left_quote: &QuoteSnapshot,
        right_quote: &QuoteSnapshot,
    ) -> Option<f64> {
        if Self::MARKET_MID_TREND_WINDOW_MS == 0 {
            return None;
        }
        let left_mid = Self::normalized_left_mid(left_quote, right_quote)?;
        let state = self.market_states.entry(market_id.clone()).or_default();
        state.market_mid_history.push_back((now_ms, left_mid));
        while state
            .market_mid_history
            .front()
            .is_some_and(|(ts, _)| now_ms.saturating_sub(*ts) > Self::MARKET_MID_TREND_WINDOW_MS)
        {
            state.market_mid_history.pop_front();
        }
        let mut min_mid = left_mid;
        let mut max_mid = left_mid;
        for (_, mid) in &state.market_mid_history {
            min_mid = min_mid.min(*mid);
            max_mid = max_mid.max(*mid);
        }
        Some(max_mid - min_mid)
    }

    fn record_market_fill_asymmetry(
        &mut self,
        market_id: &MarketId,
        instrument_id: &InstrumentId,
        quantity: f64,
        now_ms: EpochMillis,
    ) -> Option<(f64, f64)> {
        if Self::ASYMMETRIC_FILL_WINDOW_MS == 0
            || (Self::ASYMMETRIC_FILL_MODERATE_COOLDOWN_MS == 0
                && Self::ASYMMETRIC_FILL_SEVERE_COOLDOWN_MS == 0)
            || Self::ASYMMETRIC_FILL_MIN_TOTAL_QTY <= 0.0
        {
            return None;
        }
        let state = self.market_states.entry(market_id.clone()).or_default();
        state
            .recent_fills
            .push_back((now_ms, instrument_id.clone(), quantity.max(0.0)));
        while state
            .recent_fills
            .front()
            .is_some_and(|(ts, _, _)| now_ms.saturating_sub(*ts) > Self::ASYMMETRIC_FILL_WINDOW_MS)
        {
            state.recent_fills.pop_front();
        }

        let mut by_instrument: HashMap<InstrumentId, f64> = HashMap::new();
        for (_, id, qty) in &state.recent_fills {
            *by_instrument.entry(id.clone()).or_default() += *qty;
        }
        if by_instrument.len() < 2 {
            let total: f64 = by_instrument.values().sum();
            if total >= Self::ASYMMETRIC_FILL_MIN_TOTAL_QTY {
                let cooldown_ms = Self::ASYMMETRIC_FILL_SEVERE_COOLDOWN_MS;
                state.asymmetric_entry_block_until_ms = Some(now_ms.saturating_add(cooldown_ms));
                return Some((0.0, total));
            }
            return None;
        }

        let min_qty = by_instrument
            .values()
            .fold(f64::INFINITY, |acc, qty| acc.min(*qty));
        let max_qty = by_instrument
            .values()
            .fold(0.0_f64, |acc, qty| acc.max(*qty));
        let total_qty: f64 = by_instrument.values().sum();
        if max_qty <= 0.0 || total_qty < Self::ASYMMETRIC_FILL_MIN_TOTAL_QTY {
            return None;
        }
        let symmetry = (min_qty / max_qty).clamp(0.0, 1.0);
        let cooldown_ms = if symmetry < Self::ASYMMETRIC_FILL_SEVERE_SYMMETRY {
            Self::ASYMMETRIC_FILL_SEVERE_COOLDOWN_MS
        } else if symmetry < Self::ASYMMETRIC_FILL_MODERATE_SYMMETRY {
            Self::ASYMMETRIC_FILL_MODERATE_COOLDOWN_MS
        } else {
            0
        };
        if cooldown_ms > 0 {
            state.asymmetric_entry_block_until_ms = Some(now_ms.saturating_add(cooldown_ms));
            Some((symmetry, total_qty))
        } else {
            None
        }
    }

    fn is_own_entry_fill(fill: &crate::types::FillReport) -> bool {
        fill.side == TradeSide::Buy
            && fill.close_method.is_none()
            && fill
                .client_order_id
                .as_ref()
                .is_some_and(|client_order_id| {
                    let raw = client_order_id.as_str();
                    raw.starts_with("btc-5m-mm:")
                        && (raw.contains(":mm-paired-bid:")
                            || raw.contains(":mm-convex-accum:")
                            || raw.contains(":mm-late-bar-core:"))
                        && !raw.contains(":mm-hedge-rescue:")
                })
    }

    fn transition_market_mode(
        &mut self,
        market_id: &MarketId,
        now_ms: EpochMillis,
        has_inventory: bool,
        left_fair: f64,
        right_fair: f64,
        btc_regime: &crate::signals::BtcRegimeSnapshot,
        market_mid_move: Option<f64>,
    ) -> Btc5mMmMarketMode {
        // 2026-04-29 cleanup: removed post-fill cooldown, asymmetric-entry
        // cooldown, mid-trend cooling, and premium-fair-cap gates. Whale data
        // shows none of these patterns; whale fills repeatedly within seconds
        // and bids on volatile bars at $0.85+. The remaining gates are:
        //   - has_inventory (manage existing position)
        //   - per-leg ENTRY_PREMIUM_BID_CAP inside build_paired_entry_ladder
        //   - capital caps (max_leg_cost, max_gross_cost) in risk engine
        let _ = (left_fair, right_fair, btc_regime, market_mid_move);
        let next_mode = if has_inventory {
            Btc5mMmMarketMode::ManagingInventory
        } else {
            Btc5mMmMarketMode::Ready
        };
        if let Some(state) = self.market_states.get_mut(market_id) {
            state.mode = next_mode.clone();
        }
        next_mode
    }

    fn inventory_skew(&self, leg_cost: f64, gross_cost: f64) -> f64 {
        if self.config.inventory_skew_bps <= 0.0 {
            return 0.0;
        }
        let leg_pressure = (leg_cost / self.config.max_leg_cost_usd).clamp(0.0, 1.5);
        let gross_pressure = (gross_cost / self.config.max_gross_cost_usd).clamp(0.0, 1.5);
        (leg_pressure + gross_pressure) * self.config.inventory_skew_bps / 10_000.0
    }

    fn max_bid_for(&self, fair: f64, leg_cost: f64, gross_cost: f64, edge_bps: f64) -> f64 {
        deterministic_quote_unit(
            (fair - edge_bps / 10_000.0 - self.inventory_skew(leg_cost, gross_cost))
                .clamp(0.0, 0.99),
        )
    }

    fn candidate_ladder_bid_price(
        &self,
        quote: &QuoteSnapshot,
        fair: f64,
        leg_cost: f64,
        gross_cost: f64,
        edge_bps: f64,
        venue_rules: Option<&VenueMarketRules>,
        level_index: usize,
    ) -> Option<f64> {
        let best_bid = Self::best_bid(quote)?;
        let max_bid = self.max_bid_for(fair, leg_cost, gross_cost, edge_bps);
        if best_bid <= 0.0 || max_bid <= 0.0 || (level_index == 0 && best_bid > max_bid) {
            return None;
        }
        let price = self.maker_bid_price_at_level(quote, venue_rules, level_index)?;
        (price <= max_bid + 1e-9).then_some(price)
    }

    fn taker_fee_per_share(&self, price: f64) -> f64 {
        if !price.is_finite() || price <= 0.0 {
            return 0.0;
        }
        price * self.config.taker_fee_coeff * price * (1.0 - price)
    }

    fn decide_stranded_exposure(
        &self,
        held_id: &InstrumentId,
        held_fair: f64,
        avg_cost: f64,
        stranded_qty: f64,
        opposite_quote: &QuoteSnapshot,
        market_context: Option<&MarketContextRecord>,
        now_ms: EpochMillis,
    ) -> Btc5mMmExposureDecision {
        if !avg_cost.is_finite() || avg_cost <= 0.0 {
            return Btc5mMmExposureDecision {
                rescue_qty: 0.0,
                hold_qty: stranded_qty.max(0.0),
                reason: format!(
                    "hold stranded unknown cost basis leg={} fair={held_fair:.4} avg_cost={avg_cost:.4} hold_qty={:.4}",
                    held_id,
                    stranded_qty.max(0.0)
                ),
                hold_ev_per_share: f64::NAN,
                rescue_ev_per_share: None,
                held_fair,
                avg_cost,
            };
        }
        let hold_ev = held_fair - avg_cost;
        let merge_gas_per_share = if stranded_qty > 0.0 {
            self.config.merge_gas_cost_usd / stranded_qty
        } else {
            0.0
        };
        let rescue_ev = Self::best_ask(opposite_quote)
            .map(|ask| 1.0 - avg_cost - ask - self.taker_fee_per_share(ask) - merge_gas_per_share);
        let remaining_ms = Self::time_remaining_ms(market_context, now_ms);
        let late_confident = remaining_ms.is_some_and(|remaining| remaining <= 60_000)
            && held_fair >= Self::LATE_BAR_HOLD_CONFIDENCE_FAIR
            && held_fair >= avg_cost + Self::HOLD_MIN_EDGE * 2.0
            && avg_cost <= self.config.entry_premium_bid_cap;
        let cheap_positive =
            avg_cost <= Self::CONVEX_ACCUMULATION_MAX_AVG_COST && hold_ev >= Self::HOLD_MIN_EDGE;
        let beats_rescue = rescue_ev
            .map(|ev| hold_ev > ev + Self::HOLD_EV_MARGIN)
            .unwrap_or(hold_ev >= Self::HOLD_MIN_EDGE);
        // Hold is preferred whenever it beats rescue OR rescue is unavailable.
        // The cheap_positive / late_confident flags only set the QUANTITY cap,
        // not whether to hold. Previously, those flags also gated the hold
        // decision, which forced rescue into guaranteed-loss territory whenever
        // the position was just above the cheap-leg band. Strategy preference
        // now: never actively pay to lock in a worse outcome than holding
        // would produce (gas + taker fee + slippage already baked into
        // rescue_ev above).
        let should_hold = beats_rescue;
        let convex_hold_qty_cap = if avg_cost > 0.0 {
            if cheap_positive || late_confident {
                // Strong-thesis hold (cheap leg or late-bar confident): allow
                // the larger convex-budget cap.
                (self.config.max_leg_cost_usd * Self::CONVEX_BUDGET_FRACTION / avg_cost).max(0.0)
            } else {
                // Default hold (rescue would be EV-worse but no convex thesis):
                // cap at standard leg budget so we don't accumulate unbounded
                // expensive inventory just because rescue_ev happens to be
                // marginally negative.
                (self.config.max_leg_cost_usd / avg_cost).max(0.0)
            }
        } else {
            0.0
        };
        let hold_qty = if should_hold {
            stranded_qty.min(convex_hold_qty_cap)
        } else {
            0.0
        };
        let rescue_qty = (stranded_qty - hold_qty).max(0.0);
        let reason = if rescue_qty <= 1e-9 {
            format!(
                "hold stranded positive-asymmetry leg={} fair={held_fair:.4} avg_cost={avg_cost:.4} hold_ev={hold_ev:.4} rescue_ev={rescue_ev:?} hold_qty={hold_qty:.4}",
                held_id
            )
        } else if hold_qty > 1e-9 {
            format!(
                "partial rescue stranded leg={} fair={held_fair:.4} avg_cost={avg_cost:.4} hold_ev={hold_ev:.4} rescue_ev={rescue_ev:?} rescue_qty={rescue_qty:.4} hold_qty={hold_qty:.4}",
                held_id
            )
        } else {
            format!(
                "rescue stranded leg={} fair={held_fair:.4} avg_cost={avg_cost:.4} hold_ev={hold_ev:.4} rescue_ev={rescue_ev:?} rescue_qty={rescue_qty:.4}",
                held_id
            )
        };
        Btc5mMmExposureDecision {
            rescue_qty,
            hold_qty,
            reason,
            hold_ev_per_share: hold_ev,
            rescue_ev_per_share: rescue_ev,
            held_fair,
            avg_cost,
        }
    }

    /// Returns Some(reason) if convex accumulation should be skipped this
    /// tick. None means "go ahead and call build_convex_accumulation_intent".
    /// Gates checked (cheapest first, abort early):
    ///   1. time-in-bar (>= 60s remaining)
    ///   2. bar-just-opened (>= 60s elapsed since bar start)
    ///   3. trend-persistent (180s + 120s BTC return both against cheap leg
    ///      with magnitude > CONVEX_TREND_PERSISTENCE_BPS)
    ///   4. per-bar bid count (<= CONVEX_MAX_BIDS_PER_BAR for this bar)
    fn late_bar_core_remaining_budget_usd(
        &self,
        market_id: &MarketId,
        curr_bar_end: Option<EpochMillis>,
    ) -> f64 {
        if let Some(state) = self.market_states.get(market_id) {
            let same_bar = matches!(
                (state.late_bar_core_bar_end_ms, curr_bar_end),
                (Some(a), Some(b)) if a == b
            );
            if same_bar {
                return (Self::LATE_BAR_CORE_BUDGET_USD - state.late_bar_core_spend_this_bar_usd)
                    .max(0.0);
            }
        }
        Self::LATE_BAR_CORE_BUDGET_USD
    }

    /// Returns Some(reason) if late-bar expensive-leg accumulation should be
    /// skipped this tick. None means "go ahead and call
    /// build_late_bar_core_intent".
    fn late_bar_core_skip_reason(
        &self,
        market_id: &MarketId,
        expensive_leg_id: &InstrumentId,
        expensive_leg_quote: &QuoteSnapshot,
        btc_regime: &crate::signals::BtcRegimeSnapshot,
        market_context: Option<&MarketContextRecord>,
        now_ms: EpochMillis,
        inventory: &InventorySnapshot,
    ) -> Option<String> {
        let Some(ctx) = market_context else {
            return Some("late-bar-core skip: market context unavailable".to_string());
        };
        let Some(remaining_ms) = Self::time_remaining_ms(Some(ctx), now_ms) else {
            return Some("late-bar-core skip: bar timing unavailable".to_string());
        };
        if remaining_ms < Self::LATE_BAR_CORE_TIME_REMAINING_MS_MIN {
            return Some(format!(
                "late-bar-core skip: too late, remaining={}ms",
                remaining_ms
            ));
        }
        if remaining_ms > Self::LATE_BAR_CORE_TIME_REMAINING_MS_MAX {
            return Some(format!(
                "late-bar-core skip: too early, remaining={}ms",
                remaining_ms
            ));
        }

        let Some(ask) = Self::best_ask(expensive_leg_quote) else {
            return Some("late-bar-core skip: missing best ask".to_string());
        };
        if ask < Self::LATE_BAR_CORE_PRICE_FLOOR {
            return Some(format!("late-bar-core skip: ask {ask:.4} below floor"));
        }
        if ask > Self::LATE_BAR_CORE_PRICE_CEILING {
            return Some(format!("late-bar-core skip: ask {ask:.4} above ceiling"));
        }

        let vol_bps = btc_regime.realized_vol_5m_bps.unwrap_or(0.0);
        if vol_bps < Self::LATE_BAR_CORE_MIN_VOL_BPS {
            return Some(format!("late-bar-core skip: vol {vol_bps:.1}bps below min"));
        }

        let Some(leg_is_up) = Self::leg_is_up(expensive_leg_id, Some(ctx)) else {
            return Some("late-bar-core skip: unable to infer leg direction".to_string());
        };
        let Some(spot) = btc_regime.last_price else {
            return Some("late-bar-core skip: no spot/price_to_beat".to_string());
        };
        let Some(price_to_beat) = ctx.price_to_beat else {
            return Some("late-bar-core skip: no spot/price_to_beat".to_string());
        };
        if spot <= 0.0 || price_to_beat <= 0.0 {
            return Some("late-bar-core skip: no spot/price_to_beat".to_string());
        }
        let direction_bps = ((spot / price_to_beat) - 1.0) * 10_000.0;
        let confirmed = if leg_is_up {
            direction_bps >= Self::LATE_BAR_CORE_MOMENTUM_FLOOR_BPS
        } else {
            direction_bps <= -Self::LATE_BAR_CORE_MOMENTUM_FLOOR_BPS
        };
        if !confirmed {
            return Some(format!(
                "late-bar-core skip: direction not confirmed (leg_is_up={leg_is_up} direction_bps={direction_bps:.1})"
            ));
        }

        let curr_bar_end = ctx.event_end_time_ms;
        if let Some(state) = self.market_states.get(market_id) {
            let same_bar = matches!(
                (state.late_bar_core_bar_end_ms, curr_bar_end),
                (Some(a), Some(b)) if a == b
            );
            if same_bar {
                if state.late_bar_core_bids_this_bar >= Self::LATE_BAR_CORE_MAX_BIDS_PER_BAR {
                    return Some(format!(
                        "late-bar-core skip: bar count cap reached ({} bids)",
                        state.late_bar_core_bids_this_bar
                    ));
                }
                if state.late_bar_core_spend_this_bar_usd >= Self::LATE_BAR_CORE_BUDGET_USD {
                    return Some(format!(
                        "late-bar-core skip: bar budget reached (${:.2})",
                        state.late_bar_core_spend_this_bar_usd
                    ));
                }
            }
        }

        for position in &inventory.positions {
            if position.market_id == *market_id
                && position.instrument_id == *expensive_leg_id
                && position.quantity > 1e-9
                && position.avg_price > 0.80
            {
                return Some(format!(
                    "late-bar-core skip: avg_cost {:.4} too high to add",
                    position.avg_price
                ));
            }
        }

        None
    }

    fn convex_skip_reason(
        &self,
        market_id: &MarketId,
        left_id: &InstrumentId,
        left_fair: f64,
        right_id: &InstrumentId,
        right_fair: f64,
        btc_regime: &crate::signals::BtcRegimeSnapshot,
        market_context: Option<&MarketContextRecord>,
        now_ms: EpochMillis,
    ) -> Option<String> {
        // Bar timing gates: need market_context with event_end_time_ms.
        if let Some(ctx) = market_context {
            if let Some(end_ms) = ctx.event_end_time_ms {
                let remaining = end_ms.saturating_sub(now_ms);
                if remaining < Self::CONVEX_MIN_BAR_REMAINING_MS {
                    return Some(format!(
                        "convex skip: time-in-bar {remaining}ms remaining < {}ms",
                        Self::CONVEX_MIN_BAR_REMAINING_MS
                    ));
                }
                // Estimate elapsed assuming a 5min bar window. (TODO: thread
                // bar_window_ms from config.market_discovery_window once
                // strategy can read it; for now hardcoded to match BTC 5m.)
                let bar_duration_ms: u64 = 5 * 60 * 1_000;
                let elapsed = bar_duration_ms.saturating_sub(remaining);
                if elapsed < Self::CONVEX_MIN_BAR_ELAPSED_MS {
                    return Some(format!(
                        "convex skip: bar-just-opened {elapsed}ms elapsed < {}ms",
                        Self::CONVEX_MIN_BAR_ELAPSED_MS
                    ));
                }
            }
        }
        // Trend persistence gate: need both 180s and 120s return readings.
        let cheap_id = if left_fair < right_fair {
            left_id
        } else {
            right_id
        };
        if let (Some(r180), Some(r120)) = (btc_regime.return_180s_bps, btc_regime.return_120s_bps) {
            if let Some(cheap_is_up) = Self::leg_is_up(cheap_id, market_context) {
                let trend_against_cheap = if cheap_is_up {
                    r180 < -Self::CONVEX_TREND_PERSISTENCE_BPS && r120 < 0.0
                } else {
                    r180 > Self::CONVEX_TREND_PERSISTENCE_BPS && r120 > 0.0
                };
                if trend_against_cheap {
                    return Some(format!(
                        "convex skip: trend-persistent r180={:.1}bps r120={:.1}bps cheap_is_up={cheap_is_up}",
                        r180, r120
                    ));
                }
            }
        }
        // Per-bar bid count cap.
        if let Some(state) = self.market_states.get(market_id) {
            let curr_bar_end = market_context.and_then(|c| c.event_end_time_ms);
            let same_bar = matches!(
                (state.convex_bar_end_ms, curr_bar_end),
                (Some(a), Some(b)) if a == b
            );
            if same_bar && state.convex_bids_this_bar >= Self::CONVEX_MAX_BIDS_PER_BAR {
                return Some(format!(
                    "convex skip: bid-count-cap {} >= {} this bar",
                    state.convex_bids_this_bar,
                    Self::CONVEX_MAX_BIDS_PER_BAR
                ));
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn build_late_bar_core_intent(
        &self,
        market_id: &MarketId,
        expensive_leg_id: &InstrumentId,
        expensive_leg_quote: &QuoteSnapshot,
        expensive_leg_fair: f64,
        expensive_leg_cost: f64,
        gross_cost: f64,
        max_gross_cost_usd: f64,
        max_leg_cost_usd: f64,
        remaining_bar_budget_usd: f64,
        remaining_ms: u64,
        venue_rules: Option<&VenueMarketRules>,
        now_ms: EpochMillis,
    ) -> Option<OrderIntent> {
        let best_bid = Self::best_bid(expensive_leg_quote)?;
        if best_bid < Self::LATE_BAR_CORE_PRICE_FLOOR {
            return None;
        }
        let tick = self.tick_size(venue_rules);
        let venue_min_qty = venue_rules
            .map(|rules| rules.minimum_order_size)
            .filter(|qty| qty.is_finite() && *qty > 0.0)
            .unwrap_or(self.config.venue_min_order_quantity)
            .max(self.required_order_quantity(best_bid));
        let remaining_leg_usd = (max_leg_cost_usd - expensive_leg_cost).max(0.0);
        let remaining_gross_usd = (max_gross_cost_usd - gross_cost).max(0.0);
        let max_notional_usd = remaining_bar_budget_usd
            .min(remaining_leg_usd)
            .min(remaining_gross_usd)
            .max(0.0);
        if max_notional_usd <= 0.0 {
            return None;
        }
        let max_quantity = max_notional_usd / best_bid;
        if !max_quantity.is_finite() || max_quantity <= 0.0 {
            return None;
        }

        let quantity = {
            let floored = (max_quantity * 100.0).floor() / 100.0;
            if floored + 1e-9 >= venue_min_qty {
                floored
            } else {
                let min_rounded = (venue_min_qty * 100.0).ceil() / 100.0;
                if min_rounded * best_bid <= max_notional_usd + 1e-9 {
                    min_rounded
                } else {
                    return None;
                }
            }
        };
        if quantity + 1e-9 < venue_min_qty {
            return None;
        }

        let price = Self::floor_to_tick(best_bid, tick);
        Some(Self::build_order(
            market_id.clone(),
            expensive_leg_id.clone(),
            TradeSide::Buy,
            price,
            quantity,
            false,
            "mm-late-bar-core:l1".to_string(),
            format!(
                "btc-5m-mm late-bar core accumulation: ask={:.4} bid={best_bid:.4} fair={expensive_leg_fair:.4} remaining_budget_usd={remaining_bar_budget_usd:.2} remaining_ms={remaining_ms} ttl_ms={}",
                Self::best_ask(expensive_leg_quote).unwrap_or(0.0),
                Self::LATE_BAR_CORE_TTL_MS,
            ),
            IntentKind::Entry,
            now_ms,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn build_convex_accumulation_intent(
        &self,
        market_id: &MarketId,
        left_id: &InstrumentId,
        left_quote: &QuoteSnapshot,
        left_fair: f64,
        left_cost: f64,
        right_id: &InstrumentId,
        right_quote: &QuoteSnapshot,
        right_fair: f64,
        right_cost: f64,
        gross_cost: f64,
        max_gross_cost_usd: f64,
        max_leg_cost_usd: f64,
        convex_bankroll_usd: f64,
        venue_rules: Option<&VenueMarketRules>,
        now_ms: EpochMillis,
    ) -> Option<OrderIntent> {
        let left_bid = self.candidate_ladder_bid_price(
            left_quote,
            left_fair,
            left_cost,
            gross_cost,
            self.config.min_edge_bps,
            venue_rules,
            0,
        );
        let right_bid = self.candidate_ladder_bid_price(
            right_quote,
            right_fair,
            right_cost,
            gross_cost,
            self.config.min_edge_bps,
            venue_rules,
            0,
        );
        let mut candidates = Vec::new();
        if let Some(price) = left_bid {
            candidates.push((left_id, left_fair, left_cost, price));
        }
        if let Some(price) = right_bid {
            candidates.push((right_id, right_fair, right_cost, price));
        }
        candidates.sort_by(|left, right| {
            left.3
                .partial_cmp(&right.3)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for (instrument_id, fair, leg_cost, bid_price) in candidates {
            if bid_price > Self::CONVEX_ACCUMULATION_MAX_BID {
                continue;
            }
            if fair < bid_price + self.config.min_edge_bps / 10_000.0 + Self::HOLD_EV_MARGIN {
                continue;
            }
            let min_quantity = self.required_order_quantity(bid_price);
            let remaining_leg_usd = (max_leg_cost_usd - leg_cost).max(0.0);
            let remaining_gross_usd = (max_gross_cost_usd - gross_cost).max(0.0);
            let kelly_budget_usd =
                self.convex_kelly_budget_usd(fair, bid_price, convex_bankroll_usd);
            let max_notional_usd = remaining_leg_usd
                .min(remaining_gross_usd)
                .min(kelly_budget_usd)
                .min(self.config.max_clip_usd)
                .max(0.0);
            let max_quantity = max_notional_usd / bid_price;
            let min_notional_usd = min_quantity * bid_price;
            if !max_quantity.is_finite() || max_quantity + 1e-9 < min_quantity {
                tracing::info!(
                    target: "strategy.convex_kelly",
                    market = %market_id,
                    instrument = %instrument_id,
                    fair,
                    bid_price,
                    convex_bankroll_usd,
                    kelly_budget_usd,
                    min_notional_usd,
                    max_notional_usd,
                    remaining_leg_usd,
                    remaining_gross_usd,
                    outcome = "below_venue_minimum",
                    "convex accumulation skipped by Kelly budget"
                );
                continue;
            }
            let quantity = max_quantity;
            let max_bid = self.max_bid_for(fair, leg_cost, gross_cost, self.config.min_edge_bps);
            return self.build_bid_intent_at_price(
                market_id,
                instrument_id,
                bid_price,
                fair,
                max_bid,
                leg_cost,
                gross_cost,
                quantity,
                "mm-convex-accum:l1",
                "btc-5m-mm convex cheap-leg accumulation",
                now_ms,
            );
        }
        None
    }

    fn build_order(
        market_id: MarketId,
        instrument_id: InstrumentId,
        side: TradeSide,
        price: f64,
        quantity: f64,
        reduce_only: bool,
        quote_level_tag: String,
        reason: String,
        kind: IntentKind,
        now_ms: EpochMillis,
    ) -> OrderIntent {
        let client_order_tag = format!("{quote_level_tag}:attempt-{}", now_ms);
        OrderIntent {
            client_order_id: deterministic_client_order_id(
                "btc-5m-mm",
                &market_id,
                &instrument_id,
                side,
                reduce_only,
                &client_order_tag,
                price,
                quantity,
            ),
            market_id,
            instrument_id,
            side,
            limit_price: price,
            quantity,
            reduce_only,
            reason,
            quote_level_tag: Some(quote_level_tag),
            created_at_ms: now_ms,
            pair_id: None,
            kind,
        }
    }

    /// Walk the ask depth book to find the limit price that, when used on
    /// an IOC, will sweep enough liquidity to fill `target_qty`. Returns
    /// `(limit_price, fillable_qty)` where fillable_qty <= target_qty
    /// (less if total available depth is shallower than what we want).
    ///
    /// IOC behavior: a buy IOC at price P takes ALL ask liquidity at
    /// price <= P, up to the order's quantity. So setting the limit to
    /// the worst price needed to cover target_qty guarantees a sweep
    /// through every cheaper level too — one order, multi-level fill.
    fn depth_walk_to_quantity(quote: &QuoteSnapshot, target_qty: f64) -> Option<(f64, f64)> {
        let mut accum = 0.0;
        let mut last_price = 0.0;
        for level in &quote.ask_levels {
            if !level.price.is_finite() || level.price <= 0.0 || level.quantity <= 0.0 {
                continue;
            }
            accum += level.quantity;
            last_price = level.price;
            if accum >= target_qty {
                return Some((last_price, target_qty));
            }
        }
        if accum > 1e-9 {
            Some((last_price, accum))
        } else {
            None
        }
    }

    /// Build a TAKER (IOC) rescue order that sweeps the depth book of the
    /// stranded-leg's opposite outcome. Mirrors unlawful's atomic-completion
    /// pattern: when one leg fills and the other doesn't, sweep multiple
    /// price levels of the missing side in ONE IOC at the deepest price
    /// needed to cover stranded_qty (or all available depth if shallower).
    ///
    /// Economic gate: only NEW money matters — stranded leg's cost is
    /// sunk. Sweep limit must be < $1 so each pair-via-merge nets ≥ $0.
    /// Skips per-leg cap (rescue CLOSES exposure) but respects gross cap.
    fn build_rescue_intent_for_quantity(
        &self,
        market_id: &MarketId,
        instrument_id: &InstrumentId,
        quote: &QuoteSnapshot,
        quantity: f64,
        gross_cost: f64,
        max_rescue_notional_usd: f64,
        venue_rules: Option<&VenueMarketRules>,
        reason_prefix: &str,
        now_ms: EpochMillis,
    ) -> Option<OrderIntent> {
        // Tick size: prefer venue-authoritative; fall back to config default
        // when runner hasn't fetched MarketMetadata yet.
        let tick_size = venue_rules
            .map(|r| r.minimum_tick_size)
            .filter(|t| t.is_finite() && *t > 0.0)
            .unwrap_or(self.config.maker_price_tick);
        // Prefer venue-authoritative minimum_order_size; only fall back to
        // the config default while waiting for the runner to fetch
        // MarketMetadata for this market.
        let venue_min = venue_rules
            .map(|r| r.minimum_order_size)
            .filter(|m| m.is_finite() && *m > 0.0)
            .unwrap_or(self.config.venue_min_order_quantity);
        let min_marketable_notional = self
            .config
            .min_order_notional_usd
            .max(Self::MARKETABLE_BUY_MIN_NOTIONAL_USD);
        let best_sweep_price = (Self::best_ask(quote)?
            + tick_size * self.config.hedge_rescue_race_buffer_ticks)
            .min(0.99);
        if best_sweep_price >= 0.99 {
            return None;
        }
        let min_target_qty = venue_min.max(min_marketable_notional / best_sweep_price);
        // Depth walk: find the price needed to sweep enough liquidity to both
        // cover the stranded quantity and satisfy protocol marketable-BUY
        // floors. If visible depth cannot support that, do not emit an order
        // the venue will deterministically reject.
        let depth_target_qty = quantity.max(min_target_qty);
        let (depth_walk_price, depth_walk_qty) =
            Self::depth_walk_to_quantity(quote, depth_target_qty)?;
        // Race buffer: pad the limit upward by a few ticks so the FAK still
        // crosses if the book ticks up between snapshot time and venue
        // receipt. Without this we routinely get back from Polymarket:
        //   "no orders found to match with FAK order. FAK orders are
        //    partially filled or killed if no match is found."
        // because the depth-walk price is stale by ~50-200ms (network +
        // batching latency). Net economics still strongly positive: each
        // pair-via-merge releases $1, so paying e.g. $0.05 extra per share
        // costs us 5¢ vs the typical $0.50+ rescue gain. Cap at $0.99
        // to preserve at least 1¢ per-share net before fees.
        let race_buffer = tick_size * self.config.hedge_rescue_race_buffer_ticks;
        let sweep_price = (depth_walk_price + race_buffer).min(0.99);
        // Final solvency gate after the buffer: a rescue at >= $1 nets ≤ $0
        // even before fees — never worth doing.
        if sweep_price >= 0.99 {
            return None;
        }
        // Upsize to satisfy the venue's per-order minimum. Rescue qty is
        // dictated by stranded inventory, not by us — when partial fills
        // leave us with e.g. 4.99 shares and venue requires ≥5, blocking
        // the rescue here just leaves us naked long forever. Buy the venue
        // minimum instead; the merge engine pairs MIN(left, right) (see
        // `core/inventory.rs` stranded-pairing) so the (venue_min - stranded)
        // residual becomes a tiny new stranded position — bounded by
        // venue_min and itself a future rescue candidate. Same principle
        // as the other 4 cap-bypass layers (max_open_orders, max_leg_cost,
        // max_gross_cost, max_submit_per_window): entry-time caps must not
        // trap close intents in the exposure they were meant to prevent.
        //
        let min_sweep_qty = venue_min.max(min_marketable_notional / sweep_price);
        let min_sweep_notional = min_sweep_qty * sweep_price;
        if max_rescue_notional_usd + 1e-9 < min_sweep_notional {
            return None;
        }
        let max_sweep_notional = self
            .config
            .hedge_rescue_clip_usd
            .max(min_sweep_notional)
            .min(max_rescue_notional_usd)
            .max(0.0);
        let max_sweep_qty = max_sweep_notional / sweep_price;
        if max_sweep_qty + 1e-9 < min_sweep_qty || depth_walk_qty + 1e-9 < min_sweep_qty {
            return None;
        }
        let sweep_qty = depth_walk_qty.min(max_sweep_qty).max(min_sweep_qty);
        if sweep_qty < self.config.min_order_quantity {
            return None;
        }
        let notional = sweep_qty * sweep_price;
        if notional + 1e-9 < min_marketable_notional {
            return None;
        }
        // Skip BOTH max_leg_cost AND max_gross_cost caps. Both are
        // entry-time caps designed to prevent paired-entry accumulation
        // runaway. Rescue is the OPPOSITE of accumulation — it closes
        // existing directional exposure by manufacturing a paired set
        // for immediate merge. Blocking it here just leaves us naked
        // long on one side. The merge will return the cash within ~30s
        // of the rescue submit (paired_qty × $1 collateral release).
        // Wallet-cash exhaustion is still gated upstream by the
        // execution adapter's balance check; we don't need a duplicate
        // here. Suppress unused parameter warning.
        let _ = gross_cost;
        let depth_levels_swept = quote
            .ask_levels
            .iter()
            .take_while(|l| l.price <= sweep_price + 1e-9 && l.price > 0.0)
            .count();
        Some(Self::build_order(
            market_id.clone(),
            instrument_id.clone(),
            TradeSide::Buy,
            sweep_price,
            sweep_qty,
            false,
            "mm-hedge-rescue".to_string(),
            format!(
                "{reason_prefix} depth-sweep limit={sweep_price:.4} qty={sweep_qty:.2} notional={notional:.2} levels={depth_levels_swept}"
            ),
            IntentKind::Close,
            now_ms,
        ))
    }

    fn build_bid_intent_at_price(
        &self,
        market_id: &MarketId,
        instrument_id: &InstrumentId,
        bid_price: f64,
        fair: f64,
        max_bid: f64,
        leg_cost: f64,
        gross_cost: f64,
        quantity: f64,
        quote_level_tag: &str,
        reason_prefix: &str,
        now_ms: EpochMillis,
    ) -> Option<OrderIntent> {
        if quantity < self.config.min_order_quantity
            || quantity + 1e-9 < self.config.venue_min_order_quantity
        {
            return None;
        }
        let notional = quantity * bid_price;
        if notional < self.config.min_order_notional_usd
            || notional > self.config.max_leg_cost_usd - leg_cost + 1e-9
            || notional > self.config.max_gross_cost_usd - gross_cost + 1e-9
        {
            return None;
        }
        Some(Self::build_order(
            market_id.clone(),
            instrument_id.clone(),
            TradeSide::Buy,
            bid_price,
            quantity,
            false,
            quote_level_tag.to_string(),
            format!(
                "{reason_prefix} fair={fair:.4} bid={bid_price:.4} max_bid={max_bid:.4} leg_cost={leg_cost:.2}"
            ),
            IntentKind::Entry,
            now_ms,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn build_paired_entry_ladder(
        &self,
        market_id: &MarketId,
        left_id: &InstrumentId,
        left_quote: &QuoteSnapshot,
        left_fair: f64,
        left_cost: f64,
        right_id: &InstrumentId,
        right_quote: &QuoteSnapshot,
        right_fair: f64,
        right_cost: f64,
        gross_cost: f64,
        max_gross_cost_usd: f64,
        max_leg_cost_usd: f64,
        requested_clip_usd: f64,
        venue_rules: Option<&VenueMarketRules>,
        now_ms: EpochMillis,
    ) -> Vec<OrderIntent> {
        let mut intents = Vec::new();
        let mut ladder_left_cost = 0.0;
        let mut ladder_right_cost = 0.0;
        let mut ladder_gross_cost = 0.0;
        let levels = self.config.entry_ladder_levels.max(1);

        for level_index in 0..levels {
            let effective_left_cost = left_cost + ladder_left_cost;
            let effective_right_cost = right_cost + ladder_right_cost;
            let effective_gross_cost = gross_cost + ladder_gross_cost;
            let Some(left_bid_price) = self.candidate_ladder_bid_price(
                left_quote,
                left_fair,
                effective_left_cost,
                effective_gross_cost,
                self.config.min_edge_bps,
                venue_rules,
                level_index,
            ) else {
                break;
            };
            let Some(right_bid_price) = self.candidate_ladder_bid_price(
                right_quote,
                right_fair,
                effective_right_cost,
                effective_gross_cost,
                self.config.min_edge_bps,
                venue_rules,
                level_index,
            ) else {
                break;
            };
            if left_bid_price.max(right_bid_price) > self.config.entry_premium_bid_cap {
                break;
            }
            let Some(raw_quantity) = self.paired_entry_quantity(
                market_id,
                left_quote,
                right_quote,
                left_bid_price,
                right_bid_price,
                requested_clip_usd,
            ) else {
                break;
            };

            let remaining_left_usd = (max_leg_cost_usd - effective_left_cost).max(0.0);
            let remaining_right_usd = (max_leg_cost_usd - effective_right_cost).max(0.0);
            let remaining_gross_usd = (max_gross_cost_usd - effective_gross_cost).max(0.0);
            let max_quantity_by_budget = (remaining_left_usd / left_bid_price)
                .min(remaining_right_usd / right_bid_price)
                .min(remaining_gross_usd / (left_bid_price + right_bid_price));
            let min_quantity = self.required_order_quantity(left_bid_price.min(right_bid_price));
            if !max_quantity_by_budget.is_finite() || max_quantity_by_budget + 1e-9 < min_quantity {
                break;
            }
            let quantity = raw_quantity.min(max_quantity_by_budget).max(min_quantity);
            let left_tag = format!("mm-paired-bid:l{}", level_index + 1);
            let right_tag = left_tag.clone();
            let max_left_bid = self.max_bid_for(
                left_fair,
                effective_left_cost,
                effective_gross_cost,
                self.config.min_edge_bps,
            );
            let max_right_bid = self.max_bid_for(
                right_fair,
                effective_right_cost,
                effective_gross_cost,
                self.config.min_edge_bps,
            );
            let Some(mut left_bid) = self.build_bid_intent_at_price(
                market_id,
                left_id,
                left_bid_price,
                left_fair,
                max_left_bid,
                effective_left_cost,
                effective_gross_cost,
                quantity,
                &left_tag,
                "btc-5m-mm paired ladder bid",
                now_ms,
            ) else {
                break;
            };
            let Some(mut right_bid) = self.build_bid_intent_at_price(
                market_id,
                right_id,
                right_bid_price,
                right_fair,
                max_right_bid,
                effective_right_cost,
                effective_gross_cost,
                quantity,
                &right_tag,
                "btc-5m-mm paired ladder bid",
                now_ms,
            ) else {
                break;
            };

            let pair_id = format!("pair-{market_id}-{now_ms}-l{}", level_index + 1);
            left_bid.pair_id = Some(pair_id.clone());
            right_bid.pair_id = Some(pair_id);
            ladder_left_cost += left_bid.limit_price * left_bid.quantity;
            ladder_right_cost += right_bid.limit_price * right_bid.quantity;
            ladder_gross_cost += left_bid.limit_price * left_bid.quantity
                + right_bid.limit_price * right_bid.quantity;
            intents.push(left_bid);
            intents.push(right_bid);
        }

        intents
    }
}

impl Strategy for Btc5mMmStrategy {
    fn name(&self) -> &str {
        "btc_5m_mm"
    }

    fn on_start(&mut self, _context: &StrategyContext) -> StrategyDecision {
        StrategyDecision::none()
    }

    fn on_market_snapshot(
        &mut self,
        context: &StrategyContext,
        snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        self.prune_market_states(context, &snapshot.market_id);
        let (left_id, left_quote, right_id, right_quote) = {
            let state = self
                .market_states
                .entry(snapshot.market_id.clone())
                .or_default();
            state
                .quotes
                .insert(snapshot.instrument_id.clone(), snapshot.quote.clone());
            if state.quotes.len() < 2 {
                return StrategyDecision::none();
            }
            let mut sides = state
                .quotes
                .iter()
                .map(|(id, quote)| (id.clone(), quote.clone()))
                .collect::<Vec<_>>();
            sides.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
            let (left_id, left_quote) = sides[0].clone();
            let (right_id, right_quote) = sides[1].clone();
            (left_id, left_quote, right_id, right_quote)
        };

        if !self.quote_is_usable(&left_quote) || !self.quote_is_usable(&right_quote) {
            return self.no_quote_decision(
                &snapshot.market_id,
                context.now_ms,
                format!(
                    "unusable book left={} {} right={} {}",
                    left_id,
                    Self::quote_health(&left_quote),
                    right_id,
                    Self::quote_health(&right_quote)
                ),
            );
        }
        let Some((left_fair, right_fair)) = self.fair_values(
            &left_id,
            &left_quote,
            &right_id,
            &right_quote,
            &context.btc_regime,
            context.market_context.as_ref(),
            context.now_ms,
        ) else {
            return self.no_quote_decision(
                &snapshot.market_id,
                context.now_ms,
                format!("fair value unavailable left={} right={}", left_id, right_id),
            );
        };
        let market_mid_move = self.observe_market_mid_move(
            &snapshot.market_id,
            context.now_ms,
            &left_quote,
            &right_quote,
        );

        let gross_cost = Self::gross_cost_usd(&context.inventory, &snapshot.market_id);
        let (left_qty, left_avg, _) =
            Self::position_for(&context.inventory, &snapshot.market_id, &left_id);
        let (right_qty, right_avg, _) =
            Self::position_for(&context.inventory, &snapshot.market_id, &right_id);
        let left_cost = (left_qty * left_avg).max(0.0);
        let right_cost = (right_qty * right_avg).max(0.0);
        let (max_entry_gross_cost_usd, max_entry_leg_cost_usd) =
            self.effective_entry_caps_usd(&context.inventory);
        let max_rescue_notional_usd = self.rescue_free_cash_cap_usd(&context.inventory);

        let mut intents = Vec::new();
        let mut hold_notes = Vec::new();
        let actionable_inventory_min =
            self.actionable_inventory_min_quantity(context.venue_rules.as_ref());
        let left_has_inventory = left_qty + 1e-9 >= actionable_inventory_min;
        let right_has_inventory = right_qty + 1e-9 >= actionable_inventory_min;
        let market_mode = self.transition_market_mode(
            &snapshot.market_id,
            context.now_ms,
            left_has_inventory || right_has_inventory,
            left_fair,
            right_fair,
            &context.btc_regime,
            market_mid_move,
        );

        match (left_has_inventory, right_has_inventory) {
            (false, false) => {
                self.clear_rescue_state(&snapshot.market_id);
                if context.runtime_status != RuntimeStatus::Running {
                    return self.no_quote_decision(
                        &snapshot.market_id,
                        context.now_ms,
                        format!(
                            "fresh entry suppressed: runtime status is {:?}",
                            context.runtime_status
                        ),
                    );
                }
                let mut cooling_reason = None;
                if let Btc5mMmMarketMode::Cooling { reason, until_ms } = &market_mode {
                    if self.should_log_cooling_note(&snapshot.market_id, context.now_ms, reason) {
                        tracing::info!(
                            target: "strategy.market_state",
                            market = %snapshot.market_id,
                            state = "cooling",
                            reason,
                            until_ms,
                            left_fair,
                            right_fair,
                            market_mid_move,
                            "fresh paired entry suppressed by market state"
                        );
                    }
                    if !Self::cooling_allows_convex_accumulation(reason) {
                        return self.no_quote_decision(
                            &snapshot.market_id,
                            context.now_ms,
                            format!("market state cooling: {reason} until={until_ms:?}"),
                        );
                    }
                    cooling_reason = Some((reason.as_str(), *until_ms));
                }

                // Fill-rate-aware sizing (#43): scale base clip by recent
                // fill activity. Hot tape → bigger clips to compound; dead
                // tape → smaller clips to preserve capital. Range [0.7x, 2.0x].
                self.prune_fill_window(context.now_ms);
                let fill_scale = self.fill_rate_clip_scale(context.now_ms);
                let scaled_clip = (self.config.base_clip_usd * fill_scale)
                    .clamp(self.config.min_clip_usd, self.config.max_clip_usd);
                let suppress_paired = self.suppress_paired_by_flow_imbalance(
                    &snapshot.market_id,
                    &left_id,
                    &left_quote,
                    left_fair,
                    &right_id,
                    &right_quote,
                    right_fair,
                );
                if cooling_reason.is_none() {
                    if !suppress_paired {
                        intents.extend(self.build_paired_entry_ladder(
                            &snapshot.market_id,
                            &left_id,
                            &left_quote,
                            left_fair,
                            left_cost,
                            &right_id,
                            &right_quote,
                            right_fair,
                            right_cost,
                            gross_cost,
                            max_entry_gross_cost_usd,
                            max_entry_leg_cost_usd,
                            scaled_clip,
                            context.venue_rules.as_ref(),
                            context.now_ms,
                        ));
                    }
                }
                if intents.is_empty() {
                    // Late-bar expensive-leg accumulation: capture favored-leg
                    // convergence near bar end before evaluating cheap-leg
                    // convex accumulation. If both could fire on this tick,
                    // late-bar-core takes precedence.
                    let (
                        expensive_leg_id,
                        expensive_leg_quote,
                        expensive_leg_fair,
                        expensive_leg_cost,
                    ) = if left_fair > right_fair {
                        (&left_id, &left_quote, left_fair, left_cost)
                    } else {
                        (&right_id, &right_quote, right_fair, right_cost)
                    };
                    let late_skip = self.late_bar_core_skip_reason(
                        &snapshot.market_id,
                        expensive_leg_id,
                        expensive_leg_quote,
                        &context.btc_regime,
                        context.market_context.as_ref(),
                        context.now_ms,
                        &context.inventory,
                    );
                    if let Some(reason) = late_skip {
                        tracing::debug!(
                            target: "strategy.late_bar_core_gate",
                            market = %snapshot.market_id,
                            reason,
                            "late-bar core accumulation suppressed"
                        );
                    } else {
                        let curr_bar_end = context
                            .market_context
                            .as_ref()
                            .and_then(|record| record.event_end_time_ms);
                        let remaining_bar_budget_usd = self
                            .late_bar_core_remaining_budget_usd(&snapshot.market_id, curr_bar_end);
                        let remaining_ms = Self::time_remaining_ms(
                            context.market_context.as_ref(),
                            context.now_ms,
                        )
                        .unwrap_or_default();
                        if let Some(intent) = self.build_late_bar_core_intent(
                            &snapshot.market_id,
                            expensive_leg_id,
                            expensive_leg_quote,
                            expensive_leg_fair,
                            expensive_leg_cost,
                            gross_cost,
                            max_entry_gross_cost_usd,
                            max_entry_leg_cost_usd,
                            remaining_bar_budget_usd,
                            remaining_ms,
                            context.venue_rules.as_ref(),
                            context.now_ms,
                        ) {
                            let state = self
                                .market_states
                                .entry(snapshot.market_id.clone())
                                .or_default();
                            if state.late_bar_core_bar_end_ms != curr_bar_end {
                                state.late_bar_core_bids_this_bar = 0;
                                state.late_bar_core_spend_this_bar_usd = 0.0;
                                state.late_bar_core_bar_end_ms = curr_bar_end;
                            }
                            state.late_bar_core_bids_this_bar =
                                state.late_bar_core_bids_this_bar.saturating_add(1);
                            state.late_bar_core_spend_this_bar_usd +=
                                intent.limit_price * intent.quantity;
                            intents.push(intent);
                        }
                    }
                }
                if intents.is_empty() {
                    let convex_skip = self.convex_skip_reason(
                        &snapshot.market_id,
                        &left_id,
                        left_fair,
                        &right_id,
                        right_fair,
                        &context.btc_regime,
                        context.market_context.as_ref(),
                        context.now_ms,
                    );
                    if let Some(reason) = convex_skip {
                        tracing::debug!(
                            target: "strategy.convex_gate",
                            market = %snapshot.market_id,
                            reason,
                            "convex accumulation suppressed"
                        );
                    } else if let Some(intent) = self.build_convex_accumulation_intent(
                        &snapshot.market_id,
                        &left_id,
                        &left_quote,
                        left_fair,
                        left_cost,
                        &right_id,
                        &right_quote,
                        right_fair,
                        right_cost,
                        gross_cost,
                        max_entry_gross_cost_usd * Self::CONVEX_BUDGET_FRACTION,
                        max_entry_leg_cost_usd * Self::CONVEX_BUDGET_FRACTION,
                        Self::inventory_equity_usd(&context.inventory)
                            .min(context.inventory.free_cash_usd.max(0.0)),
                        context.venue_rules.as_ref(),
                        context.now_ms,
                    ) {
                        // Track per-bar count: reset if this is a new bar,
                        // increment otherwise. Used by convex_skip_reason
                        // gate #4 to cap convex bids per market per bar.
                        let curr_bar_end = context
                            .market_context
                            .as_ref()
                            .and_then(|c| c.event_end_time_ms);
                        let state = self
                            .market_states
                            .entry(snapshot.market_id.clone())
                            .or_default();
                        if state.convex_bar_end_ms != curr_bar_end {
                            state.convex_bids_this_bar = 0;
                            state.convex_bar_end_ms = curr_bar_end;
                        }
                        state.convex_bids_this_bar = state.convex_bids_this_bar.saturating_add(1);
                        intents.push(intent);
                    }
                }
                if intents.is_empty() {
                    let cooling_suffix = cooling_reason
                        .map(|(reason, until_ms)| {
                            format!(" cooling_reason={reason} until={until_ms:?}")
                        })
                        .unwrap_or_default();
                    return self.no_quote_decision(
                        &snapshot.market_id,
                        context.now_ms,
                        format!(
                            "entry rejected left={} {} right={} {} gross_cost={gross_cost:.2}/{max_entry_gross_cost_usd:.2} leg_cap={max_entry_leg_cost_usd:.2} levels={} scaled_clip={scaled_clip:.2}{}",
                            left_id,
                            self.bid_health(
                                &left_quote,
                                left_fair,
                                left_cost,
                                gross_cost,
                                self.config.min_edge_bps,
                                context.venue_rules.as_ref(),
                            ),
                            right_id,
                            self.bid_health(
                                &right_quote,
                                right_fair,
                                right_cost,
                                gross_cost,
                                self.config.min_edge_bps,
                                context.venue_rules.as_ref(),
                            ),
                            self.config.entry_ladder_levels,
                            cooling_suffix,
                        ),
                    );
                }
            }
            (true, false) | (false, true) => {
                // Stranded inventory on one side → manufacture pair by IOC-lifting
                // the OPPOSITE leg's ask. Throttle so we don't emit on every
                // book tick — the IOC submit needs ~3-6s round-trip; emitting
                // 80+ duplicates per minute drowns the engine's rate limiter
                // (which then drops them all silently).
                let throttle_ok = self
                    .market_states
                    .get(&snapshot.market_id)
                    .and_then(|state| state.last_rescue_attempt_ms)
                    .map(|last| context.now_ms.saturating_sub(last) >= self.config.cooldown_ms)
                    .unwrap_or(true);
                // The "stranded" leg is the one we already hold; we lift the
                // OPPOSITE leg's ask to manufacture the pair. Stranded
                // leg's cost is sunk — the only economic question is whether
                // best_ask < $1 (rescue netting against $1 merge release).
                let (
                    held_id,
                    held_fair,
                    lift_id,
                    lift_quote,
                    stranded_qty,
                    stranded_avg,
                    side_label,
                ) = if left_has_inventory {
                    (
                        &left_id,
                        left_fair,
                        &right_id,
                        &right_quote,
                        left_qty,
                        left_avg,
                        "lift_right_ask",
                    )
                } else {
                    (
                        &right_id,
                        right_fair,
                        &left_id,
                        &left_quote,
                        right_qty,
                        right_avg,
                        "lift_left_ask",
                    )
                };
                let exposure_decision = self.decide_stranded_exposure(
                    held_id,
                    held_fair,
                    stranded_avg,
                    stranded_qty,
                    lift_quote,
                    context.market_context.as_ref(),
                    context.now_ms,
                );
                let rescue_ok = throttle_ok
                    && exposure_decision.rescue_qty > 1e-9
                    && self.can_emit_rescue(
                        &snapshot.market_id,
                        held_id,
                        lift_id,
                        context.now_ms,
                        &mut hold_notes,
                    );
                let intent = if rescue_ok {
                    self.build_rescue_intent_for_quantity(
                        &snapshot.market_id,
                        lift_id,
                        lift_quote,
                        exposure_decision.rescue_qty,
                        gross_cost,
                        max_rescue_notional_usd,
                        context.venue_rules.as_ref(),
                        "btc-5m-mm hedge rescue",
                        context.now_ms,
                    )
                } else {
                    None
                };
                tracing::info!(
                    target: "strategy.rescue",
                    market = %snapshot.market_id,
                    side = side_label,
                    stranded_qty,
                    stranded_avg,
                    held_fair = exposure_decision.held_fair,
                    avg_cost = exposure_decision.avg_cost,
                    hold_ev_per_share = exposure_decision.hold_ev_per_share,
                    rescue_ev_per_share = ?exposure_decision.rescue_ev_per_share,
                    rescue_qty = exposure_decision.rescue_qty,
                    hold_qty = exposure_decision.hold_qty,
                    hold = exposure_decision.rescue_qty <= 1e-9,
                    lift_best_ask = ?Self::best_ask(lift_quote),
                    rescue_profit_per_share = 1.0 - Self::best_ask(lift_quote).unwrap_or(1.0),
                    throttle_ok = throttle_ok,
                    intent_built = intent.is_some(),
                    "hedge rescue branch entered"
                );
                if exposure_decision.hold_qty > 1e-9 {
                    hold_notes.push(exposure_decision.reason);
                }
                if let Some(hedge) = intent {
                    intents.push(hedge);
                    self.record_rescue_attempt(
                        &snapshot.market_id,
                        held_id,
                        lift_id,
                        exposure_decision.rescue_qty,
                        context.now_ms,
                    );
                }
            }
            (true, true) => {
                // ASYMMETRIC RESCUE: both legs have inventory, but if one is
                // much bigger than the other (e.g. 47 Up + 5 Down because Up
                // fills kept hitting in a trending market), the merge engine
                // can only pair MIN(left, right). The EXCESS is stranded and
                // bleeds at resolution if the favored side loses.
                //
                // Real failure mode observed 2026-04-27 on btc-updown-5m-1777282500:
                // -$20.55 in 30min from 47 stranded Up shares against 5 Down.
                // Strategy was no-op'ing on (true, true) instead of rescuing.
                //
                // Fix: detect imbalance, rescue the excess by lifting the
                // opposite (under-stocked) leg's ask. Same throttle + race
                // buffer + venue-min upsize as the (true, false)/(false, true)
                // rescue path.
                let imbalance = (left_qty - right_qty).abs();
                let venue_min = context
                    .venue_rules
                    .as_ref()
                    .map(|r| r.minimum_order_size)
                    .filter(|m| m.is_finite() && *m > 0.0)
                    .unwrap_or(self.config.venue_min_order_quantity);
                if imbalance < venue_min {
                    self.clear_rescue_state(&snapshot.market_id);
                } else {
                    let throttle_ok = self
                        .market_states
                        .get(&snapshot.market_id)
                        .and_then(|state| state.last_rescue_attempt_ms)
                        .map(|last| context.now_ms.saturating_sub(last) >= self.config.cooldown_ms)
                        .unwrap_or(true);
                    let (
                        held_id,
                        held_fair,
                        lift_id,
                        lift_quote,
                        stranded_excess,
                        stranded_avg,
                        side_label,
                    ) = if left_qty > right_qty {
                        // Excess is on left → manufacture more right
                        (
                            &left_id,
                            left_fair,
                            &right_id,
                            &right_quote,
                            left_qty - right_qty,
                            left_avg,
                            "lift_right_ask_asym",
                        )
                    } else {
                        (
                            &right_id,
                            right_fair,
                            &left_id,
                            &left_quote,
                            right_qty - left_qty,
                            right_avg,
                            "lift_left_ask_asym",
                        )
                    };
                    let exposure_decision = self.decide_stranded_exposure(
                        held_id,
                        held_fair,
                        stranded_avg,
                        stranded_excess,
                        lift_quote,
                        context.market_context.as_ref(),
                        context.now_ms,
                    );
                    let rescue_ok = throttle_ok
                        && exposure_decision.rescue_qty > 1e-9
                        && self.can_emit_rescue(
                            &snapshot.market_id,
                            held_id,
                            lift_id,
                            context.now_ms,
                            &mut hold_notes,
                        );
                    let intent = if rescue_ok {
                        self.build_rescue_intent_for_quantity(
                            &snapshot.market_id,
                            lift_id,
                            lift_quote,
                            exposure_decision.rescue_qty,
                            gross_cost,
                            max_rescue_notional_usd,
                            context.venue_rules.as_ref(),
                            "btc-5m-mm asymmetric rescue",
                            context.now_ms,
                        )
                    } else {
                        None
                    };
                    tracing::info!(
                        target: "strategy.rescue",
                        market = %snapshot.market_id,
                        side = side_label,
                        left_qty,
                        right_qty,
                        imbalance,
                        stranded_avg,
                        held_fair = exposure_decision.held_fair,
                        avg_cost = exposure_decision.avg_cost,
                        hold_ev_per_share = exposure_decision.hold_ev_per_share,
                        rescue_ev_per_share = ?exposure_decision.rescue_ev_per_share,
                        rescue_qty = exposure_decision.rescue_qty,
                        hold_qty = exposure_decision.hold_qty,
                        hold = exposure_decision.rescue_qty <= 1e-9,
                        lift_best_ask = ?Self::best_ask(lift_quote),
                        rescue_profit_per_share = 1.0 - Self::best_ask(lift_quote).unwrap_or(1.0),
                        throttle_ok,
                        intent_built = intent.is_some(),
                        "asymmetric rescue branch entered"
                    );
                    if exposure_decision.hold_qty > 1e-9 {
                        hold_notes.push(exposure_decision.reason);
                    }
                    if let Some(hedge) = intent {
                        intents.push(hedge);
                        self.record_rescue_attempt(
                            &snapshot.market_id,
                            held_id,
                            lift_id,
                            exposure_decision.rescue_qty,
                            context.now_ms,
                        );
                    }
                }
            }
        }

        if intents.is_empty() {
            if !hold_notes.is_empty() {
                return StrategyDecision {
                    intents,
                    notes: hold_notes,
                };
            }
            return self.no_quote_decision(
                &snapshot.market_id,
                context.now_ms,
                format!(
                    "inventory management rejected left_qty={left_qty:.4} right_qty={right_qty:.4} left={} {} right={} {} gross_cost={gross_cost:.2}",
                    left_id,
                    self.bid_health(
                        &left_quote,
                        left_fair,
                        left_cost,
                        gross_cost,
                        self.config.hedge_rescue_edge_bps,
                        context.venue_rules.as_ref(),
                    ),
                    right_id,
                    self.bid_health(
                        &right_quote,
                        right_fair,
                        right_cost,
                        gross_cost,
                        self.config.hedge_rescue_edge_bps,
                        context.venue_rules.as_ref(),
                    ),
                ),
            );
        }
        if let Some(state) = self.market_states.get_mut(&snapshot.market_id) {
            state.last_action_ms = Some(context.now_ms);
        }
        let mut notes = hold_notes;
        notes.push(format!(
            "btc-5m-mm quotes left={} fair={left_fair:.4} right={} fair={right_fair:.4} gross_cost={gross_cost:.2}",
            left_id, right_id
        ));
        StrategyDecision { notes, intents }
    }

    fn on_fill(
        &mut self,
        context: &StrategyContext,
        fill: &crate::types::FillReport,
    ) -> StrategyDecision {
        let mut notes = vec![format!(
            "btc-5m-mm fill {} {:?} qty={:.4}@{:.4}",
            fill.instrument_id, fill.side, fill.quantity, fill.price
        )];
        let mut intents = Vec::new();
        self.prune_market_states(context, &fill.market_id);

        // Track this fill for fill-rate-aware sizing (#43). Buy-side fills
        // only — sells (e.g. inventory unwinds) don't count toward the
        // "are makers being rewarded?" signal we're trying to capture.
        if matches!(fill.side, TradeSide::Buy) {
            self.recent_fill_times.push_back(context.now_ms);
            self.prune_fill_window(context.now_ms);
        }
        if Self::is_own_entry_fill(fill) {
            if let Some((symmetry, total_qty)) = self.record_market_fill_asymmetry(
                &fill.market_id,
                &fill.instrument_id,
                fill.quantity,
                context.now_ms,
            ) {
                let block_until = self
                    .market_states
                    .get(&fill.market_id)
                    .and_then(|state| state.asymmetric_entry_block_until_ms)
                    .unwrap_or(context.now_ms);
                tracing::info!(
                    target: "strategy.entry_fill_asymmetry",
                    market = %fill.market_id,
                    instrument = %fill.instrument_id,
                    symmetry,
                    total_qty,
                    block_until_ms = block_until,
                    "fresh entry blocked after asymmetric paired-bid fills"
                );
                notes.push(format!(
                    "entry-fill asymmetry cooldown symmetry={symmetry:.3} total_qty={total_qty:.2} until={block_until}"
                ));
            }
        }

        // Record per-market last-fill timestamp for the post-fill entry
        // cooldown (avoids re-stranding on the same trending market right
        // after a directional fill). Skip merge-derived fills: merges UNWIND
        // exposure (paired Up+Down -> $1 collateral release) rather than
        // ADD it, so they shouldn't trip an entry cooldown that exists to
        // prevent compounding into the wrong side. Bonereaper / unlawful
        // both cycle paired-bid -> merge -> paired-bid back-to-back; gating
        // re-entry for 15s after a merge throws away ~5% of every 5min bar.
        let is_merge_fill = matches!(fill.close_method, Some(crate::types::CloseMethod::Merge));
        if !is_merge_fill {
            let market_state_ref = self
                .market_states
                .entry(fill.market_id.clone())
                .or_default();
            market_state_ref.last_fill_ms = Some(context.now_ms);
        }

        // Atomic on-fill rescue: when this fill creates a stranded leg
        // (we now hold side X, side Y is empty), immediately fire the IOC
        // rescue intent for side Y instead of waiting for the next book
        // snapshot tick (which can be 1-3s later). Mirrors unlawful's
        // sub-second pair completion latency.
        let market_state = match self.market_states.get(&fill.market_id) {
            Some(state) => state,
            None => return StrategyDecision { notes, intents },
        };

        // Need both outcome quotes cached to know what to lift.
        if market_state.quotes.len() < 2 {
            return StrategyDecision { notes, intents };
        }

        let mut sides: Vec<(InstrumentId, QuoteSnapshot)> = market_state
            .quotes
            .iter()
            .map(|(id, q)| (id.clone(), q.clone()))
            .collect();
        sides.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
        let (left_id, left_quote) = sides[0].clone();
        let (right_id, right_quote) = sides[1].clone();

        let (left_qty, left_avg, _) =
            Self::position_for(&context.inventory, &fill.market_id, &left_id);
        let (right_qty, right_avg, _) =
            Self::position_for(&context.inventory, &fill.market_id, &right_id);
        let actionable_inventory_min =
            self.actionable_inventory_min_quantity(context.venue_rules.as_ref());
        let left_has = left_qty + 1e-9 >= actionable_inventory_min;
        let right_has = right_qty + 1e-9 >= actionable_inventory_min;
        if left_has == right_has {
            // Either both legs filled (no rescue needed) or both empty (impossible
            // since the fill we just got created at least one). No-op.
            return StrategyDecision { notes, intents };
        }

        // Throttle: skip if a rescue was emitted within cooldown_ms.
        let throttle_ok = market_state
            .last_rescue_attempt_ms
            .map(|last| context.now_ms.saturating_sub(last) >= self.config.cooldown_ms)
            .unwrap_or(true);
        if !throttle_ok {
            notes.push("on-fill rescue throttled".to_string());
            return StrategyDecision { notes, intents };
        }

        let Some((left_fair, right_fair)) = self.fair_values(
            &left_id,
            &left_quote,
            &right_id,
            &right_quote,
            &context.btc_regime,
            context.market_context.as_ref(),
            context.now_ms,
        ) else {
            return StrategyDecision { notes, intents };
        };

        let (stranded_id, held_fair, stranded_avg, lift_id, lift_quote, stranded_qty) = if left_has
        {
            (
                left_id,
                left_fair,
                left_avg,
                right_id,
                right_quote,
                left_qty,
            )
        } else {
            (
                right_id, right_fair, right_avg, left_id, left_quote, right_qty,
            )
        };
        let exposure_decision = self.decide_stranded_exposure(
            &stranded_id,
            held_fair,
            stranded_avg,
            stranded_qty,
            &lift_quote,
            context.market_context.as_ref(),
            context.now_ms,
        );
        if exposure_decision.rescue_qty <= 1e-9 {
            notes.push(exposure_decision.reason);
            return StrategyDecision { notes, intents };
        }
        if !self.can_emit_rescue(
            &fill.market_id,
            &stranded_id,
            &lift_id,
            context.now_ms,
            &mut notes,
        ) {
            return StrategyDecision { notes, intents };
        }
        let gross_cost = Self::gross_cost_usd(&context.inventory, &fill.market_id);
        let intent = self.build_rescue_intent_for_quantity(
            &fill.market_id,
            &lift_id,
            &lift_quote,
            exposure_decision.rescue_qty,
            gross_cost,
            self.rescue_free_cash_cap_usd(&context.inventory),
            context.venue_rules.as_ref(),
            "btc-5m-mm on-fill rescue",
            context.now_ms,
        );
        tracing::info!(
            target: "strategy.on_fill_rescue",
            market = %fill.market_id,
            fill_instrument = %fill.instrument_id,
            stranded_qty,
            rescue_qty = exposure_decision.rescue_qty,
            hold_qty = exposure_decision.hold_qty,
            lift_best_ask = ?Self::best_ask(&lift_quote),
            intent_built = intent.is_some(),
            "on-fill rescue evaluated"
        );
        if let Some(hedge) = intent {
            intents.push(hedge);
            self.record_rescue_attempt(
                &fill.market_id,
                &stranded_id,
                &lift_id,
                exposure_decision.rescue_qty,
                context.now_ms,
            );
            notes.push("on-fill IOC rescue emitted".to_string());
        }

        StrategyDecision { notes, intents }
    }

    fn checkpoint_state(&self) -> Option<serde_json::Value> {
        serde_json::to_value(self.persisted_state()).ok()
    }

    fn restore_checkpoint_state(
        &mut self,
        state: &serde_json::Value,
    ) -> std::result::Result<(), String> {
        let persisted = serde_json::from_value::<Btc5mMmPersistedState>(state.clone())
            .map_err(|error| format!("failed to decode btc_5m_mm state: {error}"))?;
        if persisted.version != Self::PERSISTED_STATE_VERSION {
            return Err(format!(
                "unsupported btc_5m_mm state version: got {}, expected {}",
                persisted.version,
                Self::PERSISTED_STATE_VERSION
            ));
        }
        self.restore_persisted_state(persisted);
        Ok(())
    }
}

#[derive(Debug)]
pub struct GoatPairStrategy {
    config: GoatPairConfig,
    cooldown_ms: u64,
    quote_levels_per_side: usize,
    quote_min_edge_bps: f64,
    quote_inventory_skew_bps: f64,
    quote_min_quote_age_ms: Option<u64>,
    quote_expiry_suppression_ms: Option<u64>,
    quote_refresh_interval_ms: Option<u64>,
    market_states: HashMap<MarketId, GoatMarketState>,
}

impl GoatPairStrategy {
    pub fn new(config: GoatPairConfig, cooldown_ms: u64) -> Self {
        Self {
            config,
            cooldown_ms,
            quote_levels_per_side: 1,
            quote_min_edge_bps: 0.0,
            quote_inventory_skew_bps: 0.0,
            quote_min_quote_age_ms: None,
            quote_expiry_suppression_ms: None,
            quote_refresh_interval_ms: None,
            market_states: HashMap::new(),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(GoatPairConfig::from_env(), 250)
    }

    pub fn with_profile(profile: Option<&StrategyProfile>) -> Self {
        match profile {
            Some(profile) => {
                let mut strategy =
                    Self::new(profile.goat_pair_config(), profile.goat_pair_cooldown_ms());
                strategy.quote_levels_per_side =
                    profile.quote.levels_per_side.unwrap_or(1).clamp(1, 3);
                strategy.quote_min_edge_bps = profile.quote.min_edge_bps.unwrap_or(0.0).max(0.0);
                strategy.quote_inventory_skew_bps =
                    profile.quote.inventory_skew_bps.unwrap_or(0.0).max(0.0);
                strategy.quote_min_quote_age_ms = profile.quote.min_quote_age_ms;
                strategy.quote_expiry_suppression_ms = profile.quote.expiry_suppression_ms;
                strategy.quote_refresh_interval_ms = profile.quote.refresh_interval_ms;
                strategy
            }
            None => Self::with_defaults(),
        }
    }

    pub fn taker_fee_coeff(&self) -> f64 {
        self.config.taker_fee_coeff
    }

    pub fn estimate_taker_fee_usd(&self, price: f64, quantity: f64) -> f64 {
        if !(price.is_finite() && quantity.is_finite()) {
            return 0.0;
        }
        let notional_usd = price * quantity;
        if notional_usd <= 0.0 {
            return 0.0;
        }
        notional_usd * self.config.taker_fee_coeff * price * (1.0 - price)
    }

    fn position_for(
        &self,
        inventory: &InventorySnapshot,
        instrument_id: &InstrumentId,
    ) -> (f64, f64) {
        inventory
            .positions
            .iter()
            .find(|position| &position.instrument_id == instrument_id)
            .map_or((0.0, 0.0), |position| {
                (position.quantity, position.avg_price)
            })
    }

    fn gross_cost_usd(&self, inventory: &InventorySnapshot, market_id: &MarketId) -> f64 {
        inventory
            .positions
            .iter()
            .filter(|position| &position.market_id == market_id)
            .map(|position| position.quantity.abs() * position.avg_price)
            .sum()
    }

    fn taker_fee_usd(&self, price: f64, notional: f64) -> f64 {
        notional * self.config.taker_fee_coeff * price * (1.0 - price)
    }

    fn choose_clip_usd(&self, best_ask: f64) -> Option<f64> {
        if best_ask <= self.config.aggressive_price_max {
            Some(self.config.aggressive_clip_usd)
        } else if best_ask <= self.config.accumulate_price_max {
            Some(self.config.base_clip_usd)
        } else {
            None
        }
    }

    fn completion_pnl_per_share(&self, opposite_avg: f64, price: f64) -> f64 {
        1.0 - opposite_avg - price - self.config.taker_fee_coeff * price * (1.0 - price)
    }

    fn inventory_skew_scale(&self, this_qty: f64, opp_qty: f64) -> f64 {
        if self.quote_inventory_skew_bps <= 0.0 {
            return 1.0;
        }
        let imbalance = if opp_qty > 0.0 {
            (this_qty - opp_qty) / opp_qty.max(1.0)
        } else {
            this_qty
        };
        let skew = (imbalance.abs() * self.quote_inventory_skew_bps / 10_000.0).min(0.5);
        if imbalance >= 0.0 {
            (1.0 - skew).max(0.4)
        } else {
            (1.0 + skew).min(1.6)
        }
    }

    fn push_quote_ladder(
        &mut self,
        intents: &mut Vec<OrderIntent>,
        market_id: &MarketId,
        instrument_id: &InstrumentId,
        best_ask: f64,
        this_qty: f64,
        opp_qty: f64,
        opp_avg: f64,
        clip_usd: f64,
        quote_base_tag: &str,
        reason: &str,
        now_ms: EpochMillis,
    ) {
        let levels = self.quote_levels_per_side.clamp(1, 3);
        let inventory_scale = self.inventory_skew_scale(this_qty, opp_qty);
        let edge_bps = self.quote_min_edge_bps.max(0.0);
        let level_weight = (clip_usd / levels as f64).max(0.01);

        for level in 0..levels {
            let step_bps = edge_bps * (level as f64 + 1.0);
            let level_price = deterministic_quote_unit(
                (best_ask * (1.0 - step_bps / 10_000.0)).max(0.000_000_01),
            );
            if level_price <= 0.0 {
                continue;
            }
            let level_qty_scale = inventory_scale / (1.0 + level as f64 * 0.35);
            let level_notional =
                (level_weight * level_qty_scale).min(self.config.max_gross_cost_usd.max(0.01));
            let quantity = (level_notional / level_price).max(0.0);
            if quantity <= 0.0 {
                continue;
            }
            let level_tag = format!("{quote_base_tag}:{level}");
            let level_reason = if level == 0 {
                reason.to_string()
            } else {
                format!("{reason} ladder={level} opp_avg={opp_avg:.4}")
            };
            intents.push(self.build_order(
                market_id.clone(),
                instrument_id.clone(),
                level_price,
                quantity,
                level_tag,
                level_reason,
                now_ms,
            ));
        }
    }

    fn build_order(
        &mut self,
        market_id: MarketId,
        instrument_id: InstrumentId,
        price: f64,
        quantity: f64,
        quote_level_tag: String,
        reason: String,
        now_ms: EpochMillis,
    ) -> OrderIntent {
        let market_state = self.market_states.entry(market_id.clone()).or_default();
        market_state.last_seq = market_state.last_seq.saturating_add(1);
        market_state.last_fill_ms = Some(now_ms);
        let client_order_id = deterministic_client_order_id(
            "goat-pair",
            &market_id,
            &instrument_id,
            TradeSide::Buy,
            false,
            &quote_level_tag,
            price,
            quantity,
        );

        OrderIntent {
            client_order_id,
            market_id,
            instrument_id,
            side: TradeSide::Buy,
            limit_price: price,
            quantity,
            reduce_only: false,
            reason,
            quote_level_tag: Some(quote_level_tag),
            created_at_ms: now_ms,
            pair_id: None,
            kind: IntentKind::Entry,
        }
    }

    fn top_sides(&self, snapshot: &MarketSnapshot) -> Option<(InstrumentId, f64)> {
        let book = &snapshot.quote;
        let ask = book
            .best_ask
            .as_ref()
            .map(|level| level.price)
            .filter(|price| *price > 0.0)?;
        Some((snapshot.instrument_id.clone(), ask))
    }
}

impl Strategy for GoatPairStrategy {
    fn name(&self) -> &str {
        "goat_pair"
    }

    fn on_start(&mut self, _context: &StrategyContext) -> StrategyDecision {
        StrategyDecision::none()
    }

    fn on_market_snapshot(
        &mut self,
        context: &StrategyContext,
        snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        // TODO(2026-04-23): this strategy does not yet model hidden intent signals, whale event
        // clustering, or partial-orderbook liquidity shifts seen in real two-sided execution.
        if context.runtime_status != RuntimeStatus::Running {
            return StrategyDecision::none();
        }

        let (instrument_id, ask) = match self.top_sides(snapshot) {
            Some(next) => next,
            None => return StrategyDecision::none(),
        };

        let config = self.config;
        let cooldown_ms = self.cooldown_ms;
        let quote_min_quote_age_ms = self.quote_min_quote_age_ms;
        let quote_expiry_suppression_ms = self.quote_expiry_suppression_ms;
        let quote_refresh_interval_ms = self.quote_refresh_interval_ms;
        let mut sides: Vec<(InstrumentId, f64)> = {
            let market_state = self
                .market_states
                .entry(snapshot.market_id.clone())
                .or_default();
            let last_fill_ms = market_state.last_fill_ms;
            if cooldown_ms > 0
                && last_fill_ms.is_some_and(|ms| context.now_ms.saturating_sub(ms) < cooldown_ms)
            {
                market_state.asks.insert(instrument_id.clone(), ask);
                return StrategyDecision::none();
            }
            if quote_refresh_interval_ms.is_some_and(|refresh_interval_ms| {
                last_fill_ms
                    .is_some_and(|ms| context.now_ms.saturating_sub(ms) < refresh_interval_ms)
            }) {
                market_state.asks.insert(instrument_id.clone(), ask);
                return StrategyDecision::none();
            }
            if quote_min_quote_age_ms.is_some_and(|max_age_ms| {
                context.now_ms.saturating_sub(snapshot.quote.observed_at_ms) > max_age_ms
            }) {
                market_state.asks.insert(instrument_id.clone(), ask);
                return StrategyDecision::none();
            }
            if quote_expiry_suppression_ms.is_some_and(|expiry_ms| {
                last_fill_ms.is_some_and(|ms| context.now_ms.saturating_sub(ms) > expiry_ms)
            }) {
                market_state.asks.insert(instrument_id.clone(), ask);
                return StrategyDecision::none();
            }

            market_state.asks.insert(instrument_id.clone(), ask);
            if market_state.asks.len() < 2 {
                return StrategyDecision::none();
            }

            market_state
                .asks
                .iter()
                .map(|(id, level)| (id.clone(), *level))
                .collect()
        };
        sides.retain(|(_, level)| level.is_finite() && *level > 0.0);
        if sides.len() < 2 {
            return StrategyDecision::none();
        }

        let mut intents = Vec::new();
        let mut next_quote_seq = 0usize;
        let mut next_quote_tag = |label: &str| {
            next_quote_seq += 1;
            format!("lvl-{next_quote_seq}:{label}")
        };

        sides.sort_by(|left, right| left.1.total_cmp(&right.1));
        for (side_instrument, side_ask) in sides.clone() {
            let (this_qty, _) = self.position_for(&context.inventory, &side_instrument);
            let opposite_instrument = if side_instrument == sides[0].0 {
                sides[1].0.clone()
            } else {
                sides[0].0.clone()
            };
            let (opp_qty, opp_avg) = self.position_for(&context.inventory, &opposite_instrument);
            let opposite_has = opp_qty > 0.0;

            let clip_usd = self.choose_clip_usd(side_ask).or_else(|| {
                if opposite_has && opp_qty > this_qty {
                    let pnl = self.completion_pnl_per_share(opp_avg, side_ask);
                    if pnl >= config.completion_min_pnl_per_share {
                        Some(config.base_clip_usd)
                    } else {
                        None
                    }
                } else {
                    None
                }
            });

            let Some(clip_usd) = clip_usd else {
                continue;
            };

            if opposite_has {
                let projected_same = this_qty + clip_usd / side_ask;
                let ratio = projected_same / opp_qty.max(1e-9);
                if ratio > config.max_imbalance_ratio {
                    continue;
                }
            }

            let remaining_gross = config.max_gross_cost_usd
                - self.gross_cost_usd(&context.inventory, &snapshot.market_id);
            let remaining_qty = remaining_gross.max(0.0) / side_ask;
            if remaining_qty <= 0.0 || remaining_qty * side_ask < 5e-4 {
                return StrategyDecision::none();
            }

            let qty = (clip_usd / side_ask).min(remaining_qty);
            if qty <= 0.0 {
                return StrategyDecision::none();
            }

            let fee = self.taker_fee_usd(side_ask, qty * side_ask);
            let reason = if clip_usd >= config.aggressive_clip_usd {
                format!("goat-pair accumulate aggressive p={side_ask:.4},fee={fee:.4},qty={qty:.4}")
            } else if this_qty > 0.0 && opp_qty > this_qty {
                format!("goat-pair completion p={side_ask:.4},opp_avg={opp_avg:.4}")
            } else {
                format!("goat-pair accumulate p={side_ask:.4},qty={qty:.4}")
            };
            self.push_quote_ladder(
                &mut intents,
                &snapshot.market_id,
                &side_instrument,
                side_ask,
                this_qty,
                opp_qty,
                opp_avg,
                qty * side_ask,
                &next_quote_tag("goat-primary"),
                &reason,
                context.now_ms,
            );
            break;
        }

        if intents.is_empty() {
            return StrategyDecision::none();
        }
        StrategyDecision {
            intents,
            notes: Vec::new(),
        }
    }

    fn on_fill(
        &mut self,
        _context: &StrategyContext,
        fill: &crate::types::FillReport,
    ) -> StrategyDecision {
        let note = format!(
            "fill {} {} qty={}@{} fee={:.4}",
            fill.instrument_id,
            match fill.side {
                TradeSide::Buy => "BUY",
                TradeSide::Sell => "SELL",
            },
            fill.quantity,
            fill.price,
            fill.fee_usd
        );
        StrategyDecision {
            intents: Vec::new(),
            notes: vec![note],
        }
    }
}

#[derive(Debug)]
pub struct UnlawfulShearStrategy {
    config: UnlawfulShearConfig,
    cooldown_ms: u64,
    market_states: HashMap<MarketId, UnlawfulMarketState>,
}

impl UnlawfulShearStrategy {
    pub fn new(config: UnlawfulShearConfig, cooldown_ms: u64) -> Self {
        Self {
            config,
            cooldown_ms,
            market_states: HashMap::new(),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(
            UnlawfulShearConfig::from_env(),
            parse_u64("WHALE_PAIR_UNLAWFUL_SHEAR_COOLDOWN_MS", 400),
        )
    }

    pub fn with_profile(profile: Option<&StrategyProfile>) -> Self {
        match profile {
            Some(profile) => Self::new(
                profile.unlawful_shear_config(),
                profile.unlawful_shear_cooldown_ms(),
            ),
            None => Self::with_defaults(),
        }
    }

    pub fn taker_fee_coeff(&self) -> f64 {
        self.config.taker_fee_coeff
    }

    pub fn unlawful_gate_config(&self) -> UnlawfulGateConfig {
        UnlawfulGateConfig {
            regime_primary_hours_utc: self
                .config
                .regime_primary_hours_utc
                .iter()
                .map(|value| (*value).min(23) as u8)
                .collect(),
            regime_secondary_hours_utc: self
                .config
                .regime_secondary_hours_utc
                .iter()
                .map(|value| (*value).min(23) as u8)
                .collect(),
            allow_extreme_offhour_override: self.config.allow_extreme_offhour_override,
            entry_window_seconds: self.config.entry_window_seconds,
            cleanup_start_seconds: self.config.cleanup_start_seconds,
            close_start_seconds: self.config.close_start_seconds,
            merge_stall_seconds: self.config.merge_stall_seconds,
            entry_book_max_age_ms: self.config.entry_book_max_age_ms,
            entry_btc_signal_max_age_ms: self.config.entry_btc_signal_max_age_ms,
            primary_min_btc_realized_vol_5m_bps: self.config.primary_min_btc_realized_vol_5m_bps,
            primary_min_btc_realized_vol_15m_bps: self.config.primary_min_btc_realized_vol_15m_bps,
            primary_min_btc_trade_count_5m: self.config.primary_min_btc_trade_count_5m,
            secondary_min_btc_realized_vol_5m_bps: self
                .config
                .secondary_min_btc_realized_vol_5m_bps,
            secondary_min_btc_realized_vol_15m_bps: self
                .config
                .secondary_min_btc_realized_vol_15m_bps,
            secondary_min_btc_trade_count_5m: self.config.secondary_min_btc_trade_count_5m,
            override_min_btc_realized_vol_5m_bps: self.config.override_min_btc_realized_vol_5m_bps,
            override_min_btc_realized_vol_15m_bps: self
                .config
                .override_min_btc_realized_vol_15m_bps,
            override_min_btc_trade_count_5m: self.config.override_min_btc_trade_count_5m,
            entry_cheap_ask_max: self.config.entry_cheap_ask_max,
            entry_expensive_ask_min: self.config.entry_expensive_ask_min,
            entry_expensive_ask_max: self.config.entry_expensive_ask_max,
            entry_price_gap_min: self.config.entry_price_gap_min,
            preferred_cheap_ask_max: self.config.preferred_cheap_ask_max,
            preferred_expensive_ask_min: self.config.preferred_expensive_ask_min,
            preferred_expensive_ask_max: self.config.preferred_expensive_ask_max,
            preferred_price_gap_min: self.config.preferred_price_gap_min,
            hard_shock_return_30s_bps: self.config.hard_shock_return_30s_bps,
            hard_shock_return_60s_bps: self.config.hard_shock_return_60s_bps,
            soft_shock_return_30s_bps: self.config.soft_shock_return_30s_bps,
            soft_shock_return_60s_bps: self.config.soft_shock_return_60s_bps,
        }
    }

    fn best_bid(quote: &QuoteSnapshot) -> Option<f64> {
        quote
            .best_bid
            .as_ref()
            .map(|level| level.price)
            .filter(|price| price.is_finite() && *price > 0.0)
    }

    fn best_ask(quote: &QuoteSnapshot) -> Option<f64> {
        quote
            .best_ask
            .as_ref()
            .map(|level| level.price)
            .filter(|price| price.is_finite() && *price > 0.0)
    }

    fn buy_limit_price(&self, quote: &QuoteSnapshot, fallback_ask: f64) -> f64 {
        if self.config.maker_entry_pricing {
            Self::best_bid(quote).unwrap_or(fallback_ask)
        } else {
            fallback_ask
        }
    }

    fn position_state<'a>(
        &self,
        inventory: &'a InventorySnapshot,
        instrument_id: &InstrumentId,
    ) -> Option<&'a PositionState> {
        inventory
            .positions
            .iter()
            .find(|position| &position.instrument_id == instrument_id)
    }

    fn gross_cost_usd(&self, inventory: &InventorySnapshot, market_id: &MarketId) -> f64 {
        inventory
            .positions
            .iter()
            .filter(|position| &position.market_id == market_id)
            .map(|position| position.quantity.abs() * position.avg_price)
            .sum()
    }

    fn build_order(
        &mut self,
        market_id: MarketId,
        instrument_id: InstrumentId,
        side: TradeSide,
        price: f64,
        quantity: f64,
        reduce_only: bool,
        quote_level_tag: String,
        reason: String,
        now_ms: EpochMillis,
    ) -> OrderIntent {
        let market_state = self.market_states.entry(market_id.clone()).or_default();
        market_state.last_seq = market_state.last_seq.saturating_add(1);
        market_state.last_action_ms = Some(now_ms);
        let client_order_id = deterministic_client_order_id(
            "unlawful-shear",
            &market_id,
            &instrument_id,
            side,
            reduce_only,
            &quote_level_tag,
            price,
            quantity,
        );

        OrderIntent {
            client_order_id,
            market_id,
            instrument_id,
            side,
            limit_price: price,
            quantity,
            reduce_only,
            reason,
            quote_level_tag: Some(quote_level_tag),
            created_at_ms: now_ms,
            pair_id: None,
            // unlawful-shear strategy has no rescue intents (whales of this
            // type unwind via merge of paired buys, not sells). Defensively
            // tag the rare reduce_only path as Close so caps still bypass
            // correctly if it ever fires; otherwise Entry.
            kind: if reduce_only {
                IntentKind::Close
            } else {
                IntentKind::Entry
            },
        }
    }

    fn push_buy(
        &mut self,
        intents: &mut Vec<OrderIntent>,
        remaining_gross: &mut f64,
        market_id: &MarketId,
        instrument_id: &InstrumentId,
        price: f64,
        clip_usd: f64,
        quote_level_tag: String,
        reason: String,
        now_ms: EpochMillis,
    ) {
        if price <= 0.0 || clip_usd <= 0.0 || *remaining_gross <= 0.0 {
            return;
        }
        let notional = clip_usd.min(*remaining_gross);
        if notional < 1e-3 {
            return;
        }
        let child_notional = self.micro_clip_children(notional);
        let child_count = child_notional.len();
        for (idx, child_usd) in child_notional.into_iter().enumerate() {
            if child_usd <= 0.0 || *remaining_gross <= 0.0 {
                continue;
            }
            let child_usd = child_usd.min(*remaining_gross);
            if child_usd < 1e-3 {
                continue;
            }
            let quantity = child_usd / price;
            if quantity <= 0.0 {
                continue;
            }
            let child_quote_level_tag = if child_count > 1 {
                format!("{quote_level_tag}:child-{}/{}", idx + 1, child_count)
            } else {
                quote_level_tag.clone()
            };
            let child_reason = if child_count > 1 {
                format!(
                    "{reason} child={}/{} notional_usd={:.2}",
                    idx + 1,
                    child_count,
                    child_usd
                )
            } else {
                reason.clone()
            };
            intents.push(self.build_order(
                market_id.clone(),
                instrument_id.clone(),
                TradeSide::Buy,
                price,
                quantity,
                false,
                child_quote_level_tag,
                child_reason,
                now_ms,
            ));
            *remaining_gross -= quantity * price;
        }
    }

    fn micro_clip_children(&self, notional_usd: f64) -> Vec<f64> {
        if notional_usd <= 0.0 {
            return Vec::new();
        }
        let target_usd = self.config.micro_clip_target_usd.max(0.01);
        let min_usd = self.config.micro_clip_min_usd.max(0.01);
        let max_children = self.config.micro_clip_max_children.max(1);
        if max_children == 1 || notional_usd <= target_usd {
            return vec![notional_usd];
        }

        let mut children = ((notional_usd / target_usd).ceil() as usize).clamp(1, max_children);
        while children > 1 && notional_usd / children as f64 <= min_usd {
            children -= 1;
        }
        if children <= 1 {
            return vec![notional_usd];
        }

        let base = notional_usd / children as f64;
        vec![base; children]
    }

    fn push_sell(
        &mut self,
        intents: &mut Vec<OrderIntent>,
        market_id: &MarketId,
        instrument_id: &InstrumentId,
        price: f64,
        quantity: f64,
        quote_level_tag: String,
        reason: String,
        now_ms: EpochMillis,
    ) {
        if price <= 0.0 || quantity <= 0.0 {
            return;
        }
        intents.push(self.build_order(
            market_id.clone(),
            instrument_id.clone(),
            TradeSide::Sell,
            price,
            quantity,
            true,
            quote_level_tag,
            reason,
            now_ms,
        ));
    }

    fn window_progress(&self, context: &StrategyContext) -> Option<f64> {
        let market = context.market_context.as_ref()?;
        let start = market.event_start_time_ms?;
        let end = market.event_end_time_ms?;
        if end <= start {
            return None;
        }
        let elapsed = context.now_ms.saturating_sub(start);
        let total = end.saturating_sub(start).max(1);
        Some((elapsed as f64 / total as f64).clamp(0.0, 1.0))
    }

    fn window_elapsed_seconds(&self, context: &StrategyContext) -> Option<u64> {
        let market = context.market_context.as_ref()?;
        let start = market.event_start_time_ms?;
        if context.now_ms <= start {
            return Some(0);
        }
        let end = market.event_end_time_ms?;
        if end <= start {
            return None;
        }
        Some((context.now_ms.saturating_sub(start)) / 1_000)
    }

    fn determine_phase(&self, context: &StrategyContext) -> UnlawfulShearPhase {
        let elapsed = match self.window_elapsed_seconds(context) {
            Some(value) => value,
            None => {
                return match self.window_progress(context) {
                    Some(progress) if progress <= 0.22 => UnlawfulShearPhase::Early,
                    Some(progress) if progress <= 0.62 => UnlawfulShearPhase::Mid,
                    Some(progress) if progress <= 0.80 => UnlawfulShearPhase::Late,
                    Some(_) => UnlawfulShearPhase::VeryLate,
                    None => UnlawfulShearPhase::Unknown,
                };
            }
        };

        if elapsed <= 60 {
            UnlawfulShearPhase::Early
        } else if elapsed <= 180 {
            UnlawfulShearPhase::Mid
        } else if elapsed <= 240 {
            UnlawfulShearPhase::Late
        } else {
            UnlawfulShearPhase::VeryLate
        }
    }

    fn phase_clip_scale(&self, phase: UnlawfulShearPhase) -> f64 {
        match phase {
            UnlawfulShearPhase::Early => 0.35,
            UnlawfulShearPhase::Mid => 0.9,
            UnlawfulShearPhase::Late => 1.05,
            UnlawfulShearPhase::VeryLate => 1.35,
            UnlawfulShearPhase::Unknown => 1.0,
        }
    }

    fn window_at_or_past_end(&self, context: &StrategyContext) -> bool {
        let Some(market) = context.market_context.as_ref() else {
            return false;
        };
        let Some(end_ms) = market.event_end_time_ms else {
            return false;
        };
        context.now_ms >= end_ms
    }

    fn is_up_like(instrument_id: &InstrumentId) -> bool {
        let id = instrument_id.as_str().to_ascii_lowercase();
        id.contains("up") || id.contains("long") || id.contains("bull")
    }

    fn is_down_like(instrument_id: &InstrumentId) -> bool {
        let id = instrument_id.as_str().to_ascii_lowercase();
        id.contains("down") || id.contains("short") || id.contains("bear")
    }

    fn infer_winning_leg(
        &self,
        market: &crate::market_context::MarketContextRecord,
        cheap_id: &InstrumentId,
        expensive_id: &InstrumentId,
    ) -> SettlementLeg {
        let Some(final_price) = market.final_price else {
            return SettlementLeg::Unknown;
        };
        let Some(price_to_beat) = market.price_to_beat else {
            return SettlementLeg::Unknown;
        };
        let expect_up_wins = final_price >= price_to_beat;
        if expect_up_wins {
            if Self::is_up_like(cheap_id) && !Self::is_up_like(expensive_id) {
                SettlementLeg::Up
            } else if Self::is_up_like(expensive_id) && !Self::is_up_like(cheap_id) {
                SettlementLeg::Up
            } else {
                SettlementLeg::Unknown
            }
        } else if Self::is_down_like(cheap_id) && !Self::is_down_like(expensive_id) {
            SettlementLeg::Down
        } else if Self::is_down_like(expensive_id) && !Self::is_down_like(cheap_id) {
            SettlementLeg::Down
        } else {
            SettlementLeg::Unknown
        }
    }

    fn fallback_close_fraction(&self, phase: UnlawfulShearPhase, progress: Option<f64>) -> f64 {
        let base = match phase {
            UnlawfulShearPhase::Early => 0.25,
            UnlawfulShearPhase::Mid => 0.40,
            UnlawfulShearPhase::Late => 0.65,
            UnlawfulShearPhase::VeryLate => 0.90,
            UnlawfulShearPhase::Unknown => 0.55,
        };
        if progress.unwrap_or(0.0) >= 0.995 {
            1.0
        } else {
            base
        }
    }

    fn quantize_cleanup_qty(raw_qty: f64) -> f64 {
        if !raw_qty.is_finite() || raw_qty <= 0.0 {
            return 0.0;
        }
        // Coarse cleanup sizing reduces cancel/recreate churn from tiny inventory
        // changes in late-window salvage and fallback-close mode.
        let bucket = 0.25;
        let quantized = (raw_qty / bucket).floor() * bucket;
        if quantized >= bucket {
            quantized
        } else {
            0.0
        }
    }

    fn winning_instrument_id(
        &self,
        market: &crate::market_context::MarketContextRecord,
        cheap_id: &InstrumentId,
        expensive_id: &InstrumentId,
    ) -> Option<InstrumentId> {
        match self.infer_winning_leg(market, cheap_id, expensive_id) {
            SettlementLeg::Up => {
                if Self::is_up_like(cheap_id) {
                    Some(cheap_id.clone())
                } else if Self::is_up_like(expensive_id) {
                    Some(expensive_id.clone())
                } else {
                    None
                }
            }
            SettlementLeg::Down => {
                if Self::is_down_like(cheap_id) {
                    Some(cheap_id.clone())
                } else if Self::is_down_like(expensive_id) {
                    Some(expensive_id.clone())
                } else {
                    None
                }
            }
            SettlementLeg::Unknown => None,
        }
    }

    fn signal_mode_and_aggression(
        &self,
        context: &StrategyContext,
        elapsed_s: Option<u64>,
        has_inventory: bool,
        cheap_ask: f64,
        expensive_ask: f64,
        price_gap: f64,
    ) -> (
        UnlawfulExecutionMode,
        UnlawfulAggressionTier,
        Vec<String>,
        f64,
    ) {
        if let Some(signal) = context.unlawful_signal.as_ref() {
            let mut mode = signal.mode;
            let mut reasons = signal.gate_reasons.clone();
            let mut aggression = if matches!(
                mode,
                UnlawfulExecutionMode::Standby
                    | UnlawfulExecutionMode::Cleanup
                    | UnlawfulExecutionMode::Flatten
            ) || !signal.clip_scale.is_finite()
                || signal.clip_scale <= 0.0
            {
                UnlawfulAggressionTier::Suppressed
            } else if signal.clip_scale <= 0.6 {
                UnlawfulAggressionTier::Light
            } else if signal.clip_scale < 1.0 {
                UnlawfulAggressionTier::Normal
            } else {
                UnlawfulAggressionTier::Press
            };

            let geometry_clip_scale = self.geometry_clip_scale(cheap_ask, expensive_ask, price_gap);

            if geometry_clip_scale <= 0.0
                && matches!(
                    mode,
                    UnlawfulExecutionMode::Entry | UnlawfulExecutionMode::Manage
                )
            {
                reasons.push("entry geometry gate blocked (hard entry band invalid)".to_string());
                aggression = UnlawfulAggressionTier::Suppressed;
                mode = UnlawfulExecutionMode::Standby;
            }

            if has_inventory
                && signal.first_fill_ms.is_some()
                && signal.first_merge_ms.is_none()
                && signal.first_fill_ms.is_some_and(|filled_at| {
                    context.now_ms.saturating_sub(filled_at)
                        > self.config.merge_stall_seconds.saturating_mul(1000)
                })
            {
                reasons.push("merge stalled 60s".to_string());
                mode = UnlawfulExecutionMode::Cleanup;
                aggression = UnlawfulAggressionTier::Suppressed;
            }

            let clip_scale = if matches!(
                mode,
                UnlawfulExecutionMode::Standby
                    | UnlawfulExecutionMode::Cleanup
                    | UnlawfulExecutionMode::Flatten
            ) {
                0.0
            } else {
                signal.clip_scale.max(0.0) * geometry_clip_scale
            };
            return (mode, aggression, reasons, clip_scale);
        }

        let elapsed_s = match elapsed_s {
            Some(value) => value,
            None => 0,
        };

        let entry_window_seconds = self.config.entry_window_seconds.max(1);
        let cleanup_start_seconds = self
            .config
            .cleanup_start_seconds
            .max(entry_window_seconds.saturating_add(1));
        let close_start_seconds = self
            .config
            .close_start_seconds
            .max(cleanup_start_seconds.saturating_add(1));

        let mode = if context.market_context.is_none() {
            UnlawfulExecutionMode::Standby
        } else if elapsed_s >= close_start_seconds {
            UnlawfulExecutionMode::Flatten
        } else if elapsed_s >= cleanup_start_seconds {
            UnlawfulExecutionMode::Cleanup
        } else if elapsed_s <= entry_window_seconds {
            UnlawfulExecutionMode::Entry
        } else if has_inventory {
            UnlawfulExecutionMode::Manage
        } else {
            UnlawfulExecutionMode::Standby
        };

        let aggression = match mode {
            UnlawfulExecutionMode::Standby => UnlawfulAggressionTier::Suppressed,
            UnlawfulExecutionMode::Entry => UnlawfulAggressionTier::Normal,
            UnlawfulExecutionMode::Manage => UnlawfulAggressionTier::Normal,
            UnlawfulExecutionMode::Cleanup => UnlawfulAggressionTier::Suppressed,
            UnlawfulExecutionMode::Flatten => UnlawfulAggressionTier::Suppressed,
        };
        let reasons = vec!["unlawful_signal missing: fallback mode evaluation".to_string()];
        let clip_scale = if matches!(
            mode,
            UnlawfulExecutionMode::Entry | UnlawfulExecutionMode::Manage
        ) {
            self.geometry_clip_scale(cheap_ask, expensive_ask, price_gap)
        } else {
            0.0
        };

        (mode, aggression, reasons, clip_scale)
    }

    fn geometry_clip_scale(&self, cheap_ask: f64, expensive_ask: f64, price_gap: f64) -> f64 {
        let hard_band = cheap_ask <= self.config.entry_cheap_ask_max
            && expensive_ask >= self.config.entry_expensive_ask_min
            && expensive_ask <= self.config.entry_expensive_ask_max
            && price_gap >= self.config.entry_price_gap_min;
        let preferred_band = cheap_ask <= self.config.preferred_cheap_ask_max
            && expensive_ask >= self.config.preferred_expensive_ask_min
            && expensive_ask <= self.config.preferred_expensive_ask_max
            && price_gap >= self.config.preferred_price_gap_min;

        if hard_band {
            if preferred_band {
                1.0
            } else {
                0.6
            }
        } else {
            0.0
        }
    }

    fn aggression_clip_scale(&self, aggression: UnlawfulAggressionTier) -> f64 {
        match aggression {
            UnlawfulAggressionTier::Suppressed => 0.0,
            UnlawfulAggressionTier::Light => 0.6,
            UnlawfulAggressionTier::Normal => 1.0,
            UnlawfulAggressionTier::Press => 1.1,
        }
    }

    fn microstructure_adjusted_clip(
        &self,
        signal: Option<&UnlawfulSignalSnapshot>,
        leg: UnlawfulMicrostructureLeg,
        action: &str,
        requested_clip_usd: f64,
    ) -> (f64, Vec<String>) {
        if !self.config.microstructure_enabled || requested_clip_usd <= 0.0 {
            return (requested_clip_usd.max(0.0), Vec::new());
        }

        let Some(signal) = signal else {
            if self.config.microstructure_require_depth {
                return (
                    0.0,
                    vec![format!(
                        "microstructure blocked action={action} reason=missing signal"
                    )],
                );
            }
            return (requested_clip_usd, Vec::new());
        };

        let (label, spread, ask_notional_top3, imbalance) = match leg {
            UnlawfulMicrostructureLeg::Cheap => (
                "cheap",
                signal.book.cheap_spread,
                signal.book.cheap_ask_notional_top3,
                signal.book.cheap_depth_imbalance_top3,
            ),
            UnlawfulMicrostructureLeg::Expensive => (
                "expensive",
                signal.book.expensive_spread,
                signal.book.expensive_ask_notional_top3,
                signal.book.expensive_depth_imbalance_top3,
            ),
        };

        let mut reasons = Vec::new();
        if spread
            .is_some_and(|value| value.is_finite() && value > self.config.microstructure_max_spread)
        {
            return (
                0.0,
                vec![format!(
                    "microstructure blocked action={action} leg={label} spread={} max={}",
                    Self::fmt_opt_f64(spread, 4),
                    self.config.microstructure_max_spread
                )],
            );
        }

        let Some(ask_notional_top3) =
            ask_notional_top3.filter(|value| value.is_finite() && *value > 0.0)
        else {
            if self.config.microstructure_require_depth {
                return (
                    0.0,
                    vec![format!(
                        "microstructure blocked action={action} leg={label} reason=missing ask depth"
                    )],
                );
            }
            return (requested_clip_usd, reasons);
        };

        if ask_notional_top3 < self.config.microstructure_min_ask_notional_top3_usd {
            return (
                0.0,
                vec![format!(
                    "microstructure blocked action={action} leg={label} ask_notional_top3={ask_notional_top3:.2} min={:.2}",
                    self.config.microstructure_min_ask_notional_top3_usd
                )],
            );
        }

        let mut adjusted_clip = requested_clip_usd.min(
            ask_notional_top3
                * self
                    .config
                    .microstructure_max_clip_ask_notional_fraction
                    .clamp(0.0, 1.0),
        );
        if adjusted_clip < requested_clip_usd {
            reasons.push(format!(
                "microstructure scaled action={action} leg={label} requested={requested_clip_usd:.2} adjusted={adjusted_clip:.2} ask_notional_top3={ask_notional_top3:.2}"
            ));
        }

        if let Some(imbalance) = imbalance.filter(|value| value.is_finite()) {
            let threshold = self.config.microstructure_imbalance_threshold.abs();
            if threshold > 0.0 && imbalance <= -threshold {
                let before = adjusted_clip;
                adjusted_clip *= self.config.microstructure_weak_bid_scale.clamp(0.0, 1.0);
                if adjusted_clip < before {
                    reasons.push(format!(
                        "microstructure weak-bid scale action={action} leg={label} imbalance={imbalance:.3} adjusted={adjusted_clip:.2}"
                    ));
                }
            } else if threshold > 0.0 && imbalance >= threshold {
                let before = adjusted_clip;
                adjusted_clip *= self.config.microstructure_thin_ask_scale.clamp(0.0, 1.0);
                if adjusted_clip < before {
                    reasons.push(format!(
                        "microstructure thin-ask scale action={action} leg={label} imbalance={imbalance:.3} adjusted={adjusted_clip:.2}"
                    ));
                }
            }
        }

        if adjusted_clip < 1e-3 {
            reasons.push(format!(
                "microstructure blocked action={action} leg={label} reason=adjusted clip below floor"
            ));
            return (0.0, reasons);
        }

        (adjusted_clip, reasons)
    }

    fn fmt_opt_f64(value: Option<f64>, decimals: usize) -> String {
        value.map_or_else(|| "na".to_string(), |value| format!("{value:.decimals$}"))
    }

    fn fmt_opt_u64(value: Option<u64>) -> String {
        value.map_or_else(|| "na".to_string(), |value| value.to_string())
    }

    fn format_gate_reasons(reasons: &[String]) -> String {
        if reasons.is_empty() {
            "none".to_string()
        } else {
            reasons.join(";")
        }
    }

    fn format_unlawful_eval_summary(
        &self,
        context: &StrategyContext,
        snapshot: &MarketSnapshot,
        signal: Option<&UnlawfulSignalSnapshot>,
        cheap_id: &InstrumentId,
        cheap_bid: f64,
        cheap_ask: f64,
        expensive_id: &InstrumentId,
        expensive_bid: f64,
        expensive_ask: f64,
        price_gap: f64,
        hedge_ratio: f64,
        progress: Option<f64>,
        mode: UnlawfulExecutionMode,
        aggression: UnlawfulAggressionTier,
        signal_clip_scale: f64,
        phase_clip_scale: f64,
        buy_clip_scale: f64,
        has_inventory: bool,
        gate_reasons: &[String],
    ) -> String {
        let session_bucket = signal
            .map(|signal| format!("{:?}", signal.session_bucket))
            .unwrap_or_else(|| "Unknown".to_string());
        let books_fresh = signal
            .map(|signal| signal.book.books_fresh)
            .unwrap_or(false);
        let both_sides_present = signal
            .map(|signal| signal.book.both_sides_present)
            .unwrap_or(false);
        let book_age_ms = signal.and_then(|signal| {
            (signal.book.observed_at_ms > 0)
                .then_some(context.now_ms.saturating_sub(signal.book.observed_at_ms))
        });
        let btc_age_ms = signal.and_then(|signal| {
            (signal.btc.observed_at_ms > 0)
                .then_some(context.now_ms.saturating_sub(signal.btc.observed_at_ms))
        });

        format!(
            "unlawful eval market={} cheap_id={} expensive_id={} progress={} elapsed_s={} remaining_s={} session={} btc_last={} btc_vol_5m_bps={} btc_vol_15m_bps={} btc_trade_count_5m={} btc_trade_count_15m={} btc_ret_30s_bps={} btc_ret_60s_bps={} btc_age_ms={} cheap_bid={:.4} cheap_ask={:.4} expensive_bid={:.4} expensive_ask={:.4} gap={:.4} hedge_ratio={:.4} book_age_ms={} books_fresh={} both_sides_present={} activity_10s={} activity_30s={} activity_60s={} activity_age_ms={} first_fill_ms={} first_merge_ms={} mode={:?} aggression={:?} signal_clip_scale={:.3} phase_clip_scale={:.3} buy_clip_scale={:.3} market_has_inventory={} reasons={}",
            snapshot.market_id,
            cheap_id,
            expensive_id,
            Self::fmt_opt_f64(progress, 3),
            Self::fmt_opt_u64(signal.and_then(|signal| signal.elapsed_s)),
            Self::fmt_opt_u64(signal.and_then(|signal| signal.time_remaining_s)),
            session_bucket,
            Self::fmt_opt_f64(signal.and_then(|signal| signal.btc.last_price), 2),
            Self::fmt_opt_f64(signal.and_then(|signal| signal.btc.realized_vol_5m_bps), 2),
            Self::fmt_opt_f64(signal.and_then(|signal| signal.btc.realized_vol_15m_bps), 2),
            signal
                .map(|signal| signal.btc.trade_count_5m.to_string())
                .unwrap_or_else(|| "na".to_string()),
            signal
                .map(|signal| signal.btc.trade_count_15m.to_string())
                .unwrap_or_else(|| "na".to_string()),
            Self::fmt_opt_f64(signal.and_then(|signal| signal.btc.return_30s_bps), 2),
            Self::fmt_opt_f64(signal.and_then(|signal| signal.btc.return_60s_bps), 2),
            Self::fmt_opt_u64(btc_age_ms),
            cheap_bid,
            cheap_ask,
            expensive_bid,
            expensive_ask,
            price_gap,
            hedge_ratio,
            Self::fmt_opt_u64(book_age_ms),
            books_fresh,
            both_sides_present,
            signal
                .map(|signal| signal.activity.last_trade_event_count_10s.to_string())
                .unwrap_or_else(|| "na".to_string()),
            signal
                .map(|signal| signal.activity.last_trade_event_count_30s.to_string())
                .unwrap_or_else(|| "na".to_string()),
            signal
                .map(|signal| signal.activity.last_trade_event_count_60s.to_string())
                .unwrap_or_else(|| "na".to_string()),
            Self::fmt_opt_u64(signal.and_then(|signal| signal.activity.last_trade_event_age_ms)),
            Self::fmt_opt_u64(signal.and_then(|signal| signal.first_fill_ms)),
            Self::fmt_opt_u64(signal.and_then(|signal| signal.first_merge_ms)),
            mode,
            aggression,
            signal_clip_scale,
            phase_clip_scale,
            buy_clip_scale,
            has_inventory,
            Self::format_gate_reasons(gate_reasons),
        )
    }

    fn can_launch_mode_action(mode: UnlawfulExecutionMode, action: &str) -> bool {
        matches!(
            (mode, action),
            (UnlawfulExecutionMode::Entry, "core-entry")
                | (UnlawfulExecutionMode::Entry, "hedge-probe")
                | (UnlawfulExecutionMode::Entry, "early-probe")
                | (UnlawfulExecutionMode::Manage, "add-hedge")
                | (UnlawfulExecutionMode::Manage, "rebalance-core")
                | (UnlawfulExecutionMode::Manage, "rebalance-flip")
        )
    }
}

impl Strategy for UnlawfulShearStrategy {
    fn name(&self) -> &str {
        "unlawful_shear"
    }

    fn unlawful_gate_config(&self) -> Option<UnlawfulGateConfig> {
        Some(UnlawfulShearStrategy::unlawful_gate_config(self))
    }

    fn on_market_snapshot(
        &mut self,
        context: &StrategyContext,
        snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        if context.runtime_status != RuntimeStatus::Running {
            return StrategyDecision::none();
        }

        let (mut sides, last_action_ms) = {
            let market_state = self
                .market_states
                .entry(snapshot.market_id.clone())
                .or_default();
            let last_action_ms = market_state.last_action_ms.unwrap_or_default();
            market_state
                .quotes
                .insert(snapshot.instrument_id.clone(), snapshot.quote.clone());
            let sides = market_state
                .quotes
                .iter()
                .map(|(instrument_id, quote)| (instrument_id.clone(), quote.clone()))
                .collect::<Vec<_>>();
            (sides, last_action_ms)
        };

        if self.cooldown_ms > 0 && context.now_ms.saturating_sub(last_action_ms) < self.cooldown_ms
        {
            return StrategyDecision::none();
        }

        sides.retain(|(_, quote)| Self::best_ask(quote).is_some());
        if sides.len() < 2 {
            return StrategyDecision::none();
        }

        sides.sort_by(|left, right| {
            let left_ask = Self::best_ask(&left.1).unwrap_or(1.0);
            let right_ask = Self::best_ask(&right.1).unwrap_or(1.0);
            left_ask.total_cmp(&right_ask)
        });
        let (cheap_id, cheap_quote) = (&sides[0].0, &sides[0].1);
        let (expensive_id, expensive_quote) =
            (&sides[sides.len() - 1].0, &sides[sides.len() - 1].1);
        let Some(cheap_ask) = Self::best_ask(cheap_quote) else {
            return StrategyDecision::none();
        };
        let Some(expensive_ask) = Self::best_ask(expensive_quote) else {
            return StrategyDecision::none();
        };
        let price_gap = expensive_ask - cheap_ask;

        let cheap_position = self.position_state(&context.inventory, cheap_id);
        let expensive_position = self.position_state(&context.inventory, expensive_id);

        let cheap_qty = cheap_position
            .map(|position| position.quantity)
            .unwrap_or(0.0);
        let cheap_avg = cheap_position
            .map(|position| position.avg_price)
            .unwrap_or(0.0);
        let cheap_cost = cheap_qty * cheap_avg;
        let expensive_qty = expensive_position
            .map(|position| position.quantity)
            .unwrap_or(0.0);
        let expensive_avg = expensive_position
            .map(|position| position.avg_price)
            .unwrap_or(0.0);
        let expensive_cost = expensive_qty * expensive_avg;
        let paired = cheap_qty > 0.0 && expensive_qty > 0.0;
        let hedge_ratio = if expensive_cost > 0.0 {
            cheap_cost / expensive_cost.max(1e-9)
        } else {
            0.0
        };
        let has_inventory = paired || cheap_qty > 0.0 || expensive_qty > 0.0;
        let elapsed_s = self.window_elapsed_seconds(context);
        let (mode, aggression, gate_reasons, signal_clip_scale) = self.signal_mode_and_aggression(
            context,
            elapsed_s,
            has_inventory,
            cheap_ask,
            expensive_ask,
            price_gap,
        );
        let aggression_clip_scale = self.aggression_clip_scale(aggression);
        let buy_clip_scale = signal_clip_scale * aggression_clip_scale;

        let mut intents = Vec::new();
        let mut next_quote_seq = 0usize;
        let mut next_quote_tag = |label: &str| {
            next_quote_seq += 1;
            format!("lvl-{next_quote_seq}:{label}")
        };
        let progress = self.window_progress(context);
        let phase = self.determine_phase(context);
        let phase_clip_scale = self.phase_clip_scale(phase);
        let salvage_phase = matches!(
            phase,
            UnlawfulShearPhase::Late | UnlawfulShearPhase::VeryLate
        ) || progress.is_none();
        let suppressed_by_gate_reasons = !gate_reasons.is_empty();
        let expensive_bid = Self::best_bid(expensive_quote).unwrap_or(0.0);
        let cheap_bid = Self::best_bid(cheap_quote).unwrap_or(0.0);
        let expensive_buy_price = self.buy_limit_price(expensive_quote, expensive_ask);
        let cheap_buy_price = self.buy_limit_price(cheap_quote, cheap_ask);
        let mut notes = vec![self.format_unlawful_eval_summary(
            context,
            snapshot,
            context.unlawful_signal.as_ref(),
            cheap_id,
            cheap_bid,
            cheap_ask,
            expensive_id,
            expensive_bid,
            expensive_ask,
            price_gap,
            hedge_ratio,
            progress,
            mode,
            aggression,
            signal_clip_scale,
            phase_clip_scale,
            buy_clip_scale,
            has_inventory,
            &gate_reasons,
        )];
        let mut controller_blocked = false;
        let near_end =
            self.window_at_or_past_end(context) || progress.is_some_and(|value| value >= 0.96);
        let close_fraction = self.fallback_close_fraction(phase, progress);
        if near_end {
            if let Some(market) = context.market_context.as_ref() {
                if market.final_price.is_some() {
                    let winner = self.winning_instrument_id(market, cheap_id, expensive_id);
                    if let Some(winner_id) = winner {
                        if cheap_qty > 0.0 && cheap_bid > 0.0 {
                            self.push_sell(
                                &mut intents,
                                &snapshot.market_id,
                                cheap_id,
                                cheap_bid,
                                cheap_qty,
                                next_quote_tag("close-cheap-inferred"),
                                format!(
                                    "unlawful-shear end-window close winner={}",
                                    if cheap_id == &winner_id {
                                        "cheap"
                                    } else {
                                        "loser"
                                    },
                                ),
                                context.now_ms,
                            );
                        }
                        if expensive_qty > 0.0 && expensive_bid > 0.0 {
                            self.push_sell(
                                &mut intents,
                                &snapshot.market_id,
                                expensive_id,
                                expensive_bid,
                                expensive_qty,
                                next_quote_tag("close-expensive-inferred"),
                                format!(
                                    "unlawful-shear end-window close winner={}",
                                    if expensive_id == &winner_id {
                                        "expensive"
                                    } else {
                                        "loser"
                                    },
                                ),
                                context.now_ms,
                            );
                        }
                        notes.push(format!(
                            "end-window settlement anchor final={} beat={} winner={}",
                            market.final_price.unwrap_or(0.0),
                            market.price_to_beat.unwrap_or(0.0),
                            if Self::is_up_like(&winner_id) {
                                "up"
                            } else if Self::is_down_like(&winner_id) {
                                "down"
                            } else {
                                "unknown"
                            }
                        ));
                    } else {
                        notes.push(format!(
                            "end-window settlement attempted but winner leg could not be inferred from ids {} and {}",
                            cheap_id,
                            expensive_id
                        ));
                    }
                } else {
                    notes.push(format!(
                        "end-window settlement skipped: final_price missing for market={}",
                        snapshot.market_id
                    ));
                }
            } else {
                notes.push(format!(
                    "end-window settlement skipped: market context missing for market={}",
                    snapshot.market_id
                ));
            }
            if intents.is_empty() {
                let close_qty =
                    |qty: f64, fraction: f64| Self::quantize_cleanup_qty((qty * fraction).min(qty));
                if cheap_qty > 0.0 && cheap_bid > 0.0 {
                    let amount = close_qty(cheap_qty, close_fraction);
                    if amount > 0.0 {
                        self.push_sell(
                            &mut intents,
                            &snapshot.market_id,
                            cheap_id,
                            cheap_bid,
                            amount,
                            next_quote_tag("fallback-close-cheap"),
                            format!(
                                "unlawful-shear end-window fallback cleanup fraction={:.2}",
                                close_fraction
                            ),
                            context.now_ms,
                        );
                    }
                }
                if expensive_qty > 0.0 && expensive_bid > 0.0 {
                    let amount = close_qty(expensive_qty, close_fraction);
                    if amount > 0.0 {
                        self.push_sell(
                            &mut intents,
                            &snapshot.market_id,
                            expensive_id,
                            expensive_bid,
                            amount,
                            next_quote_tag("fallback-close-expensive"),
                            format!(
                                "unlawful-shear end-window fallback cleanup fraction={:.2}",
                                close_fraction
                            ),
                            context.now_ms,
                        );
                    }
                }
                if !intents.is_empty() {
                    notes.push(format!(
                        "end-window fallback cleanup executed fraction={:.2}",
                        close_fraction
                    ));
                }
            }
            if !intents.is_empty() {
                if let Some(market) = context.market_context.as_ref() {
                    if let Some(end_ms) = market.event_end_time_ms {
                        notes.push(format!("forced close at {} phase={:?}", end_ms, phase));
                    }
                }
                notes.push("unlawful-shear closing window for attribution".to_string());
                return StrategyDecision { intents, notes };
            }
        }

        if paired {
            if expensive_avg > 0.0
                && expensive_bid >= self.config.salvage_bid_floor
                && expensive_bid <= expensive_avg * (1.0 - self.config.salvage_drawdown_ratio)
                && expensive_cost > cheap_cost * 0.8
                && salvage_phase
            {
                let trim_qty = (expensive_qty * self.config.trim_clip_fraction).min(expensive_qty);
                self.push_sell(
                    &mut intents,
                    &snapshot.market_id,
                    expensive_id,
                    expensive_bid,
                    trim_qty,
                    next_quote_tag("salvage-expensive"),
                    format!(
                        "unlawful-shear salvage expensive bid={:.4} avg={:.4}",
                        expensive_bid, expensive_avg
                    ),
                    context.now_ms,
                );
            } else if cheap_avg > 0.0
                && cheap_bid >= self.config.salvage_bid_floor
                && cheap_bid <= cheap_avg * (1.0 - self.config.salvage_drawdown_ratio)
                && cheap_cost > expensive_cost * self.config.target_hedge_ratio_max
                && salvage_phase
            {
                let trim_qty = (cheap_qty * self.config.trim_clip_fraction).min(cheap_qty);
                self.push_sell(
                    &mut intents,
                    &snapshot.market_id,
                    cheap_id,
                    cheap_bid,
                    trim_qty,
                    next_quote_tag("salvage-cheap"),
                    format!(
                        "unlawful-shear salvage hedge bid={:.4} avg={:.4}",
                        cheap_bid, cheap_avg
                    ),
                    context.now_ms,
                );
            }
        }

        let mut remaining_gross = self.config.max_gross_cost_usd
            - self.gross_cost_usd(&context.inventory, &snapshot.market_id);
        let record_action_block = |action: &str, action_note: &str, notes: &mut Vec<String>| {
            notes.push(format!(
                "unlawful gate blocked action={} mode={:?} reason={}",
                action, mode, action_note
            ));
        };

        let core_candidate = price_gap >= self.config.min_price_gap
            && expensive_ask >= self.config.core_price_min
            && expensive_ask <= self.config.core_price_max;
        let cheap_candidate = cheap_ask <= self.config.cheap_hedge_price_max;
        if expensive_cost <= 0.0 && core_candidate {
            let core_clip_base = self.config.core_clip_usd * phase_clip_scale;
            let core_clip_usd = match phase {
                UnlawfulShearPhase::VeryLate => core_clip_base * 1.25,
                UnlawfulShearPhase::Early => self.config.probe_clip_usd.max(core_clip_base),
                _ => core_clip_base,
            };
            if Self::can_launch_mode_action(mode, "core-entry") && buy_clip_scale > 0.0 {
                let requested_clip = core_clip_usd * buy_clip_scale;
                let (clip_usd, micro_notes) = self.microstructure_adjusted_clip(
                    context.unlawful_signal.as_ref(),
                    UnlawfulMicrostructureLeg::Expensive,
                    "core-entry",
                    requested_clip,
                );
                if clip_usd <= 0.0 {
                    controller_blocked = true;
                    notes.extend(micro_notes);
                    record_action_block("core-entry", "microstructure blocked", &mut notes);
                } else {
                    notes.extend(micro_notes);
                    self.push_buy(
                        &mut intents,
                        &mut remaining_gross,
                        &snapshot.market_id,
                        expensive_id,
                        expensive_buy_price,
                        clip_usd,
                        next_quote_tag("core-entry"),
                        format!(
                            "unlawful-shear core-entry gap={:.4} ask={:.4} limit={:.4}",
                            price_gap, expensive_ask, expensive_buy_price
                        ),
                        context.now_ms,
                    );
                }
            } else {
                record_action_block(
                    "core-entry",
                    if buy_clip_scale <= 0.0 {
                        "zero clip scale"
                    } else {
                        "mode disallowed"
                    },
                    &mut notes,
                );
            }
        }

        if cheap_cost <= 0.0 && cheap_candidate {
            let hedge_probe_usd = if matches!(phase, UnlawfulShearPhase::Early) {
                self.config.probe_clip_usd * 0.75
            } else {
                (self.config.hedge_clip_usd * phase_clip_scale).min(self.config.hedge_clip_usd)
            };
            if Self::can_launch_mode_action(mode, "hedge-probe") && buy_clip_scale > 0.0 {
                let requested_clip = hedge_probe_usd * buy_clip_scale;
                let (clip_usd, micro_notes) = self.microstructure_adjusted_clip(
                    context.unlawful_signal.as_ref(),
                    UnlawfulMicrostructureLeg::Cheap,
                    "hedge-probe",
                    requested_clip,
                );
                if clip_usd <= 0.0 {
                    controller_blocked = true;
                    notes.extend(micro_notes);
                    record_action_block("hedge-probe", "microstructure blocked", &mut notes);
                } else {
                    notes.extend(micro_notes);
                    self.push_buy(
                        &mut intents,
                        &mut remaining_gross,
                        &snapshot.market_id,
                        cheap_id,
                        cheap_buy_price,
                        clip_usd,
                        next_quote_tag("hedge-probe"),
                        format!(
                            "unlawful-shear hedge-probe gap={:.4} ask={:.4} limit={:.4}",
                            price_gap, cheap_ask, cheap_buy_price
                        ),
                        context.now_ms,
                    );
                }
            } else {
                record_action_block(
                    "hedge-probe",
                    if buy_clip_scale <= 0.0 {
                        "zero clip scale"
                    } else {
                        "mode disallowed"
                    },
                    &mut notes,
                );
            }
        }

        if paired {
            if hedge_ratio < self.config.target_hedge_ratio_min && cheap_candidate {
                let hedge_clip_usd = match phase {
                    UnlawfulShearPhase::VeryLate => self.config.hedge_clip_usd * 1.35,
                    UnlawfulShearPhase::Late => self.config.hedge_clip_usd * 1.15,
                    _ => self.config.hedge_clip_usd * phase_clip_scale,
                };
                if Self::can_launch_mode_action(mode, "add-hedge") && buy_clip_scale > 0.0 {
                    let requested_clip = hedge_clip_usd * buy_clip_scale;
                    let (clip_usd, micro_notes) = self.microstructure_adjusted_clip(
                        context.unlawful_signal.as_ref(),
                        UnlawfulMicrostructureLeg::Cheap,
                        "add-hedge",
                        requested_clip,
                    );
                    if clip_usd <= 0.0 {
                        controller_blocked = true;
                        notes.extend(micro_notes);
                        record_action_block("add-hedge", "microstructure blocked", &mut notes);
                    } else {
                        notes.extend(micro_notes);
                        self.push_buy(
                            &mut intents,
                            &mut remaining_gross,
                            &snapshot.market_id,
                            cheap_id,
                            cheap_buy_price,
                            clip_usd,
                            next_quote_tag("add-hedge"),
                            format!(
                                "unlawful-shear add-hedge ratio={:.4} ask={:.4} limit={:.4}",
                                hedge_ratio, cheap_ask, cheap_buy_price
                            ),
                            context.now_ms,
                        );
                    }
                } else {
                    record_action_block(
                        "add-hedge",
                        if buy_clip_scale <= 0.0 {
                            "zero clip scale"
                        } else {
                            "mode disallowed"
                        },
                        &mut notes,
                    );
                }
            } else if hedge_ratio > self.config.target_hedge_ratio_max && core_candidate {
                let rebalance_clip_usd = match phase {
                    UnlawfulShearPhase::VeryLate => self.config.rebalance_clip_usd * 1.35,
                    UnlawfulShearPhase::Late => self.config.rebalance_clip_usd * 1.2,
                    _ => self.config.rebalance_clip_usd * phase_clip_scale,
                };
                if Self::can_launch_mode_action(mode, "rebalance-core") && buy_clip_scale > 0.0 {
                    let requested_clip = rebalance_clip_usd * buy_clip_scale;
                    let (clip_usd, micro_notes) = self.microstructure_adjusted_clip(
                        context.unlawful_signal.as_ref(),
                        UnlawfulMicrostructureLeg::Expensive,
                        "rebalance-core",
                        requested_clip,
                    );
                    if clip_usd <= 0.0 {
                        controller_blocked = true;
                        notes.extend(micro_notes);
                        record_action_block("rebalance-core", "microstructure blocked", &mut notes);
                    } else {
                        notes.extend(micro_notes);
                        self.push_buy(
                            &mut intents,
                            &mut remaining_gross,
                            &snapshot.market_id,
                            expensive_id,
                            expensive_buy_price,
                            clip_usd,
                            next_quote_tag("rebalance-core"),
                            format!(
                                "unlawful-shear rebalance-core ratio={:.4} ask={:.4} limit={:.4}",
                                hedge_ratio, expensive_ask, expensive_buy_price
                            ),
                            context.now_ms,
                        );
                    }
                } else {
                    record_action_block(
                        "rebalance-core",
                        if buy_clip_scale <= 0.0 {
                            "zero clip scale"
                        } else {
                            "mode disallowed"
                        },
                        &mut notes,
                    );
                }
            } else if core_candidate
                && cheap_candidate
                && price_gap >= self.config.min_price_gap * 1.5
                && remaining_gross > self.config.probe_clip_usd
            {
                let target_id = if expensive_cost <= cheap_cost {
                    expensive_id
                } else {
                    cheap_id
                };
                let target_price = if target_id == expensive_id {
                    expensive_buy_price
                } else {
                    cheap_buy_price
                };
                if Self::can_launch_mode_action(mode, "rebalance-flip") && buy_clip_scale > 0.0 {
                    let requested_clip = match phase {
                        UnlawfulShearPhase::VeryLate => {
                            self.config.rebalance_clip_usd * 1.35 * buy_clip_scale
                        }
                        UnlawfulShearPhase::Late => {
                            self.config.rebalance_clip_usd * 1.2 * buy_clip_scale
                        }
                        _ => self.config.rebalance_clip_usd * phase_clip_scale * buy_clip_scale,
                    };
                    let target_leg = if target_id == expensive_id {
                        UnlawfulMicrostructureLeg::Expensive
                    } else {
                        UnlawfulMicrostructureLeg::Cheap
                    };
                    let (clip_usd, micro_notes) = self.microstructure_adjusted_clip(
                        context.unlawful_signal.as_ref(),
                        target_leg,
                        "rebalance-flip",
                        requested_clip,
                    );
                    if clip_usd <= 0.0 {
                        controller_blocked = true;
                        notes.extend(micro_notes);
                        record_action_block("rebalance-flip", "microstructure blocked", &mut notes);
                    } else {
                        notes.extend(micro_notes);
                        self.push_buy(
                            &mut intents,
                            &mut remaining_gross,
                            &snapshot.market_id,
                            target_id,
                            target_price,
                            clip_usd,
                            next_quote_tag("rebalance-flip"),
                            format!(
                                "unlawful-shear high-flip rebalance gap={:.4} target={}",
                                price_gap, target_id
                            ),
                            context.now_ms,
                        );
                    }
                } else {
                    record_action_block(
                        "rebalance-flip",
                        if buy_clip_scale <= 0.0 {
                            "zero clip scale"
                        } else {
                            "mode disallowed"
                        },
                        &mut notes,
                    );
                }
            }
        } else if cheap_candidate
            && cheap_cost <= 0.0
            && !intents
                .iter()
                .any(|intent| intent.side == TradeSide::Buy && intent.instrument_id == *cheap_id)
        {
            if Self::can_launch_mode_action(mode, "early-probe") && buy_clip_scale > 0.0 {
                let requested_clip =
                    (self.config.probe_clip_usd * phase_clip_scale).max(1.0) * buy_clip_scale;
                let (clip_usd, micro_notes) = self.microstructure_adjusted_clip(
                    context.unlawful_signal.as_ref(),
                    UnlawfulMicrostructureLeg::Cheap,
                    "early-probe",
                    requested_clip,
                );
                if clip_usd <= 0.0 {
                    controller_blocked = true;
                    notes.extend(micro_notes);
                    record_action_block("early-probe", "microstructure blocked", &mut notes);
                } else {
                    notes.extend(micro_notes);
                    self.push_buy(
                        &mut intents,
                        &mut remaining_gross,
                        &snapshot.market_id,
                        cheap_id,
                        cheap_buy_price,
                        clip_usd,
                        next_quote_tag("early-probe"),
                        format!(
                            "unlawful-shear early-probe ask={:.4} limit={:.4} gap={:.4}",
                            cheap_ask, cheap_buy_price, price_gap
                        ),
                        context.now_ms,
                    );
                }
            } else {
                record_action_block(
                    "early-probe",
                    if buy_clip_scale <= 0.0 {
                        "zero clip scale"
                    } else {
                        "mode disallowed"
                    },
                    &mut notes,
                );
            }
        }

        if intents.is_empty() {
            let suppression_mode = matches!(
                mode,
                UnlawfulExecutionMode::Standby
                    | UnlawfulExecutionMode::Cleanup
                    | UnlawfulExecutionMode::Flatten
            );
            let suppression_due_to_scaling = buy_clip_scale <= 0.0
                && (matches!(
                    mode,
                    UnlawfulExecutionMode::Entry | UnlawfulExecutionMode::Manage
                ));
            if suppressed_by_gate_reasons || suppression_mode || suppression_due_to_scaling {
                notes.push(format!(
                    "unlawful gate suppressed market mode={:?} actions_blocked",
                    mode
                ));
                return StrategyDecision { intents, notes };
            }
            if controller_blocked {
                notes.push(format!(
                    "unlawful microstructure controller suppressed market mode={:?}",
                    mode
                ));
                return StrategyDecision { intents, notes };
            }
            return StrategyDecision::none();
        }

        if let Some(market) = &context.market_context {
            if let Some(price_to_beat) = market.price_to_beat {
                notes.push(format!(
                    "gamma-context price_to_beat={:.4} final_price={:?}",
                    price_to_beat, market.final_price
                ));
            }
        }
        notes.push("unlawful-shear strategy fired from paired-book geometry".to_string());
        let phase_name = match phase {
            UnlawfulShearPhase::Early => "early",
            UnlawfulShearPhase::Mid => "mid",
            UnlawfulShearPhase::Late => "late",
            UnlawfulShearPhase::VeryLate => "very_late",
            UnlawfulShearPhase::Unknown => "unknown",
        };
        notes.push(format!(
            "phase={} clip_scale={:.2} elapsed_sec={:?}",
            phase_name,
            phase_clip_scale,
            self.window_elapsed_seconds(context)
        ));
        StrategyDecision { intents, notes }
    }

    fn on_fill(
        &mut self,
        _context: &StrategyContext,
        fill: &crate::types::FillReport,
    ) -> StrategyDecision {
        let note = format!(
            "unlawful-shear fill {} {} qty={}@{} fee={:.4}",
            fill.instrument_id,
            match fill.side {
                TradeSide::Buy => "BUY",
                TradeSide::Sell => "SELL",
            },
            fill.quantity,
            fill.price,
            fill.fee_usd
        );
        StrategyDecision {
            intents: Vec::new(),
            notes: vec![note],
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct NoopStrategy;

impl Strategy for NoopStrategy {
    fn name(&self) -> &str {
        "noop"
    }
}

fn parse_f64(key: &str, default: f64) -> f64 {
    env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<f64>().ok())
        .unwrap_or(default)
}

fn parse_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(default)
}

fn parse_usize(key: &str, default: usize) -> usize {
    env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(default)
}

fn env_or_profile_f64(key: &str, profile: Option<f64>, default: f64) -> f64 {
    env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<f64>().ok())
        .unwrap_or(profile.unwrap_or(default))
}

fn env_or_profile_usize(key: &str, profile: Option<usize>, default: usize) -> usize {
    env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(profile.unwrap_or(default))
}

fn env_or_profile_bool(key: &str, profile: Option<bool>, default: bool) -> bool {
    env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<bool>().ok())
        .unwrap_or(profile.unwrap_or(default))
}

fn env_or_profile_u64(key: &str, profile: Option<u64>, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(profile.unwrap_or(default))
}

fn sorted_unique_u32_vec(mut values: Vec<u32>) -> Vec<u32> {
    values.retain(|value| *value < 24);
    values.sort_unstable();
    values.dedup();
    values
}

fn normalize_unlawful_invariants(mut config: UnlawfulShearConfig) -> UnlawfulShearConfig {
    config.regime_primary_hours_utc = sorted_unique_u32_vec(config.regime_primary_hours_utc);
    if config.regime_primary_hours_utc.is_empty() {
        config.regime_primary_hours_utc = vec![10, 11, 19, 22, 23];
    }

    config.regime_secondary_hours_utc = sorted_unique_u32_vec(config.regime_secondary_hours_utc);
    if config.regime_secondary_hours_utc.is_empty() {
        config.regime_secondary_hours_utc = vec![0, 9, 12, 20, 21];
    }

    config.entry_window_seconds = config.entry_window_seconds.max(1);
    config.cleanup_start_seconds = config
        .cleanup_start_seconds
        .max(config.entry_window_seconds.saturating_add(1));
    config.close_start_seconds = config
        .close_start_seconds
        .max(config.cleanup_start_seconds.saturating_add(1));

    config.merge_stall_seconds = config.merge_stall_seconds.max(1);
    config.entry_book_max_age_ms = config.entry_book_max_age_ms.max(1);
    config.entry_btc_signal_max_age_ms = config.entry_btc_signal_max_age_ms.max(1);
    config.micro_clip_target_usd = config.micro_clip_target_usd.max(0.01);
    config.micro_clip_min_usd = config.micro_clip_min_usd.max(0.01);
    config.micro_clip_max_children = config.micro_clip_max_children.max(1);
    if config.micro_clip_min_usd > config.micro_clip_target_usd {
        config.micro_clip_target_usd = config.micro_clip_min_usd;
    }
    config.microstructure_max_spread = config.microstructure_max_spread.max(0.001);
    config.microstructure_min_ask_notional_top3_usd =
        config.microstructure_min_ask_notional_top3_usd.max(0.0);
    config.microstructure_max_clip_ask_notional_fraction = config
        .microstructure_max_clip_ask_notional_fraction
        .clamp(0.0, 1.0);
    config.microstructure_imbalance_threshold = config
        .microstructure_imbalance_threshold
        .abs()
        .clamp(0.0, 1.0);
    config.microstructure_weak_bid_scale = config.microstructure_weak_bid_scale.clamp(0.0, 1.0);
    config.microstructure_thin_ask_scale = config.microstructure_thin_ask_scale.clamp(0.0, 1.0);

    config
}

#[cfg(test)]
mod tests;
