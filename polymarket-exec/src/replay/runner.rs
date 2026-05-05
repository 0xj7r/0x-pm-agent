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

use crate::collector::schema::{Event, EventType};
use crate::replay::fill_sim::{
    FillSimConfig, FillSimulator, Side, SimulatedFill, SimulatedRejection, StrategyOrderIntent,
};
use crate::replay::risk_trace::RiskRejection;
use crate::replay::synthesizer::EventSynthesizer;

/// What a window outputs. Aggregated into the run summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSummary {
    pub window_id: String,
    pub events_replayed: u64,
    pub intents_submitted: u64,
    pub fills: Vec<SimulatedFill>,
    /// Deterministic accounting from the replayed fill stream plus the
    /// replayed market marks. This is the canonical source for backtest
    /// PnL/equity fields; downstream reports should not infer PnL from
    /// `filled_qty * limit_price` alone.
    #[serde(default)]
    pub accounting: ReplayAccountingSummary,
    /// Post-only rejections produced by the fill simulator (intents that
    /// would have crossed the live book at submit time).
    #[serde(default)]
    pub post_only_rejections: Vec<SimulatedRejection>,
    /// Risk-engine rejections captured during replay. Emitted to
    /// `runs/run_id=<id>/trace/strand=risk_rejections/...parquet` by the
    /// downstream writer (Phase 3d wiring; the strand is captured here).
    #[serde(default)]
    pub risk_rejections: Vec<RiskRejection>,
    pub status: WindowStatus,
}

#[cfg(test)]
mod replay_accounting_tests {
    use super::*;
    use serde_json::json;

    use crate::collector::schema::Source;
    use crate::replay::fill_sim::MakerOrTaker;

    fn mark_event(asset_id: &str, price: &str) -> Event {
        Event {
            v: 1,
            ts_ns: 1,
            received_ns: 1,
            event_type: EventType::BookSnapshot,
            market_type: "btc_5m".to_string(),
            market_slug: Some("btc-up-or-down".to_string()),
            asset_id: Some(asset_id.to_string()),
            side: Some("buy".to_string()),
            price: Some(price.to_string()),
            size: Some("100".to_string()),
            sequence: Some(1),
            source: Source::PolymarketMarketWs,
            raw: json!({}),
        }
    }

    fn resolution_event(winner_asset_id: &str) -> Event {
        Event {
            v: 1,
            ts_ns: 2,
            received_ns: 2,
            event_type: EventType::Resolution,
            market_type: "btc_5m".to_string(),
            market_slug: Some("btc-up-or-down".to_string()),
            asset_id: None,
            side: None,
            price: None,
            size: None,
            sequence: Some(2),
            source: Source::Synthesizer,
            raw: json!({ "winner_asset_id": winner_asset_id }),
        }
    }

    fn buy_fill(asset_id: &str, price: f64, size: f64) -> SimulatedFill {
        SimulatedFill {
            client_order_id: format!("buy-{asset_id}"),
            asset_id: asset_id.to_string(),
            side: Side::Buy,
            price,
            size,
            fill_ms: 1,
            maker_or_taker: MakerOrTaker::Maker,
        }
    }

    #[test]
    fn accounting_marks_open_inventory_to_last_replayed_market_price() {
        let accounting = compute_accounting(
            &[mark_event("UP", "0.45")],
            &[buy_fill("UP", 0.40, 10.0)],
            1_000.0,
        );

        assert_eq!(accounting.starting_cash_usd, 1_000.0);
        assert_eq!(accounting.ending_cash_usd, 996.0);
        assert_eq!(accounting.market_value_usd, 4.5);
        assert_eq!(accounting.ending_equity_usd, 1_000.5);
        assert_eq!(accounting.total_pnl_usd, 0.5);
        assert_eq!(accounting.unrealized_pnl_usd, 0.5);
        assert_eq!(accounting.unmarked_open_positions, 0);
        assert_eq!(accounting.mark_source, "last_replayed_market_price");
    }

