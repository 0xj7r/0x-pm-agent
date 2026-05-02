use serde::Serialize;

use crate::types::{
    ClientOrderId, EpochMillis, InstrumentId, MarketId, OrderId, RuntimeStatus, TradeSide,
};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RuntimeCheckpointOrder {
    pub client_order_id: ClientOrderId,
    pub venue_order_id: Option<OrderId>,
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub side: TradeSide,
    pub limit_price: f64,
    pub reduce_only: bool,
    pub original_qty: f64,
    pub remaining_qty: f64,
    pub filled_qty: f64,
    pub status: String,
    pub submitted_at_ms: EpochMillis,
    pub last_update_ms: EpochMillis,
    pub reason: Option<String>,
    pub strategy_tag: String,
    pub quote_level_tag: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RuntimeCheckpoint {
    pub observed_at_ms: EpochMillis,
    pub run_id: String,
    pub name: String,
    pub runtime_status: RuntimeStatus,
    pub open_orders: Vec<RuntimeCheckpointOrder>,
    pub needs_reconcile_orders: usize,
    pub event_seq_checkpoint: u64,
}
