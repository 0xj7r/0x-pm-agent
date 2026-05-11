//! Raw Telonex/Binance Parquet reader for replay.
//!
//! This bypasses the Python `raw-parquet-to-rust-parquet` materialization
//! step for hot backtest runs. It intentionally supports only the raw channels
//! needed by BTC 5m paired-MM replay: Polymarket trades, quotes,
//! `book_snapshot_25`, on-chain fills, and Binance BTCUSDT agg trades.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use arrow::array::{Array, Float64Array, Int64Array, LargeStringArray, StringArray, UInt64Array};
use chrono::{TimeZone, Utc};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde_json::{json, Value};
use walkdir::WalkDir;

use crate::collector::schema::{Event, EventType, Source};
use crate::replay::reader::dedupe_and_sort;

#[derive(Debug, Clone)]
pub struct RawReplayOptions {
    pub window_start_ns: i64,
    pub window_end_ns: i64,
    pub market_filter: String,
    pub max_book_levels: usize,
    pub markets: Vec<RawReplayMarket>,
}

#[derive(Debug, Clone)]
pub struct RawReplayMarket {
    pub slug: String,
    pub asset_ids: [String; 2],
    pub strike: Option<String>,
}

pub fn read_raw_replay(path: &Path, options: &RawReplayOptions) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    let mut btc_ticks = Vec::new();
    let mut book_state = BookDiffState::default();
    let file_filter = RawFileFilter::new(options);
    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        if !file_filter.should_read(p) {
            continue;
        }
        read_raw_parquet_file(p, options, &mut book_state, &mut events, &mut btc_ticks)?;
    }
    let routed_ticks = route_btc_ticks_per_market(&btc_ticks, options)?;
    let strike = first_btc_tick_price(&routed_ticks);
    events.extend(routed_ticks);
    events.extend(market_meta_events(options, strike)?);
    Ok(dedupe_and_sort(events))
}

fn route_btc_ticks_per_market(
    ticks: &[Event],
    options: &RawReplayOptions,
) -> Result<Vec<Event>> {
    if options.markets.is_empty() {
        return Ok(ticks.to_vec());
    }
    let mut windows: Vec<(String, i64, i64)> = options
        .markets
        .iter()
        .map(|market| {
            let start_ms = market_start_ms(&market.slug)
                .unwrap_or(options.window_start_ns / 1_000_000);
            let end_ms = market_end_ms(&market.slug)
                .unwrap_or(options.window_end_ns / 1_000_000);
            (
                market.slug.clone(),
                start_ms.saturating_mul(1_000_000),
                end_ms.saturating_mul(1_000_000),
            )
        })
        .collect();
    windows.sort_by_key(|w| w.1);

    let min_start = windows.iter().map(|w| w.1).min().unwrap_or(i64::MIN);
    let max_end = windows.iter().map(|w| w.2).max().unwrap_or(i64::MAX);

    let mut hits_per_market: BTreeMap<String, usize> = windows
        .iter()
        .map(|w| (w.0.clone(), 0))
        .collect();
    let mut dropped_before = 0_usize;
    let mut dropped_after = 0_usize;
    let mut dropped_gap = 0_usize;
    let mut routed = Vec::with_capacity(ticks.len());

    for tick in ticks {
        let t = tick.received_ns;
        if t < min_start {
            dropped_before += 1;
            continue;
        }
        if t >= max_end {
            dropped_after += 1;
            continue;
        }
        let mut matched = false;
        for (slug, start_ns, end_ns) in &windows {
            if t >= *start_ns && t < *end_ns {
                let mut routed_tick = tick.clone();
                routed_tick.market_slug = Some(slug.clone());
                routed.push(routed_tick);
                if let Some(count) = hits_per_market.get_mut(slug) {
                    *count += 1;
                }
                matched = true;
                break;
            }
        }
        if !matched {
            dropped_gap += 1;
        }
    }

    if dropped_before > 0 || dropped_after > 0 || dropped_gap > 0 {
        eprintln!(
            "raw_parquet: dropped btc ticks before={} after={} gap={}",
            dropped_before, dropped_after, dropped_gap
        );
    }

    let empty: Vec<&String> = hits_per_market
        .iter()
        .filter(|(_, c)| **c == 0)
        .map(|(slug, _)| slug)
        .collect();
    if !empty.is_empty() {
        return Err(anyhow!(
            "raw_parquet: market windows have zero btc ticks: {:?}",
            empty
        ));
    }

    Ok(routed)
}

struct RawFileFilter {
    start_date: String,
    end_date: String,
    market_slugs: BTreeSet<String>,
    asset_ids: BTreeSet<String>,
    book_channel: &'static str,
}

