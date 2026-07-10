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
use clap::{Parser, ValueEnum};
use polymarket_exec::shadow_exec::{
    connect_live_adapter, exec_env_fingerprint, run_execution_loop, LiveArm,
};

/// Named engine config profile. The full decide config comes from the
/// pm-shadow SSOT builders; nothing strategy-shaped is compiled in here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Profile {
    /// Frozen leading config (ungated), validated backtest + shadow-final.
    Frozen,
    /// Frozen + validated live gate package (mom30 / lottery floor /
    /// prod_gap_full).
    Gated,
    /// Current backtest-recommended config from the backtest repo SSOT.
    Recommended,
}

impl Profile {
    fn shadow_args(self, out_dir: PathBuf) -> pm_shadow::ShadowArgs {
        match self {
            Self::Frozen => pm_shadow::frozen_shadow_final_args(out_dir),
            Self::Gated => pm_shadow::gated_shadow_final_args(out_dir),
            Self::Recommended => pm_shadow::recommended_shadow_final_args(out_dir),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Frozen => "frozen",
            Self::Gated => "gated",
            Self::Recommended => "recommended",
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "fast_live")]
struct Args {
    /// Engine config profile (required: the config is chosen at launch,
    /// never compiled in).
    #[arg(long, value_enum)]
    profile: Profile,

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

    /// Evaluate decisions on the next poll tick after any input event,
    /// floored by 20ms spacing; decide-interval-ms becomes the fallback
    /// heartbeat.
    #[arg(long)]
    decide_on_event: bool,

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

    // Full decide config from the pm-shadow SSOT profile builder; only
    // runtime plumbing (out_dir / slug / cadence) is applied on top.
    let mut shadow_args = args.profile.shadow_args(args.out_dir.clone());
    shadow_args.slug_prefix = args.slug_prefix.clone();
    shadow_args.decide_interval_ms = args.decide_interval_ms;
    shadow_args.decide_on_event = args.decide_on_event;

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
        profile = args.profile.name(),
        decide_interval_ms = args.decide_interval_ms,
        decide_on_event = args.decide_on_event,
        out_dir = %args.out_dir.display(),
        slug_prefix = %args.slug_prefix,
        live = live_armed,
        paper = paper_armed,
        "fast_live: in-process pm-shadow engine + execution (no tail hop)"
    );

    // Executor env fingerprint: resolved sizing/arming values, one JSON line.
    // The engine emits the matching decide-config event as the first JSONL
    // record via run_shadow_with_sink.
    {
        let a = arm.lock().expect("arm");
        println!("{}", exec_env_fingerprint(&a, paper_mode));
    }

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
