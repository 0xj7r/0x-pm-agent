//! Shared domain types for orders, fills, quotes, and runtime commands.

use std::fmt;

use serde::Serialize;

pub type EpochMillis = u64;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(MarketId);
id_type!(InstrumentId);
id_type!(ClientOrderId);
id_type!(OrderId);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum RuntimeStatus {
    #[default]
    Starting,
    Running,
    Degraded,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum TradeSide {
    Buy,
    Sell,
}

impl TradeSide {
    pub fn sign(self) -> f64 {
        match self {
            Self::Buy => 1.0,
            Self::Sell => -1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum FillLiquidity {
    Maker,
    Taker,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum CloseMethod {
    #[default]
    Unknown,
    Merge,
    Redeem,
    Sell,
    Settle,
    Settlement,
}

impl CloseMethod {
    pub fn from_raw(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "merge" | "mergepositions" => Self::Merge,
            "redeem" | "redeempositions" => Self::Redeem,
            "sell" => Self::Sell,
            "settle" | "settlement" => Self::Settlement,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Merge => "merge",
            Self::Redeem => "redeem",
            Self::Sell => "sell",
            Self::Settle | Self::Settlement => "settlement",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BookLevel {
    pub price: f64,
    pub quantity: f64,
}

impl BookLevel {
    pub fn new(price: f64, quantity: f64) -> Self {
        Self { price, quantity }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct QuoteSnapshot {
    pub best_bid: Option<BookLevel>,
    pub best_ask: Option<BookLevel>,
    pub bid_levels: Vec<BookLevel>,
    pub ask_levels: Vec<BookLevel>,
    pub depth_observed_at_ms: Option<EpochMillis>,
    pub last_trade_price: Option<f64>,
    pub observed_at_ms: EpochMillis,
}

impl QuoteSnapshot {
    pub fn mid_price(&self) -> Option<f64> {
        match (&self.best_bid, &self.best_ask) {
            (Some(bid), Some(ask)) => Some((bid.price + ask.price) * 0.5),
            _ => self.last_trade_price,
        }
    }

    pub fn spread(&self) -> Option<f64> {
        match (&self.best_bid, &self.best_ask) {
            (Some(bid), Some(ask)) => Some(ask.price - bid.price),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MarketSnapshot {
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub quote: QuoteSnapshot,
}

impl MarketSnapshot {
    pub fn mark_price(&self) -> Option<f64> {
        self.quote
            .mid_price()
            .or(self.quote.best_ask.as_ref().map(|level| level.price))
            .or(self.quote.best_bid.as_ref().map(|level| level.price))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OrderIntent {
    pub client_order_id: ClientOrderId,
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub side: TradeSide,
    pub limit_price: f64,
    pub quantity: f64,
    pub reduce_only: bool,
    pub reason: String,
    pub quote_level_tag: Option<String>,
    pub created_at_ms: EpochMillis,
}

impl OrderIntent {
    pub fn notional_usd(&self) -> f64 {
        self.limit_price * self.quantity
    }

    pub fn signed_notional_usd(&self) -> f64 {
        self.notional_usd() * self.side.sign()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MergeIntent {
    pub command_id: ClientOrderId,
    pub market_id: MarketId,
    pub condition_id: Option<String>,
    pub yes_instrument_id: InstrumentId,
    pub no_instrument_id: InstrumentId,
    pub quantity: f64,
    pub expected_cash_usd: f64,
    pub expected_cost_usd: f64,
    pub expected_fee_usd: f64,
    pub expected_gas_usd: f64,
    pub reason: String,
    pub created_at_ms: EpochMillis,
}

impl MergeIntent {
    pub fn expected_net_gain_usd(&self) -> f64 {
        self.expected_cash_usd
            - self.expected_cost_usd
            - self.expected_fee_usd
            - self.expected_gas_usd
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RedeemIntent {
    pub command_id: ClientOrderId,
    pub market_id: MarketId,
    pub condition_id: Option<String>,
    pub reason: String,
    pub created_at_ms: EpochMillis,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FillReport {
    pub order_id: Option<OrderId>,
    pub client_order_id: Option<ClientOrderId>,
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub side: TradeSide,
    pub price: f64,
    pub quantity: f64,
    pub fee_usd: f64,
    pub liquidity: FillLiquidity,
    pub close_method: Option<CloseMethod>,
    pub observed_at_ms: EpochMillis,
}

impl FillReport {
    pub fn notional_usd(&self) -> f64 {
        self.price * self.quantity
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum RuntimeCommand {
    Submit(OrderIntent),
    Cancel {
        client_order_id: ClientOrderId,
        reason: String,
    },
    Merge(MergeIntent),
    Redeem(RedeemIntent),
    Noop,
}
