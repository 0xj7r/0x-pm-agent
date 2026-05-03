//! Optional raw-frame tap for the live collector.
//!
//! Each wire-client (market_ws, user_ws, spot_ws) accepts an optional
//! `tokio::sync::mpsc::UnboundedSender<RawFrame>` via `with_raw_tap`. When set,
//! the client clones each parsed frame into a `RawFrame` and pushes it to the
//! tap. When unset (the live trader's default), there is no overhead beyond
//! a single `Option::is_none` check.
//!
//! The collector binary (`bin/live_collector`) constructs each client with a
//! tap and routes the resulting frames into the canonical `Event` schema and
//! Firehose batch sink. The trader path is unaffected.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Collector-side wall-clock in unix nanoseconds. Saturates to 0 if the system
/// clock is set before the epoch (never expected in production; the caller
/// will see a synthetic zero rather than a panic).
pub fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// One parsed inbound frame, ready to be normalised into a canonical
/// `collector::Event` record. Cheap to clone (single `Value` allocation;
/// optional small strings).
#[derive(Debug, Clone)]
pub struct RawFrame {
    /// Stable identifier for the upstream feed. Matches the `source` enum in
    /// the canonical event schema (`polymarket_market_ws`,
    /// `polymarket_user_ws`, `binance_aggtrade`, `coinbase_match`, etc).
    pub source: &'static str,

    /// Token id when the frame is per-asset, otherwise `None` (BTC ticks,
    /// market metadata, heartbeats).
    pub asset_id: Option<String>,

    /// Collector receipt time in unix nanoseconds. Monotonic per `(host,
    /// source)` over a process lifetime; not necessarily across restarts.
    pub observed_at_ns: i64,

    /// Verbatim venue payload. Preserved so the collector never has to
    /// re-collect to recover a field added later.
    pub payload: Value,
}
