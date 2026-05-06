//! Phase 3a backtest replay engine.
//!
//! Drives the live strategy engine over canonical v=1 `Event` records
//! (produced by `collector::firehose_sink`, partitioned per
//! `collector::partition`, and converted to Parquet by Phase 2 Glue).
//!
//! Determinism contract (per
//! `docs/superpowers/specs/2026-05-02-phase3-backtest-runner-design.md`):
//! - No wall-clock reads inside this module. The replay clock advances on
//!   `received_ns` only.
//! - No `HashMap` iteration in any path that produces an output record;
//!   `BTreeMap` everywhere.
//! - Sort key after dedup is `(received_ns, market_slug, asset_id, sequence,
//!   event_type, source)`.
//! - Run-id is `sha256(canonical_profile || window_plan || git_rev ||
//!   schema_version || fill_sim_version)[..16]`.

pub mod fill_sim;
pub mod journal;
pub mod manifest;
pub mod reader;
pub mod risk_trace;
pub mod runner;
pub mod strategy_adapter;
pub mod synthesizer;
pub mod window_summary;
