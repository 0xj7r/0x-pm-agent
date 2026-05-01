//! Strongly typed market metadata consumed by market-agnostic strategies.

use crate::market_context::MarketContextRecord;
use crate::types::{EpochMillis, InstrumentId, MarketId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnderlyingAsset {
    Btc,
    Eth,
    Sol,
    Other(String),
}

impl UnderlyingAsset {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Btc => "BTC",
            Self::Eth => "ETH",
            Self::Sol => "SOL",
            Self::Other(value) => value.as_str(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarketTenor {
    Minutes(u16),
}

impl MarketTenor {
    pub fn window_ms(self) -> u64 {
        match self {
            Self::Minutes(minutes) => minutes as u64 * 60_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarketKind {
    UpDownBinary,
}

/// Minimal interface a strategy needs to trade a market.
pub trait MarketDescriptor {
    fn market_id(&self) -> &MarketId;
    fn yes_instrument_id(&self) -> &InstrumentId;
    fn no_instrument_id(&self) -> &InstrumentId;
    fn underlying(&self) -> &UnderlyingAsset;
    fn tenor(&self) -> MarketTenor;
    fn kind(&self) -> MarketKind;
    fn tick_size(&self) -> f64;
    fn min_order_size(&self) -> f64;
    fn price_to_beat(&self) -> Option<f64>;
    fn event_start_ms(&self) -> Option<EpochMillis>;
    fn event_end_ms(&self) -> Option<EpochMillis>;

    fn window_ms(&self) -> u64 {
        self.tenor().window_ms()
    }

    fn time_remaining_ms(&self, now_ms: EpochMillis) -> Option<u64> {
        self.event_end_ms()
            .map(|end_ms| end_ms.saturating_sub(now_ms))
    }

    fn time_remaining_fraction(&self, now_ms: EpochMillis) -> f64 {
        let Some(remaining_ms) = self.time_remaining_ms(now_ms) else {
            return 1.0;
        };
        let window_ms = self.window_ms().max(1);
        (remaining_ms as f64 / window_ms as f64).clamp(0.0, 1.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BinaryOutcomeMarket {
    pub market_id: MarketId,
    pub yes_instrument_id: InstrumentId,
    pub no_instrument_id: InstrumentId,
    pub underlying: UnderlyingAsset,
    pub tenor: MarketTenor,
    pub tick_size: f64,
    pub min_order_size: f64,
    pub price_to_beat: Option<f64>,
    pub event_start_ms: Option<EpochMillis>,
    pub event_end_ms: Option<EpochMillis>,
}

impl BinaryOutcomeMarket {
    pub fn btc_5m(
        market_id: MarketId,
        yes_instrument_id: InstrumentId,
        no_instrument_id: InstrumentId,
    ) -> Self {
        Self::timed_binary(
            market_id,
            yes_instrument_id,
            no_instrument_id,
            UnderlyingAsset::Btc,
            MarketTenor::Minutes(5),
        )
    }

    pub fn timed_binary(
        market_id: MarketId,
        yes_instrument_id: InstrumentId,
        no_instrument_id: InstrumentId,
        underlying: UnderlyingAsset,
        tenor: MarketTenor,
    ) -> Self {
        Self {
            market_id,
            yes_instrument_id,
            no_instrument_id,
            underlying,
            tenor,
            tick_size: 0.01,
            min_order_size: 5.0,
            price_to_beat: None,
            event_start_ms: None,
            event_end_ms: None,
        }
    }

    pub fn btc_15m(
        market_id: MarketId,
        yes_instrument_id: InstrumentId,
        no_instrument_id: InstrumentId,
    ) -> Self {
        Self::timed_binary(
            market_id,
            yes_instrument_id,
            no_instrument_id,
            UnderlyingAsset::Btc,
            MarketTenor::Minutes(15),
        )
    }

    pub fn eth_5m(
        market_id: MarketId,
        yes_instrument_id: InstrumentId,
        no_instrument_id: InstrumentId,
    ) -> Self {
        Self::timed_binary(
            market_id,
            yes_instrument_id,
            no_instrument_id,
            UnderlyingAsset::Eth,
            MarketTenor::Minutes(5),
        )
    }

    pub fn eth_15m(
        market_id: MarketId,
        yes_instrument_id: InstrumentId,
        no_instrument_id: InstrumentId,
    ) -> Self {
        Self::timed_binary(
            market_id,
            yes_instrument_id,
            no_instrument_id,
            UnderlyingAsset::Eth,
            MarketTenor::Minutes(15),
        )
    }

    pub fn with_context(mut self, context: &MarketContextRecord) -> Self {
        self.price_to_beat = context.price_to_beat;
        self.event_start_ms = context.event_start_time_ms;
        self.event_end_ms = context.event_end_time_ms;
        self
    }
}

impl MarketDescriptor for BinaryOutcomeMarket {
    fn market_id(&self) -> &MarketId {
        &self.market_id
    }

    fn yes_instrument_id(&self) -> &InstrumentId {
        &self.yes_instrument_id
    }

    fn no_instrument_id(&self) -> &InstrumentId {
        &self.no_instrument_id
    }

    fn underlying(&self) -> &UnderlyingAsset {
        &self.underlying
    }

    fn tenor(&self) -> MarketTenor {
        self.tenor
    }

    fn kind(&self) -> MarketKind {
        MarketKind::UpDownBinary
    }

    fn tick_size(&self) -> f64 {
        self.tick_size
    }

    fn min_order_size(&self) -> f64 {
        self.min_order_size
    }

    fn price_to_beat(&self) -> Option<f64> {
        self.price_to_beat
    }

    fn event_start_ms(&self) -> Option<EpochMillis> {
        self.event_start_ms
    }

    fn event_end_ms(&self) -> Option<EpochMillis> {
        self.event_end_ms
    }
}
