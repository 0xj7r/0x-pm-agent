//! Runtime-local BTC spot signal accumulator.

use std::collections::VecDeque;

use crate::signals::{BtcRegimeSnapshot, MomentumEngine, MomentumSignal};
use pm_strategy::regime::WhipsawRiskSnapshot;

const BTC_SIGNAL_WINDOW_5M_MS: u64 = 5 * 60 * 1_000;
const BTC_SIGNAL_WINDOW_15M_MS: u64 = 15 * 60 * 1_000;
const BTC_SIGNAL_WINDOW_45M_MS: u64 = 45 * 60 * 1_000;
const BTC_SIGNAL_MIN_VOL_HISTORY_MS: u64 = 60 * 1_000;
const MAX_BTC_PRICE_SAMPLES: usize = 20_000;

#[derive(Debug, Default)]
pub(super) struct BtcSignalStore {
    last_price: Option<f64>,
    observed_at_ms: u64,
    price_samples: VecDeque<(u64, f64)>,
    trade_times: VecDeque<u64>,
}

impl BtcSignalStore {
    pub(super) fn record_trade(&mut self, price: f64, observed_at_ms: u64) {
        if !price.is_finite() || price <= 0.0 {
            return;
        }
        self.last_price = Some(price);
        self.observed_at_ms = observed_at_ms;
        self.price_samples.push_back((observed_at_ms, price));
        self.trade_times.push_back(observed_at_ms);
        self.prune(observed_at_ms);
    }

    pub(super) fn snapshot(&self, now_ms: u64) -> BtcRegimeSnapshot {
        let price_history_ms = self.price_history_ms(now_ms);
        let realized_vol_5m_bps = self.realized_vol_bps(now_ms, BTC_SIGNAL_WINDOW_5M_MS);
        let realized_vol_15m_bps = self.realized_vol_bps(now_ms, BTC_SIGNAL_WINDOW_15M_MS);
        let trade_count_5m = self.trade_count(now_ms, BTC_SIGNAL_WINDOW_5M_MS);
        let trade_count_15m = self.trade_count(now_ms, BTC_SIGNAL_WINDOW_15M_MS);
        let return_30s_bps = self.return_bps(now_ms, 30_000);
        let return_60s_bps = self.return_bps(now_ms, 60_000);
        let return_120s_bps = self.return_bps(now_ms, 120_000);
        let return_180s_bps = self.return_bps(now_ms, 180_000);

        BtcRegimeSnapshot {
            last_price: self.last_price,
            price_history_ms,
            realized_vol_5m_bps,
            realized_vol_15m_bps,
            trade_count_5m,
            trade_count_15m,
            return_30s_bps,
            return_60s_bps,
            return_120s_bps,
            return_180s_bps,
            observed_at_ms: self.observed_at_ms,
        }
    }

    pub(super) fn momentum_signal(&self, now_ms: u64) -> MomentumSignal {
        let samples = self.price_samples.iter().copied().collect::<Vec<_>>();
        MomentumEngine::default().compute(now_ms, &samples)
    }

