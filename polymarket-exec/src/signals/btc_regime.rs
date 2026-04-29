//! BTC micro-regime calculations from recent trade stream samples.

#[derive(Debug, Clone, Default)]
pub struct BtcRegimeSnapshot {
    pub last_price: Option<f64>,
    pub realized_vol_5m_bps: Option<f64>,
    pub realized_vol_15m_bps: Option<f64>,
    pub trade_count_5m: u64,
    pub trade_count_15m: u64,
    pub return_30s_bps: Option<f64>,
    pub return_60s_bps: Option<f64>,
    /// Return over the last 120s (2 minutes). Used for trend-persistence
    /// detection: when |return_180s_bps| > threshold AND sign matches
    /// return_120s_bps, the trend is persistent (not a single-tick spike).
    pub return_120s_bps: Option<f64>,
    /// Return over the last 180s (3 minutes). Trend-persistence horizon.
    /// Convex accumulation uses this to skip cheap-leg bids that are
    /// against a sustained directional move.
    pub return_180s_bps: Option<f64>,
    pub observed_at_ms: u64,
}

/// Composed-regime classifier for the BTC underlying. Built from
/// `realized_vol_5m_bps` (noise floor) and `|return_180s_bps|` (trend
/// strength). Single-signal gating misses the "directional but smooth"
/// pattern where vol is low yet net move is large — exactly the regime
/// where late-bar-core's mispricing thesis is strongest.
///
/// The four regimes form an orthogonal split on (vol-level × trend-strength):
/// - `Flat`: low noise, no trend. Skip everything; nothing to trade.
/// - `Whipsaw`: high noise, no trend. Mean-reverting; paired-bid +EV.
/// - `DirectionalSmooth`: low noise, strong trend. Resolution likely
///   determined; late-bar-core's mispricing thesis strongest here.
/// - `TrendingVolatile`: high noise + strong trend. Mixed; both paths
///   cautious.
///
/// Discriminator: `|return_180s_bps| / max(realized_vol_5m_bps, floor)`.
/// Ratio >= TREND_NOISE_RATIO means trend dominates noise (directional
/// regimes); below means noise dominates (no clear direction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtcRegime {
    Flat,
    Whipsaw,
    DirectionalSmooth,
    TrendingVolatile,
}

impl BtcRegime {
    /// Threshold separating "low noise" from "high noise" regimes.
    /// Calibrated against post-sqrt(N)-fix realized_vol values:
    /// today's directional-smooth tape is ~2-3 bps; active is 10+.
    pub const VOL_LOW_HIGH_BPS: f64 = 8.0;
    /// Threshold on `|return_180s| / vol` discriminating "trend dominates
    /// noise" from "noise dominates." 5 means the 3min net move is 5×
    /// the 5min std dev — a strong directional signal.
    pub const TREND_NOISE_RATIO: f64 = 5.0;
    /// Floor on vol used in the ratio denominator to avoid div-by-zero
    /// or runaway ratios on truly flat tape (vol could legitimately be
    /// near 0 during news lulls).
    pub const VOL_FLOOR_BPS: f64 = 0.5;
}

impl std::fmt::Display for BtcRegime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BtcRegime::Flat => f.write_str("flat"),
            BtcRegime::Whipsaw => f.write_str("whipsaw"),
            BtcRegime::DirectionalSmooth => f.write_str("directional_smooth"),
            BtcRegime::TrendingVolatile => f.write_str("trending_volatile"),
        }
    }
}

