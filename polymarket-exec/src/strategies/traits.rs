//! Generic strategy traits for market-agnostic execution.

use crate::market_making::pairing::pair_cost_tracker::PairCostTracker;
use crate::market_making::pairing::types::{PairedInventorySnapshot, PairedMarketSnapshot};
use crate::signals::{
    BtcRegimeSnapshot, FairValueEstimate, MomentumSignal, OrderBookPressureSignal,
};
use crate::types::{EpochMillis, FillReport, StrategyDecision};
use crate::types::{InstrumentId, MarketId, TradeSide};

#[derive(Clone, Debug, PartialEq)]
pub struct StrategyOpenOrderSnapshot {
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub side: TradeSide,
    pub limit_price: f64,
    pub remaining_qty: f64,
    pub reduce_only: bool,
    pub quote_level_tag: Option<String>,
    /// When the order's underlying intent was created. Lanes that escalate
    /// stale makers (e.g. late-fav climb FAK escalation) read this to
    /// compute age. Defaults to 0 in non-live adapters and is best-effort.
    pub created_at_ms: EpochMillis,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StrategyDirectionalInventorySnapshot {
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub quantity: f64,
    pub avg_cost: f64,
    pub quote_level_tag: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PairedOpenOrderExposure {
    pub yes_qty: f64,
    pub yes_notional_usd: f64,
    pub yes_count: usize,
    pub no_qty: f64,
    pub no_notional_usd: f64,
    pub no_count: usize,
}

#[derive(Clone, Debug)]
pub struct StrategyInput<M> {
    pub market: M,
    pub snapshot: PairedMarketSnapshot,
    /// Total venue/account inventory for this market. Directional lanes use
    /// this to cap favorite/tail exposure.
    pub inventory: PairedInventorySnapshot,
    /// Inventory attributable to the paired-core lane only. Paired-core merge
    /// and repair gates must use this, not total inventory, otherwise a
    /// late-favorite load looks like imbalance to repair.
    pub paired_core_inventory: PairedInventorySnapshot,
    /// Filled inventory attributable specifically to the late-favorite lane.
    /// Cheap-tail hedges must size from this plus working late-fav orders,
    /// not from aggregate directional inventory, otherwise cheap-tail can
    /// bootstrap itself without any favorite exposure.
    pub late_fav_inventory: PairedInventorySnapshot,
    /// Filled inventory attributable specifically to the cheap-tail hedge
    /// lane. Tail sizing must account for already-filled hedge cost so the
    /// hedge cannot repeatedly consume the late-favorite win-upside.
    pub cheap_tail_inventory: PairedInventorySnapshot,
    pub open_convex_order_exposure: PairedOpenOrderExposure,
    pub open_late_fav_order_exposure: PairedOpenOrderExposure,
    pub open_paired_core_order_exposure: PairedOpenOrderExposure,
    /// Per-order snapshot of the strategy's currently-open orders on this
    /// market. Used by lanes that need to reason about individual orders
    /// (e.g. stale-maker -> FAK escalation in late-fav climb).
    pub open_orders: Vec<StrategyOpenOrderSnapshot>,
    pub pair_cost: PairCostTracker,
    pub fair_value: FairValueEstimate,
    pub btc_regime: BtcRegimeSnapshot,
    pub momentum: MomentumSignal,
    pub order_book_pressure: OrderBookPressureSignal,
    pub now_ms: EpochMillis,
}

#[derive(Clone, Debug)]
pub struct StrategyFillInput<M> {
    pub market: M,
    pub snapshot: PairedMarketSnapshot,
    pub fair_value: FairValueEstimate,
    pub fill: FillReport,
}

/// Strategy interface for new modules.
///
/// Implementations must be deterministic for a fixed input. They may hold
/// internal state, but they must not call venues, mutate runtime state, or
/// bypass risk. They return typed decisions only.
pub trait TradingStrategy<M> {
    fn name(&self) -> &'static str;
    fn on_tick(&mut self, input: StrategyInput<M>) -> StrategyDecision;

    fn on_fill(&mut self, _input: StrategyFillInput<M>) -> StrategyDecision {
        StrategyDecision::noop()
    }
}
