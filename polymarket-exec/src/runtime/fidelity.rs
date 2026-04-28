//! Fidelity scoring: compares our shadow's predicted fills to bonereaper's
//! actual on-chain fills (via /activity API) and emits a per-minute,
//! per-market-family verdict that gates live deployment.
//!
//! Phase 7 will add the polling logic. This file currently defines the
//! event/verdict types so journal.rs can journal them and the live process
//! can read them at the deploy gate.

use serde::Serialize;

use crate::core::types::EpochMillis;

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
}
