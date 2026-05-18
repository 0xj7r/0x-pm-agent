//! Replay input data-quality checks.
//!
//! These checks run before strategy replay. They do not try to prove a data
//! set is profitable or complete; they surface conditions that make PnL
//! interpretation unsafe, and reject windows with hard replay blockers.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::collector::partition::event_type_str;
use crate::collector::schema::{Event, EventType};

const NS_PER_SECOND: i64 = 1_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataQualityConfig {
    pub timestamp_drift_warn_ns: i64,
    pub timestamp_drift_reject_ns: i64,
    pub book_gap_warn_ns: i64,
    pub book_gap_reject_ns: i64,
    pub crossed_book_reject_samples: u64,
    pub crossed_book_reject_duration_ns: i64,
}

impl Default for DataQualityConfig {
    fn default() -> Self {
        Self {
            timestamp_drift_warn_ns: 2 * NS_PER_SECOND,
            timestamp_drift_reject_ns: 5 * 60 * NS_PER_SECOND,
            book_gap_warn_ns: 10 * NS_PER_SECOND,
            book_gap_reject_ns: 60 * NS_PER_SECOND,
            crossed_book_reject_samples: 100,
            crossed_book_reject_duration_ns: 250_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataQualityStatus {
    Pass,
    Warn,
    Reject,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataQualitySummary {
    pub window_id: String,
    pub status: DataQualityStatus,
    pub total_events: u64,
    pub event_type_counts: BTreeMap<String, u64>,
    pub source_counts: BTreeMap<String, u64>,
    pub first_received_ns: Option<i64>,
    pub last_received_ns: Option<i64>,
    pub max_timestamp_drift_ns: i64,
    pub timestamp_drift_warn_count: u64,
    pub timestamp_drift_reject_count: u64,
    pub non_monotonic_received_count: u64,
    pub duplicate_sequence_count: u64,
    pub invalid_price_count: u64,
    pub invalid_size_count: u64,
    pub book_event_count: u64,
    pub trade_event_count: u64,
    pub btc_tick_count: u64,
    pub market_meta_count: u64,
    pub resolution_event_count: u64,
    pub max_book_gap_ns: i64,
    pub missing_book_asset_ids: Vec<String>,
    pub crossed_book_samples: u64,
    pub crossed_book_max_duration_ns: i64,
    pub crossed_book_persistent_count: u64,
    pub warnings: Vec<String>,
    pub reject_reasons: Vec<String>,
}

impl Default for DataQualityStatus {
    fn default() -> Self {
        Self::Pass
    }
}

#[derive(Debug, Clone, Default)]
struct BookSide {
    levels: BTreeMap<i64, f64>,
}

impl BookSide {
    fn update(&mut self, price: f64, size: f64) {
        let tick = price_tick(price);
        if size <= 0.0 {
            self.levels.remove(&tick);
        } else {
            self.levels.insert(tick, size);
        }
    }

    fn best_bid(&self) -> Option<i64> {
        self.levels.keys().next_back().copied()
    }

    fn best_ask(&self) -> Option<i64> {
        self.levels.keys().next().copied()
    }

    fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
struct BookState {
    bids: BookSide,
    asks: BookSide,
    saw_snapshot: bool,
    crossed_since_ns: Option<i64>,
}

pub fn summarize_data_quality(
    window_id: &str,
    events: &[Event],
    cfg: &DataQualityConfig,
) -> DataQualitySummary {
    let mut summary = DataQualitySummary {
        window_id: window_id.to_string(),
        total_events: events.len() as u64,
        ..DataQualitySummary::default()
    };
    let mut last_received_ns = None;
    let mut last_book_ns = None;
    let mut books: BTreeMap<String, BookState> = BTreeMap::new();
    let mut expected_asset_ids = BTreeSet::new();
    let mut seen_sequence_keys = BTreeSet::new();

    for event in events {
        *summary
            .event_type_counts
            .entry(event_type_str(event.event_type).to_string())
            .or_insert(0) += 1;
        *summary
            .source_counts
            .entry(format!("{:?}", event.source))
            .or_insert(0) += 1;

        summary.first_received_ns = summary.first_received_ns.or(Some(event.received_ns));
        summary.last_received_ns = Some(event.received_ns);
        if let Some(prev) = last_received_ns {
            if event.received_ns < prev {
                summary.non_monotonic_received_count += 1;
            }
        }
        last_received_ns = Some(event.received_ns);

        let drift = event.received_ns.saturating_sub(event.ts_ns).abs();
        summary.max_timestamp_drift_ns = summary.max_timestamp_drift_ns.max(drift);
        if drift > cfg.timestamp_drift_warn_ns {
            summary.timestamp_drift_warn_count += 1;
        }
        if drift > cfg.timestamp_drift_reject_ns {
            summary.timestamp_drift_reject_count += 1;
        }

        let seq_key = (
            event_type_str(event.event_type),
            event.market_slug.clone(),
            event.asset_id.clone(),
            event.sequence,
        );
        if event.sequence.is_some() && !seen_sequence_keys.insert(seq_key) {
            summary.duplicate_sequence_count += 1;
        }

        match event.event_type {
            EventType::BookDelta | EventType::BookSnapshot => {
                summary.book_event_count += 1;
                if let Some(prev) = last_book_ns {
                    summary.max_book_gap_ns = summary
                        .max_book_gap_ns
                        .max(event.received_ns.saturating_sub(prev));
                }
                last_book_ns = Some(event.received_ns);

                let price = parse_non_negative_f64(event.price.as_deref());
                let size = parse_non_negative_f64(event.size.as_deref());
                if event.event_type == EventType::BookSnapshot
                    && price.is_none()
                    && size.is_none()
                    && apply_raw_book_snapshot(event, &mut books, &mut summary)
                {
                    continue;
                }
                if event.event_type == EventType::BookSnapshot && price.is_none() && size.is_none()
                {
                    if let Some(asset) = event.asset_id.as_deref().filter(|asset| !asset.is_empty())
                    {
                        books.insert(
                            asset.to_string(),
                            BookState {
                                saw_snapshot: true,
                                ..BookState::default()
                            },
                        );
                        continue;
                    }
                }
                if price.is_none() {
                    summary.invalid_price_count += 1;
                }
                if size.is_none() {
                    summary.invalid_size_count += 1;
                }
                if let (Some(asset), Some(side), Some(price), Some(size)) = (
                    event.asset_id.as_deref(),
                    event.side.as_deref(),
                    price,
                    size,
                ) {
                    let state = books.entry(asset.to_string()).or_default();
                    if event.event_type == EventType::BookSnapshot {
                        state.saw_snapshot = true;
                    }
                    match side.to_ascii_lowercase().as_str() {
                        "buy" | "bid" | "bids" => state.bids.update(price, size),
                        "sell" | "ask" | "asks" => state.asks.update(price, size),
                        _ => summary.invalid_size_count += 1,
                    }
                    record_cross_status(state, event.received_ns, &mut summary, cfg);
                }
            }
            EventType::Trade => {
                summary.trade_event_count += 1;
                if parse_non_negative_f64(event.price.as_deref()).is_none() {
                    summary.invalid_price_count += 1;
                }
                if parse_non_negative_f64(event.size.as_deref()).is_none() {
                    summary.invalid_size_count += 1;
                }
            }
            EventType::BtcTick => {
                summary.btc_tick_count += 1;
                if parse_non_negative_f64(event.price.as_deref()).is_none() {
                    summary.invalid_price_count += 1;
                }
            }
            EventType::MarketMeta => {
                summary.market_meta_count += 1;
                expected_asset_ids.extend(asset_ids_from_market_meta(event));
            }
            EventType::Resolution => summary.resolution_event_count += 1,
            EventType::PriceToBeat => {
                if parse_non_negative_f64(event.price.as_deref()).is_none() {
                    summary.invalid_price_count += 1;
                }
            }
            EventType::UserFill | EventType::UserOrder | EventType::Heartbeat | EventType::Gap => {}
        }
    }

    for asset_id in expected_asset_ids {
        let missing = books
            .get(&asset_id)
            .is_none_or(|book| !book.saw_snapshot || book.bids.is_empty() || book.asks.is_empty());
        if missing {
            summary.missing_book_asset_ids.push(asset_id);
        }
    }
    if let Some(last_ns) = summary.last_received_ns {
        for state in books.values_mut() {
            close_open_cross(state, last_ns, &mut summary, cfg);
        }
    }

    classify_data_quality(&mut summary, cfg);
    summary
}

pub fn summarize_data_quality_windows(
    windows: &BTreeMap<String, Vec<Event>>,
    cfg: &DataQualityConfig,
) -> Vec<DataQualitySummary> {
    windows
        .iter()
        .map(|(window_id, events)| summarize_data_quality(window_id, events, cfg))
        .collect()
}

pub fn validate_data_quality(summaries: &[DataQualitySummary]) -> anyhow::Result<()> {
    let rejected = summaries
        .iter()
        .filter(|summary| summary.status == DataQualityStatus::Reject)
        .map(|summary| {
            format!(
                "{} rejected: {}",
                summary.window_id,
                summary.reject_reasons.join(",")
            )
        })
        .take(10)
        .collect::<Vec<_>>();
    if rejected.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "replay input failed data-quality preflight for {} window(s): {}",
        rejected.len(),
        rejected.join("; ")
    );
}

fn classify_data_quality(summary: &mut DataQualitySummary, cfg: &DataQualityConfig) {
    if summary.total_events == 0 {
        summary.reject_reasons.push("empty_window".to_string());
    }
    if summary.book_event_count == 0 {
        summary
            .reject_reasons
            .push("missing_book_events".to_string());
    }
    if summary.market_meta_count == 0
        && summary
            .event_type_counts
            .keys()
            .any(|kind| kind == "book_delta" || kind == "book_snapshot" || kind == "trade")
    {
        summary
            .reject_reasons
            .push("missing_market_meta".to_string());
    }
    if summary.invalid_price_count > 0 {
        summary.reject_reasons.push(format!(
            "invalid_price_count={}",
            summary.invalid_price_count
        ));
    }
    if summary.timestamp_drift_reject_count > 0 {
        summary.reject_reasons.push(format!(
            "timestamp_drift_reject_count={}",
            summary.timestamp_drift_reject_count
        ));
    }
    if summary.max_book_gap_ns > cfg.book_gap_reject_ns {
        summary
            .reject_reasons
            .push(format!("max_book_gap_ns={}", summary.max_book_gap_ns));
    }
    if summary.timestamp_drift_warn_count > 0 {
        summary.warnings.push(format!(
            "timestamp_drift_warn_count={}",
            summary.timestamp_drift_warn_count
        ));
    }
    if summary.duplicate_sequence_count > 0 {
        summary.warnings.push(format!(
            "duplicate_sequence_count={}",
            summary.duplicate_sequence_count
        ));
    }
    if summary.invalid_size_count > 0 {
        summary
            .warnings
            .push(format!("invalid_size_count={}", summary.invalid_size_count));
    }
    if summary.max_book_gap_ns > cfg.book_gap_warn_ns {
        summary
            .warnings
            .push(format!("max_book_gap_ns={}", summary.max_book_gap_ns));
    }
    if !summary.missing_book_asset_ids.is_empty() {
        summary.warnings.push(format!(
            "missing_book_asset_ids={}",
            summary.missing_book_asset_ids.join(",")
        ));
    }
    if summary.crossed_book_samples > 0 {
        summary.warnings.push(format!(
            "crossed_book_samples={},max_duration_ns={}",
            summary.crossed_book_samples, summary.crossed_book_max_duration_ns
        ));
    }
    if summary.crossed_book_persistent_count > 0 {
        summary.warnings.push(format!(
            "crossed_book_persistent_count={},max_duration_ns={}",
            summary.crossed_book_persistent_count, summary.crossed_book_max_duration_ns
        ));
    }
    if summary.resolution_event_count == 0 {
        summary
            .warnings
            .push("missing_resolution_event".to_string());
    }

    summary.status = if !summary.reject_reasons.is_empty() {
        DataQualityStatus::Reject
    } else if !summary.warnings.is_empty() {
        DataQualityStatus::Warn
    } else {
        DataQualityStatus::Pass
    };
}

fn parse_non_negative_f64(value: Option<&str>) -> Option<f64> {
    let value = value?;
    let parsed = value.parse::<f64>().ok()?;
    (parsed.is_finite() && parsed >= 0.0).then_some(parsed)
}

fn parse_json_non_negative_f64(value: Option<&serde_json::Value>) -> Option<f64> {
    match value? {
        serde_json::Value::String(s) => parse_non_negative_f64(Some(s)),
        serde_json::Value::Number(n) => {
            let parsed = n.as_f64()?;
            (parsed.is_finite() && parsed >= 0.0).then_some(parsed)
        }
        _ => None,
    }
}

fn apply_raw_book_snapshot(
    event: &Event,
    books: &mut BTreeMap<String, BookState>,
    summary: &mut DataQualitySummary,
) -> bool {
    let has_levels = event
        .raw
        .get("bids")
        .and_then(|value| value.as_array())
        .is_some()
        || event
            .raw
            .get("asks")
            .and_then(|value| value.as_array())
            .is_some();
    if !has_levels {
        return false;
    }

    let asset = event
        .asset_id
        .as_deref()
        .filter(|asset| !asset.is_empty())
        .or_else(|| event.raw.get("asset_id").and_then(|value| value.as_str()));
    let Some(asset) = asset else {
        summary.invalid_size_count += 1;
        return true;
    };

    let state = books.entry(asset.to_string()).or_default();
    state.saw_snapshot = true;
    state.bids.levels.clear();
    state.asks.levels.clear();

    for (key, is_bid) in [("bids", true), ("asks", false)] {
        let Some(levels) = event.raw.get(key).and_then(|value| value.as_array()) else {
            continue;
        };
        for level in levels {
            let price = parse_json_non_negative_f64(level.get("price"));
            let size = parse_json_non_negative_f64(level.get("size"));
            if price.is_none() {
                summary.invalid_price_count += 1;
            }
            if size.is_none() {
                summary.invalid_size_count += 1;
            }
            if let (Some(price), Some(size)) = (price, size) {
                if is_bid {
                    state.bids.update(price, size);
                } else {
                    state.asks.update(price, size);
                }
            }
        }
    }

    record_cross_status(
        state,
        event.received_ns,
        summary,
        &DataQualityConfig::default(),
    );

    true
}

fn record_cross_status(
    state: &mut BookState,
    received_ns: i64,
    summary: &mut DataQualitySummary,
    cfg: &DataQualityConfig,
) {
    let crossed = matches!(
        (state.bids.best_bid(), state.asks.best_ask()),
        (Some(best_bid), Some(best_ask)) if best_bid > best_ask
    );
    if crossed {
        summary.crossed_book_samples += 1;
        if state.crossed_since_ns.is_none() {
            state.crossed_since_ns = Some(received_ns);
        }
        return;
    }
    close_open_cross(state, received_ns, summary, cfg);
}

fn close_open_cross(
    state: &mut BookState,
    received_ns: i64,
    summary: &mut DataQualitySummary,
    cfg: &DataQualityConfig,
) {
    let Some(start_ns) = state.crossed_since_ns.take() else {
        return;
    };
    let duration = received_ns.saturating_sub(start_ns);
    summary.crossed_book_max_duration_ns = summary.crossed_book_max_duration_ns.max(duration);
    if duration > cfg.crossed_book_reject_duration_ns {
        summary.crossed_book_persistent_count += 1;
    }
}

fn price_tick(price: f64) -> i64 {
    (price * 10_000.0).round() as i64
}

fn asset_ids_from_market_meta(event: &Event) -> Vec<String> {
    event
        .raw
        .get("asset_ids")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::schema::Source;
    use serde_json::json;

    fn event(event_type: EventType, received_ns: i64) -> Event {
        Event {
            v: 1,
            ts_ns: received_ns,
            received_ns,
            event_type,
            market_type: "btc_5m".to_string(),
            market_slug: Some("btc-updown-5m-1776127500".to_string()),
            asset_id: None,
            side: None,
            price: None,
            size: None,
            sequence: Some(received_ns),
            source: Source::PolymarketMarketWs,
            raw: json!({}),
        }
    }

    fn meta() -> Event {
        Event {
            event_type: EventType::MarketMeta,
            source: Source::PolymarketDataApi,
            raw: json!({ "asset_ids": ["UP", "DOWN"] }),
            ..event(EventType::MarketMeta, 1)
        }
    }

    fn book(asset: &str, side: &str, price: &str, received_ns: i64) -> Event {
        Event {
            event_type: EventType::BookSnapshot,
            asset_id: Some(asset.to_string()),
            side: Some(side.to_string()),
            price: Some(price.to_string()),
            size: Some("100".to_string()),
            raw: json!({}),
            ..event(EventType::BookSnapshot, received_ns)
        }
    }

    #[test]
    fn quality_passes_complete_window() {
        let events = vec![
            meta(),
            book("UP", "buy", "0.49", 2),
            book("UP", "sell", "0.51", 3),
            book("DOWN", "buy", "0.48", 4),
            book("DOWN", "sell", "0.52", 5),
            Event {
                event_type: EventType::Trade,
                asset_id: Some("UP".to_string()),
                side: Some("buy".to_string()),
                price: Some("0.51".to_string()),
                size: Some("5".to_string()),
                raw: json!({}),
                ..event(EventType::Trade, 6)
            },
            Event {
                event_type: EventType::Resolution,
                raw: json!({ "winning_outcome": "Up" }),
                ..event(EventType::Resolution, 7)
            },
        ];

        let summary = summarize_data_quality("w", &events, &DataQualityConfig::default());

        assert_eq!(summary.status, DataQualityStatus::Pass);
        assert!(summary.reject_reasons.is_empty());
        assert!(summary.warnings.is_empty());
    }

    #[test]
    fn quality_rejects_missing_book_events() {
        let summary = summarize_data_quality("w", &[meta()], &DataQualityConfig::default());

        assert_eq!(summary.status, DataQualityStatus::Reject);
        assert!(summary
            .reject_reasons
            .contains(&"missing_book_events".to_string()));
    }

    #[test]
    fn quality_warns_on_missing_resolution_and_book_side() {
        let events = vec![meta(), book("UP", "buy", "0.49", 2)];

        let summary = summarize_data_quality("w", &events, &DataQualityConfig::default());

        assert_eq!(summary.status, DataQualityStatus::Warn);
        assert!(summary
            .warnings
            .iter()
            .any(|warning| warning.contains("missing_book_asset_ids")));
        assert!(summary
            .warnings
            .contains(&"missing_resolution_event".to_string()));
    }

    #[test]
    fn quality_rejects_large_timestamp_drift() {
        let mut stale = book("UP", "buy", "0.49", 10 * 60 * NS_PER_SECOND);
        stale.ts_ns = 0;

        let summary = summarize_data_quality("w", &[meta(), stale], &DataQualityConfig::default());

        assert_eq!(summary.status, DataQualityStatus::Reject);
        assert!(summary
            .reject_reasons
            .iter()
            .any(|reason| reason.contains("timestamp_drift_reject_count")));
    }

    #[test]
    fn quality_warns_on_persistent_crossed_book() {
        let events = vec![
            meta(),
            book("UP", "buy", "0.60", 2),
            book("UP", "sell", "0.50", 3),
            book("UP", "sell", "0.61", NS_PER_SECOND),
            book("DOWN", "buy", "0.48", NS_PER_SECOND + 1),
            book("DOWN", "sell", "0.52", NS_PER_SECOND + 2),
            Event {
                event_type: EventType::Resolution,
                raw: json!({ "winning_outcome": "Up" }),
                ..event(EventType::Resolution, NS_PER_SECOND + 3)
            },
        ];

        let summary = summarize_data_quality("w", &events, &DataQualityConfig::default());

        assert_eq!(summary.status, DataQualityStatus::Warn);
        assert!(summary.reject_reasons.is_empty());
        assert_eq!(summary.crossed_book_persistent_count, 1);
        assert!(summary
            .warnings
            .iter()
            .any(|warning| warning.contains("crossed_book_persistent_count=1")));
    }
}
