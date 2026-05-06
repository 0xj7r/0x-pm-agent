//! Parameterized order-arrival latency model for the backtest replay engine.
//!
//! Buffers strategy-emitted intents (orders, cancels, replaces) by a
//! configured delay before applying them to the simulated book. Lets us
//! answer questions of the form "if my orders arrive 100 ms later than
//! ideal, does paired_mm still profit?".
//!
//! Determinism: `Uniform` and `Normal` use a self-contained PCG-style
//! generator seeded by `seed`. The same seed produces the same delay
//! sequence run-to-run, regardless of platform.

use std::collections::BinaryHeap;

#[derive(Debug, Clone, Copy)]
pub enum LatencyModel {
    Constant { delay_ns: u64 },
    Uniform { min_ns: u64, max_ns: u64, seed: u64 },
    Normal { mean_ns: u64, stddev_ns: u64, seed: u64 },
}

/// Tiny PCG-XSH-RR style 64-bit generator. Self-contained so we avoid
/// pulling `rand` into the workspace just for delay sampling.
#[derive(Debug, Clone)]
struct Pcg64 {
    state: u128,
    inc: u128,
}

impl Pcg64 {
    fn new(seed: u64) -> Self {
        // Stream constant from the PCG reference; any odd 128-bit value works.
        let inc: u128 = 0xda3e_39cb_94b9_5bdb_5851_f42d_4c95_7f2du128 | 1;
        let mut rng = Self {
            state: 0,
            inc,
        };
        // Standard PCG init: step, add seed, step.
        rng.next_u64();
        rng.state = rng.state.wrapping_add(seed as u128);
        rng.next_u64();
        rng
    }

    fn next_u64(&mut self) -> u64 {
        let old = self.state;
        self.state = old
            .wrapping_mul(0x2360_ed05_1fc6_5da4_4385_df64_9fcc_f645u128)
            .wrapping_add(self.inc);
        let xorshifted = (((old >> 29) ^ old) >> 29) as u64;
        let rot = (old >> 122) as u32;
        xorshifted.rotate_right(rot)
    }

    /// Uniform f64 in [0, 1).
    fn next_f64(&mut self) -> f64 {
        // 53 bits of mantissa precision.
        ((self.next_u64() >> 11) as f64) * (1.0_f64 / ((1u64 << 53) as f64))
    }

    /// Standard normal via Box-Muller. Returns one sample; the paired
    /// sample is discarded for simplicity (delay sampling is not hot).
    fn next_standard_normal(&mut self) -> f64 {
        // Avoid log(0).
        let mut u1 = self.next_f64();
        while u1 <= f64::MIN_POSITIVE {
            u1 = self.next_f64();
        }
        let u2 = self.next_f64();
        let r = (-2.0_f64 * u1.ln()).sqrt();
        let theta = 2.0_f64 * std::f64::consts::PI * u2;
        r * theta.cos()
    }
}

/// Heap entry. `Reverse` semantics implemented manually so we get a
/// min-heap on `effective_at`, with submission order as tiebreaker.
#[derive(Debug)]
struct Entry<T> {
    effective_at: u64,
    seq: u64,
    intent: T,
}

impl<T> PartialEq for Entry<T> {
    fn eq(&self, other: &Self) -> bool {
        self.effective_at == other.effective_at && self.seq == other.seq
    }
}
impl<T> Eq for Entry<T> {}

impl<T> PartialOrd for Entry<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for Entry<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse so BinaryHeap (max-heap) pops the smallest first.
        other
            .effective_at
            .cmp(&self.effective_at)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

pub struct LatencyBuffer<T> {
    model: LatencyModel,
    rng: Option<Pcg64>,
    heap: BinaryHeap<Entry<T>>,
    next_seq: u64,
}

