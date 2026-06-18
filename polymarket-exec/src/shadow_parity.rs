//! LIVE ↔ shadow-final parity gate for the JSONL-tailing executor.
//!
//! Every venue submit must match a tailed `would_enter` on `(slug, side, clip)`
//! within the configured window (default 120s). Emits structured JSONL audit events.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use pm_shadow::ExecIntent;
use serde::Serialize;
use tracing::warn;

const DEFAULT_WINDOW_S: f64 = 120.0;
const DEFAULT_STATS_INTERVAL_S: u64 = 300;

#[derive(Debug, Clone, Serialize)]
struct ParityEvent {
    #[serde(rename = "type")]
    event_type: &'static str,
    kind: &'static str,
    slug: String,
    side: String,
    clip: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    ref_ts_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    submit_ts_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    age_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
struct ParityStatsEvent {
    #[serde(rename = "type")]
    event_type: &'static str,
    matched: u64,
    orphan: u64,
    missed_ref: u64,
    window_s: f64,
}

#[derive(Debug, Clone)]
struct ParityRef {
    slug: String,
    side: String,
    clip: u32,
    ref_ts_s: f64,
    matched: bool,
    expired: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ParityStats {
    pub matched: u64,
    pub orphan: u64,
    pub missed_ref: u64,
}

pub struct ParityGate {
    window_s: f64,
    stats_interval_s: u64,
    refs: Vec<ParityRef>,
    stats: ParityStats,
    log_path: Option<PathBuf>,
    last_stats_log_s: f64,
}

impl ParityGate {
    pub fn from_env() -> Self {
        let window_s = std::env::var("PM_SHADOW_PARITY_WINDOW_S")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(DEFAULT_WINDOW_S);
        let stats_interval_s = std::env::var("PM_SHADOW_PARITY_STATS_MIN")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .map(|m| m * 60)
            .unwrap_or(DEFAULT_STATS_INTERVAL_S);
        let log_path = std::env::var("PM_SHADOW_PARITY_LOG_PATH")
            .ok()
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty());
        Self {
            window_s,
            stats_interval_s,
            refs: Vec::new(),
            stats: ParityStats::default(),
            log_path,
            last_stats_log_s: 0.0,
        }
    }

    pub fn stats(&self) -> &ParityStats {
        &self.stats
    }

    pub fn window_s(&self) -> f64 {
        self.window_s
    }

    pub fn record_would_enter(&mut self, slug: &str, side: &str, clip: u32, ref_ts_s: f64) {
        self.refs.push(ParityRef {
            slug: slug.to_string(),
            side: side.to_string(),
            clip,
            ref_ts_s,
            matched: false,
            expired: false,
        });
    }

    /// Returns true when submit is authorized (matching ref within window).
    pub fn authorize_submit(&mut self, intent: &ExecIntent, submit_ts_s: f64) -> bool {
        if let Some(idx) = self.find_open_ref(&intent.slug, &intent.side, intent.clip, submit_ts_s)
        {
            self.refs[idx].matched = true;
            self.stats.matched += 1;
            let delta_s = submit_ts_s - self.refs[idx].ref_ts_s;
            self.emit(ParityEvent {
                event_type: "parity_event",
                kind: "matched",
                slug: intent.slug.clone(),
                side: intent.side.clone(),
                clip: intent.clip,
                ref_ts_s: Some(self.refs[idx].ref_ts_s),
                submit_ts_s: Some(submit_ts_s),
                delta_s: Some(delta_s),
                age_s: None,
                reason: None,
            });
            return true;
        }

        self.stats.orphan += 1;
        self.emit(ParityEvent {
            event_type: "parity_event",
            kind: "orphan",
            slug: intent.slug.clone(),
            side: intent.side.clone(),
            clip: intent.clip,
            ref_ts_s: None,
            submit_ts_s: Some(submit_ts_s),
            delta_s: None,
            age_s: None,
            reason: Some("no_matching_would_enter"),
        });
        warn!(
            target: "shadow_parity",
            slug = %intent.slug,
            side = %intent.side,
            clip = intent.clip,
            "ORPHAN submit rejected — no matching would_enter within {}s",
            self.window_s
        );
        false
    }

    pub fn tick_expired(&mut self, now_s: f64) {
        let mut missed: Vec<(String, String, u32, f64, f64)> = Vec::new();
        for pr in &mut self.refs {
            if pr.matched || pr.expired {
                continue;
            }
            let age_s = now_s - pr.ref_ts_s;
            if age_s > self.window_s {
                pr.expired = true;
                self.stats.missed_ref += 1;
                missed.push((
                    pr.slug.clone(),
                    pr.side.clone(),
                    pr.clip,
                    pr.ref_ts_s,
                    age_s,
                ));
            }
        }
        for (slug, side, clip, ref_ts_s, age_s) in missed {
            self.emit(ParityEvent {
                event_type: "parity_event",
                kind: "missed_ref",
                slug: slug.clone(),
                side: side.clone(),
                clip,
                ref_ts_s: Some(ref_ts_s),
                submit_ts_s: None,
                delta_s: None,
                age_s: Some(age_s),
                reason: Some("submit_not_seen_within_window"),
            });
            warn!(
                target: "shadow_parity",
                slug = %slug,
                side = %side,
                clip,
                age_s,
                "MISSED REF — would_enter not submitted within {}s",
                self.window_s
            );
        }
    }

