//! Fair-value model for Polymarket BTC up/down binary digital options.
//!
//! # Background
//!
//! Polymarket "Bitcoin Up or Down" markets resolve based on whether BTC's
//! spot price at bar-end is above or below a strike (`price_to_beat`,
//! typically the bar-start BTC price). Each market has two legs (Up, Down)
//! whose prices sum to ~$1 by no-arbitrage. The market's bid-mid encodes
//! the collective probability estimate that the leg wins.
//!
//! Our existing strategy uses book-mid as `fair_value` everywhere. That's
//! fine when the market is well-priced, but it locks `hold_ev` and
//! `sell_ev` mechanically together (both compute against the same
//! reference) — meaning the SELL fallback can never trigger on a losing
//! position because `hold_ev ≈ sell_ev` always.
//!
//! This module computes an INDEPENDENT fair-value estimate using the
//! Black-Scholes binary digital formula adapted for short horizons:
//!
//!   P(BTC_T > K | S_t) = Φ((ln(S_t / K)) / (σ × sqrt(T_remaining / T_total)))
//!
//! Where σ is realized vol observed from the BTC spot feed. The output
//! is the model's probability that the Up leg wins, in [0, 1]. The Down
//! leg's probability is `1 - P(Up)`.
//!
//! # When the model differs from book-mid
//!
//! - Polymarket bid-ask spread (the model gives us a "true" fair number,
//!   not market-mid which is biased toward whichever side is currently
//!   liquid)
//! - Late-bar microstructure (book starts to converge to 0/1, but model
//!   reflects the actual physics of "is BTC actually above strike?")
//! - Adverse-selection-induced mispricing (informed flow has pushed the
//!   book away from physical fair)
//!
//! # Calibration / future work
//!
//! BSM assumes log-normal returns. BTC at 5-min horizon has:
//!   - Excess kurtosis (fat tails)
//!   - Time-varying vol (GARCH effects)
//!   - Microstructure noise dominates last ~30s of bar
//!
//! Phase 1 (this module): pure BSM-binary, shadow-logged alongside
//!   book-mid, no decision impact. Validate divergence patterns.
//!
//! Phase 2 (later): empirical calibration from collected (S/K,
//!   T_remaining) → realized-outcome tuples; adjust model where it's
//!   systematically off.
//!
//! Phase 3 (later): phase into individual gates, starting with
//!   decide_stranded_exposure where the EV gate is most directly limited
//!   by book-mid coupling.

/// Bar duration assumption for normalizing realized_vol_5m_bps. Polymarket
/// "BTC Up or Down 5m" markets are 300s. For 15m markets this should be
/// 900, etc. — when we add multi-horizon, this becomes per-strategy
/// config.
const BAR_TOTAL_DURATION_SECONDS: f64 = 300.0;

/// When time-remaining is below this floor, treat the resolution as
/// effectively decided (sigma_remaining → 0 produces numerically unstable
/// CDF). Falls back to step function: P(Up) = 1 if spot > strike else 0.
const TIME_FLOOR_SECONDS: f64 = 1.0;

/// Output of the fair-value model. `p_up` ∈ [0, 1] is the model's
/// probability that the Up leg wins. Carries metadata so callers can
/// log the inputs alongside the output for post-hoc analysis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FairValueEstimate {
    pub p_up: f64,
    pub p_down: f64,
    pub log_moneyness: f64,
    pub sigma_remaining: f64,
    pub time_remaining_s: f64,
    pub model: FairValueModel,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FairValueModel {
    /// Black-Scholes binary digital — closed-form, current implementation.
    BsmBinary,
    /// Spot known to be on one side of strike with insufficient remaining
    /// time for reversal — degenerate "decided" case.
    StepFunctionDecided,
    /// One or more inputs missing or invalid. Output `p_up = 0.5` is a
    /// safe default but caller should NOT rely on it for decisions —
    /// treat as "no signal." Reason field tells callers exactly which
    /// input failed validation so the issue can be fixed at source.
    NoSignal(NoSignalReason),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NoSignalReason {
    /// Spot price is NaN / non-finite / non-positive.
    SpotInvalid,
    /// Strike (price_to_beat) is NaN / non-finite / non-positive.
    StrikeInvalid,
    /// Time remaining is NaN / non-finite / negative.
    TimeRemainingInvalid,
    /// Realized vol is NaN / non-finite / non-positive (e.g. spot WS
    /// stale → vol can't be computed → 100% None readings observed
    /// 2026-04-29).
    VolInvalid,
}

impl std::fmt::Display for NoSignalReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoSignalReason::SpotInvalid => f.write_str("spot_invalid"),
            NoSignalReason::StrikeInvalid => f.write_str("strike_invalid"),
            NoSignalReason::TimeRemainingInvalid => f.write_str("time_remaining_invalid"),
            NoSignalReason::VolInvalid => f.write_str("vol_invalid"),
        }
    }
}

