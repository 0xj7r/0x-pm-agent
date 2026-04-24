//! Live CLOB execution adapter contract (Section 8.1).

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;
use polymarket_client_sdk::auth::{Credentials as SdkCredentials, Signer, Uuid};
use polymarket_client_sdk::clob::types::request::{BalanceAllowanceRequest, OrdersRequest};
use polymarket_client_sdk::clob::types::{
    OrderStatusType, OrderType as SdkOrderType, Side as SdkSide, SignatureType as SdkSignatureType,
};
use polymarket_client_sdk::clob::{Client as SdkClobClient, Config as SdkClobConfig};
use polymarket_client_sdk::types::{
    Address as SdkAddress, DateTime as SdkDateTime, Decimal as SdkDecimal, Utc as SdkUtc,
    U256 as SdkU256,
};
use polymarket_client_sdk::{auth, POLYGON};
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
    pub expires_at_ms: Option<EpochMillis>,
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
    pub private_key: String,
    pub signature_type: PolymarketSignatureType,
    pub funder_address: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PolymarketL1Credentials {
    pub private_key: String,
    pub signature_type: PolymarketSignatureType,
    pub funder_address: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PolymarketSignatureType {
    Eoa,
    Proxy,
    #[default]
    GnosisSafe,
}

impl PolymarketSignatureType {
    pub fn parse(raw: &str) -> Result<Self, ExecutionError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "0" | "eoa" => Ok(Self::Eoa),
            "1" | "proxy" | "poly_proxy" | "poly-proxy" => Ok(Self::Proxy),
            "2" | "safe" | "gnosis" | "gnosis_safe" | "gnosis-safe" => Ok(Self::GnosisSafe),
            other => Err(ExecutionError::BadRequest(format!(
                "unsupported POLYMARKET_SIGNATURE_TYPE `{other}`"
            ))),
        }
    }

    fn as_sdk(self) -> SdkSignatureType {
        match self {
            Self::Eoa => SdkSignatureType::Eoa,
            Self::Proxy => SdkSignatureType::Proxy,
            Self::GnosisSafe => SdkSignatureType::GnosisSafe,
        }
    }
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
    RateLimit { retry_after_ms: Option<u64> },
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
    signer: PrivateKeySigner,
    client: SdkClobClient<auth::state::Authenticated<auth::Normal>>,
    state: Arc<RwLock<AdapterState>>,
}

impl PolymarketExecutionAdapter {
    pub async fn connect(credentials: PolymarketCredentials) -> Result<Self, ExecutionError> {
        Self::connect_with_config(PolymarketConfig {
            api_url: "https://clob.polymarket.com".to_string(),
            credentials: Some(credentials),
        })
        .await
    }

    pub async fn connect_with_l1(
        credentials: PolymarketL1Credentials,
    ) -> Result<Self, ExecutionError> {
        let signer = PrivateKeySigner::from_str(credentials.private_key.trim())
            .map_err(|error| {
                ExecutionError::AuthFailure(format!("invalid POLYMARKET_PRIVATE_KEY: {error}"))
            })?
            .with_chain_id(Some(POLYGON));

        let mut auth_builder = SdkClobClient::new(
            "https://clob.polymarket.com",
            SdkClobConfig::builder().use_server_time(true).build(),
        )
        .map_err(map_sdk_error)?
        .authentication_builder(&signer)
        .signature_type(credentials.signature_type.as_sdk());

        if let Some(funder) = credentials
            .funder_address
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            auth_builder = auth_builder.funder(SdkAddress::from_str(funder).map_err(|error| {
                ExecutionError::AuthFailure(format!("invalid POLYMARKET_FUNDER_ADDRESS: {error}"))
            })?);
        }

        let client = auth_builder.authenticate().await.map_err(map_sdk_error)?;

