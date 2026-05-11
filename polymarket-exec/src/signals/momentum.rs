//! BTC momentum signal derived from underlying spot samples.
//!
//! This intentionally does not look at Polymarket YES/NO prices. It answers:
//! "is the BTC underlying persistently moving, and is that move accelerating?"

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SignalDirection {
    Up,
    Down,
    #[default]
    Neutral,
}

impl SignalDirection {
    pub fn from_signed(value: f64, deadband: f64) -> Self {
        if !value.is_finite() || value.abs() <= deadband {
            Self::Neutral
        } else if value > 0.0 {
            Self::Up
        } else {
            Self::Down
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MomentumConfig {
    pub lookback_windows: usize,
    pub window_ms: u64,
    pub decay_factor: f64,
    pub directional_deadband: f64,
}

impl Default for MomentumConfig {
    fn default() -> Self {
        Self {
            lookback_windows: 6,
            window_ms: 5 * 60 * 1_000,
            decay_factor: 0.75,
            directional_deadband: 0.15,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MomentumSignal {
    pub direction: SignalDirection,
    pub score: f64,
    pub strength: f64,
    pub latest_window_return_bps: Option<f64>,
    pub acceleration_bps: Option<f64>,
    pub window_returns_bps: Vec<f64>,
}

impl MomentumSignal {
    pub fn is_ready(&self) -> bool {
        !self.window_returns_bps.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MomentumEngine {
    config: MomentumConfig,
}

impl MomentumEngine {
    pub fn new(config: MomentumConfig) -> Self {
        Self { config }
    }

    pub fn compute(&self, now_ms: u64, samples: &[(u64, f64)]) -> MomentumSignal {
        let window_returns = window_returns_bps(now_ms, samples, self.config);
        if window_returns.is_empty() {
            return MomentumSignal::default();
        }

        let mut weighted_sum = 0.0;
        for (idx, ret) in window_returns.iter().enumerate() {
            let weight = self.config.decay_factor.clamp(0.0, 1.0).powi(idx as i32);
            weighted_sum += ret.signum() * weight;
        }
        let score = weighted_sum;
        let latest_window_return_bps = window_returns.first().copied();
        let acceleration_bps = match (window_returns.first(), window_returns.get(1)) {
            (Some(latest), Some(previous)) => Some(latest - previous),
            _ => None,
        };

        MomentumSignal {
            direction: SignalDirection::from_signed(score, self.config.directional_deadband),
            score,
            strength: score.abs().clamp(0.0, 1.0),
            latest_window_return_bps,
            acceleration_bps,
            window_returns_bps: window_returns,
        }
    }
}

impl Default for MomentumEngine {
    fn default() -> Self {
        Self::new(MomentumConfig::default())
    }
}

fn window_returns_bps(now_ms: u64, samples: &[(u64, f64)], config: MomentumConfig) -> Vec<f64> {
    if config.window_ms == 0 || config.lookback_windows == 0 {
        return Vec::new();
    }
    let valid = samples
        .iter()
        .copied()
        .filter(|(_, price)| price.is_finite() && *price > 0.0)
        .collect::<Vec<_>>();
    if valid.len() < 2 {
        return Vec::new();
    }

    let mut returns = Vec::with_capacity(config.lookback_windows);
    for window_idx in 0..config.lookback_windows {
        let end_ms = now_ms.saturating_sub(window_idx as u64 * config.window_ms);
        let start_ms = end_ms.saturating_sub(config.window_ms);
        let Some(start_price) = price_at_or_after(&valid, start_ms) else {
            break;
        };
        let Some(end_price) = price_at_or_before(&valid, end_ms) else {
            break;
        };
        if start_price <= 0.0 || end_price <= 0.0 {
            break;
        }
        returns.push(((end_price / start_price) - 1.0) * 10_000.0);
    }
    returns
}

fn price_at_or_after(samples: &[(u64, f64)], target_ms: u64) -> Option<f64> {
    samples
        .iter()
        .find(|(sample_ms, _)| *sample_ms >= target_ms)
        .map(|(_, price)| *price)
}

fn price_at_or_before(samples: &[(u64, f64)], target_ms: u64) -> Option<f64> {
    samples
        .iter()
        .rev()
        .find(|(sample_ms, _)| *sample_ms <= target_ms)
        .map(|(_, price)| *price)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(minute: u64, price: f64) -> (u64, f64) {
        (minute * 60_000, price)
    }

    #[test]
    fn multi_window_downtrend_produces_down_signal() {
        let engine = MomentumEngine::new(MomentumConfig {
            lookback_windows: 3,
            window_ms: 5 * 60_000,
            decay_factor: 0.75,
            directional_deadband: 0.15,
        });
        let samples = vec![
            sample(0, 100.0),
            sample(5, 99.0),
            sample(10, 98.0),
            sample(15, 97.0),
        ];

        let signal = engine.compute(15 * 60_000, &samples);

        assert_eq!(signal.direction, SignalDirection::Down);
        assert!(signal.score < -0.99);
        assert_eq!(signal.window_returns_bps.len(), 3);
    }

    #[test]
    fn mixed_windows_decay_toward_recent_move() {
        let engine = MomentumEngine::new(MomentumConfig {
            lookback_windows: 3,
            window_ms: 5 * 60_000,
            decay_factor: 0.5,
            directional_deadband: 0.15,
        });
        let samples = vec![
            sample(0, 100.0),
            sample(5, 99.0),
            sample(10, 98.0),
            sample(15, 99.0),
        ];

        let signal = engine.compute(15 * 60_000, &samples);

        assert_eq!(signal.direction, SignalDirection::Up);
        assert!(signal.score > 0.0);
        assert!(signal.acceleration_bps.is_some_and(|accel| accel > 0.0));
    }
}