impl<T> LatencyBuffer<T> {
    pub fn new(model: LatencyModel) -> Self {
        let rng = match model {
            LatencyModel::Constant { .. } => None,
            LatencyModel::Uniform { seed, .. } | LatencyModel::Normal { seed, .. } => {
                Some(Pcg64::new(seed))
            }
        };
        Self {
            model,
            rng,
            heap: BinaryHeap::new(),
            next_seq: 0,
        }
    }

    /// Strategy emits `intent` at `now_ns`; this stores it with
    /// `effective_at = now_ns + sample(delay)`.
    pub fn submit(&mut self, now_ns: u64, intent: T) {
        let delay = self.sample_delay();
        let effective_at = now_ns.saturating_add(delay);
        let seq = self.next_seq;
        self.next_seq += 1;
        self.heap.push(Entry {
            effective_at,
            seq,
            intent,
        });
    }

    /// Drain all intents with `effective_at <= now_ns`, returning them in
    /// chronological `(effective_at, intent)` order. Ties broken by
    /// submission order.
    pub fn pop_due(&mut self, now_ns: u64) -> Vec<(u64, T)> {
        let mut out = Vec::new();
        while let Some(top) = self.heap.peek() {
            if top.effective_at <= now_ns {
                let e = self.heap.pop().expect("peeked");
                out.push((e.effective_at, e.intent));
            } else {
                break;
            }
        }
        out
    }

    /// Count intents with `effective_at <= now_ns` without draining.
    pub fn peek_due(&self, now_ns: u64) -> usize {
        self.heap
            .iter()
            .filter(|e| e.effective_at <= now_ns)
            .count()
    }

