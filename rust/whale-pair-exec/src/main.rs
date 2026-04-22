mod book;
mod config;
mod logging;
mod market_ws;
mod metrics;
mod user_ws;

use std::sync::Arc;

use anyhow::Result;
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::book::BookStore;
use crate::config::AppConfig;
use crate::market_ws::MarketWsClient;
use crate::metrics::{serve_http, AppMetrics};
use crate::user_ws::UserWsClient;

#[tokio::main]
async fn main() -> Result<()> {
    let config = AppConfig::from_env()?;
    logging::init(&config)?;

    let metrics = Arc::new(AppMetrics::new()?);
    let books = Arc::new(BookStore::new(&config.market_assets));
    let shutdown = CancellationToken::new();

    info!(
        service = %config.service_name,
        assets = ?config.market_assets,
        user_markets = ?config.user_markets,
        metrics_bind = %config.metrics_bind,
        loop_interval_ms = config.runtime_loop_interval.as_millis(),
        book_stale_after_ms = config.book_stale_after.as_millis(),
        "starting whale pair execution scaffold"
    );

    let metrics_handle = spawn_metrics(metrics.clone(), &config, shutdown.child_token());
    let market_ws_handle = spawn_market_ws(metrics.clone(), books.clone(), &config, shutdown.child_token());
    let user_ws_handle = spawn_user_ws(metrics.clone(), &config, shutdown.child_token());

    run_runtime_loop(&config, books.clone(), metrics.clone(), shutdown.clone()).await?;

    shutdown.cancel();
    join_task("market-ws", market_ws_handle).await;
    if let Some(handle) = user_ws_handle {
        join_task("user-ws", handle).await;
    }
    join_task("metrics", metrics_handle).await;

    info!("whale pair execution scaffold stopped");
    Ok(())
}

fn spawn_metrics(
    metrics: Arc<AppMetrics>,
    config: &AppConfig,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    let bind = config.metrics_bind;
    tokio::spawn(async move {
        if let Err(error) = serve_http(metrics, bind, shutdown).await {
            warn!(error = ?error, "metrics server exited with error");
        }
    })
}

fn spawn_market_ws(
    metrics: Arc<AppMetrics>,
    books: Arc<BookStore>,
    config: &AppConfig,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    let client = MarketWsClient::new(
        config.market_ws_url.clone(),
        config.market_assets.clone(),
        config.ping_interval,
        books,
        metrics,
    );
    tokio::spawn(async move {
        client.run(shutdown).await;
    })
}

fn spawn_user_ws(
    metrics: Arc<AppMetrics>,
    config: &AppConfig,
    shutdown: CancellationToken,
) -> Option<JoinHandle<()>> {
    let auth = match config.user_auth.clone() {
        Some(auth) => auth,
        None => {
            warn!("POLYMARKET_API_KEY/SECRET/PASSPHRASE not set; user websocket disabled");
            return None;
        }
    };
    let client = UserWsClient::new(
        config.user_ws_url.clone(),
        auth,
        config.user_markets.clone(),
        config.ping_interval,
        metrics,
    );
    Some(tokio::spawn(async move {
        client.run(shutdown).await;
    }))
}

async fn run_runtime_loop(
    config: &AppConfig,
    books: Arc<BookStore>,
    metrics: Arc<AppMetrics>,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut ticks = interval(config.runtime_loop_interval);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut summaries = interval(config.summary_log_interval);
    summaries.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl-c received; shutting down");
                return Ok(());
            }
            _ = shutdown.cancelled() => {
                return Ok(());
            }
            _ = ticks.tick() => {
                let _timer = metrics.runtime_loop_timer();
                metrics.refresh_stream_ages();
                for asset_id in &config.market_assets {
                    match books.snapshot(asset_id).await {
                        Some(book) if book.last_update_unix_ms > 0 => metrics.observe_book(&book, config.book_stale_after),
                        _ => metrics.observe_missing_book(asset_id),
                    }
                }
            }
            _ = summaries.tick() => {
                let snapshots = books.snapshots(&config.market_assets).await;
                if snapshots.is_empty() {
                    warn!("runtime summary: no book snapshots available yet");
                    continue;
                }
                let summary: Vec<String> = snapshots
                    .into_iter()
                    .map(|book| {
                        format!(
                            "{} bid={:.4} ask={:.4} spread={:.4} age_ms={}",
                            book.asset_id,
                            book.best_bid,
                            book.best_ask,
                            book.spread,
                            book.age_ms().map(|value| value.to_string()).unwrap_or_else(|| "na".to_string()),
                        )
                    })
                    .collect();
                info!(books = summary.join(" | "), "runtime book summary");
            }
        }
    }
}

async fn join_task(name: &str, handle: JoinHandle<()>) {
    if let Err(error) = handle.await {
        warn!(task = name, error = ?error, "background task join failed");
    }
}
