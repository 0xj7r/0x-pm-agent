use std::fmt;

use async_trait::async_trait;

use crate::types::{
    ClientOrderId, EpochMillis, FillLiquidity, InstrumentId, MarketId, OrderId, TradeSide,
};
use crate::wire::eoa_polygon::PusdWrapReport;

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
    /// Shares filled (venue `makingAmount` on matched BUY).
    pub filled_qty: Option<f64>,
    /// Volume-weighted fill price in [0, 1] (USDC / share).
    pub avg_fill_price: Option<f64>,
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
pub struct MergePositionsRequest {
    pub command_id: ClientOrderId,
    pub market_id: MarketId,
    pub condition_id: Option<String>,
    pub yes_instrument_id: InstrumentId,
    pub no_instrument_id: InstrumentId,
    pub quantity: f64,
    pub submitted_at_ms: EpochMillis,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MergePositionsAck {
    pub command_id: ClientOrderId,
    pub accepted: bool,
    pub accepted_at_ms: EpochMillis,
    pub venue_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RedeemPositionsRequest {
    pub command_id: ClientOrderId,
    pub market_id: MarketId,
    pub condition_id: String,
    /// Optional collateral override for legacy positions. Live trading can use
    /// pUSD while pre-cutover positions may still redeem against USDC.e.
    pub collateral_token_address: Option<String>,
    /// CTF index sets to redeem. For binary markets pass `vec![1, 2]`
    /// to claim both legs (winning leg pays, losing leg returns nothing
    /// but the call still succeeds atomically).
    pub index_sets: Vec<u64>,
    pub submitted_at_ms: EpochMillis,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RedeemPositionsAck {
    pub command_id: ClientOrderId,
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
    pub positions_authoritative: bool,
    pub observed_at_ms: EpochMillis,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VenuePosition {
    pub market_id: MarketId,
    pub condition_id: Option<String>,
    pub instrument_id: InstrumentId,
    pub quantity: f64,
    pub average_cost_usd: f64,
    /// True when the underlying market has resolved and the position
    /// can be redeemed for collateral (winning side pays $1/share,
    /// losing side returns 0). Set from the Polymarket Data API
    /// `redeemable` flag; defaults to false in synthetic constructions.
    pub redeemable: bool,
    /// True when the holder also has the opposite-outcome position in
    /// matching size, allowing a CTF merge to recover collateral
    /// without waiting for resolution.
    pub mergeable: bool,
    /// Current market value of the position (for ranking which to
    /// redeem first). $0 doesn't mean unredeemable — losing-side legs
    /// of resolved markets still need to be redeemed to clear inventory.
    pub current_value_usd: f64,
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
pub struct MarketMetadata {
    pub condition_id: String,
    pub minimum_order_size: f64,
    pub minimum_tick_size: f64,
    pub neg_risk: bool,
    pub active: bool,
    pub closed: bool,
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
    async fn merge_positions(
        &self,
        req: MergePositionsRequest,
    ) -> Result<MergePositionsAck, ExecutionError> {
        Err(ExecutionError::BadRequest(format!(
            "merge positions not implemented for execution adapter command_id={}",
            req.command_id
        )))
    }
    async fn redeem_positions(
        &self,
        req: RedeemPositionsRequest,
    ) -> Result<RedeemPositionsAck, ExecutionError> {
        Err(ExecutionError::BadRequest(format!(
            "redeem positions not implemented for execution adapter command_id={}",
            req.command_id
        )))
    }
    async fn ensure_pusd_collateral_from_usdce(
        &self,
        _min_wrap_usd: f64,
    ) -> Result<Option<PusdWrapReport>, ExecutionError> {
        Ok(None)
    }
    /// Returns map of `venue_order_id` -> `is_scoring_for_rewards`.
    /// Default impl returns an empty map (paper / non-live adapters do
    /// not have rebate eligibility). Live adapters override to call the
    /// venue's /order-scoring endpoint.
    async fn check_orders_scoring(
        &self,
        _venue_order_ids: &[&str],
    ) -> Result<std::collections::HashMap<String, bool>, ExecutionError> {
        Ok(std::collections::HashMap::new())
    }
    async fn sync_open_orders(&self) -> Result<Vec<VenueOpenOrder>, ExecutionError>;
    async fn sync_balances(&self) -> Result<VenueBalances, ExecutionError>;
    async fn sync_recent_fills(
        &self,
        after_ms: EpochMillis,
    ) -> Result<Vec<VenueFill>, ExecutionError>;
    /// Fetch venue-authoritative market rules (minimum_order_size, tick).
    /// Default impl errors so the runtime can fall back to its config
    /// defaults. Live adapter overrides to call the venue API.
    async fn fetch_market_metadata(
        &self,
        condition_id: &str,
    ) -> Result<MarketMetadata, ExecutionError> {
        Err(ExecutionError::BadRequest(format!(
            "fetch_market_metadata not implemented on this adapter (condition_id={condition_id})"
        )))
    }
}
