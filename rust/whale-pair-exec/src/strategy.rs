use crate::inventory::InventoryState;
use crate::types::{EpochMillis, FillReport, MarketSnapshot, OrderIntent, RuntimeStatus};

#[derive(Clone, Debug, Default, PartialEq)]
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

pub struct StrategyContext<'a> {
    pub now_ms: EpochMillis,
    pub runtime_status: RuntimeStatus,
    pub inventory: &'a InventoryState,
    pub open_orders_total: usize,
}

pub trait Strategy {
    fn name(&self) -> &str;

    fn on_start(&mut self, _context: &StrategyContext<'_>) -> StrategyDecision {
        StrategyDecision::none()
    }

    fn on_market_snapshot(
        &mut self,
        _context: &StrategyContext<'_>,
        _snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        StrategyDecision::none()
    }

    fn on_fill(
        &mut self,
        _context: &StrategyContext<'_>,
        _fill: &FillReport,
    ) -> StrategyDecision {
        StrategyDecision::none()
    }
}

#[derive(Debug, Default)]
pub struct NoopStrategy;

impl Strategy for NoopStrategy {
    fn name(&self) -> &str {
        "noop"
    }
}
