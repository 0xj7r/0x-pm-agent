//! Agent-side runner for the SHARED `pm-shadow` engine.
//!
//! This runs the EXACT `ShadowCore` engine + feeds + `decide()` that the backtest
//! and the validated `shadow` use — no reimplementation. It is the foundation of
//! the live-execution rebuild after `fade_live` (a separate reimplementation)
//! diverged and lost money (see `docs/postmortem-2026-06-16-fade-live-divergence.md`).
//!
//! CURRENTLY LOG-ONLY (pure shadow): it places NO orders. Live execution will be
//! added by consuming `pm_shadow::run_shadow_with_sink`'s `ExecIntent` channel and
//! routing it to the proven execution adapter — gated behind a parity proof and an
//! explicit arm flag. Until then this binary is safe to run anywhere.
//!
//! Config comes from `pm_shadow::frozen_shadow_final_args` — the SSOT for the
//! validated `shadow-final` twin (edge 0.12, perp 0.75, realized vol / 3600s,
//! hold-to-redemption, rearm 0.08, max_clips 2, sigma floor 3.0, skip-Saturday,
//! 90s pre-close stop).

use anyhow::Result;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let out_dir = std::env::var("PM_SHADOW_OUT_DIR")
        .unwrap_or_else(|_| "shadow-agent".to_string());

    let args = pm_shadow::frozen_shadow_final_args(PathBuf::from(&out_dir));

    tracing::warn!(
        out_dir = %out_dir,
        edge = args.edge_threshold,
        perp = args.perp_price_weight,
        vol_lookback_s = args.vol_lookback_s,
        rearm = args.rearm_edge,
        max_clips = args.max_clips,
        sigma_floor = args.min_entry_sigma_bps,
        skip_saturday = args.skip_saturday,
        "agent shadow_live starting: SHARED pm-shadow engine, LOG-ONLY (no orders)"
    );

    pm_shadow::run_shadow(args).await
}