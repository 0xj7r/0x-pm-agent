//! Market descriptors decouple "what we trade" from "how we trade it".
//!
//! Paired MM is the reusable strategy. BTC 5m, BTC 15m, ETH 5m, and future
//! timed binaries are market descriptors that provide token ids, tick rules,
//! oracle/strike context, and bar timing.

pub mod descriptor;
pub mod profile;
pub mod registry;

pub use descriptor::{
    BinaryOutcomeMarket, MarketDescriptor, MarketKind, MarketTenor, UnderlyingAsset,
};
pub use profile::{MarketProfile, MarketProfileRegistry};
pub use registry::MarketRegistry;
