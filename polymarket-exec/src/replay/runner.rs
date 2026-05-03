//! Deterministic replay loop.
//!
//! Drives a `ReplayStrategy` over a sorted, deduped stream of `Event`
//! records. Per-event lifecycle:
//!
//! 1. Advance the virtual clock to `event.received_ns`.
//! 2. Update orderbook state (delegated to the strategy via `on_event` —
//!    the live `core::book::BookStore` is accessible to the strategy
//!    adapter).
//! 3. Tick the strategy. Returned intents go through `FillSimulator`.
//! 4. Drain any fills and forward them to `on_fill`.
//!
//! Bug isolation: each window is wrapped in `panic::catch_unwind`. A
//! strategy panic in window N records a `Panicked` outcome and does not
//! poison any other window.
//!
//! Determinism: no `Instant::now`, no `SystemTime::now`, no
//! `chrono::Utc::now`. The clock is `received_ns` only. RNG, if any,
//! flows from `Seed64` derived per-window. `BTreeMap` everywhere so
//! iteration order is fixed.

use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::collector::schema::Event;
use crate::replay::fill_sim::{FillSimConfig, FillSimulator, SimulatedFill, StrategyOrderIntent};

/// What a window outputs. Aggregated into the run summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSummary {
    pub window_id: String,
    pub events_replayed: u64,
    pub intents_submitted: u64,
    pub fills: Vec<SimulatedFill>,
    pub status: WindowStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowStatus {
    Ok,
    Failed,
    Panicked,
}

/// Decision emitted by a strategy on each event.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplayDecision {
    pub submits: Vec<StrategyOrderIntent>,
    pub cancels: Vec<String>,
}

/// Trait the runner depends on. Implemented by a thin adapter over
/// `StrategyRegistry` (Phase 3b) and by the test fixture for Phase 3a.
///
/// `on_event` runs first per event. `on_fill` runs after the simulator
/// matches; both can return additional intents.
pub trait ReplayStrategy {
    fn on_event(&mut self, event: &Event) -> ReplayDecision;
    fn on_fill(&mut self, fill: &SimulatedFill) -> ReplayDecision;
}

/// Configuration for the replay run.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub window_id: String,
    pub fill_sim: FillSimConfig,
    /// `--max-window-failures` from the CLI; used at the run level by
    /// `run_run`. A single window's `run_window` always returns whatever
    /// outcome it reaches.
    pub max_window_failures: usize,
}

/// Run a single window. Bug-isolated by `panic::catch_unwind` around the
/// strategy + simulator invocation so a panic does not poison adjacent
/// windows.
pub fn run_window<S: ReplayStrategy>(
    strategy: &mut S,
    events: &[Event],
    cfg: &RunnerConfig,
) -> WindowSummary {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut sim = FillSimulator::new(cfg.fill_sim.clone());
        let mut intents_submitted: u64 = 0;
        for event in events {
            // Strategy reacts to event first
            let decision = strategy.on_event(event);
            for intent in decision.submits {
                sim.submit(intent);
                intents_submitted += 1;
            }
            let event_ms = (event.received_ns / 1_000_000) as u64;
            for coid in decision.cancels {
                sim.cancel(&coid, event_ms);
            }
            // Apply event to simulator (matches against resting orders)
            let fills_before = sim.fills().len();
            sim.on_event(event);
            let fills_after = sim.fills().len();
            for fill in sim.fills()[fills_before..fills_after].to_vec() {
                let decision = strategy.on_fill(&fill);
                for intent in decision.submits {
                    sim.submit(intent);
                    intents_submitted += 1;
                }
                for coid in decision.cancels {
                    sim.cancel(&coid, event_ms);
                }
            }
        }
        (sim.fills().to_vec(), intents_submitted)
    }));

    match outcome {
        Ok((fills, intents_submitted)) => WindowSummary {
            window_id: cfg.window_id.clone(),
            events_replayed: events.len() as u64,
            intents_submitted,
            fills,
            status: WindowStatus::Ok,
        },
        Err(_panic) => WindowSummary {
            window_id: cfg.window_id.clone(),
            events_replayed: events.len() as u64,
            intents_submitted: 0,
            fills: Vec::new(),
            status: WindowStatus::Panicked,
        },
    }
}

