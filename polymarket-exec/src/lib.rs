//! Crate module graph with compatibility re-exports for core and market-making surfaces.

pub mod config;
pub mod core;
pub mod event_log;
pub mod journal;
pub mod logging;
pub mod market_making;
pub mod metrics;
pub mod paper;
pub mod runtime;
pub mod signals;
pub mod strategy;
pub mod wire;

pub use core::{book, inventory, market_context, risk, types};
pub use market_making as mm;
pub use market_making::{merge_executor, pair_ledger, quote_engine, quote_reconciler};
