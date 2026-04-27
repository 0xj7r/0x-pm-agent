//! Runtime orchestration loop wiring books, websockets, execution adapter, and ops APIs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::{mpsc, watch, RwLock};
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::book::{BookState, BookStore};
use crate::config::{AppConfig, UserWsAuth};
use crate::event_log::EventLog;
use crate::inventory::VenuePositionSnapshot;
use crate::journal::JournalWriter;
use crate::market_context::MarketContextStore;
use crate::metrics::AppMetrics;
use crate::quote_reconciler::ReconcilerConfig;
use crate::risk::RiskLimits;
use crate::runtime::audit::AuditWriter;
use crate::runtime::live_auth::{connect_live_adapter, connect_live_session};
use crate::runtime::order_store::SqliteOrderStore;
use crate::runtime::types::ManagedOrderStatus;
use crate::runtime::{Runtime, RuntimeConfig, RuntimeOutcome};
use crate::strategy::{Strategy, StrategyMode, VenueMarketRules};
use crate::types::{
    ClientOrderId, FillLiquidity, FillReport, InstrumentId, MarketId, OrderId, OrderIntent,
    RuntimeCommand, RuntimeStatus, TradeSide,
};
use crate::wire::api::{
    serve_http, DashboardBook, DashboardEvent, DashboardOrder, DashboardPosition,
    DashboardSnapshot, DashboardUiState,
};
use crate::wire::execution_adapter::{
    CancelOrderRequest, ExecutionAdapter, MergePositionsRequest, PaperExecutionAdapter,
    RedeemPositionsRequest, SubmitOrderRequest, TimeInForce, VenueFill, VenuePosition,
};
use crate::wire::market_ws::MarketWsClient;
use crate::wire::spot_ws::{SpotTradeEvent, SpotWsClient};
use crate::wire::user_ws::{UserOrderEvent, UserWsClient};

const LIVE_HEALTH_STARTUP_GRACE_MS: u64 = 15_000;
const BTC_5M_WINDOW_MS: u64 = 5 * 60 * 1_000;

#[derive(Debug, Clone)]
struct RuntimeMarketUniverse {
    market_assets: Vec<String>,
    user_markets: Vec<String>,
    market_id_by_asset: HashMap<String, String>,
}

impl RuntimeMarketUniverse {
    fn from_config_and_context(config: &AppConfig, contexts: &MarketContextStore) -> Self {
        let context_assets = contexts.asset_ids();
        let context_markets = contexts.market_ids();
        let context_map = contexts.asset_market_map();
        Self {
            market_assets: if context_assets.is_empty() {
                config.market_assets.clone()
            } else {
                context_assets
            },
            user_markets: if context_markets.is_empty() {
                config.user_markets.clone()
            } else {
                context_markets
            },
            market_id_by_asset: if context_map.is_empty() {
                config.market_id_by_asset.clone()
            } else {
                context_map
            },
        }
    }

    fn market_id_for_asset(&self, config: &AppConfig, asset_id: &str) -> String {
        self.market_id_by_asset
            .get(asset_id)
            .cloned()
            .or_else(|| config.market_id_by_asset.get(asset_id).cloned())
            .unwrap_or_else(|| asset_id.to_string())
    }
}

#[derive(Debug, Default)]
struct LiveSafetyState {
    consecutive_submit_errors: usize,
    consecutive_cancel_errors: usize,
    consecutive_reconcile_mismatches: usize,
    last_venue_cash_usd: Option<f64>,
    last_venue_position_count: usize,
    last_venue_balance_observed_at_ms: Option<u64>,
}

#[derive(Debug, Default)]
struct ExecutionSyncReport {
    open_order_count: usize,
    balance_synced: bool,
    venue_cash_usd: Option<f64>,
    venue_position_count: usize,
    venue_positions_authoritative: bool,
    venue_balance_observed_at_ms: Option<u64>,
    venue_fills: Vec<VenueFill>,
    venue_positions: Vec<VenuePosition>,
    errors: usize,
    missing_local_orders: Vec<ClientOrderId>,
    pending_missing_local_orders: Vec<ClientOrderId>,
}

#[derive(Debug, Clone)]
struct ExecutionPolicy {
    paper_mode: bool,
    live_post_only: bool,
    live_order_ttl_ms: u64,
    live_order_max_age_ms: u64,
    live_reconcile_missing_grace_ms: u64,
    live_max_submit_errors: usize,
    live_max_cancel_errors: usize,
    live_kill_on_reconcile_mismatch: bool,
    paper_min_fill_notional_usd: f64,
    paper_max_fills_per_order: usize,
    paper_min_fill_interval_ms: u64,
    paper_market_close_at_ms: Option<u64>,
    paper_market_resolution_price: Option<f64>,
    paper_submit_latency_ms: u64,
    paper_queue_depth_fraction: f64,
    paper_post_only_reject_probability: f64,
    paper_cancel_race_window_ms: u64,
    paper_maker_rebate_coeff: f64,
    paper_taker_fee_coeff_override: Option<f64>,
}

impl ExecutionPolicy {
    fn from_config(config: &AppConfig) -> Self {
        Self {
            paper_mode: config.paper_mode,
            live_post_only: config.live_post_only,
            live_order_ttl_ms: config.live_order_ttl.as_millis() as u64,
            live_order_max_age_ms: config.live_order_max_age.as_millis() as u64,
            live_reconcile_missing_grace_ms: config.live_reconcile_missing_grace.as_millis() as u64,
            live_max_submit_errors: config.live_max_submit_errors,
            live_max_cancel_errors: config.live_max_cancel_errors,
            live_kill_on_reconcile_mismatch: config.live_kill_on_reconcile_mismatch,
            paper_min_fill_notional_usd: config.paper_min_fill_notional_usd,
            paper_max_fills_per_order: config.paper_max_fills_per_order,
            paper_min_fill_interval_ms: config.paper_min_fill_interval.as_millis() as u64,
            paper_market_close_at_ms: config.paper_market_close_at_ms,
            paper_market_resolution_price: config.paper_market_resolution_price,
            paper_submit_latency_ms: config.paper_submit_latency_ms,
            paper_queue_depth_fraction: config.paper_queue_depth_fraction,
            paper_post_only_reject_probability: config.paper_post_only_reject_probability,
            paper_cancel_race_window_ms: config.paper_cancel_race_window_ms,
            paper_maker_rebate_coeff: config.paper_maker_rebate_coeff,
            paper_taker_fee_coeff_override: config.paper_taker_fee_coeff_override,
        }
    }
}

pub async fn run() -> Result<()> {
    let mut config = AppConfig::from_env()?;
    match std::env::var("WHALE_PAIR_EXEC_MODE")
        .unwrap_or_default()
        .as_str()
    {
        "live_smoke" => return run_live_smoke(config).await,
        "live_cancel" => return run_live_cancel(config).await,
        "live_reconcile" => return run_live_reconcile(config).await,
        "live_redeem" => return run_live_redeem(config).await,
        "shadow_live" => return run_shadow_live(config).await,
        "replay" => return run_replay_cli(config).await,
        _ => {}
    }
    // Suppress unused-must-use mut warning when no shadow path is taken.
    let _ = &mut config;
    run_with_config(config).await
}

/// Phase 5 paper env: replay mode. Reads a recorded book snapshot log
/// (the JSONL produced by `BookSnapshotWriter` during a prior live or
/// shadow_live run), drives `Runtime<StrategyMode>` through it
/// deterministically, routes any submits the strategy emits through
/// `paper_fill_from_book_snapshot` using the recorded timestamp as the
/// replay clock, and writes a `PaperReportSummary` JSON.
///
/// Inputs:
/// - `WHALE_PAIR_REPLAY_INPUT_PATH`: required. Path to the JSONL log.
/// - `WHALE_PAIR_PAPER_REPORT_PATH`: optional. Where to write the
///   resulting `paper_report.json`. Defaults to `<input>.replay.json`.
///
/// This is the foundation for A/B parameter calibration: change a paper
/// fill knob (queue depth, post-only reject prob, latency), re-run
/// replay against the same recorded log, and compare two report cards.
async fn run_replay_cli(config: AppConfig) -> Result<()> {
    crate::logging::init(&config)?;
    let input_path = std::env::var("WHALE_PAIR_REPLAY_INPUT_PATH")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(std::path::PathBuf::from)
        .ok_or_else(|| {
            anyhow::anyhow!("WHALE_PAIR_EXEC_MODE=replay requires WHALE_PAIR_REPLAY_INPUT_PATH")
        })?;
    let output_path = config.paper_report_path.clone().unwrap_or_else(|| {
        let mut p = input_path.clone();
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "replay".to_string());
        p.set_file_name(format!("{stem}.replay.json"));
        p
    });
    info!(
        target: "replay.startup",
        input = %input_path.display(),
        output = %output_path.display(),
        starting_cash_usd = config.starting_cash_usd,
        "replay mode engaged (strategy-driven)"
    );
    let records = crate::paper::replay::read_snapshot_log(&input_path)?;
    let started_at_ms = records.first().map(|r| r.t).unwrap_or_else(now_unix_ms);

    // Build runtime with the same shape as live, but no order_store /
    // journal / live adapter. Replay is in-memory only.
    let strategy = crate::strategy::StrategyMode::from_name(
        &config.strategy_name,
        config.strategy_profile.as_ref(),
    );
    // Capture fee coefficient before strategy is moved into Runtime so
    // replay's paper_fill_from_book_snapshot calls model fees correctly.
    // Bug fix: was passing 0.0, which made every replay-mode taker fill
    // appear fee-free and inflated reported P&L.
    let replay_taker_fee_coeff = strategy.taker_fee_coeff();
    let mut runtime = Runtime::new(
        crate::runtime::types::RuntimeConfig {
            starting_cash_usd: config.starting_cash_usd,
            event_log_capacity: config.event_log_capacity,
            initial_status: RuntimeStatus::Running,
            quote_engine_config: crate::quote_engine::QuoteEngineConfig::default(),
            quote_stale_ms: config.quote_min_order_age.as_millis() as u64,
        },
        config.risk_limits.clone(),
        strategy,
        MarketContextStore::empty(),
    );

    let execution_policy = ExecutionPolicy::from_config(&config);
    let mut paper_order_ctx: HashMap<ClientOrderId, PaperOrderContext> = HashMap::new();
    let mut report = crate::paper::report::PaperReportWriter::new(
        format!("replay-{}", now_unix_ms()),
        "replay",
        output_path.clone(),
        started_at_ms,
    );

    let mut submits = 0usize;
    let mut fills = 0usize;
    let mut rejects = 0usize;
    let mut cancels = 0usize;

    for record in records.iter() {
        let asset = record.asset.clone();
        let book = crate::paper::replay::ReplayBookRecord {
            t: record.t,
            asset: record.asset.clone(),
            bids: record.bids.clone(),
            asks: record.asks.clone(),
            last_trade: record.last_trade,
        }
        .into_book_state();
        // Retry-fill loop: attempt fills against this book on any existing
        // open order in the same instrument that hasn't filled yet.
        // Mirrors the production retry loop in execute_execution_adapter.
        let asset_instrument = InstrumentId::from(asset.as_str());
        let retry_targets: Vec<crate::runtime::types::ManagedOrder> = runtime
            .open_order_snapshots()
            .into_iter()
            .filter(|m| m.intent.instrument_id == asset_instrument && m.remaining_qty() > 1e-9)
            .collect();
        for managed in retry_targets {
            let intent = managed.intent.clone();
            let mid_at_submit = if book.best_bid > 0.0 && book.best_ask > 0.0 {
                Some((book.best_bid + book.best_ask) * 0.5)
            } else {
                None
            };
            let ctx = paper_order_context_mut(&mut paper_order_ctx, &intent, record.t);
            if let Some(fill) = paper_fill_from_book_snapshot(
                &book,
                &intent,
                record.t,
                replay_taker_fee_coeff,
                ctx,
                managed.remaining_qty(),
                &execution_policy,
            ) {
                report.record_fill(&fill, mid_at_submit);
                fills += 1;
                runtime.on_fill(fill)?;
            }
        }
        let market_id = MarketId::from(config.market_id_for_asset(&asset));
        let instrument_id = InstrumentId::from(asset.as_str());

        let outcome = runtime.on_book_state(market_id.clone(), instrument_id.clone(), &book)?;
        for command in outcome.commands {
            match command {
                RuntimeCommand::Submit(intent) => {
                    submits += 1;
                    if paper_post_only_should_reject(&intent, &book, &execution_policy) {
                        report.record_reject(
                            &intent.client_order_id,
                            &intent.market_id,
                            &intent.instrument_id,
                            intent.limit_price,
                            "post-only-cross-paper",
                            record.t,
                        );
                        rejects += 1;
                        runtime.on_order_rejected(
                            &intent.client_order_id,
                            "post-only-cross-paper",
                            record.t,
                        );
                        continue;
                    }
                    let mid_at_submit = if book.best_bid > 0.0 && book.best_ask > 0.0 {
                        Some((book.best_bid + book.best_ask) * 0.5)
                    } else {
                        None
                    };
                    if let Some(mid) = mid_at_submit {
                        report.record_submit_edge(
                            intent.side,
                            intent.limit_price,
                            intent.quantity,
                            mid,
                            record.t,
                        );
                    }
                    let ctx = paper_order_context_mut(&mut paper_order_ctx, &intent, record.t);
                    if let Some(fill) = paper_fill_from_book_snapshot(
                        &book,
                        &intent,
                        record.t,
                        replay_taker_fee_coeff,
                        ctx,
                        intent.quantity,
                        &execution_policy,
                    ) {
                        report.record_fill(&fill, mid_at_submit);
                        fills += 1;
                        runtime.on_fill(fill)?;
                    } else {
                        runtime.on_order_opened(&intent.client_order_id, record.t);
                    }
                }
                RuntimeCommand::Cancel {
                    client_order_id,
                    reason,
                } => {
                    cancels += 1;
                    paper_order_ctx.remove(&client_order_id);
                    runtime.on_order_cancelled(&client_order_id, reason, record.t);
                }
                RuntimeCommand::Merge(_) | RuntimeCommand::Redeem(_) | RuntimeCommand::Noop => {
                    // Replay does not exercise relayer/redeem in-process;
                    // treated as no-ops. Production runs handle these via
                    // execute_execution_adapter.
                }
            }
        }
    }
    report.flush()?;
    info!(
        target: "replay.complete",
        records_consumed = records.len(),
        submits,
        fills,
        rejects,
        cancels,
        report = %output_path.display(),
        "replay finished"
    );
    Ok(())
}

/// Phase 4 paper env: shadow-live mode. Connects live market_ws + spot_ws +
/// (optional) user_ws and runs the full strategy decisioning loop, but
/// forces paper_mode=true so every submit goes through PaperExecutionAdapter
/// rather than the live CLOB. Operators use this to validate strategy
/// behavior against the real book without exposing capital.
///
/// Per the design doc, the safety contract is: paper_mode is forced true
/// at this entry point regardless of WHALE_PAIR_PAPER_MODE — even if the
/// operator misconfigures the env, no live order can leave the engine.
async fn run_shadow_live(mut config: AppConfig) -> Result<()> {
    if !config.paper_mode {
        config.paper_mode = true;
    }
    // NOTE: deliberately NOT resetting quote_min_order_age. The whole point
    // of shadow_live is to mirror LIVE behavior with paper safety; the
    // operator's live-tier quote churn timing (often 5000ms in tinylive)
    // is what we want to validate. Forcing a paper default here would
    // defeat the realism goal.
    info!(
        target: "shadow_live.startup",
        clob_api_url = %config.clob_api_url,
        market_ws_url = %config.market_ws_url,
        spot_ws_url = %config.spot_ws_url,
        user_ws_url = %config.user_ws_url,
        user_auth_present = config.user_auth.is_some(),
        paper_report_path = ?config.paper_report_path,
        "shadow-live mode engaged: live feeds, paper submits"
    );
    run_with_config(config).await
}

