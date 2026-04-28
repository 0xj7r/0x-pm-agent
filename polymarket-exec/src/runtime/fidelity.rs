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

pub fn summarise_window(
    shadow_fills: &[ShadowFill],
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
    for f in bonereaper_fills {
        let agg = by_family.entry(f.market_family.clone()).or_default();
        agg.bonereaper_count += 1;
        agg.bonereaper_notional += f.notional_usd;
        agg.bonereaper_rebate += f.rebate_usd;
    }
    by_family
        .into_iter()
        .map(|(family, agg)| {
            let mape = if agg.bonereaper_count == 0 {
                if agg.shadow_count == 0 {
                    0.0
                } else {
                    f64::INFINITY
                }
            } else {
                ((agg.shadow_count as f64) - (agg.bonereaper_count as f64)).abs()
                    / (agg.bonereaper_count as f64)
            };
            FidelityEvent {
                observed_at_ms,
                market_family: family,
                window_start_ms,
                window_end_ms,
                shadow_fill_count: agg.shadow_count,
                shadow_fill_notional_usd: agg.shadow_notional,
                shadow_rebate_usd: agg.shadow_rebate,
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
    pub bonereaper_fill_count: u32,
    pub bonereaper_fill_notional_usd: f64,
    pub bonereaper_rebate_usd: f64,
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
}
