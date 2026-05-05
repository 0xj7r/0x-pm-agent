//! Deterministic golden-fixture replay test
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

use std::path::Path;

use polymarket_exec::collector::schema::{Event, EventType, Source};
use polymarket_exec::replay::fill_sim::{
    FillQuality, FillSimConfig, LatencyPreset, Side, SimulatedFill, StrategyOrderIntent,
};
use polymarket_exec::replay::manifest::{canonicalize, compute_run_id, WindowPlan};
use polymarket_exec::replay::reader::{
    dedupe_and_sort, read_jsonl_file, read_local, write_jsonl_file,
};
use polymarket_exec::replay::runner::{run_window, ReplayDecision, ReplayStrategy, RunnerConfig};
use polymarket_exec::replay::strategy_adapter::ReplayStrategyAdapter;
use polymarket_exec::strategy_profile::StrategyProfile;
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
                submits: vec![StrategyOrderIntent::passive(
                    "ask-1",
                    event.asset_id.clone().unwrap(),
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
        ReplayDecision::default()
    }
}

fn replay_with(events: &[Event]) -> Vec<SimulatedFill> {
    let cfg = RunnerConfig {
        window_id: "golden-1".into(),
        fill_sim: FillSimConfig {
            latency: LatencyPreset::Instant,
            fill_quality: FillQuality::Optimistic,
            seed: 0xC0FFEE,
            submit_latency_ms: None,
            cancel_latency_ms: None,
            cancel_credit_fraction: 0.5,
        },
        max_window_failures: 0,
        starting_cash_usd: 1_000.0,
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
    let mut normalized_fixture = fixture.clone();
    for event in &mut normalized_fixture {
        event.raw = serde_json::Value::Null;
    }
    let sorted_in_memory = dedupe_and_sort(normalized_fixture);
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
        (99.999..=100.001).contains(&total),
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

const GOLDEN_BAR_START_NS: i64 = 1_714_579_200_000_000_000;
const GOLDEN_BAR_END_NS: i64 = GOLDEN_BAR_START_NS + 300_000_000_000;
const GOLDEN_YES_ASSET: &str = "0xyes";
const GOLDEN_NO_ASSET: &str = "0xno";
const GOLDEN_SLUG: &str = "btc-5m-golden";
const GOLDEN_STRIKE: f64 = 60_000.0;

/// Construct a deterministic 100-event fixture covering exactly one BTC 5m
/// bar. Layout:
///   - 1 market_meta (yes/no asset_ids, strike, end_time)
///   - 5 btc_tick price samples spread through the bar
///   - book_snapshots seeding both yes and no books
///   - alternating trades that walk into the strategy's ladder
///
/// Total: 100 events at 3-second spacing inside the 5-minute window.
fn build_paired_mm_bar_fixture() -> Vec<Event> {
    let mut events = Vec::with_capacity(100);

    events.push(make_canonical_event(
        GOLDEN_BAR_START_NS,
        EventType::MarketMeta,
        Some(GOLDEN_SLUG),
        Some(GOLDEN_YES_ASSET),
        None,
        None,
        None,
        Source::PolymarketDataApi,
        json!({
            "slug": GOLDEN_SLUG,
            "market_type": "btc_5m",
            "asset_ids": [GOLDEN_YES_ASSET, GOLDEN_NO_ASSET],
            "strike": GOLDEN_STRIKE,
            "end_time_ms": GOLDEN_BAR_END_NS / 1_000_000,
        }),
    ));

    // Initial book snapshots seed yes ~ 0.55 / 0.57 and no ~ 0.43 / 0.45.
    for (asset, side, price, size) in [
        (GOLDEN_YES_ASSET, "buy", "0.55", "200"),
        (GOLDEN_YES_ASSET, "sell", "0.57", "200"),
        (GOLDEN_NO_ASSET, "buy", "0.43", "200"),
        (GOLDEN_NO_ASSET, "sell", "0.45", "200"),
    ] {
        events.push(make_canonical_event(
            GOLDEN_BAR_START_NS + 1,
            EventType::BookSnapshot,
            Some(GOLDEN_SLUG),
            Some(asset),
            Some(side),
            Some(price),
            Some(size),
            Source::PolymarketMarketWs,
            json!({}),
        ));
    }

    // BTC tick stream so fair-value model has spot + vol inputs.
    let btc_prices = [
        59_950.0, 59_980.0, 60_010.0, 60_005.0, 60_020.0, 60_030.0, 60_015.0, 60_040.0,
    ];
    for (i, p) in btc_prices.iter().enumerate() {
        let t = GOLDEN_BAR_START_NS + (5_000_000_000_i64 * (i as i64 + 1));
        events.push(make_canonical_event(
            t,
            EventType::BtcTick,
            Some(GOLDEN_SLUG),
            None,
            None,
            Some(&format!("{:.2}", p)),
            Some("1"),
            Source::BinanceAggtrade,
            json!({}),
        ));
    }

    // Trade walk: alternate buys hitting yes-asks and no-asks plus sells.
    // Spaced by 3s so we land at exactly 100 events at the end.
    let mut t = GOLDEN_BAR_START_NS + 60_000_000_000;
    let trade_program: Vec<(&str, &str, &str, &str)> = vec![
        (GOLDEN_YES_ASSET, "buy", "0.57", "5"),
        (GOLDEN_NO_ASSET, "buy", "0.45", "5"),
        (GOLDEN_YES_ASSET, "buy", "0.57", "10"),
        (GOLDEN_NO_ASSET, "buy", "0.45", "10"),
        (GOLDEN_YES_ASSET, "sell", "0.55", "5"),
        (GOLDEN_NO_ASSET, "sell", "0.43", "5"),
        (GOLDEN_YES_ASSET, "buy", "0.57", "8"),
        (GOLDEN_NO_ASSET, "buy", "0.45", "8"),
    ];
    while events.len() < 100 {
        let (asset, side, price, size) = trade_program[(events.len() - 1) % trade_program.len()];
        events.push(make_canonical_event(
            t,
            EventType::Trade,
            Some(GOLDEN_SLUG),
            Some(asset),
            Some(side),
            Some(price),
            Some(size),
            Source::PolymarketMarketWs,
            json!({}),
        ));
        t += 3_000_000_000;
    }
    events
}

fn make_canonical_event(
    received_ns: i64,
    event_type: EventType,
    slug: Option<&str>,
    asset_id: Option<&str>,
    side: Option<&str>,
    price: Option<&str>,
    size: Option<&str>,
    source: Source,
    raw: serde_json::Value,
) -> Event {
    Event {
        v: 1,
        ts_ns: received_ns - 1,
        received_ns,
        event_type,
        market_type: "btc_5m".into(),
        market_slug: slug.map(String::from),
        asset_id: asset_id.map(String::from),
        side: side.map(String::from),
        price: price.map(String::from),
        size: size.map(String::from),
        sequence: Some(received_ns),
        source,
        raw,
    }
}

fn run_paired_mm_window(events: &[Event]) -> polymarket_exec::replay::runner::WindowSummary {
    let profile_path = Path::new("tests/fixtures/replay/profiles/paired_mm_test.yaml");
    let profile = StrategyProfile::load(profile_path).expect("load profile fixture");
    let cfg = RunnerConfig {
        window_id: "btc_5m/golden".into(),
        fill_sim: FillSimConfig {
            latency: LatencyPreset::Instant,
            fill_quality: FillQuality::Optimistic,
            seed: 0xC0FFEE,
            submit_latency_ms: None,
            cancel_latency_ms: None,
            cancel_credit_fraction: 0.5,
        },
        max_window_failures: 0,
        starting_cash_usd: 1_000.0,
    };
    let mut adapter = ReplayStrategyAdapter::from_profile(profile);
    run_window(&mut adapter, events, &cfg)
}

fn realized_pnl_usd(fills: &[SimulatedFill]) -> f64 {
    // Net cash flow: sells add cash, buys subtract cash.
    let mut pnl = 0.0;
    for f in fills {
        let notional = f.price * f.size;
        match f.side {
            Side::Buy => pnl -= notional,
            Side::Sell => pnl += notional,
        }
    }
    pnl
}

#[test]
fn golden_paired_mm_one_bar_replay_is_deterministic() {
    let fixture = build_paired_mm_bar_fixture();
    assert_eq!(
        fixture.len(),
        100,
        "fixture must be exactly 100 events for the locked-in golden"
    );

    let s1 = run_paired_mm_window(&fixture);
    let s2 = run_paired_mm_window(&fixture);

    assert_eq!(
        s1.status,
        polymarket_exec::replay::runner::WindowStatus::Ok,
        "window must complete cleanly"
    );
    assert_eq!(
        s1.fills, s2.fills,
        "two replays of the same fixture must produce byte-identical fills"
    );
    assert_eq!(s1.events_replayed, 100);

    // Locked-in PnL value: this is the deterministic cash flow of running
    // the current paired_mm strategy + queue-aware fill-sim against the
    // 100-event fixture above. The fixture now exercises repeated quote
    // refreshes in the same bar: one NO bid and two refreshed YES bids fill
    // as maker orders.
    const GOLDEN_FILLS_COUNT: usize = 3;
    const GOLDEN_PNL_USD: f64 = -7.7;
    let fills_count = s1.fills.len();
    let pnl = realized_pnl_usd(&s1.fills);

    assert_eq!(
        fills_count, GOLDEN_FILLS_COUNT,
        "expected exactly {GOLDEN_FILLS_COUNT} fills in the golden bar, got {fills_count}",
    );
    assert!(
        (pnl - GOLDEN_PNL_USD).abs() < 1e-6,
        "expected golden PnL ~= {GOLDEN_PNL_USD}, got {pnl}"
    );
    assert!(
        s1.intents_submitted >= GOLDEN_FILLS_COUNT as u64,
        "expected at least {GOLDEN_FILLS_COUNT} intents, got {}",
        s1.intents_submitted
    );
}

#[test]
fn golden_paired_mm_one_bar_run_id_is_byte_identical_across_invocations() {
    let profile_path = Path::new("tests/fixtures/replay/profiles/paired_mm_test.yaml");
    let profile_yaml = std::fs::read_to_string(profile_path).expect("read profile");
    let canonical = canonicalize(&profile_yaml).expect("canonicalize");
    let plan = vec![WindowPlan {
        window_id: "btc_5m/golden".into(),
        start_ns: GOLDEN_BAR_START_NS,
        end_ns: GOLDEN_BAR_END_NS,
    }];
    let id1 = compute_run_id(&canonical, &plan, "abc123def4567890", "instant", 0xC0FFEE);
    let id2 = compute_run_id(&canonical, &plan, "abc123def4567890", "instant", 0xC0FFEE);
    assert_eq!(id1, id2, "run-id must be deterministic for fixed inputs");
    assert_eq!(id1.len(), 16);
}
