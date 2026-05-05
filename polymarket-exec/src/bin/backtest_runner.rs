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
//! Exit codes (per spec):
//!   0 ok
//!   2 config error
//!   3 input not found
//!   4 partial (>= 1 window failed but under threshold)
//!   5 unrecoverable

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone};
use clap::Parser;
use serde::{Deserialize, Serialize};

use polymarket_exec::collector::schema::Event;
use polymarket_exec::replay::fill_sim::{FillQuality, FillSimConfig, LatencyPreset};
use polymarket_exec::replay::manifest::{
    canonicalize, compute_run_id, profile_hash, Manifest, WindowPlan, FILL_SIM_VERSION,
    SCHEMA_VERSION,
};
use polymarket_exec::replay::reader::read_local_filtered;
use polymarket_exec::replay::runner::{run_run, RunnerConfig, WindowStatus, WindowSummary};
use polymarket_exec::replay::strategy_adapter::ReplayStrategyAdapter;
use polymarket_exec::strategy_profile::StrategyProfile;

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

    /// Per-window parallelism. Phase 3a runs windows sequentially (the
    /// flag is accepted for forward-compatibility).
    #[arg(long, default_value_t = 1)]
    concurrency: usize,

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

    /// Plan + manifest only, skip replay.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct RunSummary {
    run_id: String,
    git_rev: String,
    schema_version: u32,
    fill_sim_version: String,
    fill_config: String,
    windows: Vec<WindowSummary>,
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

fn run_main(cli: Cli) -> Result<i32> {
    // Validate CLI inputs.
    let _start_dt = DateTime::parse_from_rfc3339(&cli.window_start)
        .with_context(|| format!("invalid --window-start: {}", cli.window_start))?;
    let _end_dt = DateTime::parse_from_rfc3339(&cli.window_end)
        .with_context(|| format!("invalid --window-end: {}", cli.window_end))?;

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

    // Load events from input.
    if !cli.input_prefix.exists() {
        // Exit code 3 per spec.
        eprintln!("input prefix not found: {}", cli.input_prefix.display());
        return Ok(3);
    }
    let events: Vec<Event> = read_local_filtered(&cli.input_prefix, None)
        .with_context(|| format!("reading input from {}", cli.input_prefix.display()))?;

    // Group into windows by (market_type, dt). Phase 3a uses one window
    // per market_type (broader windowing per the spec is Phase 3b/5).
    let market_filter = parse_market_filter(&cli.market_filter);
    let mut windows: BTreeMap<String, Vec<Event>> = BTreeMap::new();
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
            let window_id = format!("{market_type}/{dt}");
            windows.entry(window_id).or_default().push(e.clone());
        }
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
        "{}+{}+submit_ms={:?}+cancel_ms={:?}",
        cli.fill_config, cli.fill_quality, cli.submit_latency_ms, cli.cancel_latency_ms
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
    };

    // Load profile and instantiate the strategy adapter for each window.
    let profile = StrategyProfile::load(&cli.strategy_profile)
        .with_context(|| format!("load strategy profile {}", cli.strategy_profile.display()))?;
    let summaries = match run_run(windows, &runner_cfg, |_| {
        ReplayStrategyAdapter::from_profile(profile.clone())
    }) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("run aborted: {e}");
            return Ok(4);
        }
    };

    // Write per-window summary as JSON. (Per-window Parquet output is a
    // forward-compatible Phase 3b enhancement — JSON is enough for the
    // determinism-validation contract today.)
    let windows_root = run_root.join("windows");
    fs::create_dir_all(&windows_root)?;
    let mut had_failure = false;
    for s in &summaries {
        if s.status != WindowStatus::Ok {
            had_failure = true;
        }
        let safe_id = s.window_id.replace('/', "_");
        let path = windows_root.join(format!("{safe_id}.json"));
        fs::write(&path, serde_json::to_string_pretty(&s)?)
            .with_context(|| format!("write window summary {}", path.display()))?;
    }
    let summary = RunSummary {
        run_id: run_id.clone(),
        git_rev: cli.git_rev,
        schema_version: SCHEMA_VERSION,
        fill_sim_version: FILL_SIM_VERSION.to_string(),
        fill_config: combined_fill_config,
        windows: summaries,
    };
    fs::write(
        run_root.join("summary.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;

    println!("run_id={} windows={}", run_id, summary.windows.len());
    Ok(if had_failure { 4 } else { 0 })
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
}
