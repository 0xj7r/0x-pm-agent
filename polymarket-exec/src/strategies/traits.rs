//! Generic strategy traits for market-agnostic execution.

use crate::market_making::pairing::pair_cost_tracker::PairCostTracker;
use crate::market_making::pairing::types::{PairedInventorySnapshot, PairedMarketSnapshot};
use crate::signals::{
    BtcRegimeSnapshot, FairValueEstimate, MomentumSignal, OrderBookPressureSignal,
};
use crate::types::{EpochMillis, FillReport, StrategyDecision};

#[derive(Clone, Debug)]
pub struct StrategyInput<M> {
    pub market: M,
    pub snapshot: PairedMarketSnapshot,
    pub inventory: PairedInventorySnapshot,
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