    fn sample_delay(&mut self) -> u64 {
        match self.model {
            LatencyModel::Constant { delay_ns } => delay_ns,
            LatencyModel::Uniform { min_ns, max_ns, .. } => {
                let rng = self.rng.as_mut().expect("uniform has rng");
                if max_ns <= min_ns {
                    return min_ns;
                }
                let span = max_ns - min_ns;
                // Inclusive of min, exclusive of max. For our purposes
                // (delay sampling) this is fine and matches typical
                // floating-point uniform conventions.
                let u = rng.next_f64();
                min_ns + (u * (span as f64)) as u64
            }
            LatencyModel::Normal {
                mean_ns,
                stddev_ns,
                ..
            } => {
                let rng = self.rng.as_mut().expect("normal has rng");
                let z = rng.next_standard_normal();
                let raw = (mean_ns as f64) + z * (stddev_ns as f64);
                if raw <= 0.0 {
                    0
                } else {
                    raw as u64
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_delay_holds_until_due() {
        let mut buf: LatencyBuffer<&'static str> =
            LatencyBuffer::new(LatencyModel::Constant {
                delay_ns: 100_000_000,
            });
        buf.submit(0, "order_a");

        assert_eq!(buf.pop_due(99_999_999).len(), 0);
        let due = buf.pop_due(100_000_000);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, 100_000_000);
        assert_eq!(due[0].1, "order_a");
        // Already drained.
        assert_eq!(buf.pop_due(1_000_000_000).len(), 0);
    }

    #[test]
    fn same_time_intents_emerge_in_submission_order() {
        let mut buf: LatencyBuffer<u32> =
            LatencyBuffer::new(LatencyModel::Constant { delay_ns: 50 });
        for i in 0..5 {
            buf.submit(0, i);
        }
        let due = buf.pop_due(100);
        assert_eq!(due.len(), 5);
        let intents: Vec<u32> = due.iter().map(|(_, v)| *v).collect();
        assert_eq!(intents, vec![0, 1, 2, 3, 4]);
        for (t, _) in &due {
            assert_eq!(*t, 50);
        }
    }

    #[test]
    fn uniform_delay_is_reproducible_with_same_seed() {
        let model = LatencyModel::Uniform {
            min_ns: 1_000_000,
            max_ns: 200_000_000,
            seed: 42,
        };
        let collect = |m: LatencyModel| -> Vec<u64> {
            let mut buf: LatencyBuffer<u32> = LatencyBuffer::new(m);
            for i in 0..32 {
                buf.submit(0, i);
            }
            let mut out: Vec<u64> = buf.pop_due(u64::MAX).iter().map(|(t, _)| *t).collect();
            out.sort();
            out
        };
        let a = collect(model);
        let b = collect(model);
        assert_eq!(a, b);
        for t in &a {
            assert!(*t >= 1_000_000 && *t <= 200_000_000);
        }
        // Sanity: not all the same value.
        let unique: std::collections::HashSet<u64> = a.iter().copied().collect();
        assert!(unique.len() > 1);
    }

    #[test]
    fn normal_delay_clipped_at_zero() {
        // Mean 0, large stddev: half the samples would be negative; all
        // must clip to 0.
        let mut buf: LatencyBuffer<u32> = LatencyBuffer::new(LatencyModel::Normal {
            mean_ns: 0,
            stddev_ns: 1_000_000,
            seed: 7,
        });
        for i in 0..200 {
            buf.submit(1_000_000_000, i);
        }
        let due = buf.pop_due(u64::MAX);
        assert_eq!(due.len(), 200);
        for (t, _) in &due {
            // effective_at >= now_ns since delay >= 0.
            assert!(*t >= 1_000_000_000);
        }
    }

    #[test]
    fn pop_due_returns_in_chronological_order() {
        // Submit in non-monotonic effective_at order using Constant=0
        // and varying now_ns.
        let mut buf: LatencyBuffer<&'static str> =
            LatencyBuffer::new(LatencyModel::Constant { delay_ns: 0 });
        buf.submit(500, "c");
        buf.submit(100, "a");
        buf.submit(900, "e");
        buf.submit(300, "b");
        buf.submit(700, "d");

        let due = buf.pop_due(1_000);
        let times: Vec<u64> = due.iter().map(|(t, _)| *t).collect();
        let intents: Vec<&str> = due.iter().map(|(_, v)| *v).collect();
        assert_eq!(times, vec![100, 300, 500, 700, 900]);
        assert_eq!(intents, vec!["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn peek_due_does_not_drain() {
        let mut buf: LatencyBuffer<u32> =
            LatencyBuffer::new(LatencyModel::Constant { delay_ns: 100 });
        buf.submit(0, 1);
        buf.submit(0, 2);
        buf.submit(50, 3);

        assert_eq!(buf.peek_due(99), 0);
        assert_eq!(buf.peek_due(100), 2);
        assert_eq!(buf.peek_due(150), 3);
        // Still all there after peeks.
        let due = buf.pop_due(150);
        assert_eq!(due.len(), 3);
    }

    #[test]
    fn fresh_instances_with_same_seed_match() {
        let model = LatencyModel::Normal {
            mean_ns: 50_000_000,
            stddev_ns: 10_000_000,
            seed: 12345,
        };
        let collect = |m: LatencyModel| -> Vec<u64> {
            let mut buf: LatencyBuffer<u32> = LatencyBuffer::new(m);
            for i in 0..16 {
                buf.submit(0, i);
            }
            buf.pop_due(u64::MAX).iter().map(|(t, _)| *t).collect()
        };
        let a = collect(model);
        let b = collect(model);
        assert_eq!(a, b);

        // Different seed should diverge.
        let other = LatencyModel::Normal {
            mean_ns: 50_000_000,
            stddev_ns: 10_000_000,
            seed: 67890,
        };
        let c = collect(other);
        assert_ne!(a, c);
    }

    #[test]
    fn uniform_degenerate_min_equals_max() {
        let mut buf: LatencyBuffer<u32> = LatencyBuffer::new(LatencyModel::Uniform {
            min_ns: 1_000,
            max_ns: 1_000,
            seed: 1,
        });
        buf.submit(0, 1);
        buf.submit(0, 2);
        let due = buf.pop_due(10_000);
        assert_eq!(due.len(), 2);
        assert!(due.iter().all(|(t, _)| *t == 1_000));
    }
}
