//! Data ingestion and replay-facing storage surfaces.
//!
//! The production collector currently lives on the `phase1/live-collector-bin`
//! branch. This module is kept as the crate-level namespace so strategy,
//! paper, and runtime code can depend on a stable `data` boundary while the
//! collector implementation is merged independently.

pub mod operator_calibration;
