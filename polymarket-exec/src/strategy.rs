//! Strategy implementations and decision logic for unlawful_shear and baseline modes.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::inventory::{InventorySnapshot, PositionState};
use crate::market_context::MarketContextRecord;
use crate::quote_engine::QuoteEngineConfig;
use crate::signals::UnlawfulGateConfig;
use crate::types::{
    BookLevel, ClientOrderId, EpochMillis, InstrumentId, MarketId, MarketSnapshot, OrderIntent,
    QuoteSnapshot, RuntimeStatus, TradeSide,
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
        }
    }
}

#[derive(Debug, Clone)]
pub struct MarketActivitySignal {
    pub last_trade_event_count_10s: u32,
    pub last_trade_event_count_30s: u32,
    pub last_trade_event_count_60s: u32,
    pub last_trade_event_age_ms: Option<u64>,
}

impl Default for MarketActivitySignal {
    fn default() -> Self {
        Self {
            last_trade_event_count_10s: 0,
            last_trade_event_count_30s: 0,
            last_trade_event_count_60s: 0,
            last_trade_event_age_ms: None,
        }
    }
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
    pub max_gross_cost_usd: f64,
    pub max_leg_cost_usd: f64,
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
    pub cooldown_ms: u64,
    pub taker_fee_coeff: f64,
    pub allow_single_leg_entry: bool,
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
            max_gross_cost_usd: parse_f64("WHALE_PAIR_BTC_5M_MM_MAX_GROSS_COST_USD", 20.0),
            max_leg_cost_usd: parse_f64("WHALE_PAIR_BTC_5M_MM_MAX_LEG_COST_USD", 10.0),
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
            cooldown_ms: parse_u64("WHALE_PAIR_BTC_5M_MM_COOLDOWN_MS", 1_000),
            taker_fee_coeff: parse_f64("WHALE_PAIR_TAKER_FEE_COEFF", 0.072),
            allow_single_leg_entry: parse_bool(
                "WHALE_PAIR_BTC_5M_MM_ALLOW_SINGLE_LEG_ENTRY",
                false,
            ),
        };
        Self {
            base_clip_usd: config.base_clip_usd.max(0.01),
            min_clip_usd: config.min_clip_usd.max(0.01),
            max_clip_usd: config.max_clip_usd.max(config.min_clip_usd.max(0.01)),
            liquidity_clip_fraction: config.liquidity_clip_fraction.clamp(0.0, 1.0),
            hedge_rescue_clip_usd: config.hedge_rescue_clip_usd.max(0.01),
            max_gross_cost_usd: config.max_gross_cost_usd.max(0.01),
            max_leg_cost_usd: config.max_leg_cost_usd.max(0.01),
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
            cooldown_ms: config.cooldown_ms,
            taker_fee_coeff: config.taker_fee_coeff.max(0.0),
            allow_single_leg_entry: config.allow_single_leg_entry,
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
    quotes: HashMap<InstrumentId, QuoteSnapshot>,
    last_action_ms: Option<EpochMillis>,
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

fn deterministic_client_order_id(
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
}

#[derive(Debug)]
pub enum StrategyMode {
    Goat(GoatPairStrategy),
    Btc5mMm(Btc5mMmStrategy),
    UnlawfulShear(UnlawfulShearStrategy),
    Noop(NoopStrategy),
}

impl StrategyMode {
    pub fn from_name(name: &str, profile: Option<&StrategyProfile>) -> Self {
        match name {
            "btc_5m_mm" => Self::Btc5mMm(Btc5mMmStrategy::with_defaults()),
            "goat_pair" => Self::Goat(GoatPairStrategy::with_profile(profile)),
            "noop" => Self::Noop(NoopStrategy),
            "unlawful_shear" => Self::UnlawfulShear(UnlawfulShearStrategy::with_profile(profile)),
            _ => Self::UnlawfulShear(UnlawfulShearStrategy::with_defaults()),
        }
    }

    pub fn taker_fee_coeff(&self) -> f64 {
        match self {
            Self::Btc5mMm(strategy) => strategy.taker_fee_coeff(),
            Self::Goat(strategy) => strategy.taker_fee_coeff(),
            Self::UnlawfulShear(strategy) => strategy.taker_fee_coeff(),
            Self::Noop(_) => 0.0,
        }
    }

    pub fn unlawful_gate_config(&self) -> Option<UnlawfulGateConfig> {
        match self {
            Self::Btc5mMm(_) => None,
            Self::Goat(_) => None,
            Self::Noop(_) => None,
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
            Self::Noop(strategy) => strategy.on_fill(context, fill),
        }
    }
}

#[derive(Debug)]
pub struct Btc5mMmStrategy {
    config: Btc5mMmConfig,
    market_states: HashMap<MarketId, Btc5mMmMarketState>,
}

impl Btc5mMmStrategy {
    pub fn new(config: Btc5mMmConfig) -> Self {
        Self {
            config,
            market_states: HashMap::new(),
        }
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
        deterministic_quote_unit((value / tick).floor() * tick)
    }

    fn maker_bid_price(&self, quote: &QuoteSnapshot, max_bid: f64) -> Option<f64> {
        let best_bid = Self::best_bid(quote)?;
        let best_ask = Self::best_ask(quote)?;
        let maker_cap = best_ask - self.config.maker_price_tick * self.config.maker_safety_ticks;
        let price = Self::floor_to_tick(
            best_bid.min(max_bid).min(maker_cap),
            self.config.maker_price_tick,
        );
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
        left_quote: &QuoteSnapshot,
        right_quote: &QuoteSnapshot,
        requested_clip_usd: f64,
    ) -> Option<f64> {
        let left_bid = Self::best_bid(left_quote)?;
        let right_bid = Self::best_bid(right_quote)?;
        let reference_bid = left_bid.max(right_bid);
        if reference_bid <= 0.0 {
            return None;
        }
        let clip_usd = self.paired_entry_clip_usd(left_quote, right_quote, requested_clip_usd);
        let raw_quantity = clip_usd / reference_bid;
        let min_quantity =
            self.config.venue_min_order_quantity * self.config.entry_min_size_multiplier;
        let max_quantity = self.config.max_clip_usd / reference_bid;
        Some(raw_quantity.max(min_quantity).min(max_quantity).max(0.0))
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

    fn fair_values(&self, left: &QuoteSnapshot, right: &QuoteSnapshot) -> Option<(f64, f64)> {
        let left_mid = (Self::best_bid(left)? + Self::best_ask(left)?) * 0.5;
        let right_mid = (Self::best_bid(right)? + Self::best_ask(right)?) * 0.5;
        let sum = left_mid + right_mid;
        if sum.is_finite() && sum > 0.0 {
            Some((left_mid / sum, right_mid / sum))
        } else {
            None
        }
    }

    fn inventory_skew(&self, leg_cost: f64, gross_cost: f64) -> f64 {
        if self.config.inventory_skew_bps <= 0.0 {
            return 0.0;
        }
        let leg_pressure = (leg_cost / self.config.max_leg_cost_usd).clamp(0.0, 1.5);
        let gross_pressure = (gross_cost / self.config.max_gross_cost_usd).clamp(0.0, 1.5);
        (leg_pressure + gross_pressure) * self.config.inventory_skew_bps / 10_000.0
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
        now_ms: EpochMillis,
    ) -> OrderIntent {
        OrderIntent {
            client_order_id: deterministic_client_order_id(
                "btc-5m-mm",
                &market_id,
                &instrument_id,
                side,
                reduce_only,
                &quote_level_tag,
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
        }
    }

    fn build_bid_intent_for_quantity(
        &self,
        market_id: &MarketId,
        instrument_id: &InstrumentId,
        quote: &QuoteSnapshot,
        fair: f64,
        leg_cost: f64,
        gross_cost: f64,
        quantity: f64,
        edge_bps: f64,
        quote_level_tag: &str,
        reason_prefix: &str,
        now_ms: EpochMillis,
    ) -> Option<OrderIntent> {
        let best_bid = Self::best_bid(quote)?;
        let edge = edge_bps / 10_000.0;
        let max_bid = deterministic_quote_unit(
            (fair - edge - self.inventory_skew(leg_cost, gross_cost)).clamp(0.0, 0.99),
        );
        if best_bid <= 0.0 || best_bid > max_bid {
            return None;
        }
        let Some(bid_price) = self.maker_bid_price(quote, max_bid) else {
            return None;
        };
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
            now_ms,
        ))
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
        if context.runtime_status != RuntimeStatus::Running {
            return StrategyDecision::none();
        }
        if self.config.cooldown_ms > 0
            && self
                .market_states
                .get(&snapshot.market_id)
                .and_then(|state| state.last_action_ms)
                .is_some_and(|last_ms| {
                    context.now_ms.saturating_sub(last_ms) < self.config.cooldown_ms
                })
        {
            return StrategyDecision::none();
        }

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
            return StrategyDecision::none();
        }
        let Some((left_fair, right_fair)) = self.fair_values(&left_quote, &right_quote) else {
            return StrategyDecision::none();
        };

        let gross_cost = Self::gross_cost_usd(&context.inventory, &snapshot.market_id);
        let (left_qty, left_avg, _) =
            Self::position_for(&context.inventory, &snapshot.market_id, &left_id);
        let (right_qty, right_avg, _) =
            Self::position_for(&context.inventory, &snapshot.market_id, &right_id);
        let left_cost = (left_qty * left_avg).max(0.0);
        let right_cost = (right_qty * right_avg).max(0.0);

        let mut intents = Vec::new();
        let left_has_inventory = left_qty > 1e-9;
        let right_has_inventory = right_qty > 1e-9;

        match (left_has_inventory, right_has_inventory) {
            (false, false) => {
                let Some(entry_quantity) = self.paired_entry_quantity(
                    &left_quote,
                    &right_quote,
                    self.config.base_clip_usd,
                ) else {
                    return StrategyDecision::none();
                };
                let left_bid = self.build_bid_intent_for_quantity(
                    &snapshot.market_id,
                    &left_id,
                    &left_quote,
                    left_fair,
                    left_cost,
                    gross_cost,
                    entry_quantity,
                    self.config.min_edge_bps,
                    "mm-paired-bid",
                    "btc-5m-mm paired bid",
                    context.now_ms,
                );
                let right_bid = self.build_bid_intent_for_quantity(
                    &snapshot.market_id,
                    &right_id,
                    &right_quote,
                    right_fair,
                    right_cost,
                    gross_cost,
                    entry_quantity,
                    self.config.min_edge_bps,
                    "mm-paired-bid",
                    "btc-5m-mm paired bid",
                    context.now_ms,
                );
                match (left_bid, right_bid) {
                    (Some(left), Some(right)) => {
                        intents.push(left);
                        intents.push(right);
                    }
                    (Some(single), None) | (None, Some(single))
                        if self.config.allow_single_leg_entry =>
                    {
                        intents.push(single);
                    }
                    _ => {}
                }
            }
            (true, false) => {
                let hedge_qty = left_qty
                    .min(self.config.max_clip_usd / Self::best_bid(&right_quote).unwrap_or(1.0));
                if let Some(hedge) = self.build_bid_intent_for_quantity(
                    &snapshot.market_id,
                    &right_id,
                    &right_quote,
                    right_fair,
                    right_cost,
                    gross_cost,
                    hedge_qty,
                    self.config.hedge_rescue_edge_bps,
                    "mm-hedge-rescue",
                    "btc-5m-mm hedge rescue",
                    context.now_ms,
                ) {
                    intents.push(hedge);
                }
            }
            (false, true) => {
                let hedge_qty = right_qty
                    .min(self.config.max_clip_usd / Self::best_bid(&left_quote).unwrap_or(1.0));
                if let Some(hedge) = self.build_bid_intent_for_quantity(
                    &snapshot.market_id,
                    &left_id,
                    &left_quote,
                    left_fair,
                    left_cost,
                    gross_cost,
                    hedge_qty,
                    self.config.hedge_rescue_edge_bps,
                    "mm-hedge-rescue",
                    "btc-5m-mm hedge rescue",
                    context.now_ms,
                ) {
                    intents.push(hedge);
                }
            }
            (true, true) => {}
        }

        if intents.is_empty() {
            return StrategyDecision::none();
        }
        if let Some(state) = self.market_states.get_mut(&snapshot.market_id) {
            state.last_action_ms = Some(context.now_ms);
        }
        StrategyDecision {
            notes: vec![format!(
                "btc-5m-mm quotes left={} fair={left_fair:.4} right={} fair={right_fair:.4} gross_cost={gross_cost:.2}",
                left_id, right_id
            )],
            intents,
        }
    }

    fn on_fill(
        &mut self,
        _context: &StrategyContext,
        fill: &crate::types::FillReport,
    ) -> StrategyDecision {
        StrategyDecision {
            intents: Vec::new(),
            notes: vec![format!(
                "btc-5m-mm fill {} {:?} qty={:.4}@{:.4}",
                fill.instrument_id, fill.side, fill.quantity, fill.price
            )],
        }
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

    fn quote_is_fresh(&self, observed_at_ms: EpochMillis, now_ms: EpochMillis) -> bool {
        if let Some(max_age_ms) = self.quote_min_quote_age_ms {
            if now_ms.saturating_sub(observed_at_ms) > max_age_ms {
                return false;
            }
        }
        true
    }

    fn quote_refresh_allowed(
        &self,
        last_action_ms: Option<EpochMillis>,
        now_ms: EpochMillis,
    ) -> bool {
        let Some(refresh_interval_ms) = self.quote_refresh_interval_ms else {
            return true;
        };
        let Some(last_action_ms) = last_action_ms else {
            return true;
        };
        now_ms.saturating_sub(last_action_ms) >= refresh_interval_ms
    }

    fn quote_expired(&self, last_action_ms: Option<EpochMillis>, now_ms: EpochMillis) -> bool {
        self.quote_expiry_suppression_ms.is_some_and(|expiry_ms| {
            last_action_ms
                .is_some_and(|last_action_ms| now_ms.saturating_sub(last_action_ms) > expiry_ms)
        })
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

fn parse_bool(key: &str, default: bool) -> bool {
    env::var(key)
        .ok()
        .and_then(|raw| match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "y" | "on" => Some(true),
            "0" | "false" | "no" | "n" | "off" => Some(false),
            _ => None,
        })
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
mod tests {
    use super::{
        Btc5mMmConfig, Btc5mMmStrategy, GoatPairConfig, GoatPairStrategy, NoopStrategy,
        QuoteSnapshot, Strategy, StrategyContext, StrategyDecision,
    };
    use super::{
        BtcRegimeSnapshot, MarketActivitySignal, PairedBookSignal, SessionBucket,
        UnlawfulAggressionTier, UnlawfulExecutionMode, UnlawfulShearConfig, UnlawfulShearStrategy,
        UnlawfulSignalSnapshot,
    };
    use crate::inventory::{InventorySnapshot, PositionState};
    use crate::types::{
        BookLevel, InstrumentId, MarketId, MarketSnapshot, RuntimeStatus, TradeSide,
    };

    fn snapshot(asset: &str, market: &str, bid: f64, ask: f64, ts: u64) -> MarketSnapshot {
        MarketSnapshot {
            market_id: MarketId::from(market),
            instrument_id: InstrumentId::from(asset),
            quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new(bid, 1000.0)),
                best_ask: Some(BookLevel::new(ask, 1000.0)),
                bid_levels: vec![BookLevel::new(bid, 1000.0)],
                ask_levels: vec![BookLevel::new(ask, 1000.0)],
                depth_observed_at_ms: Some(ts),
                last_trade_price: Some(ask),
                observed_at_ms: ts,
            },
        }
    }

    fn context(positions: Vec<PositionState>) -> StrategyContext {
        context_with_unlawful_signal(positions, 10, 0, 0, None)
    }

    fn context_with_unlawful_signal(
        positions: Vec<PositionState>,
        now_ms: u64,
        open_orders_total: usize,
        open_orders_for_market: usize,
        unlawful_signal: Option<UnlawfulSignalSnapshot>,
    ) -> StrategyContext {
        StrategyContext {
            now_ms,
            runtime_status: RuntimeStatus::Running,
            inventory: InventorySnapshot {
                free_cash_usd: 1_000.0,
                reserved_cash_usd: 0.0,
                total_cash_usd: 1_000.0,
                realized_pnl_usd: 0.0,
                gross_exposure_usd: positions
                    .iter()
                    .map(|position| position.quantity * position.avg_price)
                    .sum(),
                positions,
            },
            open_orders_total,
            open_orders_for_market,
            market_context: None,
            unlawful_signal,
        }
    }

    fn btc_5m_mm_test_config() -> Btc5mMmConfig {
        Btc5mMmConfig {
            base_clip_usd: 1.10,
            min_clip_usd: 0.25,
            max_clip_usd: 5.0,
            liquidity_clip_fraction: 0.02,
            hedge_rescue_clip_usd: 2.50,
            max_gross_cost_usd: 20.0,
            max_leg_cost_usd: 10.0,
            min_edge_bps: 75.0,
            hedge_rescue_edge_bps: 25.0,
            inventory_skew_bps: 150.0,
            max_spread: 0.08,
            min_top_depth_notional_usd: 2.0,
            min_order_notional_usd: 1.0,
            venue_min_order_quantity: 5.0,
            entry_min_size_multiplier: 1.0,
            min_order_quantity: 0.01,
            maker_price_tick: 0.01,
            maker_safety_ticks: 2.0,
            cooldown_ms: 0,
            taker_fee_coeff: 0.072,
            allow_single_leg_entry: false,
        }
    }

    fn unlawful_signal_snapshot(
        mode: UnlawfulExecutionMode,
        clip_scale: f64,
        now_ms: u64,
    ) -> UnlawfulSignalSnapshot {
        UnlawfulSignalSnapshot {
            session_bucket: SessionBucket::Preferred,
            mode,
            gate_reasons: vec!["unlawful test signal".to_string()],
            btc: BtcRegimeSnapshot {
                last_price: Some(50_000.0),
                realized_vol_5m_bps: Some(6.0),
                realized_vol_15m_bps: Some(12.0),
                trade_count_5m: 9_000,
                trade_count_15m: 9_000,
                return_30s_bps: Some(0.0),
                return_60s_bps: Some(0.0),
                observed_at_ms: now_ms,
            },
            book: PairedBookSignal::with_ids(
                InstrumentId::from("down"),
                InstrumentId::from("up"),
                Some(BookLevel::new(0.24, 1_000.0)),
                Some(BookLevel::new(0.24, 1_000.0)),
                Some(BookLevel::new(0.65, 1_000.0)),
                Some(BookLevel::new(0.65, 1_000.0)),
                now_ms,
                true,
            ),
            activity: MarketActivitySignal::default(),
            first_fill_ms: None,
            first_merge_ms: None,
            elapsed_s: None,
            time_remaining_s: None,
            clip_scale,
        }
    }

    fn with_microstructure(
        mut signal: UnlawfulSignalSnapshot,
        cheap_ask_notional_top3: f64,
        expensive_ask_notional_top3: f64,
    ) -> UnlawfulSignalSnapshot {
        signal.gate_reasons.clear();
        signal.book.cheap_spread = Some(0.02);
        signal.book.expensive_spread = Some(0.02);
        signal.book.cheap_ask_notional_top3 = Some(cheap_ask_notional_top3);
        signal.book.expensive_ask_notional_top3 = Some(expensive_ask_notional_top3);
        signal.book.cheap_depth_imbalance_top3 = Some(0.0);
        signal.book.expensive_depth_imbalance_top3 = Some(0.0);
        signal
    }

    fn unlawful_shear_test_config() -> UnlawfulShearConfig {
        let mut cfg = UnlawfulShearConfig::from_env();
        cfg.cheap_hedge_price_max = 0.40;
        cfg.core_price_min = 0.50;
        cfg.core_price_max = 0.95;
        cfg.min_price_gap = 0.10;
        cfg.probe_clip_usd = 10.0;
        cfg.core_clip_usd = 35.0;
        cfg.hedge_clip_usd = 15.0;
        cfg.rebalance_clip_usd = 20.0;
        cfg.micro_clip_target_usd = 8.0;
        cfg.micro_clip_min_usd = 1.0;
        cfg.micro_clip_max_children = 1;
        cfg.trim_clip_fraction = 0.25;
        cfg.max_gross_cost_usd = 120.0;
        cfg.target_hedge_ratio_min = 0.20;
        cfg.target_hedge_ratio_max = 0.60;
        cfg.salvage_drawdown_ratio = 0.20;
        cfg.salvage_bid_floor = 0.05;
        cfg.max_open_orders_total = 8;
        cfg.taker_fee_coeff = 0.072;
        cfg.microstructure_enabled = true;
        cfg.microstructure_require_depth = false;
        cfg.microstructure_max_spread = 0.05;
        cfg.microstructure_min_ask_notional_top3_usd = 2.0;
        cfg.microstructure_max_clip_ask_notional_fraction = 0.10;
        cfg.microstructure_imbalance_threshold = 0.55;
        cfg.microstructure_weak_bid_scale = 0.65;
        cfg.microstructure_thin_ask_scale = 0.85;
        cfg
    }

    #[test]
    fn unlawful_shear_from_env_respects_offhour_override_flag() {
        let key = "WHALE_PAIR_UNLAWFUL_SHEAR_ALLOW_EXTREME_OFFHOUR_OVERRIDE";
        let original = std::env::var(key).ok();
        std::env::set_var(key, "true");

        let cfg = UnlawfulShearConfig::from_env();

        match original {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }

        assert!(cfg.allow_extreme_offhour_override);
    }

    #[test]
    fn unlawful_cleanup_qty_quantization_drops_micro_churn_and_buckets_size() {
        assert_eq!(UnlawfulShearStrategy::quantize_cleanup_qty(0.01), 0.0);
        assert_eq!(UnlawfulShearStrategy::quantize_cleanup_qty(0.10), 0.0);
        assert_eq!(UnlawfulShearStrategy::quantize_cleanup_qty(0.24), 0.0);
        assert!((UnlawfulShearStrategy::quantize_cleanup_qty(0.25) - 0.25).abs() < 1e-9);
        assert!((UnlawfulShearStrategy::quantize_cleanup_qty(1.23) - 1.0).abs() < 1e-9);
        assert!((UnlawfulShearStrategy::quantize_cleanup_qty(2.74) - 2.5).abs() < 1e-9);
    }

    #[test]
    fn noop_stays_idle() {
        let mut strategy = NoopStrategy;
        let decision = strategy.on_market_snapshot(
            &context(Vec::new()),
            &snapshot("token-up", "market", 0.4, 0.5, 1),
        );
        assert!(
            matches!(decision, StrategyDecision { intents: ref i, notes: ref n } if i.is_empty() && n.is_empty())
        );
    }

    #[test]
    fn goat_pairs_builds_order_when_side_cheap() {
        let mut strategy = GoatPairStrategy::new(
            GoatPairConfig {
                accumulate_price_max: 1.0,
                aggressive_price_max: 0.6,
                base_clip_usd: 20.0,
                aggressive_clip_usd: 50.0,
                max_gross_cost_usd: 1000.0,
                completion_min_pnl_per_share: 0.0,
                max_imbalance_ratio: 9.0,
                taker_fee_coeff: 0.072,
            },
            0,
        );
        strategy.on_market_snapshot(
            &context(Vec::new()),
            &snapshot("up", "market-a", 0.5, 0.5, 10),
        );
        let decision = strategy.on_market_snapshot(
            &context(Vec::new()),
            &snapshot("down", "market-a", 0.5, 0.52, 10),
        );
        assert!(!matches!(decision, StrategyDecision { intents: ref i, .. } if i.is_empty()));
    }

    #[test]
    fn goat_pairs_emit_three_level_bid_ladder() {
        let mut strategy = GoatPairStrategy::new(
            GoatPairConfig {
                accumulate_price_max: 1.0,
                aggressive_price_max: 0.6,
                base_clip_usd: 30.0,
                aggressive_clip_usd: 50.0,
                max_gross_cost_usd: 1_000.0,
                completion_min_pnl_per_share: 0.0,
                max_imbalance_ratio: 9.0,
                taker_fee_coeff: 0.072,
            },
            0,
        );
        strategy.quote_levels_per_side = 3;
        strategy.quote_min_edge_bps = 50.0;
        strategy.quote_inventory_skew_bps = 25.0;
        let ctx = context(Vec::new());
        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-b", 0.50, 0.50, 10));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-b", 0.50, 0.52, 10));
        assert_eq!(decision.intents.len(), 3);
        assert!(decision
            .intents
            .iter()
            .all(|intent| intent.side == TradeSide::Buy));
        assert!(decision
            .intents
            .windows(2)
            .all(|window| window[0].limit_price >= window[1].limit_price));
    }

    #[test]
    fn btc_5m_mm_quotes_maker_bids_on_both_outcomes() {
        let mut strategy = Btc5mMmStrategy::new(btc_5m_mm_test_config());
        let ctx = context(Vec::new());
        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.52, 10));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.52, 10));

        assert_eq!(decision.intents.len(), 2);
        assert!(decision
            .intents
            .iter()
            .all(|intent| intent.side == TradeSide::Buy && !intent.reduce_only));
        assert!(decision
            .intents
            .iter()
            .all(|intent| intent.quantity >= strategy.config.venue_min_order_quantity));
    }

    #[test]
    fn btc_5m_mm_bids_do_not_cross_book_with_post_only_buffer() {
        let mut strategy = Btc5mMmStrategy::new(btc_5m_mm_test_config());
        let ctx = context(Vec::new());
        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.45, 0.46, 10));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.53, 0.54, 10));

        assert_eq!(decision.intents.len(), 2);
        let up = decision
            .intents
            .iter()
            .find(|intent| intent.instrument_id == InstrumentId::from("up"))
            .expect("up bid");
        let down = decision
            .intents
            .iter()
            .find(|intent| intent.instrument_id == InstrumentId::from("down"))
            .expect("down bid");
        assert_eq!(up.limit_price, 0.44);
        assert_eq!(down.limit_price, 0.52);
    }

    #[test]
    fn btc_5m_mm_entries_are_dynamic_but_venue_safe() {
        let mut config = btc_5m_mm_test_config();
        config.base_clip_usd = 0.25;
        config.min_clip_usd = 0.25;
        config.max_clip_usd = 1.00;
        config.min_order_notional_usd = 0.20;
        config.venue_min_order_quantity = 0.01;
        config.min_order_quantity = 0.01;
        let mut strategy = Btc5mMmStrategy::new(config);
        let ctx = context(Vec::new());
        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.52, 10));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.52, 10));

        assert_eq!(decision.intents.len(), 2);
        assert!(decision.intents.iter().all(|intent| intent.quantity < 5.0));
        assert!(decision
            .intents
            .iter()
            .all(|intent| intent.quantity >= config.venue_min_order_quantity));
    }

    #[test]
    fn btc_5m_mm_can_scale_parent_size_above_venue_minimum() {
        let mut config = btc_5m_mm_test_config();
        config.base_clip_usd = 1.10;
        config.max_clip_usd = 8.00;
        config.entry_min_size_multiplier = 1.30;
        let mut strategy = Btc5mMmStrategy::new(config);
        let ctx = context(Vec::new());
        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.74, 0.76, 10));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.16, 0.18, 10));

        assert_eq!(decision.intents.len(), 2);
        assert!(decision
            .intents
            .iter()
            .all(|intent| (intent.quantity - 6.5).abs() < 1e-9));
    }

    #[test]
    fn btc_5m_mm_suppresses_unpaired_flat_entry_by_default() {
        let mut strategy = Btc5mMmStrategy::new(btc_5m_mm_test_config());
        let ctx = context(Vec::new());
        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.52, 10));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.52, 0.56, 10));

        assert!(decision.intents.is_empty());
    }

    #[test]
    fn btc_5m_mm_hedge_rescues_one_sided_inventory() {
        let mut config = btc_5m_mm_test_config();
        config.inventory_skew_bps = 0.0;
        let mut strategy = Btc5mMmStrategy::new(config);
        let positions = vec![PositionState {
            market_id: MarketId::from("market-mm"),
            instrument_id: InstrumentId::from("up"),
            quantity: 5.0,
            avg_price: 0.33,
            mark_price: Some(0.33),
            updated_at_ms: 1,
        }];
        let ctx = context(positions);
        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.32, 0.34, 10));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.50, 10));

        assert_eq!(decision.intents.len(), 1);
        assert_eq!(
            decision.intents[0].instrument_id,
            InstrumentId::from("down")
        );
        assert_eq!(decision.intents[0].side, TradeSide::Buy);
        assert_eq!(
            decision.intents[0].quote_level_tag.as_deref(),
            Some("mm-hedge-rescue")
        );
    }

    #[test]
    fn btc_5m_mm_does_not_sell_unhedged_inventory() {
        let mut config = btc_5m_mm_test_config();
        config.inventory_skew_bps = 0.0;
        config.min_edge_bps = 0.0;
        let mut strategy = Btc5mMmStrategy::new(config);
        let positions = vec![PositionState {
            market_id: MarketId::from("market-mm"),
            instrument_id: InstrumentId::from("up"),
            quantity: 5.0,
            avg_price: 0.75,
            mark_price: Some(0.67),
            updated_at_ms: 1,
        }];
        let ctx = context(positions);
        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.66, 0.67, 10));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.65, 0.66, 10));

        assert!(decision
            .intents
            .iter()
            .all(|intent| intent.side == TradeSide::Buy && !intent.reduce_only));
    }

    #[test]
    fn unlawful_shear_builds_core_and_hedge() {
        let mut strategy = UnlawfulShearStrategy::new(unlawful_shear_test_config(), 0);
        let ctx = context_with_unlawful_signal(
            Vec::new(),
            10,
            0,
            0,
            Some(unlawful_signal_snapshot(
                UnlawfulExecutionMode::Entry,
                1.0,
                10,
            )),
        );
        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-a", 0.70, 0.74, 10));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-a", 0.18, 0.22, 10));
        assert_eq!(decision.intents.len(), 2);
        assert!(decision
            .intents
            .iter()
            .any(|intent| intent.side == TradeSide::Buy));
    }

    #[test]
    fn unlawful_shear_micro_clips_split_approved_buy_intents() {
        let mut config = unlawful_shear_test_config();
        config.core_clip_usd = 40.0;
        config.hedge_clip_usd = 12.0;
        config.micro_clip_target_usd = 8.0;
        config.micro_clip_min_usd = 1.0;
        config.micro_clip_max_children = 4;
        let mut strategy = UnlawfulShearStrategy::new(config, 0);
        let ctx = context_with_unlawful_signal(
            Vec::new(),
            10,
            0,
            0,
            Some(unlawful_signal_snapshot(
                UnlawfulExecutionMode::Entry,
                1.0,
                10,
            )),
        );

        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-a", 0.70, 0.74, 10));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-a", 0.18, 0.22, 10));

        let core_orders: Vec<_> = decision
            .intents
            .iter()
            .filter(|intent| intent.instrument_id == InstrumentId::from("up"))
            .collect();
        assert_eq!(core_orders.len(), 4);
        assert!(core_orders.iter().all(|intent| {
            intent
                .quote_level_tag
                .as_deref()
                .is_some_and(|tag| tag.contains("core-entry:child-"))
        }));
        assert!(core_orders
            .iter()
            .all(|intent| intent.reason.contains("child=")));
    }

    #[test]
    fn unlawful_shear_microstructure_caps_clip_to_visible_depth() {
        let mut config = unlawful_shear_test_config();
        config.microstructure_require_depth = true;
        config.microstructure_max_clip_ask_notional_fraction = 0.10;
        config.microstructure_imbalance_threshold = 1.0;
        let mut strategy = UnlawfulShearStrategy::new(config, 0);
        let signal = with_microstructure(
            unlawful_signal_snapshot(UnlawfulExecutionMode::Entry, 1.0, 10),
            1_000.0,
            50.0,
        );
        let ctx = context_with_unlawful_signal(Vec::new(), 10, 0, 0, Some(signal));

        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-a", 0.70, 0.74, 10));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-a", 0.18, 0.22, 10));

        let core = decision
            .intents
            .iter()
            .find(|intent| intent.instrument_id == InstrumentId::from("up"))
            .expect("core order");
        assert!((core.quantity * core.limit_price - 5.0).abs() < 1e-9);
        assert!(decision
            .notes
            .iter()
            .any(|note| note.contains("microstructure scaled action=core-entry")));
    }

    #[test]
    fn unlawful_shear_microstructure_blocks_wide_spread_entry() {
        let mut config = unlawful_shear_test_config();
        config.microstructure_require_depth = true;
        config.microstructure_max_spread = 0.03;
        let mut strategy = UnlawfulShearStrategy::new(config, 0);
        let mut signal = with_microstructure(
            unlawful_signal_snapshot(UnlawfulExecutionMode::Entry, 1.0, 10),
            1_000.0,
            1_000.0,
        );
        signal.book.cheap_spread = Some(0.04);
        signal.book.expensive_spread = Some(0.04);
        let ctx = context_with_unlawful_signal(Vec::new(), 10, 0, 0, Some(signal));

        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-a", 0.70, 0.74, 10));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-a", 0.18, 0.22, 10));

        assert!(decision.intents.is_empty());
        assert!(decision
            .notes
            .iter()
            .any(|note| note.contains("microstructure blocked action=core-entry")));
        assert!(decision
            .notes
            .iter()
            .any(|note| { note.contains("unlawful microstructure controller suppressed market") }));
    }

    #[test]
    fn unlawful_shear_allows_new_market_entry_despite_global_open_order_pressure() {
        let mut strategy = UnlawfulShearStrategy::new(unlawful_shear_test_config(), 0);
        let ctx = context_with_unlawful_signal(
            Vec::new(),
            10,
            32,
            0,
            Some(unlawful_signal_snapshot(
                UnlawfulExecutionMode::Entry,
                1.0,
                10,
            )),
        );
        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-a", 0.70, 0.74, 10));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-a", 0.18, 0.22, 10));

        assert_eq!(decision.intents.len(), 2);
        assert!(decision
            .intents
            .iter()
            .all(|intent| intent.market_id == MarketId::from("market-a")));
    }

    #[test]
    fn unlawful_shear_salvages_losing_leg() {
        let mut config = unlawful_shear_test_config();
        config.salvage_drawdown_ratio = 0.15;
        let mut strategy = UnlawfulShearStrategy::new(config, 0);
        let positions = vec![
            PositionState {
                market_id: MarketId::from("market-a"),
                instrument_id: InstrumentId::from("up"),
                quantity: 100.0,
                avg_price: 0.80,
                mark_price: Some(0.60),
                updated_at_ms: 1,
            },
            PositionState {
                market_id: MarketId::from("market-a"),
                instrument_id: InstrumentId::from("down"),
                quantity: 60.0,
                avg_price: 0.18,
                mark_price: Some(0.24),
                updated_at_ms: 1,
            },
        ];
        let ctx = context(positions);
        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-a", 0.55, 0.60, 10));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-a", 0.22, 0.25, 10));
        assert!(decision
            .intents
            .iter()
            .any(|intent| intent.side == TradeSide::Sell && intent.reduce_only));
    }

    #[test]
    fn unlawful_shear_mode_action_rules_are_deterministic() {
        assert!(UnlawfulShearStrategy::can_launch_mode_action(
            UnlawfulExecutionMode::Entry,
            "core-entry"
        ));
        assert!(UnlawfulShearStrategy::can_launch_mode_action(
            UnlawfulExecutionMode::Entry,
            "hedge-probe"
        ));
        assert!(!UnlawfulShearStrategy::can_launch_mode_action(
            UnlawfulExecutionMode::Entry,
            "add-hedge"
        ));
        assert!(UnlawfulShearStrategy::can_launch_mode_action(
            UnlawfulExecutionMode::Manage,
            "rebalance-core"
        ));
        assert!(!UnlawfulShearStrategy::can_launch_mode_action(
            UnlawfulExecutionMode::Manage,
            "core-entry"
        ));
        assert!(!UnlawfulShearStrategy::can_launch_mode_action(
            UnlawfulExecutionMode::Cleanup,
            "core-entry"
        ));
        assert!(!UnlawfulShearStrategy::can_launch_mode_action(
            UnlawfulExecutionMode::Flatten,
            "hedge-probe"
        ));
    }

    #[test]
    fn unlawful_shear_signal_mode_entry_allows_core_and_hedge_actions_only() {
        let strategy = UnlawfulShearStrategy::new(unlawful_shear_test_config(), 0);
        let (mode, _, _, clip_scale) = strategy.signal_mode_and_aggression(
            &context_with_unlawful_signal(
                Vec::new(),
                10_000,
                0,
                0,
                Some(unlawful_signal_snapshot(
                    UnlawfulExecutionMode::Entry,
                    1.0,
                    10_000,
                )),
            ),
            Some(10),
            false,
            0.38,
            0.64,
            0.26,
        );

        assert_eq!(mode, UnlawfulExecutionMode::Entry);
        assert!(UnlawfulShearStrategy::can_launch_mode_action(
            mode,
            "core-entry"
        ));
        assert!(UnlawfulShearStrategy::can_launch_mode_action(
            mode,
            "hedge-probe"
        ));
        assert_eq!(clip_scale, 0.6);
    }

    #[test]
    fn unlawful_shear_signal_mode_manage_stays_aggressive_and_no_entry_actions() {
        let strategy = UnlawfulShearStrategy::new(unlawful_shear_test_config(), 0);
        let (mode, aggression, _reasons, clip_scale) = strategy.signal_mode_and_aggression(
            &context_with_unlawful_signal(
                Vec::new(),
                10_000,
                0,
                0,
                Some(unlawful_signal_snapshot(
                    UnlawfulExecutionMode::Manage,
                    1.0,
                    10_000,
                )),
            ),
            Some(40),
            true,
            0.38,
            0.64,
            0.26,
        );

        assert_eq!(mode, UnlawfulExecutionMode::Manage);
        assert_ne!(aggression, UnlawfulAggressionTier::Suppressed);
        assert!(clip_scale > 0.0);
        assert!(!UnlawfulShearStrategy::can_launch_mode_action(
            mode,
            "core-entry"
        ));
        assert!(UnlawfulShearStrategy::can_launch_mode_action(
            mode,
            "add-hedge"
        ));
        assert!(!UnlawfulShearStrategy::can_launch_mode_action(
            mode,
            "early-probe"
        ));
    }

    #[test]
    fn unlawful_shear_signal_mode_standby_blocks_buys() {
        let strategy = UnlawfulShearStrategy::new(unlawful_shear_test_config(), 0);
        let (mode, aggression, _reasons, clip_scale) = strategy.signal_mode_and_aggression(
            &context_with_unlawful_signal(
                Vec::new(),
                10_000,
                0,
                0,
                Some(unlawful_signal_snapshot(
                    UnlawfulExecutionMode::Standby,
                    0.2,
                    10_000,
                )),
            ),
            Some(5),
            false,
            0.38,
            0.64,
            0.26,
        );

        assert_eq!(mode, UnlawfulExecutionMode::Standby);
        assert_eq!(aggression, UnlawfulAggressionTier::Suppressed);
        assert_eq!(clip_scale, 0.0);
        assert!(!UnlawfulShearStrategy::can_launch_mode_action(
            mode,
            "core-entry"
        ));
        assert!(!UnlawfulShearStrategy::can_launch_mode_action(
            mode,
            "rebalance-core"
        ));
    }

    #[test]
    fn unlawful_shear_signal_reason_and_aggression_logged_in_decision_notes() {
        let mut strategy = UnlawfulShearStrategy::new(unlawful_shear_test_config(), 0);
        let mut signal = unlawful_signal_snapshot(UnlawfulExecutionMode::Entry, 1.0, 10_000);
        signal.gate_reasons = vec![
            "unit-test gate reason: entry geometry valid".to_string(),
            "unit-test gate reason: synthetic invariant".to_string(),
        ];

        let ctx = context_with_unlawful_signal(Vec::new(), 10_000, 0, 0, Some(signal));
        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-a", 0.70, 0.74, 10_000));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-a", 0.18, 0.22, 10_000));

        let notes_blob = decision.notes.join("|");
        assert!(notes_blob.contains("unlawful eval market=market-a"));
        assert!(notes_blob.contains("cheap_id=down"));
        assert!(notes_blob.contains("expensive_id=up"));
        assert!(notes_blob.contains("session=Preferred"));
        assert!(notes_blob.contains("btc_vol_5m_bps=6.00"));
        assert!(notes_blob.contains("btc_trade_count_5m=9000"));
        assert!(notes_blob.contains("mode=Entry"));
        assert!(notes_blob.contains("aggression=Press"));
        assert!(notes_blob.contains("signal_clip_scale=1.000"));
        assert!(notes_blob.contains("buy_clip_scale=1.100"));
        assert!(notes_blob.contains("books_fresh=true"));
        assert!(notes_blob.contains(
            "reasons=unit-test gate reason: entry geometry valid;unit-test gate reason: synthetic invariant"
        ));
    }
}