impl BtcRegimeSnapshot {
    /// Classify the current regime. Returns `None` if either input is
    /// unavailable (warmup). Callers should treat None as "regime
    /// unknown" and fall back to single-signal gating.
    pub fn regime(&self) -> Option<BtcRegime> {
        let vol = self
            .realized_vol_5m_bps
            .filter(|v| v.is_finite() && *v >= 0.0)?;
        let trend = self
            .return_180s_bps
            .filter(|r| r.is_finite())
            .map(f64::abs)?;
        let vol_eff = vol.max(BtcRegime::VOL_FLOOR_BPS);
        let trend_dominates = (trend / vol_eff) >= BtcRegime::TREND_NOISE_RATIO;
        let high_vol = vol >= BtcRegime::VOL_LOW_HIGH_BPS;
        Some(match (high_vol, trend_dominates) {
            (false, false) => BtcRegime::Flat,
            (false, true) => BtcRegime::DirectionalSmooth,
            (true, false) => BtcRegime::Whipsaw,
            (true, true) => BtcRegime::TrendingVolatile,
        })
    }

    /// True when the current regime is one where late-bar-core has
    /// positive expected value: clear direction, market likely
    /// underpricing the favored leg as resolution approaches.
    pub fn favors_late_bar_core(&self) -> bool {
        matches!(
            self.regime(),
            Some(BtcRegime::DirectionalSmooth | BtcRegime::TrendingVolatile)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(vol: Option<f64>, ret180: Option<f64>) -> BtcRegimeSnapshot {
        BtcRegimeSnapshot {
            realized_vol_5m_bps: vol,
            return_180s_bps: ret180,
            ..Default::default()
        }
    }

    #[test]
    fn flat_regime_low_vol_no_trend() {
        // vol 2 bps, trend 5 bps (ratio 2.5, below 5 threshold)
        let s = snap(Some(2.0), Some(5.0));
        assert_eq!(s.regime(), Some(BtcRegime::Flat));
        assert!(!s.favors_late_bar_core());
    }

    #[test]
    fn directional_smooth_regime_low_vol_strong_trend() {
        // Today's regime: vol ~2 bps, but BTC moved 30+ bps in 3min
        // (ratio = 15, well above threshold 5)
        let s = snap(Some(2.0), Some(30.0));
        assert_eq!(s.regime(), Some(BtcRegime::DirectionalSmooth));
        assert!(s.favors_late_bar_core());
    }

    #[test]
    fn whipsaw_regime_high_vol_no_trend() {
        // vol 15 bps but net move only 10 bps (ratio < 5)
        let s = snap(Some(15.0), Some(10.0));
        assert_eq!(s.regime(), Some(BtcRegime::Whipsaw));
        assert!(!s.favors_late_bar_core());
    }

    #[test]
    fn trending_volatile_regime_high_vol_strong_trend() {
        // Hurricane: vol 20 bps + 150 bps net move
        let s = snap(Some(20.0), Some(150.0));
        assert_eq!(s.regime(), Some(BtcRegime::TrendingVolatile));
        assert!(s.favors_late_bar_core());
    }

    #[test]
    fn negative_return_uses_absolute_value() {
        // Strong DOWN trend treated same as strong UP
        let s = snap(Some(2.0), Some(-30.0));
        assert_eq!(s.regime(), Some(BtcRegime::DirectionalSmooth));
    }

    #[test]
    fn missing_vol_returns_none() {
        let s = snap(None, Some(30.0));
        assert_eq!(s.regime(), None);
        assert!(!s.favors_late_bar_core());
    }

    #[test]
    fn missing_return_returns_none() {
        let s = snap(Some(5.0), None);
        assert_eq!(s.regime(), None);
    }

    #[test]
    fn vol_floor_prevents_runaway_ratio_on_truly_flat_tape() {
        // vol 0.01 bps (essentially zero) → vol_floor 0.5 used
        // trend 1 bps → ratio 2 → still classifies as Flat (correct)
        let s = snap(Some(0.01), Some(1.0));
        assert_eq!(s.regime(), Some(BtcRegime::Flat));
    }

    #[test]
    fn boundary_at_8_bps_is_high_vol() {
        // exactly at threshold → high_vol
        let s = snap(Some(8.0), Some(2.0));
        assert_eq!(s.regime(), Some(BtcRegime::Whipsaw));
    }
}
