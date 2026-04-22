use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use whale_pair_exec::book::BookStore;
use whale_pair_exec::config::AppConfig;
use whale_pair_exec::journal::JournalWriter;
use whale_pair_exec::market_ws::MarketWsClient;
use whale_pair_exec::metrics::{serve_http, AppMetrics};
use whale_pair_exec::event_log::EventLog;
use whale_pair_exec::runtime::{Runtime, RuntimeConfig, RuntimeOutcome};
use whale_pair_exec::strategy::NoopStrategy;
use whale_pair_exec::types::{InstrumentId, MarketId, RuntimeStatus};
use whale_pair_exec::user_ws::UserWsClient;

#[tokio::main]
async fn main() -> Result<()> {
    let config = AppConfig::from_env()?;
    whale_pair_exec::logging::init(&config)?;

    let metrics = Arc::new(AppMetrics::new()?);
    let books = Arc::new(BookStore::new(&config.market_assets));
    let shutdown = CancellationToken::new();
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: config.starting_cash_usd,
            event_log_capacity: config.event_log_capacity,
            initial_status: RuntimeStatus::Starting,
        },
        config.risk_limits.clone(),
        NoopStrategy,
    );
    let mut journal = config
        .journal_path
        .clone()
        .map(JournalWriter::open)
        .transpose()?;

    let startup_outcome = runtime.start(now_unix_ms());
    persist_runtime_outcome(
        &mut journal,
        runtime.event_log(),
        "startup",
        startup_outcome,
    )?;

    info!(
        service = %config.service_name,
        assets = ?config.market_assets,
        user_markets = ?config.user_markets,
        metrics_bind = %config.metrics_bind,
        loop_interval_ms = config.runtime_loop_interval.as_millis(),
        book_stale_after_ms = config.book_stale_after.as_millis(),
        starting_cash_usd = config.starting_cash_usd,
        event_log_capacity = config.event_log_capacity,
        journal_path = ?config.journal_path,
        "starting whale pair execution scaffold"
    );

    let metrics_handle = spawn_metrics(metrics.clone(), &config, shutdown.child_token());
    let market_ws_handle =
        spawn_market_ws(metrics.clone(), books.clone(), &config, shutdown.child_token());
    let user_ws_handle = spawn_user_ws(metrics.clone(), &config, shutdown.child_token());

    run_runtime_loop(
        &config,
        books.clone(),
        metrics.clone(),
        shutdown.clone(),
        &mut runtime,
        &mut journal,
    )
    .await?;

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
    runtime: &mut Runtime<NoopStrategy>,
    journal: &mut Option<JournalWriter>,
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
                        Some(book) if book.last_update_unix_ms > 0 => {
                            metrics.observe_book(&book, config.book_stale_after);
                            let outcome = runtime.on_book_state(
                                MarketId::from(config.market_id_for_asset(asset_id)),
                                InstrumentId::from(asset_id.as_str()),
                                &book,
                            )?;
                            persist_runtime_outcome(
                                journal,
                                runtime.event_log(),
                                "book",
                                outcome,
                            )?;
                        }
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
                info!(
                    books = summary.join(" | "),
                    runtime_events = runtime.event_log().latest_seq(),
                    open_orders = runtime.open_orders().count(),
                    free_cash_usd = runtime.inventory().free_cash_usd(),
                    gross_exposure_usd = runtime.inventory().gross_exposure_usd(),
                    "runtime book summary"
                );
            }
        }
    }
}

async fn join_task(name: &str, handle: JoinHandle<()>) {
    if let Err(error) = handle.await {
        warn!(task = name, error = ?error, "background task join failed");
    }
}

fn persist_runtime_outcome(
    journal: &mut Option<JournalWriter>,
    event_log: &EventLog,
    source: &str,
    outcome: RuntimeOutcome,
) -> Result<()> {
    if !outcome.commands.is_empty() {
        warn!(
            source,
            command_count = outcome.commands.len(),
            commands = ?outcome.commands,
            "runtime emitted commands but execution adapter is not wired yet"
        );
    }
    if !outcome.event_seqs.is_empty() {
        info!(
            source,
            event_count = outcome.event_seqs.len(),
            latest_seq = outcome.event_seqs.last().copied().unwrap_or_default(),
            "runtime accepted hot-path update"
        );
    }

    if let Some(writer) = journal.as_mut() {
        let after_seq = outcome
            .event_seqs
            .first()
            .copied()
            .unwrap_or_default()
            .saturating_sub(1);
        for record in event_log.snapshot_since(after_seq) {
            writer.append_event(&record)?;
        }
        for command in &outcome.commands {
            writer.append_command(command)?;
        }
        writer.flush()?;
    }

    Ok(())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
