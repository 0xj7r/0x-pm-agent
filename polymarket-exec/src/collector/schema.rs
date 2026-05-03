//! Canonical event-record schema (v=1).
//!
//! Mirrors `docs/superpowers/specs/2026-05-02-event-schema-contract.md`.
//! Both producers (this crate) and consumers (Python preprocessing, Rust
//! replay) MUST honor this exactly. Adding or retyping a field requires a
//! `v=2` parallel pipeline, not an in-place edit.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Producer of an event. Snake-case matches the schema-contract `source` enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    PolymarketMarketWs,
    PolymarketUserWs,
    PolymarketDataApi,
    BinanceAggtrade,
    CoinbaseMatch,
    Collector,
    /// Emitted by the replay-side `EventSynthesizer` for derived events
    /// (Phase 3d-a: `price_to_beat` at window-open and `resolution` at
    /// window-close). Never produced by the live collector path.
    Synthesizer,
}

/// Canonical event-type enum. Snake-case matches the schema-contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    BookDelta,
    BookSnapshot,
    Trade,
    UserFill,
    UserOrder,
    BtcTick,
    MarketMeta,
    Heartbeat,
    Gap,
    /// Synthesized at the start of a window. Carries the BTC oracle price
    /// observed at window-open, the strike, and the asserted window
    /// timing. Phase 3d-a: emitted only by the replay synthesizer.
    PriceToBeat,
    /// Synthesized at the close of a window. Carries the resolved winning
    /// outcome and the oracle price at close. Phase 3d-a: emitted only by
    /// the replay synthesizer.
    Resolution,
}

/// Canonical record. Field order matches the contract for human readability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub v: u32,
    pub ts_ns: i64,
    pub received_ns: i64,
    pub event_type: EventType,
    pub market_type: String,
    pub market_slug: Option<String>,
    pub asset_id: Option<String>,
    pub side: Option<String>,
    pub price: Option<String>,
    pub size: Option<String>,
    pub sequence: Option<i64>,
    pub source: Source,
    pub raw: Value,
}

impl Event {
    /// Schema version emitted by this build.
    pub const SCHEMA_VERSION: u32 = 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_event() -> Event {
        Event {
            v: 1,
            ts_ns: 1_714_579_200_123_456_789,
            received_ns: 1_714_579_200_124_111_000,
            event_type: EventType::BookDelta,
            market_type: "btc_5m".into(),
            market_slug: Some("btc-up-or-down".into()),
            asset_id: Some("0xabc".into()),
            side: Some("BUY".into()),
            price: Some("0.5234".into()),
            size: Some("120.0".into()),
            sequence: Some(8_842_713),
            source: Source::PolymarketMarketWs,
            raw: json!({"hello": "world", "n": 42}),
        }
    }

    #[test]
    fn round_trip_serde_byte_equal() {
        let event = sample_event();
        let serialized = serde_json::to_string(&event).expect("serialize");
        let parsed: Event = serde_json::from_str(&serialized).expect("deserialize");
        assert_eq!(parsed, event);
        let reserialized = serde_json::to_string(&parsed).expect("reserialize");
        assert_eq!(reserialized, serialized);
    }

    #[test]
    fn event_type_snake_case_serialization() {
        assert_eq!(
            serde_json::to_string(&EventType::BookDelta).unwrap(),
            "\"book_delta\""
        );
        assert_eq!(
            serde_json::to_string(&EventType::BookSnapshot).unwrap(),
            "\"book_snapshot\""
        );
        assert_eq!(
            serde_json::to_string(&EventType::UserFill).unwrap(),
            "\"user_fill\""
        );
        assert_eq!(
            serde_json::to_string(&EventType::BtcTick).unwrap(),
            "\"btc_tick\""
        );
        assert_eq!(
            serde_json::to_string(&EventType::MarketMeta).unwrap(),
            "\"market_meta\""
        );
        assert_eq!(serde_json::to_string(&EventType::Gap).unwrap(), "\"gap\"");
        assert_eq!(
            serde_json::to_string(&EventType::PriceToBeat).unwrap(),
            "\"price_to_beat\""
        );
        assert_eq!(
            serde_json::to_string(&EventType::Resolution).unwrap(),
            "\"resolution\""
        );
    }

    #[test]
    fn source_snake_case_serialization() {
        assert_eq!(
            serde_json::to_string(&Source::PolymarketMarketWs).unwrap(),
            "\"polymarket_market_ws\""
        );
        assert_eq!(
            serde_json::to_string(&Source::BinanceAggtrade).unwrap(),
            "\"binance_aggtrade\""
        );
        assert_eq!(
            serde_json::to_string(&Source::Collector).unwrap(),
            "\"collector\""
        );
        assert_eq!(
            serde_json::to_string(&Source::Synthesizer).unwrap(),
            "\"synthesizer\""
        );
    }

    #[test]
    fn nullable_fields_serialize_as_null() {
        let event = Event {
            v: 1,
            ts_ns: 0,
            received_ns: 0,
            event_type: EventType::Heartbeat,
            market_type: "_global".into(),
            market_slug: None,
            asset_id: None,
            side: None,
            price: None,
            size: None,
            sequence: None,
            source: Source::Collector,
            raw: json!({}),
        };
        let serialized = serde_json::to_string(&event).expect("serialize");
        assert!(serialized.contains("\"market_slug\":null"));
        assert!(serialized.contains("\"asset_id\":null"));
        assert!(serialized.contains("\"price\":null"));
        assert!(serialized.contains("\"sequence\":null"));
    }

    #[test]
    fn schema_version_constant_matches_v1() {
        assert_eq!(Event::SCHEMA_VERSION, 1);
    }
}
