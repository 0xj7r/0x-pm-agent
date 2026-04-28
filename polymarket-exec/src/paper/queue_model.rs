//! Self-calibrating FIFO queue-priority model for the shadow fill simulator.
//!
//! Maintains a rolling estimate of `queue_decay_rate_per_sec` per market
//! family: the rate at which depth ahead of our order in the FIFO queue
//! dissipates from cancellations, independent of fills at our price level.
//! This rate is INTERNAL STATE, not configuration. It is updated from
//! observed fill outcomes (live wallet fills in production, joined
//! /activity fills during shadow).
//!
//! Formula:
//!   depth_ahead_remaining = max(
//!       0,
//!       depth_ahead_at_post
//!           - queue_decay_rate_per_sec * elapsed_seconds
//!           - cumulative_volume_at_or_better_since_post
//!   )

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthInputs {
    pub depth_ahead_at_post: f64,
    pub elapsed_seconds: f64,
    pub cumulative_volume_at_or_better: f64,
}

pub fn depth_ahead_remaining(inputs: DepthInputs, decay_rate_per_sec: f64) -> f64 {
    if decay_rate_per_sec.is_nan() {
        return f64::NAN;
    }
    (inputs.depth_ahead_at_post
        - decay_rate_per_sec * inputs.elapsed_seconds
        - inputs.cumulative_volume_at_or_better)
        .max(0.0)
}

#[derive(Debug, Clone, Copy)]
struct FamilyState {
    rate: f64,
    n_observations: u32,
}

#[derive(Debug)]
pub struct QueueDecayEstimator {
    state: HashMap<String, FamilyState>,
    min_observations: u32,
    ema_alpha: f64,
}

impl Default for QueueDecayEstimator {
    fn default() -> Self {
        Self::new(8, 0.2)
    }
}

impl QueueDecayEstimator {
    pub fn new(min_observations: u32, ema_alpha: f64) -> Self {
        assert!(min_observations >= 1);
        assert!((0.0..=1.0).contains(&ema_alpha));
        Self {
            state: HashMap::new(),
            min_observations,
            ema_alpha,
        }
    }

    pub fn rate_for(&self, family: &str) -> f64 {
        self.state.get(family).map(|s| s.rate).unwrap_or(f64::NAN)
    }

    pub fn n_observations(&self, family: &str) -> u32 {
        self.state.get(family).map(|s| s.n_observations).unwrap_or(0)
    }

