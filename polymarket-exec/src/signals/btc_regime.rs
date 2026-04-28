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
