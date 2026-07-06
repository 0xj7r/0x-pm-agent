//! In-process engine + execution: the validated `pm_shadow` engine
//! (`run_shadow_with_sink`) and the shared [`polymarket_exec::shadow_exec`]
//! loop in ONE process, removing the JSONL-tail hop (~100-250ms).
//!
//! Paper-safe by default: real submission requires BOTH `--live` and
//! `LiveArm::from_env()` arming (paper-mode env off, kill-switch path and
//! notional caps configured). No parity gate: in-process, the engine IS the
//! reference.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::Parser;
use polymarket_exec::shadow_exec::{connect_live_adapter, run_execution_loop, LiveArm};

#[derive(Debug, Parser)]
#[command(name = "fast_live")]
struct Args {
    /// Shadow JSONL output directory.
    #[arg(long)]
    out_dir: PathBuf,

    /// Gamma market discovery slug prefix.
    #[arg(long, default_value = "btc-updown-5m-")]
    slug_prefix: String,

    /// Engine decide cadence in milliseconds (wired into pm-shadow once the
    /// engine exposes the field; logged in the banner meanwhile).
    #[arg(long, default_value_t = 100)]
    decide_interval_ms: u64,

    /// Force paper mode: log SUBMITTED without sending real orders.
    /// Paper is also the default whenever --live is absent or unarmed.
    #[arg(long)]
    paper: bool,

    /// Request real-money submission; still requires LiveArm env arming.
    #[arg(long)]
    live: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    // Frozen config + the validated live gate package, matching
    // scripts/shadow_final_gated_flags.sh (== pm_shadow::gated_shadow_final_args).
    let mut shadow_args = pm_shadow::frozen_shadow_final_args(args.out_dir.clone());
    shadow_args.skip_spot_misalign_s = 30;
    shadow_args.min_entry_ask = 0.45;
    shadow_args.skip_open_fav_gap = true;
    shadow_args.open_fav_p_min = 0.88;
    shadow_args.open_fav_ask_max = 0.62;
    shadow_args.open_fav_secs = 300;
    shadow_args.slug_prefix = args.slug_prefix.clone();

    let arm = Arc::new(Mutex::new(LiveArm::from_env()));
    // Read arm state once: locking the std Mutex twice in one statement
    // deadlocks (both guards live to end of statement).
    let (live_armed, paper_armed) = {
        let a = arm.lock().expect("arm");
        (a.live_trade_armed, a.paper_trade_armed)
    };

    let paper_mode = args.paper || !args.live || !live_armed;
    if args.live && paper_mode {
        tracing::warn!(
            live_armed,
            forced_paper = args.paper,
            "--live requested but not armed; staying PAPER"
        );
    }

    tracing::warn!(
        mode = if paper_mode { "PAPER" } else { "LIVE" },
        decide_interval_ms = args.decide_interval_ms,
        out_dir = %args.out_dir.display(),
        slug_prefix = %args.slug_prefix,
        live = live_armed,
        paper = paper_armed,
        "fast_live: in-process pm-shadow engine + execution (no tail hop)"
    );

    let (intent_tx, intent_rx) = tokio::sync::mpsc::unbounded_channel();
    let (commit_tx, commit_rx) = tokio::sync::mpsc::unbounded_channel();

    let adapter = if !paper_mode && arm.lock().expect("arm").live_trade_armed {
        Some(Arc::new(connect_live_adapter().await?))
    } else {
        None
    };

    let exec = tokio::spawn(run_execution_loop(
        intent_rx,
        commit_tx,
        arm.clone(),
        adapter,
        None,
        paper_mode,
    ));

    let engine = tokio::spawn(pm_shadow::run_shadow_with_sink(
        shadow_args,
        Some(intent_tx),
        Some(commit_rx),
    ));

    tokio::select! {
        r = engine => r??,
        r = exec => r?,
    }

    Ok(())
}