    pub(super) fn whipsaw_snapshot(&self, now_ms: u64) -> WhipsawRiskSnapshot {
        const WINDOW_MS: u64 = 180_000;
        const STEP_MS: u64 = 5_000;

        let start_ms = now_ms.saturating_sub(WINDOW_MS);
        let mut sampled = Vec::with_capacity((WINDOW_MS / STEP_MS) as usize + 1);
        let mut next_ms = start_ms;
        while next_ms <= now_ms {
            if let Some(price) = self.price_at_or_before(next_ms) {
                sampled.push((next_ms, price));
            }
            next_ms = next_ms.saturating_add(STEP_MS);
            if next_ms == u64::MAX {
                break;
            }
        }
        if sampled.len() < 8 {
            return WhipsawRiskSnapshot::default();
        }

        let first = sampled.first().map(|(_, price)| *price).unwrap_or(0.0);
        let last = sampled.last().map(|(_, price)| *price).unwrap_or(0.0);
        if first <= 0.0 || last <= 0.0 {
            return WhipsawRiskSnapshot::default();
        }

        let mut path_abs = 0.0f64;
        let mut sumsq = 0.0f64;
        let mut returns = Vec::with_capacity(sampled.len().saturating_sub(1));
        for pair in sampled.windows(2) {
            let prev = pair[0].1;
            let next = pair[1].1;
            if prev <= 0.0 || next <= 0.0 {
                continue;
            }
            let ret = (next / prev).ln();
            if ret.is_finite() {
                path_abs += ret.abs();
                sumsq += ret * ret;
                returns.push(ret);
            }
        }
        if returns.len() < 7 || path_abs <= 0.0 {
            return WhipsawRiskSnapshot::default();
        }

        let net_abs = (last / first).ln().abs();
        let path_efficiency = (net_abs / path_abs).clamp(0.0, 1.0);
        let realized_vol_180s_bps =
            (sumsq / returns.len() as f64).sqrt() * (returns.len() as f64).sqrt() * 10_000.0;

        let mut sign_flips = 0usize;
        let mut prev_sign = 0i8;
        for ret in &returns {
            let sign = if *ret > 0.0 {
                1
            } else if *ret < 0.0 {
                -1
            } else {
                0
            };
            if sign != 0 && prev_sign != 0 && sign != prev_sign {
                sign_flips += 1;
            }
            if sign != 0 {
                prev_sign = sign;
            }
        }
        let sign_flip_rate = if returns.len() > 1 {
            sign_flips as f64 / (returns.len() - 1) as f64
        } else {
            0.0
        };

        let mid = sampled.len() / 2;
        let early = sampled
            .get(mid)
            .map(|(_, price)| (*price / first - 1.0) * 10_000.0)
            .unwrap_or(0.0);
        let late = (last / sampled[mid].1 - 1.0) * 10_000.0;
        let reversal_pressure =
            if early.is_finite() && late.is_finite() && early.signum() != late.signum() {
                (early.abs().min(late.abs()) / 12.0).clamp(0.0, 1.0)
            } else {
                0.0
            };

        let score =
            ((1.0 - path_efficiency) * 0.45 + sign_flip_rate * 0.35 + reversal_pressure * 0.20)
                .clamp(0.0, 1.0);

        WhipsawRiskSnapshot {
            score: score as f32,
            path_efficiency: path_efficiency as f32,
            sign_flip_rate: sign_flip_rate as f32,
            realized_vol_180s_bps: realized_vol_180s_bps as f32,
            reversal_pressure: reversal_pressure as f32,
            sample_count: sampled.len(),
        }
    }

    fn prune(&mut self, now_ms: u64) {
        while let Some((sample_ms, _)) = self.price_samples.front().copied() {
            if now_ms.saturating_sub(sample_ms) <= BTC_SIGNAL_WINDOW_45M_MS {
                break;
            }
            self.price_samples.pop_front();
        }
        while let Some(sample_ms) = self.trade_times.front().copied() {
            if now_ms.saturating_sub(sample_ms) <= BTC_SIGNAL_WINDOW_45M_MS {
                break;
            }
            self.trade_times.pop_front();
        }
        while self.price_samples.len() > MAX_BTC_PRICE_SAMPLES {
            self.price_samples.pop_front();
        }
    }

    fn trade_count(&self, now_ms: u64, window_ms: u64) -> u64 {
        self.trade_times
            .iter()
            .rev()
            .take_while(|&&sample_ms| now_ms.saturating_sub(sample_ms) <= window_ms)
            .count() as u64
    }

    fn price_history_ms(&self, now_ms: u64) -> u64 {
        self.price_samples
            .front()
            .map(|(sample_ms, _)| now_ms.saturating_sub(*sample_ms))
            .unwrap_or(0)
    }

