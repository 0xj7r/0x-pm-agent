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
use crate::runtime::order_store::SqliteOrderStore;
use crate::runtime::types::ManagedOrderStatus;
use crate::strategy::{Strategy, StrategyMode};
use crate::types::{
    ClientOrderId, FillLiquidity, FillReport, InstrumentId, MarketId, OrderIntent, OrderId,
    RuntimeCommand, RuntimeStatus, TradeSide,
};
use crate::wire::execution_adapter::{
    CancelOrderRequest, ExecutionAdapter, PaperExecutionAdapter, PolymarketCredentials,
    PolymarketExecutionAdapter, SubmitOrderRequest, TimeInForce,
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
    let strategy = StrategyMode::from_name(&config.strategy_name, config.strategy_profile.as_ref());
    let strategy_name = strategy.name().to_string();
    let paper_fee_coeff = strategy.taker_fee_coeff();
    let shutdown = CancellationToken::new();
    let order_store = config
        .order_store_path
        .as_ref()
        .map(|path| -> Result<Box<dyn crate::runtime::order_store::OrderStore>> {
            Ok(Box::new(SqliteOrderStore::open(path)?))
        })
        .transpose()?;
    let mut runtime = Runtime::new_with_order_store(
        RuntimeConfig {
            starting_cash_usd: config.starting_cash_usd,
            event_log_capacity: config.event_log_capacity,
            initial_status: RuntimeStatus::Starting,
            quote_engine_config: config
                .strategy_profile
                .as_ref()
                .map(|profile| profile.quote_engine_config())
                .unwrap_or_default(),
            quote_stale_ms: config
                .strategy_profile
                .as_ref()
                .and_then(|profile| profile.quote.min_quote_age_ms)
                .unwrap_or(10_000),
        },
        config.risk_limits.clone(),
        strategy,
        market_contexts,
        order_store,
        config.runtime_run_id.clone().unwrap_or_else(|| format!("run-{}", now_unix_ms())),
    );
    let mut journal = config
        .journal_path
        .clone()
        .map(JournalWriter::open)
        .transpose()?;

    let mut startup_outcome =
        runtime.recover_from_store(now_unix_ms(), config.order_reconcile_stale_window.as_millis() as u64);
    startup_outcome.extend(runtime.start(now_unix_ms()));
    persist_runtime_outcome(
        &mut journal,
        runtime.event_log(),
        "startup",
        startup_outcome,
    )?;
    persist_runtime_checkpoint(
        &mut journal,
        &runtime,
        now_unix_ms(),
        "startup",
    )?;
    let execution_adapter: Arc<dyn ExecutionAdapter> = match (config.paper_mode, &config.user_auth) {
        (true, _) => Arc::new(PaperExecutionAdapter::new()),
        (false, None) => {
            warn!(
                "POLYMARKET_API_KEY/SECRET/PASSPHRASE not set; falling back to paper mode execution"
            );
            Arc::new(PaperExecutionAdapter::new())
        }
        (false, Some(auth)) => Arc::new(PolymarketExecutionAdapter::new(PolymarketCredentials {
            api_key: auth.api_key.clone(),
            api_secret: auth.api_secret.clone(),
            api_passphrase: auth.api_passphrase.clone(),
        })),
    };
    metrics.set_execution_adapter_connected(true);
    let dashboard_state = Arc::new(RwLock::new(DashboardSnapshot::default()));
    refresh_dashboard_state(
        &mut runtime,
        &books,
        metrics.as_ref(),
        &config,
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
    let mut paper_order_ctx = HashMap::<ClientOrderId, PaperOrderContext>::new();
    let mut execution_venue_map = HashMap::<ClientOrderId, Option<OrderId>>::new();

    run_runtime_loop(
        &config,
        &books,
        paper_fee_coeff,
        metrics.clone(),
        shutdown.clone(),
        &mut runtime,
        &mut journal,
        &mut paper_order_ctx,
        &mut execution_venue_map,
        execution_adapter,
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
    execution_venue_map: &mut HashMap<ClientOrderId, Option<OrderId>>,
    execution_adapter: Arc<dyn ExecutionAdapter>,
    mut user_events: mpsc::UnboundedReceiver<UserOrderEvent>,
    dashboard: Arc<RwLock<DashboardSnapshot>>,
    dashboard_event_limit: usize,
    strategy_name: &str,
) -> Result<()> {
    let mut ticks = interval(config.runtime_loop_interval);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut reconcile_ticks = interval(config.order_reconcile_interval);
    reconcile_ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut checkpoint_ticks = interval(config.runtime_checkpoint_interval);
    checkpoint_ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);

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
                        let user_outcome = handle_user_event(
                            runtime,
                            paper_order_ctx,
                            execution_venue_map,
                            metrics.as_ref(),
                            event,
                        )?;
                        persist_runtime_outcome(
                            journal,
                            runtime.event_log(),
                            "user-ws",
                            user_outcome,
                        )?;
                        refresh_dashboard_state(
                            runtime,
                            &books,
                            metrics.as_ref(),
                            &config,
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
                                metrics.as_ref(),
                                outcome,
                                paper_order_ctx,
                                execution_venue_map,
                                execution_adapter.clone(),
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
                                metrics.as_ref(),
                                &config,
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
            _ = reconcile_ticks.tick() => {
                let reconcile_outcome = runtime.reconcile_open_orders(
                    now_unix_ms(),
                    config.order_reconcile_stale_window.as_millis() as u64,
                );
                persist_runtime_outcome(
                    journal,
                    runtime.event_log(),
                    "reconcile",
                    reconcile_outcome,
                )?;
                metrics.touch_reconcile();
                metrics.refresh_stream_ages();
                persist_runtime_checkpoint(
                    journal,
                    runtime,
                    now_unix_ms(),
                    "reconcile",
                )?;
            }
            _ = checkpoint_ticks.tick() => {
                persist_runtime_checkpoint(
                    journal,
                    runtime,
                    now_unix_ms(),
                    "periodic",
                )?;
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
    metrics: &AppMetrics,
    config: &AppConfig,
    dashboard: Arc<RwLock<DashboardSnapshot>>,
    market_assets: &[String],
    strategy_name: &str,
    event_limit: usize,
) -> Result<()> {
    let inventory_snapshot = runtime.inventory().snapshot();
    let open_order_snapshots = runtime.open_order_snapshots();
    let books = books.snapshots(market_assets).await;
    let now_ms = now_unix_ms();
    let profile = config.strategy_profile.as_ref();
    metrics.refresh_stream_ages();

    let open_orders: Vec<DashboardOrder> = open_order_snapshots
        .iter()
        .cloned()
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

    let mut yes_qty = 0.0;
    let mut no_qty = 0.0;
    let mut yes_notional_usd = 0.0;
    let mut no_notional_usd = 0.0;
    let mut unrealized_pnl_usd = 0.0;
    let positions: Vec<DashboardPosition> = inventory_snapshot
        .positions
        .into_iter()
        .map(|position| {
            let unrealized = (position.mark_or_cost() - position.avg_price) * position.quantity;
            let is_yes_like = is_yes_like(&position.instrument_id.as_str().to_ascii_lowercase());
            let quantity = position.quantity.abs();
            let notional = quantity * position.mark_or_cost();
            if is_yes_like {
                yes_qty += quantity;
                yes_notional_usd += notional;
            } else {
                no_qty += quantity;
                no_notional_usd += notional;
            }
            unrealized_pnl_usd += unrealized;
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

    let mut book_mid_by_asset = HashMap::new();
    let books_state: Vec<DashboardBook> = books
        .into_iter()
        .map(|book| {
            let mid_price = if book.best_bid > 0.0 && book.best_ask > 0.0 {
                Some((book.best_bid + book.best_ask) * 0.5)
            } else {
                None
            };
            if let Some(mid) = mid_price {
                book_mid_by_asset.insert(book.asset_id.clone(), mid);
            }
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

    let quote_snapshot = profile.map(|profile| profile.quote.clone()).unwrap_or_default();
    let quote_ladder_count = open_order_snapshots
        .iter()
        .filter(|managed| managed.remaining_qty() > 0.0 && managed.intent.quote_level_tag.is_some())
        .count();
    let quote_edge_bps = if quote_ladder_count == 0 {
        0.0
    } else {
        let mut total_edge_bps = 0.0;
        let mut matched_quotes = 0.0;
        for managed in open_order_snapshots {
            if managed.remaining_qty() <= 0.0 {
                continue;
            }
            let Some(mid_price) = book_mid_by_asset.get(managed.intent.instrument_id.as_str()) else {
                continue;
            };
            if *mid_price <= 0.0 {
                continue;
            }
            let edge_bps = match managed.intent.side {
                crate::types::TradeSide::Buy => {
                    ((*mid_price - managed.intent.limit_price) / *mid_price) * 10_000.0
                }
                crate::types::TradeSide::Sell => {
                    ((managed.intent.limit_price - *mid_price) / *mid_price) * 10_000.0
                }
            };
            total_edge_bps += edge_bps;
            matched_quotes += 1.0;
        }
        if matched_quotes > 0.0 {
            total_edge_bps / matched_quotes
        } else {
            0.0
        }
    };
    let quote_age_ms = runtime
        .open_order_snapshots()
        .into_iter()
        .map(|managed| now_ms.saturating_sub(managed.last_update_ms) as f64)
        .fold(0.0, f64::max);
    metrics.set_quote_metrics(
        quote_ladder_count,
        quote_snapshot.max_quote_per_side_usd.unwrap_or(0.0),
        quote_snapshot.min_edge_bps.unwrap_or(0.0),
        quote_snapshot.skew_cap_bps.unwrap_or(0.0),
        quote_snapshot.refresh_interval_ms.unwrap_or(0),
        quote_edge_bps,
        quote_age_ms,
    );

    let merge_candidate_qty = yes_qty.min(no_qty);
    let stranded_yes_qty = (yes_qty - no_qty).max(0.0);
    let stranded_no_qty = (no_qty - yes_qty).max(0.0);
    let inventory_skew_usd = (yes_notional_usd - no_notional_usd).abs();
    metrics.set_pair_metrics(stranded_yes_qty, stranded_no_qty, merge_candidate_qty);
    metrics.set_risk_metrics(inventory_skew_usd);

    let control_plane_metrics = metrics.snapshot();
    let net_edge_usd_total = inventory_snapshot.realized_pnl_usd + unrealized_pnl_usd
        - control_plane_metrics.fees_usd_total
        + control_plane_metrics.rebates_usd_total;
    metrics.set_economics_metrics(
        inventory_snapshot.realized_pnl_usd,
        unrealized_pnl_usd,
        net_edge_usd_total,
    );
    let control_plane_metrics = metrics.snapshot();

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
        control_plane: crate::wire::api::RuntimeControlPlaneState {
            profile_name: profile.map(|profile| profile.profile_name.clone()),
            profile_version: profile.and_then(|profile| profile.version.clone()),
            market_context_version: Some(runtime.market_context_version().to_string()),
            quote: crate::wire::api::QuoteControlPlaneState {
                ladder_count: control_plane_metrics.quote_ladder_count,
                max_quote_per_side_usd: control_plane_metrics.quote_max_per_side_usd,
                min_edge_bps: control_plane_metrics.quote_min_edge_bps,
                skew_cap_bps: control_plane_metrics.quote_skew_cap_bps,
                refresh_interval_ms: control_plane_metrics.quote_refresh_interval_ms as u64,
                edge_bps: control_plane_metrics.quote_edge_bps,
                age_ms: control_plane_metrics.quote_age_ms,
            },
            fill: crate::wire::api::FillControlPlaneState {
                total: control_plane_metrics.fill_total,
                maker_total: control_plane_metrics.fill_maker_total,
                taker_total: control_plane_metrics.fill_taker_total,
                maker_share: control_plane_metrics.fill_maker_share,
                notional_usd_total: control_plane_metrics.fill_notional_usd_total,
            },
            pair: crate::wire::api::PairControlPlaneState {
                completed_qty_total: control_plane_metrics.pair_completed_qty_total,
                stranded_yes_qty: control_plane_metrics.stranded_yes_qty,
                stranded_no_qty: control_plane_metrics.stranded_no_qty,
                merge_candidate_qty: control_plane_metrics.merge_candidate_qty,
                merge_latency_ms: control_plane_metrics.merge_latency_ms,
            },
            risk: crate::wire::api::RiskControlPlaneState {
                book_stale_events_total: control_plane_metrics.book_stale_events_total,
                reconcile_failures_total: control_plane_metrics.reconcile_failures_total,
                runtime_riskoff_transitions_total: control_plane_metrics
                    .runtime_riskoff_transitions_total,
                uncertain_submit_total: control_plane_metrics.uncertain_submit_total,
                inventory_skew_usd: control_plane_metrics.inventory_skew_usd,
            },
            economics: crate::wire::api::EconomicsControlPlaneState {
                realized_pnl_usd: control_plane_metrics.realized_pnl_usd,
                unrealized_pnl_usd: control_plane_metrics.unrealized_pnl_usd,
                fees_usd_total: control_plane_metrics.fees_usd_total,
                rebates_usd_total: control_plane_metrics.rebates_usd_total,
                net_edge_usd_total: control_plane_metrics.net_edge_usd_total,
            },
            health: crate::wire::api::HealthControlPlaneState {
                market_ws_connected: control_plane_metrics.market_ws_connected,
                user_ws_connected: control_plane_metrics.user_ws_connected,
                execution_adapter_connected: control_plane_metrics.execution_adapter_connected,
                last_market_message_age_ms: control_plane_metrics.market_last_message_age_ms,
                last_user_message_age_ms: control_plane_metrics.user_last_message_age_ms,
                last_reconcile_age_ms: control_plane_metrics.last_reconcile_age_ms,
            },
        },
    };
    Ok(())
}

fn handle_user_event(
    runtime: &mut Runtime<StrategyMode>,
    paper_order_ctx: &mut HashMap<ClientOrderId, PaperOrderContext>,
    execution_venue_map: &mut HashMap<ClientOrderId, Option<OrderId>>,
    metrics: &AppMetrics,
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
                let client_order_id = ClientOrderId::from(client_order_id);
                paper_order_ctx.remove(&client_order_id);
                execution_venue_map.remove(&client_order_id);
                runtime.on_order_rejected(
                    &client_order_id,
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
                let client_order_id = ClientOrderId::from(client_order_id);
                paper_order_ctx.remove(&client_order_id);
                execution_venue_map.remove(&client_order_id);
                runtime.on_order_cancelled(
                    &client_order_id,
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
                metrics.record_fill(
                    &fill,
                    if matches!(fill.close_method, Some(crate::types::CloseMethod::Merge)) {
                        Some(0)
                    } else {
                        None
                    },
                );
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
            metrics.record_fill(&fill, Some(0));
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
            metrics.record_fill(&fill, None);
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

fn is_yes_like(raw: &str) -> bool {
    raw.contains("yes")
        || raw.contains("up")
        || raw.contains("long")
        || raw.contains("bull")
        || raw.contains("call")
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

fn persist_runtime_checkpoint(
    journal: &mut Option<JournalWriter>,
    runtime: &Runtime<StrategyMode>,
    observed_at_ms: u64,
    name: &str,
) -> Result<()> {
    if let Some(writer) = journal.as_mut() {
        let open_orders = runtime.open_order_snapshots();
        let open_orders_count = open_orders.len();
        let needs_reconcile_orders = open_orders
            .iter()
            .filter(|managed| managed.status == ManagedOrderStatus::NeedsReconcile)
            .count();
        writer.append_checkpoint(
            observed_at_ms,
            runtime.run_id(),
            name,
            open_orders_count,
            needs_reconcile_orders,
            runtime.event_log().latest_seq(),
        )?;
        writer.flush()?;
    }
    Ok(())
}

async fn execute_execution_adapter(
    runtime: &mut Runtime<StrategyMode>,
    books: &Arc<BookStore>,
    paper_fee_coeff: f64,
    metrics: &AppMetrics,
    outcome: RuntimeOutcome,
    paper_order_ctx: &mut HashMap<ClientOrderId, PaperOrderContext>,
    execution_venue_map: &mut HashMap<ClientOrderId, Option<OrderId>>,
    execution_adapter: Arc<dyn ExecutionAdapter>,
    paper_mode: bool,
) -> Result<RuntimeOutcome> {
    let mut combined = RuntimeOutcome {
        commands: Vec::new(),
        event_seqs: outcome.event_seqs,
    };

    let observed_at_ms = now_unix_ms();
    let mut queue: VecDeque<RuntimeCommand> = outcome.commands.into_iter().collect();

    if !paper_mode {
        sync_execution_state(execution_adapter.as_ref(), runtime, execution_venue_map).await;
        let mut dedupe = HashSet::new();
        for managed in runtime.open_order_snapshots() {
            if managed.remaining_qty() <= 0.0 {
                continue;
            }
            let client_order_id = managed.intent.client_order_id.clone();
            if !dedupe.insert(client_order_id.clone()) {
                continue;
            }
            match managed.status {
                ManagedOrderStatus::PendingSubmit => {
                    queue.push_back(RuntimeCommand::Submit(managed.intent.clone()))
                }
                ManagedOrderStatus::NeedsReconcile => {
                    queue.push_back(RuntimeCommand::Submit(managed.intent.clone()))
                }
                ManagedOrderStatus::CancelRequested => queue.push_back(RuntimeCommand::Cancel {
                    client_order_id,
                    reason: "recovering live order".to_string(),
                }),
                _ => {}
            }
        }
    }

    while let Some(command) = queue.pop_front() {
        combined.commands.push(command.clone());
        match command {
            RuntimeCommand::Submit(intent) => {
                if paper_mode {
                    let Some(book) = books.snapshot(intent.instrument_id.as_str()).await else {
                        continue;
                    };
                    let ctx = paper_order_context(paper_order_ctx, &intent, observed_at_ms);
                    mark_paper_attempt(paper_order_ctx, &intent, observed_at_ms);

                    if let Some(fill) = paper_fill_from_book_snapshot(
                        &book,
                        &intent,
                        observed_at_ms,
                        paper_fee_coeff,
                        &ctx,
                        intent.quantity,
                    ) {
                        metrics.record_fill(
                            &fill,
                            if matches!(fill.close_method, Some(crate::types::CloseMethod::Merge))
                            {
                                Some(observed_at_ms.saturating_sub(fill.observed_at_ms))
                            } else {
                                None
                            },
                        );
                        let fill_outcome = runtime.on_fill(fill)?;
                        let chained_commands = fill_outcome.commands.clone();
                        combined.extend(fill_outcome);
                        for command in chained_commands {
                            queue.push_back(command);
                        }
                    }
                    continue;
                }

                let submit_req = submit_request_from_intent(&intent, observed_at_ms);
                match execution_adapter.submit(submit_req).await {
                    Ok(ack) if ack.accepted => {
                        if let Some(order_id) = ack.venue_order_id.clone() {
                            execution_venue_map.insert(intent.client_order_id.clone(), Some(order_id));
                        }
                        let opened_outcome =
                            runtime.on_order_opened(&intent.client_order_id, observed_at_ms);
                        combined.extend(opened_outcome);
                    }
                    Ok(ack) => {
                        let reason = ack
                            .venue_message
                            .unwrap_or_else(|| "execution venue rejected submit".to_string());
                        let rejected_outcome = runtime.on_order_rejected(
                            &intent.client_order_id,
                            reason,
                            ack.accepted_at_ms,
                        );
                        paper_order_ctx.remove(&intent.client_order_id);
                        combined.extend(rejected_outcome);
                    }
                    Err(error) => {
                        if error.is_retryable() {
                            warn!(
                                mode = "live",
                                client_order_id = %intent.client_order_id,
                                error = %error,
                                requires_reconcile = error.requires_reconcile(),
                                "submit retry scheduled by classification"
                            );
                            if error.requires_reconcile() {
                                metrics.observe_uncertain_submit();
                                combined.extend(runtime.set_order_status(
                                    &intent.client_order_id,
                                    ManagedOrderStatus::NeedsReconcile,
                                    observed_at_ms,
                                    "submission uncertain; moving to needs-reconcile",
                                ));
                            }
                        } else {
                            let rejected_outcome = runtime.on_order_rejected(
                                &intent.client_order_id,
                                error.to_string(),
                                observed_at_ms,
                            );
                            paper_order_ctx.remove(&intent.client_order_id);
                            combined.extend(rejected_outcome);
                        }
                    }
                }
            }
            RuntimeCommand::Cancel {
                client_order_id,
                reason,
            } => {
                if paper_mode {
                    let cancelled_outcome = runtime.on_order_cancelled(
                        &client_order_id,
                        reason.clone(),
                        observed_at_ms,
                    );
                    combined.extend(cancelled_outcome);
                    paper_order_ctx.remove(&client_order_id);
                    debug!(
                        source = "execution_bridge",
                        client_order_id = %client_order_id,
                        mode = "paper",
                        "cancel command reconciled locally"
                    );
                    continue;
                }

                let cancel_req = CancelOrderRequest {
                    client_order_id: client_order_id.clone(),
                    venue_order_id: execution_venue_map.get(&client_order_id).cloned().flatten(),
                    reason,
                    submitted_at_ms: observed_at_ms,
                };
                match execution_adapter.cancel(cancel_req).await {
                    Ok(ack) if ack.accepted => {
                        execution_venue_map.remove(&client_order_id);
                        let cancelled_outcome = runtime.on_order_cancelled(
                            &client_order_id,
                            ack.venue_message.unwrap_or_else(|| "execution cancelled".to_string()),
                            ack.accepted_at_ms,
                        );
                        combined.extend(cancelled_outcome);
                    }
                    Ok(ack) => {
                        let reason = ack
                            .venue_message
                            .unwrap_or_else(|| "execution venue rejected cancel".to_string());
                        let rejected_outcome = runtime.on_order_rejected(
                            &client_order_id,
                            reason,
                            ack.accepted_at_ms,
                        );
                        combined.extend(rejected_outcome);
                    }
                    Err(error) => {
                        if error.is_retryable() {
                            warn!(
                                mode = "live",
                                client_order_id = %client_order_id,
                                error = %error,
                                requires_reconcile = error.requires_reconcile(),
                                "cancel retry scheduled by classification"
                            );
                        } else {
                            let rejected_outcome = runtime.on_order_rejected(
                                &client_order_id,
                                error.to_string(),
                                observed_at_ms,
                            );
                            combined.extend(rejected_outcome);
                        }
                    }
                }
            }
            RuntimeCommand::Noop => {}
        }
    }

    if !paper_mode {
        return Ok(combined);
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
            metrics.record_fill(
                &fill,
                if matches!(fill.close_method, Some(crate::types::CloseMethod::Merge)) {
                    Some(observed_at_ms.saturating_sub(fill.observed_at_ms))
                } else {
                    None
                },
            );
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

fn submit_request_from_intent(intent: &OrderIntent, observed_at_ms: u64) -> SubmitOrderRequest {
    SubmitOrderRequest {
        client_order_id: intent.client_order_id.clone(),
        market_id: intent.market_id.clone(),
        instrument_id: intent.instrument_id.clone(),
        side: intent.side,
        limit_price: intent.limit_price,
        quantity: intent.quantity,
        post_only: false,
        time_in_force: TimeInForce::Gtc,
        strategy_tag: "runtime".to_string(),
        quote_level_tag: intent.quote_level_tag.clone(),
        submitted_at_ms: observed_at_ms,
    }
}

async fn sync_execution_state(
    execution_adapter: &dyn ExecutionAdapter,
    runtime: &Runtime<StrategyMode>,
    execution_venue_map: &mut HashMap<ClientOrderId, Option<OrderId>>,
) {
    match execution_adapter.sync_open_orders().await {
        Ok(open_orders) => {
            let open_order_count = open_orders.len();
            for order in open_orders {
                if let Some(client_order_id) = order.client_order_id.clone() {
                    execution_venue_map
                        .entry(client_order_id)
                        .or_insert(Some(order.venue_order_id.clone()));
                }
            }
            debug!(
                open_orders = open_order_count,
                local_open_orders = runtime.open_order_snapshots().len(),
                "synced execution open orders"
            );
        }
        Err(error) => {
            warn!(error = %error, "execution open orders sync failed");
        }
    }

    match execution_adapter.sync_balances().await {
        Ok(balances) => {
            debug!(
                cash_usd = balances.cash_usd,
                positions = balances.positions.len(),
                observed_at_ms = balances.observed_at_ms,
                "synced execution balances"
            );
        }
        Err(error) => {
            warn!(error = %error, "execution balances sync failed");
        }
    }
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