impl RawFileFilter {
    fn new(options: &RawReplayOptions) -> Self {
        let start_date = date_partition(options.window_start_ns);
        let end_date = date_partition(options.window_end_ns.saturating_sub(1));
        let mut market_slugs = BTreeSet::new();
        let mut asset_ids = BTreeSet::new();
        for market in &options.markets {
            market_slugs.insert(market.slug.clone());
            asset_ids.extend(market.asset_ids.iter().cloned());
        }
        Self {
            start_date,
            end_date,
            market_slugs,
            asset_ids,
            book_channel: selected_book_channel(options.max_book_levels),
        }
    }

    fn should_read(&self, path: &Path) -> bool {
        if path.extension().and_then(|e| e.to_str()) != Some("parquet") {
            return false;
        }

        if let Some(date) = partition_value(path, "date").or_else(|| partition_value(path, "dt")) {
            if date.as_str() < self.start_date.as_str() || date.as_str() > self.end_date.as_str() {
                return false;
            }
        }

        if let Some(slug) =
            partition_value(path, "slug").or_else(|| partition_value(path, "market_slug"))
        {
            if !self.market_slugs.is_empty() && !self.market_slugs.contains(&slug) {
                return false;
            }
        }

        if let Some(asset_id) = partition_value(path, "asset_id")
            .or_else(|| partition_value(path, "token_id"))
            .or_else(|| partition_value(path, "asset"))
        {
            if !self.asset_ids.is_empty() && !self.asset_ids.contains(&asset_id) {
                return false;
            }
        }

        let exchange = partition_value(path, "exchange");
        let channel = partition_value(path, "channel").or_else(|| partition_value(path, "dataset"));
        matches!(
            (exchange.as_deref(), channel.as_deref()),
            (Some("polymarket"), Some("trades"))
                | (Some("polymarket"), Some("quotes"))
                | (Some("polymarket"), Some("onchain_fills"))
                | (Some("binance"), Some("agg_trades"))
                | (Some("binance"), Some("trades"))
        ) || matches!(
            (exchange.as_deref(), channel.as_deref()),
            (Some("polymarket"), Some(channel)) if channel == self.book_channel
        )
    }
}

fn selected_book_channel(max_book_levels: usize) -> &'static str {
    if max_book_levels <= 5 {
        "book_snapshot_5"
    } else if max_book_levels <= 25 {
        "book_snapshot_25"
    } else {
        "book_snapshot_full"
    }
}

fn date_partition(ts_ns: i64) -> String {
    Utc.timestamp_opt(ts_ns / 1_000_000_000, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "1970-01-01".to_string())
}