async fn run_live_reconcile(config: AppConfig) -> Result<()> {
    crate::logging::init(&config)?;
    if config.paper_mode {
        anyhow::bail!("live reconcile mode requires WHALE_PAIR_PAPER_MODE=false");
    }
    let adapter = connect_live_adapter(&config).await?;
    let now_ms = now_unix_ms();
    let after_ms = std::env::var("WHALE_PAIR_LIVE_RECONCILE_AFTER_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_else(|| now_ms.saturating_sub(60 * 60 * 1_000));

    let open_orders = adapter.sync_open_orders().await?;
    info!(
        open_order_count = open_orders.len(),
        "live reconcile open orders synced"
    );
    for order in open_orders {
        info!(
            venue_order_id = %order.venue_order_id,
            client_order_id = ?order.client_order_id,
            market_id = %order.market_id,
            instrument_id = %order.instrument_id,
            side = ?order.side,
            price = order.limit_price,
            original_qty = order.original_qty,
            remaining_qty = order.remaining_qty,
            "live reconcile open order"
        );
    }

    let fills = adapter.sync_recent_fills(after_ms).await?;
    info!(
        fill_count = fills.len(),
        after_ms, "live reconcile recent fills synced"
    );
    for fill in fills {
        info!(
            venue_order_id = %fill.venue_order_id,
            client_order_id = ?fill.client_order_id,
            market_id = %fill.market_id,
            instrument_id = %fill.instrument_id,
            side = ?fill.side,
            price = fill.price,
            quantity = fill.quantity,
            liquidity = ?fill.liquidity,
            observed_at_ms = fill.observed_at_ms,
            "live reconcile recent fill"
        );
    }

    let balances = adapter.sync_balances().await?;
    info!(
        cash_usd = balances.cash_usd,
        position_count = balances.positions.len(),
        observed_at_ms = balances.observed_at_ms,
        "live reconcile balances synced"
    );

    let mut seen_conditions: std::collections::HashSet<String> = std::collections::HashSet::new();
    for position in &balances.positions {
        let Some(condition_id) = position
            .condition_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if !seen_conditions.insert(condition_id.to_string()) {
            continue;
        }
        match adapter.fetch_market_metadata(condition_id).await {
            Ok(md) => info!(
                target: "live_reconcile.venue_metadata",
                condition_id = %md.condition_id,
                minimum_order_size = md.minimum_order_size,
                minimum_tick_size = md.minimum_tick_size,
                neg_risk = md.neg_risk,
                active = md.active,
                closed = md.closed,
                "venue metadata (compare against strategy hardcoded sizing — Q6)"
            ),
            Err(error) => warn!(
                target: "live_reconcile.venue_metadata",
                condition_id = %condition_id,
                error = %error,
                "failed to fetch venue metadata for known position"
            ),
        }
    }
    Ok(())
}

async fn run_live_smoke(config: AppConfig) -> Result<()> {
    crate::logging::init(&config)?;
    if config.paper_mode {
        anyhow::bail!("live smoke mode requires WHALE_PAIR_PAPER_MODE=false");
    }
    if config
        .live_kill_switch_path
        .as_ref()
        .is_some_and(|path| path.exists())
    {
        anyhow::bail!("live smoke blocked by active kill switch");
    }
    let adapter = connect_live_adapter(&config).await?;

    let asset_id = std::env::var("WHALE_PAIR_LIVE_SMOKE_ASSET_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| config.market_assets.first().cloned())
        .ok_or_else(|| anyhow::anyhow!("live smoke mode requires an asset id"))?;
    let market_id = std::env::var("WHALE_PAIR_LIVE_SMOKE_MARKET_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| config.market_id_for_asset(&asset_id));
    let price = std::env::var("WHALE_PAIR_LIVE_SMOKE_PRICE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.01);
    let notional = std::env::var("WHALE_PAIR_LIVE_SMOKE_NOTIONAL_USD")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(1.0);
    if price <= 0.0 || notional <= 0.0 {
        anyhow::bail!("live smoke price and notional must be positive");
    }
    let time_in_force = match std::env::var("WHALE_PAIR_LIVE_SMOKE_TIME_IN_FORCE")
        .unwrap_or_else(|_| "GTD".to_string())
        .trim()
        .to_ascii_uppercase()
        .as_str()
    {
        "GTC" => TimeInForce::Gtc,
        "GTD" => TimeInForce::Gtd,
        other => anyhow::bail!("unsupported WHALE_PAIR_LIVE_SMOKE_TIME_IN_FORCE={other}"),
    };
    let now_ms = now_unix_ms();
    let ttl_ms = config.live_order_ttl.as_millis().max(5_000) as u64;
    let client_order_id = ClientOrderId::from(format!("live-smoke:{now_ms}:{asset_id}"));
    let submit = SubmitOrderRequest {
        client_order_id: client_order_id.clone(),
        market_id: MarketId::from(market_id),
        instrument_id: InstrumentId::from(asset_id),
        side: TradeSide::Buy,
        limit_price: price,
        quantity: notional / price,
        post_only: true,
        time_in_force,
        expires_at_ms: if matches!(time_in_force, TimeInForce::Gtd) {
            Some(now_ms.saturating_add(ttl_ms))
        } else {
            None
        },
        strategy_tag: "live-smoke".to_string(),
        quote_level_tag: Some("far-touch-smoke".to_string()),
        submitted_at_ms: now_ms,
    };
    info!(
        client_order_id = %submit.client_order_id,
        instrument_id = %submit.instrument_id,
        price = submit.limit_price,
        quantity = submit.quantity,
        "submitting live smoke order"
    );
    let ack = adapter.submit(submit).await?;
    if !ack.accepted {
        anyhow::bail!(
            "live smoke submit rejected by venue: {}",
            ack.venue_message.unwrap_or_else(|| "unknown".to_string())
        );
    }
    let open_orders = adapter.sync_open_orders().await?;
    let venue_order_id = ack.venue_order_id.clone();
    let visible = venue_order_id.as_ref().is_some_and(|order_id| {
        open_orders
            .iter()
            .any(|order| &order.venue_order_id == order_id)
    });
    if !visible {
        anyhow::bail!(
            "live smoke order was accepted but not visible in open-order sync; venue_order_id={:?}",
            venue_order_id
        );
    }
    let cancel = adapter
        .cancel(CancelOrderRequest {
            client_order_id: client_order_id.clone(),
            venue_order_id,
            reason: "live smoke cancel".to_string(),
            submitted_at_ms: now_unix_ms(),
        })
        .await?;
    if !cancel.accepted {
        anyhow::bail!(
            "live smoke cancel rejected by venue: {}",
            cancel
                .venue_message
                .unwrap_or_else(|| "unknown".to_string())
        );
    }
    let open_after = adapter.sync_open_orders().await?;
    if let Some(order_id) = cancel.venue_order_id.as_ref() {
        if open_after
            .iter()
            .any(|order| &order.venue_order_id == order_id)
        {
            anyhow::bail!("live smoke order still visible after cancel: {order_id}");
        }
    }
    info!("live smoke submit/cancel/reconcile completed");
    Ok(())
}

async fn run_live_cancel(config: AppConfig) -> Result<()> {
    crate::logging::init(&config)?;
    if config.paper_mode {
        anyhow::bail!("live cancel mode requires WHALE_PAIR_PAPER_MODE=false");
    }
    let raw_order_ids = std::env::var("WHALE_PAIR_LIVE_CANCEL_ORDER_IDS")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("live cancel mode requires WHALE_PAIR_LIVE_CANCEL_ORDER_IDS")
        })?;
    let order_ids = raw_order_ids
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(OrderId::from)
        .collect::<Vec<_>>();
    if order_ids.is_empty() {
        anyhow::bail!("live cancel mode received no order ids");
    }

    let adapter = connect_live_adapter(&config).await?;
    for order_id in order_ids {
        let client_order_id = ClientOrderId::from(format!("manual-cancel:{order_id}"));
        info!(venue_order_id = %order_id, "submitting manual live cancel");
        let ack = adapter
            .cancel(CancelOrderRequest {
                client_order_id,
                venue_order_id: Some(order_id.clone()),
                reason: "operator live_cancel".to_string(),
                submitted_at_ms: now_unix_ms(),
            })
            .await?;
        if !ack.accepted {
            anyhow::bail!(
                "manual live cancel rejected for {}: {}",
                order_id,
                ack.venue_message.unwrap_or_else(|| "unknown".to_string())
            );
        }
        info!(venue_order_id = %order_id, "manual live cancel accepted");
    }

    let open_after = adapter.sync_open_orders().await?;
    let visible_after = open_after
        .iter()
        .map(|order| order.venue_order_id.to_string())
        .collect::<Vec<_>>();
    info!(
        remaining_open_orders = visible_after.len(),
        remaining_venue_order_ids = ?visible_after,
        "manual live cancel reconciliation complete"
    );
    Ok(())
}

/// Live redeem mode: scans the wallet's positions via the Polymarket
/// Data API, filters to positions whose underlying market has resolved
/// (`redeemable: true`), and submits one CTF redeem per unique
/// `condition_id` through the relayer. Recovers stranded collateral
/// from expired positions that would otherwise tie up capital.
///
/// Honors `WHALE_PAIR_LIVE_REDEEM_DRY_RUN=true` to log the planned
/// redemptions without submitting (useful before risking gas).
async fn run_live_redeem(config: AppConfig) -> Result<()> {
    crate::logging::init(&config)?;
    if config.paper_mode {
        anyhow::bail!("live redeem mode requires WHALE_PAIR_PAPER_MODE=false");
    }
    if config
        .live_kill_switch_path
        .as_ref()
        .is_some_and(|path| path.exists())
    {
        anyhow::bail!("live redeem blocked by active kill switch");
    }
    let dry_run = std::env::var("WHALE_PAIR_LIVE_REDEEM_DRY_RUN")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "yes"
            )
        })
        .unwrap_or(false);

    let adapter = connect_live_adapter(&config).await?;
    let balances = adapter.sync_balances().await?;
    info!(
        position_count = balances.positions.len(),
        cash_usd = balances.cash_usd,
        positions_authoritative = balances.positions_authoritative,
        "live redeem: fetched venue positions"
    );

    // Group redeemable positions by condition_id. Both legs of a
    // resolved binary market share one condition_id; we want one
    // redeem call per condition that claims both legs (winning side
    // pays out, losing side returns 0 atomically).
    let mut redeemable_by_condition: std::collections::BTreeMap<String, Vec<&VenuePosition>> =
        std::collections::BTreeMap::new();
    let mut total_value_usd = 0.0;
    for position in &balances.positions {
        if !position.redeemable {
            continue;
        }
        let Some(condition_id) = position.condition_id.as_ref() else {
            warn!(
                instrument_id = %position.instrument_id,
                "redeemable position missing condition_id; skipping"
            );
            continue;
        };
        total_value_usd += position.current_value_usd;
        redeemable_by_condition
            .entry(condition_id.clone())
            .or_default()
            .push(position);
    }

    info!(
        condition_count = redeemable_by_condition.len(),
        total_value_usd, dry_run, "live redeem: identified redeemable positions"
    );
    if redeemable_by_condition.is_empty() {
        info!("live redeem: no redeemable positions found; nothing to do");
        return Ok(());
    }

    let mut submitted = 0_usize;
    let mut failed = 0_usize;
    for (condition_id, positions) in &redeemable_by_condition {
        let market_id = positions
            .first()
            .map(|p| p.market_id.clone())
            .unwrap_or_else(|| MarketId::from(condition_id.as_str()));
        let value_usd: f64 = positions.iter().map(|p| p.current_value_usd).sum();
        let leg_summary: Vec<String> = positions
            .iter()
            .map(|p| {
                format!(
                    "{}={:.2}sh@${:.2}",
                    p.instrument_id, p.quantity, p.current_value_usd
                )
            })
            .collect();
        info!(
            condition_id = %condition_id,
            market_id = %market_id,
            legs = ?leg_summary,
            value_usd,
            dry_run,
            "live redeem: planning redemption"
        );
        if dry_run {
            continue;
        }
        let now_ms = now_unix_ms();
        let request = RedeemPositionsRequest {
            command_id: ClientOrderId::from(format!("manual-redeem:{condition_id}:{now_ms}")),
            market_id,
            condition_id: condition_id.clone(),
            // [1, 2] redeems both binary outcomes atomically; loss leg
            // returns 0 collateral but the call succeeds.
            index_sets: vec![1, 2],
            submitted_at_ms: now_ms,
        };
        match adapter.redeem_positions(request).await {
            Ok(ack) => {
                submitted += 1;
                info!(
                    condition_id = %condition_id,
                    venue_message = ack.venue_message.as_deref().unwrap_or("(none)"),
                    "live redeem: submitted"
                );
            }
            Err(error) => {
                failed += 1;
                warn!(
                    condition_id = %condition_id,
                    error = %error,
                    "live redeem: submission failed"
                );
            }
        }
    }

    info!(
        planned = redeemable_by_condition.len(),
        submitted, failed, dry_run, "live redeem: complete"
    );
    if failed > 0 && !dry_run {
        anyhow::bail!(
            "live redeem: {failed} of {} submissions failed (see logs)",
            redeemable_by_condition.len()
        );
    }
    Ok(())
}

pub async fn run_with_config(config: AppConfig) -> Result<()> {
    crate::logging::init(&config)?;

    let metrics = Arc::new(AppMetrics::new()?);
    let mut market_contexts = match &config.market_context_path {
        Some(path) => MarketContextStore::load_json(path)?,
        None => MarketContextStore::empty(),
    };
    if config.market_discovery_enabled {
        match fetch_btc_5m_market_contexts(&config, now_unix_ms()).await {
            Ok(discovered) if discovered.len() > 0 => {
                info!(
                    target: "market_discovery",
                    market_count = discovered.len(),
                    "loaded initial market universe from engine discovery"
                );
                market_contexts = discovered;
            }
            Ok(_) => warn!(
                target: "market_discovery",
                "initial market discovery returned no markets; falling back to configured context"
            ),
            Err(error) => warn!(
                target: "market_discovery",
                error = %error,
                "initial market discovery failed; falling back to configured context"
            ),
        }
    }
    let initial_universe =
        RuntimeMarketUniverse::from_config_and_context(&config, &market_contexts);
    if initial_universe.market_assets.is_empty() {
        anyhow::bail!("no market assets configured or discovered");
    }
    let books = Arc::new(BookStore::new(&initial_universe.market_assets));
    let market_universe = Arc::new(RwLock::new(initial_universe.clone()));
    let (market_assets_tx, market_assets_rx) =
        watch::channel(initial_universe.market_assets.clone());
    let (user_markets_tx, user_markets_rx) = watch::channel(initial_universe.user_markets.clone());
    let strategy = StrategyMode::from_name(&config.strategy_name, config.strategy_profile.as_ref());
    let strategy_name = strategy.name().to_string();
    let paper_fee_coeff = strategy.taker_fee_coeff();
    let shutdown = CancellationToken::new();
    let order_store = config
        .order_store_path
        .as_ref()
        .map(
            |path| -> Result<Box<dyn crate::runtime::order_store::OrderStore>> {
                Ok(Box::new(SqliteOrderStore::open(path)?))
            },
        )
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
        config
            .runtime_run_id
            .clone()
            .unwrap_or_else(|| format!("run-{}", now_unix_ms())),
    );
    runtime.set_quote_reconciler_config(ReconcilerConfig {
        min_order_age_ms: config.quote_min_order_age.as_millis() as u64,
        max_churn_per_window: config.quote_max_churn_per_window,
        churn_window_ms: config.quote_churn_window.as_millis() as u64,
        hard_pull_ms: config.quote_hard_pull.as_millis() as u64,
        max_submit_per_window: config.quote_max_submit_per_window,
        max_replace_per_window: config.quote_max_replace_per_window,
        max_cancel_per_window: config.quote_max_cancel_per_window,
        ..ReconcilerConfig::default()
    });
    let mut journal = config
        .journal_path
        .clone()
        .map(|path| JournalWriter::open_with_rotation(path, config.journal_rotate_bytes))
        .transpose()?;
    let mut audit = config
        .audit_path
        .as_deref()
        .map(|path| AuditWriter::open(path, config.journal_rotate_bytes))
        .transpose()?;
    let mut paper_report: Option<crate::paper::report::PaperReportWriter> = if config.paper_mode {
        config.paper_report_path.clone().map(|path| {
            crate::paper::report::PaperReportWriter::new(
                runtime.run_id().to_string(),
                "paper",
                path,
                now_unix_ms(),
            )
        })
    } else {
        None
    };
    let mut book_snapshot: Option<crate::paper::snapshot::BookSnapshotWriter> = config
        .book_snapshot_log_path
        .as_deref()
        .map(crate::paper::snapshot::BookSnapshotWriter::open)
        .transpose()?;

    let mut startup_outcome = runtime.recover_from_store(
        now_unix_ms(),
        config.order_reconcile_stale_window.as_millis() as u64,
    );
    if !config.paper_mode && runtime.has_needs_reconcile_orders() {
        startup_outcome.extend(
            runtime.degrade_and_cancel_all(
                now_unix_ms(),
                "startup has orders requiring reconciliation",
            ),
        );
    } else {
        startup_outcome.extend(runtime.start(now_unix_ms()));
    }
    persist_runtime_outcome(
        &mut journal,
        metrics.as_ref(),
        runtime.event_log(),
        "startup",
        startup_outcome.clone(),
    )?;
    persist_audit_outcome(&mut audit, "startup", &runtime, &startup_outcome)?;
    persist_runtime_checkpoint(&mut journal, &runtime, now_unix_ms(), "startup")?;
    let mut effective_user_auth = config.user_auth.clone();
    let execution_adapter: Arc<dyn ExecutionAdapter> = match config.paper_mode {
        true => Arc::new(PaperExecutionAdapter::new()),
        false => {
            let live_connection = connect_live_session(&config).await?;
            effective_user_auth = live_connection.user_auth;
            Arc::new(live_connection.adapter)
        }
    };
    metrics.set_execution_adapter_connected(true);
    let dashboard_state = Arc::new(RwLock::new(DashboardSnapshot::default()));
    refresh_dashboard_state(
        &mut runtime,
        &books,
        metrics.as_ref(),
        &config,
        dashboard_state.clone(),
        &initial_universe.market_assets,
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
        assets = ?initial_universe.market_assets,
        user_markets = ?initial_universe.user_markets,
        metrics_bind = %config.metrics_bind,
        loop_interval_ms = config.runtime_loop_interval.as_millis(),
        book_stale_after_ms = config.book_stale_after.as_millis(),
        starting_cash_usd = config.starting_cash_usd,
        event_log_capacity = config.event_log_capacity,
        journal_path = ?config.journal_path,
        journal_rotate_bytes = ?config.journal_rotate_bytes,
        paper_mode = config.paper_mode,
        "starting whale pair execution scaffold"
    );

    let metrics_handle = spawn_metrics(
        metrics.clone(),
        &config,
        dashboard_state.clone(),
        shutdown.child_token(),
    );
    let market_ws_handle = spawn_market_ws(
        metrics.clone(),
        books.clone(),
        &config,
        market_assets_rx,
        shutdown.child_token(),
    );
    let (spot_trade_tx, spot_trade_rx) = mpsc::unbounded_channel();
    let spot_ws_handle = spawn_spot_ws(
        metrics.clone(),
        &config,
        Some(spot_trade_tx),
        shutdown.child_token(),
    );
    let (user_order_tx, user_order_rx) = mpsc::unbounded_channel();
    let user_ws_handle = spawn_user_ws(
        metrics.clone(),
        &config,
        effective_user_auth,
        Some(user_order_tx),
        user_markets_rx,
        shutdown.child_token(),
    );
    let mut paper_order_ctx = HashMap::<ClientOrderId, PaperOrderContext>::new();
    let mut execution_venue_map = HashMap::<ClientOrderId, Option<OrderId>>::new();
    let mut live_safety = LiveSafetyState::default();
    let execution_policy = ExecutionPolicy::from_config(&config);

    run_runtime_loop(
        &config,
        &execution_policy,
        &books,
        paper_fee_coeff,
        metrics.clone(),
        shutdown.clone(),
        &mut runtime,
        &mut journal,
        &mut audit,
        &mut paper_report,
        &mut book_snapshot,
        &mut paper_order_ctx,
        &mut execution_venue_map,
        &mut live_safety,
        execution_adapter,
        market_universe,
        market_assets_tx,
        user_markets_tx,
        spot_trade_rx,
        user_order_rx,
        dashboard_state.clone(),
        config.dashboard_event_limit,
        strategy_name.as_str(),
    )
    .await?;
    if let Some(report) = paper_report.as_mut() {
        if let Some(whale_path) = config.dashboard_whale_events_path.as_deref() {
            let session_start = report.started_at_ms();
            let session_end = now_unix_ms();
            let whale_events = crate::wire::api::load_whale_events(whale_path, 100_000);
            let mut ingested = 0usize;
            for ev in whale_events {
                if ev.observed_at_ms < session_start || ev.observed_at_ms > session_end {
                    continue;
                }
                let notional = ev
                    .notional_usd
                    .or_else(|| ev.price.and_then(|p| ev.quantity.map(|q| p * q)))
                    .unwrap_or(0.0);
                // Bug fix: skip events with no derivable notional rather
                // than recording a $0 fill. Otherwise the vs_whale section
                // looks like the whale traded at $0 and double-counts in
                // the maker_fraction / capture-ratio math.
                if notional <= 0.0 {
                    continue;
                }
                report.record_whale_fill_observed(ev.observed_at_ms, ev.side.as_deref(), notional);
                ingested += 1;
            }
            info!(
                target: "paper_report.vs_whale",
                whale_events_ingested = ingested,
                source = %whale_path.display(),
                "whale events ingested into vs_whale section"
            );
        }
        if let Err(error) = report.flush() {
            warn!(
                target: "paper_report.flush",
                output = %report.output_path().display(),
                error = %error,
                "failed to flush paper report on shutdown"
            );
        } else {
            info!(
                target: "paper_report.flush",
                output = %report.output_path().display(),
                "paper report written"
            );
        }
    }
    if let Some(snap) = book_snapshot.as_mut() {
        if let Err(error) = snap.flush() {
            warn!(
                target: "book_snapshot.flush",
                output = %snap.path().display(),
                error = %error,
                "failed to flush book snapshot log on shutdown"
            );
        } else {
            info!(
                target: "book_snapshot.flush",
                output = %snap.path().display(),
                bytes_written = snap.bytes_written(),
                "book snapshot log flushed"
            );
        }
    }

    shutdown.cancel();
    join_task("market-ws", market_ws_handle).await;
    join_task("spot-ws", spot_ws_handle).await;
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
    assets_rx: watch::Receiver<Vec<String>>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    let client = MarketWsClient::new(
        config.market_ws_url.clone(),
        config.market_assets.clone(),
        config.ping_interval,
        books,
        metrics,
    )
    .with_asset_updates(assets_rx);
    tokio::spawn(async move {
        client.run(shutdown).await;
    })
}

fn spawn_spot_ws(
    metrics: Arc<AppMetrics>,
    config: &AppConfig,
    event_tx: Option<mpsc::UnboundedSender<SpotTradeEvent>>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    let client = SpotWsClient::new(
        config.spot_ws_url.clone(),
        config.spot_symbol.clone(),
        config.ping_interval,
        metrics,
        event_tx,
    );
    tokio::spawn(async move {
        client.run(shutdown).await;
    })
}

fn spawn_user_ws(
    metrics: Arc<AppMetrics>,
    config: &AppConfig,
    user_auth: Option<UserWsAuth>,
    event_tx: Option<mpsc::UnboundedSender<UserOrderEvent>>,
    markets_rx: watch::Receiver<Vec<String>>,
    shutdown: CancellationToken,
) -> Option<JoinHandle<()>> {
    let auth = match user_auth {
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
    )
    .with_market_updates(markets_rx);
    Some(tokio::spawn(async move {
        client.run(shutdown).await;
    }))
}

