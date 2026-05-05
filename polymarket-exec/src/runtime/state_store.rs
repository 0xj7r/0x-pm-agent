//! Replay runtime state-store seam for order lifecycle state.
//!
//! Live trading currently owns working orders in `Runtime::open_orders` and
//! persists them through the durable `OrderStore`; do not treat this module as
//! the live source of truth until the runtime is explicitly migrated onto this
//! trait. Replay uses this seam to share the same `ManagedOrder` shape with
//! quote reconciliation and risk-context construction.

use std::collections::HashMap;

use crate::runtime::types::{ManagedOrder, ManagedOrderStatus};
use crate::types::{ClientOrderId, EpochMillis, OrderIntent};

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeStateStoreError {
    DuplicateOrder(ClientOrderId),
    MissingOrder(ClientOrderId),
    InvalidTransition {
        client_order_id: ClientOrderId,
        from: ManagedOrderStatus,
        to: ManagedOrderStatus,
    },
}

impl std::fmt::Display for RuntimeStateStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateOrder(client_order_id) => {
                write!(f, "duplicate runtime order: {client_order_id}")
            }
            Self::MissingOrder(client_order_id) => {
                write!(f, "missing runtime order: {client_order_id}")
            }
            Self::InvalidTransition {
                client_order_id,
                from,
                to,
            } => write!(
                f,
                "invalid runtime order transition for {client_order_id}: {from:?} -> {to:?}"
            ),
        }
    }
}

impl std::error::Error for RuntimeStateStoreError {}

pub trait RuntimeStateStore {
    fn submit_order(
        &mut self,
        intent: OrderIntent,
        submitted_at_ms: EpochMillis,
    ) -> Result<(), RuntimeStateStoreError>;
    fn cancel_order(
        &mut self,
        client_order_id: &ClientOrderId,
        cancelled_at_ms: EpochMillis,
    ) -> Result<(), RuntimeStateStoreError>;
    fn apply_fill(
        &mut self,
        client_order_id: &ClientOrderId,
        fill_qty: f64,
        filled_at_ms: EpochMillis,
    ) -> Result<(), RuntimeStateStoreError>;
    fn open_orders(&self) -> HashMap<ClientOrderId, ManagedOrder>;
    fn get_order(&self, client_order_id: &ClientOrderId) -> Option<&ManagedOrder>;
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryRuntimeStateStore {
    orders: HashMap<ClientOrderId, ManagedOrder>,
}

impl InMemoryRuntimeStateStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn transition(
        order: &mut ManagedOrder,
        next: ManagedOrderStatus,
        updated_at_ms: EpochMillis,
    ) -> Result<(), RuntimeStateStoreError> {
        let current = order.status;
        if !current.can_transition_to(next) {
            return Err(RuntimeStateStoreError::InvalidTransition {
                client_order_id: order.intent.client_order_id.clone(),
                from: current,
                to: next,
            });
        }
        order.status = next;
        order.last_update_ms = updated_at_ms;
        Ok(())
    }
}

impl RuntimeStateStore for InMemoryRuntimeStateStore {
    fn submit_order(
        &mut self,
        intent: OrderIntent,
        submitted_at_ms: EpochMillis,
    ) -> Result<(), RuntimeStateStoreError> {
        let client_order_id = intent.client_order_id.clone();
        if self.orders.contains_key(&client_order_id) {
            return Err(RuntimeStateStoreError::DuplicateOrder(client_order_id));
        }
        self.orders.insert(
            client_order_id,
            ManagedOrder {
                intent,
                status: ManagedOrderStatus::Working,
                cumulative_filled_qty: 0.0,
                reserved_cash_usd: 0.0,
                last_update_ms: submitted_at_ms,
            },
        );
        Ok(())
    }

    fn cancel_order(
        &mut self,
        client_order_id: &ClientOrderId,
        cancelled_at_ms: EpochMillis,
    ) -> Result<(), RuntimeStateStoreError> {
        let Some(order) = self.orders.get_mut(client_order_id) else {
            return Err(RuntimeStateStoreError::MissingOrder(
                client_order_id.clone(),
            ));
        };
        Self::transition(order, ManagedOrderStatus::Cancelled, cancelled_at_ms)
    }

    fn apply_fill(
        &mut self,
        client_order_id: &ClientOrderId,
        fill_qty: f64,
        filled_at_ms: EpochMillis,
    ) -> Result<(), RuntimeStateStoreError> {
        let Some(order) = self.orders.get_mut(client_order_id) else {
            return Err(RuntimeStateStoreError::MissingOrder(
                client_order_id.clone(),
            ));
        };
        if fill_qty <= 0.0 {
            return Ok(());
        }
        let remaining_before = order.remaining_qty();
        order.cumulative_filled_qty += fill_qty.min(remaining_before);
        let next = if order.remaining_qty() <= 1e-9 {
            ManagedOrderStatus::Filled
        } else {
            ManagedOrderStatus::Working
        };
        Self::transition(order, next, filled_at_ms)
    }

    fn open_orders(&self) -> HashMap<ClientOrderId, ManagedOrder> {
        self.orders
            .iter()
            .filter(|(_, order)| !order.status.is_terminal())
            .map(|(client_order_id, order)| (client_order_id.clone(), order.clone()))
            .collect()
    }

    fn get_order(&self, client_order_id: &ClientOrderId) -> Option<&ManagedOrder> {
        self.orders.get(client_order_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{InstrumentId, IntentKind, MarketId, TradeSide};

    fn intent(client_order_id: &str) -> OrderIntent {
        OrderIntent {
            client_order_id: ClientOrderId::new(client_order_id),
            market_id: MarketId::new("market"),
            instrument_id: InstrumentId::new("asset"),
            side: TradeSide::Buy,
            limit_price: 0.5,
            quantity: 10.0,
            reduce_only: false,
            reason: "test".to_string(),
            quote_level_tag: Some("level-0".to_string()),
            created_at_ms: 100,
            pair_id: None,
            kind: IntentKind::Entry,
        }
    }

    #[test]
    fn in_memory_store_tracks_open_and_filled_orders() {
        let mut store = InMemoryRuntimeStateStore::new();
        let client_order_id = ClientOrderId::new("order-1");

        store.submit_order(intent("order-1"), 100).unwrap();
        assert_eq!(store.open_orders().len(), 1);

        store.apply_fill(&client_order_id, 4.0, 110).unwrap();
        let order = store.get_order(&client_order_id).unwrap();
        assert_eq!(order.status, ManagedOrderStatus::Working);
        assert_eq!(order.cumulative_filled_qty, 4.0);

        store.apply_fill(&client_order_id, 6.0, 120).unwrap();
        assert_eq!(store.open_orders().len(), 0);
        assert_eq!(
            store.get_order(&client_order_id).unwrap().status,
            ManagedOrderStatus::Filled
        );
    }

    #[test]
    fn in_memory_store_cancel_removes_order_from_open_view() {
        let mut store = InMemoryRuntimeStateStore::new();
        let client_order_id = ClientOrderId::new("order-1");

        store.submit_order(intent("order-1"), 100).unwrap();
        store.cancel_order(&client_order_id, 110).unwrap();

        assert_eq!(store.open_orders().len(), 0);
        assert_eq!(
            store.get_order(&client_order_id).unwrap().status,
            ManagedOrderStatus::Cancelled
        );
    }
}
