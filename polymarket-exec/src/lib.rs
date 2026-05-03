//! Crate module graph with compatibility re-exports for core and market-making surfaces.

pub mod collector;
pub mod config;
pub mod core;
pub mod data;
pub mod event_log;
pub mod infra;
pub mod journal;
pub mod logging;
pub mod market_making;
pub mod markets;
pub mod metrics;
pub mod paper;
pub mod replay;
pub mod runtime;
pub mod signals;
pub mod strategies;
pub mod strategy;
pub mod strategy_profile;
pub mod wire;

pub use core::{book, inventory, lot_ledger, market_context, risk, types};
pub use market_making as mm;
pub use market_making::pairing::{merge_executor, pair_ledger};
pub use market_making::{quote_engine, quote_reconciler};