        Ok(Self {
            _config: PolymarketConfig::default(),
            signer,
            client,
            state: Arc::new(RwLock::new(AdapterState::default())),
        })
    }

    pub async fn connect_with_config(config: PolymarketConfig) -> Result<Self, ExecutionError> {
        let credentials = config.credentials.clone().ok_or_else(|| {
            ExecutionError::AuthFailure("missing Polymarket live credentials".to_string())
        })?;
        let api_key = Uuid::parse_str(credentials.api_key.trim()).map_err(|error| {
            ExecutionError::AuthFailure(format!("invalid POLYMARKET_API_KEY: {error}"))
        })?;
        let sdk_credentials = SdkCredentials::new(
            api_key,
            credentials.api_secret.clone(),
            credentials.api_passphrase.clone(),
        );
        let signer = PrivateKeySigner::from_str(credentials.private_key.trim())
            .map_err(|error| {
                ExecutionError::AuthFailure(format!("invalid POLYMARKET_PRIVATE_KEY: {error}"))
            })?
            .with_chain_id(Some(POLYGON));

        let mut auth_builder = SdkClobClient::new(
            config.api_url.as_str(),
            SdkClobConfig::builder().use_server_time(true).build(),
        )
        .map_err(map_sdk_error)?
        .authentication_builder(&signer)
        .credentials(sdk_credentials)
        .signature_type(credentials.signature_type.as_sdk());

        if let Some(funder) = credentials
            .funder_address
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            auth_builder = auth_builder.funder(SdkAddress::from_str(funder).map_err(|error| {
                ExecutionError::AuthFailure(format!("invalid POLYMARKET_FUNDER_ADDRESS: {error}"))
            })?);
        }

        let client = auth_builder.authenticate().await.map_err(map_sdk_error)?;

        Ok(Self {
            _config: config,
            signer,
            client,
            state: Arc::new(RwLock::new(AdapterState::default())),
        })
    }

    fn map_order_type(req: &SubmitOrderRequest) -> Result<SdkOrderType, ExecutionError> {
        match req.time_in_force {
            TimeInForce::Gtc => Ok(SdkOrderType::GTC),
            TimeInForce::Ioc => {
                if req.post_only {
                    Err(ExecutionError::BadRequest(
                        "post-only IOC/FAK orders are invalid on Polymarket".to_string(),
                    ))
                } else {
                    Ok(SdkOrderType::FAK)
                }
            }
            TimeInForce::Fok => {
                if req.post_only {
                    Err(ExecutionError::BadRequest(
                        "post-only FOK orders are invalid on Polymarket".to_string(),
                    ))
                } else {
                    Ok(SdkOrderType::FOK)
                }
            }
            TimeInForce::Gtd => Ok(SdkOrderType::GTD),
        }
    }

    fn sdk_side(side: TradeSide) -> SdkSide {
        match side {
            TradeSide::Buy => SdkSide::Buy,
            TradeSide::Sell => SdkSide::Sell,
        }
    }

    fn decimal_from_f64(
        value: f64,
        scale: usize,
        field: &str,
    ) -> Result<SdkDecimal, ExecutionError> {
        if !value.is_finite() || value <= 0.0 {
            return Err(ExecutionError::BadRequest(format!(
                "{field} must be positive and finite, got {value}"
            )));
        }
        format!("{value:.scale$}")
            .parse::<SdkDecimal>()
            .map_err(|error| ExecutionError::BadRequest(format!("invalid {field}: {error}")))
    }

    async fn sync_open_orders_from_client(&self) -> Result<Vec<VenueOpenOrder>, ExecutionError> {
        let page = self
            .client
            .orders(&OrdersRequest::default(), None)
            .await
            .map_err(map_sdk_error)?;
        let mut orders = Vec::new();
        for item in page.data {
            if !matches!(
                item.status,
                OrderStatusType::Live | OrderStatusType::Delayed
            ) {
                continue;
            }
            let original_qty = item.original_size.to_string().parse::<f64>().unwrap_or(0.0);
            let matched_qty = item.size_matched.to_string().parse::<f64>().unwrap_or(0.0);
            orders.push(VenueOpenOrder {
                venue_order_id: OrderId::from(item.id),
                client_order_id: None,
                market_id: MarketId::from(format!("{:#x}", item.market)),
                instrument_id: InstrumentId::from(item.asset_id.to_string()),
                side: match item.side {
                    SdkSide::Buy => TradeSide::Buy,
                    _ => TradeSide::Sell,
                },
                limit_price: item.price.to_string().parse::<f64>().unwrap_or(0.0),
                original_qty,
                remaining_qty: (original_qty - matched_qty).max(0.0),
                created_at_ms: item.created_at.timestamp_millis().max(0) as u64,
            });
        }

        Ok(orders)
    }

    async fn sync_balances_from_client(&self) -> Result<VenueBalances, ExecutionError> {
        let response = self
            .client
            .balance_allowance(BalanceAllowanceRequest::default())
            .await
            .map_err(map_sdk_error)?;
        Ok(VenueBalances {
            cash_usd: response.balance.to_string().parse::<f64>().unwrap_or(0.0),
            positions: Vec::new(),
            observed_at_ms: now_unix_ms(),
        })
    }
}

