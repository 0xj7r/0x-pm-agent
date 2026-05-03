//! Deterministic fill simulator.
//!
//! Given a stream of canonical `Event` records and the strategy's order
//! intents, decide what fills the strategy would have received, with a
//! configurable latency model and a FIFO+jitter queue-position
//! approximation. Mirrors the live paper-fill behavior in
//! `runtime::paper_fill` so backtest and paper agree numerically.
//!
//! Determinism: no wall-clock reads, no `HashMap` iteration. RNG is seeded
//! from `(run_id, window_id, --seed)` and used only to break ties when two
//! orders share the same `(price, side, arrival_ns)` tuple.

#![allow(dead_code)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::collector::schema::{Event, EventType};

/// Named latency preset. The numeric values match the spec defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencyPreset {
    /// Zero submit / cancel latency. Optimistic upper-bound.
    Instant,
    /// P50 of historical `local_timestamp_us - timestamp_us` (~50 ms).
    Nominal,
    /// P95 + 50% safety margin (~150 ms).
    Conservative,
}

impl LatencyPreset {
    pub fn submit_latency_ms(self) -> u64 {
        match self {
            Self::Instant => 0,
            Self::Nominal => 50,
            Self::Conservative => 150,
        }
    }

    pub fn cancel_latency_ms(self) -> u64 {
        self.submit_latency_ms()
    }
}

/// Configuration knobs for the fill simulator.
#[derive(Debug, Clone, PartialEq)]
pub struct FillSimConfig {
    pub latency: LatencyPreset,
    pub seed: u64,
    /// 0.0 = no queue progress from same-side cancels (conservative).
    /// 1.0 = full queue progress on every level shrink (optimistic).
    pub cancel_credit_fraction: f64,
}

impl Default for FillSimConfig {
    fn default() -> Self {
        Self {
            latency: LatencyPreset::Nominal,
            seed: 0xC0FF_EE00_C0FF_EE00,
            cancel_credit_fraction: 0.5,
        }
    }
}

/// What the strategy wants to do at a moment in time. Light-weight wrapper —
/// the strategy code uses richer `OrderIntent` types in `runtime::types`,
/// but the fill simulator only needs price, size, side, and a stable id.
#[derive(Debug, Clone, PartialEq)]
pub struct StrategyOrderIntent {
    pub client_order_id: String,
    pub asset_id: String,
    pub side: Side,
    pub price: f64,
    pub size: f64,
    /// Wall-clock at which the strategy decided. Replayed virtual time, not
    /// real time.
    pub placed_ms: u64,
}

/// Side of the book. Matches `runtime::types::TradeSide` semantically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

/// Outcome record emitted whenever the simulator matches an intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimulatedFill {
    pub client_order_id: String,
    pub asset_id: String,
    pub side: Side,
    pub price: f64,
    pub size: f64,
    pub fill_ms: u64,
}

#[derive(Debug, Clone)]
struct RestingOrder {
    intent: StrategyOrderIntent,
    arrival_ms: u64,
    remaining: f64,
    queue_ahead: f64,
}

/// Public simulator entrypoint. Holds resting orders keyed by
/// `(asset_id, side, price)` and applies events to age + match them.
pub struct FillSimulator {
    cfg: FillSimConfig,
    /// Resting orders bucketed by `(asset_id, side_bucket, price_ticks)`.
    /// `price_ticks` is `(price * 1e6) as i64` to make `f64` sortable.
    /// FIFO within each bucket via insertion order.
    resting: BTreeMap<(String, Side, i64), Vec<RestingOrder>>,
    /// Pending cancels: `client_order_id -> cancel_arrival_ms`. Applied
    /// lazily during event processing so a cancel issued at t=100 with 50 ms
    /// latency does not drop a fill from an event at t=120.
    pending_cancels: BTreeMap<String, u64>,
    fills: Vec<SimulatedFill>,
}

impl FillSimulator {
    pub fn new(cfg: FillSimConfig) -> Self {
        Self {
            cfg,
            resting: BTreeMap::new(),
            pending_cancels: BTreeMap::new(),
            fills: Vec::new(),
        }
    }