    pub fn maybe_log_stats(&mut self, now_s: f64) {
        if self.last_stats_log_s > 0.0
            && now_s - self.last_stats_log_s < self.stats_interval_s as f64
        {
            return;
        }
        self.last_stats_log_s = now_s;
        self.emit_stats();
    }

    fn find_open_ref(
        &self,
        slug: &str,
        side: &str,
        clip: u32,
        submit_ts_s: f64,
    ) -> Option<usize> {
        self.refs.iter().position(|pr| {
            !pr.matched
                && !pr.expired
                && pr.slug == slug
                && pr.side == side
                && pr.clip == clip
                && (submit_ts_s - pr.ref_ts_s).abs() <= self.window_s
        })
    }

    fn emit(&self, event: ParityEvent) {
        if let Ok(line) = serde_json::to_string(&event) {
            tracing::info!(target: "shadow_parity", "{line}");
            if let Some(path) = &self.log_path {
                if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
    }

    fn emit_stats(&self) {
        let event = ParityStatsEvent {
            event_type: "parity_stats",
            matched: self.stats.matched,
            orphan: self.stats.orphan,
            missed_ref: self.stats.missed_ref,
            window_s: self.window_s,
        };
        if let Ok(line) = serde_json::to_string(&event) {
            tracing::info!(target: "shadow_parity", "{line}");
            if let Some(path) = &self.log_path {
                if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
    }
}

pub fn now_unix_s() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn parse_ts_utc(ts: &str) -> Option<f64> {
    if ts.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.timestamp() as f64 + f64::from(dt.timestamp_subsec_micros()) / 1_000_000.0)
}

pub fn paper_mode_from_env() -> bool {
    env_truthy(&["PAPER_MODE", "PM_SHADOW_PAPER_MODE", "PM_FADE_PAPER_MODE"])
}

fn env_truthy(names: &[&str]) -> bool {
    names.iter().any(|name| {
        std::env::var(name)
            .ok()
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or(false)
    })
}

/// Shared gate for tailer + execution loop.
pub type SharedParityGate = std::sync::Arc<Mutex<ParityGate>>;

pub fn shared_gate() -> SharedParityGate {
    std::sync::Arc::new(Mutex::new(ParityGate::from_env()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(slug: &str, side: &str, clip: u32) -> ExecIntent {
        ExecIntent {
            slug: slug.to_string(),
            side: side.to_string(),
            token_id: "t".to_string(),
            p_exo: 0.6,
            p_side: 0.6,
            touch_price: 0.5,
            marketable_limit_price: 0.55,
            target_notional: 50.0,
            hold_to_redemption: true,
            clip,
            edge: 0.1,
            sigma_bar_bps: 0.0,
            strike: 0.0,
            close_ts_s: 1,
            condition_id: None,
            up_index_set: 1,
            down_index_set: 2,
        }
    }

    #[test]
    fn authorize_matches_within_window() {
        let mut gate = ParityGate::from_env();
        gate.window_s = 120.0;
        gate.record_would_enter("s1", "up", 1, 1000.0);
        assert!(gate.authorize_submit(&intent("s1", "up", 1), 1050.0));
        assert_eq!(gate.stats().matched, 1);
        assert_eq!(gate.stats().orphan, 0);
    }

    #[test]
    fn authorize_rejects_orphan_outside_window() {
        let mut gate = ParityGate::from_env();
        gate.window_s = 120.0;
        gate.record_would_enter("s1", "up", 1, 1000.0);
        assert!(!gate.authorize_submit(&intent("s1", "up", 1), 1300.0));
        assert_eq!(gate.stats().orphan, 1);
    }

    #[test]
    fn tick_expired_marks_missed_ref() {
        let mut gate = ParityGate::from_env();
        gate.window_s = 120.0;
        gate.record_would_enter("s1", "down", 2, 1000.0);
        gate.tick_expired(1121.0);
        assert_eq!(gate.stats().missed_ref, 1);
        assert!(!gate.authorize_submit(&intent("s1", "down", 2), 1122.0));
        assert_eq!(gate.stats().orphan, 1);
    }

    #[test]
    fn parse_ts_utc_rfc3339() {
        let ts = parse_ts_utc("2026-06-16T10:20:01Z").expect("parse");
        assert!(ts > 1_700_000_000.0);
    }
}