async fn run_runtime_loop(
    config: &AppConfig,
    execution_policy: &ExecutionPolicy,
    books: &Arc<BookStore>,
    paper_fee_coeff: f64,
    metrics: Arc<AppMetrics>,
    shutdown: CancellationToken,
    runtime: &mut Runtime<StrategyMode>,
    journal: &mut Option<JournalWriter>,
    audit: &mut Option<AuditWriter>,
    paper_report: &mut Option<crate::paper::report::PaperReportWriter>,
    book_snapshot: &mut Option<crate::paper::snapshot::BookSnapshotWriter>,
    paper_order_ctx: &mut HashMap<ClientOrderId, PaperOrderContext>,
    execution_venue_map: &mut HashMap<ClientOrderId, Option<OrderId>>,
    live_safety: &mut LiveSafetyState,
    execution_adapter: Arc<dyn ExecutionAdapter>,
    market_universe: Arc<RwLock<RuntimeMarketUniverse>>,
    market_assets_tx: watch::Sender<Vec<String>>,
    user_markets_tx: watch::Sender<Vec<String>>,
    mut spot_events: mpsc::UnboundedReceiver<SpotTradeEvent>,
    mut user_events: mpsc::UnboundedReceiver<UserOrderEvent>,
    dashboard: Arc<RwLock<DashboardSnapshot>>,
    dashboard_event_limit: usize,
    strategy_name: &str,
) -> Result<()> {
    let live_health_started_at_ms = now_unix_ms();
    let mut ticks = interval(config.runtime_loop_interval);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut reconcile_ticks = interval(config.order_reconcile_interval);
    reconcile_ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut checkpoint_ticks = interval(config.runtime_checkpoint_interval);
    checkpoint_ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut summaries = interval(config.summary_log_interval);
    summaries.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut market_discovery_ticks = interval(config.market_discovery_interval);
    market_discovery_ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Auto-redeem worker — periodic sweep that scans venue positions
    // for resolved markets and submits CTF redeems via the relayer.
    // Default interval 60s; disabled in paper mode (no real positions
    // to redeem). Operator can disable via WHALE_PAIR_LIVE_AUTO_REDEEM=false
    // (default true so live deployments don't accumulate stranded
    // collateral). 60s is well above the 5-min market cycle so we
    // never spam the relayer.
    let auto_redeem_enabled = !config.paper_mode
        && std::env::var("WHALE_PAIR_LIVE_AUTO_REDEEM")
            .ok()
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "0" | "no"))
            .unwrap_or(true);
    let auto_redeem_period = std::env::var("WHALE_PAIR_LIVE_AUTO_REDEEM_PERIOD_SEC")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(60);
    let mut auto_redeem_ticks = interval(std::time::Duration::from_secs(auto_redeem_period));
    auto_redeem_ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // In-process dedup so we don't re-submit the same condition_id
    // before the relayer has settled the previous redeem. Cleared on
    // restart; the venue's own positions sync re-discovers anything
    // still pending.
    let mut auto_redeem_seen_conditions: HashSet<String> = HashSet::new();

    let mut spot_events_open = true;
    let mut user_events_open = true;
    let mut seen_venue_fill_keys = HashSet::<String>::new();
    let mut paper_market_closed = false;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl-c received; shutting down");
                return Ok(());
            }
            _ = shutdown.cancelled() => {
                return Ok(());
            }
            maybe_spot_event = spot_events.recv(), if spot_events_open => {
                match maybe_spot_event {
                    Some(event) => {
                        runtime.on_btc_trade(event.price, event.observed_at_ms);
                    }
                    None => {
                        spot_events_open = false;
                        info!("spot websocket event channel closed; disabling btc regime updates");
                    }
                }
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
                            metrics.as_ref(),
                            runtime.event_log(),
                            "user-ws",
                            user_outcome.clone(),
                        )?;
                        persist_audit_outcome(audit, "user-ws", runtime, &user_outcome)?;
                        let assets = market_universe.read().await.market_assets.clone();
                        refresh_dashboard_state(
                            runtime,
                            &books,
                            metrics.as_ref(),
                            &config,
                            dashboard.clone(),
                            &assets,
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
            _ = market_discovery_ticks.tick(), if config.market_discovery_enabled => {
                match refresh_runtime_market_universe(
                    config,
                    runtime,
                    &market_universe,
                    &market_assets_tx,
                    &user_markets_tx,
                    now_unix_ms(),
                )
                .await {
                    Ok(Some(outcome)) => {
                        let assets = market_universe.read().await.market_assets.clone();
                        let combined = execute_execution_adapter(
                            runtime,
                            books,
                            &assets,
                            paper_fee_coeff,
                            metrics.as_ref(),
                            outcome,
                            paper_order_ctx,
                            execution_venue_map,
                            live_safety,
                            execution_adapter.clone(),
                            execution_policy,
                            &mut seen_venue_fill_keys,
                            paper_report.as_mut(),
                        )
                        .await?;
                        persist_runtime_outcome(
                            journal,
                            metrics.as_ref(),
                            runtime.event_log(),
                            "market-discovery",
                            combined.clone(),
                        )?;
                        persist_audit_outcome(audit, "market-discovery", runtime, &combined)?;
                        refresh_dashboard_state(
                            runtime,
                            &books,
                            metrics.as_ref(),
                            &config,
                            dashboard.clone(),
                            &assets,
                            strategy_name,
                            dashboard_event_limit,
                        )
                        .await?;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        warn!(
                            target: "market_discovery",
                            error = %error,
                            "market discovery refresh failed; keeping current universe"
                        );
                    }
                }
            }
            _ = ticks.tick() => {
                let _timer = metrics.runtime_loop_timer();
                metrics.refresh_stream_ages();
                let current_universe = market_universe.read().await.clone();
                let current_assets = current_universe.market_assets.clone();
                if execution_policy.paper_mode && !paper_market_closed {
                    if let Some(close_at_ms) = execution_policy.paper_market_close_at_ms {
                        let now = now_unix_ms();
                        if now >= close_at_ms {
                            paper_market_closed = true;
                            info!(
                                target: "paper_env.market_close",
                                close_at_ms,
                                now_ms = now,
                                resolution_price = ?execution_policy.paper_market_resolution_price,
                                "paper market close triggered"
                            );
                            let close_outcome = runtime.plan_paper_close(
                                now,
                                execution_policy.paper_market_resolution_price,
                            );
                            let combined = execute_execution_adapter(
                                runtime,
                                books,
                                &current_assets,
                                paper_fee_coeff,
                                metrics.as_ref(),
                                close_outcome,
                                paper_order_ctx,
                                execution_venue_map,
                                live_safety,
                                execution_adapter.clone(),
                                execution_policy,
                                &mut seen_venue_fill_keys,
                                paper_report.as_mut(),
                            )
                            .await?;
                            persist_runtime_outcome(
                                journal,
                                metrics.as_ref(),
                                runtime.event_log(),
                                "paper-market-close",
                                combined.clone(),
                            )?;
                            persist_audit_outcome(audit, "paper-market-close", runtime, &combined)?;
                            refresh_dashboard_state(
                                runtime,
                                books,
                                metrics.as_ref(),
                                &config,
                                dashboard.clone(),
                                &current_assets,
                                strategy_name,
                                dashboard_event_limit,
                            )
                            .await?;
                        }
                    }
                }
                let capital_outcome = enforce_capital_guard(
                    runtime,
                    metrics.as_ref(),
                    &config.risk_limits,
                    config.starting_cash_usd,
                    now_unix_ms(),
                    if config.paper_mode { "paper" } else { "live" },
                );
                if !capital_outcome.event_seqs.is_empty() || !capital_outcome.commands.is_empty() {
                    let combined = execute_execution_adapter(
                        runtime,
                        books,
                        &current_assets,
                        paper_fee_coeff,
                        metrics.as_ref(),
                        capital_outcome,
                        paper_order_ctx,
                        execution_venue_map,
                        live_safety,
                        execution_adapter.clone(),
                        execution_policy,
                        &mut seen_venue_fill_keys,
                        paper_report.as_mut(),
                    )
                    .await?;
                    persist_runtime_outcome(
                        journal,
                        metrics.as_ref(),
                        runtime.event_log(),
                        "capital-guard",
                        combined.clone(),
                    )?;
                    persist_audit_outcome(audit, "capital-guard", runtime, &combined)?;
                }
                if !config.paper_mode {
                    let health_outcome = enforce_live_health(
                        runtime,
                        metrics.as_ref(),
                        config,
                        live_safety,
                        now_unix_ms(),
                        live_health_started_at_ms,
                    );
                    if !health_outcome.event_seqs.is_empty() || !health_outcome.commands.is_empty() {
                        let combined = execute_execution_adapter(
                            runtime,
                            books,
                            &current_assets,
                            paper_fee_coeff,
                            metrics.as_ref(),
                            health_outcome,
                            paper_order_ctx,
                            execution_venue_map,
                            live_safety,
                            execution_adapter.clone(),
                            execution_policy,
                            &mut seen_venue_fill_keys,
                            paper_report.as_mut(),
                        )
                        .await?;
                        persist_runtime_outcome(
                            journal,
                            metrics.as_ref(),
                            runtime.event_log(),
                            "live-health",
                            combined.clone(),
                        )?;
                        persist_audit_outcome(audit, "live-health", runtime, &combined)?;
                    }
                }
                for asset_id in &current_assets {
                    match books.snapshot(asset_id).await {
                        Some(book) if book.last_update_unix_ms > 0 => {
                            if let Some(snap) = book_snapshot.as_mut() {
                                if let Err(error) =
                                    snap.record(&book, config.book_snapshot_max_levels)
                                {
                                    warn!(
                                        target: "book_snapshot",
                                        asset = %asset_id,
                                        error = %error,
                                        "failed to append book snapshot record"
                                    );
                                }
                            }
                            metrics.observe_book(&book, config.book_stale_after);
                            let (c10, c30, c60, last_age_ms) =
                                books.trade_activity(asset_id, now_unix_ms()).await;
                            runtime.on_market_activity(
                                InstrumentId::from(asset_id.as_str()),
                                crate::signals::MarketActivitySignal {
                                    last_trade_event_count_10s: c10,
                                    last_trade_event_count_30s: c30,
                                    last_trade_event_count_60s: c60,
                                    last_trade_event_age_ms: last_age_ms,
                                },
                            );
                            let outcome = runtime.on_book_state(
                                MarketId::from(
                                    current_universe.market_id_for_asset(config, asset_id),
                                ),
                                InstrumentId::from(asset_id.as_str()),
                                &book,
                            )?;
                            let combined = execute_execution_adapter(
                                runtime,
                                books,
                                &current_assets,
                                paper_fee_coeff,
                                metrics.as_ref(),
                                outcome,
                                paper_order_ctx,
                                execution_venue_map,
                                live_safety,
                                execution_adapter.clone(),
                                execution_policy,
                                &mut seen_venue_fill_keys,
                                paper_report.as_mut(),
                            )
                            .await?;
                            persist_runtime_outcome(
                                journal,
                                metrics.as_ref(),
                                runtime.event_log(),
                                "book",
                                combined.clone(),
                            )?;
                            persist_audit_outcome(audit, "book", runtime, &combined)?;
                            refresh_dashboard_state(
                                runtime,
                                books,
                                metrics.as_ref(),
                                &config,
                                dashboard.clone(),
                                &current_assets,
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
                let needs_reconcile_before = needs_reconcile_order_count(runtime);
                let reconcile_outcome = runtime.reconcile_open_orders(
                    now_unix_ms(),
                    config.order_reconcile_stale_window.as_millis() as u64,
                );
                let needs_reconcile_after = needs_reconcile_order_count(runtime);
                for _ in needs_reconcile_before..needs_reconcile_after {
                    metrics.observe_reconcile_failure();
                }
                persist_runtime_outcome(
                    journal,
                    metrics.as_ref(),
                    runtime.event_log(),
                    "reconcile",
                    reconcile_outcome.clone(),
                )?;
                persist_audit_outcome(audit, "reconcile", runtime, &reconcile_outcome)?;
                metrics.touch_reconcile();
                metrics.refresh_stream_ages();
                // Sweep open orders for maker-rebate eligibility. No-op
                // for paper adapter (default trait impl returns empty
                // map). Live adapter calls /orders-scoring batch via
                // the cached V2 SDK client. Cheap (one HTTP per sweep).
                if !config.paper_mode {
                    match execution_adapter.sync_open_orders().await {
                        Ok(venue_orders) if !venue_orders.is_empty() => {
                            let ids: Vec<String> = venue_orders
                                .iter()
                                .map(|o| o.venue_order_id.to_string())
                                .collect();
                            let id_refs: Vec<&str> =
                                ids.iter().map(|s| s.as_str()).collect();
                            match execution_adapter
                                .check_orders_scoring(&id_refs)
                                .await
                            {
                                Ok(map) => {
                                    let scoring = map.values().filter(|s| **s).count();
                                    let non_scoring = map.len() - scoring;
                                    metrics.record_order_scoring_counts(scoring, non_scoring);
                                }
                                Err(error) => {
                                    warn!(error = %error, "order-scoring sweep failed (non-fatal)");
                                }
                            }
                        }
                        Ok(_) => {
                            metrics.record_order_scoring_counts(0, 0);
                        }
                        Err(error) => {
                            warn!(error = %error, "sync_open_orders for scoring sweep failed (non-fatal)");
                        }
                    }
                }
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
            _ = auto_redeem_ticks.tick(), if auto_redeem_enabled => {
                // Scan venue for redeemable positions and submit one
                // CTF redeem per unique condition_id (binary market both
                // legs). Idempotent within process lifetime via the
                // seen_conditions set; restarts re-discover from venue.
                match execution_adapter.sync_balances().await {
                    Ok(balances) => {
                        let mut by_condition: std::collections::BTreeMap<String, Vec<&VenuePosition>> =
                            std::collections::BTreeMap::new();
                        for position in &balances.positions {
                            if !position.redeemable {
                                continue;
                            }
                            let Some(condition_id) = position.condition_id.as_ref() else {
                                continue;
                            };
                            if auto_redeem_seen_conditions.contains(condition_id) {
                                continue;
                            }
                            by_condition
                                .entry(condition_id.clone())
                                .or_default()
                                .push(position);
                        }
                        if !by_condition.is_empty() {
                            info!(
                                target: "auto_redeem",
                                condition_count = by_condition.len(),
                                "auto-redeem: planning sweep of resolved positions"
                            );
                        }
                        for (condition_id, positions) in &by_condition {
                            let market_id = positions
                                .first()
                                .map(|p| p.market_id.clone())
                                .unwrap_or_else(|| MarketId::from(condition_id.as_str()));
                            let now_ms = now_unix_ms();
                            let request = crate::wire::execution_adapter::RedeemPositionsRequest {
                                command_id: ClientOrderId::from(format!(
                                    "auto-redeem:{condition_id}:{now_ms}"
                                )),
                                market_id,
                                condition_id: condition_id.clone(),
                                index_sets: vec![1, 2],
                                submitted_at_ms: now_ms,
                            };
                            match execution_adapter.redeem_positions(request).await {
                                Ok(ack) => {
                                    auto_redeem_seen_conditions.insert(condition_id.clone());
                                    info!(
                                        target: "auto_redeem",
                                        condition_id = %condition_id,
                                        venue_message = ack.venue_message.as_deref().unwrap_or("(none)"),
                                        "auto-redeem: submitted"
                                    );
                                }
                                Err(error) => {
                                    warn!(
                                        target: "auto_redeem",
                                        condition_id = %condition_id,
                                        error = %error,
                                        "auto-redeem: submission failed (will retry next tick unless venue confirmed)"
                                    );
                                }
                            }
                        }
                    }
                    Err(error) => {
                        warn!(
                            target: "auto_redeem",
                            error = %error,
                            "auto-redeem: sync_balances failed (non-fatal)"
                        );
                    }
                }
            }
            _ = summaries.tick() => {
                let assets = market_universe.read().await.market_assets.clone();
                let snapshots = books.snapshots(&assets).await;
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

async fn refresh_runtime_market_universe(
    config: &AppConfig,
    runtime: &mut Runtime<StrategyMode>,
    market_universe: &Arc<RwLock<RuntimeMarketUniverse>>,
    market_assets_tx: &watch::Sender<Vec<String>>,
    user_markets_tx: &watch::Sender<Vec<String>>,
    now_ms: u64,
) -> Result<Option<RuntimeOutcome>> {
    let contexts = fetch_btc_5m_market_contexts(config, now_ms).await?;
    if contexts.len() == 0 {
        anyhow::bail!("market discovery returned no BTC 5m markets");
    }
    let next = RuntimeMarketUniverse::from_config_and_context(config, &contexts);
    if next.market_assets.is_empty() {
        anyhow::bail!("market discovery returned no token ids");
    }

    let mut guard = market_universe.write().await;
    let changed = guard.market_assets != next.market_assets
        || guard.user_markets != next.user_markets
        || guard.market_id_by_asset != next.market_id_by_asset;
    if !changed {
        return Ok(None);
    }

    let active_instruments = next
        .market_assets
        .iter()
        .map(|asset| InstrumentId::from(asset.as_str()))
        .collect::<HashSet<_>>();
    let mut outcome = runtime.replace_market_contexts(contexts, now_ms, "gamma market discovery");
    outcome.extend(runtime.request_cancel_orders_not_in_instruments(
        &active_instruments,
        now_ms,
        "market universe rolled; cancel stale-market quote",
    ));

    *guard = next.clone();
    let _ = market_assets_tx.send(next.market_assets.clone());
    let _ = user_markets_tx.send(next.user_markets.clone());
    info!(
        target: "market_discovery",
        asset_count = next.market_assets.len(),
        market_count = next.user_markets.len(),
        markets = ?next.user_markets,
        "runtime market universe refreshed"
    );
    Ok(Some(outcome))
}

async fn fetch_btc_5m_market_contexts(
    config: &AppConfig,
    now_ms: u64,
) -> Result<MarketContextStore> {
    let records = fetch_btc_5m_gamma_records(config, now_ms).await?;
    let selected = select_runtime_market_records(
        records,
        now_ms,
        config.market_discovery_include_prev,
        config.market_discovery_include_next,
    );
    Ok(MarketContextStore::from_records(
        selected,
        Some("gamma-api:engine-discovery".to_string()),
        Some(now_ms),
    ))
}

async fn fetch_btc_5m_gamma_records(
    config: &AppConfig,
    now_ms: u64,
) -> Result<Vec<crate::market_context::MarketContextRecord>> {
    let client = reqwest::Client::new();
    let current_start_ms = now_ms - (now_ms % BTC_5M_WINDOW_MS);
    let start_offset = -(config.market_discovery_include_prev as i64);
    let end_offset = config.market_discovery_include_next as i64;
    let mut records = Vec::new();
    for offset in start_offset..=end_offset {
        let start_ms = if offset < 0 {
            current_start_ms.saturating_sub((-offset as u64) * BTC_5M_WINDOW_MS)
        } else {
            current_start_ms.saturating_add((offset as u64) * BTC_5M_WINDOW_MS)
        };
        let slug = format!(
            "{}{}",
            config.market_discovery_slug_prefix,
            start_ms / 1_000
        );
        let payload = client
            .get(&config.market_discovery_gamma_url)
            .query(&[("slug", slug.as_str())])
            .header("User-Agent", "polymarket-agent/1.0")
            .header("Accept", "application/json")
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        let Some(items) = payload.as_array() else {
            continue;
        };
        for item in items {
            if let Some(record) =
                parse_gamma_market_record(item, &config.market_discovery_slug_prefix)
            {
                records.push(record);
            }
        }
    }
    records.sort_by_key(|record| {
        (
            record.event_start_time_ms.unwrap_or_default(),
            record.event_end_time_ms.unwrap_or_default(),
            record.market_id.clone(),
        )
    });
    records.dedup_by(|left, right| left.market_id == right.market_id);
    Ok(records)
}

fn select_runtime_market_records(
    records: Vec<crate::market_context::MarketContextRecord>,
    now_ms: u64,
    include_prev: usize,
    include_next: usize,
) -> Vec<crate::market_context::MarketContextRecord> {
    let mut previous = records
        .iter()
        .filter(|record| record.event_end_time_ms.is_some_and(|end| end < now_ms))
        .cloned()
        .collect::<Vec<_>>();
    let active = records
        .iter()
        .filter(|record| record.is_active_btc_5m_window(now_ms))
        .cloned()
        .collect::<Vec<_>>();
    let mut upcoming = records
        .into_iter()
        .filter(|record| {
            record
                .event_start_time_ms
                .is_some_and(|start| start > now_ms)
        })
        .collect::<Vec<_>>();
    previous.sort_by_key(|record| std::cmp::Reverse(record.event_end_time_ms.unwrap_or_default()));
    upcoming.sort_by_key(|record| record.event_start_time_ms.unwrap_or_default());

    let mut selected = previous.into_iter().take(include_prev).collect::<Vec<_>>();
    selected.reverse();
    selected.extend(active);
    selected.extend(upcoming.into_iter().take(include_next));
    selected
}

fn parse_gamma_market_record(
    value: &Value,
    slug_prefix: &str,
) -> Option<crate::market_context::MarketContextRecord> {
    let slug = value.get("slug")?.as_str()?.trim();
    if !slug.starts_with(slug_prefix) {
        return None;
    }
    let market_id = value
        .get("id")
        .or_else(|| value.get("market_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let instrument_ids = parse_gamma_token_ids(
        value
            .get("clobTokenIds")
            .or_else(|| value.get("clobTokenIdsJson"))
            .or_else(|| value.get("token_ids_json"))
            .or_else(|| value.get("tokenIds")),
    );
    if instrument_ids.len() < 2 {
        return None;
    }
    let start_ms = parse_gamma_time_ms(
        value
            .get("event_start_time")
            .or_else(|| value.get("eventStartTime"))
            .or_else(|| value.get("startTime"))
            .or_else(|| value.get("startDate"))
            .or_else(|| value.get("start_date")),
    )
    .or_else(|| parse_start_ms_from_btc_slug(slug));
    let end_ms = parse_gamma_time_ms(
        value
            .get("endDate")
            .or_else(|| value.get("end_date"))
            .or_else(|| value.get("endTime"))
            .or_else(|| value.get("end_time")),
    )
    .or_else(|| start_ms.map(|start| start.saturating_add(BTC_5M_WINDOW_MS)));
    let start_ms = start_ms.or_else(|| end_ms.map(|end| end.saturating_sub(BTC_5M_WINDOW_MS)));

    Some(crate::market_context::MarketContextRecord {
        market_id: market_id.to_string(),
        instrument_ids: instrument_ids.into_iter().take(2).collect(),
        price_to_beat: pick_gamma_f64(value, &["priceToBeat", "price_to_beat"]),
        final_price: pick_gamma_f64(value, &["finalPrice", "final_price"]),
        event_start_time_ms: start_ms,
        event_end_time_ms: end_ms,
    })
}

fn parse_gamma_token_ids(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .filter(|item| !item.trim().is_empty())
            .collect(),
        Some(Value::String(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Vec::new();
            }
            if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                return parse_gamma_token_ids(Some(&parsed));
            }
            trimmed
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        }
        _ => Vec::new(),
    }
}

fn parse_gamma_time_ms(value: Option<&Value>) -> Option<u64> {
    let raw = value?.as_str()?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|ts| ts.with_timezone(&Utc).timestamp_millis().max(0) as u64)
}

fn parse_start_ms_from_btc_slug(slug: &str) -> Option<u64> {
    slug.rsplit('-')
        .next()
        .and_then(|part| part.parse::<u64>().ok())
        .map(|seconds| seconds.saturating_mul(1_000))
}

fn pick_gamma_f64(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| match value.get(*key)? {
        Value::Number(number) => number.as_f64(),
        Value::String(raw) => raw.parse::<f64>().ok(),
        _ => None,
    })
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

    let quote_snapshot = profile
        .map(|profile| profile.quote.clone())
        .unwrap_or_default();
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
            let Some(mid_price) = book_mid_by_asset.get(managed.intent.instrument_id.as_str())
            else {
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
                venue_cash_usd: control_plane_metrics.venue_cash_usd,
                venue_position_count: control_plane_metrics.venue_position_count,
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
            let resolved_client_order_id = resolve_user_event_client_order_id(
                client_order_id,
                order_id.as_deref(),
                execution_venue_map,
            );
            if quantity <= 0.0 || price <= 0.0 || resolved_client_order_id.is_none() {
                RuntimeOutcome::default()
            } else {
                let fill_side = parse_trade_side(&side);
                let liquidity = parse_fill_liquidity(&liquidity);
                let market_id = market_id.map(MarketId::from);
                let instrument_id = asset_id.map(InstrumentId::from);
                let fill = FillReport {
                    order_id: order_id.map(crate::types::OrderId::from),
                    client_order_id: resolved_client_order_id
                        .map(crate::types::ClientOrderId::from),
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

fn resolve_user_event_client_order_id(
    client_order_id: Option<String>,
    venue_order_id: Option<&str>,
    execution_venue_map: &HashMap<ClientOrderId, Option<OrderId>>,
) -> Option<String> {
    if client_order_id
        .as_deref()
        .is_some_and(|value| !value.is_empty())
    {
        return client_order_id;
    }
    let venue_order_id = venue_order_id?;
    execution_venue_map
        .iter()
        .find_map(|(client_id, mapped_venue_id)| {
            mapped_venue_id
                .as_ref()
                .filter(|order_id| order_id.as_str() == venue_order_id)
                .map(|_| client_id.to_string())
        })
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
    last_fill_ms: u64,
    last_fill_book_update_ms: u64,
    fill_count: usize,
    /// Phase 2 paper env cancel race window: when a Cancel command is
    /// received in paper mode, this is set to observed_at_ms instead of
    /// removing the context. Subsequent ticks within the configured
    /// window may still apply a fill (mirrors the live race between
    /// venue cancel ack and an in-flight fill). Once the window
    /// elapses without a fill, the deferred cancel is applied.
    cancel_requested_at_ms: Option<u64>,
}

fn paper_order_context_mut<'a>(
    paper_order_ctx: &'a mut HashMap<ClientOrderId, PaperOrderContext>,
    intent: &OrderIntent,
    now_ms: u64,
) -> &'a mut PaperOrderContext {
    let state = PaperOrderContext {
        arrival_ms: intent.created_at_ms.min(now_ms),
        queue_bias: deterministic_hash_0_95(intent.client_order_id.as_str()),
        last_attempt_ms: now_ms,
        last_fill_ms: 0,
        last_fill_book_update_ms: 0,
        fill_count: 0,
        cancel_requested_at_ms: None,
    };
    let ctx = paper_order_ctx
        .entry(intent.client_order_id.clone())
        .or_insert(state);
    ctx.last_attempt_ms = now_ms;
    ctx
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
    metrics: &AppMetrics,
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

    let records = if let Some(first_seq) = outcome.event_seqs.first().copied() {
        event_log.snapshot_since(first_seq.saturating_sub(1))
    } else {
        Vec::new()
    };
    record_strategy_attribution(metrics, &records, &outcome.commands);

    if let Some(writer) = journal.as_mut() {
        for record in &records {
            writer.append_event(record)?;
        }
        for command in &outcome.commands {
            writer.append_command(command)?;
        }
        writer.flush()?;
    }

    Ok(())
}

fn record_strategy_attribution(
    metrics: &AppMetrics,
    records: &[crate::event_log::EventRecord],
    commands: &[RuntimeCommand],
) {
    for record in records {
        if let Some(event) = classify_runtime_event(record.message.as_str()) {
            metrics.observe_strategy_event(event);
        }
    }
    for command in commands {
        if let Some(intent) = classify_runtime_command(command) {
            metrics.observe_strategy_intent(intent);
        }
    }
}

fn classify_runtime_event(message: &str) -> Option<&'static str> {
    if message.contains("entry-fill asymmetry cooldown")
        || message.contains("asymmetric entry-fill cooldown")
    {
        return Some("asym_fill_cooldown");
    }
    if message.contains("market mid moved") {
        return Some("market_mid_trend_gate");
    }
    if message.contains("btc regime flat") {
        return Some("btc_flat_gate");
    }
    if message.contains("btc regime trending") {
        return Some("btc_trend_gate");
    }
    if message.contains("premium fair cap") {
        return Some("premium_fair_gate");
    }
    if message.contains("hold stranded positive-asymmetry") {
        return Some("hold_positive_asym");
    }
    if message.contains("rescue stranded leg") {
        return Some("rescue_ev_selected");
    }
    if message.contains("on-fill IOC rescue emitted") {
        return Some("on_fill_rescue");
    }
    if message.contains("on-fill rescue throttled") {
        return Some("rescue_throttled");
    }
    if message.contains("paired entry ladder rejected") {
        return Some("entry_ladder_rejected");
    }
    if message.contains("inventory management rejected") {
        return Some("inventory_management_rejected");
    }
    if message.contains("market context replaced") {
        return Some("market_rollover");
    }
    if message.contains("runtime degraded") {
        return Some("runtime_degraded");
    }
    if message.contains("runtime risk-off") {
        return Some("runtime_riskoff");
    }
    None
}

fn classify_runtime_command(command: &RuntimeCommand) -> Option<&'static str> {
    let RuntimeCommand::Submit(intent) = command else {
        return None;
    };
    match intent.quote_level_tag.as_deref().unwrap_or_default() {
        tag if tag.starts_with("mm-paired-bid") => Some("paired_ladder"),
        tag if tag.starts_with("mm-convex-accum") => Some("convex_accum"),
        tag if tag.starts_with("mm-hedge-rescue") => Some("hedge_rescue"),
        tag if tag.starts_with("mm-reduce") => Some("reduce_cleanup"),
        "" => Some("untagged_submit"),
        _ => Some("other_submit"),
    }
}

fn persist_audit_outcome(
    audit: &mut Option<AuditWriter>,
    source: &str,
    runtime: &Runtime<StrategyMode>,
    outcome: &RuntimeOutcome,
) -> Result<()> {
    if let Some(writer) = audit.as_mut() {
        writer.append_outcome(source, runtime, outcome)?;
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
    market_assets: &[String],
    paper_fee_coeff: f64,
    metrics: &AppMetrics,
    outcome: RuntimeOutcome,
    paper_order_ctx: &mut HashMap<ClientOrderId, PaperOrderContext>,
    execution_venue_map: &mut HashMap<ClientOrderId, Option<OrderId>>,
    live_safety: &mut LiveSafetyState,
    execution_adapter: Arc<dyn ExecutionAdapter>,
    execution_policy: &ExecutionPolicy,
    seen_venue_fill_keys: &mut HashSet<String>,
    paper_report: Option<&mut crate::paper::report::PaperReportWriter>,
) -> Result<RuntimeOutcome> {
    let mut combined = RuntimeOutcome {
        commands: Vec::new(),
        event_seqs: outcome.event_seqs,
    };

    let observed_at_ms = now_unix_ms();
    let mut queue: VecDeque<RuntimeCommand> = outcome.commands.into_iter().collect();
    let mut paper_report = paper_report;

    fn book_mid(book: &BookState) -> Option<f64> {
        if book.best_bid > 0.0 && book.best_ask > 0.0 {
            Some((book.best_bid + book.best_ask) * 0.5)
        } else {
            None
        }
    }

    if !execution_policy.paper_mode {
        let report = sync_execution_state(
            execution_adapter.as_ref(),
            runtime,
            execution_venue_map,
            execution_policy,
            seen_venue_fill_keys,
            observed_at_ms,
        )
        .await;
        let sync_outcome = apply_sync_report(
            runtime,
            metrics,
            live_safety,
            execution_policy,
            market_assets,
            report,
            observed_at_ms,
            execution_adapter.as_ref(),
        )
        .await;
        stage_outcome_commands(&mut combined, &mut queue, sync_outcome);
        let needs_reconcile_quarantine_age_ms = execution_policy
            .live_reconcile_missing_grace_ms
            .saturating_mul(2)
            .max(10_000);
        let quarantine_outcome = runtime.quarantine_stale_needs_reconcile_orders(
            observed_at_ms,
            needs_reconcile_quarantine_age_ms,
        );
        stage_outcome_commands(&mut combined, &mut queue, quarantine_outcome);
        let stale_cancel_outcome = cancel_stale_live_orders(
            runtime,
            observed_at_ms,
            execution_policy.live_order_max_age_ms,
        );
        stage_outcome_commands(&mut combined, &mut queue, stale_cancel_outcome);
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
                ManagedOrderStatus::NeedsReconcile => debug!(
                    mode = "live",
                    client_order_id = %client_order_id,
                    "order requires reconciliation; skipping automatic submit replay"
                ),
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
                if execution_policy.paper_mode {
                    let Some(book) = books.snapshot(intent.instrument_id.as_str()).await else {
                        continue;
                    };
                    if paper_post_only_should_reject(&intent, &book, execution_policy) {
                        if let Some(reporter) = paper_report.as_deref_mut() {
                            reporter.record_reject(
                                &intent.client_order_id,
                                &intent.market_id,
                                &intent.instrument_id,
                                intent.limit_price,
                                "post-only-cross-paper",
                                observed_at_ms,
                            );
                        }
                        paper_order_ctx.remove(&intent.client_order_id);
                        let reject_outcome = runtime.on_order_rejected(
                            &intent.client_order_id,
                            "post-only-cross-paper",
                            observed_at_ms,
                        );
                        let chained_commands = reject_outcome.commands.clone();
                        combined.extend(reject_outcome);
                        for command in chained_commands {
                            queue.push_back(command);
                        }
                        continue;
                    }
                    let mid_at_submit = book_mid(&book);
                    if let (Some(reporter), Some(mid)) =
                        (paper_report.as_deref_mut(), mid_at_submit)
                    {
                        reporter.record_submit_edge(
                            intent.side,
                            intent.limit_price,
                            intent.quantity,
                            mid,
                            observed_at_ms,
                        );
                    }
                    let ctx = paper_order_context_mut(paper_order_ctx, &intent, observed_at_ms);

                    if let Some(fill) = paper_fill_from_book_snapshot(
                        &book,
                        &intent,
                        observed_at_ms,
                        paper_fee_coeff,
                        ctx,
                        intent.quantity,
                        execution_policy,
                    ) {
                        metrics.record_fill(
                            &fill,
                            if matches!(fill.close_method, Some(crate::types::CloseMethod::Merge)) {
                                Some(observed_at_ms.saturating_sub(fill.observed_at_ms))
                            } else {
                                None
                            },
                        );
                        if let Some(reporter) = paper_report.as_deref_mut() {
                            reporter.record_fill(&fill, mid_at_submit);
                        }
                        let fill_outcome = runtime.on_fill(fill)?;
                        let chained_commands = fill_outcome.commands.clone();
                        combined.extend(fill_outcome);
                        for command in chained_commands {
                            queue.push_back(command);
                        }
                    } else {
                        combined.extend(
                            runtime.on_order_opened(&intent.client_order_id, observed_at_ms),
                        );
                    }
                    continue;
                }

                let submit_req =
                    submit_request_from_intent(&intent, observed_at_ms, execution_policy);
                if !runtime_has_active_order(runtime, &intent.client_order_id) {
                    debug!(
                        mode = "live",
                        client_order_id = %intent.client_order_id,
                        "skipping stale submit command for order no longer active"
                    );
                    paper_order_ctx.remove(&intent.client_order_id);
                    execution_venue_map.remove(&intent.client_order_id);
                    continue;
                }
                match execution_adapter.submit(submit_req).await {
                    Ok(ack) if ack.accepted => {
                        live_safety.consecutive_submit_errors = 0;
                        execution_venue_map
                            .insert(intent.client_order_id.clone(), ack.venue_order_id.clone());
                        let opened_outcome = runtime.on_order_opened_with_venue(
                            &intent.client_order_id,
                            ack.venue_order_id.clone(),
                            observed_at_ms,
                        );
                        combined.extend(opened_outcome);
                        let report = sync_execution_state(
                            execution_adapter.as_ref(),
                            runtime,
                            execution_venue_map,
                            execution_policy,
                            seen_venue_fill_keys,
                            observed_at_ms,
                        )
                        .await;
                        let sync_outcome = apply_sync_report(
                            runtime,
                            metrics,
                            live_safety,
                            execution_policy,
                            market_assets,
                            report,
                            observed_at_ms,
                            execution_adapter.as_ref(),
                        )
                        .await;
                        stage_outcome_commands(&mut combined, &mut queue, sync_outcome);
                    }
                    Ok(ack) => {
                        let reason = ack
                            .venue_message
                            .unwrap_or_else(|| "execution venue rejected submit".to_string());
                        let active_order =
                            runtime_has_active_order(runtime, &intent.client_order_id);
                        let counts_against_budget = submit_rejection_counts_against_live_budget(
                            &reason,
                            execution_policy.live_post_only,
                        );
                        if active_order && counts_against_budget {
                            live_safety.consecutive_submit_errors =
                                live_safety.consecutive_submit_errors.saturating_add(1);
                        }
                        warn!(
                            target: "polymarket_exec::runtime::runner",
                            mode = "live",
                            client_order_id = %intent.client_order_id,
                            instrument_id = %intent.instrument_id,
                            price = intent.limit_price,
                            qty = intent.quantity,
                            reason = %reason,
                            counts_against_budget,
                            active_order,
                            "submit ack-rejected by venue (full venue text)"
                        );
                        paper_order_ctx.remove(&intent.client_order_id);
                        if active_order {
                            let rejected_outcome = runtime.on_order_rejected(
                                &intent.client_order_id,
                                reason,
                                ack.accepted_at_ms,
                            );
                            combined.extend(rejected_outcome);
                        } else {
                            execution_venue_map.remove(&intent.client_order_id);
                        }
                    }
                    Err(error) => {
                        let active_order =
                            runtime_has_active_order(runtime, &intent.client_order_id);
                        if active_order {
                            live_safety.consecutive_submit_errors =
                                live_safety.consecutive_submit_errors.saturating_add(1);
                            warn!(
                                target: "polymarket_exec::runtime::runner",
                                mode = "live",
                                client_order_id = %intent.client_order_id,
                                instrument_id = %intent.instrument_id,
                                price = intent.limit_price,
                                qty = intent.quantity,
                                error = %error,
                                error_kind = std::any::type_name_of_val(&error),
                                "submit Err returned by adapter (full venue text)"
                            );
                        } else {
                            debug!(
                                mode = "live",
                                client_order_id = %intent.client_order_id,
                                error = %error,
                                "submit error ignored for order no longer active"
                            );
                        }
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
                        } else if active_order {
                            let rejected_outcome = runtime.on_order_rejected(
                                &intent.client_order_id,
                                error.to_string(),
                                observed_at_ms,
                            );
                            paper_order_ctx.remove(&intent.client_order_id);
                            combined.extend(rejected_outcome);
                        } else {
                            paper_order_ctx.remove(&intent.client_order_id);
                            execution_venue_map.remove(&intent.client_order_id);
                        }
                    }
                }
                enforce_live_error_budget(
                    runtime,
                    metrics,
                    live_safety,
                    execution_policy,
                    observed_at_ms,
                    &mut combined,
                );
            }
            RuntimeCommand::Cancel {
                client_order_id,
                reason,
            } => {
                if execution_policy.paper_mode {
                    if execution_policy.paper_cancel_race_window_ms > 0 {
                        if let Some(ctx) = paper_order_ctx.get_mut(&client_order_id) {
                            if ctx.cancel_requested_at_ms.is_none() {
                                ctx.cancel_requested_at_ms = Some(observed_at_ms);
                                debug!(
                                    source = "execution_bridge",
                                    client_order_id = %client_order_id,
                                    mode = "paper",
                                    cancel_race_window_ms =
                                        execution_policy.paper_cancel_race_window_ms,
                                    "cancel deferred for paper race window"
                                );
                                continue;
                            }
                        }
                    }
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
                        live_safety.consecutive_cancel_errors = 0;
                        execution_venue_map.remove(&client_order_id);
                        let cancelled_outcome = runtime.on_order_cancelled(
                            &client_order_id,
                            ack.venue_message
                                .unwrap_or_else(|| "execution cancelled".to_string()),
                            ack.accepted_at_ms,
                        );
                        combined.extend(cancelled_outcome);
                        let report = sync_execution_state(
                            execution_adapter.as_ref(),
                            runtime,
                            execution_venue_map,
                            execution_policy,
                            seen_venue_fill_keys,
                            observed_at_ms,
                        )
                        .await;
                        let sync_outcome = apply_sync_report(
                            runtime,
                            metrics,
                            live_safety,
                            execution_policy,
                            market_assets,
                            report,
                            observed_at_ms,
                            execution_adapter.as_ref(),
                        )
                        .await;
                        stage_outcome_commands(&mut combined, &mut queue, sync_outcome);
                    }
                    Ok(ack) => {
                        let reason = ack
                            .venue_message
                            .unwrap_or_else(|| "execution venue rejected cancel".to_string());
                        let lower_reason = reason.to_ascii_lowercase();
                        let uncertain_cancel = lower_reason.contains("matched")
                            || lower_reason.contains("already canceled")
                            || lower_reason.contains("can't be found");
                        if !uncertain_cancel {
                            live_safety.consecutive_cancel_errors =
                                live_safety.consecutive_cancel_errors.saturating_add(1);
                        }
                        warn!(
                            mode = "live",
                            client_order_id = %client_order_id,
                            venue_order_id = ?ack.venue_order_id,
                            reason = %reason,
                            "cancel rejected by venue; treating order state as uncertain"
                        );
                        metrics.observe_uncertain_submit();
                        if !uncertain_cancel {
                            combined.extend(runtime.mark_order_needs_reconcile(
                                &client_order_id,
                                ack.accepted_at_ms,
                                format!("cancel rejected by venue; uncertain state: {reason}"),
                            ));
                            metrics.observe_riskoff_transition();
                            combined.extend(runtime.degrade_and_cancel_all(
                                ack.accepted_at_ms,
                                format!("cancel rejected by venue; risk-off until venue fill state is reconciled: {reason}"),
                            ));
                        }
                        let report = sync_execution_state(
                            execution_adapter.as_ref(),
                            runtime,
                            execution_venue_map,
                            execution_policy,
                            seen_venue_fill_keys,
                            ack.accepted_at_ms,
                        )
                        .await;
                        let sync_outcome = apply_sync_report(
                            runtime,
                            metrics,
                            live_safety,
                            execution_policy,
                            market_assets,
                            report,
                            ack.accepted_at_ms,
                            execution_adapter.as_ref(),
                        )
                        .await;
                        stage_outcome_commands(&mut combined, &mut queue, sync_outcome);
                    }
                    Err(error) => {
                        live_safety.consecutive_cancel_errors =
                            live_safety.consecutive_cancel_errors.saturating_add(1);
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
                enforce_live_error_budget(
                    runtime,
                    metrics,
                    live_safety,
                    execution_policy,
                    observed_at_ms,
                    &mut combined,
                );
            }
            RuntimeCommand::Merge(intent) => {
                if execution_policy.paper_mode {
                    let fill = crate::types::FillReport {
                        order_id: None,
                        client_order_id: Some(intent.command_id.clone()),
                        market_id: intent.market_id.clone(),
                        instrument_id: intent.yes_instrument_id.clone(),
                        side: TradeSide::Buy,
                        price: 1.0,
                        quantity: intent.quantity,
                        fee_usd: intent.expected_fee_usd + intent.expected_gas_usd,
                        liquidity: crate::types::FillLiquidity::Unknown,
                        close_method: Some(crate::types::CloseMethod::Merge),
                        observed_at_ms,
                    };
                    metrics.record_fill(
                        &fill,
                        Some(observed_at_ms.saturating_sub(intent.created_at_ms)),
                    );
                    let merge_outcome = runtime.on_fill(fill)?;
                    let chained_commands = merge_outcome.commands.clone();
                    combined.extend(merge_outcome);
                    for command in chained_commands {
                        queue.push_back(command);
                    }
                    continue;
                }

                let merge_req = MergePositionsRequest {
                    command_id: intent.command_id.clone(),
                    market_id: intent.market_id.clone(),
                    condition_id: intent.condition_id.clone(),
                    yes_instrument_id: intent.yes_instrument_id.clone(),
                    no_instrument_id: intent.no_instrument_id.clone(),
                    quantity: intent.quantity,
                    submitted_at_ms: observed_at_ms,
                };
                match execution_adapter.merge_positions(merge_req).await {
                    Ok(ack) if ack.accepted => {
                        info!(
                            mode = "live",
                            market_id = %intent.market_id,
                            yes_instrument_id = %intent.yes_instrument_id,
                            no_instrument_id = %intent.no_instrument_id,
                            quantity = intent.quantity,
                            command_id = %intent.command_id,
                            message = ?ack.venue_message,
                            "merge command accepted by execution adapter; awaiting venue reconciliation"
                        );
                        let report = sync_execution_state(
                            execution_adapter.as_ref(),
                            runtime,
                            execution_venue_map,
                            execution_policy,
                            seen_venue_fill_keys,
                            ack.accepted_at_ms,
                        )
                        .await;
                        let sync_outcome = apply_sync_report(
                            runtime,
                            metrics,
                            live_safety,
                            execution_policy,
                            market_assets,
                            report,
                            ack.accepted_at_ms,
                            execution_adapter.as_ref(),
                        )
                        .await;
                        stage_outcome_commands(&mut combined, &mut queue, sync_outcome);
                    }
                    Ok(ack) => {
                        let reason = ack.venue_message.unwrap_or_else(|| {
                            "execution venue rejected merge positions".to_string()
                        });
                        // Clear dedup so future merges aren't permanently blocked.
                        runtime.clear_pending_merge(&intent.market_id, ack.accepted_at_ms);
                        metrics.observe_riskoff_transition();
                        let degrade_outcome = runtime.degrade_and_cancel_all(
                            ack.accepted_at_ms,
                            format!("live merge rejected; risk-off until recycle path is fixed: {reason}"),
                        );
                        stage_outcome_commands(&mut combined, &mut queue, degrade_outcome);
                    }
                    Err(error) => {
                        // CRITICAL: clear dedup on BOTH retryable and non-retryable
                        // failures. Without this, the next reconcile sweep sees the
                        // stale pending_merge entry and skips the merge — making
                        // "will retry on next sweep" a lie that strands the market.
                        runtime.clear_pending_merge(&intent.market_id, observed_at_ms);
                        if error.is_retryable() {
                            warn!(
                                mode = "live",
                                market_id = %intent.market_id,
                                yes_instrument_id = %intent.yes_instrument_id,
                                no_instrument_id = %intent.no_instrument_id,
                                error = %error,
                                "transient merge failure; will retry on next reconcile sweep"
                            );
                        } else {
                            metrics.observe_riskoff_transition();
                            let degrade_outcome = runtime.degrade_and_cancel_all(
                                observed_at_ms,
                                format!(
                                    "live merge failed; risk-off until recycle path is fixed: {error}"
                                ),
                            );
                            stage_outcome_commands(&mut combined, &mut queue, degrade_outcome);
                        }
                    }
                }
            }
            RuntimeCommand::Redeem(intent) => {
                warn!(
                    mode = if execution_policy.paper_mode { "paper" } else { "live" },
                    market_id = %intent.market_id,
                    command_id = %intent.command_id,
                    "redeem command planned but relayer submission is not implemented"
                );
            }
            RuntimeCommand::Noop => {}
        }
    }

    if !execution_policy.paper_mode {
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
        let ctx = paper_order_context_mut(paper_order_ctx, &managed.intent, observed_at_ms);
        if let Some(fill) = paper_fill_from_book_snapshot(
            &book,
            &managed.intent,
            observed_at_ms,
            paper_fee_coeff,
            ctx,
            managed.remaining_qty(),
            execution_policy,
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

    // Phase 2 paper env cancel race: any deferred cancel whose race window
    // has elapsed without a fill is now finalized. If a fill arrived during
    // the window, the order's paper context was already removed by the
    // finished-order sweep above, so we skip it.
    if execution_policy.paper_mode && execution_policy.paper_cancel_race_window_ms > 0 {
        let expired_cancels: Vec<ClientOrderId> = paper_order_ctx
            .iter()
            .filter_map(|(coid, ctx)| {
                let req_ms = ctx.cancel_requested_at_ms?;
                if observed_at_ms.saturating_sub(req_ms)
                    >= execution_policy.paper_cancel_race_window_ms
                {
                    Some(coid.clone())
                } else {
                    None
                }
            })
            .collect();
        for coid in expired_cancels {
            let cancelled_outcome = runtime.on_order_cancelled(
                &coid,
                "paper cancel race window elapsed without fill",
                observed_at_ms,
            );
            combined.extend(cancelled_outcome);
            paper_order_ctx.remove(&coid);
        }
    }

    Ok(combined)
}

fn stage_outcome_commands(
    combined: &mut RuntimeOutcome,
    queue: &mut VecDeque<RuntimeCommand>,
    mut outcome: RuntimeOutcome,
) {
    queue.extend(outcome.commands.drain(..));
    combined.event_seqs.extend(outcome.event_seqs);
}

fn needs_reconcile_order_count(runtime: &Runtime<StrategyMode>) -> usize {
    runtime
        .open_order_snapshots()
        .into_iter()
        .filter(|managed| managed.status == ManagedOrderStatus::NeedsReconcile)
        .count()
}

fn submit_request_from_intent(
    intent: &OrderIntent,
    observed_at_ms: u64,
    execution_policy: &ExecutionPolicy,
) -> SubmitOrderRequest {
    // Hedge-rescue intents are taker IOC orders that lift the opposite leg
    // to manufacture paired inventory (whales' atomic completion pattern).
    // They MUST cross the book — post-only would defeat the whole purpose.
    let is_hedge_rescue = intent.kind == crate::types::IntentKind::Close;
    let live_expires_at_ms = (!execution_policy.paper_mode
        && execution_policy.live_order_ttl_ms > 0
        && !is_hedge_rescue)
        .then_some(observed_at_ms.saturating_add(execution_policy.live_order_ttl_ms));
    let (time_in_force, post_only) = if is_hedge_rescue {
        (TimeInForce::Ioc, false)
    } else if live_expires_at_ms.is_some() {
        (
            TimeInForce::Gtd,
            !execution_policy.paper_mode && execution_policy.live_post_only,
        )
    } else {
        (
            TimeInForce::Gtc,
            !execution_policy.paper_mode && execution_policy.live_post_only,
        )
    };
    SubmitOrderRequest {
        client_order_id: intent.client_order_id.clone(),
        market_id: intent.market_id.clone(),
        instrument_id: intent.instrument_id.clone(),
        side: intent.side,
        limit_price: intent.limit_price,
        quantity: intent.quantity,
        post_only,
        time_in_force,
        expires_at_ms: live_expires_at_ms,
        strategy_tag: "runtime".to_string(),
        quote_level_tag: intent.quote_level_tag.clone(),
        submitted_at_ms: observed_at_ms,
    }
}

async fn sync_execution_state(
    execution_adapter: &dyn ExecutionAdapter,
    runtime: &Runtime<StrategyMode>,
    execution_venue_map: &mut HashMap<ClientOrderId, Option<OrderId>>,
    execution_policy: &ExecutionPolicy,
    seen_venue_fill_keys: &mut HashSet<String>,
    now_ms: u64,
) -> ExecutionSyncReport {
    let mut report = ExecutionSyncReport::default();
    match execution_adapter.sync_open_orders().await {
        Ok(open_orders) => {
            let open_order_count = open_orders.len();
            report.open_order_count = open_order_count;
            let venue_ids = open_orders
                .iter()
                .map(|order| order.venue_order_id.clone())
                .collect::<HashSet<_>>();
            for order in open_orders {
                if let Some(client_order_id) = order.client_order_id.clone() {
                    execution_venue_map
                        .entry(client_order_id)
                        .or_insert(Some(order.venue_order_id.clone()));
                }
            }
            let fill_after_ms = now_ms.saturating_sub(15 * 60 * 1_000);
            let filled_client_ids = match execution_adapter.sync_recent_fills(fill_after_ms).await {
                Ok(fills) => {
                    let mut filled_client_ids = HashSet::new();
                    for mut fill in fills {
                        let fill_key = venue_fill_key(&fill);
                        if seen_venue_fill_keys.contains(&fill_key) {
                            continue;
                        }
                        let resolved_client_order_id = fill.client_order_id.clone().or_else(|| {
                            resolve_user_event_client_order_id(
                                None,
                                Some(fill.venue_order_id.as_str()),
                                execution_venue_map,
                            )
                            .map(ClientOrderId::from)
                        });
                        let Some(client_order_id) = resolved_client_order_id else {
                            continue;
                        };
                        fill.client_order_id = Some(client_order_id.clone());
                        filled_client_ids.insert(client_order_id);
                        report.venue_fills.push(fill);
                        seen_venue_fill_keys.insert(fill_key);
                    }
                    filled_client_ids
                }
                Err(error) => {
                    report.errors = report.errors.saturating_add(1);
                    warn!(error = %error, "execution recent fills sync failed");
                    HashSet::new()
                }
            };
            for managed in runtime.open_order_snapshots() {
                if !matches!(
                    managed.status,
                    ManagedOrderStatus::Submitted
                        | ManagedOrderStatus::Working
                        | ManagedOrderStatus::CancelRequested
                ) {
                    continue;
                }
                let Some(Some(venue_order_id)) =
                    execution_venue_map.get(&managed.intent.client_order_id)
                else {
                    continue;
                };
                if filled_client_ids.contains(&managed.intent.client_order_id) {
                    continue;
                }
                if !venue_ids.contains(venue_order_id) {
                    let age_ms = now_ms.saturating_sub(managed.last_update_ms);
                    if age_ms < execution_policy.live_reconcile_missing_grace_ms {
                        report
                            .pending_missing_local_orders
                            .push(managed.intent.client_order_id.clone());
                    } else {
                        report
                            .missing_local_orders
                            .push(managed.intent.client_order_id.clone());
                    }
                }
            }
            debug!(
                open_orders = open_order_count,
                venue_fills = report.venue_fills.len(),
                local_open_orders = runtime.open_order_snapshots().len(),
                pending_missing_local_orders = report.pending_missing_local_orders.len(),
                missing_local_orders = report.missing_local_orders.len(),
                "synced execution open orders"
            );
        }
        Err(error) => {
            report.errors = report.errors.saturating_add(1);
            warn!(error = %error, "execution open orders sync failed");
        }
    }

    match execution_adapter.sync_balances().await {
        Ok(balances) => {
            report.balance_synced = true;
            report.venue_cash_usd = Some(balances.cash_usd);
            report.venue_position_count = balances.positions.len();
            report.venue_positions_authoritative = balances.positions_authoritative;
            report.venue_balance_observed_at_ms = Some(balances.observed_at_ms);
            report.venue_positions = balances.positions;
            debug!(
                cash_usd = balances.cash_usd,
                positions = report.venue_position_count,
                positions_authoritative = report.venue_positions_authoritative,
                observed_at_ms = balances.observed_at_ms,
                "synced execution balances"
            );
        }
        Err(error) => {
            report.errors = report.errors.saturating_add(1);
            warn!(error = %error, "execution balances sync failed");
        }
    }
    report
}

async fn apply_sync_report(
    runtime: &mut Runtime<StrategyMode>,
    metrics: &AppMetrics,
    live_safety: &mut LiveSafetyState,
    execution_policy: &ExecutionPolicy,
    market_assets: &[String],
    report: ExecutionSyncReport,
    now_ms: u64,
    execution_adapter: &dyn ExecutionAdapter,
) -> RuntimeOutcome {
    let mut outcome = RuntimeOutcome::default();
    if execution_policy.paper_mode {
        return outcome;
    }
    if report.errors > 0 || !report.missing_local_orders.is_empty() {
        live_safety.consecutive_reconcile_mismatches = live_safety
            .consecutive_reconcile_mismatches
            .saturating_add(1);
        metrics.observe_reconcile_failure();
    } else {
        live_safety.consecutive_reconcile_mismatches = 0;
    }
    if report.balance_synced {
        live_safety.last_venue_cash_usd = report.venue_cash_usd;
        live_safety.last_venue_position_count = report.venue_position_count;
        live_safety.last_venue_balance_observed_at_ms = report.venue_balance_observed_at_ms;
        if let Some(cash_usd) = report.venue_cash_usd {
            metrics.set_venue_balance_metrics(cash_usd, report.venue_position_count);
            info!(
                mode = "live",
                venue_cash_usd = cash_usd,
                venue_position_count = report.venue_position_count,
                "synced venue balance state"
            );
        }
        if report.venue_positions_authoritative || !report.venue_positions.is_empty() {
            let observed_at_ms = report.venue_balance_observed_at_ms.unwrap_or(now_ms);
            let active_instruments = market_assets
                .iter()
                .map(|asset| InstrumentId::from(asset.as_str()))
                .collect::<HashSet<_>>();
            let mut snapshots = report
                .venue_positions
                .iter()
                .filter(|position| active_instruments.contains(&position.instrument_id))
                .map(|position| VenuePositionSnapshot {
                    market_id: position.market_id.clone(),
                    condition_id: position.condition_id.clone(),
                    instrument_id: position.instrument_id.clone(),
                    quantity: position.quantity,
                    average_cost_usd: position.average_cost_usd,
                    mark_price: None,
                    observed_at_ms,
                })
                .collect::<Vec<_>>();
            if report.venue_positions_authoritative {
                let venue_instruments = snapshots
                    .iter()
                    .map(|snapshot| snapshot.instrument_id.clone())
                    .collect::<HashSet<_>>();
                snapshots.extend(
                    runtime
                        .inventory()
                        .positions()
                        .filter(|position| {
                            !venue_instruments.contains(&position.instrument_id)
                                || !active_instruments.contains(&position.instrument_id)
                        })
                        .map(|position| VenuePositionSnapshot {
                            market_id: position.market_id.clone(),
                            condition_id: None,
                            instrument_id: position.instrument_id.clone(),
                            quantity: 0.0,
                            average_cost_usd: position.avg_price,
                            mark_price: position.mark_price,
                            observed_at_ms,
                        }),
                );
            }
            match runtime.reconcile_venue_positions(&snapshots, observed_at_ms) {
                Ok(reconcile_report) => {
                    let mut merge_markets = snapshots
                        .iter()
                        .filter(|snapshot| snapshot.quantity > 1e-9)
                        .map(|snapshot| snapshot.market_id.clone())
                        .collect::<HashSet<_>>();
                    for stranded in reconcile_report.stranded_markets {
                        merge_markets.insert(stranded.market_id.clone());
                        warn!(
                            mode = "live",
                            market_id = %stranded.market_id,
                            paired_quantity = stranded.paired_quantity,
                            stranded_legs = stranded.stranded_positions.len(),
                            "venue reconciliation found stranded inventory"
                        );
                    }
                    // Lazily fetch venue rules (minimum_order_size, tick) for
                    // any market we just learned we have a position in. One
                    // HTTP call per market per session — strategy then reads
                    // venue truth instead of duplicating it as an env knob.
                    for snapshot in &snapshots {
                        if runtime.venue_market_rules(&snapshot.market_id).is_some() {
                            continue;
                        }
                        let Some(condition_id) = snapshot
                            .condition_id
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        else {
                            continue;
                        };
                        match execution_adapter.fetch_market_metadata(condition_id).await {
                            Ok(md) => {
                                let rules = VenueMarketRules {
                                    minimum_order_size: md.minimum_order_size,
                                    minimum_tick_size: md.minimum_tick_size,
                                    neg_risk: md.neg_risk,
                                };
                                info!(
                                    target: "live_reconcile.venue_metadata",
                                    market_id = %snapshot.market_id,
                                    condition_id = %md.condition_id,
                                    minimum_order_size = md.minimum_order_size,
                                    minimum_tick_size = md.minimum_tick_size,
                                    neg_risk = md.neg_risk,
                                    "cached venue rules for market"
                                );
                                runtime.set_venue_market_rules(snapshot.market_id.clone(), rules);
                            }
                            Err(error) => warn!(
                                target: "live_reconcile.venue_metadata",
                                market_id = %snapshot.market_id,
                                condition_id = %condition_id,
                                error = %error,
                                "failed to fetch venue metadata; strategy will use config defaults"
                            ),
                        }
                    }
                    for market_id in merge_markets {
                        outcome.extend(runtime.plan_merge_command_for_market(
                            &market_id,
                            observed_at_ms,
                            "paired inventory after venue reconciliation",
                        ));
                    }
                }
                Err(error) => {
                    live_safety.consecutive_reconcile_mismatches = live_safety
                        .consecutive_reconcile_mismatches
                        .saturating_add(1);
                    metrics.observe_reconcile_failure();
                    warn!(error = ?error, "failed to reconcile venue position snapshot");
                }
            }
        } else {
            debug!("venue balance sync returned no positions; local inventory left unchanged");
        }
    }

    for fill in report.venue_fills {
        let fill_report = FillReport {
            order_id: Some(fill.venue_order_id),
            client_order_id: fill.client_order_id,
            market_id: fill.market_id,
            instrument_id: fill.instrument_id,
            side: fill.side,
            price: fill.price,
            quantity: fill.quantity,
            fee_usd: fill.fee_usd,
            liquidity: fill.liquidity,
            close_method: None,
            observed_at_ms: fill.observed_at_ms,
        };
        if fill_report.quantity <= 0.0 || fill_report.price <= 0.0 {
            continue;
        }
        metrics.record_fill(&fill_report, None);
        match runtime.on_fill(fill_report) {
            Ok(fill_outcome) => outcome.extend(fill_outcome),
            Err(error) => {
                live_safety.consecutive_reconcile_mismatches = live_safety
                    .consecutive_reconcile_mismatches
                    .saturating_add(1);
                metrics.observe_reconcile_failure();
                warn!(error = ?error, "failed to apply venue fill during live reconciliation");
            }
        }
    }

    for client_order_id in report.pending_missing_local_orders {
        debug!(
            mode = "live",
            client_order_id = %client_order_id,
            "live order temporarily absent from open-order sync within grace; deferring reconcile"
        );
    }

    for client_order_id in report.missing_local_orders {
        outcome.extend(runtime.mark_order_needs_reconcile(
            &client_order_id,
            now_ms,
            "venue sync missing locally tracked live order",
        ));
    }

    if execution_policy.live_kill_on_reconcile_mismatch
        && live_safety.consecutive_reconcile_mismatches > 0
    {
        let reason = "live reconciliation mismatch; fail-closed risk-off";
        metrics.observe_riskoff_transition();
        outcome.extend(runtime.degrade_and_cancel_all(now_ms, reason));
    }
    outcome
}

fn submit_rejection_counts_against_live_budget(reason: &str, post_only: bool) -> bool {
    let lower = reason.to_ascii_lowercase();
    if post_only && lower == "execution venue rejected submit" {
        return false;
    }
    !(lower.contains("post-only")
        || lower.contains("crosses book")
        || lower.contains("would cross")
        || lower.contains("would take liquidity"))
}

fn runtime_has_active_order(
    runtime: &Runtime<StrategyMode>,
    client_order_id: &ClientOrderId,
) -> bool {
    runtime
        .open_orders()
        .any(|managed| &managed.intent.client_order_id == client_order_id)
}

fn enforce_live_error_budget(
    runtime: &mut Runtime<StrategyMode>,
    metrics: &AppMetrics,
    live_safety: &LiveSafetyState,
    execution_policy: &ExecutionPolicy,
    now_ms: u64,
    combined: &mut RuntimeOutcome,
) {
    if execution_policy.paper_mode {
        return;
    }
    let submit_limit_hit =
        live_safety.consecutive_submit_errors >= execution_policy.live_max_submit_errors.max(1);
    let cancel_limit_hit =
        live_safety.consecutive_cancel_errors >= execution_policy.live_max_cancel_errors.max(1);
    if submit_limit_hit || cancel_limit_hit {
        let reason = format!(
            "live execution error budget exhausted submit_errors={} cancel_errors={}",
            live_safety.consecutive_submit_errors, live_safety.consecutive_cancel_errors
        );
        metrics.observe_riskoff_transition();
        combined.extend(runtime.degrade_and_cancel_all(now_ms, reason));
    }
}

fn cancel_stale_live_orders(
    runtime: &mut Runtime<StrategyMode>,
    now_ms: u64,
    max_age_ms: u64,
) -> RuntimeOutcome {
    let mut outcome = RuntimeOutcome::default();
    if max_age_ms == 0 {
        return outcome;
    }
    let stale_ids = runtime
        .open_order_snapshots()
        .into_iter()
        .filter(|managed| {
            matches!(
                managed.status,
                ManagedOrderStatus::Submitted | ManagedOrderStatus::Working
            ) && now_ms.saturating_sub(managed.intent.created_at_ms) >= max_age_ms
        })
        .map(|managed| managed.intent.client_order_id)
        .collect::<Vec<_>>();
    for client_order_id in stale_ids {
        outcome.extend(runtime.request_cancel_order(
            &client_order_id,
            now_ms,
            format!("live order max age exceeded {max_age_ms}ms"),
        ));
    }
    outcome
}

fn enforce_live_health(
    runtime: &mut Runtime<StrategyMode>,
    metrics: &AppMetrics,
    config: &AppConfig,
    live_safety: &LiveSafetyState,
    now_ms: u64,
    started_at_ms: u64,
) -> RuntimeOutcome {
    if config.paper_mode || runtime.status() != RuntimeStatus::Running {
        return RuntimeOutcome::default();
    }
    if now_ms.saturating_sub(started_at_ms) < LIVE_HEALTH_STARTUP_GRACE_MS {
        return RuntimeOutcome::default();
    }
    let snapshot = metrics.snapshot();
    let market_stale_ms = config
        .strategy_profile
        .as_ref()
        .and_then(|profile| profile.health.market_ws_stale_ms)
        .unwrap_or(config.book_stale_after.as_millis() as u64 * 3);
    let user_stale_ms = config
        .strategy_profile
        .as_ref()
        .and_then(|profile| profile.health.user_ws_stale_ms)
        .unwrap_or(30_000);
    let mut health_failures = Vec::new();
    let mut risk_failures = Vec::new();
    if !snapshot.market_ws_connected {
        health_failures.push("market websocket disconnected".to_string());
    }
    if snapshot.market_last_message_age_ms >= 0.0
        && snapshot.market_last_message_age_ms > market_stale_ms as f64
    {
        health_failures.push(format!(
            "market websocket stale age_ms={:.0} max_ms={market_stale_ms}",
            snapshot.market_last_message_age_ms
        ));
    }
    if !snapshot.user_ws_connected && snapshot.user_last_message_age_ms >= 0.0 {
        health_failures.push("user websocket disconnected".to_string());
    }
    if snapshot.user_last_message_age_ms >= 0.0
        && snapshot.user_last_message_age_ms > user_stale_ms as f64
    {
        health_failures.push(format!(
            "user websocket stale age_ms={:.0} max_ms={user_stale_ms}",
            snapshot.user_last_message_age_ms
        ));
    }
    if !snapshot.execution_adapter_connected {
        health_failures.push("execution adapter disconnected".to_string());
    }
    let free_cash_floor_usd = config
        .risk_limits
        .free_cash_floor_usd(config.starting_cash_usd);
    match live_safety.last_venue_cash_usd {
        Some(cash_usd) if cash_usd < free_cash_floor_usd => {
            risk_failures.push(format!(
                "venue cash below floor cash={cash_usd:.4} floor={:.4}",
                free_cash_floor_usd
            ));
        }
        Some(_) => {}
        None => health_failures.push("venue balance has not synced".to_string()),
    }
    let needs_reconcile = needs_reconcile_order_count(runtime);
    if needs_reconcile > 0 {
        health_failures.push(format!(
            "orders need reconciliation count={needs_reconcile}"
        ));
    }
    if runtime.inventory().gross_exposure_usd() > config.risk_limits.max_gross_notional_usd {
        risk_failures.push(format!(
            "gross exposure exceeded cap exposure={:.4} cap={:.4}",
            runtime.inventory().gross_exposure_usd(),
            config.risk_limits.max_gross_notional_usd
        ));
    }
    if let Some(equity_floor_usd) =
        portfolio_equity_floor_usd(&config.risk_limits, config.starting_cash_usd)
    {
        let local_equity_usd =
            runtime.inventory().total_cash_usd() + runtime.inventory().gross_exposure_usd();
        if local_equity_usd < equity_floor_usd {
            risk_failures.push(format!(
                "local portfolio equity below floor equity={local_equity_usd:.4} floor={equity_floor_usd:.4}"
            ));
        }
        if let Some(venue_cash_usd) = live_safety.last_venue_cash_usd {
            let venue_marked_equity_usd = venue_cash_usd + runtime.inventory().gross_exposure_usd();
            if venue_marked_equity_usd < equity_floor_usd {
                risk_failures.push(format!(
                    "venue marked equity below floor equity={venue_marked_equity_usd:.4} floor={equity_floor_usd:.4}"
                ));
            }
        }
    }
    if let Some(path) = config.live_kill_switch_path.as_ref() {
        if path.exists() {
            health_failures.push(format!(
                "operator kill switch active path={}",
                path.display()
            ));
        }
    }

    if health_failures.is_empty() && risk_failures.is_empty() {
        RuntimeOutcome::default()
    } else if health_failures.is_empty() {
        metrics.observe_riskoff_transition();
        runtime.riskoff_and_cancel_entry_orders(
            now_ms,
            format!("live risk limit failure: {}", risk_failures.join("; ")),
        )
    } else {
        metrics.observe_riskoff_transition();
        let mut failures = health_failures;
        failures.extend(risk_failures);
        runtime.degrade_and_cancel_all(
            now_ms,
            format!("live health failure: {}", failures.join("; ")),
        )
    }
}

fn enforce_capital_guard(
    runtime: &mut Runtime<StrategyMode>,
    metrics: &AppMetrics,
    risk_limits: &RiskLimits,
    starting_cash_usd: f64,
    now_ms: u64,
    mode: &str,
) -> RuntimeOutcome {
    if runtime.status() != RuntimeStatus::Running {
        return RuntimeOutcome::default();
    }
    let Some(equity_floor_usd) = portfolio_equity_floor_usd(risk_limits, starting_cash_usd) else {
        return RuntimeOutcome::default();
    };

    let local_equity_usd =
        runtime.inventory().total_cash_usd() + runtime.inventory().gross_exposure_usd();
    if local_equity_usd >= equity_floor_usd {
        return RuntimeOutcome::default();
    }

    metrics.observe_riskoff_transition();
    runtime.riskoff_and_cancel_entry_orders(
        now_ms,
        format!(
            "{mode} capital guard: portfolio equity below floor equity={local_equity_usd:.4} floor={equity_floor_usd:.4}"
        ),
    )
}

fn portfolio_equity_floor_usd(risk_limits: &RiskLimits, starting_cash_usd: f64) -> Option<f64> {
    risk_limits.portfolio_equity_floor_usd(starting_cash_usd)
}

fn paper_fill_from_book_snapshot(
    book: &BookState,
    intent: &OrderIntent,
    observed_at_ms: u64,
    paper_fee_coeff: f64,
    order_ctx: &mut PaperOrderContext,
    remaining_qty: f64,
    execution_policy: &ExecutionPolicy,
) -> Option<FillReport> {
    if remaining_qty <= 0.0 || intent.limit_price <= 0.0 {
        return None;
    }
    if order_ctx.fill_count >= execution_policy.paper_max_fills_per_order {
        return None;
    }
    if execution_policy.paper_submit_latency_ms > 0
        && observed_at_ms.saturating_sub(order_ctx.arrival_ms)
            < execution_policy.paper_submit_latency_ms
    {
        return None;
    }
    if order_ctx.last_fill_ms > 0
        && observed_at_ms.saturating_sub(order_ctx.last_fill_ms)
            < execution_policy.paper_min_fill_interval_ms
    {
        return None;
    }
    if book.last_update_unix_ms > 0
        && order_ctx.last_fill_book_update_ms == book.last_update_unix_ms
    {
        return None;
    }

    let candidate_levels: Vec<_> = if matches!(intent.side, TradeSide::Buy) {
        book.ask_levels()
            .iter()
            .filter(|level| level.price > 0.0 && level.price <= intent.limit_price)
            .collect()
    } else {
        book.bid_levels()
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

    let best_opposite = candidate_levels[0].price;
    let crossing = if matches!(intent.side, TradeSide::Buy) {
        book.best_ask > 0.0 && intent.limit_price >= book.best_ask
    } else {
        book.best_bid > 0.0 && intent.limit_price <= book.best_bid
    };
    let maker_trade_through = if matches!(intent.side, TradeSide::Buy) {
        book.last_trade_price > 0.0 && book.last_trade_price <= intent.limit_price
    } else {
        book.last_trade_price > 0.0 && book.last_trade_price >= intent.limit_price
    };
    // Distinguish a fresh submit (could be TAKER if crossing) from a
    // resting order that the book later moved into (always MAKER, fills
    // at our limit). A real venue does not turn our resting limit into
    // a taker just because the opposite side moved through us; we get
    // price-improved as the maker. Fresh = within submit-latency window.
    let order_age_ms = observed_at_ms.saturating_sub(order_ctx.arrival_ms);
    // "Fresh" = just arrived at venue; "resting" = strictly older than the
    // submit-latency window. A fresh order that crosses on arrival is a
    // taker; a resting order the book later moves into is a maker
    // (price-improvement to whoever takes our resting bid).
    let is_resting = order_age_ms > execution_policy.paper_submit_latency_ms;
    let crosses_as_taker = crossing && !is_resting;
    if !crossing {
        let queue_wait_ms = 1_000 + (order_ctx.queue_bias * 3_000.0) as u64;
        if order_age_ms < queue_wait_ms || !maker_trade_through {
            return None;
        }
    }
    // Maker fills land at OUR limit (the resting price). Taker fills
    // (fresh order crossing the book) land at the opposite-side touch.
    let best_fill_price = if crosses_as_taker {
        best_opposite
    } else {
        intent.limit_price
    };
    let fill_ratio = paper_fill_ratio(
        remaining_qty,
        total_available,
        book.last_update_unix_ms,
        best_fill_price,
        intent.limit_price,
        order_ctx,
        crossing,
        observed_at_ms,
    );
    let target_fill_qty = (remaining_qty * fill_ratio)
        .min(total_available)
        .min(remaining_qty);
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
        // Phase 2 paper env: replace opaque queue_bias with explicit
        // queue-depth-fraction model. Non-crossing maker orders can claim
        // (1.0 - paper_queue_depth_fraction) of top-level size, representing
        // the fraction of the queue ahead of us that has already cleared.
        // Default 0.75 → we claim 25% of top-of-book per fill attempt.
        // Reference: Moallemi-Yuan queue position valuation.
        let level_ratio = if crossing {
            1.0
        } else if idx == 0 {
            (1.0 - execution_policy.paper_queue_depth_fraction).max(0.0)
        } else {
            0.0
        };
        let level_fill = (level.size * level_ratio).min(remaining);
        if level_fill > 0.0 {
            qty_filled += level_fill;
            // Maker fills land at OUR limit (the resting price); taker
            // fills walk the book at level prices.
            let price_at_level = if crosses_as_taker {
                level.price
            } else {
                intent.limit_price
            };
            amount += level_fill * price_at_level;
            remaining -= level_fill;
        }
    }

    if qty_filled <= 0.0 {
        qty_filled = target_fill_qty.min(candidate_levels[0].size);
        let fallback_price = if crosses_as_taker {
            candidate_levels[0].price
        } else {
            intent.limit_price
        };
        amount = qty_filled * fallback_price;
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

    // Same is_resting / crosses_as_taker invariant as the price assignment
    // above. fill_ratio >= 0.75 alone does NOT make us a taker — a maker
    // order can still claim a large fraction of top-of-book; it's just
    // good queue position, not a crossing event.
    let liquidity = if crosses_as_taker {
        FillLiquidity::Taker
    } else {
        FillLiquidity::Maker
    };
    let notional = qty_filled * price;
    if notional < execution_policy.paper_min_fill_notional_usd
        && (remaining_qty * price) >= execution_policy.paper_min_fill_notional_usd
    {
        return None;
    }
    let effective_taker_coeff = execution_policy
        .paper_taker_fee_coeff_override
        .unwrap_or(paper_fee_coeff);
    let fee_basis = price * (1.0 - price);
    let fee = match liquidity {
        FillLiquidity::Maker => -(notional * execution_policy.paper_maker_rebate_coeff * fee_basis),
        FillLiquidity::Taker => notional * effective_taker_coeff * fee_basis,
        FillLiquidity::Unknown => notional * effective_taker_coeff * fee_basis,
    };
    order_ctx.last_fill_ms = observed_at_ms;
    order_ctx.last_fill_book_update_ms = book.last_update_unix_ms;
    order_ctx.fill_count = order_ctx.fill_count.saturating_add(1);

    Some(FillReport {
        order_id: None,
        client_order_id: Some(intent.client_order_id.clone()),
        market_id: intent.market_id.clone(),
        instrument_id: intent.instrument_id.clone(),
        side: intent.side,
        price,
        quantity: qty_filled,
        fee_usd: fee,
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
    observed_at_ms: u64,
) -> f64 {
    if order_qty <= 0.0 || available_qty <= 0.0 || fill_price <= 0.0 || limit_price <= 0.0 {
        return 0.0;
    }

    // Use observed_at_ms (the simulated/replay clock) instead of wall
    // clock so replays produce deterministic fill ratios. Was a hidden
    // bug: now_unix_ms() inside this function caused replay results to
    // depend on how fast the host machine ran the loop.
    let age_ms = observed_at_ms.saturating_sub(order_ctx.arrival_ms.max(order_ctx.last_attempt_ms));
    let age_pressure = if crossing {
        0.15 + 0.30 * ((age_ms as f64 / 3_000.0).clamp(0.0, 1.0))
    } else {
        0.02 + 0.18 * ((age_ms as f64 / 5_000.0).clamp(0.0, 1.0))
    };
    let size_pressure = 0.25 + 0.75 * (available_qty / (available_qty + order_qty));
    let queue_pressure = 0.08 + order_ctx.queue_bias * 0.52;
    let staleness_pressure = 0.30
        + 0.60
            * ((observed_at_ms.saturating_sub(snapshot_unix_ms) as f64 / 2_000.0).clamp(0.0, 1.0));
    let premium = ((limit_price - fill_price) / fill_price).max(0.0).min(1.0);
    let limit_pressure = if crossing {
        0.65
    } else {
        0.20 + (premium * 0.20)
    };
    (age_pressure * size_pressure * queue_pressure * staleness_pressure * limit_pressure)
        .clamp(0.0, if crossing { 0.65 } else { 0.20 })
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

/// Deterministic [0, 1) value derived from a client_order_id and a book
/// update timestamp. Used by paper-mode post-only rejection so that the
/// same fixture replays produce the same accept/reject pattern across
/// runs, while the decision varies per book update (matching real venue:
/// a previously-rejected post-only may succeed when the book moves).
fn deterministic_unit_hash(client_order_id: &str, book_update_ms: u64) -> f64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in client_order_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    for byte in book_update_ms.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    (hash & 0xffff_ffff) as f64 / 4_294_967_296.0
}

/// Phase 2 paper env: returns true when paper mode should reject a post-
/// only intent because it would cross the book. Probabilistic (controlled
/// by paper_post_only_reject_probability) and deterministic per
/// (client_order_id, book.last_update_unix_ms).
///
/// Real Polymarket post-only orders that arrive while the book is crossing
/// are rejected most of the time, but occasionally slip through as taker
/// fills because the ack and the book update aren't atomic. This models
/// that behavior in paper.
fn paper_post_only_should_reject(
    intent: &OrderIntent,
    book: &BookState,
    execution_policy: &ExecutionPolicy,
) -> bool {
    if !execution_policy.paper_mode {
        return false;
    }
    if execution_policy.paper_post_only_reject_probability <= 0.0 {
        return false;
    }
    let crossing = if matches!(intent.side, TradeSide::Buy) {
        book.best_ask > 0.0 && intent.limit_price >= book.best_ask
    } else {
        book.best_bid > 0.0 && intent.limit_price <= book.best_bid
    };
    if !crossing {
        return false;
    }
    let roll = deterministic_unit_hash(intent.client_order_id.as_str(), book.last_update_unix_ms);
    roll < execution_policy.paper_post_only_reject_probability
}

fn venue_fill_key(fill: &VenueFill) -> String {
    format!(
        "{}:{}:{:.8}:{:.8}:{}",
        fill.venue_order_id, fill.instrument_id, fill.price, fill.quantity, fill.observed_at_ms
    )
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    use crate::book::Level;
    use crate::market_context::MarketContextStore;
    use crate::risk::RiskLimits;
    use crate::runtime::order_store::{OrderRecord, OrderStore, SqliteOrderStore};
    use crate::strategy::NoopStrategy;
    use crate::wire::execution_adapter::{
        CancelOrderAck, ExecutionError, MergePositionsAck, MergePositionsRequest, SubmitOrderAck,
        VenueBalances, VenueFill, VenuePosition,
    };

    #[derive(Default)]
    struct RecordingAdapter {
        submitted: Mutex<Vec<ClientOrderId>>,
        cancelled: Mutex<Vec<ClientOrderId>>,
        merged: Mutex<Vec<MergePositionsRequest>>,
        submit_reject_message: Option<String>,
        cancel_reject_message: Option<String>,
        merge_accept: bool,
        open_orders: Vec<crate::wire::execution_adapter::VenueOpenOrder>,
        fills: Vec<VenueFill>,
        balances: Option<VenueBalances>,
    }

    #[async_trait]
    impl ExecutionAdapter for RecordingAdapter {
        async fn submit(&self, req: SubmitOrderRequest) -> Result<SubmitOrderAck, ExecutionError> {
            self.submitted
                .lock()
                .expect("submitted lock")
                .push(req.client_order_id.clone());
            if let Some(message) = self.submit_reject_message.clone() {
                return Ok(SubmitOrderAck {
                    client_order_id: req.client_order_id,
                    venue_order_id: Some(OrderId::from("venue-submit")),
                    accepted: false,
                    accepted_at_ms: req.submitted_at_ms,
                    venue_message: Some(message),
                });
            }
            Ok(SubmitOrderAck {
                client_order_id: req.client_order_id,
                venue_order_id: Some(OrderId::from("venue-submit")),
                accepted: true,
                accepted_at_ms: req.submitted_at_ms,
                venue_message: None,
            })
        }

        async fn cancel(&self, req: CancelOrderRequest) -> Result<CancelOrderAck, ExecutionError> {
            self.cancelled
                .lock()
                .expect("cancelled lock")
                .push(req.client_order_id.clone());
            if let Some(message) = self.cancel_reject_message.clone() {
                return Ok(CancelOrderAck {
                    client_order_id: req.client_order_id,
                    venue_order_id: req.venue_order_id,
                    accepted: false,
                    accepted_at_ms: req.submitted_at_ms,
                    venue_message: Some(message),
                });
            }
            Ok(CancelOrderAck {
                client_order_id: req.client_order_id,
                venue_order_id: req.venue_order_id,
                accepted: true,
                accepted_at_ms: req.submitted_at_ms,
                venue_message: Some("cancelled".to_string()),
            })
        }

        async fn merge_positions(
            &self,
            req: MergePositionsRequest,
        ) -> Result<MergePositionsAck, ExecutionError> {
            self.merged.lock().expect("merged lock").push(req.clone());
            if !self.merge_accept {
                return Err(ExecutionError::BadRequest(
                    "test adapter merge not implemented".to_string(),
                ));
            }
            Ok(MergePositionsAck {
                command_id: req.command_id,
                accepted: true,
                accepted_at_ms: req.submitted_at_ms,
                venue_message: Some("test merge accepted".to_string()),
            })
        }

        async fn sync_open_orders(
            &self,
        ) -> Result<Vec<crate::wire::execution_adapter::VenueOpenOrder>, ExecutionError> {
            Ok(self.open_orders.clone())
        }

        async fn sync_balances(&self) -> Result<VenueBalances, ExecutionError> {
            Ok(self.balances.clone().unwrap_or(VenueBalances {
                cash_usd: 0.0,
                positions: Vec::new(),
                positions_authoritative: false,
                observed_at_ms: 0,
            }))
        }

        async fn sync_recent_fills(
            &self,
            _after_ms: u64,
        ) -> Result<Vec<VenueFill>, ExecutionError> {
            Ok(self.fills.clone())
        }
    }

    #[tokio::test]
    async fn live_execution_skips_needs_reconcile_submit_replay() {
        let mut runtime = runtime_with_recovered_needs_reconcile_order();
        let adapter = Arc::new(RecordingAdapter {
            open_orders: vec![crate::wire::execution_adapter::VenueOpenOrder {
                venue_order_id: OrderId::from("venue-1"),
                client_order_id: Some(ClientOrderId::from("client-reconcile")),
                market_id: MarketId::from("market-1"),
                instrument_id: InstrumentId::from("token-1"),
                side: TradeSide::Buy,
                limit_price: 0.40,
                original_qty: 5.0,
                remaining_qty: 5.0,
                created_at_ms: 1,
            }],
            ..RecordingAdapter::default()
        });
        let metrics = AppMetrics::new().expect("metrics");
        let assets: Vec<String> = Vec::new();
        let books = Arc::new(BookStore::new(&assets));
        let mut paper_order_ctx = HashMap::new();
        let mut execution_venue_map = HashMap::new();
        let mut live_safety = LiveSafetyState::default();
        let execution_policy = live_test_policy();
        let mut seen_venue_fill_keys = HashSet::new();

        let outcome = execute_execution_adapter(
            &mut runtime,
            &books,
            &assets,
            0.0,
            &metrics,
            RuntimeOutcome::default(),
            &mut paper_order_ctx,
            &mut execution_venue_map,
            &mut live_safety,
            adapter.clone(),
            &execution_policy,
            &mut seen_venue_fill_keys,
            None,
        )
        .await
        .expect("execute");

        assert!(outcome.commands.is_empty());
        assert!(adapter.submitted.lock().expect("submitted lock").is_empty());
        assert!(runtime.open_order_snapshots().into_iter().all(|managed| {
            managed.intent.client_order_id != ClientOrderId::from("client-reconcile")
        }));
    }

    #[tokio::test]
    async fn live_execution_ignores_stale_submit_after_order_left_memory() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Running,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            StrategyMode::Noop(NoopStrategy),
            MarketContextStore::empty(),
        );
        let stale_client_id = ClientOrderId::from("client-filled-before-submit-ack");
        let stale_intent = OrderIntent {
            client_order_id: stale_client_id,
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            limit_price: 0.40,
            quantity: 5.0,
            reduce_only: false,
            reason: "stale queued submit".to_string(),
            quote_level_tag: None,
            created_at_ms: now_unix_ms(),
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
        };
        let mut initial_outcome = RuntimeOutcome::default();
        initial_outcome.push_command(RuntimeCommand::Submit(stale_intent));

        let adapter = Arc::new(RecordingAdapter {
            submit_reject_message: Some("execution venue rejected submit".to_string()),
            ..RecordingAdapter::default()
        });
        let metrics = AppMetrics::new().expect("metrics");
        let assets: Vec<String> = Vec::new();
        let books = Arc::new(BookStore::new(&assets));
        let mut paper_order_ctx = HashMap::new();
        let mut execution_venue_map = HashMap::new();
        let mut live_safety = LiveSafetyState::default();
        let execution_policy = live_test_policy();
        let mut seen_venue_fill_keys = HashSet::new();

        let _outcome = execute_execution_adapter(
            &mut runtime,
            &books,
            &assets,
            0.0,
            &metrics,
            initial_outcome,
            &mut paper_order_ctx,
            &mut execution_venue_map,
            &mut live_safety,
            adapter.clone(),
            &execution_policy,
            &mut seen_venue_fill_keys,
            None,
        )
        .await
        .expect("execute");

        assert!(adapter.submitted.lock().expect("submitted lock").is_empty());
        assert_eq!(live_safety.consecutive_submit_errors, 0);
        assert_ne!(runtime.status(), RuntimeStatus::Degraded);
        assert_eq!(metrics.snapshot().runtime_riskoff_transitions_total, 0);
    }

    #[tokio::test]
    async fn live_sync_defers_recent_missing_working_order() {
        let mut runtime = runtime_with_recovered_working_order(now_unix_ms());
        let adapter = Arc::new(RecordingAdapter::default());
        let metrics = AppMetrics::new().expect("metrics");
        let assets: Vec<String> = Vec::new();
        let books = Arc::new(BookStore::new(&assets));
        let mut paper_order_ctx = HashMap::new();
        let mut execution_venue_map = HashMap::from([(
            ClientOrderId::from("client-working"),
            Some(OrderId::from("venue-1")),
        )]);
        let mut live_safety = LiveSafetyState::default();
        let execution_policy = live_test_policy();
        let mut seen_venue_fill_keys = HashSet::new();

        let _outcome = execute_execution_adapter(
            &mut runtime,
            &books,
            &assets,
            0.0,
            &metrics,
            RuntimeOutcome::default(),
            &mut paper_order_ctx,
            &mut execution_venue_map,
            &mut live_safety,
            adapter.clone(),
            &execution_policy,
            &mut seen_venue_fill_keys,
            None,
        )
        .await
        .expect("execute");

        assert_eq!(live_safety.consecutive_reconcile_mismatches, 0);
        assert!(adapter.cancelled.lock().expect("cancelled lock").is_empty());
        let order = runtime
            .open_order_snapshots()
            .into_iter()
            .find(|managed| managed.intent.client_order_id == ClientOrderId::from("client-working"))
            .expect("managed order");
        assert_eq!(order.status, ManagedOrderStatus::Working);
    }

    #[tokio::test]
    async fn live_sync_applies_late_fill_after_order_left_open_memory() {
        let mut runtime = runtime_with_recovered_working_order(now_unix_ms());
        let client_order_id = ClientOrderId::from("client-working");
        runtime.on_order_cancelled(&client_order_id, "test cancel before fill", now_unix_ms());
        assert!(runtime.open_order_snapshots().is_empty());

        let adapter = Arc::new(RecordingAdapter {
            fills: vec![VenueFill {
                venue_order_id: OrderId::from("venue-1"),
                client_order_id: None,
                market_id: MarketId::from("market-1"),
                instrument_id: InstrumentId::from("token-1"),
                side: TradeSide::Buy,
                price: 0.40,
                quantity: 5.0,
                fee_usd: 0.0,
                liquidity: FillLiquidity::Maker,
                observed_at_ms: now_unix_ms(),
            }],
            ..RecordingAdapter::default()
        });
        let metrics = AppMetrics::new().expect("metrics");
        let assets: Vec<String> = Vec::new();
        let books = Arc::new(BookStore::new(&assets));
        let mut paper_order_ctx = HashMap::new();
        let mut execution_venue_map =
            HashMap::from([(client_order_id.clone(), Some(OrderId::from("venue-1")))]);
        let mut live_safety = LiveSafetyState::default();
        let execution_policy = live_test_policy();
        let mut seen_venue_fill_keys = HashSet::new();

        let _outcome = execute_execution_adapter(
            &mut runtime,
            &books,
            &assets,
            0.0,
            &metrics,
            RuntimeOutcome::default(),
            &mut paper_order_ctx,
            &mut execution_venue_map,
            &mut live_safety,
            adapter.clone(),
            &execution_policy,
            &mut seen_venue_fill_keys,
            None,
        )
        .await
        .expect("execute");

        let position = runtime
            .inventory()
            .position(&InstrumentId::from("token-1"))
            .expect("late fill should create inventory");
        assert_eq!(position.quantity, 5.0);
        assert_eq!(live_safety.consecutive_reconcile_mismatches, 0);
        assert_eq!(metrics.snapshot().fill_total, 1);
        assert_eq!(metrics.snapshot().fill_maker_total, 1);
    }

    #[tokio::test]
    async fn live_sync_reconciles_non_empty_venue_position_snapshot() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Running,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            StrategyMode::Noop(NoopStrategy),
            MarketContextStore::empty(),
        );
        assert_eq!(
            runtime
                .inventory()
                .position_qty(&InstrumentId::from("down")),
            0.0
        );

        let now_ms = now_unix_ms();
        let adapter = Arc::new(RecordingAdapter {
            balances: Some(VenueBalances {
                cash_usd: 74.89,
                positions: vec![VenuePosition {
                    market_id: MarketId::from("market-mm"),
                    condition_id: None,
                    instrument_id: InstrumentId::from("down"),
                    quantity: 6.5,
                    average_cost_usd: 0.80,
                    redeemable: false,
                    mergeable: false,
                    current_value_usd: 0.0,
                }],
                positions_authoritative: true,
                observed_at_ms: now_ms,
            }),
            ..RecordingAdapter::default()
        });
        let metrics = AppMetrics::new().expect("metrics");
        let assets = vec!["down".to_string()];
        let books = Arc::new(BookStore::new(&assets));
        let mut paper_order_ctx = HashMap::new();
        let mut execution_venue_map = HashMap::new();
        let mut live_safety = LiveSafetyState::default();
        let execution_policy = live_test_policy();
        let mut seen_venue_fill_keys = HashSet::new();

        let _outcome = execute_execution_adapter(
            &mut runtime,
            &books,
            &assets,
            0.0,
            &metrics,
            RuntimeOutcome::default(),
            &mut paper_order_ctx,
            &mut execution_venue_map,
            &mut live_safety,
            adapter.clone(),
            &execution_policy,
            &mut seen_venue_fill_keys,
            None,
        )
        .await
        .expect("execute");

        let position = runtime
            .inventory()
            .position(&InstrumentId::from("down"))
            .expect("venue position should reconcile into runtime inventory");
        assert_eq!(position.quantity, 6.5);
        assert_eq!(position.avg_price, 0.80);
        assert_eq!(runtime.stranded_inventory().len(), 1);
        assert_eq!(live_safety.consecutive_reconcile_mismatches, 0);
        assert_eq!(metrics.snapshot().venue_position_count, 1);
    }

    #[tokio::test]
    async fn live_sync_executes_merge_plan_and_fails_closed_when_adapter_cannot_merge() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Running,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            StrategyMode::Noop(NoopStrategy),
            MarketContextStore::empty(),
        );

        let now_ms = now_unix_ms();
        let adapter = Arc::new(RecordingAdapter {
            balances: Some(VenueBalances {
                cash_usd: 80.0,
                positions: vec![
                    VenuePosition {
                        market_id: MarketId::from("market-mm"),
                        condition_id: Some(
                            "0x1111111111111111111111111111111111111111111111111111111111111111"
                                .to_string(),
                        ),
                        instrument_id: InstrumentId::from("up"),
                        quantity: 6.5,
                        average_cost_usd: 0.20,
                        redeemable: false,
                        mergeable: true,
                        current_value_usd: 1.30,
                    },
                    VenuePosition {
                        market_id: MarketId::from("market-mm"),
                        condition_id: Some(
                            "0x1111111111111111111111111111111111111111111111111111111111111111"
                                .to_string(),
                        ),
                        instrument_id: InstrumentId::from("down"),
                        quantity: 6.5,
                        average_cost_usd: 0.79,
                        redeemable: false,
                        mergeable: true,
                        current_value_usd: 5.13,
                    },
                ],
                positions_authoritative: true,
                observed_at_ms: now_ms,
            }),
            ..RecordingAdapter::default()
        });
        let metrics = AppMetrics::new().expect("metrics");
        let assets = vec!["up".to_string(), "down".to_string()];
        let books = Arc::new(BookStore::new(&assets));
        let mut paper_order_ctx = HashMap::new();
        let mut execution_venue_map = HashMap::new();
        let mut live_safety = LiveSafetyState::default();
        let execution_policy = live_test_policy();
        let mut seen_venue_fill_keys = HashSet::new();

        let outcome = execute_execution_adapter(
            &mut runtime,
            &books,
            &assets,
            0.0,
            &metrics,
            RuntimeOutcome::default(),
            &mut paper_order_ctx,
            &mut execution_venue_map,
            &mut live_safety,
            adapter.clone(),
            &execution_policy,
            &mut seen_venue_fill_keys,
            None,
        )
        .await
        .expect("execute");

        assert!(outcome
            .commands
            .iter()
            .any(|command| matches!(command, RuntimeCommand::Merge(_))));
        assert_eq!(runtime.status(), RuntimeStatus::Degraded);
        assert_eq!(metrics.snapshot().runtime_riskoff_transitions_total, 1);
        assert!(adapter.submitted.lock().expect("submitted lock").is_empty());
        let merges = adapter.merged.lock().expect("merged lock");
        assert_eq!(merges.len(), 1);
        assert_eq!(
            merges[0].condition_id.as_deref(),
            Some("0x1111111111111111111111111111111111111111111111111111111111111111")
        );
    }

    #[tokio::test]
    async fn live_sync_excludes_inactive_venue_positions_from_strategy_inventory() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Running,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            StrategyMode::Noop(NoopStrategy),
            MarketContextStore::empty(),
        );

        let now_ms = now_unix_ms();
        let adapter = Arc::new(RecordingAdapter {
            balances: Some(VenueBalances {
                cash_usd: 74.89,
                positions: vec![VenuePosition {
                    market_id: MarketId::from("old-market"),
                    condition_id: None,
                    instrument_id: InstrumentId::from("old-token"),
                    quantity: 6.5,
                    average_cost_usd: 0.80,
                    redeemable: false,
                    mergeable: false,
                    current_value_usd: 0.0,
                }],
                positions_authoritative: true,
                observed_at_ms: now_ms,
            }),
            ..RecordingAdapter::default()
        });
        let metrics = AppMetrics::new().expect("metrics");
        let assets = vec!["active-token".to_string()];
        let books = Arc::new(BookStore::new(&assets));
        let mut paper_order_ctx = HashMap::new();
        let mut execution_venue_map = HashMap::new();
        let mut live_safety = LiveSafetyState::default();
        let execution_policy = live_test_policy();
        let mut seen_venue_fill_keys = HashSet::new();

        let _outcome = execute_execution_adapter(
            &mut runtime,
            &books,
            &assets,
            0.0,
            &metrics,
            RuntimeOutcome::default(),
            &mut paper_order_ctx,
            &mut execution_venue_map,
            &mut live_safety,
            adapter,
            &execution_policy,
            &mut seen_venue_fill_keys,
            None,
        )
        .await
        .expect("execute");

        assert_eq!(metrics.snapshot().venue_position_count, 1);
        assert_eq!(
            runtime
                .inventory()
                .position_qty(&InstrumentId::from("old-token")),
            0.0
        );
        assert_eq!(runtime.inventory().gross_exposure_usd(), 0.0);
        assert_eq!(live_safety.consecutive_reconcile_mismatches, 0);
    }

    #[tokio::test]
    async fn matched_cancel_reject_reconciles_without_risk_off() {
        let mut runtime = runtime_with_recovered_working_order(now_unix_ms());
        let client_order_id = ClientOrderId::from("client-working");
        let cancel_outcome =
            runtime.request_cancel_order(&client_order_id, now_unix_ms(), "test cancel race");

        let adapter = Arc::new(RecordingAdapter {
            cancel_reject_message: Some("matched orders can't be canceled".to_string()),
            fills: vec![VenueFill {
                venue_order_id: OrderId::from("venue-1"),
                client_order_id: None,
                market_id: MarketId::from("market-1"),
                instrument_id: InstrumentId::from("token-1"),
                side: TradeSide::Buy,
                price: 0.40,
                quantity: 5.0,
                fee_usd: 0.0,
                liquidity: FillLiquidity::Maker,
                observed_at_ms: now_unix_ms(),
            }],
            ..RecordingAdapter::default()
        });
        let metrics = AppMetrics::new().expect("metrics");
        let assets = vec!["token-1".to_string()];
        let books = Arc::new(BookStore::new(&assets));
        let mut paper_order_ctx = HashMap::new();
        let mut execution_venue_map =
            HashMap::from([(client_order_id.clone(), Some(OrderId::from("venue-1")))]);
        let mut live_safety = LiveSafetyState::default();
        let execution_policy = live_test_policy();
        let mut seen_venue_fill_keys = HashSet::new();

        let _outcome = execute_execution_adapter(
            &mut runtime,
            &books,
            &assets,
            0.0,
            &metrics,
            cancel_outcome,
            &mut paper_order_ctx,
            &mut execution_venue_map,
            &mut live_safety,
            adapter,
            &execution_policy,
            &mut seen_venue_fill_keys,
            None,
        )
        .await
        .expect("execute");

        assert_ne!(runtime.status(), RuntimeStatus::Degraded);
        assert_eq!(live_safety.consecutive_cancel_errors, 0);
        assert_eq!(metrics.snapshot().runtime_riskoff_transitions_total, 0);
        assert_eq!(
            runtime
                .inventory()
                .position_qty(&InstrumentId::from("token-1")),
            5.0
        );
    }

    #[tokio::test]
    async fn live_sync_clears_local_inventory_on_authoritative_empty_venue_positions() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Running,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            StrategyMode::Noop(NoopStrategy),
            MarketContextStore::empty(),
        );
        let now_ms = now_unix_ms();
        runtime
            .reconcile_venue_positions(
                &[VenuePositionSnapshot {
                    market_id: MarketId::from("market-mm"),
                    condition_id: None,
                    instrument_id: InstrumentId::from("down"),
                    quantity: 6.5,
                    average_cost_usd: 0.80,
                    mark_price: None,
                    observed_at_ms: now_ms,
                }],
                now_ms,
            )
            .expect("seed inventory");
        assert_eq!(
            runtime
                .inventory()
                .position_qty(&InstrumentId::from("down")),
            6.5
        );

        let adapter = Arc::new(RecordingAdapter {
            balances: Some(VenueBalances {
                cash_usd: 80.0,
                positions: Vec::new(),
                positions_authoritative: true,
                observed_at_ms: now_ms.saturating_add(1),
            }),
            ..RecordingAdapter::default()
        });
        let metrics = AppMetrics::new().expect("metrics");
        let assets: Vec<String> = Vec::new();
        let books = Arc::new(BookStore::new(&assets));
        let mut paper_order_ctx = HashMap::new();
        let mut execution_venue_map = HashMap::new();
        let mut live_safety = LiveSafetyState::default();
        let execution_policy = live_test_policy();
        let mut seen_venue_fill_keys = HashSet::new();

        let _outcome = execute_execution_adapter(
            &mut runtime,
            &books,
            &assets,
            0.0,
            &metrics,
            RuntimeOutcome::default(),
            &mut paper_order_ctx,
            &mut execution_venue_map,
            &mut live_safety,
            adapter,
            &execution_policy,
            &mut seen_venue_fill_keys,
            None,
        )
        .await
        .expect("execute");

        assert_eq!(
            runtime
                .inventory()
                .position_qty(&InstrumentId::from("down")),
            0.0
        );
        assert_eq!(metrics.snapshot().venue_position_count, 0);
        assert_eq!(live_safety.consecutive_reconcile_mismatches, 0);
    }

    #[test]
    fn user_fill_resolves_client_order_from_venue_order_id() {
        let execution_venue_map = HashMap::from([(
            ClientOrderId::from("client-1"),
            Some(OrderId::from("venue-1")),
        )]);

        assert_eq!(
            resolve_user_event_client_order_id(None, Some("venue-1"), &execution_venue_map),
            Some("client-1".to_string())
        );
        assert_eq!(
            resolve_user_event_client_order_id(
                Some("client-direct".to_string()),
                Some("venue-1"),
                &execution_venue_map,
            ),
            Some("client-direct".to_string())
        );
    }

    #[test]
    fn live_submit_request_uses_post_only_gtd_with_expiry() {
        let intent = OrderIntent {
            client_order_id: ClientOrderId::from("client-ttl"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            limit_price: 0.40,
            quantity: 5.0,
            reduce_only: false,
            reason: "test live lifecycle".to_string(),
            quote_level_tag: Some("lvl-1:test".to_string()),
            created_at_ms: 10,
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
        };
        let policy = live_test_policy();
        let request = submit_request_from_intent(&intent, 1_000, &policy);
        assert!(request.post_only);
        assert_eq!(request.time_in_force, TimeInForce::Gtd);
        assert_eq!(request.expires_at_ms, Some(21_000));
    }

    #[test]
    fn generic_post_only_submit_reject_does_not_consume_live_budget() {
        assert!(!submit_rejection_counts_against_live_budget(
            "execution venue rejected submit",
            true
        ));
        assert!(submit_rejection_counts_against_live_budget(
            "execution venue rejected submit",
            false
        ));
        assert!(submit_rejection_counts_against_live_budget(
            "insufficient balance",
            true
        ));
    }

    #[test]
    fn portfolio_equity_floor_uses_stricter_absolute_or_session_loss_floor() {
        let risk_limits = RiskLimits {
            min_portfolio_equity_usd: 60.0,
            max_session_loss_usd: 25.0,
            ..RiskLimits::default()
        };

        assert_eq!(portfolio_equity_floor_usd(&risk_limits, 100.0), Some(75.0));

        let risk_limits = RiskLimits {
            min_portfolio_equity_usd: 90.0,
            max_session_loss_usd: 25.0,
            ..RiskLimits::default()
        };

        assert_eq!(portfolio_equity_floor_usd(&risk_limits, 100.0), Some(90.0));
        assert_eq!(
            portfolio_equity_floor_usd(&RiskLimits::default(), 100.0),
            None
        );
    }

    #[test]
    fn capital_guard_sets_riskoff_when_marked_equity_breaks_floor() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Running,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            StrategyMode::Noop(NoopStrategy),
            MarketContextStore::empty(),
        );
        runtime
            .on_fill(FillReport {
                order_id: None,
                client_order_id: None,
                market_id: MarketId::from("market-1"),
                instrument_id: InstrumentId::from("token-1"),
                side: TradeSide::Buy,
                price: 0.60,
                quantity: 100.0,
                fee_usd: 0.0,
                liquidity: FillLiquidity::Maker,
                close_method: None,
                observed_at_ms: 1,
            })
            .expect("fill");
        runtime
            .reconcile_venue_positions(
                &[VenuePositionSnapshot {
                    market_id: MarketId::from("market-1"),
                    condition_id: None,
                    instrument_id: InstrumentId::from("token-1"),
                    quantity: 100.0,
                    average_cost_usd: 0.60,
                    mark_price: Some(0.30),
                    observed_at_ms: 2,
                }],
                2,
            )
            .expect("reconcile");

        let metrics = AppMetrics::new().expect("metrics");
        let outcome = enforce_capital_guard(
            &mut runtime,
            &metrics,
            &RiskLimits {
                max_session_loss_usd: 20.0,
                ..RiskLimits::default()
            },
            100.0,
            3,
            "paper",
        );

        assert_eq!(runtime.status(), RuntimeStatus::RiskOff);
        assert!(!outcome.event_seqs.is_empty());
        assert_eq!(metrics.snapshot().runtime_riskoff_transitions_total, 1);
    }

    #[test]
    fn conservative_paper_fill_does_not_refill_same_book_update() {
        let now_ms = now_unix_ms();
        let book = BookState::from_top_of_book("token-1", 0.48, 100.0, 0.50, 100.0, 0.50, now_ms);
        let intent = OrderIntent {
            client_order_id: ClientOrderId::from("client-paper"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            limit_price: 0.50,
            quantity: 20.0,
            reduce_only: false,
            reason: "test paper fill".to_string(),
            quote_level_tag: None,
            created_at_ms: now_ms,
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
        };
        let policy = paper_test_policy();
        let mut ctx = PaperOrderContext {
            arrival_ms: now_ms,
            queue_bias: 0.5,
            last_attempt_ms: now_ms,
            last_fill_ms: 0,
            last_fill_book_update_ms: 0,
            fill_count: 0,
            cancel_requested_at_ms: None,
        };
        // Past the paper_submit_latency_ms gate (Phase 2 conservative model).
        let after_latency_ms = now_ms + policy.paper_submit_latency_ms + 50;
        let first = paper_fill_from_book_snapshot(
            &book,
            &intent,
            after_latency_ms,
            0.0,
            &mut ctx,
            intent.quantity,
            &policy,
        )
        .expect("first fill");
        assert!(first.notional_usd() >= policy.paper_min_fill_notional_usd);
        let second = paper_fill_from_book_snapshot(
            &book,
            &intent,
            after_latency_ms + 1_000,
            0.0,
            &mut ctx,
            intent.quantity - first.quantity,
            &policy,
        );
        assert!(second.is_none());
    }

    #[test]
    fn paper_post_only_should_reject_returns_false_outside_paper_mode() {
        let book = BookState::from_top_of_book("token-1", 0.49, 100.0, 0.50, 100.0, 0.50, 1_000);
        let intent = OrderIntent {
            client_order_id: ClientOrderId::from("client-postonly"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            limit_price: 0.51,
            quantity: 5.0,
            reduce_only: false,
            reason: "test post-only".to_string(),
            quote_level_tag: None,
            created_at_ms: 1_000,
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
        };
        let policy = live_test_policy();
        assert!(!paper_post_only_should_reject(&intent, &book, &policy));
    }

    #[test]
    fn paper_post_only_should_reject_skips_non_crossing_orders() {
        let book = BookState::from_top_of_book("token-1", 0.49, 100.0, 0.55, 100.0, 0.50, 1_000);
        let intent = OrderIntent {
            client_order_id: ClientOrderId::from("client-postonly-noncross"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            limit_price: 0.50,
            quantity: 5.0,
            reduce_only: false,
            reason: "test post-only no cross".to_string(),
            quote_level_tag: None,
            created_at_ms: 1_000,
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
        };
        let policy = paper_test_policy();
        assert!(!paper_post_only_should_reject(&intent, &book, &policy));
    }

    #[test]
    fn paper_post_only_should_reject_at_high_probability_when_crossing() {
        let book = BookState::from_top_of_book("token-1", 0.49, 100.0, 0.50, 100.0, 0.50, 1_000);
        let mut policy = paper_test_policy();
        policy.paper_post_only_reject_probability = 1.0;
        let intent = OrderIntent {
            client_order_id: ClientOrderId::from("client-postonly-cross"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            limit_price: 0.51,
            quantity: 5.0,
            reduce_only: false,
            reason: "test post-only cross".to_string(),
            quote_level_tag: None,
            created_at_ms: 1_000,
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
        };
        // probability 1.0 always rejects when crossing
        assert!(paper_post_only_should_reject(&intent, &book, &policy));
        // probability 0.0 never rejects
        policy.paper_post_only_reject_probability = 0.0;
        assert!(!paper_post_only_should_reject(&intent, &book, &policy));
    }

    #[test]
    fn paper_post_only_reject_decision_is_deterministic_per_book_update() {
        let book_a = BookState::from_top_of_book("token-1", 0.49, 100.0, 0.50, 100.0, 0.50, 1_000);
        let book_b = BookState::from_top_of_book("token-1", 0.49, 100.0, 0.50, 100.0, 0.50, 2_000);
        let intent = OrderIntent {
            client_order_id: ClientOrderId::from("client-determinism"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            limit_price: 0.51,
            quantity: 5.0,
            reduce_only: false,
            reason: "determinism".to_string(),
            quote_level_tag: None,
            created_at_ms: 1_000,
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
        };
        let mut policy = paper_test_policy();
        policy.paper_post_only_reject_probability = 0.5;
        // Same book, same decision repeated
        let r1 = paper_post_only_should_reject(&intent, &book_a, &policy);
        let r2 = paper_post_only_should_reject(&intent, &book_a, &policy);
        assert_eq!(r1, r2, "same book update must give same decision");
        // Decision varies independently across book updates (one of these is
        // exceedingly unlikely to fail; if it ever does, the hash is broken).
        let _r_b = paper_post_only_should_reject(&intent, &book_b, &policy);
    }

    #[test]
    fn resting_order_when_book_moves_into_us_fills_as_maker_at_limit() {
        // Real venue: resting limit buy at 0.45; book moves so best_ask
        // drops to 0.43; a new sell at 0.45 hits our resting buy → we
        // fill at 0.45 (our limit) as MAKER (price improvement to seller).
        // The paper model used to misclassify this as Taker at 0.43.
        let now_ms = now_unix_ms();
        // Arrival in the past so order is "resting" (past submit-latency).
        let arrival_ms = now_ms - 5_000;
        let mut book =
            BookState::from_top_of_book("token-1", 0.42, 150.0, 0.43, 150.0, 0.43, now_ms);
        book.bids = vec![Level {
            price: 0.42,
            size: 150.0,
        }];
        book.asks = vec![Level {
            price: 0.43,
            size: 150.0,
        }];
        let intent = OrderIntent {
            client_order_id: ClientOrderId::from("client-resting-maker"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            limit_price: 0.45,
            quantity: 5.0,
            reduce_only: false,
            reason: "test resting maker fill".to_string(),
            quote_level_tag: None,
            created_at_ms: arrival_ms,
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
        };
        let mut policy = paper_test_policy();
        policy.paper_post_only_reject_probability = 0.0; // skip reject path
        let mut ctx = PaperOrderContext {
            arrival_ms,
            queue_bias: 0.5,
            last_attempt_ms: arrival_ms,
            last_fill_ms: 0,
            last_fill_book_update_ms: 0,
            fill_count: 0,
            cancel_requested_at_ms: None,
        };
        let fill = paper_fill_from_book_snapshot(
            &book,
            &intent,
            now_ms,
            0.0,
            &mut ctx,
            intent.quantity,
            &policy,
        )
        .expect("expected resting maker fill when book crossed into us");
        assert!(
            matches!(fill.liquidity, FillLiquidity::Maker),
            "expected Maker liquidity for resting order book moved into us, got {:?}",
            fill.liquidity
        );
        assert!(
            (fill.price - 0.45).abs() < 1e-9,
            "expected fill at our limit price 0.45, got {}",
            fill.price
        );
    }

    #[test]
    fn fresh_submit_into_crossed_book_fills_as_taker_at_opposite() {
        // Counterexample: a brand-new submit at 0.45 when book ask is
        // already at 0.43 IS a taker scenario (we crossed at submit time).
        // Fill at best_opposite 0.43 with Taker liquidity.
        let now_ms = now_unix_ms();
        let mut book =
            BookState::from_top_of_book("token-1", 0.42, 150.0, 0.43, 150.0, 0.43, now_ms);
        book.bids = vec![Level {
            price: 0.42,
            size: 150.0,
        }];
        book.asks = vec![Level {
            price: 0.43,
            size: 150.0,
        }];
        let intent = OrderIntent {
            client_order_id: ClientOrderId::from("client-fresh-taker"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            limit_price: 0.45,
            quantity: 5.0,
            reduce_only: false,
            reason: "test fresh taker".to_string(),
            quote_level_tag: None,
            created_at_ms: now_ms,
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
        };
        let mut policy = paper_test_policy();
        policy.paper_post_only_reject_probability = 0.0; // bypass reject for the test
        policy.paper_min_fill_notional_usd = 0.0; // allow tiny initial fill ratio
                                                  // arrival_ms == now_ms; well within submit-latency window (default 150ms).
        let mut ctx = PaperOrderContext {
            arrival_ms: now_ms,
            queue_bias: 0.5,
            last_attempt_ms: now_ms,
            last_fill_ms: 0,
            last_fill_book_update_ms: 0,
            fill_count: 0,
            cancel_requested_at_ms: None,
        };
        // submit-latency gate would normally suppress; advance the
        // observed_at_ms by exactly the latency window so a fill is
        // possible but order is still "fresh" (not aged past it).
        let observed = now_ms + policy.paper_submit_latency_ms;
        let fill = paper_fill_from_book_snapshot(
            &book,
            &intent,
            observed,
            0.0,
            &mut ctx,
            intent.quantity,
            &policy,
        )
        .expect("expected taker fill at submit-latency boundary");
        assert!(
            matches!(fill.liquidity, FillLiquidity::Taker),
            "expected Taker for fresh crossing submit, got {:?}",
            fill.liquidity
        );
        assert!(
            (fill.price - 0.43).abs() < 1e-9,
            "expected fill at best_ask 0.43, got {}",
            fill.price
        );
    }

    #[test]
    fn paper_submit_latency_gate_suppresses_fill_until_window_passes() {
        let now_ms = now_unix_ms();
        let book = BookState::from_top_of_book("token-1", 0.48, 100.0, 0.50, 100.0, 0.50, now_ms);
        let intent = OrderIntent {
            client_order_id: ClientOrderId::from("client-latency"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            limit_price: 0.50,
            quantity: 20.0,
            reduce_only: false,
            reason: "test latency gate".to_string(),
            quote_level_tag: None,
            created_at_ms: now_ms,
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
        };
        let policy = paper_test_policy();
        assert!(
            policy.paper_submit_latency_ms >= 100,
            "test assumes default >= 100ms; got {}",
            policy.paper_submit_latency_ms
        );
        let mut ctx = PaperOrderContext {
            arrival_ms: now_ms,
            queue_bias: 0.5,
            last_attempt_ms: now_ms,
            last_fill_ms: 0,
            last_fill_book_update_ms: 0,
            fill_count: 0,
            cancel_requested_at_ms: None,
        };
        // Within latency window: suppressed.
        let inside = paper_fill_from_book_snapshot(
            &book,
            &intent,
            now_ms + policy.paper_submit_latency_ms - 1,
            0.0,
            &mut ctx,
            intent.quantity,
            &policy,
        );
        assert!(
            inside.is_none(),
            "expected no fill inside latency window, got {inside:?}"
        );
        // Past latency window with fresh book update: allowed.
        let later_book = BookState::from_top_of_book(
            "token-1",
            0.48,
            100.0,
            0.50,
            100.0,
            0.50,
            now_ms + policy.paper_submit_latency_ms + 50,
        );
        let outside = paper_fill_from_book_snapshot(
            &later_book,
            &intent,
            now_ms + policy.paper_submit_latency_ms + 50,
            0.0,
            &mut ctx,
            intent.quantity,
            &policy,
        );
        assert!(outside.is_some(), "expected fill past latency window");
    }

    fn live_test_policy() -> ExecutionPolicy {
        ExecutionPolicy {
            paper_mode: false,
            live_post_only: true,
            live_order_ttl_ms: 20_000,
            live_order_max_age_ms: 25_000,
            live_reconcile_missing_grace_ms: 5_000,
            live_max_submit_errors: 1,
            live_max_cancel_errors: 1,
            live_kill_on_reconcile_mismatch: true,
            paper_min_fill_notional_usd: 0.05,
            paper_max_fills_per_order: 3,
            paper_min_fill_interval_ms: 750,
            paper_market_close_at_ms: None,
            paper_market_resolution_price: None,
            paper_submit_latency_ms: 150,
            paper_queue_depth_fraction: 0.75,
            paper_post_only_reject_probability: 0.85,
            paper_cancel_race_window_ms: 500,
            paper_maker_rebate_coeff: 0.0,
            paper_taker_fee_coeff_override: None,
        }
    }

    fn paper_test_policy() -> ExecutionPolicy {
        ExecutionPolicy {
            paper_mode: true,
            ..live_test_policy()
        }
    }

    fn runtime_with_recovered_needs_reconcile_order() -> Runtime<StrategyMode> {
        runtime_with_recovered_order(
            ClientOrderId::from("client-reconcile"),
            ManagedOrderStatus::NeedsReconcile,
            1,
            "polymarket-exec-live-reconcile-replay",
        )
    }

    fn runtime_with_recovered_working_order(last_update_ms: u64) -> Runtime<StrategyMode> {
        runtime_with_recovered_order(
            ClientOrderId::from("client-working"),
            ManagedOrderStatus::Working,
            last_update_ms,
            "polymarket-exec-live-working-replay",
        )
    }

    fn runtime_with_recovered_order(
        client_order_id: ClientOrderId,
        status: ManagedOrderStatus,
        last_update_ms: u64,
        path_prefix: &str,
    ) -> Runtime<StrategyMode> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{path_prefix}-{ts}.sqlite"));
        let mut store = SqliteOrderStore::open(&path).expect("store");
        let mut record = OrderRecord::from_intent(
            "run-test",
            &OrderIntent {
                client_order_id,
                market_id: MarketId::from("market-1"),
                instrument_id: InstrumentId::from("token-1"),
                side: TradeSide::Buy,
                limit_price: 0.40,
                quantity: 5.0,
                reduce_only: false,
                reason: "test recovered live order".to_string(),
                quote_level_tag: None,
                created_at_ms: last_update_ms,
                pair_id: None,
                kind: crate::types::IntentKind::Entry,
            },
            "noop",
        );
        record.status = status;
        record.last_update_ms = last_update_ms;
        store.insert(record).expect("insert order");

        let mut runtime = Runtime::new_with_order_store(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Starting,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            StrategyMode::Noop(NoopStrategy),
            MarketContextStore::empty(),
            Some(Box::new(store)),
            "run-test".to_string(),
        );
        runtime.recover_from_store(10_000, 100);
        let _ = std::fs::remove_file(path);
        runtime
    }
}
