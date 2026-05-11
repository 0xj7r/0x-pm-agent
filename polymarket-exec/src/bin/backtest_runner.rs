//! backtest_runner: deterministic replay of captured Phase 1 events through
//! the live strategy engine.
//!
//! Implements the CLI surface, content-addressed run-id
//! derivation, and a local-filesystem replay path. S3 reads/writes and the
//! RDS upsert are deferred to Phase 3b (Terraform + Batch); this binary
//! supports `--input-prefix file://...` and `--output-prefix file://...`
//! today which is sufficient to validate determinism end-to-end before the
//! infrastructure lands.
//!
//! # Live/replay parity
//!
//! Verified-correct (May 2026):
//! - The runner consumes the same YAML the live trader does
//!   (`StrategyProfile::load` is the single loader; the live runtime calls
//!   it from `config/mod.rs` and we call it from `run_main`). There is no
//!   bespoke replay-only profile schema.
//! - `ReplayStrategyAdapter::register_market_if_needed` calls
//!   `StrategyRegistry::register_paired_mm` with `profile.paired_mm_config()`,
//!   the same entry point the live trader uses. There is no replay-only
//!   strategy fork.
//! - The strategy crate has no `cfg!(test)` or `if replay {}` divergence
//!   branches; all `#[cfg(test)]` blocks are isolated test modules at the
//!   bottom of each file.
//! - Replay clocks advance using `event.received_ns / 1_000_000`, which is
//!   the local-collector receipt time. Per `polymarket-research/docs/
//!   telonex-reference.md`, this is the only timestamp that lives in a
//!   single clock domain (the collector hosts) and therefore the only one
//!   safe for cross-exchange ordering. The Polymarket exchange-emit
//!   `timestamp_us` (mapped to `event.ts_ns`) and the Binance matching-
//!   engine clock would otherwise drift independently.
//! - `OrderIntent.kind` (Entry vs Close) drives both `aggressive` and
//!   `post_only` flags on the simulator submission, mirroring the venue
//!   request the live trader sends.
//!
//! Exit codes (per spec):
//!   0 ok
//!   2 config error
//!   3 input not found
//!   4 partial (>= 1 window failed but under threshold)
//!   5 unrecoverable

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

use polymarket_exec::collector::schema::{Event, EventType};
use polymarket_exec::replay::fill_sim::{FillQuality, FillSimConfig, LatencyPreset, SimulatedFill};
use polymarket_exec::replay::journal::{write_journal_parquet, JournalEvent};
use polymarket_exec::replay::manifest::{
    canonicalize, compute_run_id, profile_hash, InputWindowDigest, Manifest, WindowPlan,
    FILL_SIM_VERSION, SCHEMA_VERSION,
};
use polymarket_exec::replay::raw_parquet::{read_raw_replay, RawReplayMarket, RawReplayOptions};
use polymarket_exec::replay::reader::{dedupe_and_sort, read_local_filtered};
use polymarket_exec::replay::runner::{
    run_run_parallel_with_journal_mode, run_run_with_journal_mode, run_window_with_journal_mode,
    ReplayJournalMode, RunnerConfig, WindowStatus, WindowSummary,
};
use polymarket_exec::replay::strategy_adapter::ReplayStrategyAdapter;
use polymarket_exec::replay::tape::events::{read_tape_replay_windows, TapeReplayOptions};
use polymarket_exec::strategy_profile::StrategyProfile;
use sha2::{Digest, Sha256};

/// Backtest CLI. Mirrors the spec's clap::Parser shape.
#[derive(Parser, Debug)]
#[command(
    name = "backtest_runner",
    version,
    about = "Phase 3a deterministic backtest runner"
)]
struct Cli {
    /// RFC3339 inclusive start of the replay window.
    #[arg(long)]
    window_start: String,

    /// RFC3339 exclusive end of the replay window.
    #[arg(long)]
    window_end: String,

    /// Path or s3:// URI of the strategy profile YAML.
    #[arg(long)]
    strategy_profile: PathBuf,

    /// Comma-separated market_type filter (e.g. "btc_5m,eth_5m"). Empty = all.
    #[arg(long, default_value = "")]
    market_filter: String,

    /// Path or s3:// URI of input Parquet/JSONL prefix. s3:// not yet
    /// supported in Phase 3a; use a local path.
    #[arg(long)]
    input_prefix: PathBuf,

    /// Local canonical market metadata prefix. For `rust-event` this enriches
    /// discovered markets with `MarketMeta` rows. For `telonex-raw` this is
    /// required to prevent synthetic market reconstruction.
    #[arg(long)]
    metadata_prefix: Option<PathBuf>,

    /// Optional prefix containing raw Binance agg_trades parquets
    /// (e.g. s3://.../raw/binance/exchange=binance/channel=agg_trades/symbol=BTCUSDT/date=<dt>/
    /// synced locally). When set, the runner reads BTC spot trades from this
    /// prefix and feeds them as `btc_tick` events alongside the polymarket
    /// events so the BtcRegimeAggregator can populate `return_120s_bps` and
    /// the bonereaper late-cert spot gate can fire. Without this, replay
    /// data that lacks `btc_tick` events leaves the regime signal empty and
    /// blocks every directional intent.
    #[arg(long)]
    btc_tick_prefix: Option<PathBuf>,

    /// Input format. `rust-event` reads canonical Event v1 Parquet/JSONL.
    /// `telonex-raw` reads raw Telonex/Binance Parquet directly.
    /// `tape` reads packed binary files generated by `tape_convert`.
    #[arg(long, default_value = "rust-event")]
    input_format: String,

    /// Raw replay market map: slug=up_asset,down_asset[,strike]. Required for
    /// `--input-format telonex-raw` so each market has real metadata.
    #[arg(long = "raw-market-asset-map")]
    raw_market_asset_maps: Vec<String>,

    /// Book levels per side to decode from raw Telonex book snapshots.
    #[arg(long, default_value_t = 25)]
    raw_max_book_levels: usize,

    /// Output prefix for manifest + per-window summaries.
    #[arg(long)]
    output_prefix: PathBuf,

    /// Optional Secrets Manager ARN for RDS DSN. Phase 3b only.
    #[arg(long)]
    rds_dsn_secret: Option<String>,

    /// Forty-hex-char SHA. Hashed into the run-id.
    #[arg(long)]
    git_rev: String,

    /// "auto" derives the run-id from the inputs; an explicit value is
    /// allowed only with --force-run-id (escape hatch).
    #[arg(long, default_value = "auto")]
    run_id: String,

    /// Override the auto-derived run-id. Use with care.
    #[arg(long, default_value_t = false)]
    force_run_id: bool,

    /// Per-window parallelism. >1 runs windows in parallel.
    #[arg(long, default_value_t = 1)]
    concurrency: usize,

    /// Run each window independently without post-hoc cross-window cash carry.
    #[arg(long, default_value_t = false)]
    independent_windows: bool,

    /// Named latency preset.
    #[arg(long, default_value = "nominal")]
    fill_config: String,

    /// Optional explicit submit latency override in milliseconds. This
    /// lets replay mirror vendor static latency models such as
    /// base_latency_ms + insert_latency_ms without adding another preset.
    #[arg(long)]
    submit_latency_ms: Option<u64>,

    /// Optional explicit cancel latency override in milliseconds. This
    /// lets replay mirror vendor static latency models such as
    /// base_latency_ms + cancel_latency_ms without adding another preset.
    #[arg(long)]
    cancel_latency_ms: Option<u64>,

    /// Fill-quality regime: optimistic | base | conservative. Orthogonal
    /// to `--fill-config`. Defaults to `base` (the realistic regime).
    #[arg(long, default_value = "base")]
    fill_quality: String,

    /// 64-bit seed (hex or decimal).
    #[arg(long, default_value = "0xC0FFEE")]
    seed: String,

    /// Abort the run after this many failed windows.
    #[arg(long, default_value_t = 5)]
    max_window_failures: usize,

    /// Starting cash used for replay accounting/equity output.
    #[arg(long, default_value_t = 1_000.0)]
    starting_cash_usd: f64,

    /// Maker rebate in basis points credited per maker fill. Polymarket's
    /// dynamic-taker-fee redistribution paid us measurable USDC in 2026-04;
    /// /rebates/current shows ~10 bps blended on btc_5m_mm. Default 0
    /// preserves prior accounting.
    #[arg(long, default_value_t = 0.0)]
    maker_rebate_bps: f64,

    /// Replay journal persistence mode. `full` preserves audit-grade
    /// per-event rows; `none` skips runner journal accumulation/writes for
    /// faster large tape backtests.
    #[arg(long, value_enum, default_value = "full")]
    journal_mode: CliJournalMode,

    /// Plan + manifest only, skip replay.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliJournalMode {
    Full,
    None,
}

