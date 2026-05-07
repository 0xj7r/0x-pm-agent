//! Replay determinism property test.
//!
//! Asserts byte-identical Parquet journal output across two replays of the
//! same fixture through the live strategy adapter. The adapter is the path
//! that exercises `runtime_state.open_orders()` (a `HashMap`) feeding
//! floating-point sums in `RiskContext`; without the audit fix, HashMap
//! rehash order can flip risk decisions and produce different journals
//! across runs of bit-identical input.
//!
//! Two layers of comparison:
//!   1. structured `JournalEvent` Vec equality across two `run_run`
//!      invocations -- catches divergence at the in-memory layer.
//!   2. raw bytes of `write_journal_parquet` output -- catches divergence
//!      at the Parquet writer layer (writer metadata, footer offsets,
//!      column ordering).
//!
//! The fixture is intentionally small (one bar, ~120 events). Runs in
//! well under 5 seconds.

use std::collections::BTreeMap;
use std::path::Path;

use polymarket_exec::collector::schema::{Event, EventType, Source};
use polymarket_exec::replay::fill_sim::{FillQuality, FillSimConfig, LatencyPreset};
use polymarket_exec::replay::journal::{write_journal_parquet, JournalEvent};
use polymarket_exec::replay::runner::{run_run, RunnerConfig, WindowStatus};
use polymarket_exec::replay::strategy_adapter::ReplayStrategyAdapter;
use polymarket_exec::strategy_profile::StrategyProfile;
use serde_json::json;
use tempfile::tempdir;

const BAR_START_NS: i64 = 1_714_579_200_000_000_000;
const BAR_END_NS: i64 = BAR_START_NS + 300_000_000_000;
const SLUG: &str = "btc-5m-determinism";
const YES_ASSET: &str = "0xyes-det";
const NO_ASSET: &str = "0xno-det";
const STRIKE: f64 = 60_000.0;

fn make_event(
    received_ns: i64,
    event_type: EventType,
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
        market_slug: Some(SLUG.into()),
        asset_id: asset_id.map(String::from),
        side: side.map(String::from),
        price: price.map(String::from),
        size: size.map(String::from),
        sequence: Some(received_ns),
        source,
        raw,
    }
}

/// Builds a deterministic, slightly wider fixture than the existing golden:
/// enough quote refresh + fill traffic that the strategy adapter touches
/// `runtime_state.open_orders()` repeatedly, exposing any HashMap iteration
/// order leak through the journal.
fn build_fixture() -> Vec<Event> {
    let mut events = Vec::with_capacity(120);

    events.push(make_event(
        BAR_START_NS,
        EventType::MarketMeta,
        None,
        None,
        None,
        None,
        Source::PolymarketDataApi,
        json!({
            "slug": SLUG,
            "market_type": "btc_5m",
            "asset_ids": [YES_ASSET, NO_ASSET],
            "strike": STRIKE,
            "end_time_ms": BAR_END_NS / 1_000_000,
        }),
    ));

    for (asset, side, price, size) in [
        (YES_ASSET, "buy", "0.55", "200"),
        (YES_ASSET, "sell", "0.57", "200"),
        (NO_ASSET, "buy", "0.43", "200"),
        (NO_ASSET, "sell", "0.45", "200"),
    ] {
        events.push(make_event(
            BAR_START_NS + 1,
            EventType::BookSnapshot,
            Some(asset),
            Some(side),
            Some(price),
            Some(size),
            Source::PolymarketMarketWs,
            json!({}),
        ));
    }

    let btc_prices = [
        59_950.0, 59_980.0, 60_010.0, 60_005.0, 60_020.0, 60_030.0, 60_015.0, 60_040.0,
    ];
    for (i, p) in btc_prices.iter().enumerate() {
        let t = BAR_START_NS + (5_000_000_000_i64 * (i as i64 + 1));
        events.push(make_event(
            t,
            EventType::BtcTick,
            None,
            None,
            Some(&format!("{:.2}", p)),
            Some("1"),
            Source::BinanceAggtrade,
            json!({}),
        ));
    }

    let mut t = BAR_START_NS + 60_000_000_000;
    let trade_program: Vec<(&str, &str, &str, &str)> = vec![
        (YES_ASSET, "buy", "0.57", "5"),
        (NO_ASSET, "buy", "0.45", "5"),
        (YES_ASSET, "buy", "0.57", "10"),
        (NO_ASSET, "buy", "0.45", "10"),
        (YES_ASSET, "sell", "0.55", "5"),
        (NO_ASSET, "sell", "0.43", "5"),
        (YES_ASSET, "buy", "0.57", "8"),
        (NO_ASSET, "buy", "0.45", "8"),
    ];
    while events.len() < 120 {
        let (asset, side, price, size) = trade_program[(events.len() - 1) % trade_program.len()];
        events.push(make_event(
            t,
            EventType::Trade,
            Some(asset),
            Some(side),
            Some(price),
            Some(size),
            Source::PolymarketMarketWs,
            json!({}),
        ));
        t += 2_000_000_000;
    }

    events
}

fn run_once() -> Vec<JournalEvent> {
    let profile_path = Path::new("tests/fixtures/replay/profiles/paired_mm_test.yaml");
    let profile = StrategyProfile::load(profile_path).expect("load profile fixture");
    let cfg = RunnerConfig {
        window_id: String::new(),
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

    let mut windows: BTreeMap<String, Vec<Event>> = BTreeMap::new();
    windows.insert("btc_5m/det".into(), build_fixture());

    let summaries = run_run(windows, &cfg, |_window_id, starting_cash_usd| {
        ReplayStrategyAdapter::from_profile(profile.clone())
            .with_starting_cash_usd(starting_cash_usd)
    })
    .expect("run_run should not abort");

    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].status, WindowStatus::Ok);

    summaries
        .into_iter()
        .flat_map(|s| s.journal_events.into_iter())
        .collect()
}

#[test]
fn byte_identical_journal_in_memory() {
    let a = run_once();
    let b = run_once();
    assert_eq!(
        a.len(),
        b.len(),
        "journal event count must match across runs ({} vs {})",
        a.len(),
        b.len()
    );
    assert_eq!(
        a, b,
        "two replays of the same fixture must produce byte-identical JournalEvent vectors"
    );
    assert!(
        !a.is_empty(),
        "fixture must produce at least one journal event; otherwise the test would be vacuous"
    );
}

#[test]
fn byte_identical_journal_parquet() {
    let dir = tempdir().expect("tempdir");
    let a_path = dir.path().join("a.parquet");
    let b_path = dir.path().join("b.parquet");

    let a_events = run_once();
    let b_events = run_once();
    assert_eq!(a_events, b_events, "in-memory journal events must match");

    write_journal_parquet(&a_path, &a_events).expect("write a.parquet");
    write_journal_parquet(&b_path, &b_events).expect("write b.parquet");

    let a_bytes = std::fs::read(&a_path).expect("read a.parquet");
    let b_bytes = std::fs::read(&b_path).expect("read b.parquet");

    assert_eq!(
        a_bytes.len(),
        b_bytes.len(),
        "journal Parquet byte length must match across runs ({} vs {})",
        a_bytes.len(),
        b_bytes.len()
    );
    assert_eq!(
        a_bytes, b_bytes,
        "journal Parquet output must be byte-identical across two replays of the same fixture"
    );
}
