//! JSONL-tailing executor: follows `shadow-final` `would_enter` lines and submits
//! via the shared [`polymarket_exec::shadow_exec`] loop. No belief recompute.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::Parser;
use polymarket_exec::shadow_exec::{connect_live_adapter, run_execution_loop, LiveArm};
use polymarket_exec::shadow_jsonl::JsonlTailer;
use polymarket_exec::shadow_parity::{paper_mode_from_env, shared_gate};

#[derive(Debug, Parser)]
#[command(name = "shadow_exec_tail")]
struct Args {
    /// Shadow JSONL file or directory (latest shadow-*.jsonl when dir).
    #[arg(long)]
    path: Option<std::path::PathBuf>,

    /// Follow appended lines (default true).
    #[arg(long, default_value_t = true)]
    follow: bool,

    #[arg(long)]
    no_follow: bool,

    /// Byte offset to start from (overrides persisted state).
    #[arg(long)]
    offset: Option<u64>,

    /// Ignore persisted tail state and read from byte 0.
    #[arg(long)]
    from_start: bool,

    /// Paper mode: log SUBMITTED but do not send real orders (parity audit).
    #[arg(long)]
    paper: bool,
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
    let follow = if args.no_follow { false } else { args.follow };
    let paper_mode = args.paper || paper_mode_from_env();

    let mut tailer = JsonlTailer::from_env()?;
    if let Some(path) = args.path {
        tailer = tailer.with_path(path);
    }
    tailer = tailer.with_follow(follow);
    if let Some(offset) = args.offset {
        tailer = tailer.with_offset(offset);
    }
    if args.from_start {
        tailer = tailer.with_from_start(true);
    }

    let arm = Arc::new(Mutex::new(LiveArm::from_env()));
    let (live_armed, paper_armed) = {
        let a = arm.lock().expect("arm");
        (a.live_trade_armed, a.paper_trade_armed)
    };
    tracing::warn!(
        follow,
        live = live_armed,
        paper = paper_armed,
        paper_mode,
        "shadow_exec_tail: JSONL consumer (no engine recompute)"
    );

    let parity = shared_gate();
    let (intent_tx, intent_rx) = tokio::sync::mpsc::unbounded_channel();
    let (commit_tx, mut commit_rx) = tokio::sync::mpsc::unbounded_channel();

    tokio::spawn(async move {
        while commit_rx.recv().await.is_some() {}
    });

    let adapter = if !paper_mode && arm.lock().expect("arm").live_trade_armed {
        Some(Arc::new(connect_live_adapter().await?))
    } else {
        None
    };

    let exec_arm = arm.clone();
    let exec_parity = parity.clone();
    let exec = tokio::spawn(run_execution_loop(
        intent_rx,
        commit_tx,
        exec_arm,
        adapter,
        Some(exec_parity),
        paper_mode,
    ));

    let tail_parity = parity.clone();
    let tail = tokio::spawn(async move { tailer.run(intent_tx, Some(tail_parity)).await });

    tokio::select! {
        r = tail => r??,
        r = exec => r?,
    }

    Ok(())
}