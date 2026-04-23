use std::collections::VecDeque;

use serde::Serialize;

use crate::types::{
    ClientOrderId, EpochMillis, InstrumentId, MarketId, OrderId, RuntimeStatus,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum EventCategory {
    Runtime,
    Strategy,
    Risk,
    Inventory,
    Execution,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct EventMetrics {
    pub price: Option<f64>,
    pub quantity: Option<f64>,
    pub notional_usd: Option<f64>,
    pub cash_delta_usd: Option<f64>,
    pub position_delta: Option<f64>,
    pub free_cash_after_usd: Option<f64>,
    pub gross_exposure_after_usd: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EventRecord {
    pub seq: u64,
    pub observed_at_ms: EpochMillis,
    pub category: EventCategory,
    pub message: String,
    pub market_id: Option<MarketId>,
    pub instrument_id: Option<InstrumentId>,
    pub client_order_id: Option<ClientOrderId>,
    pub order_id: Option<OrderId>,
    pub metrics: EventMetrics,
}

impl EventRecord {
    pub fn new(
        category: EventCategory,
        observed_at_ms: EpochMillis,
        message: impl Into<String>,
    ) -> Self {
        Self {
            seq: 0,
            observed_at_ms,
            category,
            message: message.into(),
            market_id: None,
            instrument_id: None,
            client_order_id: None,
            order_id: None,
            metrics: EventMetrics::default(),
        }
    }

    pub fn with_market(mut self, market_id: impl Into<MarketId>) -> Self {
        self.market_id = Some(market_id.into());
        self
    }

    pub fn with_instrument(mut self, instrument_id: impl Into<InstrumentId>) -> Self {
        self.instrument_id = Some(instrument_id.into());
        self
    }

    pub fn with_client_order(mut self, client_order_id: impl Into<ClientOrderId>) -> Self {
        self.client_order_id = Some(client_order_id.into());
        self
    }

    pub fn with_order_id(mut self, order_id: impl Into<OrderId>) -> Self {
        self.order_id = Some(order_id.into());
        self
    }

    pub fn with_metrics(mut self, metrics: EventMetrics) -> Self {
        self.metrics = metrics;
        self
    }

    pub fn runtime_status(observed_at_ms: EpochMillis, status: RuntimeStatus) -> Self {
        Self::new(
            EventCategory::Runtime,
            observed_at_ms,
            format!("runtime status -> {:?}", status),
        )
    }
}

#[derive(Clone, Debug)]
pub struct EventLog {
    capacity: usize,
    next_seq: u64,
    records: VecDeque<EventRecord>,
}

impl EventLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            next_seq: 1,
            records: VecDeque::with_capacity(capacity.max(1)),
        }
    }

    pub fn push(&mut self, mut record: EventRecord) -> u64 {
        record.seq = self.next_seq;
        self.next_seq += 1;
        if self.records.len() == self.capacity {
            self.records.pop_front();
        }
        let seq = record.seq;
        self.records.push_back(record);
        seq
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn latest_seq(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    pub fn iter(&self) -> impl Iterator<Item = &EventRecord> {
        self.records.iter()
    }

    pub fn recent(&self, limit: usize) -> Vec<EventRecord> {
        let start = self.records.len().saturating_sub(limit);
        self.records.iter().skip(start).cloned().collect()
    }

    pub fn snapshot_since(&self, after_seq: u64) -> Vec<EventRecord> {
        self.records
            .iter()
            .filter(|record| record.seq > after_seq)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{EventCategory, EventLog, EventRecord};

    #[test]
    fn drops_oldest_records_when_capacity_is_reached() {
        let mut log = EventLog::new(2);

        log.push(EventRecord::new(EventCategory::Runtime, 1, "a"));
        log.push(EventRecord::new(EventCategory::Runtime, 2, "b"));
        log.push(EventRecord::new(EventCategory::Runtime, 3, "c"));

        let recent = log.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].message, "b");
        assert_eq!(recent[1].message, "c");
        assert_eq!(recent[0].seq, 2);
        assert_eq!(recent[1].seq, 3);
    }
}