fn read_raw_parquet_file(
    path: &Path,
    options: &RawReplayOptions,
    book_state: &mut BookDiffState,
    events: &mut Vec<Event>,
    btc_ticks: &mut Vec<Event>,
) -> Result<()> {
    let exchange = partition_value(path, "exchange");
    let channel = partition_value(path, "channel").or_else(|| partition_value(path, "dataset"));
    let Some(channel) = channel else {
        return Ok(());
    };
    let file = File::open(path).with_context(|| format!("open raw parquet {}", path.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .with_context(|| format!("read parquet metadata {}", path.display()))?;
    let reader = builder
        .build()
        .with_context(|| format!("build parquet reader {}", path.display()))?;
    let mut sequence = 0_i64;
    for batch in reader {
        let batch = batch.with_context(|| format!("read parquet batch {}", path.display()))?;
        for row in 0..batch.num_rows() {
            let Some(received_ns) = row_received_ns(&batch, row) else {
                continue;
            };
            if received_ns < options.window_start_ns || received_ns >= options.window_end_ns {
                continue;
            }
            let channel_events = match (exchange.as_deref(), channel.as_str()) {
                (Some("polymarket"), "trades") => {
                    polymarket_trade_event(&batch, row, received_ns, sequence)
                }
                (Some("polymarket"), "quotes") => {
                    polymarket_quote_events(&batch, row, received_ns, sequence)
                }
                (Some("polymarket"), "book_snapshot_25")
                | (Some("polymarket"), "book_snapshot_5")
                | (Some("polymarket"), "book_snapshot_full") => polymarket_book_events(
                    &batch,
                    row,
                    received_ns,
                    sequence,
                    options.max_book_levels,
                    book_state,
                ),
                (Some("polymarket"), "onchain_fills") => {
                    polymarket_onchain_event(&batch, row, received_ns, sequence)
                }
                (Some("binance"), "agg_trades") | (Some("binance"), "trades") => {
                    binance_tick_event(&batch, row, received_ns, sequence)
                }
                _ => Vec::new(),
            };
            for event in channel_events {
                if event.event_type == EventType::BtcTick {
                    btc_ticks.push(event);
                } else if event_matches_options(&event, options) {
                    events.push(event);
                }
            }
            sequence += 1;
        }
    }
    Ok(())
}

fn market_meta_events(options: &RawReplayOptions, strike: Option<String>) -> Result<Vec<Event>> {
    let mut events = Vec::with_capacity(options.markets.len());
    for (idx, market) in options.markets.iter().enumerate() {
        let start_ms = market_start_ms(&market.slug).unwrap_or(options.window_start_ns / 1_000_000);
        let end_ms = market_end_ms(&market.slug).unwrap_or(options.window_end_ns / 1_000_000);
        let mut raw = json!({
            "slug": market.slug,
            "market_type": market_type_from_slug(&market.slug),
            "asset_ids": market.asset_ids,
            "outcomes": ["Up", "Down"],
            "start_time_ms": start_ms,
            "end_time_ms": end_ms,
            "source": "resolved_raw_replay_market_meta"
        });
        if let Some(strike) = market.strike.as_ref().or(strike.as_ref()) {
            raw["strike"] = json!(strike.parse::<f64>().unwrap_or(0.0));
            raw["strike_source"] = json!(if market.strike.is_some() {
                "market_metadata"
            } else {
                "binance_first_tick_at_market_start"
            });
        }
        events.push(Event {
            v: 1,
            ts_ns: options.window_start_ns,
            received_ns: options.window_start_ns,
            event_type: EventType::MarketMeta,
            market_type: market_type_from_slug(&market.slug),
            market_slug: Some(market.slug.clone()),
            asset_id: None,
            side: None,
            price: None,
            size: None,
            sequence: Some(-10_000 + idx as i64),
            source: Source::PolymarketDataApi,
            raw,
        });
    }
    Ok(events)
}

fn first_btc_tick_price(events: &[Event]) -> Option<String> {
    events
        .iter()
        .filter(|event| event.event_type == EventType::BtcTick)
        .min_by_key(|event| event.received_ns)
        .and_then(|event| event.price.clone())
}

fn polymarket_trade_event(
    batch: &arrow::record_batch::RecordBatch,
    row: usize,
    received_ns: i64,
    sequence: i64,
) -> Vec<Event> {
    vec![base_polymarket_event(
        batch,
        row,
        received_ns,
        sequence,
        EventType::Trade,
    )]
}

fn polymarket_onchain_event(
    batch: &arrow::record_batch::RecordBatch,
    row: usize,
    received_ns: i64,
    sequence: i64,
) -> Vec<Event> {
    vec![base_polymarket_event(
        batch,
        row,
        received_ns,
        sequence,
        EventType::UserFill,
    )]
}

fn polymarket_quote_events(
    batch: &arrow::record_batch::RecordBatch,
    row: usize,
    received_ns: i64,
    sequence: i64,
) -> Vec<Event> {
    let mut events = Vec::new();
    if let (Some(price), Some(size)) = (
        value_string(batch, "bid_price", row).or_else(|| value_string(batch, "bid_price_0", row)),
        value_string(batch, "bid_size", row).or_else(|| value_string(batch, "bid_size_0", row)),
    ) {
        events.push(book_event(
            batch,
            row,
            received_ns,
            sequence * 10_000,
            "buy",
            price,
            size,
        ));
    }
    if let (Some(price), Some(size)) = (
        value_string(batch, "ask_price", row).or_else(|| value_string(batch, "ask_price_0", row)),
        value_string(batch, "ask_size", row).or_else(|| value_string(batch, "ask_size_0", row)),
    ) {
        events.push(book_event(
            batch,
            row,
            received_ns,
            sequence * 10_000 + 1,
            "sell",
            price,
            size,
        ));
    }
    events
}

fn polymarket_book_events(
    batch: &arrow::record_batch::RecordBatch,
    row: usize,
    received_ns: i64,
    sequence: i64,
    max_book_levels: usize,
    book_state: &mut BookDiffState,
) -> Vec<Event> {
    let slug = row_slug(batch, row).unwrap_or_default();
    let asset_id = row_asset_id(batch, row).unwrap_or_default();
    let mut events = Vec::new();
    for (side, prefix, side_offset) in [("buy", "bid", 0_i64), ("sell", "ask", 5_000_i64)] {
        let levels = flattened_levels(batch, row, prefix, max_book_levels);
        let diffs = book_state.diff(&slug, &asset_id, side, levels);
        for (idx, (price, size)) in diffs.into_iter().enumerate() {
            events.push(book_event(
                batch,
                row,
                received_ns,
                sequence * 10_000 + side_offset + idx as i64,
                side,
                price,
                size,
            ));
        }
    }
    events
}

fn binance_tick_event(
    batch: &arrow::record_batch::RecordBatch,
    row: usize,
    received_ns: i64,
    sequence: i64,
) -> Vec<Event> {
    vec![Event {
        v: 1,
        ts_ns: row_ts_ns(batch, row).unwrap_or(received_ns),
        received_ns,
        event_type: EventType::BtcTick,
        market_type: "btc_ref".to_string(),
        market_slug: Some("btcusdt".to_string()),
        asset_id: Some("BTC".to_string()),
        side: None,
        price: value_string(batch, "price", row).or_else(|| value_string(batch, "last_price", row)),
        size: value_string(batch, "quantity", row).or_else(|| value_string(batch, "qty", row)),
        sequence: Some(sequence),
        source: Source::BinanceAggtrade,
        raw: Value::Null,
    }]
}

fn base_polymarket_event(
    batch: &arrow::record_batch::RecordBatch,
    row: usize,
    received_ns: i64,
    sequence: i64,
    event_type: EventType,
) -> Event {
    Event {
        v: 1,
        ts_ns: row_ts_ns(batch, row).unwrap_or(received_ns),
        received_ns,
        event_type,
        market_type: market_type_from_slug(row_slug(batch, row).as_deref().unwrap_or_default()),
        market_slug: row_slug(batch, row),
        asset_id: row_asset_id(batch, row),
        side: row_side(batch, row),
        price: value_string(batch, "price", row)
            .or_else(|| value_string(batch, "last_trade_price", row)),
        size: value_string(batch, "size", row)
            .or_else(|| value_string(batch, "quantity", row))
            .or_else(|| value_string(batch, "amount", row)),
        sequence: Some(sequence),
        source: Source::Collector,
        raw: Value::Null,
    }
}

fn book_event(
    batch: &arrow::record_batch::RecordBatch,
    row: usize,
    received_ns: i64,
    sequence: i64,
    side: &str,
    price: String,
    size: String,
) -> Event {
    Event {
        v: 1,
        ts_ns: row_ts_ns(batch, row).unwrap_or(received_ns),
        received_ns,
        event_type: EventType::BookDelta,
        market_type: market_type_from_slug(row_slug(batch, row).as_deref().unwrap_or_default()),
        market_slug: row_slug(batch, row),
        asset_id: row_asset_id(batch, row),
        side: Some(side.to_string()),
        price: Some(price),
        size: Some(size),
        sequence: Some(sequence),
        source: Source::Collector,
        raw: Value::Null,
    }
}

fn event_matches_options(event: &Event, options: &RawReplayOptions) -> bool {
    if event.market_type == "btc_ref" || event.market_type == "reference" {
        return true;
    }
    if !options.market_filter.is_empty() && event.market_type != options.market_filter {
        return false;
    }
    if options.markets.is_empty() {
        return true;
    }
    event
        .market_slug
        .as_ref()
        .is_some_and(|slug| options.markets.iter().any(|m| m.slug == *slug))
}

#[derive(Default)]
struct BookDiffState {
    levels: BTreeMap<(String, String, String), BTreeMap<String, String>>,
}

impl BookDiffState {
    fn diff(
        &mut self,
        slug: &str,
        asset_id: &str,
        side: &str,
        levels: Vec<(String, String)>,
    ) -> Vec<(String, String)> {
        let key = (slug.to_string(), asset_id.to_string(), side.to_string());
        let previous = self.levels.get(&key).cloned().unwrap_or_default();
        let current: BTreeMap<String, String> = levels.into_iter().collect();
        let mut out = Vec::new();
        let prices: BTreeSet<String> = previous.keys().chain(current.keys()).cloned().collect();
        for price in prices {
            let old = previous.get(&price);
            let new = current.get(&price);
            if old == new {
                continue;
            }
            out.push((
                price.clone(),
                new.cloned().unwrap_or_else(|| "0".to_string()),
            ));
        }
        out.sort_by(|a, b| price_sort_key(&a.0).total_cmp(&price_sort_key(&b.0)));
        self.levels.insert(key, current);
        out
    }
}

fn flattened_levels(
    batch: &arrow::record_batch::RecordBatch,
    row: usize,
    prefix: &str,
    max_book_levels: usize,
) -> Vec<(String, String)> {
    let mut levels = Vec::new();
    for idx in 0..max_book_levels {
        let price = value_string(batch, &format!("{prefix}_price_{idx}"), row);
        let size = value_string(batch, &format!("{prefix}_size_{idx}"), row);
        match (price, size) {
            (Some(price), Some(size)) => levels.push((price, size)),
            (None, None) => break,
            _ => {}
        }
    }
    levels
}

fn row_received_ns(batch: &arrow::record_batch::RecordBatch, row: usize) -> Option<i64> {
    for name in [
        "local_timestamp_us",
        "timestamp_us",
        "block_timestamp_us",
        "transact_time_ms",
        "trade_time_ms",
        "timestamp",
    ] {
        if let Some(value) = value_i64(batch, name, row) {
            return Some(timestamp_to_ns(value));
        }
        if let Some(value) = value_string(batch, name, row).and_then(|v| v.parse::<i64>().ok()) {
            return Some(timestamp_to_ns(value));
        }
    }
    None
}

fn row_ts_ns(batch: &arrow::record_batch::RecordBatch, row: usize) -> Option<i64> {
    for name in [
        "timestamp_us",
        "block_timestamp_us",
        "transact_time_ms",
        "trade_time_ms",
        "timestamp",
        "local_timestamp_us",
    ] {
        if let Some(value) = value_i64(batch, name, row) {
            return Some(timestamp_to_ns(value));
        }
        if let Some(value) = value_string(batch, name, row).and_then(|v| v.parse::<i64>().ok()) {
            return Some(timestamp_to_ns(value));
        }
    }
    None
}

fn timestamp_to_ns(value: i64) -> i64 {
    if value > 10_000_000_000_000 {
        value * 1_000
    } else if value > 10_000_000_000 {
        value * 1_000_000
    } else {
        value * 1_000_000_000
    }
}

fn row_slug(batch: &arrow::record_batch::RecordBatch, row: usize) -> Option<String> {
    value_string(batch, "slug", row)
        .or_else(|| value_string(batch, "market_slug", row))
        .or_else(|| value_string(batch, "market", row))
}

fn row_asset_id(batch: &arrow::record_batch::RecordBatch, row: usize) -> Option<String> {
    value_string(batch, "asset_id", row)
        .or_else(|| value_string(batch, "token_id", row))
        .or_else(|| value_string(batch, "asset", row))
        .or_else(|| partition_value_from_batch_path(batch, row))
}

fn partition_value_from_batch_path(
    _batch: &arrow::record_batch::RecordBatch,
    _row: usize,
) -> Option<String> {
    None
}

fn row_side(batch: &arrow::record_batch::RecordBatch, row: usize) -> Option<String> {
    value_string(batch, "side", row)
        .or_else(|| value_string(batch, "trader_side", row))
        .map(|side| side.to_ascii_lowercase())
}

fn value_string(
    batch: &arrow::record_batch::RecordBatch,
    name: &str,
    row: usize,
) -> Option<String> {
    let idx = batch.schema().index_of(name).ok()?;
    let arr = batch.column(idx);
    if arr.is_null(row) {
        return None;
    }
    if let Some(a) = arr.as_any().downcast_ref::<StringArray>() {
        return Some(a.value(row).to_string());
    }
    if let Some(a) = arr.as_any().downcast_ref::<LargeStringArray>() {
        return Some(a.value(row).to_string());
    }
    if let Some(a) = arr.as_any().downcast_ref::<Int64Array>() {
        return Some(a.value(row).to_string());
    }
    if let Some(a) = arr.as_any().downcast_ref::<UInt64Array>() {
        return Some(a.value(row).to_string());
    }
    if let Some(a) = arr.as_any().downcast_ref::<Float64Array>() {
        return Some(a.value(row).to_string());
    }
    None
}

fn value_i64(batch: &arrow::record_batch::RecordBatch, name: &str, row: usize) -> Option<i64> {
    let idx = batch.schema().index_of(name).ok()?;
    let arr = batch.column(idx);
    if arr.is_null(row) {
        return None;
    }
    if let Some(a) = arr.as_any().downcast_ref::<Int64Array>() {
        return Some(a.value(row));
    }
    if let Some(a) = arr.as_any().downcast_ref::<UInt64Array>() {
        return i64::try_from(a.value(row)).ok();
    }
    None
}

fn partition_value(path: &Path, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    path.components().find_map(|part| {
        let text = part.as_os_str().to_string_lossy();
        text.strip_prefix(&prefix).map(ToString::to_string)
    })
}

fn market_type_from_slug(slug: &str) -> String {
    let text = slug.to_ascii_lowercase();
    if text.contains("eth") {
        if text.contains("15m") {
            "eth_15m".to_string()
        } else {
            "eth_5m".to_string()
        }
    } else if text.contains("btc") || text.contains("bitcoin") {
        if text.contains("15m") {
            "btc_15m".to_string()
        } else {
            "btc_5m".to_string()
        }
    } else {
        "unknown".to_string()
    }
}

fn market_start_ms(slug: &str) -> Option<i64> {
    for prefix in ["btc-updown-5m-", "eth-updown-5m-"] {
        if let Some(rest) = slug.strip_prefix(prefix) {
            return rest.parse::<i64>().ok().map(|seconds| seconds * 1000);
        }
    }
    None
}

fn market_end_ms(slug: &str) -> Option<i64> {
    market_start_ms(slug).map(|start| start + 5 * 60 * 1000)
}

fn price_sort_key(price: &str) -> f64 {
    price.parse::<f64>().unwrap_or(f64::INFINITY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[test]
    fn raw_reader_maps_polymarket_book_and_binance_tick() {
        let dir = tempdir().unwrap();
        let poly = dir
            .path()
            .join("exchange=polymarket/channel=book_snapshot_25/date=2026-02-15/asset_id=UP");
        std::fs::create_dir_all(&poly).unwrap();
        write_book_fixture(&poly.join("book.parquet"));
        let binance = dir
            .path()
            .join("exchange=binance/channel=agg_trades/symbol=BTCUSDT/date=2026-02-15");
        std::fs::create_dir_all(&binance).unwrap();
        write_binance_fixture(&binance.join("btc.parquet"));

        let events = read_raw_replay(
            dir.path(),
            &RawReplayOptions {
                window_start_ns: 1_771_178_400_000_000_000,
                window_end_ns: 1_771_178_700_000_000_000,
                market_filter: "btc_5m".to_string(),
                max_book_levels: 25,
                markets: vec![RawReplayMarket {
                    slug: "btc-updown-5m-1771178400".to_string(),
                    asset_ids: ["UP".to_string(), "DOWN".to_string()],
                    strike: None,
                }],
            },
        )
        .unwrap();

        assert!(events.iter().any(|e| e.event_type == EventType::MarketMeta));
        let market_meta = events
            .iter()
            .find(|e| e.event_type == EventType::MarketMeta)
            .unwrap();
        assert_eq!(
            market_meta.raw.get("strike").and_then(|v| v.as_f64()),
            Some(79000.12)
        );
        assert!(events.iter().any(|e| {
            e.event_type == EventType::BookDelta
                && e.side.as_deref() == Some("buy")
                && e.price.as_deref() == Some("0.44")
        }));
        assert!(events.iter().any(|e| {
            e.event_type == EventType::BtcTick
                && e.market_type == "btc_ref"
                && e.price.as_deref() == Some("79000.12")
        }));
    }

    #[test]
    fn raw_reader_uses_single_book_depth_channel_for_requested_depth() {
        let dir = tempdir().unwrap();
        let book_25 = dir
            .path()
            .join("exchange=polymarket/channel=book_snapshot_25/date=2026-02-15/asset_id=UP");
        std::fs::create_dir_all(&book_25).unwrap();
        write_book_fixture(&book_25.join("book.parquet"));

        let book_full = dir
            .path()
            .join("exchange=polymarket/channel=book_snapshot_full/date=2026-02-15/asset_id=UP");
        std::fs::create_dir_all(&book_full).unwrap();
        write_book_fixture(&book_full.join("book.parquet"));

        let binance = dir
            .path()
            .join("exchange=binance/channel=agg_trades/symbol=BTCUSDT/date=2026-02-15");
        std::fs::create_dir_all(&binance).unwrap();
        write_binance_fixture(&binance.join("btc.parquet"));

        let events = read_raw_replay(
            dir.path(),
            &RawReplayOptions {
                window_start_ns: 1_771_178_400_000_000_000,
                window_end_ns: 1_771_178_700_000_000_000,
                market_filter: "btc_5m".to_string(),
                max_book_levels: 25,
                markets: vec![RawReplayMarket {
                    slug: "btc-updown-5m-1771178400".to_string(),
                    asset_ids: ["UP".to_string(), "DOWN".to_string()],
                    strike: None,
                }],
            },
        )
        .unwrap();

        let book_deltas = events
            .iter()
            .filter(|event| event.event_type == EventType::BookDelta)
            .count();
        assert_eq!(book_deltas, 2);
    }

    #[test]
    fn raw_file_filter_skips_dates_and_assets_outside_window_plan() {
        let options = RawReplayOptions {
            window_start_ns: 1_771_178_400_000_000_000,
            window_end_ns: 1_771_178_700_000_000_000,
            market_filter: "btc_5m".to_string(),
            max_book_levels: 25,
            markets: vec![RawReplayMarket {
                slug: "btc-updown-5m-1771178400".to_string(),
                asset_ids: ["UP".to_string(), "DOWN".to_string()],
                strike: None,
            }],
        };
        let filter = RawFileFilter::new(&options);

        assert!(filter.should_read(Path::new(
            "exchange=polymarket/channel=book_snapshot_25/date=2026-02-15/asset_id=UP/file.parquet"
        )));
        assert!(!filter.should_read(Path::new(
            "exchange=polymarket/channel=book_snapshot_full/date=2026-02-15/asset_id=UP/file.parquet"
        )));
        assert!(!filter.should_read(Path::new(
            "exchange=polymarket/channel=book_snapshot_25/date=2026-02-14/asset_id=UP/file.parquet"
        )));
        assert!(!filter.should_read(Path::new(
            "exchange=polymarket/channel=book_snapshot_25/date=2026-02-15/asset_id=OTHER/file.parquet"
        )));
    }

    fn write_book_fixture(path: &Path) {
        let schema = Arc::new(Schema::new(vec![
            Field::new("local_timestamp_us", DataType::Int64, false),
            Field::new("slug", DataType::Utf8, false),
            Field::new("asset_id", DataType::Utf8, false),
            Field::new("bid_price_0", DataType::Utf8, true),
            Field::new("bid_size_0", DataType::Utf8, true),
            Field::new("ask_price_0", DataType::Utf8, true),
            Field::new("ask_size_0", DataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![1_771_178_401_000_000])),
                Arc::new(StringArray::from(vec!["btc-updown-5m-1771178400"])),
                Arc::new(StringArray::from(vec!["UP"])),
                Arc::new(StringArray::from(vec![Some("0.44")])),
                Arc::new(StringArray::from(vec![Some("10")])),
                Arc::new(StringArray::from(vec![Some("0.46")])),
                Arc::new(StringArray::from(vec![Some("20")])),
            ],
        )
        .unwrap();
        let file = File::create(path).unwrap();
        let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    #[test]
    fn raw_reader_routes_btc_ticks_per_market_window() {
        let dir = tempdir().unwrap();
        let m0_start_s: i64 = 1_771_178_400;
        let m1_start_s: i64 = m0_start_s + 300;

        let poly0 = dir.path().join(format!(
            "exchange=polymarket/channel=book_snapshot_25/date=2026-02-15/asset_id=UP0"
        ));
        std::fs::create_dir_all(&poly0).unwrap();
        write_book_fixture_for(
            &poly0.join("book.parquet"),
            (m0_start_s * 1_000 + 1) * 1_000,
            "btc-updown-5m-1771178400",
            "UP0",
        );

        let poly1 = dir.path().join(format!(
            "exchange=polymarket/channel=book_snapshot_25/date=2026-02-15/asset_id=UP1"
        ));
        std::fs::create_dir_all(&poly1).unwrap();
        write_book_fixture_for(
            &poly1.join("book.parquet"),
            (m1_start_s * 1_000 + 1) * 1_000,
            &format!("btc-updown-5m-{}", m1_start_s),
            "UP1",
        );

        let binance = dir
            .path()
            .join("exchange=binance/channel=agg_trades/symbol=BTCUSDT/date=2026-02-15");
        std::fs::create_dir_all(&binance).unwrap();
        write_binance_fixture_multi(
            &binance.join("btc.parquet"),
            &[
                (m0_start_s * 1_000 + 50, "70000.00"),
                (m0_start_s * 1_000 + 200_000, "70010.00"),
                (m1_start_s * 1_000 + 50, "70020.00"),
                (m1_start_s * 1_000 + 100_000, "70030.00"),
            ],
        );

        let events = read_raw_replay(
            dir.path(),
            &RawReplayOptions {
                window_start_ns: (m0_start_s * 1_000_000_000),
                window_end_ns: ((m1_start_s + 300) * 1_000_000_000),
                market_filter: "btc_5m".to_string(),
                max_book_levels: 2,
                markets: vec![
                    RawReplayMarket {
                        slug: "btc-updown-5m-1771178400".to_string(),
                        asset_ids: ["UP0".to_string(), "DOWN0".to_string()],
                        strike: None,
                    },
                    RawReplayMarket {
                        slug: format!("btc-updown-5m-{}", m1_start_s),
                        asset_ids: ["UP1".to_string(), "DOWN1".to_string()],
                        strike: None,
                    },
                ],
            },
        )
        .unwrap();

        let m0_ticks: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event_type == EventType::BtcTick
                    && e.market_slug.as_deref() == Some("btc-updown-5m-1771178400")
            })
            .collect();
        let m1_ticks: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event_type == EventType::BtcTick
                    && e.market_slug.as_deref()
                        == Some(&format!("btc-updown-5m-{}", m1_start_s))
            })
            .collect();

        assert_eq!(m0_ticks.len(), 2);
        assert_eq!(m1_ticks.len(), 2);
        assert!(m0_ticks
            .iter()
            .all(|e| e.market_type == "btc_ref"
                && e.received_ns >= m0_start_s * 1_000_000_000
                && e.received_ns < m1_start_s * 1_000_000_000));
        assert!(m1_ticks.iter().all(|e| e.received_ns
            >= m1_start_s * 1_000_000_000
            && e.received_ns < (m1_start_s + 300) * 1_000_000_000));
    }

    #[test]
    fn raw_reader_halts_when_a_market_window_has_zero_btc_ticks() {
        let dir = tempdir().unwrap();
        let m0_start_s: i64 = 1_771_178_400;
        let m1_start_s: i64 = m0_start_s + 300;

        let poly0 = dir
            .path()
            .join("exchange=polymarket/channel=book_snapshot_25/date=2026-02-15/asset_id=UP0");
        std::fs::create_dir_all(&poly0).unwrap();
        write_book_fixture_for(
            &poly0.join("book.parquet"),
            (m0_start_s * 1_000 + 1) * 1_000,
            "btc-updown-5m-1771178400",
            "UP0",
        );

        let binance = dir
            .path()
            .join("exchange=binance/channel=agg_trades/symbol=BTCUSDT/date=2026-02-15");
        std::fs::create_dir_all(&binance).unwrap();
        write_binance_fixture_multi(
            &binance.join("btc.parquet"),
            &[(m0_start_s * 1_000 + 50, "70000.00")],
        );

        let res = read_raw_replay(
            dir.path(),
            &RawReplayOptions {
                window_start_ns: m0_start_s * 1_000_000_000,
                window_end_ns: (m1_start_s + 300) * 1_000_000_000,
                market_filter: "btc_5m".to_string(),
                max_book_levels: 2,
                markets: vec![
                    RawReplayMarket {
                        slug: "btc-updown-5m-1771178400".to_string(),
                        asset_ids: ["UP0".to_string(), "DOWN0".to_string()],
                        strike: None,
                    },
                    RawReplayMarket {
                        slug: format!("btc-updown-5m-{}", m1_start_s),
                        asset_ids: ["UP1".to_string(), "DOWN1".to_string()],
                        strike: None,
                    },
                ],
            },
        );
        let err = res.expect_err("expected halt on empty market window");
        assert!(format!("{err}").contains("zero btc ticks"));
    }

    fn write_book_fixture_for(path: &Path, ts_us: i64, slug: &str, asset_id: &str) {
        let schema = Arc::new(Schema::new(vec![
            Field::new("local_timestamp_us", DataType::Int64, false),
            Field::new("slug", DataType::Utf8, false),
            Field::new("asset_id", DataType::Utf8, false),
            Field::new("bid_price_0", DataType::Utf8, true),
            Field::new("bid_size_0", DataType::Utf8, true),
            Field::new("ask_price_0", DataType::Utf8, true),
            Field::new("ask_size_0", DataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![ts_us])),
                Arc::new(StringArray::from(vec![slug])),
                Arc::new(StringArray::from(vec![asset_id])),
                Arc::new(StringArray::from(vec![Some("0.44")])),
                Arc::new(StringArray::from(vec![Some("10")])),
                Arc::new(StringArray::from(vec![Some("0.46")])),
                Arc::new(StringArray::from(vec![Some("20")])),
            ],
        )
        .unwrap();
        let file = File::create(path).unwrap();
        let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    fn write_binance_fixture_multi(path: &Path, rows: &[(i64, &str)]) {
        let schema = Arc::new(Schema::new(vec![
            Field::new("transact_time_ms", DataType::Int64, false),
            Field::new("symbol", DataType::Utf8, false),
            Field::new("price", DataType::Utf8, false),
            Field::new("quantity", DataType::Utf8, false),
        ]));
        let times: Vec<i64> = rows.iter().map(|r| r.0).collect();
        let prices: Vec<&str> = rows.iter().map(|r| r.1).collect();
        let qty: Vec<&str> = rows.iter().map(|_| "0.1").collect();
        let symbols: Vec<&str> = rows.iter().map(|_| "BTCUSDT").collect();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(times)),
                Arc::new(StringArray::from(symbols)),
                Arc::new(StringArray::from(prices)),
                Arc::new(StringArray::from(qty)),
            ],
        )
        .unwrap();
        let file = File::create(path).unwrap();
        let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    fn write_binance_fixture(path: &Path) {
        let schema = Arc::new(Schema::new(vec![
            Field::new("transact_time_ms", DataType::Int64, false),
            Field::new("symbol", DataType::Utf8, false),
            Field::new("price", DataType::Utf8, false),
            Field::new("quantity", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![1_771_178_401_123])),
                Arc::new(StringArray::from(vec!["BTCUSDT"])),
                Arc::new(StringArray::from(vec!["79000.12"])),
                Arc::new(StringArray::from(vec!["0.2"])),
            ],
        )
        .unwrap();
        let file = File::create(path).unwrap();
        let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }
}
