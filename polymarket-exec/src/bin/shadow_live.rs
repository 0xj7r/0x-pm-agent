//! Agent runner for the SHARED `pm-shadow` engine (validated backtest parity).
//!
//! Decisions flow ONLY through `pm_shadow::ShadowCore` + `decide_entry` SSOT.
//! Optional live/paper execution consumes `ExecIntent` from the engine — never
//! a reimplemented input pipeline. See `docs/postmortem-2026-06-16-fade-live-divergence.md`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use polymarket_exec::shadow_exec::{connect_live_adapter, run_execution_loop, LiveArm};

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let out_dir = std::env::var("PM_SHADOW_OUT_DIR")
        .unwrap_or_else(|_| "shadow-agent".to_string());

    let args = pm_shadow::frozen_shadow_final_args(PathBuf::from(&out_dir));
    let arm = Arc::new(Mutex::new(LiveArm::from_env()));

    tracing::warn!(
        out_dir = %out_dir,
        edge = args.edge_threshold,
        perp = args.perp_price_weight,
        live = arm.lock().expect("arm").live_trade_armed,
        paper = arm.lock().expect("arm").paper_trade_armed,
        "shadow_live: SHARED pm-shadow engine"
    );

    let (intent_tx, commit_rx) = if arm.lock().expect("arm").wants_execution() {
        let (itx, irx) = tokio::sync::mpsc::unbounded_channel();
        let (ctx, crx) = tokio::sync::mpsc::unbounded_channel();
        let adapter = if arm.lock().expect("arm").live_trade_armed {
            Some(Arc::new(connect_live_adapter().await?))
        } else {
            None
        };
        tokio::spawn(run_execution_loop(irx, ctx, arm.clone(), adapter));
        (Some(itx), Some(crx))
    } else {
        (None, None)
    };

    pm_shadow::run_shadow_with_sink(args, intent_tx, commit_rx).await
}