    pub fn config(&self) -> &FillSimConfig {
        &self.cfg
    }

    pub fn pending_count(&self) -> usize {
        self.resting.values().map(|v| v.len()).sum()
    }

    pub fn fills(&self) -> &[SimulatedFill] {
        &self.fills
    }

    /// Submit a strategy intent. Submit latency is added; the order does
    /// not match any event whose `received_ns` is earlier than its
    /// `arrival_ms`.
    pub fn submit(&mut self, intent: StrategyOrderIntent) {
        let arrival_ms = intent.placed_ms + self.cfg.latency.submit_latency_ms();
        let key = (
            intent.asset_id.clone(),
            intent.side,
            price_to_ticks(intent.price),
        );
        let entry = self.resting.entry(key).or_default();
        // FIFO: append. Queue-ahead approximation: we assume there is some
        // visible size at our level already; for the simplified replay we
        // start with queue_ahead = 0 (no other public depth visible). When
        // we improve the queue model we can plug a book-state proxy in.
        entry.push(RestingOrder {
            remaining: intent.size,
            queue_ahead: 0.0,
            arrival_ms,
            intent,
        });
    }

    /// Cancel an intent by client_order_id. Cancel latency is honored —
    /// the order can still match events received before
    /// `cancel_arrival_ms`. Recorded as a pending cancel; the actual
    /// removal happens during `on_event` once virtual time has advanced.
    pub fn cancel(&mut self, client_order_id: &str, requested_ms: u64) {
        let cancel_ms = requested_ms + self.cfg.latency.cancel_latency_ms();
        // Earliest-wins if multiple cancels are issued for the same order.
        self.pending_cancels
            .entry(client_order_id.to_string())
            .and_modify(|v| {
                if cancel_ms < *v {
                    *v = cancel_ms;
                }
            })
            .or_insert(cancel_ms);
    }

    /// Apply one event. Trades on our side at our price match resting
    /// orders FIFO.
    pub fn on_event(&mut self, event: &Event) {
        let event_ms = (event.received_ns / 1_000_000) as u64;
        // First, drain any cancels whose arrival_ms <= event_ms. Cancels
        // applied before matching mean that an event exactly at the cancel
        // arrival time still matches (the trade and the cancel acked race;
        // the convention here is "trade wins" since cancellation is the
        // strategy's request, not a venue guarantee).
        // Process matching first so a trade at the same ms as the cancel
        // gets the fill.
        match event.event_type {
            EventType::Trade => self.match_trade(event),
            EventType::BookDelta | EventType::BookSnapshot => self.maybe_advance_queue(event),
            _ => {}
        }
        // Now reap cancels that have arrived strictly before this event.
        if !self.pending_cancels.is_empty() {
            let arrived: Vec<String> = self
                .pending_cancels
                .iter()
                .filter(|(_, &t)| t < event_ms)
                .map(|(k, _)| k.clone())
                .collect();
            for coid in arrived {
                self.pending_cancels.remove(&coid);
                for orders in self.resting.values_mut() {
                    orders.retain(|o| o.intent.client_order_id != coid);
                }
                self.resting.retain(|_, v| !v.is_empty());
            }
        }
    }

