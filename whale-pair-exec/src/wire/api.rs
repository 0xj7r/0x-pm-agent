use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::get};
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::RwLock;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::metrics::AppMetrics;

#[derive(Debug, Clone, Serialize)]
pub struct DashboardPosition {
    pub market_id: String,
    pub instrument_id: String,
    pub quantity: f64,
    pub avg_price: f64,
    pub mark_price: Option<f64>,
    pub updated_at_ms: u64,
    pub gross_notional_usd: f64,
    pub unrealized_pnl_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardOrder {
    pub client_order_id: String,
    pub market_id: String,
    pub instrument_id: String,
    pub side: String,
    pub status: String,
    pub created_at_ms: u64,
    pub last_update_ms: u64,
    pub limit_price: f64,
    pub quantity: f64,
    pub cumulative_filled_qty: f64,
    pub remaining_qty: f64,
    pub reserved_cash_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardBook {
    pub asset_id: String,
    pub best_bid: f64,
    pub best_bid_size: f64,
    pub best_ask: f64,
    pub best_ask_size: f64,
    pub spread: f64,
    pub mid_price: Option<f64>,
    pub last_trade_price: f64,
    pub age_ms: Option<u64>,
    pub bids: Vec<(f64, f64)>,
    pub asks: Vec<(f64, f64)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardEvent {
    pub seq: u64,
    pub observed_at_ms: u64,
    pub category: String,
    pub message: String,
    pub market_id: Option<String>,
    pub instrument_id: Option<String>,
    pub client_order_id: Option<String>,
    pub order_id: Option<String>,
    pub price: Option<f64>,
    pub quantity: Option<f64>,
    pub notional_usd: Option<f64>,
    pub cash_delta_usd: Option<f64>,
    pub position_delta: Option<f64>,
    pub free_cash_after_usd: Option<f64>,
    pub gross_exposure_after_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardEventPayload {
    pub events: Vec<DashboardEvent>,
    pub generated_at_ms: u64,
    pub status: String,
    pub strategy_name: String,
    pub market_assets: Vec<String>,
    pub free_cash_usd: f64,
    pub reserved_cash_usd: f64,
    pub total_cash_usd: f64,
    pub realized_pnl_usd: f64,
    pub gross_exposure_usd: f64,
    pub event_log_len: usize,
    pub event_last_seq: u64,
    pub positions: Vec<DashboardPosition>,
    pub open_orders: Vec<DashboardOrder>,
    pub books: Vec<DashboardBook>,
    pub recent_events: Vec<DashboardEvent>,
}

#[derive(Debug, Clone, Default)]
pub struct DashboardSnapshot {
    pub status: String,
    pub strategy_name: String,
    pub market_assets: Vec<String>,
    pub free_cash_usd: f64,
    pub reserved_cash_usd: f64,
    pub total_cash_usd: f64,
    pub realized_pnl_usd: f64,
    pub gross_exposure_usd: f64,
    pub event_log_len: usize,
    pub event_last_seq: u64,
    pub generated_at_ms: u64,
    pub positions: Vec<DashboardPosition>,
    pub open_orders: Vec<DashboardOrder>,
    pub books: Vec<DashboardBook>,
    pub recent_events: Vec<DashboardEvent>,
}

impl DashboardSnapshot {
    pub fn to_payload(&self) -> DashboardEventPayload {
        DashboardEventPayload {
            events: self.recent_events.clone(),
            generated_at_ms: self.generated_at_ms,
            status: self.status.clone(),
            strategy_name: self.strategy_name.clone(),
            market_assets: self.market_assets.clone(),
            free_cash_usd: self.free_cash_usd,
            reserved_cash_usd: self.reserved_cash_usd,
            total_cash_usd: self.total_cash_usd,
            realized_pnl_usd: self.realized_pnl_usd,
            gross_exposure_usd: self.gross_exposure_usd,
            event_log_len: self.event_log_len,
            event_last_seq: self.event_last_seq,
            positions: self.positions.clone(),
            open_orders: self.open_orders.clone(),
            books: self.books.clone(),
            recent_events: self.recent_events.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DashboardUiState {
    pub metrics: Arc<AppMetrics>,
    pub snapshot: Arc<RwLock<DashboardSnapshot>>,
    pub whale_events_path: Option<PathBuf>,
    pub whale_events_limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct WhaleEvent {
    pub observed_at_ms: u64,
    pub market_id: Option<String>,
    pub instrument_id: Option<String>,
    pub side: Option<String>,
    pub close_method: Option<String>,
    pub price: Option<f64>,
    pub quantity: Option<f64>,
    pub notional_usd: Option<f64>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WhalePayload {
    pub source_path: Option<String>,
    pub events: Vec<WhaleEvent>,
}

pub async fn serve_http(
    state: DashboardUiState,
    bind: std::net::SocketAddr,
    shutdown: CancellationToken,
) -> Result<()> {
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics_handler))
        .route("/api/state", get(api_state))
        .route("/api/whale/events", get(api_whale_events))
        .with_state(state);

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind metrics server at {bind}"))?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .context("metrics server terminated unexpectedly")?;
    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn metrics_handler(State(state): State<DashboardUiState>) -> Response {
    match state.metrics.encode() {
        Ok(body) => (
            [(CONTENT_TYPE, HeaderValue::from_static("text/plain; version=0.0.4"))],
            body,
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("metrics encoding failed: {error:#}"),
        )
            .into_response(),
    }
}

async fn api_state(State(state): State<DashboardUiState>) -> Json<DashboardEventPayload> {
    let snapshot = state.snapshot.read().await.clone();
    Json(snapshot.to_payload())
}

async fn api_whale_events(State(state): State<DashboardUiState>) -> Json<WhalePayload> {
    let events = state
        .whale_events_path
        .as_deref()
        .map(|path| load_whale_events(path, state.whale_events_limit))
        .unwrap_or_default();
    let source_path = state.whale_events_path.as_deref().map(|path| path.display().to_string());
    Json(WhalePayload { source_path, events })
}

fn load_whale_events(path: &std::path::Path, limit: usize) -> Vec<WhaleEvent> {
    let payload = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(_) => return Vec::new(),
    };
    if payload.trim().is_empty() {
        return Vec::new();
    }

    let rows = if payload.trim().starts_with('[') {
        serde_json::from_str::<Vec<Value>>(&payload).unwrap_or_default()
    } else {
        payload
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect()
    };

    let mut events = rows
        .into_iter()
        .map(|row| {
            let observed_at_ms = value_to_u64(&row, &["timestamp", "observed_at_ms", "ts", "time", "time_ms"])
                .unwrap_or(0);
            let market_id = value_to_string(&row, &["market_id", "market", "marketId"]);
            let instrument_id = value_to_string(&row, &["instrument_id", "asset_id", "assetId"]);
            let side = value_to_string(&row, &["side", "direction"]);
            let price = value_to_f64(&row, &["price", "match_price", "execution_price"]);
            let quantity = value_to_f64(&row, &["quantity", "size", "qty", "matched_amount"]);
            let notional_usd = value_to_f64(&row, &["notional_usd", "notional", "gross_amount"])
                .or_else(|| price.and_then(|value| quantity.map(|q| value * q)));
            let close_method = value_to_string(
                &row,
                &["close_method", "method", "activity", "action", "event"],
            );
            let source = value_to_string(&row, &["source", "source_name", "origin"]);
            WhaleEvent {
                observed_at_ms,
                market_id,
                instrument_id,
                side,
                close_method,
                price,
                quantity,
                notional_usd,
                source,
            }
        })
        .collect::<Vec<_>>();

    events.sort_by_key(|row| row.observed_at_ms);
    events.reverse();
    if events.len() > limit {
        events.truncate(limit);
    }
    events
}

fn value_to_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| value.get(key)).and_then(|candidate| {
        candidate
            .as_u64()
            .or_else(|| candidate.as_f64().map(|value| value as u64))
            .or_else(|| candidate.as_str().and_then(|raw| raw.parse().ok()))
    })
}

fn value_to_f64(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| value.get(key)).and_then(|candidate| {
        candidate
            .as_f64()
            .or_else(|| candidate.as_str().and_then(|raw| raw.parse().ok()))
    })
}

fn value_to_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(key))
        .and_then(|candidate| candidate.as_str())
        .map(ToOwned::to_owned)
}
