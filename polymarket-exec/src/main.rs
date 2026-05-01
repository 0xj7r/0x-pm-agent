//! Binary entrypoint that loads env and runs the executor runtime loop.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let _singleton_guard = maybe_acquire_singleton_lock()?;
    polymarket_exec::runtime::runner::run().await
}

fn maybe_acquire_singleton_lock(
) -> Result<Option<polymarket_exec::infra::preflight::LivePreflightGuard>> {
    let paper_mode = std::env::var("WHALE_PAIR_PAPER_MODE")
        .ok()
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(true);
    let disabled = std::env::var("WHALE_PAIR_DISABLE_SINGLETON_LOCK")
        .ok()
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    if paper_mode || disabled {
        return Ok(None);
    }

    let lock_path = std::env::var("WHALE_PAIR_EXEC_LOCK_PATH")
        .unwrap_or_else(|_| "/tmp/polymarket-exec.live.lock".to_string());
    let config = polymarket_exec::infra::preflight::LivePreflightConfig::new(
        lock_path,
        "polymarket-exec-live",
    );
    Ok(Some(polymarket_exec::infra::preflight::run_live_preflight(
        &config,
    )?))
}