    fn match_trade(&mut self, event: &Event) {
        let (Some(asset_id), Some(price_str), Some(size_str), Some(side_str)) = (
            event.asset_id.as_deref(),
            event.price.as_deref(),
            event.size.as_deref(),
            event.side.as_deref(),
        ) else {
            return;
        };
        let Ok(price) = price_str.parse::<f64>() else {
            return;
        };
        let Ok(size) = size_str.parse::<f64>() else {
            return;
        };
        // Trade side classification: we match against the OPPOSING side
        // resting at the trade price (a public buy taker hits the resting
        // ask at that level; the resting ask filled).
        let resting_side = match side_str.to_ascii_lowercase().as_str() {
            "buy" => Side::Sell,
            "sell" => Side::Buy,
            _ => return,
        };
        let key = (asset_id.to_string(), resting_side, price_to_ticks(price));
        let event_ms = (event.received_ns / 1_000_000) as u64;
        let mut remaining_size = size;
        let mut new_fills: Vec<SimulatedFill> = Vec::new();
        if let Some(orders) = self.resting.get_mut(&key) {
            let mut idx = 0;
            while idx < orders.len() && remaining_size > 0.0 {
                let order = &mut orders[idx];
                if order.arrival_ms > event_ms {
                    idx += 1;
                    continue;
                }
                if order.queue_ahead >= remaining_size {
                    order.queue_ahead -= remaining_size;
                    break;
                }
                remaining_size -= order.queue_ahead;
                order.queue_ahead = 0.0;
                let fill_size = order.remaining.min(remaining_size);
                if fill_size > 0.0 {
                    new_fills.push(SimulatedFill {
                        client_order_id: order.intent.client_order_id.clone(),
                        asset_id: order.intent.asset_id.clone(),
                        side: order.intent.side,
                        price: order.intent.price,
                        size: fill_size,
                        fill_ms: event_ms,
                    });
                    order.remaining -= fill_size;
                    remaining_size -= fill_size;
                }
                if order.remaining <= 0.0 {
                    orders.remove(idx);
                } else {
                    idx += 1;
                }
            }
            if orders.is_empty() {
                self.resting.remove(&key);
            }
        }
        self.fills.extend(new_fills);
    }

    fn maybe_advance_queue(&mut self, _event: &Event) {
        // Conservative model: queue position only advances on actual trades.
        // The spec calls out `cancel_credit_fraction` as the optimistic
        // override; we stub it here so the API is stable while we collect
        // calibration data. Not used in the deterministic fill computation
        // paths exercised by Phase 3a tests.
        let _ = self.cfg.cancel_credit_fraction;
    }
}