/// Compute the model's probability that the Up leg of a binary digital
/// option wins, given current spot, strike, time remaining, and observed
/// realized volatility.
///
/// Inputs:
/// - `spot_price`: current BTC spot price (positive finite).
/// - `strike`: the bar's `price_to_beat` (bar-start BTC price).
/// - `time_remaining_s`: seconds until resolution.
/// - `realized_vol_bps_over_bar`: realized volatility (std-dev of log
///    returns) over the bar window in basis points. Must already be
///    scaled to the bar duration (NOT annualized).
///
/// Returns a `FairValueEstimate` with `p_up` in [0, 1]. On degenerate or
/// missing inputs, returns a `NoSignal` variant with `p_up = 0.5`.
pub fn estimate_fair_value(
    spot_price: f64,
    strike: f64,
    time_remaining_s: f64,
    realized_vol_bps_over_bar: f64,
) -> FairValueEstimate {
    let no_signal = |reason: NoSignalReason| FairValueEstimate {
        p_up: 0.5,
        p_down: 0.5,
        log_moneyness: f64::NAN,
        sigma_remaining: f64::NAN,
        time_remaining_s,
        model: FairValueModel::NoSignal(reason),
    };

    if !spot_price.is_finite() || spot_price <= 0.0 {
        return no_signal(NoSignalReason::SpotInvalid);
    }
    if !strike.is_finite() || strike <= 0.0 {
        return no_signal(NoSignalReason::StrikeInvalid);
    }
    if !time_remaining_s.is_finite() || time_remaining_s < 0.0 {
        return no_signal(NoSignalReason::TimeRemainingInvalid);
    }
    if !realized_vol_bps_over_bar.is_finite() || realized_vol_bps_over_bar <= 0.0 {
        return no_signal(NoSignalReason::VolInvalid);
    }

    let log_moneyness = (spot_price / strike).ln();

    // If we're past resolution OR vol is so low the outcome is
    // effectively determined, use the step function.
    if time_remaining_s < TIME_FLOOR_SECONDS {
        let p_up = if log_moneyness > 0.0 { 1.0 } else { 0.0 };
        return FairValueEstimate {
            p_up,
            p_down: 1.0 - p_up,
            log_moneyness,
            sigma_remaining: 0.0,
            time_remaining_s,
            model: FairValueModel::StepFunctionDecided,
        };
    }

    let sigma_bar = realized_vol_bps_over_bar / 10_000.0;
    let time_fraction = (time_remaining_s / BAR_TOTAL_DURATION_SECONDS).clamp(0.0, 1.0);
    let sigma_remaining = sigma_bar * time_fraction.sqrt();

    if sigma_remaining < 1e-12 {
        let p_up = if log_moneyness > 0.0 { 1.0 } else { 0.0 };
        return FairValueEstimate {
            p_up,
            p_down: 1.0 - p_up,
            log_moneyness,
            sigma_remaining,
            time_remaining_s,
            model: FairValueModel::StepFunctionDecided,
        };
    }

    let d = log_moneyness / sigma_remaining;
    let p_up = standard_normal_cdf(d).clamp(0.0, 1.0);

    FairValueEstimate {
        p_up,
        p_down: (1.0 - p_up).clamp(0.0, 1.0),
        log_moneyness,
        sigma_remaining,
        time_remaining_s,
        model: FairValueModel::BsmBinary,
    }
}