impl From<CliJournalMode> for ReplayJournalMode {
    fn from(value: CliJournalMode) -> Self {
        match value {
            CliJournalMode::Full => ReplayJournalMode::Full,
            CliJournalMode::None => ReplayJournalMode::None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct RunSummary {
    run_id: String,
    git_rev: String,
    schema_version: u32,
    fill_sim_version: String,
    fill_config: String,
    windows: Vec<CompactWindowSummary>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CompactWindowSummary {
    window_id: String,
    input_hash: String,
    events_replayed: u64,
    intents_submitted: u64,
    fill_count: usize,
    post_only_rejection_count: usize,
    risk_rejection_count: usize,
    accounting: polymarket_exec::replay::runner::ReplayAccountingSummary,
    status: WindowStatus,
}

impl From<&WindowSummary> for CompactWindowSummary {
    fn from(summary: &WindowSummary) -> Self {
        Self {
            window_id: summary.window_id.clone(),
            input_hash: summary.input_hash.clone(),
            events_replayed: summary.events_replayed,
            intents_submitted: summary.intents_submitted,
            fill_count: summary.fills.len(),
            post_only_rejection_count: summary.post_only_rejections.len(),
            risk_rejection_count: summary.risk_rejections.len(),
            accounting: summary.accounting.clone(),
            status: summary.status.clone(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MetricsSummary {
    run_id: String,
    fill_config: String,
    window_count: u64,
    events_replayed: u64,
    intents_submitted: u64,
    accepted_fills: u64,
    gross_fill_notional_usd: f64,
    ending_equity_usd_sum: f64,
    portfolio_starting_cash_usd: f64,
    portfolio_ending_cash_usd: f64,
    portfolio_ending_equity_usd: f64,
    portfolio_total_pnl_usd: f64,
    portfolio_max_drawdown_usd: f64,
    total_pnl_usd: f64,
    realized_pnl_usd: f64,
    unrealized_pnl_usd: f64,
    mean_pnl_per_window_usd: f64,
    std_pnl_per_window_usd: f64,
    sharpe_per_window: Option<f64>,
    sortino_per_window: Option<f64>,
    win_rate: f64,
    profit_factor: Option<f64>,
    max_drawdown_usd: f64,
    worst_window_pnl_usd: f64,
    best_window_pnl_usd: f64,
    negative_cash_windows: u64,
    risk_rejection_count: u64,
    risk_rejection_reasons: BTreeMap<String, u64>,
    post_only_rejection_count: u64,
    queue_calibration_sample_size: u64,
    queue_calibration_filled_orders: u64,
    queue_calibration_queue_miss_count: u64,
    queue_fill_rate: Option<f64>,
    queue_partial_fill_rate: Option<f64>,
    fees_paid_usd: f64,
    merge_fee_usd: f64,
    merge_gas_usd: f64,
    redeem_fee_usd: f64,
    redeem_gas_usd: f64,
    total_fees_and_gas_usd: f64,
    avg_fees_and_gas_per_fill_usd: Option<f64>,
    avg_exchange_fees_per_fill_usd: Option<f64>,
    avg_gas_per_fill_usd: Option<f64>,
    merge_attempted_count: u64,
    merge_success_count: u64,
    merged_pair_qty: f64,
    unmerged_pairable_qty: f64,
    stranded_qty_total: f64,
    stranded_cost_usd: f64,
    /// Cost basis of stranded inventory bucketed by attribution path
    /// (paired_mm / late_favorite_loading / cheap_tail_convexity / other).
    /// Lets the caller separate "MM imbalance leak" from "intentional late
    /// directional bet" instead of seeing only the totals.
    #[serde(default)]
    stranded_cost_by_path: BTreeMap<String, f64>,
    #[serde(default)]
    stranded_qty_by_path: BTreeMap<String, f64>,
    /// Cost basis of inventory that resolved as the losing side, bucketed by
    /// attribution path. Already counted negatively in realized_pnl_usd; this
    /// surfaces the gross spend.
    #[serde(default)]
    expired_losing_cost_by_path: BTreeMap<String, f64>,
    #[serde(default)]
    expired_losing_qty_by_path: BTreeMap<String, f64>,
    settlement_status_counts: BTreeMap<String, u64>,
    fill_path: BTreeMap<String, FillAttribution>,
    fill_path_leg: BTreeMap<String, FillAttribution>,
    fill_time_bucket: BTreeMap<String, FillAttribution>,
    fill_price_bucket: BTreeMap<String, FillAttribution>,
    windows: Vec<MetricsWindowSummary>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct FillAttribution {
    fills: u64,
    quantity: f64,
    notional_usd: f64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MetricsWindowSummary {
    window_id: String,
    events_replayed: u64,
    intents_submitted: u64,
    fills: u64,
    gross_fill_notional_usd: f64,
    total_pnl_usd: f64,
    realized_pnl_usd: f64,
    unrealized_pnl_usd: f64,
    ending_cash_usd: f64,
    ending_equity_usd: f64,
    risk_rejections: u64,
    settlement_status: String,
    settlement_status_reason: String,
    merge_attempted_count: u64,
    merge_success_count: u64,
    unmerged_pairable_qty: f64,
    unmerged_pairable_net_gain_usd: f64,
    stranded_qty_total: f64,
    stranded_cost_usd: f64,
    paired_ladder_notional_usd: f64,
    convex_overlay_notional_usd: f64,
}

fn parse_seed(s: &str) -> Result<u64> {
    if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(rest, 16).context("invalid hex seed")
    } else {
        s.parse::<u64>().context("invalid decimal seed")
    }
}

fn parse_fill_config(s: &str) -> Result<LatencyPreset> {
    match s.to_ascii_lowercase().as_str() {
        "instant" => Ok(LatencyPreset::Instant),
        "nominal" => Ok(LatencyPreset::Nominal),
        "conservative" => Ok(LatencyPreset::Conservative),
        other => anyhow::bail!("unknown fill-config preset: {other}"),
    }
}

fn parse_fill_quality(s: &str) -> Result<FillQuality> {
    match s.to_ascii_lowercase().as_str() {
        "optimistic" => Ok(FillQuality::Optimistic),
        "base" => Ok(FillQuality::Base),
        "conservative" => Ok(FillQuality::Conservative),
        other => anyhow::bail!("unknown fill-quality regime: {other}"),
    }
}

fn parse_market_filter(s: &str) -> Vec<String> {
    s.split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn parse_raw_market_asset_maps(values: &[String]) -> Result<Vec<RawReplayMarket>> {
    let mut markets = Vec::new();
    for raw in values {
        for item in raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let (slug, assets) = item.split_once('=').with_context(|| {
                format!("invalid --raw-market-asset-map {item}; expected slug=up_asset,down_asset")
            })?;
            let asset_ids: Vec<String> = assets
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect();
            if asset_ids.len() < 2 {
                anyhow::bail!("invalid --raw-market-asset-map {item}; expected two asset IDs");
            }
            markets.push(RawReplayMarket {
                slug: slug.trim().to_string(),
                asset_ids: [asset_ids[0].clone(), asset_ids[1].clone()],
                strike: asset_ids.get(2).cloned(),
            });
        }
    }
    Ok(markets)
}

fn event_matches_market_filter(event: &Event, market_filter: &[String]) -> bool {
    market_filter.is_empty()
        || market_filter.contains(&event.market_type)
        || event.market_type == "btc_ref"
        || event.market_type == "reference"
}

fn target_window_market_types(event: &Event, market_filter: &[String]) -> Vec<String> {
    if (event.market_type == "btc_ref" || event.market_type == "reference")
        && !market_filter.is_empty()
    {
        return market_filter.to_vec();
    }
    vec![event.market_type.clone()]
}

fn replay_window_ids(
    event: &Event,
    market_type: &str,
    dt: &str,
    target_market_window_ids: &[String],
) -> Vec<String> {
    if event.event_type == EventType::MarketMeta && !target_market_window_ids.is_empty() {
        if let Some(slug) = event
            .market_slug
            .as_deref()
            .filter(|slug| !slug.is_empty() && *slug != "btcusdt")
        {
            let suffix = format!("/{slug}");
            let routed = target_market_window_ids
                .iter()
                .filter(|window_id| {
                    window_id.starts_with(&format!("{market_type}/"))
                        && window_id.ends_with(&suffix)
                })
                .cloned()
                .collect::<Vec<_>>();
            if !routed.is_empty() {
                return routed;
            }
        }
        return Vec::new();
    }
    if (event.market_type == "btc_ref" || event.market_type == "reference")
        && !target_market_window_ids.is_empty()
    {
        if let Some(slug) = event
            .market_slug
            .as_deref()
            .filter(|slug| !slug.is_empty() && *slug != "btcusdt")
        {
            let owned_window = format!("{market_type}/{dt}/{slug}");
            if target_market_window_ids
                .iter()
                .any(|window_id| window_id == &owned_window)
            {
                return vec![owned_window];
            }
        }
        // Restrict tick fan-out to windows whose strike-derived
        // [start_ns, end_ns] interval covers the tick's received_ns.
        // Without this gate every BTC tick lands in every market window
        // (~400k ticks * ~300 markets = ~120M tick deliveries) which is
        // a 100x over-replication of the data the strategy actually needs.
        return target_market_window_ids
            .iter()
            .filter(|wid| btc5m_window_contains(wid, event.received_ns))
            .cloned()
            .collect();
    }
    vec![replay_window_id(event, market_type, dt)]
}

/// For a btc_5m window id of the form `btc_5m/<dt>/btc-updown-5m-<start_epoch_s>`,
/// derive the 5-minute market interval and check whether `received_ns` falls
/// inside an extended pre-open window covering the strategy's vol/return
/// lookback (45m max in `runtime::btc_signals`) plus the 5-minute trading
/// interval itself. Returns true (default-allow) for any window id that does
/// not follow the binary slug convention so non-btc_5m callers are not
/// silently filtered.
fn btc5m_window_contains(window_id: &str, received_ns: i64) -> bool {
    let Some(slug) = window_id.rsplit('/').next() else {
        return true;
    };
    let Some(strike_str) = slug.strip_prefix("btc-updown-5m-") else {
        return true;
    };
    let Ok(start_epoch_s) = strike_str.parse::<i64>() else {
        return true;
    };
    let market_start_ns = start_epoch_s.saturating_mul(1_000_000_000);
    // Lookback = 45 min vol/return retention + 5 min trade interval.
    // Without the 45 min warmup, realized_vol_5m / 15m read empty buffers
    // for the first half of the window and fair-value falls back to the
    // book-mid no-signal path.
    let start_ns = market_start_ns.saturating_sub(45 * 60 * 1_000_000_000);
    let end_ns = market_start_ns.saturating_add(5 * 60 * 1_000_000_000);
    received_ns >= start_ns && received_ns < end_ns
}

fn replay_window_id(event: &Event, market_type: &str, dt: &str) -> String {
    event
        .market_slug
        .as_ref()
        .filter(|slug| {
            market_type != "btc_ref" && market_type != "reference" && slug.as_str() != "btcusdt"
        })
        .map(|slug| format!("{market_type}/{dt}/{slug}"))
        .unwrap_or_else(|| format!("{market_type}/{dt}"))
}

fn group_events_into_windows(events: Vec<Event>, cli: &Cli) -> BTreeMap<String, Vec<Event>> {
    let market_filter = parse_market_filter(&cli.market_filter);
    let mut windows: BTreeMap<String, Vec<Event>> = BTreeMap::new();
    let mut target_market_window_ids = tape_target_market_window_ids(cli);
    // Discover market windows when the caller does not provide explicit targets.
    // This is required for both rust-event and telonex-raw so btc_ref/reference
    // ticks can be fanned into all active per-market windows before replay.
    if target_market_window_ids.is_empty() {
        let mut discovered: BTreeSet<String> = BTreeSet::new();
        for e in &events {
            if !event_matches_market_filter(e, &market_filter) {
                continue;
            }
            if matches!(e.market_type.as_str(), "btc_ref" | "reference") {
                continue;
            }
            if e.event_type == EventType::MarketMeta {
                continue;
            }
            let Some(slug) = e
                .market_slug
                .as_deref()
                .filter(|s| !s.is_empty() && *s != "btcusdt")
            else {
                continue;
            };
            let dt = chrono::Utc
                .timestamp_opt(e.received_ns / 1_000_000_000, 0)
                .single()
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "1970-01-01".to_string());
            for market_type in target_window_market_types(e, &market_filter) {
                discovered.insert(format!("{market_type}/{dt}/{slug}"));
            }
        }
        target_market_window_ids = discovered.into_iter().collect();
    }
    for e in events {
        if !event_matches_market_filter(&e, &market_filter) {
            continue;
        }
        let dt = chrono::Utc
            .timestamp_opt(e.received_ns / 1_000_000_000, 0)
            .single()
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "1970-01-01".to_string());
        for market_type in target_window_market_types(&e, &market_filter) {
            let window_ids = replay_window_ids(&e, &market_type, &dt, &target_market_window_ids);
            for window_id in window_ids {
                windows.entry(window_id).or_default().push(e.clone());
            }
        }
    }
    windows
}

fn join_market_metadata(
    mut events: Vec<Event>,
    metadata_prefix: &PathBuf,
    market_filter: &str,
) -> Result<Vec<Event>> {
    let market_filter = parse_market_filter(market_filter);
    let needed_slugs = discovered_market_slugs(&events, &market_filter);
    if needed_slugs.is_empty() {
        return Ok(events);
    }
    let metadata_events = read_local_filtered(metadata_prefix, None)
        .with_context(|| format!("reading metadata prefix {}", metadata_prefix.display()))?;
    let mut matched_slugs = BTreeSet::new();
    for event in metadata_events {
        if event.event_type != EventType::MarketMeta {
            continue;
        }
        if !event_matches_market_filter(&event, &market_filter) {
            continue;
        }
        let Some(slug) = event.market_slug.as_deref() else {
            continue;
        };
        if needed_slugs.contains(slug) {
            matched_slugs.insert(slug.to_string());
            events.push(event);
        }
    }
    let missing = needed_slugs
        .difference(&matched_slugs)
        .take(10)
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        anyhow::bail!(
            "metadata prefix {} is missing market_meta for {} discovered market slugs; sample: {}",
            metadata_prefix.display(),
            needed_slugs.len() - matched_slugs.len(),
            missing.join(", ")
        );
    }
    Ok(dedupe_and_sort(events))
}

fn filter_events_to_replay_window(events: Vec<Event>, start_ns: i64, end_ns: i64) -> Vec<Event> {
    events
        .into_iter()
        .filter(|event| {
            event.event_type == EventType::MarketMeta
                || (event.received_ns >= start_ns && event.received_ns < end_ns)
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
struct MarketWindowBounds {
    start_ns: i64,
    end_ns: i64,
}

fn filter_market_events_to_market_windows(events: Vec<Event>) -> Vec<Event> {
    let mut windows_by_slug = BTreeMap::new();
    for event in &events {
        if event.event_type != EventType::MarketMeta {
            continue;
        }
        let Some(slug) = event.market_slug.as_deref() else {
            continue;
        };
        let Some(end_time_ms) = raw_i64(&event.raw, &["end_time_ms", "window_end_ms"]) else {
            continue;
        };
        let start_time_ms = raw_i64(&event.raw, &["start_time_ms", "window_start_ms"])
            .unwrap_or_else(|| end_time_ms - market_window_ms(&event.market_type));
        if start_time_ms >= end_time_ms {
            continue;
        }
        windows_by_slug.insert(
            slug.to_string(),
            MarketWindowBounds {
                start_ns: start_time_ms.saturating_mul(1_000_000),
                end_ns: end_time_ms.saturating_mul(1_000_000),
            },
        );
    }
    events
        .into_iter()
        .filter(|event| {
            if !matches!(
                event.event_type,
                EventType::BookDelta | EventType::BookSnapshot | EventType::Trade
            ) {
                return true;
            }
            let Some(slug) = event.market_slug.as_deref() else {
                return true;
            };
            let window = windows_by_slug
                .get(slug)
                .copied()
                .or_else(|| infer_market_window_from_slug(slug, &event.market_type));
            let Some(window) = window else {
                return true;
            };
            event.received_ns >= window.start_ns && event.received_ns < window.end_ns
        })
        .collect()
}

fn raw_i64(raw: &serde_json::Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| {
        raw.get(*key).and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|s| s.parse::<i64>().ok()))
        })
    })
}

fn market_window_ms(market_type: &str) -> i64 {
    if market_type.contains("15m") {
        15 * 60 * 1_000
    } else {
        5 * 60 * 1_000
    }
}

fn infer_market_window_from_slug(slug: &str, market_type: &str) -> Option<MarketWindowBounds> {
    let start_s = slug.rsplit('-').next()?.parse::<i64>().ok()?;
    let start_time_ms = start_s.checked_mul(1_000)?;
    let end_time_ms = start_time_ms.checked_add(market_window_ms(market_type))?;
    Some(MarketWindowBounds {
        start_ns: start_time_ms.saturating_mul(1_000_000),
        end_ns: end_time_ms.saturating_mul(1_000_000),
    })
}

fn discovered_market_slugs(events: &[Event], market_filter: &[String]) -> BTreeSet<String> {
    let mut slugs = BTreeSet::new();
    for event in events {
        if !event_matches_market_filter(event, market_filter) {
            continue;
        }
        if event.event_type == EventType::MarketMeta {
            continue;
        }
        if matches!(event.market_type.as_str(), "btc_ref" | "reference") {
            continue;
        }
        if let Some(slug) = event
            .market_slug
            .as_deref()
            .filter(|slug| !slug.is_empty() && *slug != "btcusdt")
        {
            slugs.insert(slug.to_string());
        }
    }
    slugs
}

fn validate_rust_event_market_meta(windows: &BTreeMap<String, Vec<Event>>) -> Result<()> {
    let mut missing = Vec::new();
    for (window_id, events) in windows {
        if !window_id.contains('/') {
            continue;
        }
        let has_market_events = events.iter().any(|event| {
            !matches!(event.market_type.as_str(), "btc_ref" | "reference")
                && event
                    .market_slug
                    .as_deref()
                    .is_some_and(|slug| !slug.is_empty())
        });
        if !has_market_events {
            continue;
        }
        let has_market_meta = events
            .iter()
            .any(|event| event.event_type == EventType::MarketMeta);
        if !has_market_meta {
            missing.push(window_id.clone());
        }
    }
    if !missing.is_empty() {
        let sample = missing
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "rust-event input is missing market_meta for {} replay windows; sample: {}. Rebuild processed data from real market metadata before trusting this backtest.",
            missing.len(),
            sample
        );
    }
    Ok(())
}

fn validate_rust_event_market_meta_events(events: &[Event], market_filter: &str) -> Result<()> {
    let market_filter = parse_market_filter(market_filter);
    let needed_slugs = discovered_market_slugs(events, &market_filter);
    if needed_slugs.is_empty() {
        return Ok(());
    }
    let matched_slugs = events
        .iter()
        .filter(|event| event.event_type == EventType::MarketMeta)
        .filter(|event| event_matches_market_filter(event, &market_filter))
        .filter_map(|event| event.market_slug.clone())
        .collect::<BTreeSet<_>>();
    let missing = needed_slugs
        .difference(&matched_slugs)
        .take(10)
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        anyhow::bail!(
            "rust-event input is missing market_meta for {} discovered market slugs; sample: {}. Rebuild processed data from real market metadata before trusting this backtest.",
            needed_slugs.len() - matched_slugs.len(),
            missing.join(", ")
        );
    }
    Ok(())
}

fn profile_strategy_tokens(profile: &StrategyProfile) -> BTreeSet<String> {
    profile
        .strategy
        .as_deref()
        .unwrap_or_default()
        .split([',', '+'])
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

fn profile_requires_btc_ticks(profile: &StrategyProfile) -> bool {
    profile_strategy_tokens(profile).iter().any(|token| {
        matches!(
            token.as_str(),
            "bonereaper" | "bonereaper_mm" | "late_favorite_directional"
        )
    })
}

fn required_event_types(cli: &Cli, profile: &StrategyProfile) -> BTreeSet<&'static str> {
    let mut required = BTreeSet::from(["trade", "book_delta", "market_meta"]);
    if profile_requires_btc_ticks(profile) {
        required.insert("btc_tick");
    }
    if cli.input_format == "tape" {
        required.remove("market_meta");
    }
    required
}

fn event_type_key(event_type: &EventType) -> String {
    serde_json::to_value(event_type)
        .ok()
        .and_then(|value| value.as_str().map(ToString::to_string))
        .unwrap_or_else(|| format!("{event_type:?}").to_ascii_lowercase())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn summarize_input_windows(
    windows: &BTreeMap<String, Vec<Event>>,
    required_types: &BTreeSet<&'static str>,
) -> Result<Vec<InputWindowDigest>> {
    windows
        .iter()
        .map(|(window_id, events)| {
            let mut event_type_counts = BTreeMap::new();
            let mut checksum_payload = Vec::new();
            for event in events {
                *event_type_counts
                    .entry(event_type_key(&event.event_type))
                    .or_insert(0) += 1;
                checksum_payload.extend(
                    serde_json::to_vec(event).context("serialize replay event for manifest checksum")?,
                );
                checksum_payload.push(b'\n');
            }
            let missing_required_event_types = required_types
                .iter()
                .filter(|event_type| !event_type_counts.contains_key(**event_type))
                .map(|event_type| (*event_type).to_string())
                .collect::<Vec<_>>();
            Ok(InputWindowDigest {
                window_id: window_id.clone(),
                total_events: events.len() as u64,
                event_type_counts,
                event_checksum_sha256: sha256_hex(&checksum_payload),
                missing_required_event_types,
            })
        })
        .collect()
}

fn validate_input_window_digests(digests: &[InputWindowDigest]) -> Result<()> {
    let incomplete = digests
        .iter()
        .filter(|digest| !digest.missing_required_event_types.is_empty())
        .map(|digest| {
            format!(
                "{} missing {}",
                digest.window_id,
                digest.missing_required_event_types.join(",")
            )
        })
        .take(10)
        .collect::<Vec<_>>();
    if incomplete.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "replay input failed preflight for {} window(s): {}. Use canonical processed shards with market_meta, and add --btc-tick-prefix when a late-favorite/bonereaper strategy needs spot ticks.",
        incomplete.len(),
        incomplete.join("; ")
    );
}

fn portfolio_windows_from_grouped(
    windows: &BTreeMap<String, Vec<Event>>,
    window_start: &str,
    window_end: &str,
) -> BTreeMap<String, Vec<Event>> {
    let events = windows
        .values()
        .flat_map(|events| events.iter().cloned())
        .collect::<Vec<_>>();
    BTreeMap::from([(
        portfolio_window_id(window_start, window_end),
        dedupe_and_sort(events),
    )])
}

fn portfolio_window_from_events(
    events: Vec<Event>,
    window_start: &str,
    window_end: &str,
) -> BTreeMap<String, Vec<Event>> {
    BTreeMap::from([(portfolio_window_id(window_start, window_end), events)])
}

fn portfolio_window_id(window_start: &str, window_end: &str) -> String {
    format!(
        "portfolio/{}__{}",
        sanitize_window_time(window_start),
        sanitize_window_time(window_end)
    )
}

fn sanitize_window_time(value: &str) -> String {
    value
        .replace(':', "")
        .replace('-', "")
        .trim_end_matches('Z')
        .to_string()
}

fn tape_target_market_window_ids(cli: &Cli) -> Vec<String> {
    if cli.input_format != "tape" {
        return Vec::new();
    }
    let Ok(markets) = parse_raw_market_asset_maps(&cli.raw_market_asset_maps) else {
        return Vec::new();
    };
    let market_type = if cli.market_filter.trim().is_empty() {
        "btc_5m".to_string()
    } else {
        cli.market_filter
            .split(',')
            .next()
            .unwrap_or("btc_5m")
            .trim()
            .to_string()
    };
    markets
        .into_iter()
        .filter_map(|market| {
            let seconds = market.slug.rsplit('-').next()?.parse::<i64>().ok()?;
            let dt = chrono::Utc
                .timestamp_opt(seconds, 0)
                .single()?
                .format("%Y-%m-%d")
                .to_string();
            Some(format!("{market_type}/{dt}/{}", market.slug))
        })
        .collect()
}

fn run_main(cli: Cli) -> Result<i32> {
    // Validate CLI inputs.
    let start_dt = DateTime::parse_from_rfc3339(&cli.window_start)
        .with_context(|| format!("invalid --window-start: {}", cli.window_start))?;
    let end_dt = DateTime::parse_from_rfc3339(&cli.window_end)
        .with_context(|| format!("invalid --window-end: {}", cli.window_end))?;
    if start_dt >= end_dt {
        anyhow::bail!("--window-start must be before --window-end");
    }
    if cli.concurrency > 1 && !cli.independent_windows {
        anyhow::bail!(
            "--concurrency > 1 is only valid with --independent-windows. Cash-compounded/live-style replay must run sequentially so strategy risk checks see the carried cash baseline."
        );
    }

    let preset = parse_fill_config(&cli.fill_config)?;
    let fill_quality = parse_fill_quality(&cli.fill_quality)?;
    let seed = parse_seed(&cli.seed)?;

    // Load + canonicalize profile.
    if !cli.strategy_profile.exists() {
        anyhow::bail!(
            "--strategy-profile not found: {}",
            cli.strategy_profile.display()
        );
    }
    let profile_yaml = fs::read_to_string(&cli.strategy_profile)
        .with_context(|| format!("read profile {}", cli.strategy_profile.display()))?;
    let canonical_profile = canonicalize(&profile_yaml)?;
    let p_hash = profile_hash(&canonical_profile);
    let profile = StrategyProfile::load(&cli.strategy_profile)
        .with_context(|| format!("load strategy profile {}", cli.strategy_profile.display()))?;
    let required_types = required_event_types(&cli, &profile);

    // Load events from input.
    if !cli.input_prefix.exists() {
        // Exit code 3 per spec.
        eprintln!("input prefix not found: {}", cli.input_prefix.display());
        return Ok(3);
    }
    let (mut windows, diagnostic_windows): (BTreeMap<String, Vec<Event>>, BTreeMap<String, Vec<Event>>) =
        match cli.input_format.as_str() {
        "rust-event" => {
            let mut events = read_local_filtered(&cli.input_prefix, None)
                .with_context(|| format!("reading input from {}", cli.input_prefix.display()))?;
            // Optional BTC spot tick injection. The canonical processed/v=1
            // shards historically omitted btc_tick rows, leaving the BTC
            // regime engine starved. Reading raw binance agg_trades from a
            // separate prefix and merging them as btc_tick events restores
            // the spot signal without rebuilding the processed shard.
            if let Some(btc_prefix) = cli.btc_tick_prefix.as_ref() {
                let btc_options = RawReplayOptions {
                    window_start_ns: start_dt
                        .timestamp_nanos_opt()
                        .context("invalid window_start nanoseconds")?,
                    window_end_ns: end_dt
                        .timestamp_nanos_opt()
                        .context("invalid window_end nanoseconds")?,
                    market_filter: String::new(),
                    max_book_levels: 0,
                    markets: Vec::new(),
                };
                let btc_events = read_raw_replay(btc_prefix, &btc_options).with_context(|| {
                    format!(
                        "reading btc-tick prefix {}",
                        btc_prefix.display()
                    )
                })?;
                eprintln!(
                    "backtest_runner: loaded {} btc_tick events from {}",
                    btc_events.len(),
                    btc_prefix.display()
                );
                events.extend(btc_events);
                events = dedupe_and_sort(events);
            }
            events = filter_events_to_replay_window(
                events,
                start_dt
                    .timestamp_nanos_opt()
                    .context("invalid window_start nanoseconds")?,
                end_dt
                    .timestamp_nanos_opt()
                    .context("invalid window_end nanoseconds")?,
            );
            if let Some(metadata_prefix) = cli.metadata_prefix.as_ref() {
                events = join_market_metadata(events, metadata_prefix, &cli.market_filter)?;
            }
            events = filter_market_events_to_market_windows(events);
            if cli.independent_windows {
                let windows = group_events_into_windows(events, &cli);
                validate_rust_event_market_meta(&windows)?;
                (windows.clone(), windows)
            } else {
                let diagnostic_windows = group_events_into_windows(events.clone(), &cli);
                validate_rust_event_market_meta_events(&events, &cli.market_filter)?;
                (
                    portfolio_window_from_events(events, &cli.window_start, &cli.window_end),
                    diagnostic_windows,
                )
            }
        }
        "telonex-raw" => {
            let markets = parse_raw_market_asset_maps(&cli.raw_market_asset_maps)?;
            if markets.is_empty() {
                anyhow::bail!("--input-format telonex-raw requires at least one --raw-market-asset-map slug=up_asset,down_asset");
            }
            if cli.metadata_prefix.is_none() {
                anyhow::bail!(
                    "--input-format telonex-raw requires --metadata-prefix to attach real MarketMeta"
                );
            }
            let mut events = read_raw_replay(
                &cli.input_prefix,
                &RawReplayOptions {
                    window_start_ns: start_dt
                        .timestamp_nanos_opt()
                        .context("invalid window_start nanoseconds")?,
                    window_end_ns: end_dt
                        .timestamp_nanos_opt()
                        .context("invalid window_end nanoseconds")?,
                    market_filter: cli.market_filter.clone(),
                    max_book_levels: cli.raw_max_book_levels,
                    markets,
                },
            )
            .with_context(|| {
                format!(
                    "reading raw Telonex input from {}",
                    cli.input_prefix.display()
                )
            })?;
            let metadata_prefix = cli
                .metadata_prefix
                .as_ref()
                .context("input_format=telonex-raw requires --metadata-prefix")?;
            events = join_market_metadata(events, metadata_prefix, &cli.market_filter)?;
            events = filter_market_events_to_market_windows(events);
            let windows = group_events_into_windows(events, &cli);
            (windows.clone(), windows)
        }
        "tape" => {
            let markets = parse_raw_market_asset_maps(&cli.raw_market_asset_maps)?;
            if markets.is_empty() {
                anyhow::bail!("--input-format tape requires --raw-market-asset-map slug=up_asset,down_asset[,strike]");
            }
            let windows = read_tape_replay_windows(&TapeReplayOptions {
                input_prefix: cli.input_prefix.clone(),
                window_start_ns: start_dt
                    .timestamp_nanos_opt()
                    .context("invalid window_start nanoseconds")?,
                window_end_ns: end_dt
                    .timestamp_nanos_opt()
                    .context("invalid window_end nanoseconds")?,
                market_filter: cli.market_filter.clone(),
                markets,
            })
            .with_context(|| {
                format!(
                    "reading packed tape input from {}",
                    cli.input_prefix.display()
                )
            })?;
            (windows.clone(), windows)
        }
        other => anyhow::bail!("unknown --input-format: {other}"),
    };
    let input_windows = summarize_input_windows(&diagnostic_windows, &required_types)?;
    validate_input_window_digests(&input_windows)?;
    if !cli.independent_windows && cli.input_format != "rust-event" {
        windows = portfolio_windows_from_grouped(&windows, &cli.window_start, &cli.window_end);
    }

    // Build window plan in deterministic order.
    let window_plans: Vec<WindowPlan> = windows
        .iter()
        .map(|(id, evs)| WindowPlan {
            window_id: id.clone(),
            start_ns: evs.first().map(|e| e.received_ns).unwrap_or(0),
            end_ns: evs.last().map(|e| e.received_ns).unwrap_or(0),
        })
        .collect();

    // Compute run-id. We hash BOTH `fill_config` (latency preset) and
    // `fill_quality` so two runs differing only on either knob produce
    // distinct run-ids.
    let combined_fill_config = format!(
        "{}+{}+submit_ms={:?}+cancel_ms={:?}+journal={:?}",
        cli.fill_config,
        cli.fill_quality,
        cli.submit_latency_ms,
        cli.cancel_latency_ms,
        cli.journal_mode
    );
    let derived_run_id = compute_run_id(
        &canonical_profile,
        &window_plans,
        &cli.git_rev,
        &combined_fill_config,
        seed,
    );
    let run_id = if cli.run_id == "auto" {
        derived_run_id.clone()
    } else if cli.force_run_id {
        cli.run_id.clone()
    } else if cli.run_id == derived_run_id {
        derived_run_id.clone()
    } else {
        anyhow::bail!(
            "explicit --run-id {} differs from derived {}; use --force-run-id to override",
            cli.run_id,
            derived_run_id
        );
    };

    let manifest = Manifest {
        run_id: run_id.clone(),
        git_rev: cli.git_rev.clone(),
        schema_version: SCHEMA_VERSION,
        fill_sim_version: FILL_SIM_VERSION.to_string(),
        profile_hash: p_hash,
        windows: window_plans,
        fill_config: combined_fill_config.clone(),
        seed: format!("0x{:016x}", seed),
        input_windows,
    };

    // Output: <output_prefix>/runs/run_id=<id>/manifest.json
    let run_root = cli.output_prefix.join(format!("runs/run_id={}", run_id));
    fs::create_dir_all(&run_root)
        .with_context(|| format!("create run root {}", run_root.display()))?;
    let manifest_path = run_root.join("manifest.json");
    fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("write manifest {}", manifest_path.display()))?;

    if cli.dry_run {
        println!("dry-run: manifest written to {}", manifest_path.display());
        return Ok(0);
    }

    // Replay.
    let runner_cfg = RunnerConfig {
        window_id: String::new(),
        fill_sim: FillSimConfig {
            latency: preset,
            fill_quality,
            seed,
            submit_latency_ms: cli.submit_latency_ms,
            cancel_latency_ms: cli.cancel_latency_ms,
            cancel_credit_fraction: 0.5,
        },
        max_window_failures: cli.max_window_failures,
        starting_cash_usd: cli.starting_cash_usd,
        maker_rebate_bps: cli.maker_rebate_bps,
    };

    // Load profile and instantiate the strategy adapter. Default replay is
    // portfolio-scoped: one adapter, one fill simulator, one wallet, one
    // chronological event stream. `--independent-windows` keeps the old
    // per-market-window sweep semantics for research comparisons only.
    let adapter_journal_enabled = cli.journal_mode == CliJournalMode::Full;
    let mut summaries = if !cli.independent_windows {
        let (window_id, events) = windows
            .into_iter()
            .next()
            .context("portfolio replay expected one window")?;
        let mut cfg = runner_cfg.clone();
        cfg.window_id = window_id.clone();
        let mut strategy = ReplayStrategyAdapter::from_profile(profile.clone())
            .with_starting_cash_usd(cli.starting_cash_usd)
            .with_journal_enabled(adapter_journal_enabled);
        vec![run_window_with_journal_mode(
            &mut strategy,
            &events,
            &cfg,
            cli.journal_mode.into(),
        )]
    } else if cli.concurrency <= 1 {
        match run_run_with_journal_mode(
            windows,
            &runner_cfg,
            cli.journal_mode.into(),
            |_, starting_cash_usd| {
                ReplayStrategyAdapter::from_profile(profile.clone())
                    .with_starting_cash_usd(starting_cash_usd)
                    .with_journal_enabled(adapter_journal_enabled)
            },
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("run aborted: {e}");
                return Ok(4);
            }
        }
    } else {
        match run_run_parallel_with_journal_mode(
            windows,
            &runner_cfg,
            cli.journal_mode.into(),
            cli.concurrency,
            |_, starting_cash_usd| {
                ReplayStrategyAdapter::from_profile(profile.clone())
                    .with_starting_cash_usd(starting_cash_usd)
                    .with_journal_enabled(adapter_journal_enabled)
            },
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("run aborted: {e}");
                return Ok(4);
            }
        }
    };

    // Write per-window summary as JSON. (Per-window Parquet output is a
    // forward-compatible Phase 3b enhancement — JSON is enough for the
    // determinism-validation contract today.)
    let windows_root = run_root.join("windows");
    fs::create_dir_all(&windows_root)?;
    let mut had_failure = false;
    let mut journal_events: Vec<JournalEvent> = Vec::new();
    for s in summaries.iter_mut() {
        if s.status != WindowStatus::Ok {
            had_failure = true;
        }
        let safe_id = s.window_id.replace('/', "_");
        let path = windows_root.join(format!("{safe_id}.json"));
        fs::write(
            &path,
            serde_json::to_string_pretty(&CompactWindowSummary::from(&*s))?,
        )
        .with_context(|| format!("write window summary {}", path.display()))?;
        // Drain the journal events into the run-level vector so each
        // window's contribution lands in `journal.parquet` exactly once.
        // The summary's `journal_events` is `skip_serializing` so the
        // JSON file does not double-record them.
        if cli.journal_mode == CliJournalMode::Full {
            journal_events.append(&mut std::mem::take(&mut s.journal_events));
        }
    }
    if cli.journal_mode == CliJournalMode::Full {
        let journal_path = run_root.join("journal.parquet");
        write_journal_parquet(&journal_path, &journal_events)
            .with_context(|| format!("write journal {}", journal_path.display()))?;
    }
    let journal_event_count = journal_events.len();

    let summary = RunSummary {
        run_id: run_id.clone(),
        git_rev: cli.git_rev,
        schema_version: SCHEMA_VERSION,
        fill_sim_version: FILL_SIM_VERSION.to_string(),
        fill_config: combined_fill_config.clone(),
        windows: summaries.iter().map(CompactWindowSummary::from).collect(),
    };
    let metrics = compute_metrics_summary(&run_id, &combined_fill_config, &summaries);
    fs::write(
        run_root.join("metrics_summary.json"),
        serde_json::to_string_pretty(&metrics)?,
    )?;
    fs::write(
        run_root.join("summary.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;

    println!(
        "run_id={} windows={} journal_events={}",
        run_id,
        summary.windows.len(),
        journal_event_count
    );
    polymarket_exec::market_making::paired_mm::engine::print_convex_overlay_gate_counts();
    Ok(if had_failure { 4 } else { 0 })
}

fn compute_metrics_summary(
    run_id: &str,
    fill_config: &str,
    windows: &[WindowSummary],
) -> MetricsSummary {
    let mut out = MetricsSummary {
        run_id: run_id.to_string(),
        fill_config: fill_config.to_string(),
        window_count: windows.len() as u64,
        worst_window_pnl_usd: f64::INFINITY,
        best_window_pnl_usd: f64::NEG_INFINITY,
        ..MetricsSummary::default()
    };
    let mut pnls = Vec::new();
    let mut cumulative_pnl = 0.0;
    let mut peak_pnl = 0.0;
    let mut gross_wins = 0.0;
    let mut gross_losses = 0.0;
    let mut queue_partial_fills = 0.0;

    for window in windows {
        let accounting = &window.accounting;
        let settlement = &accounting.settlement;
        let queue = &window.queue_calibration;
        let pnl = accounting.total_pnl_usd;
        if pnls.is_empty() {
            out.portfolio_starting_cash_usd = accounting.starting_cash_usd;
        }
        out.portfolio_ending_cash_usd = accounting.ending_cash_usd;
        out.portfolio_ending_equity_usd = accounting.ending_equity_usd;
        pnls.push(pnl);
        cumulative_pnl += pnl;
        if cumulative_pnl > peak_pnl {
            peak_pnl = cumulative_pnl;
        }
        out.max_drawdown_usd = out.max_drawdown_usd.max(peak_pnl - cumulative_pnl);
        out.portfolio_max_drawdown_usd = out.max_drawdown_usd;
        if pnl > 0.0 {
            gross_wins += pnl;
        } else if pnl < 0.0 {
            gross_losses += -pnl;
        }
        out.best_window_pnl_usd = out.best_window_pnl_usd.max(pnl);
        out.worst_window_pnl_usd = out.worst_window_pnl_usd.min(pnl);
        out.events_replayed += window.events_replayed;
        out.intents_submitted += window.intents_submitted;
        out.accepted_fills += window.fills.len() as u64;
        out.gross_fill_notional_usd += accounting.gross_fill_notional_usd;
        out.ending_equity_usd_sum += accounting.ending_equity_usd;
        out.total_pnl_usd += pnl;
        out.realized_pnl_usd += accounting.realized_pnl_usd;
        out.unrealized_pnl_usd += accounting.unrealized_pnl_usd;
        out.negative_cash_windows += (accounting.ending_cash_usd < -1e-9) as u64;
        out.risk_rejection_count += window.risk_rejections.len() as u64;
        for rejection in &window.risk_rejections {
            *out.risk_rejection_reasons
                .entry(risk_rejection_reason_key(rejection))
                .or_insert(0) += 1;
        }
        out.post_only_rejection_count += window.post_only_rejections.len() as u64;
        out.queue_calibration_sample_size += queue.sample_size;
        out.queue_calibration_filled_orders += queue.filled_orders;
        queue_partial_fills += queue.partial_fill_rate * queue.sample_size as f64;
        out.fees_paid_usd += accounting.fees_paid_usd;
        out.merge_fee_usd += settlement.merge_fee_usd;
        out.merge_gas_usd += settlement.merge_gas_usd;
        out.redeem_fee_usd += settlement.redeem_fee_usd;
        out.redeem_gas_usd += settlement.redeem_gas_usd;
        out.merge_attempted_count += settlement.merge_attempted_count;
        out.merge_success_count += settlement.merge_success_count;
        out.merged_pair_qty += settlement.merged_pair_qty;
        out.unmerged_pairable_qty += settlement.unmerged_pairable_qty;
        out.stranded_qty_total += settlement.stranded_qty_total;
        out.stranded_cost_usd += settlement.stranded_cost_usd;
        let attribution = &accounting.attribution;
        for (label, bucket) in [
            ("paired_mm", &attribution.paired_mm),
            ("late_favorite_loading", &attribution.late_favorite_loading),
            ("cheap_tail_convexity", &attribution.cheap_tail_convexity),
            ("other", &attribution.other),
        ] {
            if bucket.stranded_cost_usd != 0.0 || bucket.stranded_qty != 0.0 {
                *out.stranded_cost_by_path
                    .entry(label.to_string())
                    .or_insert(0.0) += bucket.stranded_cost_usd;
                *out.stranded_qty_by_path
                    .entry(label.to_string())
                    .or_insert(0.0) += bucket.stranded_qty;
            }
            if bucket.expired_losing_cost_usd != 0.0 || bucket.expired_losing_qty != 0.0 {
                *out.expired_losing_cost_by_path
                    .entry(label.to_string())
                    .or_insert(0.0) += bucket.expired_losing_cost_usd;
                *out.expired_losing_qty_by_path
                    .entry(label.to_string())
                    .or_insert(0.0) += bucket.expired_losing_qty;
            }
        }
        *out.settlement_status_counts
            .entry(settlement.status.clone())
            .or_insert(0) += 1;

        let mut paired_ladder_notional = 0.0;
        let mut convex_overlay_notional = 0.0;
        for fill in &window.fills {
            let path = fill_path(fill);
            let leg = fill_leg(fill);
            let notional = fill.price * fill.size;
            if path == "paired_ladder" {
                paired_ladder_notional += notional;
            } else if path == "convex_overlay" {
                convex_overlay_notional += notional;
            }
            add_fill_attr(&mut out.fill_path, path, fill);
            add_fill_attr(&mut out.fill_path_leg, &format!("{path}:{leg}"), fill);
            add_fill_attr(&mut out.fill_time_bucket, &fill_time_bucket(fill), fill);
            add_fill_attr(&mut out.fill_price_bucket, &fill_price_bucket(fill), fill);
        }
        out.windows.push(MetricsWindowSummary {
            window_id: window.window_id.clone(),
            events_replayed: window.events_replayed,
            intents_submitted: window.intents_submitted,
            fills: window.fills.len() as u64,
            gross_fill_notional_usd: accounting.gross_fill_notional_usd,
            total_pnl_usd: pnl,
            realized_pnl_usd: accounting.realized_pnl_usd,
            unrealized_pnl_usd: accounting.unrealized_pnl_usd,
            ending_cash_usd: accounting.ending_cash_usd,
            ending_equity_usd: accounting.ending_equity_usd,
            risk_rejections: window.risk_rejections.len() as u64,
            settlement_status: settlement.status.clone(),
            settlement_status_reason: settlement.status_reason.clone(),
            merge_attempted_count: settlement.merge_attempted_count,
            merge_success_count: settlement.merge_success_count,
            unmerged_pairable_qty: settlement.unmerged_pairable_qty,
            unmerged_pairable_net_gain_usd: settlement.unmerged_pairable_net_gain_usd,
            stranded_qty_total: settlement.stranded_qty_total,
            stranded_cost_usd: settlement.stranded_cost_usd,
            paired_ladder_notional_usd: paired_ladder_notional,
            convex_overlay_notional_usd: convex_overlay_notional,
        });
    }

    if out.window_count == 0 {
        out.worst_window_pnl_usd = 0.0;
        out.best_window_pnl_usd = 0.0;
        return out;
    }
    out.portfolio_total_pnl_usd = out.portfolio_ending_equity_usd - out.portfolio_starting_cash_usd;
    let n = out.window_count as f64;
    out.mean_pnl_per_window_usd = out.total_pnl_usd / n;
    let variance = pnls
        .iter()
        .map(|pnl| {
            let diff = pnl - out.mean_pnl_per_window_usd;
            diff * diff
        })
        .sum::<f64>()
        / n;
    out.std_pnl_per_window_usd = variance.sqrt();
    out.sharpe_per_window = (out.std_pnl_per_window_usd > 1e-12)
        .then_some(out.mean_pnl_per_window_usd / out.std_pnl_per_window_usd);
    let downside = pnls
        .iter()
        .filter(|pnl| **pnl < 0.0)
        .map(|pnl| pnl * pnl)
        .sum::<f64>();
    let downside_count = pnls.iter().filter(|pnl| **pnl < 0.0).count() as f64;
    out.sortino_per_window = (downside_count > 0.0)
        .then(|| (downside / downside_count).sqrt())
        .and_then(|downside_dev| {
            (downside_dev > 1e-12).then_some(out.mean_pnl_per_window_usd / downside_dev)
        });
    out.win_rate = pnls.iter().filter(|pnl| **pnl > 0.0).count() as f64 / n;
    out.profit_factor = if gross_losses > 1e-12 {
        Some(gross_wins / gross_losses)
    } else if gross_wins > 0.0 {
        Some(f64::INFINITY)
    } else {
        None
    };
    out.queue_calibration_queue_miss_count = out
        .queue_calibration_sample_size
        .saturating_sub(out.queue_calibration_filled_orders);
    if out.queue_calibration_sample_size > 0 {
        let sample_size = out.queue_calibration_sample_size as f64;
        out.queue_fill_rate = Some(out.queue_calibration_filled_orders as f64 / sample_size);
        out.queue_partial_fill_rate = Some(queue_partial_fills / sample_size);
    }
    out.total_fees_and_gas_usd = out.fees_paid_usd
        + out.merge_fee_usd
        + out.merge_gas_usd
        + out.redeem_fee_usd
        + out.redeem_gas_usd;
    if out.accepted_fills > 0 {
        let fills = out.accepted_fills as f64;
        out.avg_fees_and_gas_per_fill_usd = Some(out.total_fees_and_gas_usd / fills);
        out.avg_exchange_fees_per_fill_usd =
            Some((out.fees_paid_usd + out.merge_fee_usd + out.redeem_fee_usd) / fills);
        out.avg_gas_per_fill_usd = Some((out.merge_gas_usd + out.redeem_gas_usd) / fills);
    }
    out
}

fn risk_rejection_reason_key(
    rejection: &polymarket_exec::replay::risk_trace::RiskRejection,
) -> String {
    serde_json::to_value(&rejection.reject_reason)
        .ok()
        .and_then(|value| value.as_str().map(ToString::to_string))
        .unwrap_or_else(|| format!("{:?}", rejection.reject_reason))
}

fn add_fill_attr(map: &mut BTreeMap<String, FillAttribution>, key: &str, fill: &SimulatedFill) {
    let entry = map.entry(key.to_string()).or_default();
    entry.fills += 1;
    entry.quantity += fill.size;
    entry.notional_usd += fill.price * fill.size;
}

fn fill_path(fill: &SimulatedFill) -> &'static str {
    let coid = fill.client_order_id.as_str();
    if coid.contains("convex") {
        "convex_overlay"
    } else if coid.contains("capital-recycle") {
        "capital_recycle"
    } else if coid.contains("hedge-rescue") {
        "hedge_rescue"
    } else if coid.contains("paired-mm") || coid.contains("paired-bid") {
        "paired_ladder"
    } else {
        "other"
    }
}

