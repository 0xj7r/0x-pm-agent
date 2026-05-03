//! Synthetic-gap detector.
//!
//! Per `event_schema_contract.md`, the collector emits a `gap` record
//! whenever the time between successive observations on a stream key
//! exceeds 15 seconds. Consumers use this to mark windows where data is
//! suspect.
//!
//! Keying is `(asset_id, event_type)` so that quiet markets do not
//! falsely trigger gaps just because *another* asset is loud.

use std::collections::HashMap;

use serde_json::json;

use super::schema::{Event, EventType, Source};

/// 15-second threshold expressed in nanoseconds.
pub const GAP_THRESHOLD_NS: i64 = 15 * 1_000_000_000;

/// Composite key for tracking last-seen times.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GapKey {
    pub asset_id: String,
    pub event_type: EventType,
}

/// Tracks last `received_ns` per stream key and emits synthetic gap
/// events when a new arrival is more than `GAP_THRESHOLD_NS` after the
/// previous one.
#[derive(Debug, Default)]
pub struct GapDetector {
    last_seen: HashMap<GapKey, i64>,
}

impl GapDetector {
    pub fn new() -> Self {
        Self {
            last_seen: HashMap::new(),
        }
    }

    /// Returns a synthetic `gap` `Event` if the interval since the last
    /// observation on this key exceeds the threshold; otherwise `None`.
    /// State is updated regardless of whether a gap is emitted.
    pub fn observe(&mut self, event: &Event) -> Option<Event> {
        let asset_id = match &event.asset_id {
            Some(id) => id.clone(),
            None => return None,
        };
        let key = GapKey {
            asset_id: asset_id.clone(),
            event_type: event.event_type,
        };
        let prev = self.last_seen.insert(key, event.received_ns);
        match prev {
            Some(prev_ns) if event.received_ns - prev_ns > GAP_THRESHOLD_NS => Some(Event {
                v: Event::SCHEMA_VERSION,
                ts_ns: event.received_ns,
                received_ns: event.received_ns,
                event_type: EventType::Gap,
                market_type: event.market_type.clone(),
                market_slug: event.market_slug.clone(),
                asset_id: Some(asset_id),
                side: None,
                price: None,
                size: None,
                sequence: None,
                source: Source::Collector,
                raw: json!({
                    "gap_ns": event.received_ns - prev_ns,
                    "previous_received_ns": prev_ns,
                    "trigger_event_type": super::partition::event_type_str(event.event_type),
                }),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(asset: Option<&str>, et: EventType, received_ns: i64) -> Event {
        Event {
            v: 1,
            ts_ns: received_ns,
            received_ns,
            event_type: et,
            market_type: "btc_5m".into(),
            market_slug: Some("slug".into()),
            asset_id: asset.map(str::to_string),
            side: None,
            price: None,
            size: None,
            sequence: None,
            source: Source::PolymarketMarketWs,
            raw: json!({}),
        }
    }

    #[test]
    fn first_observation_emits_no_gap() {
        let mut detector = GapDetector::new();
        let out = detector.observe(&ev(Some("a"), EventType::BookDelta, 1_000));
        assert!(out.is_none());
    }

    #[test]
    fn under_threshold_emits_no_gap() {
        let mut detector = GapDetector::new();
        detector.observe(&ev(Some("a"), EventType::BookDelta, 0));
        let out = detector.observe(&ev(
            Some("a"),
            EventType::BookDelta,
            10 * 1_000_000_000,
        ));
        assert!(out.is_none());
    }

    #[test]
    fn over_threshold_emits_gap_event() {
        let mut detector = GapDetector::new();
        detector.observe(&ev(Some("a"), EventType::BookDelta, 0));
        let out = detector
            .observe(&ev(
                Some("a"),
                EventType::BookDelta,
                16 * 1_000_000_000,
            ))
            .expect("gap");
        assert_eq!(out.event_type, EventType::Gap);
        assert_eq!(out.source, Source::Collector);
        assert_eq!(out.asset_id.as_deref(), Some("a"));
        assert_eq!(out.raw["gap_ns"], json!(16i64 * 1_000_000_000));
    }

    #[test]
    fn keys_are_independent_per_event_type() {
        let mut detector = GapDetector::new();
        detector.observe(&ev(Some("a"), EventType::BookDelta, 0));
        // A trade arriving 16s later on the SAME asset should not be
        // judged against the book_delta key.
        let out = detector.observe(&ev(
            Some("a"),
            EventType::Trade,
            16 * 1_000_000_000,
        ));
        assert!(out.is_none());
    }

    #[test]
    fn events_with_no_asset_id_are_skipped() {
        let mut detector = GapDetector::new();
        let out = detector.observe(&ev(None, EventType::BtcTick, 0));
        assert!(out.is_none());
    }
}
