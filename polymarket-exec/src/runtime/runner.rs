//! Runtime orchestration loop wiring books, websockets, execution adapter, and ops APIs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::sync::{mpsc, watch, RwLock};
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::book::{BookState, BookStore};
use crate::config::{AppConfig, UserWsAuth};
use crate::inventory::VenuePositionSnapshot;
use crate::journal::JournalFanout;
use crate::market_context::MarketContextStore;
use crate::metrics::AppMetrics;
use crate::quote_reconciler::ReconcilerConfig;
use crate::runtime::attribution::persist_runtime_outcome;
use crate::runtime::audit::AuditWriter;
use crate::runtime::dashboard::refresh_dashboard_state;
use crate::runtime::execution_policy::{ExecutionPolicy, ExecutionSyncReport, LiveSafetyState};
use crate::runtime::live_auth::{connect_live_adapter, connect_live_session};
#[cfg(test)]
use crate::runtime::live_health::portfolio_equity_floor_usd;
use crate::runtime::live_health::{
    auto_recover_live_riskoff, enforce_capital_guard, enforce_live_health, live_kill_switch_reason,
    needs_reconcile_order_count,
};
use crate::runtime::market_universe::{
    fetch_btc_5m_market_contexts, refresh_runtime_market_universe, RuntimeMarketUniverse,
};
use crate::runtime::order_store::SqliteOrderStore;
use crate::runtime::paper_fill::{
    deterministic_hash_0_95, paper_fill_from_book_snapshot, paper_post_only_should_reject,
};
use crate::runtime::types::{ManagedOrder, ManagedOrderStatus};
use crate::runtime::{Runtime, RuntimeConfig, RuntimeOutcome};
use crate::signals::BtcRegimeSnapshot;
use crate::strategy::{Strategy, StrategyMode, VenueMarketRules};
use crate::types::{
    ClientOrderId, FillLiquidity, FillReport, InstrumentId, MarketId, OrderId, OrderIntent,
    RuntimeCommand, RuntimeStatus, TradeSide,
};
use crate::wire::api::{serve_http, DashboardSnapshot, DashboardUiState};
use crate::wire::eoa_polygon::usdc_units_to_f64;
use crate::wire::execution_adapter::{
    CancelOrderRequest, ExecutionAdapter, MergePositionsRequest, PaperExecutionAdapter,
    RedeemPositionsRequest, SubmitOrderRequest, TimeInForce, VenueFill, VenuePosition,
};
use crate::wire::market_ws::MarketWsClient;
use crate::wire::spot_ws::{SpotTradeEvent, SpotWsClient};
use crate::wire::user_ws::{UserOrderEvent, UserWsClient};

const LATE_BAR_CORE_TTL_MS: u64 = 60_000;

fn runtime_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