fn fill_leg(fill: &SimulatedFill) -> &'static str {
    let coid = fill.client_order_id.as_str();
    if coid.contains(":yes:") {
        "yes"
    } else if coid.contains(":no:") {
        "no"
    } else {
        "unknown"
    }
}

fn fill_price_bucket(fill: &SimulatedFill) -> String {
    let price = fill.price;
    if price <= 0.02 {
        "tail_0_2c".to_string()
    } else if price <= 0.10 {
        "tail_2_10c".to_string()
    } else if price >= 0.95 {
        "favorite_95_100c".to_string()
    } else if price >= 0.85 {
        "favorite_85_95c".to_string()
    } else {
        "middle".to_string()
    }
}

fn fill_time_bucket(fill: &SimulatedFill) -> String {
    let Some(start_s) = market_epoch_from_client_order_id(&fill.client_order_id) else {
        return "unknown".to_string();
    };
    let phase_s = fill.fill_ms as i64 / 1_000 - start_s;
    match phase_s {
        i64::MIN..=-1 => "pre_window".to_string(),
        0..=59 => "000_060s".to_string(),
        60..=119 => "060_120s".to_string(),
        120..=179 => "120_180s".to_string(),
        180..=239 => "180_240s".to_string(),
        240..=300 => "240_300s".to_string(),
        _ => "post_window".to_string(),
    }
}