    #[test]
    fn accounting_uses_resolution_winner_for_redeemable_value() {
        let events = vec![
            mark_event("UP", "0.01"),
            mark_event("DOWN", "0.99"),
            resolution_event("UP"),
        ];
        let fills = vec![buy_fill("UP", 0.40, 10.0), buy_fill("DOWN", 0.55, 10.0)];
        let accounting = compute_accounting(&events, &fills, 1_000.0);

        assert_eq!(accounting.ending_cash_usd, 990.5);
        assert_eq!(accounting.redeemable_value_usd, 10.0);
        assert_eq!(accounting.market_value_usd, 10.0);
        assert_eq!(accounting.ending_equity_usd, 1_000.5);
        assert_eq!(accounting.total_pnl_usd, 0.5);
        assert_eq!(
            accounting.resolution_winner_asset_id,
            Some("UP".to_string())
        );
        assert_eq!(accounting.mark_source, "resolution");
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplayAccountingSummary {
    pub starting_cash_usd: f64,
    pub ending_cash_usd: f64,
    pub market_value_usd: f64,
    pub ending_equity_usd: f64,
    pub total_pnl_usd: f64,
    pub realized_pnl_usd: f64,
    pub unrealized_pnl_usd: f64,
    pub fees_paid_usd: f64,
    pub gross_fill_notional_usd: f64,
    pub buy_notional_usd: f64,
    pub sell_notional_usd: f64,
    pub redeemable_value_usd: f64,
    pub resolution_winner_asset_id: Option<String>,
    pub mark_source: String,
    pub unmarked_open_positions: u64,
    pub open_positions: Vec<ReplayPositionSummary>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplayPositionSummary {
    pub asset_id: String,
    pub qty: f64,
    pub avg_cost_usd: f64,
    pub cost_basis_usd: f64,
    pub mark_price_usd: Option<f64>,
    pub market_value_usd: f64,
    pub unrealized_pnl_usd: f64,
}

#[derive(Debug, Clone, Default)]
struct ReplayPositionAccounting {
    qty: f64,
    avg_cost: f64,
}

impl ReplayPositionAccounting {
    fn buy(&mut self, price: f64, size: f64) {
        let existing_cost = self.qty * self.avg_cost;
        self.qty += size;
        self.avg_cost = if self.qty > 0.0 {
            (existing_cost + price * size) / self.qty
        } else {
            0.0
        };
    }

    fn sell(&mut self, size: f64) -> f64 {
        let closed = size.min(self.qty.max(0.0));
        self.qty = (self.qty - size).max(0.0);
        if self.qty <= f64::EPSILON {
            self.avg_cost = 0.0;
        }
        closed
    }
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
    /// Risk-engine rejections produced while filtering this decision.
    /// Carried up to the runner for trace-strand emission.
    pub risk_rejections: Vec<RiskRejection>,
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
    /// Starting cash used only for replay accounting. Strategy behavior is
    /// still driven by its own profile/runtime state; this field makes the
    /// emitted equity line explicit and reproducible.
    pub starting_cash_usd: f64,
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
        let mut synthesizer = EventSynthesizer::new();
        let mut intents_submitted: u64 = 0;
        let mut risk_rejections: Vec<RiskRejection> = Vec::new();
        for event in events {
            // Synthesize any window-open / window-close markers triggered
            // by this event and dispatch them through the strategy and
            // simulator FIRST so the strategy sees, e.g., `price_to_beat`
            // before the `market_meta` it derives from. The synthesizer
            // is a pure function of prior events (no clock, no RNG) so
            // determinism holds.
            let synthetic_events = synthesizer.on_event(event);
            for synth in &synthetic_events {
                dispatch_event(
                    strategy,
                    &mut sim,
                    synth,
                    &mut intents_submitted,
                    &mut risk_rejections,
                );
            }
            dispatch_event(
                strategy,
                &mut sim,
                event,
                &mut intents_submitted,
                &mut risk_rejections,
            );
        }
        (
            sim.fills().to_vec(),
            sim.rejections().to_vec(),
            intents_submitted,
            risk_rejections,
            compute_accounting(events, sim.fills(), cfg.starting_cash_usd),
        )
    }));

    match outcome {
        Ok((fills, post_only_rejections, intents_submitted, risk_rejections, accounting)) => {
            WindowSummary {
                window_id: cfg.window_id.clone(),
                events_replayed: events.len() as u64,
                intents_submitted,
                fills,
                accounting,
                post_only_rejections,
                risk_rejections,
                status: WindowStatus::Ok,
            }
        }
        Err(_panic) => WindowSummary {
            window_id: cfg.window_id.clone(),
            events_replayed: events.len() as u64,
            intents_submitted: 0,
            fills: Vec::new(),
            accounting: ReplayAccountingSummary {
                starting_cash_usd: cfg.starting_cash_usd,
                ending_cash_usd: cfg.starting_cash_usd,
                ending_equity_usd: cfg.starting_cash_usd,
                mark_source: "panic_no_fills".to_string(),
                ..ReplayAccountingSummary::default()
            },
            post_only_rejections: Vec::new(),
            risk_rejections: Vec::new(),
            status: WindowStatus::Panicked,
        },
    }
}

