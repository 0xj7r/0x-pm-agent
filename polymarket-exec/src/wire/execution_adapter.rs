//! Live CLOB execution adapter contract (Section 8.1).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;
use polymarket_client_sdk::auth::{Credentials as SdkCredentials, ExposeSecret, Signer, Uuid};
use polymarket_client_sdk::clob::types::request::{
    BalanceAllowanceRequest, OrdersRequest, TradesRequest as ClobTradesRequest,
};
use polymarket_client_sdk::clob::types::{
    OrderStatusType, OrderType as SdkOrderType, Side as SdkSide, SignatureType as SdkSignatureType,
    TraderSide,
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
    async fn sync_recent_fills(
        &self,
        after_ms: EpochMillis,
    ) -> Result<Vec<VenueFill>, ExecutionError>;
}

#[derive(Default)]
struct AdapterState {
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

    async fn sync_recent_fills(
        &self,
        _after_ms: EpochMillis,
    ) -> Result<Vec<VenueFill>, ExecutionError> {
        Ok(Vec::new())
    }
}

pub struct PolymarketExecutionAdapter {
    _config: PolymarketConfig,
    signer: PrivateKeySigner,
    client: SdkClobClient<auth::state::Authenticated<auth::Normal>>,
    trade_address: Option<SdkAddress>,
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
        Self::connect_with_l1_url("https://clob.polymarket.com", credentials).await
    }

    pub async fn connect_with_l1_url(
        api_url: impl Into<String>,
        credentials: PolymarketL1Credentials,
    ) -> Result<Self, ExecutionError> {
        let api_url = api_url.into();
        let signer = PrivateKeySigner::from_str(credentials.private_key.trim())
            .map_err(|error| {
                ExecutionError::AuthFailure(format!("invalid POLYMARKET_PRIVATE_KEY: {error}"))
            })?
            .with_chain_id(Some(POLYGON));

        let mut auth_builder = SdkClobClient::new(
            api_url.as_str(),
            SdkClobConfig::builder().use_server_time(true).build(),
        )
        .map_err(map_sdk_error)?
        .authentication_builder(&signer)
        .signature_type(credentials.signature_type.as_sdk());

        let mut trade_address = Some(signer.address());
        if let Some(funder) = credentials
            .funder_address
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            let funder = SdkAddress::from_str(funder).map_err(|error| {
                ExecutionError::AuthFailure(format!("invalid POLYMARKET_FUNDER_ADDRESS: {error}"))
            })?;
            trade_address = Some(funder);
            auth_builder = auth_builder.funder(funder);
        }

        let client = auth_builder.authenticate().await.map_err(map_sdk_error)?;

        Ok(Self {
            _config: PolymarketConfig {
                api_url,
                credentials: None,
            },
            signer,
            client,
            trade_address,
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

        let mut trade_address = Some(signer.address());
        if let Some(funder) = credentials
            .funder_address
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            let funder = SdkAddress::from_str(funder).map_err(|error| {
                ExecutionError::AuthFailure(format!("invalid POLYMARKET_FUNDER_ADDRESS: {error}"))
            })?;
            trade_address = Some(funder);
            auth_builder = auth_builder.funder(funder);
        }

        let client = auth_builder.authenticate().await.map_err(map_sdk_error)?;

        Ok(Self {
            _config: config,
            signer,
            client,
            trade_address,
            state: Arc::new(RwLock::new(AdapterState::default())),
        })
    }

    pub fn api_credentials(&self) -> (String, String, String) {
        let credentials = self.client.credentials();
        (
            credentials.key().to_string(),
            credentials.secret().expose_secret().to_string(),
            credentials.passphrase().expose_secret().to_string(),
        )
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

    fn venue_gtd_expiration_ms(intended_expires_at_ms: EpochMillis) -> EpochMillis {
        // Polymarket requires the submitted GTD expiration to include a one
        // minute security threshold. The executor still cancels locally at the
        // intended max age; this only satisfies the venue's signing contract.
        intended_expires_at_ms.saturating_add(60_000)
    }

    fn usdc_balance_to_usd(raw: &str) -> f64 {
        let trimmed = raw.trim();
        let value = trimmed.parse::<f64>().unwrap_or(0.0);
        if trimmed.contains('.') {
            value
        } else {
            value / 1_000_000.0
        }
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
        let raw_balance = response.balance.to_string();
        Ok(VenueBalances {
            cash_usd: Self::usdc_balance_to_usd(&raw_balance),
            positions: Vec::new(),
            observed_at_ms: now_unix_ms(),
        })
    }

    async fn sync_recent_fills_from_client(
        &self,
        after_ms: EpochMillis,
    ) -> Result<Vec<VenueFill>, ExecutionError> {
        let Some(trade_address) = self.trade_address else {
            return Ok(Vec::new());
        };
        let mut fills = Vec::new();

        let maker_request = ClobTradesRequest::builder()
            .maker_address(trade_address)
            .after((after_ms / 1_000) as i64)
            .build();
        let maker_page = self
            .client
            .trades(&maker_request, None)
            .await
            .map_err(map_sdk_error)?;
        for trade in maker_page.data {
            let observed_at_ms = trade.match_time.timestamp_millis().max(0) as u64;
            let market_id = MarketId::from(format!("{:#x}", trade.market));
            for maker in trade
                .maker_orders
                .into_iter()
                .filter(|maker| maker.maker_address == trade_address)
            {
                fills.push(VenueFill {
                    venue_order_id: OrderId::from(maker.order_id),
                    client_order_id: None,
                    market_id: market_id.clone(),
                    instrument_id: InstrumentId::from(maker.asset_id.to_string()),
                    side: match maker.side {
                        SdkSide::Buy => TradeSide::Buy,
                        _ => TradeSide::Sell,
                    },
                    price: maker.price.to_string().parse::<f64>().unwrap_or(0.0),
                    quantity: maker
                        .matched_amount
                        .to_string()
                        .parse::<f64>()
                        .unwrap_or(0.0),
                    fee_usd: 0.0,
                    liquidity: FillLiquidity::Maker,
                    observed_at_ms,
                });
            }
        }

        let taker_request = ClobTradesRequest::builder()
            .taker_address(trade_address)
            .after((after_ms / 1_000) as i64)
            .build();
        let taker_page = self
            .client
            .trades(&taker_request, None)
            .await
            .map_err(map_sdk_error)?;
        for trade in taker_page.data {
            let observed_at_ms = trade.match_time.timestamp_millis().max(0) as u64;
            fills.push(VenueFill {
                venue_order_id: OrderId::from(trade.taker_order_id),
                client_order_id: None,
                market_id: MarketId::from(format!("{:#x}", trade.market)),
                instrument_id: InstrumentId::from(trade.asset_id.to_string()),
                side: match trade.side {
                    SdkSide::Buy => TradeSide::Buy,
                    _ => TradeSide::Sell,
                },
                price: trade.price.to_string().parse::<f64>().unwrap_or(0.0),
                quantity: trade.size.to_string().parse::<f64>().unwrap_or(0.0),
                fee_usd: 0.0,
                liquidity: match trade.trader_side {
                    TraderSide::Maker => FillLiquidity::Maker,
                    TraderSide::Taker => FillLiquidity::Taker,
                    TraderSide::Unknown(_) | _ => FillLiquidity::Unknown,
                },
                observed_at_ms,
            });
        }

        let mut seen = HashSet::new();
        fills.retain(|fill| {
            seen.insert(format!(
                "{}:{}:{}:{:?}:{}:{}",
                fill.venue_order_id,
                fill.market_id,
                fill.instrument_id,
                fill.side,
                fill.price,
                fill.quantity
            ))
        });
        Ok(fills)
    }
}