pub async fn run() -> Result<()> {
    let mut config = AppConfig::from_env()?;
    match runtime_env("PM_BTC_5M_EXEC_MODE")
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
/// - `PM_BTC_5M_REPLAY_INPUT_PATH`: required. Path to the JSONL log.
/// - `PM_BTC_5M_PAPER_REPORT_PATH`: optional. Where to write the
///   resulting `paper_report.json`. Defaults to `<input>.replay.json`.
///
/// This is the foundation for A/B parameter calibration: change a paper
/// fill knob (queue depth, post-only reject prob, latency), re-run
/// replay against the same recorded log, and compare two report cards.
async fn run_replay_cli(config: AppConfig) -> Result<()> {
    crate::logging::init(&config)?;
    let input_path = runtime_env("PM_BTC_5M_REPLAY_INPUT_PATH")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| {
            anyhow::anyhow!("PM_BTC_5M_EXEC_MODE=replay requires PM_BTC_5M_REPLAY_INPUT_PATH")
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
    let outcome =
        crate::paper::replay::replay_runtime_from_snapshots(&config, &input_path, &output_path)
            .await?;
    info!(
        target: "replay.complete",
        records_consumed = outcome.records_consumed,
        assets_seen = outcome.assets_seen,
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
/// at this entry point regardless of PM_BTC_5M_PAPER_MODE — even if the
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
        anyhow::bail!("live reconcile mode requires PM_BTC_5M_PAPER_MODE=false");
    }
    let adapter = connect_live_adapter(&config).await?;
    let now_ms = now_unix_ms();
    let after_ms = runtime_env("PM_BTC_5M_LIVE_RECONCILE_AFTER_MS")
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
        anyhow::bail!("live smoke mode requires PM_BTC_5M_PAPER_MODE=false");
    }
    if config
        .live_kill_switch_path
        .as_ref()
        .is_some_and(|path| path.exists())
    {
        anyhow::bail!("live smoke blocked by active kill switch");
    }
    let adapter = connect_live_adapter(&config).await?;

    let asset_id = runtime_env("PM_BTC_5M_LIVE_SMOKE_ASSET_ID")
        .or_else(|| config.market_assets.first().cloned())
        .ok_or_else(|| anyhow::anyhow!("live smoke mode requires an asset id"))?;
    let market_id = runtime_env("PM_BTC_5M_LIVE_SMOKE_MARKET_ID")
        .unwrap_or_else(|| config.market_id_for_asset(&asset_id));
    let price = runtime_env("PM_BTC_5M_LIVE_SMOKE_PRICE")
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.01);
    let notional = runtime_env("PM_BTC_5M_LIVE_SMOKE_NOTIONAL_USD")
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(1.0);
    if price <= 0.0 || notional <= 0.0 {
        anyhow::bail!("live smoke price and notional must be positive");
    }
    let time_in_force = match runtime_env("PM_BTC_5M_LIVE_SMOKE_TIME_IN_FORCE")
        .unwrap_or_else(|| "GTD".to_string())
        .trim()
        .to_ascii_uppercase()
        .as_str()
    {
        "GTC" => TimeInForce::Gtc,
        "GTD" => TimeInForce::Gtd,
        other => anyhow::bail!("unsupported PM_BTC_5M_LIVE_SMOKE_TIME_IN_FORCE={other}"),
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
        anyhow::bail!("live cancel mode requires PM_BTC_5M_PAPER_MODE=false");
    }
    let raw_order_ids = runtime_env("PM_BTC_5M_LIVE_CANCEL_ORDER_IDS").ok_or_else(|| {
        anyhow::anyhow!("live cancel mode requires PM_BTC_5M_LIVE_CANCEL_ORDER_IDS")
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
/// Honors `PM_BTC_5M_LIVE_REDEEM_DRY_RUN=true` to log the planned
/// redemptions without submitting (useful before risking gas).
async fn run_live_redeem(config: AppConfig) -> Result<()> {
    crate::logging::init(&config)?;
    if config.paper_mode {
        anyhow::bail!("live redeem mode requires PM_BTC_5M_PAPER_MODE=false");
    }
    if config
        .live_kill_switch_path
        .as_ref()
        .is_some_and(|path| path.exists())
    {
        anyhow::bail!("live redeem blocked by active kill switch");
    }
    let dry_run = runtime_env("PM_BTC_5M_LIVE_REDEEM_DRY_RUN")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "yes"
            )
        })
        .unwrap_or(false);
    let redeem_collateral_token_address = runtime_env("POLYMARKET_REDEEM_COLLATERAL_TOKEN_ADDRESS")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

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
            collateral_token_address = redeem_collateral_token_address.as_deref().unwrap_or(&config.collateral_token_address),
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
            collateral_token_address: redeem_collateral_token_address.clone(),
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
                maybe_auto_wrap_pusd_after_redeem(&config, &adapter).await;
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
    let strategy =
        StrategyMode::try_from_name(&config.strategy_name, config.strategy_profile.as_ref())
            .map_err(anyhow::Error::msg)?;
    let strategy_name = strategy.name().to_string();
    let paper_fee_coeff = strategy.taker_fee_coeff();
    let shutdown = CancellationToken::new();
    let pair_profile = config
        .strategy_profile
        .as_ref()
        .map(|profile| &profile.pair);
    let runtime_defaults = crate::runtime::types::RuntimeConfig::default();
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
            require_initial_reconcile_before_entry: !config.paper_mode,
            min_merge_notional_usd: config
                .strategy_profile
                .as_ref()
                .and_then(|profile| profile.pair.min_merge_notional_usd)
                .unwrap_or(runtime_defaults.min_merge_notional_usd),
            merge_free_cash_pressure_ratio: pair_profile
                .and_then(|pair| pair.merge_pressure_free_cash_ratio)
                .unwrap_or(runtime_defaults.merge_free_cash_pressure_ratio),
            merge_gross_exposure_pressure_ratio: pair_profile
                .and_then(|pair| pair.merge_pressure_gross_exposure_ratio)
                .unwrap_or(runtime_defaults.merge_gross_exposure_pressure_ratio),
            merge_market_exposure_pressure_usd: pair_profile
                .and_then(|pair| pair.merge_market_exposure_pressure_usd)
                .unwrap_or(runtime_defaults.merge_market_exposure_pressure_usd),
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
    let mut journal = JournalFanout::open(
        config.journal_path.clone(),
        config.journal_rotate_bytes,
        config.journal_firehose_stream.clone(),
    )
    .await?;
    let mut audit = config
        .audit_path
        .as_deref()
        .map(|path| AuditWriter::open(path, config.journal_rotate_bytes))
        .transpose()?;
    // Decision log: previously gated on `if config.paper_mode { ... }` which
    // meant LIVE mode wrote no decision log, blocking shadow-live <-> tinylive
    // comparison. The writer's mode-agnostic methods (record_book_observation,
    // record_runtime_outcome, record_suppression_sample) work identically in
    // either mode. Now path-driven instead of mode-driven, mirroring how
    // book_snapshot writer works (line ~944). Live mode still won't get
    // paper_fill records (those are gated on execution_policy.paper_mode at
    // each call site), but suppression decisions and runtime events DO land,
    // which is what we need for cross-mode A/B testing.
    let mut paper_report: Option<crate::paper::report::PaperReportWriter> =
        config.paper_report_path.clone().map(|path| {
            crate::paper::report::PaperReportWriter::new(
                runtime.run_id().to_string(),
                if config.paper_mode { "paper" } else { "live" },
                path,
                now_unix_ms(),
            )
        });
    let mut book_snapshot: Option<crate::paper::snapshot::BookSnapshotWriter> = config
        .book_snapshot_log_path
        .as_deref()
        .map(crate::paper::snapshot::BookSnapshotWriter::open)
        .transpose()?;
    let mut shadow_quote: Option<crate::paper::shadow_quote::ShadowQuoteWriter> = config
        .shadow_quote_log_path
        .as_deref()
        .map(|path| {
            crate::paper::shadow_quote::ShadowQuoteWriter::open(
                path,
                config.book_snapshot_max_levels,
            )
        })
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
        paper_report.as_mut(),
        "startup",
        startup_outcome.clone(),
    )?;
    persist_audit_outcome(&mut audit, "startup", &runtime, &startup_outcome)?;
    persist_runtime_checkpoint(&mut journal, &mut runtime, now_unix_ms(), "startup")?;
    let mut effective_user_auth = config.user_auth.clone();
    let execution_adapter: Arc<dyn ExecutionAdapter> = match config.paper_mode {
        true => Arc::new(PaperExecutionAdapter::new()),
        false => {
            let live_connection = connect_live_session(&config).await?;
            effective_user_auth = live_connection.user_auth;
            maybe_auto_wrap_pusd_at_startup(&config, &live_connection.adapter).await;
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
        &mut shadow_quote,
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
    if let Some(shadow) = shadow_quote.as_mut() {
        if let Err(error) = shadow.flush() {
            warn!(
                target: "shadow_quote.flush",
                output = %shadow.path().display(),
                error = %error,
                "failed to flush shadow quote log on shutdown"
            );
        } else {
            info!(
                target: "shadow_quote.flush",
                output = %shadow.path().display(),
                "shadow quote log flushed"
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

async fn maybe_auto_wrap_pusd(
    config: &AppConfig,
    adapter: &dyn ExecutionAdapter,
    source: &'static str,
) -> Result<()> {
    if config.paper_mode || !config.live_pusd_auto_wrap {
        return Ok(());
    }
    let Some(report) = adapter
        .ensure_pusd_collateral_from_usdce(config.live_pusd_auto_wrap_min_usd)
        .await?
    else {
        info!(
            target: "live_collateral.startup",
            source,
            "pUSD auto-wrap skipped because live signer is not EOA"
        );
        return Ok(());
    };
    let usdce_before = usdc_units_to_f64(report.usdce_balance_before);
    let pusd_before = usdc_units_to_f64(report.pusd_balance_before);
    let allowance_before = usdc_units_to_f64(report.onramp_allowance_before);
    let wrapped = usdc_units_to_f64(report.wrapped_amount);
    if report.wrapped_amount.is_zero() {
        info!(
            target: "live_collateral",
            source,
            wallet = %report.wallet,
            usdce_before,
            pusd_before,
            allowance_before,
            min_wrap_usd = config.live_pusd_auto_wrap_min_usd,
            "pUSD auto-wrap checked; no USDC.e balance above threshold"
        );
    } else {
        info!(
            target: "live_collateral",
            source,
            wallet = %report.wallet,
            usdce_before,
            pusd_before,
            allowance_before,
            wrapped,
            approve_tx_hash = ?report.approve_tx_hash,
            wrap_tx_hash = ?report.wrap_tx_hash,
            "pUSD auto-wrap completed before live quoting"
        );
    }
    Ok(())
}

async fn maybe_auto_wrap_pusd_after_redeem(config: &AppConfig, adapter: &dyn ExecutionAdapter) {
    if let Err(error) = maybe_auto_wrap_pusd(config, adapter, "auto_redeem").await {
        warn!(
            target: "live_collateral",
            error = %error,
            "pUSD auto-wrap after redeem failed; continuing live loop"
        );
    }
}

async fn maybe_auto_wrap_pusd_after_merge(config: &AppConfig, adapter: &dyn ExecutionAdapter) {
    if let Err(error) = maybe_auto_wrap_pusd(config, adapter, "merge").await {
        warn!(
            target: "live_collateral",
            error = %error,
            "pUSD auto-wrap after merge failed; continuing live loop"
        );
    }
}

async fn maybe_auto_wrap_pusd_at_startup(config: &AppConfig, adapter: &dyn ExecutionAdapter) {
    if let Err(error) = maybe_auto_wrap_pusd(config, adapter, "startup").await {
        warn!(
            target: "live_collateral.startup",
            error = %error,
            "pUSD auto-wrap at startup failed; continuing live loop"
        );
    }
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
    let client = SpotWsClient::with_timeouts(
        config.spot_ws_url.clone(),
        config.spot_symbol.clone(),
        config.ping_interval,
        config.spot_ws_conn_stale_timeout,
        config.spot_ws_data_stale_timeout,
        config.spot_rest_bootstrap_url.clone(),
        config.coinbase_spot_ws_url.clone(),
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
    journal: &mut JournalFanout,
    audit: &mut Option<AuditWriter>,
    paper_report: &mut Option<crate::paper::report::PaperReportWriter>,
    book_snapshot: &mut Option<crate::paper::snapshot::BookSnapshotWriter>,
    shadow_quote: &mut Option<crate::paper::shadow_quote::ShadowQuoteWriter>,
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
    // to redeem). Operator can disable via PM_BTC_5M_LIVE_AUTO_REDEEM=false
    // (default true so live deployments don't accumulate stranded
    // collateral). 60s is well above the 5-min market cycle so we
    // never spam the relayer.
    let auto_redeem_enabled = !config.paper_mode
        && runtime_env("PM_BTC_5M_LIVE_AUTO_REDEEM")
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "0" | "no"))
            .unwrap_or(true);
    let auto_redeem_period = runtime_env("PM_BTC_5M_LIVE_AUTO_REDEEM_PERIOD_SEC")
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
                        let ingested_at_ms = now_unix_ms();
                        runtime.on_btc_trade(event.price, ingested_at_ms);
                        metrics.observe_btc_regime(&runtime.btc_regime_snapshot(ingested_at_ms));
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
                        let should_auto_wrap_pusd = matches!(
                            event,
                            UserOrderEvent::OrderMerged { .. }
                                | UserOrderEvent::OrderRedeemed { .. }
                        );
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
                            paper_report.as_mut(),
                            "user-ws",
                            user_outcome.clone(),
                        )?;
                        persist_audit_outcome(audit, "user-ws", runtime, &user_outcome)?;
                        if should_auto_wrap_pusd && !execution_policy.paper_mode {
                            if let Err(error) = maybe_auto_wrap_pusd(
                                &config,
                                execution_adapter.as_ref(),
                                "user_ws_merge_or_redeem",
                            )
                            .await
                            {
                                warn!(
                                    target: "live_collateral",
                                    error = %error,
                                    "pUSD auto-wrap after user websocket merge/redeem failed; continuing live loop"
                                );
                            }
                        }
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
                            config,
                            outcome,
                            paper_order_ctx,
                            execution_venue_map,
                            live_safety,
                            execution_adapter.clone(),
                            execution_policy,
                            &mut seen_venue_fill_keys,
                            paper_report.as_mut(),
                            shadow_quote.as_mut(),
                        )
                        .await?;
                        persist_runtime_outcome(
                            journal,
                            metrics.as_ref(),
                            runtime.event_log(),
                            paper_report.as_mut(),
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
                                config,
                                close_outcome,
                                paper_order_ctx,
                                execution_venue_map,
                                live_safety,
                                execution_adapter.clone(),
                                execution_policy,
                                &mut seen_venue_fill_keys,
                                paper_report.as_mut(),
                                shadow_quote.as_mut(),
                            )
                            .await?;
                            persist_runtime_outcome(
                                journal,
                                metrics.as_ref(),
                                runtime.event_log(),
                                paper_report.as_mut(),
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
                        config,
                        capital_outcome,
                        paper_order_ctx,
                        execution_venue_map,
                        live_safety,
                        execution_adapter.clone(),
                        execution_policy,
                        &mut seen_venue_fill_keys,
                        paper_report.as_mut(),
                        shadow_quote.as_mut(),
                    )
                    .await?;
                    persist_runtime_outcome(
                        journal,
                        metrics.as_ref(),
                        runtime.event_log(),
                        paper_report.as_mut(),
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
                            config,
                            health_outcome,
                            paper_order_ctx,
                            execution_venue_map,
                            live_safety,
                            execution_adapter.clone(),
                            execution_policy,
                            &mut seen_venue_fill_keys,
                            paper_report.as_mut(),
                            shadow_quote.as_mut(),
                        )
                        .await?;
                        persist_runtime_outcome(
                            journal,
                            metrics.as_ref(),
                            runtime.event_log(),
                            paper_report.as_mut(),
                            "live-health",
                            combined.clone(),
                        )?;
                        persist_audit_outcome(audit, "live-health", runtime, &combined)?;
                    }
                    let recover_outcome = auto_recover_live_riskoff(
                        runtime,
                        metrics.as_ref(),
                        config,
                        live_safety,
                        now_unix_ms(),
                        live_health_started_at_ms,
                    );
                    if !recover_outcome.event_seqs.is_empty()
                        || !recover_outcome.commands.is_empty()
                    {
                        let combined = execute_execution_adapter(
                            runtime,
                            books,
                            &current_assets,
                            paper_fee_coeff,
                            metrics.as_ref(),
                            config,
                            recover_outcome,
                            paper_order_ctx,
                            execution_venue_map,
                            live_safety,
                            execution_adapter.clone(),
                            execution_policy,
                            &mut seen_venue_fill_keys,
                            paper_report.as_mut(),
                            shadow_quote.as_mut(),
                        )
                        .await?;
                        persist_runtime_outcome(
                            journal,
                            metrics.as_ref(),
                            runtime.event_log(),
                            paper_report.as_mut(),
                            "live-riskoff-auto-recover",
                            combined.clone(),
                        )?;
                        persist_audit_outcome(
                            audit,
                            "live-riskoff-auto-recover",
                            runtime,
                            &combined,
                        )?;
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
                            let market_id = MarketId::from(
                                current_universe.market_id_for_asset(config, asset_id),
                            );
                            let instrument_id = InstrumentId::from(asset_id.as_str());
                            if let Some(report) = paper_report.as_mut() {
                                report.record_book_observation(&market_id, &instrument_id, &book);
                            }
                            let outcome = runtime.on_book_state(
                                market_id,
                                instrument_id,
                                &book,
                            )?;
                            let combined = execute_execution_adapter(
                                runtime,
                                books,
                                &current_assets,
                                paper_fee_coeff,
                                metrics.as_ref(),
                                config,
                                outcome,
                                paper_order_ctx,
                                execution_venue_map,
                                live_safety,
                                execution_adapter.clone(),
                                execution_policy,
                                &mut seen_venue_fill_keys,
                                paper_report.as_mut(),
                                shadow_quote.as_mut(),
                            )
                            .await?;
                            persist_runtime_outcome(
                                journal,
                                metrics.as_ref(),
                                runtime.event_log(),
                                paper_report.as_mut(),
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
                    paper_report.as_mut(),
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
                                collateral_token_address: None,
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
                                    maybe_auto_wrap_pusd_after_redeem(
                                        &config,
                                        execution_adapter.as_ref(),
                                    )
                                    .await;
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
pub(super) struct PaperOrderContext {
    pub(super) arrival_ms: u64,
    pub(super) queue_bias: f64,
    pub(super) last_attempt_ms: u64,
    pub(super) last_fill_ms: u64,
    pub(super) last_fill_book_update_ms: u64,
    pub(super) fill_count: usize,
    /// Phase 2 paper env cancel race window: when a Cancel command is
    /// received in paper mode, this is set to observed_at_ms instead of
    /// removing the context. Subsequent ticks within the configured
    /// window may still apply a fill (mirrors the live race between
    /// venue cancel ack and an in-flight fill). Once the window
    /// elapses without a fill, the deferred cancel is applied.
    pub(super) cancel_requested_at_ms: Option<u64>,
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

async fn join_task(name: &str, handle: JoinHandle<()>) {
    if let Err(error) = handle.await {
        warn!(task = name, error = ?error, "background task join failed");
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
    journal: &mut JournalFanout,
    runtime: &mut Runtime<StrategyMode>,
    observed_at_ms: u64,
    name: &str,
) -> Result<()> {
    runtime.persist_strategy_state(observed_at_ms);
    runtime.persist_runtime_status(observed_at_ms);
    let open_orders = runtime.open_order_snapshots();
    let open_orders_count = open_orders.len();
    let needs_reconcile_orders = open_orders
        .iter()
        .filter(|managed| managed.status == ManagedOrderStatus::NeedsReconcile)
        .count();
    journal.append_checkpoint(
        observed_at_ms,
        runtime.run_id(),
        name,
        open_orders_count,
        needs_reconcile_orders,
        runtime.event_log().latest_seq(),
    )?;
    journal.flush()?;
    Ok(())
}

async fn execute_execution_adapter(
    runtime: &mut Runtime<StrategyMode>,
    books: &Arc<BookStore>,
    market_assets: &[String],
    paper_fee_coeff: f64,
    metrics: &AppMetrics,
    config: &AppConfig,
    outcome: RuntimeOutcome,
    paper_order_ctx: &mut HashMap<ClientOrderId, PaperOrderContext>,
    execution_venue_map: &mut HashMap<ClientOrderId, Option<OrderId>>,
    live_safety: &mut LiveSafetyState,
    execution_adapter: Arc<dyn ExecutionAdapter>,
    execution_policy: &ExecutionPolicy,
    seen_venue_fill_keys: &mut HashSet<String>,
    paper_report: Option<&mut crate::paper::report::PaperReportWriter>,
    shadow_quote: Option<&mut crate::paper::shadow_quote::ShadowQuoteWriter>,
) -> Result<RuntimeOutcome> {
    let mut combined = RuntimeOutcome {
        commands: Vec::new(),
        event_seqs: outcome.event_seqs,
    };

    let observed_at_ms = now_unix_ms();
    let mut queue: VecDeque<RuntimeCommand> = outcome.commands.into_iter().collect();
    let mut queued_submit_ids: HashSet<ClientOrderId> = queue
        .iter()
        .filter_map(|command| match command {
            RuntimeCommand::Submit(intent) => Some(intent.client_order_id.clone()),
            _ => None,
        })
        .collect();
    let mut paper_report = paper_report;
    let mut shadow_quote = shadow_quote;

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
            books,
            observed_at_ms,
            execution_policy.live_order_max_age_ms,
        )
        .await;
        stage_outcome_commands(&mut combined, &mut queue, stale_cancel_outcome);
        let mut queued_cancel_ids: HashSet<ClientOrderId> = queue
            .iter()
            .filter_map(|command| match command {
                RuntimeCommand::Cancel {
                    client_order_id, ..
                } => Some(client_order_id.clone()),
                _ => None,
            })
            .collect();
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
                    if queued_submit_ids.insert(client_order_id.clone()) {
                        queue.push_back(RuntimeCommand::Submit(managed.intent.clone()));
                    } else {
                        debug!(
                            mode = "live",
                            client_order_id = %client_order_id,
                            "pending submit already queued in current execution cycle"
                        );
                    }
                }
                ManagedOrderStatus::NeedsReconcile => debug!(
                    mode = "live",
                    client_order_id = %client_order_id,
                    "order requires reconciliation; skipping automatic submit replay"
                ),
                ManagedOrderStatus::CancelRequested => {
                    if queued_cancel_ids.insert(client_order_id.clone()) {
                        queue.push_back(RuntimeCommand::Cancel {
                            client_order_id,
                            reason: "recovering live order".to_string(),
                        });
                    } else {
                        debug!(
                            mode = "live",
                            client_order_id = %client_order_id,
                            "cancel already queued in current execution cycle"
                        );
                    }
                }
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
                    if let Some(writer) = shadow_quote.as_deref_mut() {
                        let market_context = runtime.market_context_record(&intent.market_id);
                        let btc_regime = runtime.btc_regime_snapshot(observed_at_ms);
                        if let Err(error) = writer.record(
                            &intent,
                            &book,
                            observed_at_ms,
                            market_context.and_then(|context| context.price_to_beat),
                            btc_regime.last_price,
                            btc_regime.realized_vol_5m_bps,
                        ) {
                            warn!(
                                target: "shadow_quote",
                                client_order_id = %intent.client_order_id,
                                error = %error,
                                "failed to write shadow quote record"
                            );
                        }
                    }
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

                if let Some(reason) =
                    live_kill_switch_reason(execution_policy.live_kill_switch_path.as_deref())
                {
                    warn!(
                        target: "polymarket_exec::runtime::runner",
                        mode = "live",
                        client_order_id = %intent.client_order_id,
                        reason = %reason,
                        "blocking live submit before venue"
                    );
                    if runtime.status() != RuntimeStatus::Degraded {
                        metrics.observe_riskoff_transition();
                        let kill_outcome = runtime.degrade_and_cancel_all(
                            observed_at_ms,
                            format!("live health failure: {reason}"),
                        );
                        let chained_commands = kill_outcome.commands.clone();
                        combined.extend(kill_outcome);
                        for command in chained_commands {
                            queue.push_back(command);
                        }
                    }
                    paper_order_ctx.remove(&intent.client_order_id);
                    execution_venue_map.remove(&intent.client_order_id);
                    continue;
                }

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
                let mut submit_req =
                    submit_request_from_intent(&intent, observed_at_ms, execution_policy);
                if submit_req.post_only {
                    let tick = runtime
                        .venue_market_rules(&intent.market_id)
                        .map(|rules| rules.minimum_tick_size)
                        .filter(|tick| tick.is_finite() && *tick > 0.0)
                        .unwrap_or(0.01);
                    if let Some(book) = books.snapshot(intent.instrument_id.as_str()).await {
                        match passive_post_only_limit_price(&submit_req, &book, tick) {
                            Some(adjusted_price) => {
                                if (adjusted_price - submit_req.limit_price).abs() > f64::EPSILON {
                                    warn!(
                                        mode = "live",
                                        client_order_id = %submit_req.client_order_id,
                                        instrument_id = %submit_req.instrument_id,
                                        old_limit_price = submit_req.limit_price,
                                        adjusted_limit_price = adjusted_price,
                                        best_bid = book.best_bid,
                                        best_ask = book.best_ask,
                                        tick,
                                        "adjusting post-only submit to current passive book price"
                                    );
                                    submit_req.limit_price = adjusted_price;
                                }
                            }
                            None => {
                                warn!(
                                    mode = "live",
                                    client_order_id = %submit_req.client_order_id,
                                    instrument_id = %submit_req.instrument_id,
                                    limit_price = submit_req.limit_price,
                                    best_bid = book.best_bid,
                                    best_ask = book.best_ask,
                                    tick,
                                    "rejecting post-only submit locally because current book has no passive price"
                                );
                                paper_order_ctx.remove(&intent.client_order_id);
                                execution_venue_map.remove(&intent.client_order_id);
                                let reject_outcome = runtime.on_order_rejected(
                                    &intent.client_order_id,
                                    "post-only-cross-live-preflight",
                                    observed_at_ms,
                                );
                                let chained_commands = reject_outcome.commands.clone();
                                combined.extend(reject_outcome);
                                for command in chained_commands {
                                    queue.push_back(command);
                                }
                                continue;
                            }
                        }
                    }
                }
                // Latency instrumentation (2026-04-29): measure two spans —
                // wire_latency (submit call → adapter return) and
                // pipeline_latency (intent creation → adapter return). Used
                // to validate whether internal pipeline overhead is
                // contributing to FAK no-match rejects.
                let submit_call_start_ms = now_unix_ms();
                let submit_result = execution_adapter.submit(submit_req).await;
                let submit_ack_ms = now_unix_ms();
                let wire_latency_ms = submit_ack_ms.saturating_sub(submit_call_start_ms);
                let pipeline_latency_ms = submit_ack_ms.saturating_sub(intent.created_at_ms);
                match submit_result {
                    Ok(ack) if ack.accepted => {
                        live_safety.consecutive_submit_errors = 0;
                        debug!(
                            target: "polymarket_exec::runtime::runner",
                            mode = "live",
                            client_order_id = %intent.client_order_id,
                            wire_latency_ms,
                            pipeline_latency_ms,
                            "submit accepted"
                        );
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
                        let immediate_live_stop =
                            active_order && submit_rejection_requires_immediate_live_stop(&reason);
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
                            immediate_live_stop,
                            active_order,
                            wire_latency_ms,
                            pipeline_latency_ms,
                            "submit ack-rejected by venue (full venue text)"
                        );
                        paper_order_ctx.remove(&intent.client_order_id);
                        if active_order {
                            let rejected_outcome = runtime.on_order_rejected(
                                &intent.client_order_id,
                                reason.clone(),
                                ack.accepted_at_ms,
                            );
                            combined.extend(rejected_outcome);
                            if immediate_live_stop {
                                metrics.observe_riskoff_transition();
                                combined.extend(runtime.degrade_and_cancel_all(
                                    ack.accepted_at_ms,
                                    format!(
                                        "deterministic live submit rejection; risk-off until wire encoding is fixed: {reason}"
                                    ),
                                ));
                            }
                        } else {
                            execution_venue_map.remove(&intent.client_order_id);
                        }
                    }
                    Err(error) => {
                        let active_order =
                            runtime_has_active_order(runtime, &intent.client_order_id);
                        let error_text = error.to_string();
                        let counts_against_budget = submit_rejection_counts_against_live_budget(
                            &error_text,
                            execution_policy.live_post_only,
                        );
                        if active_order {
                            if counts_against_budget {
                                live_safety.consecutive_submit_errors =
                                    live_safety.consecutive_submit_errors.saturating_add(1);
                            }
                            let immediate_live_stop =
                                submit_rejection_requires_immediate_live_stop(&error_text);
                            warn!(
                                target: "polymarket_exec::runtime::runner",
                                mode = "live",
                                client_order_id = %intent.client_order_id,
                                instrument_id = %intent.instrument_id,
                                price = intent.limit_price,
                                qty = intent.quantity,
                                error = %error,
                                error_kind = std::any::type_name_of_val(&error),
                                counts_against_budget,
                                immediate_live_stop,
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
                            let reason = error.to_string();
                            let immediate_live_stop =
                                submit_rejection_requires_immediate_live_stop(&reason);
                            let rejected_outcome = runtime.on_order_rejected(
                                &intent.client_order_id,
                                reason.clone(),
                                observed_at_ms,
                            );
                            paper_order_ctx.remove(&intent.client_order_id);
                            combined.extend(rejected_outcome);
                            if immediate_live_stop {
                                metrics.observe_riskoff_transition();
                                combined.extend(runtime.degrade_and_cancel_all(
                                    observed_at_ms,
                                    format!(
                                        "deterministic live submit rejection; risk-off until wire encoding is fixed: {reason}"
                                    ),
                                ));
                            }
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

                let venue_order_id = execution_venue_map.get(&client_order_id).cloned().flatten();
                let Some(venue_order_id) = venue_order_id else {
                    warn!(
                        mode = "live",
                        client_order_id = %client_order_id,
                        "live cancel missing venue_order_id; forcing venue sync before deciding local cancel"
                    );
                    live_safety.consecutive_cancel_errors = 0;
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
                    if execution_venue_map
                        .get(&client_order_id)
                        .cloned()
                        .flatten()
                        .is_some()
                    {
                        queue.push_front(RuntimeCommand::Cancel {
                            client_order_id,
                            reason,
                        });
                        continue;
                    }
                    let cancelled_outcome = runtime.on_order_cancelled(
                        &client_order_id,
                        "cancelled locally before venue_order_id was observed after venue sync",
                        observed_at_ms,
                    );
                    combined.extend(cancelled_outcome);
                    paper_order_ctx.remove(&client_order_id);
                    execution_venue_map.remove(&client_order_id);
                    continue;
                };

                let cancel_req = CancelOrderRequest {
                    client_order_id: client_order_id.clone(),
                    venue_order_id: Some(venue_order_id),
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
                        if uncertain_cancel {
                            combined.extend(runtime.mark_order_needs_reconcile(
                                &client_order_id,
                                ack.accepted_at_ms,
                                format!(
                                    "cancel rejected because venue order is likely terminal; awaiting fill/cancel sync: {reason}"
                                ),
                            ));
                        } else {
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
                        runtime.mark_pending_merge_accepted(&intent.market_id, ack.accepted_at_ms);
                        maybe_auto_wrap_pusd_after_merge(&config, execution_adapter.as_ref()).await;
                    }
                    Ok(ack) => {
                        let reason = ack.venue_message.unwrap_or_else(|| {
                            "execution venue rejected merge positions".to_string()
                        });
                        runtime.block_pending_merge(&intent.market_id, ack.accepted_at_ms, &reason);
                        warn!(
                            mode = "live",
                            market_id = %intent.market_id,
                            yes_instrument_id = %intent.yes_instrument_id,
                            no_instrument_id = %intent.no_instrument_id,
                            quantity = intent.quantity,
                            command_id = %intent.command_id,
                            reason = %reason,
                            "live merge rejected; blocking identical CTF recycle without global risk-off"
                        );
                    }
                    Err(error) => {
                        if error.is_retryable() {
                            runtime.clear_pending_merge(&intent.market_id, observed_at_ms);
                            warn!(
                                mode = "live",
                                market_id = %intent.market_id,
                                yes_instrument_id = %intent.yes_instrument_id,
                                no_instrument_id = %intent.no_instrument_id,
                                error = %error,
                                "transient merge failure; will retry on next reconcile sweep"
                            );
                        } else {
                            runtime.block_pending_merge(
                                &intent.market_id,
                                observed_at_ms,
                                error.to_string(),
                            );
                            warn!(
                                mode = "live",
                                market_id = %intent.market_id,
                                yes_instrument_id = %intent.yes_instrument_id,
                                no_instrument_id = %intent.no_instrument_id,
                                quantity = intent.quantity,
                                command_id = %intent.command_id,
                                error = %error,
                                "live merge failed; blocking identical CTF recycle without global risk-off"
                            );
                        }
                    }
                }
            }
            RuntimeCommand::Redeem(intent) => {
                let Some(condition_id) = intent.condition_id.clone() else {
                    warn!(
                        mode = if execution_policy.paper_mode { "paper" } else { "live" },
                        market_id = %intent.market_id,
                        command_id = %intent.command_id,
                        "redeem command skipped: missing condition_id"
                    );
                    continue;
                };
                let request = RedeemPositionsRequest {
                    command_id: intent.command_id.clone(),
                    market_id: intent.market_id.clone(),
                    condition_id,
                    collateral_token_address: None,
                    index_sets: vec![1, 2],
                    submitted_at_ms: observed_at_ms,
                };
                match execution_adapter.redeem_positions(request).await {
                    Ok(ack) if ack.accepted => {
                        info!(
                            mode = if execution_policy.paper_mode { "paper" } else { "live" },
                            market_id = %intent.market_id,
                            command_id = %intent.command_id,
                            message = ?ack.venue_message,
                            "redeem command accepted by execution adapter"
                        );
                        if !execution_policy.paper_mode {
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
                    }
                    Ok(ack) => {
                        warn!(
                            mode = if execution_policy.paper_mode { "paper" } else { "live" },
                            market_id = %intent.market_id,
                            command_id = %intent.command_id,
                            reason = ?ack.venue_message,
                            "redeem command rejected by execution adapter"
                        );
                    }
                    Err(error) => {
                        warn!(
                            mode = if execution_policy.paper_mode { "paper" } else { "live" },
                            market_id = %intent.market_id,
                            command_id = %intent.command_id,
                            error = %error,
                            "redeem command failed"
                        );
                    }
                }
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

fn submit_request_from_intent(
    intent: &OrderIntent,
    observed_at_ms: u64,
    execution_policy: &ExecutionPolicy,
) -> SubmitOrderRequest {
    // Hedge-rescue intents are taker IOC orders that lift the opposite leg
    // to manufacture paired inventory (whales' atomic completion pattern).
    // They MUST cross the book — post-only would defeat the whole purpose.
    let is_hedge_rescue = intent.kind == crate::types::IntentKind::Close;
    let is_late_bar_core = intent
        .quote_level_tag
        .as_deref()
        .is_some_and(|tag| tag.starts_with("mm-late-bar-core"));
    let is_aggressive_late_fav = intent
        .quote_level_tag
        .as_deref()
        .is_some_and(|tag| tag.starts_with("late-fav-taker"));
    let live_expires_at_ms = (!execution_policy.paper_mode
        && execution_policy.live_order_ttl_ms > 0
        && !is_hedge_rescue
        && !is_aggressive_late_fav
        && !is_late_bar_core)
        .then_some(observed_at_ms.saturating_add(execution_policy.live_order_ttl_ms))
        .or_else(|| {
            (!execution_policy.paper_mode && is_late_bar_core)
                .then_some(observed_at_ms.saturating_add(LATE_BAR_CORE_TTL_MS))
        });
    let (time_in_force, post_only) = if is_hedge_rescue || is_aggressive_late_fav {
        (TimeInForce::Ioc, false)
    } else if is_late_bar_core {
        (TimeInForce::Gtd, !execution_policy.paper_mode)
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
    // V2 SDK enforces strict decimal validation on order size: max 2 decimal
    // places. Strategy computes qty=clip_usd/price which produces values like
    // 9.0909090909 (15 decimals) that V1 silently accepted but V2 rejects with
    // "Validation: invalid: Unable to build Order: Size N has 15 decimal
    // places. Maximum lot size is 2". Round to 2 decimal places at the wire
    // boundary so the strategy can stay precision-agnostic.
    let venue_quantity = (intent.quantity * 100.0).floor() / 100.0;
    SubmitOrderRequest {
        client_order_id: intent.client_order_id.clone(),
        market_id: intent.market_id.clone(),
        instrument_id: intent.instrument_id.clone(),
        side: intent.side,
        limit_price: intent.limit_price,
        quantity: venue_quantity,
        post_only,
        time_in_force,
        expires_at_ms: live_expires_at_ms,
        strategy_tag: "runtime".to_string(),
        quote_level_tag: intent.quote_level_tag.clone(),
        submitted_at_ms: observed_at_ms,
    }
}

fn passive_post_only_limit_price(
    request: &SubmitOrderRequest,
    book: &BookState,
    tick: f64,
) -> Option<f64> {
    let tick = tick.max(0.0001);
    match request.side {
        TradeSide::Buy => {
            let best_ask = book.best_ask;
            if !best_ask.is_finite() || best_ask <= tick {
                return None;
            }
            let max_passive = best_ask - tick;
            let price = request.limit_price.min(max_passive);
            (price > 0.0 && price < best_ask && price < 1.0).then_some(price)
        }
        TradeSide::Sell => {
            let best_bid = book.best_bid;
            if !best_bid.is_finite() || best_bid <= 0.0 {
                return None;
            }
            let min_passive = best_bid + tick;
            let price = request.limit_price.max(min_passive);
            (price > best_bid && price > 0.0 && price < 1.0).then_some(price)
        }
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
    let reported_missing_local_orders = report
        .missing_local_orders
        .iter()
        .chain(report.pending_missing_local_orders.iter())
        .cloned()
        .collect::<HashSet<_>>();
    live_safety
        .suspect_missing_local_orders
        .retain(|client_order_id, _| reported_missing_local_orders.contains(client_order_id));
    let mut unresolved_missing_local_orders = Vec::new();
    for client_order_id in &report.missing_local_orders {
        let (removed, terminal_outcome) = runtime.remove_active_order_if_durable_terminal(
            client_order_id,
            now_ms,
            "venue sync missing locally tracked order already terminal in durable store",
        );
        outcome.extend(terminal_outcome);
        if !removed {
            let (finalized, partial_fill_outcome) = runtime.finalize_missing_partial_fill_order(
                client_order_id,
                now_ms,
                "non-resting partial fill absent from open-order sync; expiring unfilled remainder",
            );
            outcome.extend(partial_fill_outcome);
            if finalized {
                continue;
            }
            unresolved_missing_local_orders.push(client_order_id.clone());
        }
    }

    let mut still_unresolved_missing_local_orders = Vec::new();
    let open_order_by_client = runtime
        .open_order_snapshots()
        .into_iter()
        .map(|managed| (managed.intent.client_order_id.clone(), managed))
        .collect::<HashMap<_, _>>();
    for client_order_id in unresolved_missing_local_orders {
        let Some(managed) = open_order_by_client.get(&client_order_id) else {
            continue;
        };
        if can_defer_missing_local_order_escalation(managed) {
            let suspect = live_safety
                .suspect_missing_local_orders
                .entry(client_order_id.clone())
                .or_default();
            if suspect.first_seen_ms == 0 {
                suspect.first_seen_ms = now_ms;
            }
            suspect.observed_count = suspect.observed_count.saturating_add(1);
            let suspect_age_ms = now_ms.saturating_sub(suspect.first_seen_ms);
            if suspect.observed_count < 3 && suspect_age_ms < 8_000 {
                debug!(
                    mode = "live",
                    client_order_id = %client_order_id,
                    market_id = %managed.intent.market_id,
                    instrument_id = %managed.intent.instrument_id,
                    status = ?managed.status,
                    suspect_observed_count = suspect.observed_count,
                    suspect_age_ms,
                    "passive live order absent from open-order sync; deferring NeedsReconcile for fill/cancel race"
                );
                continue;
            }
        }
        if can_finalize_missing_passive_entry_from_authoritative_absence(&report, managed) {
            info!(
                mode = "live",
                client_order_id = %client_order_id,
                market_id = %managed.intent.market_id,
                instrument_id = %managed.intent.instrument_id,
                "passive live entry absent from open orders and authoritative positions; treating unfilled remainder as terminal"
            );
            live_safety
                .suspect_missing_local_orders
                .remove(&client_order_id);
            outcome.extend(runtime.on_order_cancelled(
                &client_order_id,
                "venue open-order sync and authoritative position snapshot show passive entry is terminal",
                now_ms,
            ));
            continue;
        }
        debug!(
            mode = "live",
            client_order_id = %client_order_id,
            market_id = %managed.intent.market_id,
            instrument_id = %managed.intent.instrument_id,
            status = ?managed.status,
            "keeping missing local order unresolved; empty balances do not prove resting orders are cancelled"
        );
        still_unresolved_missing_local_orders.push(client_order_id);
    }
    let unresolved_missing_local_orders = still_unresolved_missing_local_orders;

    if report.errors > 0 || !unresolved_missing_local_orders.is_empty() {
        live_safety.consecutive_reconcile_mismatches = live_safety
            .consecutive_reconcile_mismatches
            .saturating_add(1);
        metrics.observe_reconcile_failure();
    } else {
        live_safety.consecutive_reconcile_mismatches = 0;
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

    if report.balance_synced {
        live_safety.last_venue_cash_usd = report.venue_cash_usd;
        live_safety.last_venue_position_count = report.venue_position_count;
        live_safety.last_venue_balance_observed_at_ms = report.venue_balance_observed_at_ms;
        if let Some(cash_usd) = report.venue_cash_usd {
            let observed_at_ms = report.venue_balance_observed_at_ms.unwrap_or(now_ms);
            match runtime.reconcile_venue_cash(cash_usd, observed_at_ms) {
                Ok(adjustment) => {
                    debug!(
                        mode = "live",
                        venue_cash_usd = cash_usd,
                        free_cash_after_usd = adjustment.free_cash_after_usd,
                        reserved_cash_after_usd = adjustment.reserved_cash_after_usd,
                        "reconciled runtime cash from venue balance"
                    );
                }
                Err(error) => {
                    live_safety.consecutive_reconcile_mismatches = live_safety
                        .consecutive_reconcile_mismatches
                        .saturating_add(1);
                    metrics.observe_reconcile_failure();
                    warn!(error = ?error, "failed to reconcile venue cash snapshot");
                }
            }
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

    for client_order_id in report.pending_missing_local_orders {
        debug!(
            mode = "live",
            client_order_id = %client_order_id,
            "live order temporarily absent from open-order sync within grace; deferring reconcile"
        );
    }

    for client_order_id in unresolved_missing_local_orders {
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

fn can_defer_missing_local_order_escalation(managed: &ManagedOrder) -> bool {
    if !matches!(
        managed.status,
        ManagedOrderStatus::Submitted
            | ManagedOrderStatus::Working
            | ManagedOrderStatus::CancelRequested
    ) {
        return false;
    }
    if managed.intent.kind == crate::types::IntentKind::Close {
        return false;
    }
    !managed
        .intent
        .quote_level_tag
        .as_deref()
        .is_some_and(|tag| tag.starts_with("late-fav-taker"))
}

fn can_finalize_missing_passive_entry_from_authoritative_absence(
    report: &ExecutionSyncReport,
    managed: &ManagedOrder,
) -> bool {
    if !report.balance_synced || !report.venue_positions_authoritative {
        return false;
    }
    if managed.intent.kind != crate::types::IntentKind::Entry
        || managed.intent.reduce_only
        || managed.intent.side != TradeSide::Buy
    {
        return false;
    }

    !report.venue_positions.iter().any(|position| {
        position.market_id == managed.intent.market_id
            && position.instrument_id == managed.intent.instrument_id
            && position.quantity > 1e-9
    })
}

fn submit_rejection_counts_against_live_budget(reason: &str, post_only: bool) -> bool {
    let lower = reason.to_ascii_lowercase();
    if post_only && lower == "execution venue rejected submit" {
        return false;
    }
    !(lower.contains("post-only")
        || lower.contains("crosses book")
        || lower.contains("would cross")
        || lower.contains("would take liquidity")
        || lower.contains("no orders found to match")
        || lower.contains("fak orders are partially filled or killed"))
}

fn submit_rejection_requires_immediate_live_stop(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    lower.contains("invalid amounts")
        || lower.contains("maker amount supports a max accuracy")
        || lower.contains("taker amount a max")
        || (lower.contains("unable to build order")
            && (lower.contains("decimal") || lower.contains("precision")))
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

async fn cancel_stale_live_orders(
    runtime: &mut Runtime<StrategyMode>,
    books: &Arc<BookStore>,
    now_ms: u64,
    max_age_ms: u64,
) -> RuntimeOutcome {
    let mut outcome = RuntimeOutcome::default();
    if max_age_ms == 0 {
        return outcome;
    }
    let btc_regime = runtime.btc_regime_snapshot(now_ms);
    let stale_orders = runtime
        .open_order_snapshots()
        .into_iter()
        .filter(|managed| {
            matches!(
                managed.status,
                ManagedOrderStatus::Submitted | ManagedOrderStatus::Working
            ) && now_ms.saturating_sub(managed.intent.created_at_ms) >= max_age_ms
        })
        .collect::<Vec<_>>();
    for managed in stale_orders {
        let client_order_id = managed.intent.client_order_id.clone();
        let Some(book) = books.snapshot(managed.intent.instrument_id.as_str()).await else {
            outcome.extend(runtime.request_cancel_order(
                &client_order_id,
                now_ms,
                format!("live order max age exceeded {max_age_ms}ms; no current book snapshot"),
            ));
            continue;
        };

        match stale_live_order_action(&managed, &book, &btc_regime) {
            StaleLiveOrderAction::Preserve { reason } => {
                tracing::info!(
                    client_order_id = %client_order_id,
                    instrument_id = %managed.intent.instrument_id,
                    limit_price = managed.intent.limit_price,
                    best_bid = book.best_bid,
                    best_ask = book.best_ask,
                    btc_return_30s_bps = ?btc_regime.return_30s_bps,
                    btc_return_60s_bps = ?btc_regime.return_60s_bps,
                    age_ms = now_ms.saturating_sub(managed.intent.created_at_ms),
                    reason,
                    "preserving aged live order because current book still supports maker quote"
                );
            }
            StaleLiveOrderAction::Cancel { reason } => {
                outcome.extend(runtime.request_cancel_order(
                    &client_order_id,
                    now_ms,
                    format!("live order max age exceeded {max_age_ms}ms; {reason}"),
                ));
            }
        }
    }
    outcome
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StaleLiveOrderAction {
    Preserve { reason: &'static str },
    Cancel { reason: &'static str },
}

fn stale_live_order_action(
    order: &ManagedOrder,
    book: &BookState,
    btc_regime: &BtcRegimeSnapshot,
) -> StaleLiveOrderAction {
    if order
        .intent
        .quote_level_tag
        .as_deref()
        .is_some_and(|tag| tag.starts_with("paired-core:"))
    {
        return StaleLiveOrderAction::Cancel {
            reason: "paired-core quote aged out; require strategy to revalidate paired placement",
        };
    }

    let best_bid = book.best_bid;
    let best_ask = book.best_ask;
    if !best_bid.is_finite() || !best_ask.is_finite() || best_bid <= 0.0 || best_ask <= 0.0 {
        return StaleLiveOrderAction::Cancel {
            reason: "invalid current book",
        };
    }

    let spread = (best_ask - best_bid).max(0.0);
    let tolerance = spread.clamp(0.01, 0.03);
    if last_trade_invalidates_order(order, book, tolerance) {
        return StaleLiveOrderAction::Cancel {
            reason: "last-trade drift invalidates aged quote price",
        };
    }
    if btc_drift_invalidates_order(order, btc_regime) {
        return StaleLiveOrderAction::Cancel {
            reason: "btc drift invalidates aged quote side",
        };
    }
    match order.intent.side {
        TradeSide::Buy => {
            if order.intent.limit_price >= best_ask {
                return StaleLiveOrderAction::Cancel {
                    reason: "buy quote would cross current ask",
                };
            }
            if order.intent.limit_price > best_bid + tolerance {
                return StaleLiveOrderAction::Cancel {
                    reason: "buy quote is stale above current best bid",
                };
            }
            StaleLiveOrderAction::Preserve {
                reason: "buy quote remains behind current ask and near best bid",
            }
        }
        TradeSide::Sell => {
            if order.intent.limit_price <= best_bid {
                return StaleLiveOrderAction::Cancel {
                    reason: "sell quote would cross current bid",
                };
            }
            if order.intent.limit_price < best_ask - tolerance {
                return StaleLiveOrderAction::Cancel {
                    reason: "sell quote is stale below current best ask",
                };
            }
            StaleLiveOrderAction::Preserve {
                reason: "sell quote remains above current bid and near best ask",
            }
        }
    }
}

fn last_trade_invalidates_order(order: &ManagedOrder, book: &BookState, tolerance: f64) -> bool {
    let last_trade_price = book.last_trade_price;
    if !last_trade_price.is_finite() || last_trade_price <= 0.0 {
        return false;
    }
    match order.intent.side {
        TradeSide::Buy => order.intent.limit_price > last_trade_price + tolerance,
        TradeSide::Sell => order.intent.limit_price < last_trade_price - tolerance,
    }
}

fn btc_drift_invalidates_order(order: &ManagedOrder, btc_regime: &BtcRegimeSnapshot) -> bool {
    let Some(tag) = order.intent.quote_level_tag.as_deref() else {
        return false;
    };
    if !matches!(order.intent.side, TradeSide::Buy) {
        return false;
    }
    let leg = if tag.contains(":yes:") {
        Some(TradeSide::Buy)
    } else if tag.contains(":no:") {
        Some(TradeSide::Sell)
    } else {
        None
    };
    let Some(return_30s_bps) = btc_regime.return_30s_bps else {
        return false;
    };
    let return_60s_bps = btc_regime.return_60s_bps.unwrap_or(return_30s_bps);
    if return_30s_bps.signum() != return_60s_bps.signum() {
        return false;
    }
    let vol_floor_bps = btc_regime
        .realized_vol_5m_bps
        .filter(|vol| vol.is_finite())
        .unwrap_or(6.0)
        .max(6.0);
    let threshold_bps = (vol_floor_bps * 0.75).clamp(4.0, 25.0);
    match leg {
        Some(TradeSide::Buy) => return_30s_bps < -threshold_bps && return_60s_bps < -threshold_bps,
        Some(TradeSide::Sell) => return_30s_bps > threshold_bps && return_60s_bps > threshold_bps,
        _ => false,
    }
}

fn venue_fill_key(fill: &VenueFill) -> String {
    format!(
        "{}:{}:{:.8}:{:.8}:{}",
        fill.venue_order_id, fill.instrument_id, fill.price, fill.quantity, fill.observed_at_ms
    )
}

pub(super) fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
#[path = "../../tests/unit/runtime_runner.rs"]
mod runner_tests;
