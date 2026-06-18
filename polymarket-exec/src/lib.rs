//! Crate module graph with compatibility re-exports for the core surface.

pub mod collector;
pub mod config;
pub mod core;
pub mod data;
pub mod event_log;
pub mod infra;
pub mod logging;
pub mod markets;
pub mod metrics;
pub mod shadow_exec;
pub mod shadow_gamma;
pub mod shadow_jsonl;
pub mod shadow_parity;
pub mod wire;

pub use core::{book, inventory, lot_ledger, market_context, risk, types};