#[async_trait]
impl ExecutionAdapter for PolymarketExecutionAdapter {
    async fn submit(&self, req: SubmitOrderRequest) -> Result<SubmitOrderAck, ExecutionError> {
        if let Some(cached) = self
            .state
            .read()
            .await
            .submitted_orders
            .get(&req.client_order_id)
            .cloned()
        {
            return Ok(cached);
        }

        let token_id = SdkU256::from_str(req.instrument_id.as_str()).map_err(|error| {
            ExecutionError::BadRequest(format!(
                "invalid Polymarket token id `{}`: {error}",
                req.instrument_id
            ))
        })?;
        let order_type = Self::map_order_type(&req)?;
        let price = Self::decimal_from_f64(req.limit_price, 2, "limit_price")?;
        let size = Self::decimal_from_f64(req.quantity, 2, "quantity")?;
        let mut builder = self
            .client
            .limit_order()
            .token_id(token_id)
            .order_type(order_type)
            .post_only(req.post_only)
            .price(price)
            .size(size)
            .side(Self::sdk_side(req.side));
        if matches!(req.time_in_force, TimeInForce::Gtd) {
            let expires_at_ms = req.expires_at_ms.ok_or_else(|| {
                ExecutionError::BadRequest(
                    "GTD live order requires expires_at_ms on submit request".to_string(),
                )
            })?;
            let expires_at = SdkDateTime::<SdkUtc>::from_timestamp(
                (expires_at_ms / 1_000) as i64,
                ((expires_at_ms % 1_000) * 1_000_000) as u32,
            )
            .ok_or_else(|| {
                ExecutionError::BadRequest(format!(
                    "invalid GTD expiration timestamp {expires_at_ms}"
                ))
            })?;
            builder = builder.expiration(expires_at);
        }
        let order = builder.build().await.map_err(map_sdk_error)?;
        let signed_order = self
            .client
            .sign(&self.signer, order)
            .await
            .map_err(map_sdk_error)?;
        let response = self
            .client
            .post_order(signed_order)
            .await
            .map_err(map_sdk_error)?;

        let accepted = response.success;
        let ack = SubmitOrderAck {
            client_order_id: req.client_order_id.clone(),
            venue_order_id: if response.order_id.is_empty() {
                None
            } else {
                Some(OrderId::from(response.order_id.clone()))
            },
            accepted,
            accepted_at_ms: now_unix_ms(),
            venue_message: response.error_msg,
        };

        if accepted {
            let mut state = self.state.write().await;
            state
                .submitted_orders
                .insert(req.client_order_id.clone(), ack.clone());
            if let Some(order_id) = ack.venue_order_id.clone() {
                state
                    .venue_order_map
                    .insert(req.client_order_id.clone(), order_id);
            }
        }

        Ok(ack)
    }

