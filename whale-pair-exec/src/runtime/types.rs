use std::error::Error;
use std::fmt;

use crate::inventory::InventoryError;
use crate::quote_engine::QuoteEngineConfig;
use crate::types::{EpochMillis, RuntimeCommand, RuntimeStatus};

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeConfig {
    pub starting_cash_usd: f64,
    pub event_log_capacity: usize,
    pub initial_status: RuntimeStatus,
    pub quote_engine_config: QuoteEngineConfig,
    pub quote_stale_ms: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            starting_cash_usd: 0.0,
            event_log_capacity: 4_096,
            initial_status: RuntimeStatus::Starting,
            quote_engine_config: QuoteEngineConfig::default(),
            quote_stale_ms: 10_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagedOrderStatus {
    PendingSubmit,
    Submitted,
    Working,
    CancelRequested,
    Filled,
    Cancelled,
    Rejected,
    NeedsReconcile,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ManagedOrder {
    pub intent: crate::types::OrderIntent,
    pub status: ManagedOrderStatus,
    pub cumulative_filled_qty: f64,
    pub reserved_cash_usd: f64,
    pub last_update_ms: EpochMillis,
}

impl ManagedOrder {
    pub fn remaining_qty(&self) -> f64 {
        (self.intent.quantity - self.cumulative_filled_qty).max(0.0)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RuntimeOutcome {
    pub commands: Vec<RuntimeCommand>,
    pub event_seqs: Vec<u64>,
}

impl RuntimeOutcome {
    pub fn push_event(&mut self, seq: u64) {
        self.event_seqs.push(seq);
    }

    pub fn push_command(&mut self, command: RuntimeCommand) {
        self.commands.push(command);
    }

    pub fn extend(&mut self, other: RuntimeOutcome) {
        self.commands.extend(other.commands);
        self.event_seqs.extend(other.event_seqs);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RuntimeError {
    Inventory(InventoryError),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inventory(source) => source.fmt(f),
        }
    }
}

impl Error for RuntimeError {}

impl From<InventoryError> for RuntimeError {
    fn from(value: InventoryError) -> Self {
        Self::Inventory(value)
    }
}