/// Run a set of windows in deterministic order (sorted by `window_id`).
/// Aborts with `Err` if `failed_count > max_window_failures`.
pub fn run_run<S, F>(
    windows: BTreeMap<String, Vec<Event>>,
    cfg: &RunnerConfig,
    mut strategy_factory: F,
) -> Result<Vec<WindowSummary>>
where
    S: ReplayStrategy,
    F: FnMut(&str) -> S,
{
    let mut out = Vec::with_capacity(windows.len());
    let mut failed = 0usize;
    for (window_id, events) in windows {
        let mut strategy = strategy_factory(&window_id);
        let mut win_cfg = cfg.clone();
        win_cfg.window_id = window_id;
        let summary = run_window(&mut strategy, &events, &win_cfg);
        if summary.status != WindowStatus::Ok {
            failed += 1;
            if failed > cfg.max_window_failures {
                anyhow::bail!(
                    "exceeded --max-window-failures ({}); aborting",
                    cfg.max_window_failures
                );
            }
        }
        out.push(summary);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::collector::schema::{EventType, Source};
    use crate::replay::fill_sim::{LatencyPreset, Side};

    fn evt(received_ns: i64, et: EventType, asset: &str, side: &str, price: &str, size: &str) -> Event {
        Event {
            v: 1,
            ts_ns: received_ns - 1,
            received_ns,
            event_type: et,
            market_type: "btc_5m".into(),
            market_slug: Some("btc-up-or-down".into()),
            asset_id: Some(asset.into()),
            side: Some(side.into()),
            price: Some(price.into()),
            size: Some(size.into()),
            sequence: Some(received_ns),
            source: Source::PolymarketMarketWs,
            raw: json!({}),
        }
    }

    /// Test fake: places one resting sell order on the first event, then
    /// passively counts fills. Used to verify the runner end-to-end.
    struct PassiveAskStrategy {
        placed: bool,
        on_fill_count: u32,
    }

    impl ReplayStrategy for PassiveAskStrategy {
        fn on_event(&mut self, event: &Event) -> ReplayDecision {
            if !self.placed {
                self.placed = true;
                return ReplayDecision {
                    submits: vec![StrategyOrderIntent {
                        client_order_id: "passive-1".into(),
                        asset_id: event.asset_id.clone().unwrap_or_default(),
                        side: Side::Sell,
                        price: 0.55,
                        size: 100.0,
                        placed_ms: (event.received_ns / 1_000_000) as u64,
                    }],
                    cancels: vec![],
                };
            }
            ReplayDecision::default()
        }
        fn on_fill(&mut self, _fill: &SimulatedFill) -> ReplayDecision {
            self.on_fill_count += 1;
            ReplayDecision::default()
        }
    }

    #[test]
    fn run_window_executes_event_loop_and_collects_fills() {
        let events = vec![
            evt(1_000_000_000, EventType::BookSnapshot, "asset-a", "buy", "0.55", "100"),
            evt(2_000_000_000, EventType::Trade, "asset-a", "buy", "0.55", "60"),
            evt(3_000_000_000, EventType::Trade, "asset-a", "buy", "0.55", "40"),
        ];
        let cfg = RunnerConfig {
            window_id: "w1".into(),
            fill_sim: FillSimConfig {
                latency: LatencyPreset::Instant,
                ..Default::default()
            },
            max_window_failures: 0,
        };
        let mut strategy = PassiveAskStrategy {
            placed: false,
            on_fill_count: 0,
        };
        let summary = run_window(&mut strategy, &events, &cfg);
        assert_eq!(summary.status, WindowStatus::Ok);
        assert_eq!(summary.fills.len(), 2);
        assert_eq!(summary.fills[0].size, 60.0);
        assert_eq!(summary.fills[1].size, 40.0);
        assert_eq!(strategy.on_fill_count, 2);
        assert_eq!(summary.events_replayed, 3);
        assert_eq!(summary.intents_submitted, 1);
    }

    /// Strategy that panics on second event. Used to verify panic isolation.
    struct PanicAfter(usize);
    impl ReplayStrategy for PanicAfter {
        fn on_event(&mut self, _event: &Event) -> ReplayDecision {
            self.0 = self.0.saturating_sub(1);
            if self.0 == 0 {
                panic!("synthetic strategy panic");
            }
            ReplayDecision::default()
        }
        fn on_fill(&mut self, _fill: &SimulatedFill) -> ReplayDecision {
            ReplayDecision::default()
        }
    }

    #[test]
    fn run_window_catches_strategy_panic_and_marks_window() {
        let events = vec![
            evt(1_000_000_000, EventType::BookDelta, "asset-a", "buy", "0.5", "10"),
            evt(2_000_000_000, EventType::Trade, "asset-a", "buy", "0.5", "10"),
        ];
        let cfg = RunnerConfig {
            window_id: "w-bad".into(),
            fill_sim: FillSimConfig::default(),
            max_window_failures: 0,
        };
        let mut s = PanicAfter(2);
        let summary = run_window(&mut s, &events, &cfg);
        assert_eq!(summary.status, WindowStatus::Panicked);
    }

    #[test]
    fn run_run_aborts_after_threshold() {
        let mut windows = BTreeMap::new();
        windows.insert(
            "a".to_string(),
            vec![evt(1_000_000_000, EventType::BookDelta, "a", "buy", "0.5", "1")],
        );
        windows.insert(
            "b".to_string(),
            vec![evt(2_000_000_000, EventType::BookDelta, "a", "buy", "0.5", "1")],
        );
        let cfg = RunnerConfig {
            window_id: String::new(),
            fill_sim: FillSimConfig::default(),
            max_window_failures: 0,
        };
        let result = run_run(windows, &cfg, |_| PanicAfter(1));
        assert!(result.is_err());
    }

    #[test]
    fn deterministic_runs_produce_identical_summaries() {
        // Same inputs twice → byte-identical fills vector.
        let events = vec![
            evt(1_000_000_000, EventType::BookSnapshot, "asset-a", "buy", "0.55", "100"),
            evt(2_000_000_000, EventType::Trade, "asset-a", "buy", "0.55", "60"),
            evt(3_000_000_000, EventType::Trade, "asset-a", "buy", "0.55", "40"),
        ];
        let cfg = RunnerConfig {
            window_id: "w1".into(),
            fill_sim: FillSimConfig {
                latency: LatencyPreset::Instant,
                ..Default::default()
            },
            max_window_failures: 0,
        };
        let mut s1 = PassiveAskStrategy { placed: false, on_fill_count: 0 };
        let mut s2 = PassiveAskStrategy { placed: false, on_fill_count: 0 };
        let r1 = run_window(&mut s1, &events, &cfg);
        let r2 = run_window(&mut s2, &events, &cfg);
        assert_eq!(r1, r2);
    }
}