    async fn cancel(&self, req: CancelOrderRequest) -> Result<CancelOrderAck, ExecutionError> {
        let state = self.state.read().await;
        let venue_order_id = req
            .venue_order_id
            .clone()
            .or_else(|| state.venue_order_map.get(&req.client_order_id).cloned());
        drop(state);

        let Some(order_id) = venue_order_id.clone() else {
            return Err(ExecutionError::BadRequest(format!(
                "cannot cancel {} without venue_order_id",
                req.client_order_id
            )));
        };
        let response = self
            .client
            .cancel_order(order_id.as_str())
            .await
            .map_err(map_sdk_error)?;

        let accepted = response
            .canceled
            .iter()
            .any(|canceled| canceled == order_id.as_str());
        let ack = CancelOrderAck {
            client_order_id: req.client_order_id.clone(),
            venue_order_id: Some(order_id.clone()),
            accepted,
            accepted_at_ms: now_unix_ms(),
            venue_message: response
                .not_canceled
                .get(order_id.as_str())
                .cloned()
                .or_else(|| Some("cancel submitted to Polymarket CLOB".to_string())),
        };

        if accepted {
            let mut state = self.state.write().await;
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

fn map_sdk_error(error: polymarket_client_sdk::error::Error) -> ExecutionError {
    let message = error.to_string();
    let lower = message.to_ascii_lowercase();
    if lower.contains("401") || lower.contains("403") || lower.contains("auth") {
        ExecutionError::AuthFailure(message)
    } else if lower.contains("429") || lower.contains("rate limit") {
        ExecutionError::RateLimit {
            retry_after_ms: None,
        }
    } else if lower.contains("timeout")
        || lower.contains("connection")
        || lower.contains("network")
        || lower.contains("502")
        || lower.contains("503")
        || lower.contains("504")
    {
        ExecutionError::TransientNetwork(message)
    } else if lower.contains("409") || lower.contains("uncertain") {
        ExecutionError::UncertainOutcome(message)
    } else if lower.contains("400") || lower.contains("422") || lower.contains("validation") {
        ExecutionError::BadRequest(message)
    } else {
        ExecutionError::VenueRejection(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submit_req(time_in_force: TimeInForce, post_only: bool) -> SubmitOrderRequest {
        SubmitOrderRequest {
            client_order_id: ClientOrderId::from("client-1"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("asset-1"),
            side: TradeSide::Buy,
            limit_price: 0.5,
            quantity: 1.0,
            post_only,
            time_in_force,
            expires_at_ms: None,
            strategy_tag: "strategy-a".to_string(),
            quote_level_tag: None,
            submitted_at_ms: now_unix_ms(),
        }
    }

    #[test]
    fn post_only_market_order_types_are_rejected_before_venue() {
        assert!(
            PolymarketExecutionAdapter::map_order_type(&submit_req(TimeInForce::Gtc, true)).is_ok()
        );
        assert!(
            PolymarketExecutionAdapter::map_order_type(&submit_req(TimeInForce::Ioc, false))
                .is_ok()
        );
        assert!(
            PolymarketExecutionAdapter::map_order_type(&submit_req(TimeInForce::Fok, false))
                .is_ok()
        );
        assert!(matches!(
            PolymarketExecutionAdapter::map_order_type(&submit_req(TimeInForce::Ioc, true)),
            Err(ExecutionError::BadRequest(_))
        ));
        assert!(matches!(
            PolymarketExecutionAdapter::map_order_type(&submit_req(TimeInForce::Fok, true)),
            Err(ExecutionError::BadRequest(_))
        ));
        assert!(
            PolymarketExecutionAdapter::map_order_type(&submit_req(TimeInForce::Gtd, true)).is_ok()
        );
    }

    #[test]
    fn signature_type_parser_accepts_polymarket_codes() {
        assert_eq!(
            PolymarketSignatureType::parse("0").unwrap(),
            PolymarketSignatureType::Eoa
        );
        assert_eq!(
            PolymarketSignatureType::parse("POLY_PROXY").unwrap(),
            PolymarketSignatureType::Proxy
        );
        assert_eq!(
            PolymarketSignatureType::parse("gnosis_safe").unwrap(),
            PolymarketSignatureType::GnosisSafe
        );
        assert!(PolymarketSignatureType::parse("bad").is_err());
    }

    #[test]
    fn live_price_decimal_matches_polymarket_cent_tick() {
        let price = PolymarketExecutionAdapter::decimal_from_f64(0.01, 2, "limit_price").unwrap();
        assert_eq!(price.to_string(), "0.01");
    }
}