fn compute_accounting(
    events: &[Event],
    fills: &[SimulatedFill],
    starting_cash_usd: f64,
) -> ReplayAccountingSummary {
    let mut cash = starting_cash_usd;
    let mut realized_pnl = 0.0;
    let mut buy_notional = 0.0;
    let mut sell_notional = 0.0;
    let mut positions: BTreeMap<String, ReplayPositionAccounting> = BTreeMap::new();

    for fill in fills {
        let notional = fill.price * fill.size;
        let pos = positions.entry(fill.asset_id.clone()).or_default();
        match fill.side {
            Side::Buy => {
                cash -= notional;
                buy_notional += notional;
                pos.buy(fill.price, fill.size);
            }
            Side::Sell => {
                cash += notional;
                sell_notional += notional;
                let avg_cost = pos.avg_cost;
                let closed = pos.sell(fill.size);
                realized_pnl += (fill.price - avg_cost) * closed;
            }
        }
    }

    let (marks, winner_asset_id) = replay_marks(events);
    let mut market_value = 0.0;
    let mut open_cost_basis = 0.0;
    let mut redeemable_value = 0.0;
    let mut unmarked_open_positions = 0u64;
    let mut open_positions = Vec::new();

    for (asset_id, pos) in positions {
        if pos.qty <= f64::EPSILON {
            continue;
        }
        let cost_basis = pos.qty * pos.avg_cost;
        let mark = if let Some(winner) = winner_asset_id.as_deref() {
            Some(if winner == asset_id { 1.0 } else { 0.0 })
        } else {
            marks.get(&asset_id).copied()
        };
        let value = mark.map(|p| p * pos.qty).unwrap_or(0.0);
        if mark.is_none() {
            unmarked_open_positions += 1;
        }
        if winner_asset_id.as_deref() == Some(asset_id.as_str()) {
            redeemable_value += value;
        }
        market_value += value;
        open_cost_basis += cost_basis;
        open_positions.push(ReplayPositionSummary {
            asset_id,
            qty: pos.qty,
            avg_cost_usd: pos.avg_cost,
            cost_basis_usd: cost_basis,
            mark_price_usd: mark,
            market_value_usd: value,
            unrealized_pnl_usd: value - cost_basis,
        });
    }

    let ending_equity = cash + market_value;
    let unrealized_pnl = market_value - open_cost_basis;
    ReplayAccountingSummary {
        starting_cash_usd,
        ending_cash_usd: cash,
        market_value_usd: market_value,
        ending_equity_usd: ending_equity,
        total_pnl_usd: ending_equity - starting_cash_usd,
        realized_pnl_usd: realized_pnl,
        unrealized_pnl_usd: unrealized_pnl,
        fees_paid_usd: 0.0,
        gross_fill_notional_usd: buy_notional + sell_notional,
        buy_notional_usd: buy_notional,
        sell_notional_usd: sell_notional,
        redeemable_value_usd: redeemable_value,
        resolution_winner_asset_id: winner_asset_id.clone(),
        mark_source: if winner_asset_id.is_some() {
            "resolution".to_string()
        } else {
            "last_replayed_market_price".to_string()
        },
        unmarked_open_positions,
        open_positions,
    }
}

fn replay_marks(events: &[Event]) -> (BTreeMap<String, f64>, Option<String>) {
    let mut marks = BTreeMap::new();
    let mut winner_asset_id = None;
    for event in events {
        if matches!(
            event.event_type,
            EventType::BookSnapshot | EventType::BookDelta | EventType::Trade
        ) {
            if let (Some(asset_id), Some(price)) = (
                event.asset_id.as_deref(),
                parse_price(event.price.as_deref()),
            ) {
                marks.insert(asset_id.to_string(), price);
            }
        }
        if event.event_type == EventType::Resolution {
            winner_asset_id = resolution_winner_asset_id(event).or_else(|| event.asset_id.clone());
        }
    }
    (marks, winner_asset_id)
}

fn parse_price(value: Option<&str>) -> Option<f64> {
    let parsed = value?.parse::<f64>().ok()?;
    parsed.is_finite().then_some(parsed)
}

