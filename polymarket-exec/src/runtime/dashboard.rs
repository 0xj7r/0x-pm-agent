use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::RwLock;

use crate::book::BookStore;
use crate::config::AppConfig;
use crate::metrics::AppMetrics;
use crate::runtime::Runtime;
use crate::strategy::StrategyMode;
use crate::wire::api::{
    DashboardBook, DashboardEvent, DashboardOrder, DashboardPosition, DashboardSnapshot,
};

pub(super) async fn refresh_dashboard_state(
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

fn is_yes_like(raw: &str) -> bool {
    raw.contains("yes")
        || raw.contains("up")
        || raw.contains("long")
        || raw.contains("bull")
        || raw.contains("call")
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
