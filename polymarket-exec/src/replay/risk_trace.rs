//! Risk-rejection trace strand.
//!
//! When the replay adapter routes a strategy intent through the live
//! `core::risk::RiskEngine` and the engine REJECTS the intent, the
//! adapter emits a `RiskRejection` record. The runner accumulates these
//! per window and surfaces them on `WindowSummary` so a downstream
//! emitter can write
//! `runs/run_id=<id>/trace/strand=risk_rejections/...parquet`.
//!
//! Determinism: the record carries no wall-clock timestamps. `ts_ns` is
//! derived from the originating event's `received_ns`.

use serde::{Deserialize, Serialize};

use crate::risk::RiskRejectReason;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskRejection {
    /// Replay virtual time at which the engine evaluated the intent.
    pub ts_ns: i64,
    pub intent_id: String,
    pub market_id: String,
    pub asset_id: String,
    /// `place` for new orders; `cancel`/`modify` reserved for future
    /// expansion (the live runtime currently routes only place-style
    /// intents through the risk evaluator).
    pub intent_kind: String,
    /// `buy` / `sell`.
    pub side: String,
    /// Decimal string (avoids f64 round-trip drift in Parquet output).
    pub price: String,
    pub size: String,
    /// Typed risk-engine reason. Snake-case (matches the enum's
    /// `rename_all = "snake_case"` serde annotation).
    pub reject_reason: RiskRejectReason,
    /// Free-form human-readable message produced by the engine. Useful
    /// as a debugging aid; downstream consumers should pivot on
    /// `reject_reason` instead.
    pub reject_message: String,
    /// Snapshot of which caps were tight at this moment. Stored as JSON
    /// so the schema can grow without a manifest version bump.
    pub caps_at_eval: serde_json::Value,
}

impl RiskRejection {
    /// Convenience constructor used by the replay adapter.
    pub fn new(
        ts_ns: i64,
        intent_id: impl Into<String>,
        market_id: impl Into<String>,
        asset_id: impl Into<String>,
        intent_kind: impl Into<String>,
        side: impl Into<String>,
        price: f64,
        size: f64,
        reject_reason: RiskRejectReason,
        reject_message: impl Into<String>,
        caps_at_eval: serde_json::Value,
    ) -> Self {
        Self {
            ts_ns,
            intent_id: intent_id.into(),
            market_id: market_id.into(),
            asset_id: asset_id.into(),
            intent_kind: intent_kind.into(),
            side: side.into(),
            price: format_decimal(price),
            size: format_decimal(size),
            reject_reason,
            reject_message: reject_message.into(),
            caps_at_eval,
        }
    }
}

fn format_decimal(v: f64) -> String {
    // 6dp matches the price_to_ticks scale used by fill_sim and is
    // sufficient for outcome-token sizes; trailing zeros stripped to
    // keep the trace compact.
    let mut s = format!("{:.6}", v);
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_rejection_serializes_with_typed_reason() {
        let rr = RiskRejection::new(
            1_700_000_000_000_000_000,
            "coid-1",
            "btc-up",
            "0xyes",
            "place",
            "buy",
            0.55,
            10.0,
            RiskRejectReason::OrderNotionalTooLarge,
            "exceeds cap",
            serde_json::json!({"max_order_notional_usd": 5.0}),
        );
        let s = serde_json::to_string(&rr).unwrap();
        assert!(s.contains("\"reject_reason\":\"order_notional_too_large\""));
        assert!(s.contains("\"price\":\"0.55\""));
        assert!(s.contains("\"size\":\"10\""));
    }

    #[test]
    fn format_decimal_strips_trailing_zeros() {
        assert_eq!(format_decimal(0.55), "0.55");
        assert_eq!(format_decimal(10.0), "10");
        assert_eq!(format_decimal(0.500000), "0.5");
        assert_eq!(format_decimal(0.123456), "0.123456");
    }
}
