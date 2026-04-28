//! Fidelity scoring: compares our shadow's predicted fills to bonereaper's
//! actual on-chain fills (via /activity API) and emits a per-minute,
//! per-market-family verdict that gates live deployment.
//!
//! Phase 7 will add the polling logic. This file currently defines the
//! event/verdict types so journal.rs can journal them and the live process
//! can read them at the deploy gate.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::core::types::EpochMillis;

#[derive(Debug, Clone, PartialEq)]
pub struct ShadowFill {
    pub market_family: String,
    pub notional_usd: f64,
    pub rebate_usd: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BonereaperFill {
    pub market_family: String,
    pub notional_usd: f64,
    pub rebate_usd: f64,
}

/// First-party fills observed on our own wallet. When Approach C is
/// deployed (live bonereaper at micro-capital), our own fills give us
/// symmetric microstructure with shadow predictions: place-time, queue
/// depth at submit, fill time. Our-live-vs-shadow MAPE is therefore
/// the microstructure-validating signal; bonereaper-vs-shadow stays as
/// the coarser external cross-check.
#[derive(Debug, Clone, PartialEq)]
pub struct OurLiveFill {
    pub market_family: String,
    pub notional_usd: f64,
    pub rebate_usd: f64,
}

pub fn summarise_window(
    shadow_fills: &[ShadowFill],
    bonereaper_fills: &[BonereaperFill],
    window_start_ms: EpochMillis,
    window_end_ms: EpochMillis,
    observed_at_ms: EpochMillis,
) -> Vec<FidelityEvent> {
    summarise_window_with_our_live(
        shadow_fills,
        &[],
        bonereaper_fills,
        window_start_ms,
        window_end_ms,
        observed_at_ms,
    )
}

pub fn summarise_window_with_our_live(
    shadow_fills: &[ShadowFill],
    our_live_fills: &[OurLiveFill],
    bonereaper_fills: &[BonereaperFill],
    window_start_ms: EpochMillis,
    window_end_ms: EpochMillis,
    observed_at_ms: EpochMillis,
) -> Vec<FidelityEvent> {
    #[derive(Default)]
    struct Aggregate {
        shadow_count: u32,
        shadow_notional: f64,
        shadow_rebate: f64,
        our_live_count: u32,
        our_live_notional: f64,
        our_live_rebate: f64,
        bonereaper_count: u32,
        bonereaper_notional: f64,
        bonereaper_rebate: f64,
    }
    let mut by_family: BTreeMap<String, Aggregate> = BTreeMap::new();
    for f in shadow_fills {
        let agg = by_family.entry(f.market_family.clone()).or_default();
        agg.shadow_count += 1;
        agg.shadow_notional += f.notional_usd;
        agg.shadow_rebate += f.rebate_usd;
    }
    for f in our_live_fills {
        let agg = by_family.entry(f.market_family.clone()).or_default();
        agg.our_live_count += 1;
        agg.our_live_notional += f.notional_usd;
        agg.our_live_rebate += f.rebate_usd;
    }
    for f in bonereaper_fills {
        let agg = by_family.entry(f.market_family.clone()).or_default();
        agg.bonereaper_count += 1;
        agg.bonereaper_notional += f.notional_usd;
        agg.bonereaper_rebate += f.rebate_usd;
    }
    by_family
        .into_iter()
        .map(|(family, agg)| {
            // Truth precedence: our_live > bonereaper. our_live is the
            // microstructure-symmetric source; bonereaper is coarser
            // count-level only.
            let truth_count = if agg.our_live_count > 0 {
                agg.our_live_count
            } else {
                agg.bonereaper_count
            };
            let mape = if truth_count == 0 {
                if agg.shadow_count == 0 {
                    0.0
                } else {
                    f64::INFINITY
                }
            } else {
                ((agg.shadow_count as f64) - (truth_count as f64)).abs()
                    / (truth_count as f64)
            };
            FidelityEvent {
                observed_at_ms,
                market_family: family,
                window_start_ms,
                window_end_ms,
                shadow_fill_count: agg.shadow_count,
                shadow_fill_notional_usd: agg.shadow_notional,
                shadow_rebate_usd: agg.shadow_rebate,
                our_live_fill_count: agg.our_live_count,
                our_live_fill_notional_usd: agg.our_live_notional,
                our_live_rebate_usd: agg.our_live_rebate,
                bonereaper_fill_count: agg.bonereaper_count,
                bonereaper_fill_notional_usd: agg.bonereaper_notional,
                bonereaper_rebate_usd: agg.bonereaper_rebate,
                mape_fill_count: mape,
                verdict: FidelityVerdict::from_mape(mape),
            }
        })
        .collect()
}

pub fn extract_market_family(slug: &str) -> &str {
    if let Some(idx) = slug.rfind('-') {
        &slug[..idx]
    } else {
        slug
    }
}

/// Maximum staleness for bonereaper /activity truth data before we mark
/// the fidelity verdict as untrusted. Five minutes is enough to absorb
/// normal API latency / our 60s poll cadence; beyond that the truth
/// source itself is suspect (Polymarket caching, network, etc.) and we
/// should not block the live deploy gate on stale truth.
pub const FIDELITY_TRUTH_MAX_STALENESS_MS: u64 = 5 * 60 * 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruthFreshness {
    Fresh,
    Stale,
}

pub fn parse_bonereaper_activity(json: &serde_json::Value) -> Vec<(BonereaperFill, EpochMillis)> {
    let mut out = Vec::new();
    let Some(rows) = json.as_array() else {
        return out;
    };
    for row in rows {
        // Only TRADE rows count as fills; REDEEM/MERGE are lifecycle ops.
        let row_type = row.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if !row_type.eq_ignore_ascii_case("TRADE") {
            continue;
        }
        let Some(slug) = row.get("slug").and_then(|v| v.as_str()) else {
            continue;
        };
        let market_family = extract_market_family(slug).to_string();
        let price = row.get("price").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let size = row.get("size").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let usdc_size = row
            .get("usdcSize")
            .and_then(|v| v.as_f64())
            .unwrap_or(price * size);
        // Activity timestamps are epoch seconds; convert to ms.
        let ts_ms = row
            .get("timestamp")
            .and_then(|v| v.as_u64())
            .map(|s| s.saturating_mul(1_000))
            .unwrap_or(0);
        // Rebate is not directly populated on /activity rows. Set to 0
        // here; the rebate-aware path uses /rebates/current separately.
        out.push((
            BonereaperFill {
                market_family,
                notional_usd: usdc_size,
                rebate_usd: 0.0,
            },
            ts_ms,
        ));
    }
    out
}

pub fn truth_freshness(
    newest_truth_ms: Option<EpochMillis>,
    now_ms: EpochMillis,
    max_staleness_ms: u64,
) -> TruthFreshness {
    match newest_truth_ms {
        Some(ts) if ts.saturating_add(max_staleness_ms) >= now_ms => TruthFreshness::Fresh,
        _ => TruthFreshness::Stale,
    }
}

/// Window for the live-deploy gate. 24h matches the spec: the live
/// process refuses to start strategy entries unless an OK verdict is
/// observable in the most recent 24 wall-clock hours of shadow output.
pub const FIDELITY_GATE_DEFAULT_WINDOW_MS: u64 = 24 * 60 * 60 * 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Pass,
    RiskOff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateReason {
    Pass,
    /// No fidelity_event rows in the journal within the window. Could
    /// mean shadow is not running, the journal path is wrong, or shadow
    /// is in cold-warmup. Treated as evidence-of-absence.
    ShadowUnobserved,
    /// At least one Fail verdict was observed in the window.
    ShadowFail,
}

#[derive(Debug, Clone, Serialize)]
pub struct GateDecision {
    pub status: GateStatus,
    pub reason: GateReason,
    pub newest_event_ms: Option<EpochMillis>,
    pub ok_count: u32,
    pub warn_count: u32,
    pub fail_count: u32,
}

/// Read the shadow process's journal file and decide whether the live
/// process is allowed to enter strategy mode. Pass requires at least one
/// fidelity_event row in the window AND zero Fail verdicts in that
/// window. Absence-of-evidence is treated as evidence-of-absence
/// (RiskOff with reason=ShadowUnobserved).
pub fn evaluate_live_deploy_gate(
    shadow_journal_path: &std::path::Path,
    now_ms: EpochMillis,
    window_ms: u64,
) -> GateDecision {
    use std::io::BufRead;
    let mut decision = GateDecision {
        status: GateStatus::RiskOff,
        reason: GateReason::ShadowUnobserved,
        newest_event_ms: None,
        ok_count: 0,
        warn_count: 0,
        fail_count: 0,
    };
    let cutoff = now_ms.saturating_sub(window_ms);
    let Ok(file) = std::fs::File::open(shadow_journal_path) else {
        return decision;
    };
    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value.get("kind").and_then(|v| v.as_str()) != Some("fidelity_event") {
            continue;
        }
        let Some(event) = value.get("event") else {
            continue;
        };
        let window_end_ms = event
            .get("window_end_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if window_end_ms < cutoff {
            continue;
        }
        decision.newest_event_ms = Some(
            decision
                .newest_event_ms
                .map(|prev| prev.max(window_end_ms))
                .unwrap_or(window_end_ms),
        );
        let verdict = event.get("verdict").and_then(|v| v.as_str()).unwrap_or("");
        match verdict {
            "ok" => decision.ok_count += 1,
            "warn" => decision.warn_count += 1,
            "fail" => decision.fail_count += 1,
            _ => {}
        }
    }
    if decision.fail_count > 0 {
        decision.status = GateStatus::RiskOff;
        decision.reason = GateReason::ShadowFail;
    } else if decision.ok_count > 0 || decision.warn_count > 0 {
        decision.status = GateStatus::Pass;
        decision.reason = GateReason::Pass;
    }
    decision
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FidelityVerdict {
    Ok,
    Warn,
    Fail,
}

/// Verdict band thresholds. Source-of-truth constants per CLAUDE.md
/// "Constants bound the bot; signals optimize it" — never an env knob.
pub const FIDELITY_OK_MAX_MAPE: f64 = 0.30;
pub const FIDELITY_WARN_MAX_MAPE: f64 = 0.75;

impl FidelityVerdict {
    pub fn from_mape(mape: f64) -> Self {
        if !mape.is_finite() || mape > FIDELITY_WARN_MAX_MAPE {
            Self::Fail
        } else if mape > FIDELITY_OK_MAX_MAPE {
            Self::Warn
        } else {
            Self::Ok
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FidelityEvent {
    pub observed_at_ms: EpochMillis,
    pub market_family: String,
    pub window_start_ms: EpochMillis,
    pub window_end_ms: EpochMillis,
    pub shadow_fill_count: u32,
    pub shadow_fill_notional_usd: f64,
    pub shadow_rebate_usd: f64,
    /// First-party fills observed on our wallet (Approach C deployed).
    /// Zero when we have not yet shipped live bonereaper.
    pub our_live_fill_count: u32,
    pub our_live_fill_notional_usd: f64,
    pub our_live_rebate_usd: f64,
    pub bonereaper_fill_count: u32,
    pub bonereaper_fill_notional_usd: f64,
    pub bonereaper_rebate_usd: f64,
    /// MAPE between `shadow_fill_count` and the primary truth source.
    /// Truth precedence: our_live > bonereaper. When both are zero and
    /// shadow is non-zero, mape = +inf (a Fail).
    pub mape_fill_count: f64,
    pub verdict: FidelityVerdict,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_ok_below_30pct() {
        assert_eq!(FidelityVerdict::from_mape(0.0), FidelityVerdict::Ok);
        assert_eq!(FidelityVerdict::from_mape(0.30), FidelityVerdict::Ok);
    }

    #[test]
    fn verdict_warn_between_30_and_75() {
        assert_eq!(FidelityVerdict::from_mape(0.31), FidelityVerdict::Warn);
        assert_eq!(FidelityVerdict::from_mape(0.75), FidelityVerdict::Warn);
    }

    #[test]
    fn verdict_fail_above_75pct() {
        assert_eq!(FidelityVerdict::from_mape(0.76), FidelityVerdict::Fail);
        assert_eq!(FidelityVerdict::from_mape(2.5), FidelityVerdict::Fail);
    }

    #[test]
    fn verdict_fail_on_nan_or_inf() {
        assert_eq!(FidelityVerdict::from_mape(f64::NAN), FidelityVerdict::Fail);
        assert_eq!(FidelityVerdict::from_mape(f64::INFINITY), FidelityVerdict::Fail);
    }

    #[test]
    fn summarise_empty_inputs_returns_no_events() {
        let events = summarise_window(&[], &[], 0, 60_000, 100);
        assert!(events.is_empty());
    }

    #[test]
    fn summarise_equal_counts_one_family_emits_ok_verdict() {
        let shadow = vec![ShadowFill {
            market_family: "btc-updown-5m".to_string(),
            notional_usd: 20.0,
            rebate_usd: 0.04,
        }];
        let bonereaper = vec![BonereaperFill {
            market_family: "btc-updown-5m".to_string(),
            notional_usd: 24.0,
            rebate_usd: 0.05,
        }];
        let events = summarise_window(&shadow, &bonereaper, 0, 60_000, 60_500);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].market_family, "btc-updown-5m");
        assert_eq!(events[0].shadow_fill_count, 1);
        assert_eq!(events[0].bonereaper_fill_count, 1);
        assert!(events[0].mape_fill_count.abs() < 1e-9);
        assert_eq!(events[0].verdict, FidelityVerdict::Ok);
    }

    #[test]
    fn summarise_shadow_misses_all_bonereaper_fills_emits_fail_verdict() {
        let bonereaper = vec![
            BonereaperFill {
                market_family: "btc-updown-5m".to_string(),
                notional_usd: 24.0,
                rebate_usd: 0.05,
            },
            BonereaperFill {
                market_family: "btc-updown-5m".to_string(),
                notional_usd: 18.0,
                rebate_usd: 0.04,
            },
        ];
        let events = summarise_window(&[], &bonereaper, 0, 60_000, 60_500);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].shadow_fill_count, 0);
        assert_eq!(events[0].bonereaper_fill_count, 2);
        assert!((events[0].mape_fill_count - 1.0).abs() < 1e-9);
        assert_eq!(events[0].verdict, FidelityVerdict::Fail);
    }