#[async_trait]
impl ExecutionAdapter for PolymarketExecutionAdapter {
    async fn submit(&self, req: SubmitOrderRequest) -> Result<SubmitOrderAck, ExecutionError> {
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
            let venue_expires_at_ms = Self::venue_gtd_expiration_ms(expires_at_ms);
            let expires_at = SdkDateTime::<SdkUtc>::from_timestamp(
                (venue_expires_at_ms / 1_000) as i64,
                ((venue_expires_at_ms % 1_000) * 1_000_000) as u32,
            )
            .ok_or_else(|| {
                ExecutionError::BadRequest(format!(
                    "invalid GTD expiration timestamp {venue_expires_at_ms}"
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

    async fn sync_recent_fills(
        &self,
        after_ms: EpochMillis,
    ) -> Result<Vec<VenueFill>, ExecutionError> {
        self.sync_recent_fills_from_client(after_ms).await
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
    if lower.contains("400")
        || lower.contains("422")
        || lower.contains("validation")
        || lower.contains("lower than the minimum")
        || lower.contains(" is invalid")
    {
        ExecutionError::BadRequest(message)
    } else if lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
    {
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

    #[test]
    fn gtd_expiration_includes_polymarket_security_threshold() {
        assert_eq!(
            PolymarketExecutionAdapter::venue_gtd_expiration_ms(1_000),
            61_000
        );
    }

    #[test]
    fn usdc_balance_parser_handles_base_units_and_decimal_units() {
        assert!(
            (PolymarketExecutionAdapter::usdc_balance_to_usd("106484140") - 106.48414).abs() < 1e-9
        );
        assert!(
            (PolymarketExecutionAdapter::usdc_balance_to_usd("106.48414") - 106.48414).abs() < 1e-9
        );
    }

    #[test]
    fn sdk_validation_errors_are_not_auth_failures() {
        let error = polymarket_client_sdk::error::Error::status(
            polymarket_client_sdk::error::StatusCode::BAD_REQUEST,
            polymarket_client_sdk::error::Method::POST,
            "/order".to_string(),
            "{\"error\":\"order 0xabc is invalid. Size (1.64) lower than the minimum: 5\"}",
        );

        assert!(matches!(
            map_sdk_error(error),
            ExecutionError::BadRequest(_)
        ));
    }
}
