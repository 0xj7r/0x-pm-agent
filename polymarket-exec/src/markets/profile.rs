//! Config-friendly market profile activation.

use serde::{Deserialize, Serialize};

use crate::markets::descriptor::{BinaryOutcomeMarket, MarketTenor, UnderlyingAsset};
use crate::markets::registry::MarketRegistry;
use crate::types::{InstrumentId, MarketId};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MarketProfile {
    pub market_id: String,
    pub yes_instrument_id: String,
    pub no_instrument_id: String,
    pub underlying: String,
    pub tenor_minutes: u16,
    #[serde(default = "default_tick_size")]
    pub tick_size: f64,
    #[serde(default = "default_min_order_size")]
    pub min_order_size: f64,
}

impl MarketProfile {
    pub fn to_market(&self) -> BinaryOutcomeMarket {
        let underlying = match self.underlying.to_ascii_uppercase().as_str() {
            "BTC" => UnderlyingAsset::Btc,
            "ETH" => UnderlyingAsset::Eth,
            "SOL" => UnderlyingAsset::Sol,
            other => UnderlyingAsset::Other(other.to_string()),
        };
        let mut market = BinaryOutcomeMarket::timed_binary(
            MarketId::from(self.market_id.clone()),
            InstrumentId::from(self.yes_instrument_id.clone()),
            InstrumentId::from(self.no_instrument_id.clone()),
            underlying,
            MarketTenor::Minutes(self.tenor_minutes),
        );
        market.tick_size = self.tick_size;
        market.min_order_size = self.min_order_size;
        market
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MarketProfileRegistry {
    #[serde(default)]
    pub markets: Vec<MarketProfile>,
}

impl MarketProfileRegistry {
    pub fn to_registry(&self) -> MarketRegistry {
        let mut registry = MarketRegistry::new();
        for profile in &self.markets {
            registry.insert(profile.to_market());
        }
        registry
    }
}

fn default_tick_size() -> f64 {
    0.01
}

fn default_min_order_size() -> f64 {
    5.0
}
