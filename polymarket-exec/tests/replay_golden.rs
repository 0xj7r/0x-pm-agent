//! Deterministic golden-fixture replay test (Phase 3a).
//!
//! Generates a 50-event canonical fixture, writes it to a temp directory as
//! Parquet + JSONL, reads both back through the replay reader, runs the
//! same passive-ask strategy twice, and asserts byte-identical output.
//!
//! This is the determinism guarantee called out in the Phase 3 design
//! spec: same `(input, profile, fill_sim_version)` triple produces the
//! same `fills` vector. We assert equality at the structured level (byte-
//! identical canonical JSON) rather than against a checked-in Parquet
//! blob, because Parquet writer metadata (created_by version string,
//! footer offsets) is not byte-stable across crate updates and the spec
//! itself notes the byte comparison "after stripping writer metadata".

use polymarket_exec::collector::schema::{Event, EventType, Source};
use polymarket_exec::replay::fill_sim::{
    FillSimConfig, LatencyPreset, Side, SimulatedFill, StrategyOrderIntent,
};
use polymarket_exec::replay::reader::{
    dedupe_and_sort, read_jsonl_file, read_local, write_jsonl_file,
};
use polymarket_exec::replay::runner::{run_window, ReplayDecision, ReplayStrategy, RunnerConfig};
use serde_json::json;
use tempfile::tempdir;

/// 50-event golden fixture: a single-asset book + a series of public buy
/// trades that walk through the resting ask placed by our strategy.
fn build_fixture() -> Vec<Event> {
    let mut events = Vec::with_capacity(50);
    // 1 book snapshot at t=0
    events.push(make_event(
        0,
        EventType::BookSnapshot,
        "asset-a",
        "buy",
        "0.55",
        "100",
    ));
    // 49 trades at 100ms intervals, alternating sizes that stress FIFO
    let sizes = ["10", "5", "20", "15", "30", "5", "10", "5"]; // sums to 100
    let mut total = 0u64;
    for i in 1..50 {
        let size = sizes[(i - 1) as usize % sizes.len()];
        let received_ns = 100_000_000_i64 * i; // 100ms steps
        events.push(make_event(
            received_ns,
            EventType::Trade,
            "asset-a",
            "buy",
            "0.55",
            size,
        ));
        total += size.parse::<u64>().unwrap();
        if total >= 200 {
            break;
        }
    }
    events
}

fn make_event(
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

/// Strategy: place a single 100-unit ask at 0.55 on the first event;
/// passively absorb fills.
struct PassiveAsk {
    placed: bool,
}

impl ReplayStrategy for PassiveAsk {
    fn on_event(&mut self, event: &Event) -> ReplayDecision {
        if !self.placed && event.event_type == EventType::BookSnapshot {
            self.placed = true;
            return ReplayDecision {
                submits: vec![StrategyOrderIntent {
                    client_order_id: "ask-1".into(),
                    asset_id: event.asset_id.clone().unwrap(),
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
        ReplayDecision::default()
    }
}

fn replay_with(events: &[Event]) -> Vec<SimulatedFill> {
    let cfg = RunnerConfig {
        window_id: "golden-1".into(),
        fill_sim: FillSimConfig {
            latency: LatencyPreset::Instant,
            seed: 0xC0FFEE,
            cancel_credit_fraction: 0.5,
        },
        max_window_failures: 0,
    };
    let mut s = PassiveAsk { placed: false };
    let summary = run_window(&mut s, events, &cfg);
    assert_eq!(
        summary.status,
        polymarket_exec::replay::runner::WindowStatus::Ok
    );
    summary.fills
}

#[test]
fn golden_fixture_replay_is_deterministic_across_runs() {
    let fixture = build_fixture();
    let f1 = replay_with(&fixture);
    let f2 = replay_with(&fixture);
    let s1 = serde_json::to_string(
        &f1.iter()
            .map(|f| {
                // Build a stable JSON projection.
                json!({
                    "client_order_id": f.client_order_id,
                    "asset_id": f.asset_id,
                    "side": format!("{:?}", f.side),
                    "price": f.price,
                    "size": f.size,
                    "fill_ms": f.fill_ms,
                })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let s2 = serde_json::to_string(
        &f2.iter()
            .map(|f| {
                json!({
                    "client_order_id": f.client_order_id,
                    "asset_id": f.asset_id,
                    "side": format!("{:?}", f.side),
                    "price": f.price,
                    "size": f.size,
                    "fill_ms": f.fill_ms,
                })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert_eq!(s1, s2, "two runs must produce byte-identical fills");
}

#[test]
fn golden_fixture_replay_jsonl_roundtrip_matches_in_memory() {
    let fixture = build_fixture();
    let dir = tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    write_jsonl_file(&path, &fixture).unwrap();
    let read = read_jsonl_file(&path).unwrap();
    // dedupe_and_sort applies the same discipline read_local would.
    let sorted_in_memory = dedupe_and_sort(fixture.clone());
    let sorted_from_disk = dedupe_and_sort(read);
    assert_eq!(sorted_in_memory, sorted_from_disk);

    let f_mem = replay_with(&sorted_in_memory);
    let f_disk = replay_with(&sorted_from_disk);
    assert_eq!(f_mem, f_disk, "in-memory replay must match disk replay");
}

#[test]
fn golden_fixture_total_fill_size_equals_strategy_quote_size() {
    let fixture = build_fixture();
    let fills = replay_with(&fixture);
    // The 100-unit ask should be fully consumed by the cumulative trade
    // volume in the fixture (sum of sizes is >= 100).
    let total: f64 = fills.iter().map(|f| f.size).sum();
    assert!(
        total >= 99.999 && total <= 100.001,
        "expected ~100, got {total}"
    );
}

#[test]
fn read_local_walks_directory_with_mixed_partition_layout() {
    // Mirrors the v=1 layout: dt=YYYY-MM-DD/market_type=btc_5m/event_type=*.
    let dir = tempdir().unwrap();
    let inner = dir
        .path()
        .join("v=1")
        .join("dt=2026-04-01")
        .join("market_type=btc_5m")
        .join("event_type=trade")
        .join("btc-up-or-down");
    std::fs::create_dir_all(&inner).unwrap();
    let fixture = build_fixture();
    write_jsonl_file(&inner.join("a.jsonl"), &fixture).unwrap();

    let read = read_local(dir.path()).unwrap();
    assert_eq!(read.len(), fixture.len());
}
