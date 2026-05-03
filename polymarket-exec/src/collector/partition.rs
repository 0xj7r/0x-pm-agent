//! Partition-key derivation for Firehose dynamic partitioning.
//!
//! Mirrors the partition layout in the schema contract:
//! `raw/v=1/dt=YYYY-MM-DD/market_type=<m>/event_type=<e>/<slug-or-_none>/...`

use chrono::{TimeZone, Utc};

use super::schema::{Event, EventType};

/// Computed partition key for a single record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionKey {
    pub dt: String,
    pub market_type: String,
    pub event_type: String,
    pub slug: Option<String>,
}

/// Slug placeholder used when an event has no associated market.
pub const NO_SLUG: &str = "_none";

/// Compute the partition key for `event`. `dt` is derived from
/// `received_ns` in UTC.
pub fn partition_for(event: &Event) -> PartitionKey {
    PartitionKey {
        dt: format_dt(event.received_ns),
        market_type: event.market_type.clone(),
        event_type: event_type_str(event.event_type).to_string(),
        slug: event.market_slug.clone(),
    }
}

/// Snake-case label for an `EventType`. Identical to the serde tag we
/// emit on the wire; we reimplement it here so the partition derivation
/// is independent of any future serde rename.
pub fn event_type_str(et: EventType) -> &'static str {
    match et {
        EventType::BookDelta => "book_delta",
        EventType::BookSnapshot => "book_snapshot",
        EventType::Trade => "trade",
        EventType::UserFill => "user_fill",
        EventType::UserOrder => "user_order",
        EventType::BtcTick => "btc_tick",
        EventType::MarketMeta => "market_meta",
        EventType::Heartbeat => "heartbeat",
        EventType::Gap => "gap",
    }
}

fn format_dt(received_ns: i64) -> String {
    let secs = received_ns.div_euclid(1_000_000_000);
    let nsec = received_ns.rem_euclid(1_000_000_000) as u32;
    Utc.timestamp_opt(secs, nsec)
        .single()
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "1970-01-01".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::schema::Source;
    use serde_json::json;

    fn make_event(et: EventType, market_type: &str, slug: Option<&str>) -> Event {
        Event {
            v: 1,
            ts_ns: 0,
            received_ns: 1_714_579_200_124_111_000,
            event_type: et,
            market_type: market_type.into(),
            market_slug: slug.map(str::to_string),
            asset_id: None,
            side: None,
            price: None,
            size: None,
            sequence: None,
            source: Source::Collector,
            raw: json!({}),
        }
    }

    #[test]
    fn book_delta_partition() {
        let event = make_event(EventType::BookDelta, "btc_5m", Some("btc-up-or-down"));
        let key = partition_for(&event);
        assert_eq!(key.dt, "2024-05-01");
        assert_eq!(key.market_type, "btc_5m");
        assert_eq!(key.event_type, "book_delta");
        assert_eq!(key.slug.as_deref(), Some("btc-up-or-down"));
    }

    #[test]
    fn btc_tick_uses_global_market_type_no_slug() {
        let event = make_event(EventType::BtcTick, "_global", None);
        let key = partition_for(&event);
        assert_eq!(key.market_type, "_global");
        assert_eq!(key.event_type, "btc_tick");
        assert!(key.slug.is_none());
    }

    #[test]
    fn all_event_types_have_distinct_strings() {
        let names = [
            EventType::BookDelta,
            EventType::BookSnapshot,
            EventType::Trade,
            EventType::UserFill,
            EventType::UserOrder,
            EventType::BtcTick,
            EventType::MarketMeta,
            EventType::Heartbeat,
            EventType::Gap,
        ];
        let mut strs = names.iter().map(|et| event_type_str(*et)).collect::<Vec<_>>();
        strs.sort_unstable();
        let len_before = strs.len();
        strs.dedup();
        assert_eq!(strs.len(), len_before);
    }

    #[test]
    fn dt_format_uses_utc_date_from_received_ns() {
        // 2026-01-15T12:00:00Z = 1768521600 secs.
        let event = Event {
            v: 1,
            ts_ns: 0,
            received_ns: 1_768_521_600 * 1_000_000_000,
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
        assert_eq!(partition_for(&event).dt, "2026-01-16");
    }

    #[test]
    fn negative_received_ns_falls_back_to_epoch_minus_one_day() {
        let event = Event {
            v: 1,
            ts_ns: 0,
            received_ns: -1,
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
        // 1969-12-31 because rem_euclid keeps positive nanos.
        assert_eq!(partition_for(&event).dt, "1969-12-31");
    }
}
