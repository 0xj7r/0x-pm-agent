//! Fractional Kelly sizing for binary digital options.
//!
//! For a contract paying $1 on win / $0 on loss at price `ask`, the full
//! Kelly fraction of bankroll is:
//!
//!   f* = (p - ask) / (1 - ask)
//!
//! where `p` is the model probability of winning. This is the unique
//! fraction that maximizes log-growth of bankroll under known p and ask
//! (Kelly 1956; MacLean/Thorp/Ziemba 2011 for the parameter-uncertainty
//! treatment that motivates fractional Kelly).
//!
//! In practice we apply a fractional multiplier (0.25-0.50x) to manage
//! the bias-variance tradeoff: model p has estimation error, and full
//! Kelly is optimal only under known parameters. Fractional Kelly loses
//! a small amount of asymptotic growth for a large reduction in variance
//! and drawdown.
//!
//! Replaces the previous 5-multiplier compound (regime × confidence ×
//! timing × conviction × price_taper) that compounded toward 0.20-0.30
//! of configured clip size even when the model was high-conviction.

#[derive(Clone, Copy, Debug)]
pub struct KellyClipParams {
    /// Model's probability the favorite leg wins, in [0, 1].
    pub model_favorite: f64,
    /// Current ask price for the favorite leg, in [0, 1].
    pub favorite_ask: f64,
    /// Probability the market path reverses against us before
    /// resolution. Shrinks the effective probability toward 0.5.
    pub path_reversal_risk: f64,
    /// Per-market budget this lane is allowed to spend (typically
    /// `adjusted_max_load_usd` after regime/policy scaling).
    pub lane_bankroll_usd: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KellyClipConfig {
    /// Fractional Kelly multiplier. 0.25-0.40 is standard practice for
    /// managing parameter uncertainty (MacLean/Thorp/Ziemba 2011).
    pub fractional: f64,
    /// Hard cap on per-clip fraction of lane bankroll. Defends against
    /// pathological cases where edge / (1 - ask) blows up near ask = 1.
    pub max_clip_fraction: f64,
    /// Minimum reversal-adjusted edge (p_adj - ask) required to size > 0.
    /// Below this we skip the bet entirely.
    pub min_edge: f64,
}

impl Default for KellyClipConfig {
    fn default() -> Self {
        Self {
            fractional: 0.30,
            max_clip_fraction: 0.30,
            min_edge: 0.02,
        }
    }
}

/// Compute the per-clip USD amount using fractional Kelly with a
/// Bayesian shrink for path-reversal risk.
///
/// Returns 0.0 when there is no edge, inputs are degenerate, or bankroll
/// is non-positive. Caller should clamp to `min_order_usd` if a floor is
/// required for venue minimums.
pub fn kelly_clip_usd(params: KellyClipParams, cfg: &KellyClipConfig) -> f64 {
    let p = params.model_favorite.clamp(0.0, 1.0);
    let ask = params.favorite_ask.clamp(0.0, 1.0);
    let reversal = params.path_reversal_risk.clamp(0.0, 1.0);
    let bankroll = params.lane_bankroll_usd.max(0.0);

    if bankroll <= 0.0 {
        return 0.0;
    }

    // Bayesian shrink: with probability `reversal`, treat the directional
    // model as uninformative (prior 0.5). Equivalent to a Bernoulli mixture
    // of model and uniform priors.
    let p_adj = 0.5 + (p - 0.5) * (1.0 - reversal);

    let edge = p_adj - ask;
    if edge < cfg.min_edge {
        return 0.0;
    }

    // Pure Kelly for binary digital paying $1 / $0 at price `ask`.
    let q = (1.0 - ask).max(1e-6);
    let f_kelly = edge / q;

    // Fractional Kelly, then hard cap.
    let f = (cfg.fractional * f_kelly).min(cfg.max_clip_fraction);

    bankroll * f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> KellyClipConfig {
        KellyClipConfig {
            fractional: 0.30,
            max_clip_fraction: 0.30,
            min_edge: 0.02,
        }
    }

    fn params(p: f64, ask: f64, reversal: f64, bankroll: f64) -> KellyClipParams {
        KellyClipParams {
            model_favorite: p,
            favorite_ask: ask,
            path_reversal_risk: reversal,
            lane_bankroll_usd: bankroll,
        }
    }

    #[test]
    fn zero_edge_returns_zero() {
        assert_eq!(kelly_clip_usd(params(0.85, 0.85, 0.0, 450.0), &cfg()), 0.0);
        // edge 0.01 < min_edge 0.02
        assert_eq!(kelly_clip_usd(params(0.86, 0.85, 0.0, 450.0), &cfg()), 0.0);
    }

    #[test]
    fn moderate_edge_at_0_85_ask_sizes_to_expected() {
        // edge 0.05 at ask 0.85: f_kelly = 0.05 / 0.15 = 0.333
        // fractional 0.30 -> f = 0.10, clip = 450 * 0.10 = $45
        let clip = kelly_clip_usd(params(0.90, 0.85, 0.0, 450.0), &cfg());
        assert!((clip - 45.0).abs() < 0.5, "expected ~$45, got ${clip:.2}");
    }

    #[test]
    fn high_edge_hits_max_clip_cap() {
        // edge 0.15 at ask 0.85: f_kelly = 1.0, fractional 0.30 -> f = 0.30
        // but capped at max_clip_fraction = 0.30 -> $135
        let clip = kelly_clip_usd(params(1.00, 0.85, 0.0, 450.0), &cfg());
        assert!(
            (clip - 135.0).abs() < 0.5,
            "expected $135 cap, got ${clip:.2}"
        );
    }

    #[test]
    fn high_reversal_risk_shrinks_or_eliminates_clip() {
        // No reversal: p=0.95, edge=0.10, kelly=0.667, f=0.20, clip=$90
        let no_rev = kelly_clip_usd(params(0.95, 0.85, 0.0, 450.0), &cfg());
        // With reversal=0.5: p_adj = 0.5 + 0.45*0.5 = 0.725, edge = -0.125 < 0 -> 0
        let high_rev = kelly_clip_usd(params(0.95, 0.85, 0.5, 450.0), &cfg());
        assert!(no_rev > 80.0);
        assert_eq!(high_rev, 0.0);
    }

    #[test]
    fn high_ask_with_small_edge_still_sizes() {
        // edge 0.03 at ask 0.95: f_kelly = 0.03 / 0.05 = 0.60
        // fractional 0.30 -> f = 0.18, clip = 450 * 0.18 = $81
        let clip = kelly_clip_usd(params(0.98, 0.95, 0.0, 450.0), &cfg());
        assert!((clip - 81.0).abs() < 0.5, "expected ~$81, got ${clip:.2}");
    }

    #[test]
    fn invalid_bankroll_returns_zero() {
        assert_eq!(kelly_clip_usd(params(0.95, 0.85, 0.0, 0.0), &cfg()), 0.0);
        assert_eq!(kelly_clip_usd(params(0.95, 0.85, 0.0, -100.0), &cfg()), 0.0);
    }

    #[test]
    fn out_of_range_inputs_are_clamped_safely() {
        // p > 1.0 clamps to 1.0, ask > 1.0 clamps to 1.0 -> edge 0 -> skip
        assert_eq!(kelly_clip_usd(params(1.5, 1.5, 0.0, 450.0), &cfg()), 0.0);
        // p < 0 clamps to 0 -> negative edge -> skip
        assert_eq!(kelly_clip_usd(params(-0.5, 0.5, 0.0, 450.0), &cfg()), 0.0);
    }

    #[test]
    fn reversal_risk_clamped_to_unit_interval() {
        // reversal > 1.0 clamps to 1.0 -> p_adj = 0.5 -> edge = -ask -> skip
        assert_eq!(kelly_clip_usd(params(0.95, 0.85, 2.0, 450.0), &cfg()), 0.0);
        // reversal < 0 clamps to 0 -> no shrink
        let no_rev = kelly_clip_usd(params(0.95, 0.85, 0.0, 450.0), &cfg());
        let neg_rev = kelly_clip_usd(params(0.95, 0.85, -0.5, 450.0), &cfg());
        assert!((no_rev - neg_rev).abs() < 1e-9);
    }

    #[test]
    fn fractional_kelly_scales_linearly_below_cap() {
        let p_kelly = |fractional| KellyClipConfig {
            fractional,
            max_clip_fraction: 1.0,
            min_edge: 0.0,
        };
        // edge 0.05 at ask 0.85: f_kelly = 0.333
        let half_kelly = kelly_clip_usd(params(0.90, 0.85, 0.0, 1000.0), &p_kelly(0.5));
        let quarter_kelly = kelly_clip_usd(params(0.90, 0.85, 0.0, 1000.0), &p_kelly(0.25));
        // half kelly should be exactly 2x quarter kelly
        assert!((half_kelly / quarter_kelly - 2.0).abs() < 1e-9);
    }

    #[test]
    fn bankroll_scales_linearly() {
        let small = kelly_clip_usd(params(0.90, 0.85, 0.0, 100.0), &cfg());
        let large = kelly_clip_usd(params(0.90, 0.85, 0.0, 1000.0), &cfg());
        assert!((large / small - 10.0).abs() < 1e-9);
    }
}
