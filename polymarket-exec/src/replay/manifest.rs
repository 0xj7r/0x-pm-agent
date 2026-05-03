//! Run manifest: content-addressed run-id + window plan.
//!
//! `run_id` derivation per the design spec:
//!   sha256(canonical_profile || window_plan || git_rev || schema_version
//!         || fill_sim_version)[..16]
//!
//! `canonical_profile` is the strategy YAML re-serialized as canonical
//! JSON (sorted keys, no whitespace) so semantically-equivalent profiles
//! produce the same hash.
//!
//! `window_plan` is the (sorted, BTreeMap-iterated) list of
//! `(window_id, start_ns, end_ns)` triples.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::collector::schema::Event;

/// Frozen schema version we read.
pub const SCHEMA_VERSION: u32 = Event::SCHEMA_VERSION;
/// Internal version of the fill-simulator. Bumped whenever the matching
/// algorithm changes; participates in the run-id hash so backtest output
/// is keyed to the simulator that produced it.
pub const FILL_SIM_VERSION: &str = "phase3a-ext-2026-05-02";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowPlan {
    pub window_id: String,
    pub start_ns: i64,
    pub end_ns: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub run_id: String,
    pub git_rev: String,
    pub schema_version: u32,
    pub fill_sim_version: String,
    pub profile_hash: String,
    pub windows: Vec<WindowPlan>,
    pub fill_config: String,
    pub seed: String,
}

/// Hex digest of an arbitrary byte slice.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Canonicalize an arbitrary YAML/JSON-shaped string into a sorted-key,
/// whitespace-stripped JSON byte buffer. Returns the canonical bytes.
pub fn canonicalize(yaml_or_json: &str) -> Result<Vec<u8>> {
    // Parse as YAML first (YAML is a superset of JSON, so this also
    // handles JSON inputs).
    let value: serde_yaml::Value =
        serde_yaml::from_str(yaml_or_json).context("failed to parse profile YAML")?;
    // Re-serialize as JSON with sorted keys, no whitespace.
    let json_value = serde_json::to_value(&value).context("failed to convert to JSON")?;
    Ok(canonical_json(&json_value).into_bytes())
}

/// Walk a `serde_json::Value` and emit a canonical string representation:
/// sorted object keys, no whitespace, deterministic number formatting.
pub fn canonical_json(value: &serde_json::Value) -> String {
    use serde_json::Value;
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).unwrap_or_else(|_| String::from("\"\"")),
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(canonical_json).collect();
            format!("[{}]", parts.join(","))
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let parts: Vec<String> = entries
                .iter()
                .map(|(k, v)| {
                    let key = serde_json::to_string(k).unwrap_or_else(|_| String::from("\"\""));
                    format!("{}:{}", key, canonical_json(v))
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

/// Compute the content-addressed run-id from its inputs. Returns the first
/// 16 hex characters (64 bits) of `sha256` over the canonical concatenation.
pub fn compute_run_id(
    canonical_profile: &[u8],
    window_plan: &[WindowPlan],
    git_rev: &str,
    fill_config: &str,
    seed: u64,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"v=1\n");
    hasher.update(canonical_profile);
    hasher.update(b"\n");
    for w in window_plan {
        hasher.update(w.window_id.as_bytes());
        hasher.update(format!(":{}-{}\n", w.start_ns, w.end_ns).as_bytes());
    }
    hasher.update(git_rev.as_bytes());
    hasher.update(b"\n");
    hasher.update(SCHEMA_VERSION.to_le_bytes());
    hasher.update(FILL_SIM_VERSION.as_bytes());
    hasher.update(b"\n");
    hasher.update(fill_config.as_bytes());
    hasher.update(b"\n");
    hasher.update(seed.to_le_bytes());
    let digest = hasher.finalize();
    hex(&digest[..8])
}

/// Convenience: profile_hash separate from run-id (used as a manifest field
/// for indexed lookups across runs sharing a profile).
pub fn profile_hash(canonical_profile: &[u8]) -> String {
    sha256_hex(canonical_profile)[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_windows() -> Vec<WindowPlan> {
        vec![WindowPlan {
            window_id: "btc_5m/2026-04-01".into(),
            start_ns: 1_700_000_000_000_000_000,
            end_ns: 1_700_086_400_000_000_000,
        }]
    }

    #[test]
    fn canonicalize_yaml_is_whitespace_independent() {
        let a = "a: 1\nb: [2, 3]\n";
        let b = "a:    1\nb: [ 2 ,3 ]\n";
        assert_eq!(canonicalize(a).unwrap(), canonicalize(b).unwrap());
    }

    #[test]
    fn canonicalize_yaml_is_key_order_independent() {
        let a = "a: 1\nb: 2\n";
        let b = "b: 2\na: 1\n";
        assert_eq!(canonicalize(a).unwrap(), canonicalize(b).unwrap());
    }

    #[test]
    fn run_id_is_stable_for_same_inputs() {
        let profile = canonicalize("name: paired_mm\nversion: 1\n").unwrap();
        let id1 = compute_run_id(&profile, &dummy_windows(), "abc123", "nominal", 0xC0FFEE);
        let id2 = compute_run_id(&profile, &dummy_windows(), "abc123", "nominal", 0xC0FFEE);
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 16);
    }

    #[test]
    fn run_id_changes_when_inputs_change() {
        let profile = canonicalize("name: paired_mm\n").unwrap();
        let base = compute_run_id(&profile, &dummy_windows(), "abc", "nominal", 0);
        let other_profile = canonicalize("name: pair_cost_arb\n").unwrap();
        assert_ne!(
            base,
            compute_run_id(&other_profile, &dummy_windows(), "abc", "nominal", 0)
        );
        let other_windows = vec![WindowPlan {
            window_id: "different".into(),
            start_ns: 0,
            end_ns: 1,
        }];
        assert_ne!(
            base,
            compute_run_id(&profile, &other_windows, "abc", "nominal", 0)
        );
        assert_ne!(
            base,
            compute_run_id(&profile, &dummy_windows(), "def", "nominal", 0)
        );
        assert_ne!(
            base,
            compute_run_id(&profile, &dummy_windows(), "abc", "conservative", 0)
        );
        assert_ne!(
            base,
            compute_run_id(&profile, &dummy_windows(), "abc", "nominal", 1)
        );
    }
}
