//! CloudWatch Embedded-Metric-Format (EMF) emitter.
//!
//! Writes a single JSON line to stdout per snapshot. CloudWatch Logs
//! parses these and surfaces them as metrics in the `pm-research` namespace
//! with no SDK or PutMetricData calls. We tee them to tracing as well so
//! humans can read them in container logs.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// CloudWatch metric namespace.
pub const NAMESPACE: &str = "pm-research/collector";

/// Snapshot of metric counters and gauges captured at one point in time.
#[derive(Debug, Default, Clone)]
pub struct CollectorMetricsSnapshot {
    pub events_processed_total: u64,
    pub ws_disconnects_total: u64,
    pub gap_seconds_max: f64,
    pub firehose_put_latency_ms: f64,
    pub firehose_throttle_count: u64,
    pub snapshot_age_seconds: f64,
    pub btc_tick_age_seconds: f64,
}

/// Build the EMF JSON payload for a snapshot. Pure function for testability.
pub fn build_emf(snapshot: &CollectorMetricsSnapshot, now_ms: i64) -> Value {
    json!({
        "_aws": {
            "Timestamp": now_ms,
            "CloudWatchMetrics": [{
                "Namespace": NAMESPACE,
                "Dimensions": [[]],
                "Metrics": [
                    {"Name": "events_processed_total", "Unit": "Count"},
                    {"Name": "ws_disconnects_total", "Unit": "Count"},
                    {"Name": "gap_seconds_max", "Unit": "Seconds"},
                    {"Name": "firehose_put_latency_ms", "Unit": "Milliseconds"},
                    {"Name": "firehose_throttle_count", "Unit": "Count"},
                    {"Name": "snapshot_age_seconds", "Unit": "Seconds"},
                    {"Name": "btc_tick_age_seconds", "Unit": "Seconds"},
                ]
            }]
        },
        "events_processed_total": snapshot.events_processed_total,
        "ws_disconnects_total": snapshot.ws_disconnects_total,
        "gap_seconds_max": snapshot.gap_seconds_max,
        "firehose_put_latency_ms": snapshot.firehose_put_latency_ms,
        "firehose_throttle_count": snapshot.firehose_throttle_count,
        "snapshot_age_seconds": snapshot.snapshot_age_seconds,
        "btc_tick_age_seconds": snapshot.btc_tick_age_seconds,
    })
}

/// Emit a snapshot to stdout as one EMF JSON line. Returns the line.
pub fn emit(snapshot: &CollectorMetricsSnapshot) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let payload = build_emf(snapshot, now_ms);
    let line = payload.to_string();
    println!("{line}");
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emf_payload_carries_namespace_and_all_metric_names() {
        let snap = CollectorMetricsSnapshot {
            events_processed_total: 12,
            ws_disconnects_total: 3,
            gap_seconds_max: 17.5,
            firehose_put_latency_ms: 84.0,
            firehose_throttle_count: 1,
            snapshot_age_seconds: 1.2,
            btc_tick_age_seconds: 0.4,
        };
        let payload = build_emf(&snap, 1_700_000_000_000);
        assert_eq!(payload["_aws"]["Timestamp"], json!(1_700_000_000_000i64));
        assert_eq!(
            payload["_aws"]["CloudWatchMetrics"][0]["Namespace"],
            json!(NAMESPACE)
        );

        let metrics = payload["_aws"]["CloudWatchMetrics"][0]["Metrics"]
            .as_array()
            .expect("metrics array");
        let names: Vec<&str> = metrics
            .iter()
            .map(|m| m["Name"].as_str().unwrap())
            .collect();
        for required in [
            "events_processed_total",
            "ws_disconnects_total",
            "gap_seconds_max",
            "firehose_put_latency_ms",
            "firehose_throttle_count",
            "snapshot_age_seconds",
            "btc_tick_age_seconds",
        ] {
            assert!(names.contains(&required), "missing metric {required}");
        }

        assert_eq!(payload["events_processed_total"], json!(12u64));
        assert_eq!(payload["gap_seconds_max"], json!(17.5));
    }

    #[test]
    fn empty_snapshot_serializes_with_zeros() {
        let snap = CollectorMetricsSnapshot::default();
        let payload = build_emf(&snap, 0);
        assert_eq!(payload["events_processed_total"], json!(0u64));
        assert_eq!(payload["gap_seconds_max"], json!(0.0));
    }
}
