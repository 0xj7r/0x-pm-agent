use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::task::JoinHandle;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::book::{BookState, BookStore};
use crate::config::AppConfig;
use crate::event_log::EventLog;
use crate::journal::JournalWriter;
use crate::market_context::MarketContextStore;
use crate::metrics::AppMetrics;
use crate::runtime::{Runtime, RuntimeConfig, RuntimeOutcome};
use crate::strategy::{Strategy, StrategyMode};
use crate::types::{
    ClientOrderId, FillLiquidity, FillReport, InstrumentId, MarketId, OrderIntent, RuntimeCommand,
    RuntimeStatus, TradeSide,
};
use crate::wire::api::{
    DashboardBook, DashboardEvent, DashboardOrder, DashboardPosition, DashboardSnapshot,
    DashboardUiState, serve_http,
};
use crate::wire::market_ws::MarketWsClient;
use crate::wire::user_ws::{UserOrderEvent, UserWsClient};

pub async fn run() -> Result<()> {
    let config = AppConfig::from_env()?;
    run_with_config(config).await
}

pub async fn run_with_config(config: AppConfig) -> Result<()> {
    crate::logging::init(&config)?;

    let metrics = Arc::new(AppMetrics::new()?);
    let books = Arc::new(BookStore::new(&config.market_assets));
    let market_contexts = match &config.market_context_path {
        Some(path) => MarketContextStore::load_json(path)?,
        None => MarketContextStore::empty(),
    };
    let strategy = StrategyMode::from_name(&config.strategy_name);
    let strategy_name = strategy.name().to_string();
    let paper_fee_coeff = strategy.taker_fee_coeff();
    let shutdown = CancellationToken::new();
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: config.starting_cash_usd,
            event_log_capacity: config.event_log_capacity,
            initial_status: RuntimeStatus::Starting,
        },
        config.risk_limits.clone(),
        strategy,
        market_contexts,
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
    let dashboard_state = Arc::new(RwLock::new(DashboardSnapshot::default()));
    refresh_dashboard_state(
        &mut runtime,
        &books,
        dashboard_state.clone(),
        &config.market_assets,
        &strategy_name,
        config.dashboard_event_limit,
    )
    .await?;

    info!(
        service = %config.service_name,
        strategy = %strategy_name,
        market_context_rows = config
            .market_context_path
            .as_ref()
            .map(|_| "loaded")
            .unwrap_or("none"),
        assets = ?config.market_assets,
        user_markets = ?config.user_markets,
        metrics_bind = %config.metrics_bind,
        loop_interval_ms = config.runtime_loop_interval.as_millis(),
        book_stale_after_ms = config.book_stale_after.as_millis(),
        starting_cash_usd = config.starting_cash_usd,
        event_log_capacity = config.event_log_capacity,
        journal_path = ?config.journal_path,
        paper_mode = config.paper_mode,
        "starting whale pair execution scaffold"
    );

    let metrics_handle = spawn_metrics(
        metrics.clone(),
        &config,
        dashboard_state.clone(),
        shutdown.child_token(),
    );
    let market_ws_handle =
        spawn_market_ws(metrics.clone(), books.clone(), &config, shutdown.child_token());
    let (user_order_tx, user_order_rx) = mpsc::unbounded_channel();
    let user_ws_handle = spawn_user_ws(
        metrics.clone(),
        &config,
        Some(user_order_tx),
        shutdown.child_token(),
    );

    run_runtime_loop(
        &config,
        &books,
        paper_fee_coeff,
        metrics.clone(),
        shutdown.clone(),
        &mut runtime,
        &mut journal,
        &mut HashMap::<ClientOrderId, PaperOrderContext>::new(),
        user_order_rx,
        dashboard_state.clone(),
        config.dashboard_event_limit,
        strategy_name.as_str(),
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
    dashboard: Arc<RwLock<DashboardSnapshot>>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    let bind = config.metrics_bind;
    let state = DashboardUiState {
        metrics: metrics.clone(),
        snapshot: dashboard.clone(),
        whale_events_path: config.dashboard_whale_events_path.clone(),
        whale_events_limit: config.dashboard_event_limit,
    };
    tokio::spawn(async move {
        if let Err(error) = serve_http(state, bind, shutdown).await {
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
    event_tx: Option<mpsc::UnboundedSender<UserOrderEvent>>,
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
        event_tx,
    );
    Some(tokio::spawn(async move {
        client.run(shutdown).await;
    }))
}

async fn run_runtime_loop(
    config: &AppConfig,
    books: &Arc<BookStore>,
    paper_fee_coeff: f64,
    metrics: Arc<AppMetrics>,
    shutdown: CancellationToken,
    runtime: &mut Runtime<StrategyMode>,
    journal: &mut Option<JournalWriter>,
    paper_order_ctx: &mut HashMap<ClientOrderId, PaperOrderContext>,
    mut user_events: mpsc::UnboundedReceiver<UserOrderEvent>,
    dashboard: Arc<RwLock<DashboardSnapshot>>,
    dashboard_event_limit: usize,
    strategy_name: &str,
) -> Result<()> {
    let mut ticks = interval(config.runtime_loop_interval);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut summaries = interval(config.summary_log_interval);
    summaries.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut user_events_open = true;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl-c received; shutting down");
                return Ok(());
            }
            _ = shutdown.cancelled() => {
                return Ok(());
            }
            maybe_user_event = user_events.recv(), if user_events_open => {
                match maybe_user_event {
                    Some(event) => {
                        let user_outcome = handle_user_event(runtime, paper_order_ctx, event)?;
                        persist_runtime_outcome(
                            journal,
                            runtime.event_log(),
                            "user-ws",
                            user_outcome,
                        )?;
                        refresh_dashboard_state(
                            runtime,
                            &books,
                            dashboard.clone(),
                            &config.market_assets,
                            strategy_name,
                            dashboard_event_limit,
                        )
                        .await?;
                    }
                    None => {
                        user_events_open = false;
                        info!("user websocket event channel closed; disabling user event reconciliation");
                    }
                }
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
                            let combined = execute_execution_adapter(
                                runtime,
                                books,
                                paper_fee_coeff,
                                outcome,
                                paper_order_ctx,
                                config.paper_mode,
                            )
                            .await?;
                            persist_runtime_outcome(
                                journal,
                                runtime.event_log(),
                                "book",
                                combined,
                            )?;
                            refresh_dashboard_state(
                                runtime,
                                books,
                                dashboard.clone(),
                                &config.market_assets,
                                strategy_name,
                                dashboard_event_limit,
                            )
                            .await?;
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
                            book.age_ms()
                                .map(|value| value.to_string())
                                .unwrap_or_else(|| "na".to_string()),
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

async fn refresh_dashboard_state(
    runtime: &mut Runtime<StrategyMode>,
    books: &Arc<BookStore>,
    dashboard: Arc<RwLock<DashboardSnapshot>>,
    market_assets: &[String],
    strategy_name: &str,
    event_limit: usize,
) -> Result<()> {
    let inventory_snapshot = runtime.inventory().snapshot();
    let open_order_snapshots = runtime.open_order_snapshots();
    let books = books.snapshots(market_assets).await;

    let open_orders: Vec<DashboardOrder> = open_order_snapshots
        .into_iter()
        .map(|managed| DashboardOrder {
            client_order_id: managed.intent.client_order_id.as_str().to_string(),
            market_id: managed.intent.market_id.as_str().to_string(),
            instrument_id: managed.intent.instrument_id.as_str().to_string(),
            side: format!("{:?}", managed.intent.side),
            status: format!("{:?}", managed.status),
            created_at_ms: managed.intent.created_at_ms,
            last_update_ms: managed.last_update_ms,
            limit_price: managed.intent.limit_price,
            quantity: managed.intent.quantity,
            cumulative_filled_qty: managed.cumulative_filled_qty,
            remaining_qty: managed.remaining_qty(),
            reserved_cash_usd: managed.reserved_cash_usd,
        })
        .collect();

    let positions: Vec<DashboardPosition> = inventory_snapshot
        .positions
        .into_iter()
        .map(|position| {
            let unrealized = (position.mark_or_cost() - position.avg_price) * position.quantity;
            DashboardPosition {
                market_id: position.market_id.as_str().to_string(),
                instrument_id: position.instrument_id.as_str().to_string(),
                quantity: position.quantity,
                avg_price: position.avg_price,
                mark_price: position.mark_price,
                updated_at_ms: position.updated_at_ms,
                gross_notional_usd: position.gross_notional_usd(),
                unrealized_pnl_usd: unrealized,
            }
        })
        .collect();

    let books_state: Vec<DashboardBook> = books
        .into_iter()
        .map(|book| {
            let mid_price = if book.best_bid > 0.0 && book.best_ask > 0.0 {
                Some((book.best_bid + book.best_ask) * 0.5)
            } else {
                None
            };
            let bids = book
                .bid_levels()
                .iter()
                .take(5)
                .map(|level| (level.price, level.size))
                .collect::<Vec<_>>();
            let asks = book
                .ask_levels()
                .iter()
                .take(5)
                .map(|level| (level.price, level.size))
                .collect::<Vec<_>>();
            DashboardBook {
                asset_id: book.asset_id.clone(),
                best_bid: book.best_bid,
                best_bid_size: book.best_bid_size,
                best_ask: book.best_ask,
                best_ask_size: book.best_ask_size,
                spread: book.spread,
                mid_price,
                last_trade_price: book.last_trade_price,
                age_ms: book.age_ms(),
                bids,
                asks,
            }
        })
        .collect();

    let recent_events: Vec<DashboardEvent> = runtime
        .event_log()
        .recent(event_limit)
        .into_iter()
        .map(|event| DashboardEvent {
            seq: event.seq,
            observed_at_ms: event.observed_at_ms,
            category: format!("{:?}", event.category),
            message: event.message,
            market_id: event.market_id.map(|value| value.to_string()),
            instrument_id: event.instrument_id.map(|value| value.to_string()),
            client_order_id: event.client_order_id.map(|value| value.to_string()),
            order_id: event.order_id.map(|value| value.to_string()),
            price: event.metrics.price,
            quantity: event.metrics.quantity,
            notional_usd: event.metrics.notional_usd,
            cash_delta_usd: event.metrics.cash_delta_usd,
            position_delta: event.metrics.position_delta,
            free_cash_after_usd: event.metrics.free_cash_after_usd,
            gross_exposure_after_usd: event.metrics.gross_exposure_after_usd,
        })
        .collect();

    let now_ms = now_unix_ms();
    let mut snapshot = dashboard.write().await;
    *snapshot = DashboardSnapshot {
        status: format!("{:?}", runtime.status()),
        strategy_name: strategy_name.to_string(),
        market_assets: market_assets.to_vec(),
        free_cash_usd: inventory_snapshot.free_cash_usd,
        reserved_cash_usd: inventory_snapshot.reserved_cash_usd,
        total_cash_usd: inventory_snapshot.total_cash_usd,
        realized_pnl_usd: inventory_snapshot.realized_pnl_usd,
        gross_exposure_usd: inventory_snapshot.gross_exposure_usd,
        event_log_len: runtime.event_log().len(),
        event_last_seq: runtime.event_log().latest_seq(),
        generated_at_ms: now_ms,
        positions,
        open_orders,
        books: books_state,
        recent_events,
    };
    Ok(())
}

fn handle_user_event(
    runtime: &mut Runtime<StrategyMode>,
    paper_order_ctx: &mut HashMap<ClientOrderId, PaperOrderContext>,
    event: UserOrderEvent,
) -> Result<RuntimeOutcome> {
    let outcome = match event {
        UserOrderEvent::OrderOpened {
            client_order_id,
            observed_at_ms,
        } => runtime.on_order_opened(&ClientOrderId::from(client_order_id), observed_at_ms),

        UserOrderEvent::OrderRejected {
            client_order_id,
            reason,
            observed_at_ms,
        } => {
            if let Some(client_order_id) = client_order_id {
                paper_order_ctx.remove(&ClientOrderId::from(client_order_id.clone()));
                runtime.on_order_rejected(
                    &ClientOrderId::from(client_order_id),
                    reason.unwrap_or_else(|| "order rejected".to_string()),
                    observed_at_ms,
                )
            } else {
                RuntimeOutcome::default()
            }
        }

        UserOrderEvent::OrderCancelled {
            client_order_id,
            reason,
            observed_at_ms,
        } => {
            if let Some(client_order_id) = client_order_id {
                paper_order_ctx.remove(&ClientOrderId::from(client_order_id.clone()));
                runtime.on_order_cancelled(
                    &ClientOrderId::from(client_order_id),
                    reason.unwrap_or_else(|| "order cancelled".to_string()),
                    observed_at_ms,
                )
            } else {
                RuntimeOutcome::default()
            }
        }

        UserOrderEvent::OrderFilled {
            order_id,
            client_order_id,
            market_id,
            asset_id,
            side,
            price,
            quantity,
            liquidity,
            close_method,
            observed_at_ms,
        } => {
            if quantity <= 0.0 || price <= 0.0 || client_order_id.is_none() {
                RuntimeOutcome::default()
            } else {
                let fill_side = parse_trade_side(&side);
                let liquidity = parse_fill_liquidity(&liquidity);
                let market_id = market_id.map(MarketId::from);
                let instrument_id = asset_id.map(InstrumentId::from);
                let fill = FillReport {
                    order_id: order_id.map(crate::types::OrderId::from),
                    client_order_id: client_order_id.map(crate::types::ClientOrderId::from),
                    market_id: market_id.unwrap_or_else(|| MarketId::from("unknown")),
                    instrument_id: instrument_id.unwrap_or_else(|| InstrumentId::from("unknown")),
                    side: fill_side,
                    price,
                    quantity,
                    fee_usd: 0.0,
                    liquidity,
                    close_method,
                    observed_at_ms,
                };
                runtime.on_fill(fill)?
            }
        }
        UserOrderEvent::OrderMerged {
            order_id,
            client_order_id,
            market_id,
            asset_id,
            price,
            quantity,
            observed_at_ms,
        } => {
            let Some(client_order_id) = client_order_id else {
                return Ok(RuntimeOutcome::default());
            };
            let market_id = market_id.map(MarketId::from);
            let instrument_id = asset_id.map(InstrumentId::from);
            let fill = FillReport {
                order_id: order_id.map(crate::types::OrderId::from),
                client_order_id: Some(crate::types::ClientOrderId::from(client_order_id)),
                market_id: market_id.unwrap_or_else(|| MarketId::from("unknown")),
                instrument_id: instrument_id.unwrap_or_else(|| InstrumentId::from("unknown")),
                side: TradeSide::Sell,
                price,
                quantity,
                fee_usd: 0.0,
                liquidity: FillLiquidity::Unknown,
                close_method: Some(crate::types::CloseMethod::Merge),
                observed_at_ms,
            };
            runtime.on_fill(fill)?
        }
        UserOrderEvent::OrderRedeemed {
            order_id,
            client_order_id,
            market_id,
            asset_id,
            price,
            quantity,
            observed_at_ms,
        } => {
            let Some(client_order_id) = client_order_id else {
                return Ok(RuntimeOutcome::default());
            };
            let market_id = market_id.map(MarketId::from);
            let instrument_id = asset_id.map(InstrumentId::from);
            let fill = FillReport {
                order_id: order_id.map(crate::types::OrderId::from),
                client_order_id: Some(crate::types::ClientOrderId::from(client_order_id)),
                market_id: market_id.unwrap_or_else(|| MarketId::from("unknown")),
                instrument_id: instrument_id.unwrap_or_else(|| InstrumentId::from("unknown")),
                side: TradeSide::Sell,
                price,
                quantity,
                fee_usd: 0.0,
                liquidity: FillLiquidity::Unknown,
                close_method: Some(crate::types::CloseMethod::Redeem),
                observed_at_ms,
            };
            runtime.on_fill(fill)?
        }
    };
    Ok(outcome)
}

fn parse_trade_side(raw: &str) -> TradeSide {
    match raw.to_ascii_lowercase().as_str() {
        "sell" => TradeSide::Sell,
        "bid" => TradeSide::Buy,
        _ => TradeSide::Buy,
    }
}

#[derive(Debug, Clone)]
struct PaperOrderContext {
    arrival_ms: u64,
    queue_bias: f64,
    last_attempt_ms: u64,
}

fn paper_order_context(
    paper_order_ctx: &mut HashMap<ClientOrderId, PaperOrderContext>,
    intent: &OrderIntent,
    now_ms: u64,
) -> PaperOrderContext {
    if let Some(existing) = paper_order_ctx.get(&intent.client_order_id) {
        return existing.clone();
    }
    let state = PaperOrderContext {
        arrival_ms: intent.created_at_ms.min(now_ms),
        queue_bias: deterministic_hash_0_95(intent.client_order_id.as_str()),
        last_attempt_ms: now_ms,
    };
    paper_order_ctx.insert(intent.client_order_id.clone(), state.clone());
    state
}

fn mark_paper_attempt(
    paper_order_ctx: &mut HashMap<ClientOrderId, PaperOrderContext>,
    intent: &OrderIntent,
    now_ms: u64,
) {
    if let Some(state) = paper_order_ctx.get_mut(&intent.client_order_id) {
        state.last_attempt_ms = now_ms;
    }
}

fn parse_fill_liquidity(raw: &Option<String>) -> FillLiquidity {
    match raw.as_ref().map(|value| value.to_ascii_lowercase()) {
        Some(label) if label == "maker" => FillLiquidity::Maker,
        Some(label) if label == "taker" => FillLiquidity::Taker,
        _ => FillLiquidity::Unknown,
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

async fn execute_execution_adapter(
    runtime: &mut Runtime<StrategyMode>,
    books: &Arc<BookStore>,
    paper_fee_coeff: f64,
    outcome: RuntimeOutcome,
    paper_order_ctx: &mut HashMap<ClientOrderId, PaperOrderContext>,
    paper_mode: bool,
) -> Result<RuntimeOutcome> {
    let mut combined = RuntimeOutcome {
        commands: Vec::new(),
        event_seqs: outcome.event_seqs,
    };
    // Simulate execution locally for both paper-mode and non-paper-mode fallback.
    let mut queue: VecDeque<RuntimeCommand> = outcome.commands.into_iter().collect();
    let observed_at_ms = now_unix_ms();
    while let Some(command) = queue.pop_front() {
        combined.commands.push(command.clone());
        match command {
            RuntimeCommand::Submit(intent) => {
                let Some(book) = books.snapshot(intent.instrument_id.as_str()).await else {
                    continue;
                };
                let ctx = paper_order_context(paper_order_ctx, &intent, observed_at_ms);
                mark_paper_attempt(paper_order_ctx, &intent, observed_at_ms);

                if !paper_mode {
                    let opened_outcome =
                        runtime.on_order_opened(&intent.client_order_id, observed_at_ms);
                    combined.extend(opened_outcome);
                }

                if let Some(fill) = paper_fill_from_book_snapshot(
                    &book,
                    &intent,
                    observed_at_ms,
                    paper_fee_coeff,
                    &ctx,
                    intent.quantity,
                ) {
                    let fill_outcome = runtime.on_fill(fill)?;
                    let chained_commands = fill_outcome.commands.clone();
                    combined.extend(fill_outcome);
                    for command in chained_commands {
                        queue.push_back(command);
                    }
                }
            }
            RuntimeCommand::Cancel {
                client_order_id,
                reason,
            } => {
                if !paper_mode {
                    let cancelled_outcome = runtime.on_order_cancelled(
                        &client_order_id,
                        reason.clone(),
                        observed_at_ms,
                    );
                    combined.extend(cancelled_outcome);
                }
                paper_order_ctx.remove(&client_order_id);
                debug!(
                    source = "execution_bridge",
                    client_order_id = %client_order_id,
                    mode = if paper_mode { "paper" } else { "fallback-exec" },
                    "cancel command reconciled locally"
                );
            }
            RuntimeCommand::Noop => {}
        }
    }

    let open_orders = runtime.open_order_snapshots();
    let mut seen = HashSet::new();
    for managed in open_orders {
        let client_order_id = managed.intent.client_order_id.clone();
        if !seen.insert(client_order_id.clone()) {
            continue;
        }
        if managed.remaining_qty() <= 1e-9 {
            paper_order_ctx.remove(&client_order_id);
            continue;
        }
        let Some(book) = books.snapshot(managed.intent.instrument_id.as_str()).await else {
            continue;
        };
        let ctx = paper_order_context(paper_order_ctx, &managed.intent, observed_at_ms);
        mark_paper_attempt(paper_order_ctx, &managed.intent, observed_at_ms);
        if let Some(fill) = paper_fill_from_book_snapshot(
            &book,
            &managed.intent,
            observed_at_ms,
            paper_fee_coeff,
            &ctx,
            managed.remaining_qty(),
        ) {
            let fill_outcome = runtime.on_fill(fill)?;
            combined.extend(fill_outcome);
        }
    }

    // Keep only tracked paper state for active open orders.
    for finished_order in runtime
        .open_order_snapshots()
        .into_iter()
        .filter(|managed| managed.remaining_qty() <= 1e-9)
    {
        paper_order_ctx.remove(&finished_order.intent.client_order_id);
    }

    Ok(combined)
}

fn paper_fill_from_book_snapshot(
    book: &BookState,
    intent: &OrderIntent,
    observed_at_ms: u64,
    paper_fee_coeff: f64,
    order_ctx: &PaperOrderContext,
    remaining_qty: f64,
) -> Option<FillReport> {
    if remaining_qty <= 0.0 || intent.limit_price <= 0.0 {
        return None;
    }

    let candidate_levels: Vec<_> = if matches!(intent.side, TradeSide::Buy) {
        book
            .ask_levels()
            .iter()
            .filter(|level| level.price > 0.0 && level.price <= intent.limit_price)
            .collect()
    } else {
        book
            .bid_levels()
            .iter()
            .filter(|level| level.price > 0.0 && level.price >= intent.limit_price)
            .collect()
    };
    if candidate_levels.is_empty() {
        return None;
    }

    let total_available: f64 = candidate_levels.iter().map(|level| level.size).sum();
    if total_available <= 0.0 {
        return None;
    }

    let crossing = candidate_levels.len() > 1
        || ((candidate_levels[0].price - intent.limit_price).abs() > f64::EPSILON);
    let best_fill_price = candidate_levels[0].price;
    let fill_ratio = paper_fill_ratio(
        remaining_qty,
        total_available,
        book.last_update_unix_ms,
        best_fill_price,
        intent.limit_price,
        order_ctx,
        crossing,
    );
    let target_fill_qty = (remaining_qty * fill_ratio).min(total_available).min(remaining_qty);
    if target_fill_qty <= 0.0 {
        return None;
    }

    let mut remaining = target_fill_qty;
    let mut qty_filled = 0.0;
    let mut amount = 0.0;
    for (idx, level) in candidate_levels.iter().enumerate() {
        if remaining <= 0.0 {
            break;
        }
        let level_ratio = if crossing {
            1.0
        } else if idx == 0 {
            0.9 * order_ctx.queue_bias
        } else {
            0.0
        };
        let level_fill = (level.size * level_ratio).min(remaining);
        if level_fill > 0.0 {
            qty_filled += level_fill;
            amount += level_fill * level.price;
            remaining -= level_fill;
        }
    }

    if qty_filled <= 0.0 {
        qty_filled = target_fill_qty.min(candidate_levels[0].size);
        amount = qty_filled * candidate_levels[0].price;
    }

    if qty_filled <= 0.0 {
        return None;
    }

    let price = if qty_filled > 0.0 {
        amount / qty_filled
    } else {
        0.0
    };
    if price <= 0.0 {
        return None;
    }

    let liquidity = if crossing || fill_ratio >= 0.75 {
        FillLiquidity::Taker
    } else {
        FillLiquidity::Maker
    };
    let notional = qty_filled * price;
    let fee = notional * paper_fee_coeff * price * (1.0 - price);

    Some(FillReport {
        order_id: None,
        client_order_id: Some(intent.client_order_id.clone()),
        market_id: intent.market_id.clone(),
        instrument_id: intent.instrument_id.clone(),
        side: intent.side,
        price,
        quantity: qty_filled,
        fee_usd: fee.max(0.0),
        liquidity,
        close_method: None,
        observed_at_ms,
    })
}

fn paper_fill_ratio(
    order_qty: f64,
    available_qty: f64,
    snapshot_unix_ms: u64,
    fill_price: f64,
    limit_price: f64,
    order_ctx: &PaperOrderContext,
    crossing: bool,
) -> f64 {
    if order_qty <= 0.0 || available_qty <= 0.0 || fill_price <= 0.0 || limit_price <= 0.0 {
        return 0.0;
    }

    let age_ms = now_unix_ms().saturating_sub(order_ctx.arrival_ms.max(order_ctx.last_attempt_ms));
    let age_pressure = if crossing {
        0.50 + 0.50 * ((age_ms as f64 / 3_000.0).clamp(0.0, 1.0))
    } else {
        0.20 + 0.75 * ((age_ms as f64 / 3_000.0).clamp(0.0, 1.0))
    };
    let size_pressure = 0.25 + 0.75 * (available_qty / (available_qty + order_qty));
    let queue_pressure = 0.08 + order_ctx.queue_bias * 0.52;
    let staleness_pressure =
        0.30 + 0.60 * ((now_unix_ms().saturating_sub(snapshot_unix_ms) as f64 / 2_000.0).clamp(0.0, 1.0));
    let premium = ((limit_price - fill_price) / fill_price).max(0.0).min(1.0);
    let limit_pressure = if crossing {
        0.95
    } else {
        0.45 + (premium * 0.35)
    };
    (age_pressure * size_pressure * queue_pressure * staleness_pressure * limit_pressure)
        .clamp(0.02, if crossing { 1.0 } else { 0.9 })
}

fn deterministic_hash_0_95(value: &str) -> f64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let normalized = (hash & 0xffff) as f64 / 65_536.0;
    (0.05 + (normalized * 0.95)).min(1.0)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
