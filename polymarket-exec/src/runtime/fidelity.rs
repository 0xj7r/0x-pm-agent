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
