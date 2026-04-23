//! Live CLOB execution adapter contract (Section 8.1).
//! Agent A scope: execution adapter implementation and test coverage.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::types::{
    ClientOrderId, EpochMillis, FillLiquidity, InstrumentId, MarketId, OrderId, TradeSide,
};

#[derive(Clone, Debug, PartialEq)]
pub struct SubmitOrderRequest {
    pub client_order_id: ClientOrderId,
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub side: TradeSide,
    pub limit_price: f64,
    pub quantity: f64,
    pub post_only: bool,
    pub time_in_force: TimeInForce,
    pub strategy_tag: String,
    pub quote_level_tag: Option<String>,
    pub submitted_at_ms: EpochMillis,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimeInForce {
    #[default]
    Gtc,
    Ioc,
    Fok,
    Gtd,
}

impl TimeInForce {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gtc => "GTC",
            Self::Ioc => "IOC",
            Self::Fok => "FOK",
            Self::Gtd => "GTD",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubmitOrderAck {
    pub client_order_id: ClientOrderId,
    pub venue_order_id: Option<OrderId>,
    pub accepted: bool,
    pub accepted_at_ms: EpochMillis,
    pub venue_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CancelOrderRequest {
    pub client_order_id: ClientOrderId,
    pub venue_order_id: Option<OrderId>,
    pub reason: String,
    pub submitted_at_ms: EpochMillis,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CancelOrderAck {
    pub client_order_id: ClientOrderId,
    pub venue_order_id: Option<OrderId>,
    pub accepted: bool,
    pub accepted_at_ms: EpochMillis,
    pub venue_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VenueOpenOrder {
    pub venue_order_id: OrderId,
    pub client_order_id: Option<ClientOrderId>,
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub side: TradeSide,
    pub limit_price: f64,
    pub original_qty: f64,
    pub remaining_qty: f64,
    pub created_at_ms: EpochMillis,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VenueBalances {
    pub cash_usd: f64,
    pub positions: Vec<VenuePosition>,
    pub observed_at_ms: EpochMillis,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VenuePosition {
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub quantity: f64,
    pub average_cost_usd: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VenueFill {
    pub venue_order_id: OrderId,
    pub client_order_id: Option<ClientOrderId>,
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub side: TradeSide,
    pub price: f64,
    pub quantity: f64,
    pub fee_usd: f64,
    pub liquidity: FillLiquidity,
    pub observed_at_ms: EpochMillis,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PolymarketCredentials {
    pub api_key: String,
    pub api_secret: String,
    pub api_passphrase: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PolymarketConfig {
    pub api_url: String,
    pub credentials: Option<PolymarketCredentials>,
}

impl Default for PolymarketConfig {
    fn default() -> Self {
        Self {
            api_url: "https://clob.polymarket.com".to_string(),
            credentials: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionError {
    TransientNetwork(String),
    AuthFailure(String),
    BadRequest(String),
    RateLimit {
        retry_after_ms: Option<u64>,
    },
    VenueRejection(String),
    UncertainOutcome(String),
}

impl ExecutionError {
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::TransientNetwork(_) | Self::RateLimit { .. } | Self::UncertainOutcome(_)
        )
    }

    pub fn requires_reconcile(&self) -> bool {
        matches!(self, Self::UncertainOutcome(_))
    }
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TransientNetwork(msg) => write!(f, "transient network: {msg}"),
            Self::AuthFailure(msg) => write!(f, "auth failure: {msg}"),
            Self::BadRequest(msg) => write!(f, "bad request: {msg}"),
            Self::RateLimit { retry_after_ms } => match retry_after_ms {
                Some(ms) => write!(f, "rate limit (retry after {ms}ms)"),
                None => write!(f, "rate limit"),
            },
            Self::VenueRejection(msg) => write!(f, "venue rejection: {msg}"),
            Self::UncertainOutcome(msg) => write!(f, "uncertain outcome: {msg}"),
        }
    }
}

impl std::error::Error for ExecutionError {}

#[async_trait]
pub trait ExecutionAdapter: Send + Sync {
    async fn submit(&self, req: SubmitOrderRequest) -> Result<SubmitOrderAck, ExecutionError>;
    async fn cancel(&self, req: CancelOrderRequest) -> Result<CancelOrderAck, ExecutionError>;
    async fn sync_open_orders(&self) -> Result<Vec<VenueOpenOrder>, ExecutionError>;
    async fn sync_balances(&self) -> Result<VenueBalances, ExecutionError>;
}

#[derive(Default)]
struct AdapterState {
    submitted_orders: HashMap<ClientOrderId, SubmitOrderAck>,
    venue_order_map: HashMap<ClientOrderId, OrderId>,
    canceled_orders: HashSet<ClientOrderId>,
}

#[derive(Clone, Debug)]
struct HttpResponse {
    status: u16,
    body: Value,
    retry_after_ms: Option<u64>,
}

#[async_trait]
trait PolymarketHttpClient: Send + Sync {
    async fn call(
        &self,
        method: HttpMethod,
        path: &str,
        body: Option<&Value>,
    ) -> Result<HttpResponse, ExecutionError>;
}

#[derive(Clone, Copy)]
enum HttpMethod {
    Get,
    Post,
    Delete,
}

#[derive(Clone)]
struct ReqwestPolymarketClient {
    api_url: String,
    credentials: Option<PolymarketCredentials>,
    client: reqwest::Client,
}

impl ReqwestPolymarketClient {
    fn build_url(&self, path: &str) -> String {
        let trimmed_base = self.api_url.trim_end_matches('/');
        let trimmed_path = path.trim_start_matches('/');
        format!("{trimmed_base}/{trimmed_path}")
    }

    fn attach_auth_headers(
        &self,
        request: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        let Some(auth) = &self.credentials else {
            return request;
        };

        request
            .header("POLYMARKET-API-KEY", auth.api_key.clone())
            .header("POLYMARKET-API-SECRET", auth.api_secret.clone())
            .header("POLYMARKET-PASSPHRASE", auth.api_passphrase.clone())
    }

    async fn decode_retry_after(response: &reqwest::Response) -> Option<u64> {
        let header = response.headers().get("retry-after")?;
        let value = header.to_str().ok()?.trim();
        if value.is_empty() {
            return None;
        }
        value.parse::<u64>().ok()
    }

    fn map_status_error(status: u16, retry_after_ms: Option<u64>, body: &Value) -> Option<ExecutionError> {
        if (200..300).contains(&status) {
            return None;
        }

        let message = body
            .get("message")
            .or_else(|| body.get("error"))
            .or_else(|| body.get("detail"))
            .and_then(Self::as_string)
            .or_else(|| {
                body.get("errors")
                    .and_then(Value::as_array)
                    .and_then(|errors| errors.first())
                    .and_then(Self::as_string)
            })
            .unwrap_or_else(|| format!("http status {status}"));

        match status {
            401 | 403 => Some(ExecutionError::AuthFailure(message)),
            400 | 422 => Some(ExecutionError::BadRequest(message)),
            408 | 425 | 429 => Some(ExecutionError::RateLimit { retry_after_ms }),
            500 | 502 | 503 | 504 => Some(ExecutionError::TransientNetwork(message)),
            409 => Some(ExecutionError::UncertainOutcome(message)),
            _ => Some(ExecutionError::VenueRejection(message)),
        }
    }

    fn as_string(value: &Value) -> Option<String> {
        match value {
            Value::String(raw) => Some(raw.clone()),
            _ => value.as_str().map(ToOwned::to_owned),
        }
    }
}

#[async_trait]
impl PolymarketHttpClient for ReqwestPolymarketClient {
    async fn call(
        &self,
        method: HttpMethod,
        path: &str,
        body: Option<&Value>,
    ) -> Result<HttpResponse, ExecutionError> {
        let url = self.build_url(path);
        let mut request = match method {
            HttpMethod::Get => self.client.get(url),
            HttpMethod::Post => self.client.post(url),
            HttpMethod::Delete => self.client.delete(url),
        };

        request = request.header("content-type", "application/json");
        if let Some(body) = body {
            request = request.json(body);
        }
        request = self.attach_auth_headers(request);

        let response = request
            .send()
            .await
            .map_err(|error| ExecutionError::TransientNetwork(error.to_string()))?;

        let status = response.status().as_u16();
        let retry_after_ms = Self::decode_retry_after(&response).await;
        let body = response
            .json::<Value>()
            .await
            .unwrap_or(Value::Null);

        if let Some(error) = Self::map_status_error(status, retry_after_ms, &body) {
            return Err(error);
        }

        Ok(HttpResponse {
            status,
            body,
            retry_after_ms,
        })
    }
}

pub struct PaperExecutionAdapter;

impl PaperExecutionAdapter {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ExecutionAdapter for PaperExecutionAdapter {
    async fn submit(&self, req: SubmitOrderRequest) -> Result<SubmitOrderAck, ExecutionError> {
        Ok(SubmitOrderAck {
            client_order_id: req.client_order_id,
            venue_order_id: None,
            accepted: true,
            accepted_at_ms: req.submitted_at_ms,
            venue_message: None,
        })
    }

    async fn cancel(&self, req: CancelOrderRequest) -> Result<CancelOrderAck, ExecutionError> {
        Ok(CancelOrderAck {
            client_order_id: req.client_order_id,
            venue_order_id: req.venue_order_id,
            accepted: true,
            accepted_at_ms: req.submitted_at_ms,
            venue_message: Some("paper adapter cancel accepted".to_string()),
        })
    }

    async fn sync_open_orders(&self) -> Result<Vec<VenueOpenOrder>, ExecutionError> {
        Ok(Vec::new())
    }

    async fn sync_balances(&self) -> Result<VenueBalances, ExecutionError> {
        Ok(VenueBalances {
            cash_usd: 0.0,
            positions: Vec::new(),
            observed_at_ms: now_unix_ms(),
        })
    }
}

pub struct PolymarketExecutionAdapter {
    _config: PolymarketConfig,
    http_client: Arc<dyn PolymarketHttpClient + Send + Sync>,
    state: Arc<RwLock<AdapterState>>,
}

impl PolymarketExecutionAdapter {
    pub fn new(credentials: PolymarketCredentials) -> Self {
        Self::with_config(PolymarketConfig {
            api_url: "https://clob.polymarket.com".to_string(),
            credentials: Some(credentials),
        })
    }

    pub fn with_config(config: PolymarketConfig) -> Self {
        let http_client = ReqwestPolymarketClient {
            api_url: config.api_url.clone(),
            credentials: config.credentials.clone(),
            client: reqwest::Client::new(),
        };
        Self {
            _config: config,
            http_client: Arc::new(http_client),
            state: Arc::new(RwLock::new(AdapterState::default())),
        }
    }

    fn build_submit_payload(req: &SubmitOrderRequest) -> Value {
        json!({
            "client_order_id": req.client_order_id.as_str(),
            "market_id": req.market_id.as_str(),
            "instrument_id": req.instrument_id.as_str(),
            "side": match req.side {
                TradeSide::Buy => "BUY",
                TradeSide::Sell => "SELL",
            },
            "size": req.quantity,
            "price": req.limit_price,
            "post_only": req.post_only,
            "time_in_force": req.time_in_force.as_str(),
            "strategy_tag": req.strategy_tag,
            "quote_level_tag": req.quote_level_tag,
            "created_at_ms": req.submitted_at_ms,
        })
    }

    fn build_cancel_payload(
        req: &CancelOrderRequest,
        venue_order_id: Option<&OrderId>,
    ) -> (String, Value) {
        let order_id = venue_order_id
            .or_else(|| req.venue_order_id.as_ref())
            .map(ToString::to_string)
            .unwrap_or_else(|| req.client_order_id.as_str().to_string());

        let payload = json!({
            "client_order_id": req.client_order_id.as_str(),
            "order_id": order_id,
            "reason": req.reason.clone(),
            "ts": req.submitted_at_ms,
        });

        (order_id, payload)
    }

    fn parse_message(body: &Value) -> Option<String> {
        body.get("message")
            .or_else(|| body.get("error"))
            .or_else(|| body.get("reason"))
            .and_then(Self::as_string)
            .or_else(|| {
                body.get("errors")
                    .and_then(Value::as_array)
                    .and_then(|errors| errors.first())
                    .and_then(Self::as_string)
            })
    }

    fn parse_order_id(body: &Value) -> Option<OrderId> {
        body
            .get("id")
            .or_else(|| body.get("order_id"))
            .or_else(|| body.get("orderId"))
            .or_else(|| body.get("venue_order_id"))
            .and_then(Self::as_string)
            .map(OrderId::from)
    }

    fn parse_status(body: &Value) -> Option<String> {
        body.get("status")
            .or_else(|| body.get("result"))
            .or_else(|| body.get("state"))
            .and_then(Self::as_string)
    }

    fn parse_submit_ack_status(body: &Value, fallback: bool) -> bool {
        if let Some(status) = Self::parse_status(body).map(|value| value.to_ascii_lowercase()) {
            return match status.as_str() {
                "open" | "accepted" | "active" | "working" | "created" | "new" => true,
                "rejected" | "error" => false,
                _ => fallback,
            };
        }
        body.get("accepted").and_then(Value::as_bool).unwrap_or(fallback)
    }

    fn parse_cancel_ack_status(body: &Value, fallback: bool) -> bool {
        if let Some(status) = Self::parse_status(body).map(|value| value.to_ascii_lowercase()) {
            return match status.as_str() {
                "cancelled" | "canceled" | "success" | "complete" => true,
                "open" | "accepted" | "active" | "working" => false,
                "rejected" | "error" | "not_found" => false,
                _ => fallback,
            };
        }
        body.get("accepted").and_then(Value::as_bool).unwrap_or(fallback)
    }

    fn parse_u64(value: &Value) -> Option<u64> {
        match value {
            Value::Number(number) => {
                if let Some(value) = number.as_u64() {
                    Some(value)
                } else {
                    number
                        .as_f64()
                        .and_then(|value| {
                            if value.is_finite() && value >= 0.0 {
                                Some(value as u64)
                            } else {
                                None
                            }
                        })
                }
            }
            Value::String(raw) => raw.parse::<u64>().ok(),
            _ => None,
        }
    }

    fn parse_f64(value: &Value) -> Option<f64> {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|raw| raw.parse().ok()))
    }

    fn as_string(value: &Value) -> Option<String> {
        match value {
            Value::String(raw) => Some(raw.clone()),
            _ => value.as_str().map(ToOwned::to_owned),
        }
    }

    fn parse_trade_side(raw: &str) -> TradeSide {
        match raw.to_ascii_lowercase().as_str() {
            "buy" | "bid" => TradeSide::Buy,
            _ => TradeSide::Sell,
        }
    }

    fn parse_open_order_items(body: &Value) -> Option<&Vec<Value>> {
        body.get("data")
            .or_else(|| body.get("orders"))
            .and_then(Self::as_array)
            .or_else(|| body.as_array())
    }

    fn as_array(value: &Value) -> Option<&Vec<Value>> {
        value.as_array()
    }

    async fn sync_open_orders_from_client(&self) -> Result<Vec<VenueOpenOrder>, ExecutionError> {
        let response = self.http_client.call(HttpMethod::Get, "/v1/orders", None).await?;
        let Some(items) = Self::parse_open_order_items(&response.body) else {
            return Ok(Vec::new());
        };

        let mut orders = Vec::new();
        for item in items {
            let venue_order_id = Self::parse_order_id(item).map(|value| value.to_string());
            let Some(venue_order_id) = venue_order_id else {
                continue;
            };
            let market_id = item
                .get("market_id")
                .or_else(|| item.get("marketId"))
                .and_then(Self::as_string)
                .unwrap_or_default();
            let instrument_id = item
                .get("instrument_id")
                .or_else(|| item.get("asset_id"))
                .or_else(|| item.get("instrumentId"))
                .or_else(|| item.get("assetId"))
                .and_then(Self::as_string)
                .unwrap_or_default();
            let status = item.get("status").and_then(Self::as_string).unwrap_or_default();
            let status = status.to_ascii_lowercase();
            let relevant = matches!(
                status.as_str(),
                "open" | "working" | "active" | "new" | "created" | "accepted" | ""
            );
            if !relevant {
                continue;
            }

            orders.push(VenueOpenOrder {
                venue_order_id: OrderId::from(venue_order_id),
                client_order_id: item
                    .get("client_order_id")
                    .or_else(|| item.get("clientOrderId"))
                    .and_then(Self::as_string)
                    .map(ClientOrderId::from),
                market_id: MarketId::from(market_id),
                instrument_id: InstrumentId::from(instrument_id),
                side: item
                    .get("side")
                    .and_then(Self::as_string)
                    .map(|value| Self::parse_trade_side(&value))
                    .unwrap_or(TradeSide::Buy),
                limit_price: item
                    .get("price")
                    .or_else(|| item.get("limit_price"))
                    .and_then(Self::parse_f64)
                    .unwrap_or(0.0),
                original_qty: item
                    .get("size")
                    .or_else(|| item.get("original_qty"))
                    .and_then(Self::parse_f64)
                    .unwrap_or(0.0),
                remaining_qty: item
                    .get("remaining")
                    .or_else(|| item.get("remaining_qty"))
                    .and_then(Self::parse_f64)
                    .unwrap_or(0.0),
                created_at_ms: item
                    .get("created_at_ms")
                    .and_then(Self::parse_u64)
                    .unwrap_or_else(now_unix_ms),
            });
        }

        Ok(orders)
    }

    async fn sync_balances_from_client(&self) -> Result<VenueBalances, ExecutionError> {
        let response = self
            .http_client
            .call(HttpMethod::Get, "/v1/balances", None)
            .await?;
        let body = response.body;

        let cash_usd = body
            .get("cash_usd")
            .or_else(|| body.get("cash"))
            .or_else(|| body.get("balance"))
            .and_then(Self::parse_f64)
            .unwrap_or(0.0);

        let mut positions = Vec::new();
        if let Some(position_values) = body
            .get("positions")
            .or_else(|| body.get("assets"))
            .and_then(Self::as_array)
        {
            for position in position_values {
                let market_id = position
                    .get("market_id")
                    .or_else(|| position.get("marketId"))
                    .and_then(Self::as_string)
                    .unwrap_or_default();
                let instrument_id = position
                    .get("instrument_id")
                    .or_else(|| position.get("asset_id"))
                    .or_else(|| position.get("assetId"))
                    .and_then(Self::as_string)
                    .unwrap_or_default();
                let quantity = position
                    .get("quantity")
                    .or_else(|| position.get("size"))
                    .and_then(Self::parse_f64)
                    .unwrap_or(0.0);
                let average_cost_usd = position
                    .get("average_cost_usd")
                    .or_else(|| position.get("average_cost"))
                    .or_else(|| position.get("avg_price"))
                    .and_then(Self::parse_f64)
                    .unwrap_or(0.0);

                if market_id.is_empty() && instrument_id.is_empty() {
                    continue;
                }
                positions.push(VenuePosition {
                    market_id: MarketId::from(market_id),
                    instrument_id: InstrumentId::from(instrument_id),
                    quantity,
                    average_cost_usd,
                });
            }
        }

        Ok(VenueBalances {
            cash_usd,
            positions,
            observed_at_ms: body
                .get("observed_at_ms")
                .and_then(Self::parse_u64)
                .unwrap_or_else(now_unix_ms),
        })
    }
}

#[async_trait]
impl ExecutionAdapter for PolymarketExecutionAdapter {
    async fn submit(&self, req: SubmitOrderRequest) -> Result<SubmitOrderAck, ExecutionError> {
        if let Some(cached) = self.state.read().await.submitted_orders.get(&req.client_order_id).cloned()
        {
            return Ok(cached);
        }

        let response = self
            .http_client
            .call(HttpMethod::Post, "/v1/orders", Some(&Self::build_submit_payload(&req)))
            .await?;

        let accepted = Self::parse_submit_ack_status(&response.body, true);
        let ack = SubmitOrderAck {
            client_order_id: req.client_order_id.clone(),
            venue_order_id: Self::parse_order_id(&response.body),
            accepted,
            accepted_at_ms: response
                .body
                .get("accepted_at_ms")
                .and_then(Self::parse_u64)
                .unwrap_or_else(now_unix_ms),
            venue_message: Self::parse_message(&response.body),
        };

        if accepted {
            let mut state = self.state.write().await;
            state.submitted_orders.insert(req.client_order_id.clone(), ack.clone());
            if let Some(order_id) = ack.venue_order_id.clone() {
                state.venue_order_map.insert(req.client_order_id.clone(), order_id);
            }
        }

        Ok(ack)
    }

    async fn cancel(&self, req: CancelOrderRequest) -> Result<CancelOrderAck, ExecutionError> {
        {
            let state = self.state.read().await;
            if state.canceled_orders.contains(&req.client_order_id) {
                return Ok(CancelOrderAck {
                    client_order_id: req.client_order_id,
                    venue_order_id: req.venue_order_id,
                    accepted: true,
                    accepted_at_ms: now_unix_ms(),
                    venue_message: Some("idempotent cancel replay".to_string()),
                });
            }
        }

        let state = self.state.read().await;
        let venue_order_id = req
            .venue_order_id
            .clone()
            .or_else(|| state.venue_order_map.get(&req.client_order_id).cloned());
        drop(state);

        let (order_id, payload) = Self::build_cancel_payload(&req, venue_order_id.as_ref());
        let response = self
            .http_client
            .call(HttpMethod::Delete, &format!("/v1/orders/{order_id}"), Some(&payload))
            .await?;

        let accepted = Self::parse_cancel_ack_status(&response.body, true);
        let ack = CancelOrderAck {
            client_order_id: req.client_order_id.clone(),
            venue_order_id: venue_order_id.or_else(|| Self::parse_order_id(&response.body)),
            accepted,
            accepted_at_ms: response
                .body
                .get("accepted_at_ms")
                .and_then(Self::parse_u64)
                .unwrap_or_else(now_unix_ms),
            venue_message: Self::parse_message(&response.body),
        };

        if accepted {
            let mut state = self.state.write().await;
            state.canceled_orders.insert(req.client_order_id.clone());
            state.submitted_orders.remove(&req.client_order_id);
            state.venue_order_map.remove(&req.client_order_id);
        }

        Ok(ack)
    }

    async fn sync_open_orders(&self) -> Result<Vec<VenueOpenOrder>, ExecutionError> {
        self.sync_open_orders_from_client().await
    }

    async fn sync_balances(&self) -> Result<VenueBalances, ExecutionError> {
        self.sync_balances_from_client().await
    }
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
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct MockHttpClient {
        responses: Arc<Mutex<VecDeque<Result<HttpResponse, ExecutionError>>>>,
        calls: Arc<Mutex<Vec<(HttpMethod, String, Option<Value>)>>>,
    }

    impl MockHttpClient {
        fn new(responses: Vec<Result<HttpResponse, ExecutionError>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(VecDeque::from(responses))),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn success_submit() -> Self {
            let response = HttpResponse {
                status: 200,
                body: json!({
                    "id": "venue-1",
                    "status": "open",
                    "accepted": true,
                    "message": "accepted",
                    "accepted_at_ms": now_unix_ms()
                }),
                retry_after_ms: None,
            };
            Self::new(vec![Ok(response)])
        }

        fn success_cancel() -> Self {
            let response = HttpResponse {
                status: 200,
                body: json!({
                    "status": "cancelled",
                    "accepted": true,
                    "message": "cancelled",
                    "accepted_at_ms": now_unix_ms()
                }),
                retry_after_ms: None,
            };
            Self::new(vec![Ok(response)])
        }
    }

    #[async_trait]
    impl PolymarketHttpClient for MockHttpClient {
        async fn call(
            &self,
            method: HttpMethod,
            path: &str,
            body: Option<&Value>,
        ) -> Result<HttpResponse, ExecutionError> {
            self.calls
                .lock()
                .unwrap()
                .push((method, path.to_string(), body.cloned()));

            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(ExecutionError::TransientNetwork("no response queued".to_string())))
        }
    }

    #[tokio::test]
    async fn submit_and_cancel_path_is_idempotent() {
        let shared_client = Arc::new({
            let mut responses = VecDeque::new();
            responses.push_back(Ok(HttpResponse {
                status: 200,
                body: json!({
                    "id": "venue-1",
                    "status": "open",
                    "accepted": true,
                    "message": "accepted",
                    "accepted_at_ms": now_unix_ms()
                }),
                retry_after_ms: None,
            }));
            responses.push_back(Ok(HttpResponse {
                status: 200,
                body: json!({
                    "status": "cancelled",
                    "accepted": true,
                    "message": "cancelled",
                    "accepted_at_ms": now_unix_ms()
                }),
                retry_after_ms: None,
            }));
            MockHttpClient {
                responses: Arc::new(Mutex::new(responses)),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        });

        let adapter = PolymarketExecutionAdapter {
            _config: PolymarketConfig::default(),
            http_client: shared_client.clone(),
            state: Arc::new(RwLock::new(AdapterState::default())),
        };

        let req = SubmitOrderRequest {
            client_order_id: ClientOrderId::from("client-1"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("asset-1"),
            side: TradeSide::Buy,
            limit_price: 0.5,
            quantity: 1.0,
            post_only: true,
            time_in_force: TimeInForce::Gtc,
            strategy_tag: "strategy-a".to_string(),
            quote_level_tag: None,
            submitted_at_ms: now_unix_ms(),
        };

        let first = adapter.submit(req.clone()).await.expect("submit ok");
        let second = adapter.submit(req.clone()).await.expect("submit replay ok");
        assert_eq!(first, second);
        assert_eq!(first.venue_order_id, Some(OrderId::from("venue-1")));

        let cancel = adapter
            .cancel(CancelOrderRequest {
                client_order_id: ClientOrderId::from("client-1"),
                venue_order_id: first.venue_order_id,
                reason: "test cancel".to_string(),
                submitted_at_ms: now_unix_ms(),
            })
            .await
            .expect("cancel ok");
        assert!(cancel.accepted);

        let second_cancel = adapter
            .cancel(CancelOrderRequest {
                client_order_id: ClientOrderId::from("client-1"),
                venue_order_id: None,
                reason: "test cancel replay".to_string(),
                submitted_at_ms: now_unix_ms(),
            })
            .await
            .expect("cancel replay ok");
        assert!(second_cancel.accepted);

        let call_count = shared_client
            .calls
            .lock()
            .unwrap()
            .len();
        assert_eq!(call_count, 2);
    }
}