fn resolution_winner_asset_id(event: &Event) -> Option<String> {
    for key in [
        "winner_asset_id",
        "winning_asset_id",
        "resolved_asset_id",
        "asset_id",
    ] {
        if let Some(value) = event.raw.get(key).and_then(|v| v.as_str()) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Dispatch a single `event` (real or synthetic) through the strategy
/// and simulator. Hoisted out of `run_window` so the synthesizer's
/// derived events flow through the same code path as real ones.
fn dispatch_event<S: ReplayStrategy>(
    strategy: &mut S,
    sim: &mut FillSimulator,
    event: &Event,
    intents_submitted: &mut u64,
    risk_rejections: &mut Vec<RiskRejection>,
) {
    let mut decision = strategy.on_event(event);
    risk_rejections.append(&mut decision.risk_rejections);
    for intent in decision.submits {
        sim.submit(intent);
        *intents_submitted += 1;
    }
    let event_ms = (event.received_ns / 1_000_000) as u64;
    for coid in decision.cancels {
        sim.cancel(&coid, event_ms);
    }
    let fills_before = sim.fills().len();
    sim.on_event(event);
    let fills_after = sim.fills().len();
    #[allow(clippy::unnecessary_to_owned)]
    let new_fills = sim.fills()[fills_before..fills_after].to_vec();
    for fill in new_fills {
        let mut decision = strategy.on_fill(&fill);
        risk_rejections.append(&mut decision.risk_rejections);
        for intent in decision.submits {
            sim.submit(intent);
            *intents_submitted += 1;
        }
        for coid in decision.cancels {
            sim.cancel(&coid, event_ms);
        }
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

    fn evt(
        received_ns: i64,
        et: EventType,
        asset: &str,
        side: &str,
        price: &str,
        size: &str,
    ) -> Event {
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
                    submits: vec![StrategyOrderIntent::passive(
                        "passive-1",
                        event.asset_id.clone().unwrap_or_default(),
                        Side::Sell,
                        0.55,
                        100.0,
                        (event.received_ns / 1_000_000) as u64,
                    )],
                    cancels: vec![],
                    risk_rejections: vec![],
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
            evt(
                1_000_000_000,
                EventType::BookSnapshot,
                "asset-a",
                "buy",
                "0.55",
                "100",
            ),
            evt(
                2_000_000_000,
                EventType::Trade,
                "asset-a",
                "buy",
                "0.55",
                "60",
            ),
            evt(
                3_000_000_000,
                EventType::Trade,
                "asset-a",
                "buy",
                "0.55",
                "40",
            ),
        ];
        let cfg = RunnerConfig {
            window_id: "w1".into(),
            fill_sim: FillSimConfig {
                latency: LatencyPreset::Instant,
                ..Default::default()
            },
            max_window_failures: 0,
            starting_cash_usd: 1_000.0,
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
            evt(
                1_000_000_000,
                EventType::BookDelta,
                "asset-a",
                "buy",
                "0.5",
                "10",
            ),
            evt(
                2_000_000_000,
                EventType::Trade,
                "asset-a",
                "buy",
                "0.5",
                "10",
            ),
        ];
        let cfg = RunnerConfig {
            window_id: "w-bad".into(),
            fill_sim: FillSimConfig::default(),
            max_window_failures: 0,
            starting_cash_usd: 1_000.0,
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
            vec![evt(
                1_000_000_000,
                EventType::BookDelta,
                "a",
                "buy",
                "0.5",
                "1",
            )],
        );
        windows.insert(
            "b".to_string(),
            vec![evt(
                2_000_000_000,
                EventType::BookDelta,
                "a",
                "buy",
                "0.5",
                "1",
            )],
        );
        let cfg = RunnerConfig {
            window_id: String::new(),
            fill_sim: FillSimConfig::default(),
            max_window_failures: 0,
            starting_cash_usd: 1_000.0,
        };
        let result = run_run(windows, &cfg, |_| PanicAfter(1));
        assert!(result.is_err());
    }

    #[test]
    fn deterministic_runs_produce_identical_summaries() {
        // Same inputs twice → byte-identical fills vector.
        let events = vec![
            evt(
                1_000_000_000,
                EventType::BookSnapshot,
                "asset-a",
                "buy",
                "0.55",
                "100",
            ),
            evt(
                2_000_000_000,
                EventType::Trade,
                "asset-a",
                "buy",
                "0.55",
                "60",
            ),
            evt(
                3_000_000_000,
                EventType::Trade,
                "asset-a",
                "buy",
                "0.55",
                "40",
            ),
        ];
        let cfg = RunnerConfig {
            window_id: "w1".into(),
            fill_sim: FillSimConfig {
                latency: LatencyPreset::Instant,
                ..Default::default()
            },
            max_window_failures: 0,
            starting_cash_usd: 1_000.0,
        };
        let mut s1 = PassiveAskStrategy {
            placed: false,
            on_fill_count: 0,
        };
        let mut s2 = PassiveAskStrategy {
            placed: false,
            on_fill_count: 0,
        };
        let r1 = run_window(&mut s1, &events, &cfg);
        let r2 = run_window(&mut s2, &events, &cfg);
        assert_eq!(r1, r2);
    }
}