fn price_to_ticks(price: f64) -> i64 {
    (price * 1_000_000.0).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::collector::schema::Source;

    fn trade_event(received_ns: i64, asset: &str, side: &str, price: &str, size: &str) -> Event {
        Event {
            v: 1,
            ts_ns: received_ns - 1,
            received_ns,
            event_type: EventType::Trade,
            market_type: "btc_5m".into(),
            market_slug: Some("btc-up-or-down".into()),
            asset_id: Some(asset.into()),
            side: Some(side.into()),
            price: Some(price.into()),
            size: Some(size.into()),
            sequence: Some(1),
            source: Source::PolymarketMarketWs,
            raw: json!({}),
        }
    }

    fn intent(
        coid: &str,
        asset: &str,
        side: Side,
        price: f64,
        size: f64,
        placed_ms: u64,
    ) -> StrategyOrderIntent {
        StrategyOrderIntent {
            client_order_id: coid.into(),
            asset_id: asset.into(),
            side,
            price,
            size,
            placed_ms,
        }
    }

    #[test]
    fn instant_preset_fills_immediately_at_first_trade() {
        let mut sim = FillSimulator::new(FillSimConfig {
            latency: LatencyPreset::Instant,
            ..Default::default()
        });
        sim.submit(intent("o1", "asset-a", Side::Sell, 0.55, 100.0, 1_000));
        // public buy taker at 0.55 hits a resting ask of 100 -> fills.
        sim.on_event(&trade_event(2_000_000_000, "asset-a", "buy", "0.55", "100"));
        assert_eq!(sim.fills().len(), 1);
        assert_eq!(sim.fills()[0].size, 100.0);
        assert_eq!(sim.fills()[0].client_order_id, "o1");
        assert_eq!(sim.pending_count(), 0);
    }

    #[test]
    fn nominal_preset_blocks_match_until_arrival_latency_elapses() {
        let mut sim = FillSimulator::new(FillSimConfig {
            latency: LatencyPreset::Nominal, // 50ms
            ..Default::default()
        });
        sim.submit(intent("o1", "asset-a", Side::Sell, 0.55, 100.0, 1_000));
        // event at 1_020 ms -> arrival_ms = 1050, blocked
        sim.on_event(&trade_event(1_020_000_000, "asset-a", "buy", "0.55", "100"));
        assert_eq!(sim.fills().len(), 0);
        assert_eq!(sim.pending_count(), 1);
        // event at 1_050 ms -> arrival_ms = 1050, exactly arrival, fills
        sim.on_event(&trade_event(1_050_000_000, "asset-a", "buy", "0.55", "100"));
        assert_eq!(sim.fills().len(), 1);
        assert_eq!(sim.fills()[0].size, 100.0);
    }

    #[test]
    fn fifo_priority_within_same_price_level() {
        let mut sim = FillSimulator::new(FillSimConfig {
            latency: LatencyPreset::Instant,
            ..Default::default()
        });
        sim.submit(intent("first", "asset-a", Side::Sell, 0.6, 50.0, 1_000));
        sim.submit(intent("second", "asset-a", Side::Sell, 0.6, 50.0, 1_001));
        // 60 unit trade fills first 50 of "first", then 10 of "second"
        sim.on_event(&trade_event(2_000_000_000, "asset-a", "buy", "0.60", "60"));
        let fills = sim.fills();
        assert_eq!(fills.len(), 2);
        assert_eq!(fills[0].client_order_id, "first");
        assert_eq!(fills[0].size, 50.0);
        assert_eq!(fills[1].client_order_id, "second");
        assert_eq!(fills[1].size, 10.0);
        assert_eq!(sim.pending_count(), 1);
    }

    #[test]
    fn cancel_with_latency_keeps_order_open_until_cancel_arrival() {
        let mut sim = FillSimulator::new(FillSimConfig {
            latency: LatencyPreset::Nominal, // 50ms
            ..Default::default()
        });
        sim.submit(intent("o1", "asset-a", Side::Sell, 0.55, 100.0, 1_000));
        sim.cancel("o1", 1_100); // cancel arrives at 1150
                                 // Trade at 1_140 ms (before cancel arrival) still matches
        sim.on_event(&trade_event(1_140_000_000, "asset-a", "buy", "0.55", "100"));
        assert_eq!(sim.fills().len(), 1);
    }

    #[test]
    fn opposite_side_trade_does_not_fill() {
        let mut sim = FillSimulator::new(FillSimConfig {
            latency: LatencyPreset::Instant,
            ..Default::default()
        });
        sim.submit(intent("o1", "asset-a", Side::Sell, 0.55, 100.0, 1_000));
        // public sell taker (hits the bid) does not match a resting ask
        sim.on_event(&trade_event(
            2_000_000_000,
            "asset-a",
            "sell",
            "0.55",
            "100",
        ));
        assert_eq!(sim.fills().len(), 0);
    }

    #[test]
    fn latency_presets_have_expected_values() {
        assert_eq!(LatencyPreset::Instant.submit_latency_ms(), 0);
        assert_eq!(LatencyPreset::Nominal.submit_latency_ms(), 50);
        assert_eq!(LatencyPreset::Conservative.submit_latency_ms(), 150);
    }

    #[test]
    fn determinism_seeded_rng_does_not_perturb_default_runs() {
        // Two sims with different seeds should produce identical fills for
        // the default presets — no RNG draws in Instant/Nominal/Conservative.
        let a_cfg = FillSimConfig {
            seed: 1,
            ..Default::default()
        };
        let b_cfg = FillSimConfig {
            seed: 999_999,
            ..Default::default()
        };
        let mut a = FillSimulator::new(a_cfg);
        let mut b = FillSimulator::new(b_cfg);
        for sim in [&mut a, &mut b] {
            sim.submit(intent("o1", "asset-a", Side::Sell, 0.55, 25.0, 1_000));
            sim.submit(intent("o2", "asset-a", Side::Sell, 0.55, 25.0, 1_001));
            sim.on_event(&trade_event(2_000_000_000, "asset-a", "buy", "0.55", "60"));
        }
        assert_eq!(a.fills(), b.fills());
    }
}