    pub fn update_with_fill(
        &mut self,
        family: &str,
        depth_ahead_at_post: f64,
        elapsed_seconds: f64,
        cumulative_volume_at_or_better: f64,
    ) -> f64 {
        if !(elapsed_seconds.is_finite() && elapsed_seconds > 0.0) {
            return self.rate_for(family);
        }
        if !(depth_ahead_at_post.is_finite() && cumulative_volume_at_or_better.is_finite()) {
            return self.rate_for(family);
        }
        let single_rate =
            ((depth_ahead_at_post - cumulative_volume_at_or_better) / elapsed_seconds).max(0.0);
        let entry = self
            .state
            .entry(family.to_string())
            .or_insert(FamilyState {
                rate: f64::NAN,
                n_observations: 0,
            });
        entry.n_observations += 1;
        if entry.rate.is_nan() {
            if entry.n_observations >= self.min_observations {
                entry.rate = single_rate;
            }
        } else {
            entry.rate = self.ema_alpha * single_rate + (1.0 - self.ema_alpha) * entry.rate;
        }
        entry.rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_remaining_subtracts_decay_and_volume() {
        let r = depth_ahead_remaining(
            DepthInputs {
                depth_ahead_at_post: 100.0,
                elapsed_seconds: 10.0,
                cumulative_volume_at_or_better: 30.0,
            },
            2.0,
        );
        assert!((r - 50.0).abs() < 1e-9);
    }

    #[test]
    fn depth_remaining_floors_at_zero() {
        let r = depth_ahead_remaining(
            DepthInputs {
                depth_ahead_at_post: 10.0,
                elapsed_seconds: 100.0,
                cumulative_volume_at_or_better: 5.0,
            },
            2.0,
        );
        assert_eq!(r, 0.0);
    }

    #[test]
    fn depth_remaining_is_nan_when_decay_uncalibrated() {
        let r = depth_ahead_remaining(
            DepthInputs {
                depth_ahead_at_post: 100.0,
                elapsed_seconds: 10.0,
                cumulative_volume_at_or_better: 30.0,
            },
            f64::NAN,
        );
        assert!(r.is_nan());
    }

    #[test]
    fn estimator_starts_uncalibrated() {
        let est = QueueDecayEstimator::new(3, 0.3);
        assert!(est.rate_for("btc-updown-5m").is_nan());
        assert_eq!(est.n_observations("btc-updown-5m"), 0);
    }

    #[test]
    fn estimator_stays_uncalibrated_until_min_observations() {
        let mut est = QueueDecayEstimator::new(3, 0.3);
        est.update_with_fill("btc-updown-5m", 100.0, 10.0, 30.0);
        est.update_with_fill("btc-updown-5m", 120.0, 12.0, 40.0);
        assert!(est.rate_for("btc-updown-5m").is_nan());
        assert_eq!(est.n_observations("btc-updown-5m"), 2);

        est.update_with_fill("btc-updown-5m", 80.0, 8.0, 24.0);
        assert!(!est.rate_for("btc-updown-5m").is_nan());
        assert_eq!(est.n_observations("btc-updown-5m"), 3);
    }

    #[test]
    fn estimator_emas_subsequent_observations() {
        let mut est = QueueDecayEstimator::new(1, 0.5);
        let r1 = est.update_with_fill("btc-updown-5m", 100.0, 10.0, 30.0);
        assert!((r1 - 7.0).abs() < 1e-9);

        let r2 = est.update_with_fill("btc-updown-5m", 100.0, 10.0, 60.0);
        assert!((r2 - 5.5).abs() < 1e-9);
    }

    #[test]
    fn estimator_keeps_per_family_state_independent() {
        let mut est = QueueDecayEstimator::new(1, 0.5);
        est.update_with_fill("btc-updown-5m", 100.0, 10.0, 30.0);
        est.update_with_fill("eth-updown-5m", 200.0, 10.0, 40.0);
        assert!((est.rate_for("btc-updown-5m") - 7.0).abs() < 1e-9);
        assert!((est.rate_for("eth-updown-5m") - 16.0).abs() < 1e-9);
    }

    #[test]
    fn estimator_skips_invalid_inputs() {
        let mut est = QueueDecayEstimator::new(1, 0.5);
        let r = est.update_with_fill("btc-updown-5m", 100.0, 0.0, 30.0);
        assert!(r.is_nan());
        assert_eq!(est.n_observations("btc-updown-5m"), 0);

        let r = est.update_with_fill("btc-updown-5m", f64::NAN, 10.0, 30.0);
        assert!(r.is_nan());
        assert_eq!(est.n_observations("btc-updown-5m"), 0);
    }

    #[test]
    fn fill_count_monotonic_in_decay_rate_property() {
        // Property: holding (depth_ahead_at_post, elapsed, vol) constant,
        // larger decay rate should never INCREASE depth_ahead_remaining.
        let inputs = DepthInputs {
            depth_ahead_at_post: 200.0,
            elapsed_seconds: 30.0,
            cumulative_volume_at_or_better: 50.0,
        };
        let mut last = f64::INFINITY;
        for rate_milli in (0..1000).step_by(50) {
            let r = depth_ahead_remaining(inputs, rate_milli as f64 / 100.0);
            assert!(r <= last + 1e-9, "non-monotonic at rate={}", rate_milli);
            last = r;
        }
    }
}