/// Standard normal cumulative distribution function via erf.
/// Φ(x) = 0.5 × (1 + erf(x / sqrt(2)))
pub fn standard_normal_cdf(x: f64) -> f64 {
    if !x.is_finite() {
        if x.is_nan() {
            return f64::NAN;
        }
        return if x > 0.0 { 1.0 } else { 0.0 };
    }
    0.5 * (1.0 + erf_approx(x * std::f64::consts::FRAC_1_SQRT_2))
}

/// Abramowitz & Stegun 7.1.26 approximation of erf. Maximum absolute
/// error ≈ 1.5e-7, sufficient for our use case (binary option pricing
/// to 5+ significant digits). Faster than libm and avoids dependencies.
fn erf_approx(x: f64) -> f64 {
    const A1: f64 = 0.254_829_592;
    const A2: f64 = -0.284_496_736;
    const A3: f64 = 1.421_413_741;
    const A4: f64 = -1.453_152_027;
    const A5: f64 = 1.061_405_429;
    const P: f64 = 0.327_591_1;

    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let abs_x = x.abs();
    let t = 1.0 / (1.0 + P * abs_x);
    let y = 1.0 - (((((A5 * t + A4) * t) + A3) * t + A2) * t + A1) * t * (-abs_x * abs_x).exp();
    sign * y
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn fair_value_returns_no_signal_with_specific_reason() {
        let r = estimate_fair_value(f64::NAN, 100.0, 60.0, 5.0);
        assert_eq!(r.model, FairValueModel::NoSignal(NoSignalReason::SpotInvalid));
        assert_eq!(r.p_up, 0.5);

        let r = estimate_fair_value(100.0, 0.0, 60.0, 5.0);
        assert_eq!(
            r.model,
            FairValueModel::NoSignal(NoSignalReason::StrikeInvalid)
        );

        let r = estimate_fair_value(100.0, 100.0, -1.0, 5.0);
        assert_eq!(
            r.model,
            FairValueModel::NoSignal(NoSignalReason::TimeRemainingInvalid)
        );

        let r = estimate_fair_value(100.0, 100.0, 60.0, 0.0);
        assert_eq!(r.model, FairValueModel::NoSignal(NoSignalReason::VolInvalid));

        let r = estimate_fair_value(100.0, 100.0, 60.0, f64::NAN);
        assert_eq!(r.model, FairValueModel::NoSignal(NoSignalReason::VolInvalid));

        let r = estimate_fair_value(-100.0, 100.0, 60.0, 5.0);
        assert_eq!(r.model, FairValueModel::NoSignal(NoSignalReason::SpotInvalid));
    }

    #[test]
    fn fair_value_at_strike_and_finite_vol_returns_half() {
        // Spot = strike → log_moneyness = 0 → d = 0 → Φ(0) = 0.5
        let r = estimate_fair_value(100.0, 100.0, 60.0, 5.0);
        assert_eq!(r.model, FairValueModel::BsmBinary);
        assert!(approx_eq(r.p_up, 0.5, 1e-9), "p_up={}", r.p_up);
        assert!(approx_eq(r.p_down, 0.5, 1e-9));
    }

    #[test]
    fn fair_value_spot_far_above_strike_approaches_one() {
        // 1% spot premium vs strike with low vol → P(Up) very high
        let r = estimate_fair_value(101.0, 100.0, 60.0, 5.0);
        assert!(r.p_up > 0.99, "expected p_up > 0.99, got {}", r.p_up);
    }

    #[test]
    fn fair_value_spot_far_below_strike_approaches_zero() {
        let r = estimate_fair_value(99.0, 100.0, 60.0, 5.0);
        assert!(r.p_up < 0.01, "expected p_up < 0.01, got {}", r.p_up);
    }

    #[test]
    fn fair_value_with_zero_time_remaining_uses_step_function() {
        let r = estimate_fair_value(100.5, 100.0, 0.5, 5.0);
        assert_eq!(r.model, FairValueModel::StepFunctionDecided);
        assert_eq!(r.p_up, 1.0);

        let r = estimate_fair_value(99.5, 100.0, 0.5, 5.0);
        assert_eq!(r.model, FairValueModel::StepFunctionDecided);
        assert_eq!(r.p_up, 0.0);
    }

    #[test]
    fn fair_value_with_high_vol_and_short_time_softens_extremes() {
        // 5 bps spot deviation, 60s remaining, but high vol → should NOT
        // be near 0/1 because vol still allows reversal
        let low_vol = estimate_fair_value(100.05, 100.0, 60.0, 1.0);
        let high_vol = estimate_fair_value(100.05, 100.0, 60.0, 50.0);
        assert!(
            high_vol.p_up < low_vol.p_up,
            "higher vol should soften the extremes; low_vol p_up={} high_vol p_up={}",
            low_vol.p_up,
            high_vol.p_up
        );
    }

    #[test]
    fn fair_value_p_up_plus_p_down_sums_to_one() {
        for spot_premium_bps in [-100, -10, 0, 10, 100] {
            for time_s in [10.0, 60.0, 120.0, 290.0] {
                for vol_bps in [1.0, 5.0, 50.0] {
                    let spot = 100.0 * (1.0 + spot_premium_bps as f64 / 10_000.0);
                    let r = estimate_fair_value(spot, 100.0, time_s, vol_bps);
                    let sum = r.p_up + r.p_down;
                    assert!(
                        approx_eq(sum, 1.0, 1e-9),
                        "sum should be 1.0, got {} for spot={} time={} vol={}",
                        sum,
                        spot,
                        time_s,
                        vol_bps
                    );
                }
            }
        }
    }

    #[test]
    fn fair_value_more_time_remaining_pulls_extremes_toward_half() {
        // With more time remaining, σ_remaining is larger → CDF is less
        // peaked → probability moves closer to 0.5 from extremes.
        // Calibration: spot premium 1 bp + vol 5 bps over bar →
        //   near (30s):  σ = 5 × √(30/300) = 1.58 bps; d = 1/1.58 ≈ 0.63
        //   far  (290s): σ = 5 × √(290/300) = 4.92 bps; d = 1/4.92 ≈ 0.20
        // Both produce non-saturated CDF values that differ meaningfully.
        let near_resolution = estimate_fair_value(100.01, 100.0, 30.0, 5.0);
        let far_from_resolution = estimate_fair_value(100.01, 100.0, 290.0, 5.0);
        assert!(
            near_resolution.p_up > far_from_resolution.p_up,
            "near resolution should have higher confidence in Up: near={} far={}",
            near_resolution.p_up,
            far_from_resolution.p_up
        );
        assert!(
            far_from_resolution.p_up > 0.5,
            "spot above strike should still favor Up even far from resolution: {}",
            far_from_resolution.p_up
        );
        assert!(
            near_resolution.p_up < 0.95,
            "near-resolution should NOT yet saturate at this spot/vol calibration: {}",
            near_resolution.p_up
        );
    }

    #[test]
    fn standard_normal_cdf_matches_known_values() {
        assert!(approx_eq(standard_normal_cdf(0.0), 0.5, 1e-7));
        assert!(approx_eq(standard_normal_cdf(1.0), 0.8413447, 1e-5));
        assert!(approx_eq(standard_normal_cdf(-1.0), 0.1586553, 1e-5));
        assert!(approx_eq(standard_normal_cdf(1.96), 0.9750021, 1e-5));
        assert!(approx_eq(standard_normal_cdf(-1.96), 0.0249979, 1e-5));
    }

    #[test]
    fn standard_normal_cdf_handles_extreme_inputs() {
        assert_eq!(standard_normal_cdf(f64::INFINITY), 1.0);
        assert_eq!(standard_normal_cdf(f64::NEG_INFINITY), 0.0);
        assert!(standard_normal_cdf(f64::NAN).is_nan());
        assert!(standard_normal_cdf(100.0) > 0.9999);
        assert!(standard_normal_cdf(-100.0) < 0.0001);
    }
}