    fn realized_vol_bps(&self, now_ms: u64, window_ms: u64) -> Option<f64> {
        let points = self
            .price_samples
            .iter()
            .copied()
            .filter(|(sample_ms, price)| {
                now_ms.saturating_sub(*sample_ms) <= window_ms && price.is_finite() && *price > 0.0
            })
            .collect::<Vec<_>>();
        let coverage_ms = points
            .first()
            .zip(points.last())
            .map(|(first, last)| last.0.saturating_sub(first.0))
            .unwrap_or(0);
        if coverage_ms < BTC_SIGNAL_MIN_VOL_HISTORY_MS.min(window_ms) {
            return None;
        }
        if points.len() < 2 {
            return None;
        }
        let mut returns = Vec::with_capacity(points.len().saturating_sub(1));
        for pair in points.windows(2) {
            let prev = pair[0].1;
            let next = pair[1].1;
            if prev > 0.0 && next > 0.0 {
                returns.push(((next / prev) - 1.0) * 10_000.0);
            }
        }
        if returns.len() < 2 {
            return None;
        }
        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let var = returns
            .iter()
            .map(|value| {
                let d = *value - mean;
                d * d
            })
            .sum::<f64>()
            / returns.len() as f64;
        let sigma_tick = var.sqrt();
        Some(sigma_tick * (returns.len() as f64).sqrt())
    }

    fn return_bps(&self, now_ms: u64, horizon_ms: u64) -> Option<f64> {
        let current = self.last_price?;
        let target_ms = now_ms.saturating_sub(horizon_ms);
        let baseline = self
            .price_samples
            .iter()
            .rev()
            .find(|(sample_ms, _)| *sample_ms <= target_ms)
            .map(|(_, price)| *price)?;
        if baseline <= 0.0 {
            return None;
        }
        Some(((current / baseline) - 1.0) * 10_000.0)
    }

    fn price_at_or_before(&self, target_ms: u64) -> Option<f64> {
        self.price_samples
            .iter()
            .rev()
            .find(|(sample_ms, price)| *sample_ms <= target_ms && price.is_finite() && *price > 0.0)
            .map(|(_, price)| *price)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_require_history_covering_the_requested_horizon() {
        let mut store = BtcSignalStore::default();
        store.record_trade(100.0, 1_000);
        store.record_trade(101.0, 6_000);

        let snap = store.snapshot(6_000);

        assert_eq!(snap.price_history_ms, 5_000);
        assert_eq!(snap.return_30s_bps, None);
        assert_eq!(snap.return_60s_bps, None);
        assert_eq!(snap.return_120s_bps, None);
        assert_eq!(snap.return_180s_bps, None);
    }

    #[test]
    fn realized_vol_requires_real_time_history_not_just_many_recent_ticks() {
        let mut store = BtcSignalStore::default();
        for i in 0..100 {
            store.record_trade(100.0 + (i as f64 * 0.01), 1_000 + i * 10);
        }

        let snap = store.snapshot(2_000);

        assert_eq!(snap.price_history_ms, 1_000);
        assert_eq!(snap.realized_vol_5m_bps, None);
    }

    #[test]
    fn returns_appear_once_history_covers_the_horizon() {
        let mut store = BtcSignalStore::default();
        store.record_trade(100.0, 1_000);
        store.record_trade(101.0, 31_000);

        let snap = store.snapshot(31_000);

        assert_eq!(snap.price_history_ms, 30_000);
        assert!(snap.return_30s_bps.is_some());
        assert_eq!(snap.return_60s_bps, None);
    }

    #[test]
    fn whipsaw_snapshot_exports_path_features_after_warmup() {
        let mut store = BtcSignalStore::default();
        for i in 0..=36 {
            let price = if i % 2 == 0 { 100.0 } else { 100.3 };
            store.record_trade(price, i * 5_000);
        }

        let snap = store.whipsaw_snapshot(180_000);

        assert!(snap.sample_count >= 30);
        assert!(snap.sign_flip_rate > 0.8);
        assert!(snap.path_efficiency < 0.2);
        assert!(snap.score > 0.5);
    }
}
