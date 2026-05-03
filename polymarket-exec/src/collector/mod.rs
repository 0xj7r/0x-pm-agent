//! Live event collector module.
//!
//! Streams canonical `Event` records (v=1 schema) from the existing
//! `wire::market_ws`, `wire::user_ws`, and `wire::spot_ws` clients to AWS
//! Kinesis Firehose. Phase 1a per the architecture v2 addendum.

pub mod discovery;
pub mod firehose_sink;
pub mod gap_detector;
pub mod health;
pub mod metrics;
pub mod partition;
pub mod schema;

pub use schema::{Event, EventType, Source};