fn market_epoch_from_client_order_id(coid: &str) -> Option<i64> {
    coid.split(':')
        .find_map(|part| part.strip_prefix("btc-updown-5m-"))
        .and_then(|suffix| suffix.parse::<i64>().ok())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run_main(cli) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(5)),
        Err(e) => {
            eprintln!("backtest_runner: {e:#}");
            // CLI/profile parse → exit 2; everything else → exit 5.
            let msg = format!("{e:#}");
            if msg.contains("invalid --window-start")
                || msg.contains("invalid --window-end")
                || msg.contains("--strategy-profile not found")
                || msg.contains("invalid hex seed")
                || msg.contains("invalid decimal seed")
                || msg.contains("unknown fill-config preset")
                || msg.contains("--run-id")
            {
                ExitCode::from(2)
            } else {
                ExitCode::from(5)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use polymarket_exec::collector::schema::{EventType, Source};
    use polymarket_exec::replay::fill_sim::{MakerOrTaker, Side};
    use serde_json::json;

    fn event(market_type: &str) -> Event {
        Event {
            v: 1,
            ts_ns: 1,
            received_ns: 1,
            event_type: EventType::Heartbeat,
            market_type: market_type.to_string(),
            market_slug: None,
            asset_id: None,
            side: None,
            price: None,
            size: None,
            sequence: Some(1),
            source: Source::Collector,
            raw: json!({}),
        }
    }

    fn market_event(event_type: EventType) -> Event {
        let mut event = event("btc_5m");
        event.event_type = event_type;
        event.market_slug = Some("btc-updown-5m-1776047700".to_string());
        event
    }

    fn fill(client_order_id: &str) -> SimulatedFill {
        SimulatedFill {
            client_order_id: client_order_id.to_string(),
            asset_id: "asset".to_string(),
            side: Side::Buy,
            price: 0.5,
            size: 1.0,
            fill_ms: 0,
            maker_or_taker: MakerOrTaker::Maker,
        }
    }

    #[test]
    fn market_filter_keeps_reference_ticks_for_selected_crypto_market() {
        let filter = vec!["btc_5m".to_string()];

        assert!(event_matches_market_filter(&event("btc_5m"), &filter));
        assert!(event_matches_market_filter(&event("btc_ref"), &filter));
        assert!(event_matches_market_filter(&event("reference"), &filter));
        assert!(!event_matches_market_filter(&event("eth_5m"), &filter));
    }

    #[test]
    fn reference_ticks_are_grouped_into_selected_market_windows() {
        let filter = vec!["btc_5m".to_string()];

        assert_eq!(
            target_window_market_types(&event("btc_ref"), &filter),
            vec!["btc_5m".to_string()]
        );
        assert_eq!(
            target_window_market_types(&event("reference"), &filter),
            vec!["btc_5m".to_string()]
        );
    }

    #[test]
    fn market_events_are_grouped_by_slug_window() {
        let mut market_event = event("btc_5m");
        market_event.market_slug = Some("btc-updown-5m-1771119900".to_string());
        assert_eq!(
            replay_window_id(&market_event, "btc_5m", "2026-02-15"),
            "btc_5m/2026-02-15/btc-updown-5m-1771119900"
        );

        let mut btc_event = event("btc_ref");
        btc_event.market_slug = Some("btcusdt".to_string());
        assert_eq!(
            replay_window_id(&btc_event, "btc_5m", "2026-02-15"),
            "btc_5m/2026-02-15"
        );
    }

    #[test]
    fn reference_ticks_are_fanned_out_to_tape_market_windows() {
        let mut btc_event = event("btc_ref");
        btc_event.market_slug = Some("btcusdt".to_string());
        let targets = vec![
            "btc_5m/2026-02-15/btc-updown-5m-1".to_string(),
            "btc_5m/2026-02-15/btc-updown-5m-2".to_string(),
        ];
        assert_eq!(
            replay_window_ids(&btc_event, "btc_5m", "2026-02-15", &targets),
            targets
        );
    }

    #[test]
    fn owned_reference_ticks_route_to_their_market_window() {
        let mut btc_event = event("btc_ref");
        btc_event.market_slug = Some("btc-updown-5m-1".to_string());
        let targets = vec![
            "btc_5m/2026-02-15/btc-updown-5m-1".to_string(),
            "btc_5m/2026-02-15/btc-updown-5m-2".to_string(),
        ];
        assert_eq!(
            replay_window_ids(&btc_event, "btc_5m", "2026-02-15", &targets),
            vec!["btc_5m/2026-02-15/btc-updown-5m-1".to_string()]
        );
    }

    #[test]
    fn market_meta_routes_to_discovered_event_partition_for_same_slug() {
        let mut meta = market_event(EventType::MarketMeta);
        meta.received_ns = 1_776_127_500_000_000_000;
        meta.market_slug = Some("btc-updown-5m-1776127500".to_string());
        let targets = vec![
            "btc_5m/2026-04-13/btc-updown-5m-1776127500".to_string(),
            "btc_5m/2026-04-13/btc-updown-5m-1776038400".to_string(),
        ];

        assert_eq!(
            replay_window_ids(&meta, "btc_5m", "2026-04-14", &targets),
            vec!["btc_5m/2026-04-13/btc-updown-5m-1776127500".to_string()]
        );
    }

    #[test]
    fn unmatched_market_meta_does_not_create_metadata_only_window() {
        let mut meta = market_event(EventType::MarketMeta);
        meta.market_slug = Some("btc-updown-5m-1776127500".to_string());
        let targets = vec!["btc_5m/2026-04-13/btc-updown-5m-1776038400".to_string()];

        assert!(replay_window_ids(&meta, "btc_5m", "2026-04-14", &targets).is_empty());
    }

    #[test]
    fn portfolio_windows_dedupe_fanned_reference_events() {
        let mut windows = BTreeMap::new();
        let mut ref_tick = event("btc_ref");
        ref_tick.event_type = EventType::BtcTick;
        ref_tick.market_slug = Some("btcusdt".to_string());
        ref_tick.received_ns = 10;
        ref_tick.sequence = Some(10);
        let mut trade = market_event(EventType::Trade);
        trade.received_ns = 20;
        trade.sequence = Some(20);
        windows.insert(
            "btc_5m/2026-04-13/btc-updown-5m-1".to_string(),
            vec![ref_tick.clone(), trade.clone()],
        );
        windows.insert(
            "btc_5m/2026-04-13/btc-updown-5m-2".to_string(),
            vec![ref_tick.clone()],
        );

        let portfolio = portfolio_windows_from_grouped(
            &windows,
            "2026-04-13T00:00:00Z",
            "2026-04-14T00:00:00Z",
        );

        assert_eq!(portfolio.len(), 1);
        let (window_id, events) = portfolio.iter().next().unwrap();
        assert_eq!(window_id, "portfolio/20260413T000000__20260414T000000");
        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .any(|event| event.event_type == EventType::BtcTick));
        assert!(events
            .iter()
            .any(|event| event.event_type == EventType::Trade));
    }

    #[test]
    fn portfolio_window_from_events_preserves_canonical_stream_without_ref_fanout() {
        let mut ref_tick = event("btc_ref");
        ref_tick.event_type = EventType::BtcTick;
        ref_tick.market_slug = Some("btcusdt".to_string());
        ref_tick.received_ns = 10;
        ref_tick.sequence = Some(10);
        let mut trade = market_event(EventType::Trade);
        trade.received_ns = 20;
        trade.sequence = Some(20);

        let portfolio = portfolio_window_from_events(
            vec![ref_tick.clone(), trade.clone()],
            "2026-04-13T00:00:00Z",
            "2026-04-14T00:00:00Z",
        );

        let (window_id, events) = portfolio.iter().next().unwrap();
        assert_eq!(window_id, "portfolio/20260413T000000__20260414T000000");
        assert_eq!(events, &vec![ref_tick, trade]);
    }

    #[test]
    fn btc5m_window_contains_treats_slug_suffix_as_start_time() {
        let window_id = "btc_5m/2026-04-13/btc-updown-5m-1776127500";

        assert!(btc5m_window_contains(window_id, 1_776_127_500_000_000_000));
        assert!(btc5m_window_contains(window_id, 1_776_127_799_999_999_999));
        assert!(!btc5m_window_contains(window_id, 1_776_127_800_000_000_000));
    }

    #[test]
    fn rust_event_validation_rejects_market_windows_without_market_meta() {
        let mut windows = BTreeMap::new();
        windows.insert(
            "btc_5m/2026-04-13/btc-updown-5m-1776047700".to_string(),
            vec![market_event(EventType::Trade)],
        );

        let err = validate_rust_event_market_meta(&windows).unwrap_err();

        assert!(
            err.to_string().contains("missing market_meta"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rust_event_validation_accepts_registered_market_windows() {
        let mut windows = BTreeMap::new();
        windows.insert(
            "btc_5m/2026-04-13/btc-updown-5m-1776047700".to_string(),
            vec![
                market_event(EventType::MarketMeta),
                market_event(EventType::Trade),
            ],
        );

        validate_rust_event_market_meta(&windows).unwrap();
    }

    #[test]
    fn rust_event_stream_validation_rejects_market_events_without_market_meta() {
        let err =
            validate_rust_event_market_meta_events(&[market_event(EventType::Trade)], "btc_5m")
                .unwrap_err();

        assert!(
            err.to_string().contains("missing market_meta"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rust_event_stream_validation_accepts_registered_markets() {
        validate_rust_event_market_meta_events(
            &[
                market_event(EventType::MarketMeta),
                market_event(EventType::Trade),
            ],
            "btc_5m",
        )
        .unwrap();
    }

    #[test]
    fn discovered_market_slugs_ignores_reference_and_metadata_rows() {
        let filter = vec!["btc_5m".to_string()];
        let mut trade = market_event(EventType::Trade);
        trade.market_slug = Some("btc-updown-5m-1776047700".to_string());
        let mut meta = market_event(EventType::MarketMeta);
        meta.market_slug = Some("btc-updown-5m-1776048000".to_string());
        let mut reference = event("btc_ref");
        reference.market_slug = Some("btcusdt".to_string());

        assert_eq!(
            discovered_market_slugs(&[trade, meta, reference], &filter)
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["btc-updown-5m-1776047700".to_string()]
        );
    }

    #[test]
    fn rust_event_window_filter_keeps_metadata_outside_time_slice() {
        let mut early_meta = market_event(EventType::MarketMeta);
        early_meta.received_ns = 100;
        let mut in_window_trade = market_event(EventType::Trade);
        in_window_trade.received_ns = 1_500;
        let mut late_trade = market_event(EventType::Trade);
        late_trade.received_ns = 2_500;

        let kept = filter_events_to_replay_window(
            vec![early_meta.clone(), in_window_trade.clone(), late_trade],
            1_000,
            2_000,
        );

        assert_eq!(kept, vec![early_meta, in_window_trade]);
    }

    #[test]
    fn market_window_filter_drops_pre_start_and_post_end_market_events() {
        let mut meta = market_event(EventType::MarketMeta);
        meta.market_slug = Some("btc-updown-5m-1776127500".to_string());
        meta.raw = json!({
            "start_time_ms": 1_776_127_500_000i64,
            "end_time_ms": 1_776_127_800_000i64,
        });
        let mut before = market_event(EventType::BookDelta);
        before.market_slug = meta.market_slug.clone();
        before.received_ns = 1_776_127_499_999_000_000;
        let mut inside = market_event(EventType::Trade);
        inside.market_slug = meta.market_slug.clone();
        inside.received_ns = 1_776_127_600_000_000_000;
        let mut after = market_event(EventType::BookSnapshot);
        after.market_slug = meta.market_slug.clone();
        after.received_ns = 1_776_127_800_000_000_000;
        let mut ref_tick = event("btc_ref");
        ref_tick.event_type = EventType::BtcTick;
        ref_tick.received_ns = before.received_ns;

        let kept = filter_market_events_to_market_windows(vec![
            meta.clone(),
            before,
            inside.clone(),
            after,
            ref_tick.clone(),
        ]);

        assert_eq!(kept, vec![meta, inside, ref_tick]);
    }

    #[test]
    fn fill_path_separates_close_and_overlay_paths() {
        assert_eq!(
            fill_path(&fill("paired-mm:btc-updown-5m-1:yes:l1:1:1")),
            "paired_ladder"
        );
        assert_eq!(
            fill_path(&fill("paired-mm-convex:btc-updown-5m-1:yes:1:1")),
            "convex_overlay"
        );
        assert_eq!(
            fill_path(&fill("capital-recycle:btc-updown-5m-1:no:1:1")),
            "capital_recycle"
        );
        assert_eq!(
            fill_path(&fill("hedge-rescue:btc-updown-5m-1:no:1:1")),
            "hedge_rescue"
        );
    }

    #[test]
    fn parses_raw_market_asset_maps() {
        let markets = parse_raw_market_asset_maps(&[
            "btc-updown-5m-1771178400=UP,DOWN".to_string(),
            "btc-updown-5m-1771178700=UP2,DOWN2".to_string(),
        ])
        .unwrap();

        assert_eq!(markets.len(), 2);
        assert_eq!(markets[0].slug, "btc-updown-5m-1771178400");
        assert_eq!(markets[0].asset_ids, ["UP".to_string(), "DOWN".to_string()]);
        assert_eq!(markets[0].strike, None);
    }

    #[test]
    fn parses_raw_market_asset_maps_with_exact_strike() {
        let markets =
            parse_raw_market_asset_maps(&["btc-updown-5m-1771178400=UP,DOWN,79000.12".to_string()])
                .unwrap();

        assert_eq!(markets[0].strike.as_deref(), Some("79000.12"));
    }

    #[test]
    fn live_paired_mm_yaml_loads_through_runner_path() {
        // Live/replay parity guard: the runner MUST be able to load the
        // same YAML the live trader uses, and the resulting profile must
        // declare `paired_mm` as the active strategy with non-empty
        // ladder/risk knobs. Any drift between the YAML schema and what
        // `StrategyProfile::load` understands fails this test.
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("config/strategies/archive/btc_5m_paired_mm.live.yaml");
        let profile = StrategyProfile::load(&path).expect("load live YAML");
        assert_eq!(profile.strategy.as_deref(), Some("paired_mm"));
        let cfg = profile.paired_mm_config();
        assert!(
            cfg.ladder.max_depth >= 1,
            "ladder.max_depth must be >= 1; got {}",
            cfg.ladder.max_depth
        );
        assert!(cfg.ladder.base_clip_usd > 0.0);
        let limits = profile.risk_limits();
        assert!(limits.max_order_notional_usd > 0.0);
        assert!(limits.max_gross_notional_usd > 0.0);
        assert!(limits.max_open_orders_total > 0);
    }
}