    #[test]
    fn summarise_emits_one_event_per_market_family() {
        let shadow = vec![
            ShadowFill {
                market_family: "btc-updown-5m".to_string(),
                notional_usd: 20.0,
                rebate_usd: 0.04,
            },
            ShadowFill {
                market_family: "eth-updown-15m".to_string(),
                notional_usd: 15.0,
                rebate_usd: 0.03,
            },
        ];
        let bonereaper = vec![BonereaperFill {
            market_family: "btc-updown-5m".to_string(),
            notional_usd: 24.0,
            rebate_usd: 0.05,
        }];
        let mut events = summarise_window(&shadow, &bonereaper, 0, 60_000, 60_500);
        events.sort_by(|a, b| a.market_family.cmp(&b.market_family));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].market_family, "btc-updown-5m");
        assert_eq!(events[1].market_family, "eth-updown-15m");
        assert_eq!(events[1].shadow_fill_count, 1);
        assert_eq!(events[1].bonereaper_fill_count, 0);
    }

    #[test]
    fn extract_market_family_strips_trailing_timestamp() {
        assert_eq!(extract_market_family("btc-updown-5m-1776961500"), "btc-updown-5m");
        assert_eq!(extract_market_family("eth-updown-15m-1776960900"), "eth-updown-15m");
    }

    fn write_fidelity_journal(rows: &[serde_json::Value]) -> std::path::PathBuf {
        use std::io::Write;
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("fidelity-gate-{unique}.jsonl"));
        let mut file = std::fs::File::create(&path).unwrap();
        for row in rows {
            writeln!(file, "{}", serde_json::to_string(row).unwrap()).unwrap();
        }
        path
    }

    fn fidelity_event_row(window_end_ms: u64, verdict: &str) -> serde_json::Value {
        serde_json::json!({
            "kind": "fidelity_event",
            "event": {
                "window_end_ms": window_end_ms,
                "verdict": verdict,
                "market_family": "btc-updown-5m",
            }
        })
    }

    #[test]
    fn gate_returns_riskoff_shadow_unobserved_when_journal_missing() {
        let path = std::path::PathBuf::from("/nonexistent/journal.jsonl");
        let d = evaluate_live_deploy_gate(&path, 1_000_000_000, 60_000);
        assert_eq!(d.status, GateStatus::RiskOff);
        assert_eq!(d.reason, GateReason::ShadowUnobserved);
        assert_eq!(d.newest_event_ms, None);
    }

    #[test]
    fn gate_returns_pass_when_recent_ok_event_exists() {
        let now = 1_000_000_000;
        let rows = vec![fidelity_event_row(now - 1_000, "ok")];
        let path = write_fidelity_journal(&rows);
        let d = evaluate_live_deploy_gate(&path, now, 60_000);
        let _ = std::fs::remove_file(&path);
        assert_eq!(d.status, GateStatus::Pass);
        assert_eq!(d.reason, GateReason::Pass);
        assert_eq!(d.ok_count, 1);
    }

    #[test]
    fn gate_returns_riskoff_shadow_fail_on_any_recent_fail() {
        let now = 1_000_000_000;
        let rows = vec![
            fidelity_event_row(now - 5_000, "ok"),
            fidelity_event_row(now - 1_000, "fail"),
        ];
        let path = write_fidelity_journal(&rows);
        let d = evaluate_live_deploy_gate(&path, now, 60_000);
        let _ = std::fs::remove_file(&path);
        assert_eq!(d.status, GateStatus::RiskOff);
        assert_eq!(d.reason, GateReason::ShadowFail);
        assert_eq!(d.ok_count, 1);
        assert_eq!(d.fail_count, 1);
    }

    #[test]
    fn gate_ignores_events_outside_window() {
        let now = 1_000_000_000;
        let rows = vec![
            fidelity_event_row(now - 200_000, "ok"), // outside 60s window
        ];
        let path = write_fidelity_journal(&rows);
        let d = evaluate_live_deploy_gate(&path, now, 60_000);
        let _ = std::fs::remove_file(&path);
        assert_eq!(d.status, GateStatus::RiskOff);
        assert_eq!(d.reason, GateReason::ShadowUnobserved);
        assert_eq!(d.ok_count, 0);
    }

    #[test]
    fn gate_passes_on_warn_only_recent_events() {
        let now = 1_000_000_000;
        let rows = vec![fidelity_event_row(now - 1_000, "warn")];
        let path = write_fidelity_journal(&rows);
        let d = evaluate_live_deploy_gate(&path, now, 60_000);
        let _ = std::fs::remove_file(&path);
        assert_eq!(d.status, GateStatus::Pass);
        assert_eq!(d.warn_count, 1);
    }

    #[test]
    fn parse_activity_keeps_trade_rows_and_drops_lifecycle_ops() {
        let json = serde_json::json!([
            {"type": "TRADE", "slug": "btc-updown-5m-1776961500", "price": 0.49, "size": 40.0, "usdcSize": 19.6, "timestamp": 1_700_000_000u64},
            {"type": "REDEEM", "slug": "btc-updown-5m-1776961500", "price": 1.0, "size": 100.0, "timestamp": 1_700_000_100u64},
            {"type": "MERGE", "slug": "eth-updown-5m-1776961500", "size": 50.0, "timestamp": 1_700_000_200u64},
            {"type": "TRADE", "slug": "eth-updown-15m-1776960900", "price": 0.51, "size": 20.0, "usdcSize": 10.2, "timestamp": 1_700_000_300u64}
        ]);
        let out = parse_bonereaper_activity(&json);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0.market_family, "btc-updown-5m");
        assert!((out[0].0.notional_usd - 19.6).abs() < 1e-9);
        assert_eq!(out[0].1, 1_700_000_000_000);
        assert_eq!(out[1].0.market_family, "eth-updown-15m");
    }

    #[test]
    fn parse_activity_handles_empty_or_malformed_response() {
        let empty = serde_json::json!([]);
        assert!(parse_bonereaper_activity(&empty).is_empty());

        let object = serde_json::json!({"error": "not an array"});
        assert!(parse_bonereaper_activity(&object).is_empty());
    }

    #[test]
    fn truth_freshness_within_window_is_fresh() {
        let now = 1_700_000_300_000;
        let newest = 1_700_000_000_000; // 300s = 5min ago, EXACTLY at the boundary
        assert_eq!(
            truth_freshness(Some(newest), now, 5 * 60 * 1_000),
            TruthFreshness::Fresh
        );
    }

    #[test]
    fn truth_freshness_beyond_window_is_stale() {
        let now = 1_700_000_301_000;
        let newest = 1_700_000_000_000; // 301s ago, just past the boundary
        assert_eq!(
            truth_freshness(Some(newest), now, 5 * 60 * 1_000),
            TruthFreshness::Stale
        );
    }

    #[test]
    fn truth_freshness_no_data_is_stale() {
        assert_eq!(
            truth_freshness(None, 1_700_000_000_000, 5 * 60 * 1_000),
            TruthFreshness::Stale
        );
    }

    #[test]
    fn summarise_includes_our_live_fills_as_microstructure_truth_source() {
        // When our own wallet has live fills (Approach C deployed), the
        // shadow-vs-our-live MAPE is the microstructure-validating signal
        // since both sides have full place-time + queue + fill timing.
        // bonereaper-vs-shadow stays as the count-level cross-check.
        let shadow = vec![ShadowFill {
            market_family: "btc-updown-5m".to_string(),
            notional_usd: 20.0,
            rebate_usd: 0.04,
        }];
        let our_live = vec![OurLiveFill {
            market_family: "btc-updown-5m".to_string(),
            notional_usd: 18.0,
            rebate_usd: 0.04,
        }];
        let bonereaper = vec![BonereaperFill {
            market_family: "btc-updown-5m".to_string(),
            notional_usd: 24.0,
            rebate_usd: 0.05,
        }];
        let events = summarise_window_with_our_live(
            &shadow,
            &our_live,
            &bonereaper,
            0,
            60_000,
            60_500,
        );
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.shadow_fill_count, 1);
        assert_eq!(e.our_live_fill_count, 1);
        assert_eq!(e.bonereaper_fill_count, 1);
        // Verdict prefers our_live as primary truth when present.
        // |1 - 1| / 1 = 0 -> Ok
        assert_eq!(e.verdict, FidelityVerdict::Ok);
    }
}
