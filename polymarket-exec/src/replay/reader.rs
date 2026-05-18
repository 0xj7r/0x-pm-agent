//! Streaming reader for canonical v=1 `Event` records.
//!
//! Supports two layouts:
//! 1. **Parquet** (Phase 2 Glue output): `processed/v=1/dt=YYYY-MM-DD/
//!    market_type=*/event_type=*/<slug>/...parquet`. Each file is decoded via
//!    `parquet::arrow` into Arrow `RecordBatch`es and reconstituted into the
//!    `Event` struct.
//! 2. **JSON Lines** (Phase 1 raw / fixture-friendly): one canonical
//!    `Event` per line.
//!
//! The reader collects, de-duplicates, and sorts. The schema-contract
//! at-least-once guarantee means duplicates are common and must be removed
//! before a deterministic replay. Dedup key follows the contract:
//! `(event_type, market_slug, asset_id, sequence, received_ns)`.
//!
//! Sort order is `(received_ns, market_slug, asset_id, sequence,
//! event_type, source)` — stable across runs (unlike `HashMap` iteration).

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use arrow::array::{
    Array, BinaryArray, BooleanArray, Float32Array, Float64Array, Int32Array, Int64Array,
    LargeBinaryArray, LargeListArray, LargeStringArray, ListArray, StringArray, StructArray,
    UInt32Array, UInt64Array,
};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde::Deserialize;
use serde_json::{Map, Number, Value};
use walkdir::WalkDir;

use crate::collector::partition::event_type_str;
use crate::collector::schema::{Event, EventType, Source};

/// Tuple form of the dedup key: `(event_type, market_slug, asset_id, sequence,
/// received_ns)`. `Option<&str>` rather than `&str` so optional fields are
/// matched as-null, not as the empty string.
type DedupKey = (
    &'static str,
    Option<String>,
    Option<String>,
    Option<i64>,
    i64,
);

#[derive(Debug, Deserialize)]
struct EventHeader {
    v: u32,
    ts_ns: i64,
    received_ns: i64,
    event_type: EventType,
    market_type: String,
    market_slug: Option<String>,
    asset_id: Option<String>,
    side: Option<String>,
    price: Option<String>,
    size: Option<String>,
    sequence: Option<i64>,
    source: Source,
}

impl EventHeader {
    fn into_event(self, raw: Value) -> Event {
        Event {
            v: self.v,
            ts_ns: self.ts_ns,
            received_ns: self.received_ns,
            event_type: self.event_type,
            market_type: self.market_type,
            market_slug: self.market_slug,
            asset_id: self.asset_id,
            side: self.side,
            price: self.price,
            size: self.size,
            sequence: self.sequence,
            source: self.source,
            raw,
        }
    }
}

fn replay_requires_raw(event_type: EventType) -> bool {
    matches!(
        event_type,
        EventType::MarketMeta
            | EventType::PriceToBeat
            | EventType::Resolution
            | EventType::UserFill
            | EventType::UserOrder
    )
}

fn dedup_key(e: &Event) -> DedupKey {
    (
        event_type_str(e.event_type),
        e.market_slug.clone(),
        e.asset_id.clone(),
        e.sequence,
        e.received_ns,
    )
}

/// Stable sort key: `(received_ns, market_slug, asset_id, sequence,
/// event_type, source)`. The repeated tuple shape lets us delegate to
/// `Vec::sort_by_key` without writing a comparator.
fn sort_key(
    e: &Event,
) -> (
    i64,
    Option<String>,
    Option<String>,
    Option<i64>,
    &'static str,
    String,
) {
    (
        e.received_ns,
        e.market_slug.clone(),
        e.asset_id.clone(),
        e.sequence,
        event_type_str(e.event_type),
        format!("{:?}", e.source),
    )
}

/// Public reader entrypoint. `path` is a local directory; recursively walks
/// looking for `*.parquet` and `*.jsonl` files. Returns sorted, deduped events.
pub fn read_local(path: &Path) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        match p.extension().and_then(|e| e.to_str()) {
            Some("parquet") => events.extend(read_parquet_file(p)?),
            Some("jsonl") | Some("ndjson") => events.extend(read_jsonl_file(p)?),
            _ => {}
        }
    }
    Ok(dedupe_and_sort(events))
}

/// Apply the dedup-then-sort discipline mandated by the schema contract.
/// Public so callers that already have a `Vec<Event>` (e.g. from a fixture
/// builder or from the S3 reader) can normalize without rewalking the disk.
pub fn dedupe_and_sort(events: Vec<Event>) -> Vec<Event> {
    let mut seen: BTreeSet<DedupKey> = BTreeSet::new();
    let mut out: Vec<Event> = Vec::with_capacity(events.len());
    for e in events {
        let k = dedup_key(&e);
        if seen.insert(k) {
            out.push(e);
        }
    }
    out.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
    out
}

/// Parse a Hive-style partition value like `event_type=trade` from a path.
/// Returns the value if any path component starts with `key=`, else `None`.
fn partition_value(path: &Path, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    for component in path.components() {
        let s = component.as_os_str().to_string_lossy();
        if let Some(rest) = s.strip_prefix(&needle) {
            return Some(rest.to_string());
        }
    }
    None
}

/// Read a single Parquet file into `Vec<Event>`.
/// Expects the canonical v=1 schema columns; missing optional columns are
/// taken as null. The `raw` JSON column is read as either a UTF-8 string or a
/// binary blob — whatever Glue produced. Hive-partitioned outputs (the
/// `processed/v=1/dt=*/market_type=*/event_type=*/...` layout) typically omit
/// `event_type` and `market_type` from the row data; in that case we fall
/// back to parsing them out of the file path.
pub fn read_parquet_file(path: &Path) -> Result<Vec<Event>> {
    let file = File::open(path)
        .with_context(|| format!("failed to open parquet file {}", path.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .with_context(|| format!("failed to read parquet metadata {}", path.display()))?;
    let reader = builder
        .build()
        .with_context(|| format!("failed to build parquet record reader {}", path.display()))?;
    let event_type_partition = partition_value(path, "event_type");
    let market_type_partition = partition_value(path, "market_type");
    let mut events = Vec::new();
    for batch in reader {
        let batch = batch.with_context(|| format!("parquet batch read {}", path.display()))?;
        events.extend(record_batch_to_events(
            &batch,
            event_type_partition.as_deref(),
            market_type_partition.as_deref(),
        )?);
    }
    Ok(events)
}

fn record_batch_to_events(
    batch: &arrow::record_batch::RecordBatch,
    event_type_partition: Option<&str>,
    market_type_partition: Option<&str>,
) -> Result<Vec<Event>> {
    let schema = batch.schema();
    let col = |name: &str| schema.index_of(name).ok();
    let v_col = col("v");
    let ts_ns_col = col("ts_ns");
    let received_ns_col = col("received_ns");
    let event_type_col = col("event_type");
    if event_type_col.is_none() && event_type_partition.is_none() {
        return Err(anyhow::anyhow!("event_type column missing"));
    }
    let market_type_col = col("market_type");
    if market_type_col.is_none() && market_type_partition.is_none() {
        return Err(anyhow::anyhow!("market_type column missing"));
    }
    let market_slug_col = col("market_slug");
    let asset_id_col = col("asset_id");
    let side_col = col("side");
    let price_col = col("price");
    let size_col = col("size");
    let sequence_col = col("sequence");
    let source_col = col("source").context("source column missing")?;
    let raw_col = col("raw");

    let mut events = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        let v = v_col
            .and_then(|i| batch.column(i).as_any().downcast_ref::<UInt32Array>())
            .map(|a| a.value(row))
            .or_else(|| {
                v_col
                    .and_then(|i| batch.column(i).as_any().downcast_ref::<Int32Array>())
                    .map(|a| a.value(row) as u32)
            })
            .unwrap_or(1);
        let ts_ns = ts_ns_col
            .and_then(|i| batch.column(i).as_any().downcast_ref::<Int64Array>())
            .map(|a| a.value(row))
            .unwrap_or(0);
        let received_ns = received_ns_col
            .and_then(|i| batch.column(i).as_any().downcast_ref::<Int64Array>())
            .map(|a| a.value(row))
            .context("received_ns column required")?;
        let event_type_str = event_type_col
            .and_then(|i| string_at(batch, i, row))
            .or_else(|| event_type_partition.map(|s| s.to_string()))
            .context("event_type value required")?;
        let event_type = parse_event_type(&event_type_str)?;
        let market_type = market_type_col
            .and_then(|i| string_at(batch, i, row))
            .or_else(|| market_type_partition.map(|s| s.to_string()))
            .context("market_type value required")?;
        let market_slug = market_slug_col.and_then(|i| string_at(batch, i, row));
        let asset_id = asset_id_col.and_then(|i| string_at(batch, i, row));
        let side = side_col.and_then(|i| string_at(batch, i, row));
        let price = price_col.and_then(|i| string_at(batch, i, row));
        let size = size_col.and_then(|i| string_at(batch, i, row));
        let sequence = sequence_col
            .and_then(|i| batch.column(i).as_any().downcast_ref::<Int64Array>())
            .and_then(|a| {
                if a.is_null(row) {
                    None
                } else {
                    Some(a.value(row))
                }
            });
        let source_str = string_at(batch, source_col, row).context("source value required")?;
        let raw = if replay_requires_raw(event_type) {
            raw_col
                .map(|i| raw_value_at(batch.column(i).as_ref(), row))
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };

        events.push(Event {
            v,
            ts_ns,
            received_ns,
            event_type,
            market_type,
            market_slug,
            asset_id,
            side,
            price,
            size,
            sequence,
            source: parse_source(&source_str)?,
            raw,
        });
    }
    Ok(events)
}

fn raw_value_at(array: &dyn Array, row: usize) -> Value {
    if array.is_null(row) {
        return Value::Null;
    }
    if let Some(s) = string_scalar_at(array, row) {
        return serde_json::from_str::<Value>(&s).unwrap_or(Value::Null);
    }
    if let Some(bytes) = bytes_scalar_at(array, row) {
        let text = String::from_utf8_lossy(&bytes);
        return serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
    }
    arrow_value_at(array, row)
}

fn arrow_value_at(array: &dyn Array, row: usize) -> Value {
    if array.is_null(row) {
        return Value::Null;
    }
    if let Some(s) = string_scalar_at(array, row) {
        return Value::String(s);
    }
    if let Some(bytes) = bytes_scalar_at(array, row) {
        return Value::String(String::from_utf8_lossy(&bytes).into_owned());
    }
    if let Some(a) = array.as_any().downcast_ref::<BooleanArray>() {
        return Value::Bool(a.value(row));
    }
    if let Some(a) = array.as_any().downcast_ref::<Int32Array>() {
        return Value::Number(Number::from(a.value(row)));
    }
    if let Some(a) = array.as_any().downcast_ref::<Int64Array>() {
        return Value::Number(Number::from(a.value(row)));
    }
    if let Some(a) = array.as_any().downcast_ref::<UInt32Array>() {
        return Value::Number(Number::from(a.value(row)));
    }
    if let Some(a) = array.as_any().downcast_ref::<UInt64Array>() {
        return Value::Number(Number::from(a.value(row)));
    }
    if let Some(a) = array.as_any().downcast_ref::<Float32Array>() {
        return Number::from_f64(a.value(row) as f64)
            .map(Value::Number)
            .unwrap_or(Value::Null);
    }
    if let Some(a) = array.as_any().downcast_ref::<Float64Array>() {
        return Number::from_f64(a.value(row))
            .map(Value::Number)
            .unwrap_or(Value::Null);
    }
    if let Some(a) = array.as_any().downcast_ref::<StructArray>() {
        let mut object = Map::new();
        for (field, column) in a.fields().iter().zip(a.columns()) {
            object.insert(field.name().clone(), arrow_value_at(column.as_ref(), row));
        }
        return Value::Object(object);
    }
    if let Some(a) = array.as_any().downcast_ref::<ListArray>() {
        let values = a.value(row);
        return Value::Array(
            (0..values.len())
                .map(|idx| arrow_value_at(values.as_ref(), idx))
                .collect(),
        );
    }
    if let Some(a) = array.as_any().downcast_ref::<LargeListArray>() {
        let values = a.value(row);
        return Value::Array(
            (0..values.len())
                .map(|idx| arrow_value_at(values.as_ref(), idx))
                .collect(),
        );
    }
    Value::Null
}

fn string_scalar_at(array: &dyn Array, row: usize) -> Option<String> {
    if let Some(a) = array.as_any().downcast_ref::<StringArray>() {
        return Some(a.value(row).to_string());
    }
    if let Some(a) = array.as_any().downcast_ref::<LargeStringArray>() {
        return Some(a.value(row).to_string());
    }
    None
}

fn bytes_scalar_at(array: &dyn Array, row: usize) -> Option<Vec<u8>> {
    if let Some(a) = array.as_any().downcast_ref::<BinaryArray>() {
        return Some(a.value(row).to_vec());
    }
    if let Some(a) = array.as_any().downcast_ref::<LargeBinaryArray>() {
        return Some(a.value(row).to_vec());
    }
    None
}

fn string_at(batch: &arrow::record_batch::RecordBatch, col: usize, row: usize) -> Option<String> {
    let arr = batch.column(col);
    if arr.is_null(row) {
        return None;
    }
    if let Some(a) = arr.as_any().downcast_ref::<StringArray>() {
        return Some(a.value(row).to_string());
    }
    if let Some(a) = arr.as_any().downcast_ref::<LargeStringArray>() {
        return Some(a.value(row).to_string());
    }
    None
}

fn parse_event_type(s: &str) -> Result<EventType> {
    Ok(match s {
        "book_delta" => EventType::BookDelta,
        "book_snapshot" => EventType::BookSnapshot,
        "trade" => EventType::Trade,
        "user_fill" => EventType::UserFill,
        "user_order" => EventType::UserOrder,
        "btc_tick" => EventType::BtcTick,
        "market_meta" => EventType::MarketMeta,
        "heartbeat" => EventType::Heartbeat,
        "gap" => EventType::Gap,
        "price_to_beat" => EventType::PriceToBeat,
        "resolution" => EventType::Resolution,
        other => anyhow::bail!("unknown event_type {other}"),
    })
}

fn parse_source(s: &str) -> Result<Source> {
    Ok(match s {
        "polymarket_market_ws" => Source::PolymarketMarketWs,
        "polymarket_user_ws" => Source::PolymarketUserWs,
        "polymarket_data_api" => Source::PolymarketDataApi,
        "binance_aggtrade" => Source::BinanceAggtrade,
        "coinbase_match" => Source::CoinbaseMatch,
        "collector" => Source::Collector,
        "synthesizer" => Source::Synthesizer,
        other => anyhow::bail!("unknown source {other}"),
    })
}

/// Read a JSON-Lines file (one canonical `Event` per line). `.gz` is
/// auto-detected.
pub fn read_jsonl_file(path: &Path) -> Result<Vec<Event>> {
    let file = File::open(path)
        .with_context(|| format!("failed to open jsonl file {}", path.display()))?;
    let reader: Box<dyn Read> = if path.extension().and_then(|e| e.to_str()) == Some("gz") {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let buf = BufReader::new(reader);
    let mut events = Vec::new();
    for (idx, line) in buf.lines().enumerate() {
        let line = line
            .with_context(|| format!("failed to read jsonl {} line {}", path.display(), idx + 1))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let header: EventHeader = serde_json::from_str(trimmed).with_context(|| {
            format!("failed to parse jsonl {} line {}", path.display(), idx + 1)
        })?;
        if replay_requires_raw(header.event_type) {
            let event: Event = serde_json::from_str(trimmed).with_context(|| {
                format!(
                    "failed to parse raw jsonl {} line {}",
                    path.display(),
                    idx + 1
                )
            })?;
            events.push(event);
        } else {
            events.push(header.into_event(Value::Null));
        }
    }
    Ok(events)
}

/// Helper for tests / fixture pipelines: write a slice of Events to a JSONL
/// file. Not used in production; production reads only.
pub fn write_jsonl_file(path: &Path, events: &[Event]) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)
        .with_context(|| format!("failed to create jsonl {}", path.display()))?;
    for e in events {
        let line = serde_json::to_string(e)?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

/// Convenience for callers who have a directory under `<root>/dt=.../` and
/// want only files inside specific dt-partitions. Implemented as a thin
/// filter on top of `WalkDir` so that the walk order does not influence the
/// result (we always sort).
pub fn read_local_filtered(path: &Path, dt_filter: Option<&[String]>) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        if let Some(filter) = dt_filter {
            let matched = p.components().any(|c| {
                filter
                    .iter()
                    .any(|f| c.as_os_str() == format!("dt={f}").as_str())
            });
            if !matched {
                continue;
            }
        }
        match p.extension().and_then(|e| e.to_str()) {
            Some("parquet") => events.extend(read_parquet_file(p)?),
            Some("jsonl") | Some("ndjson") => events.extend(read_jsonl_file(p)?),
            Some("gz")
                if p.to_string_lossy().ends_with(".jsonl.gz")
                    || p.to_string_lossy().ends_with(".ndjson.gz") =>
            {
                events.extend(read_jsonl_file(p)?)
            }
            _ => {}
        }
    }
    Ok(dedupe_and_sort(events))
}

/// Public alias for downstream callers that already accept `PathBuf`.
pub fn read_path(path: PathBuf) -> Result<Vec<Event>> {
    read_local(&path)
}

/// Helper for tests / fixture pipelines: write a slice of Events to a Parquet
/// file using the canonical v=1 schema. Mirrors the column layout that
/// Glue/Firehose JSON->Parquet conversion produces.
#[cfg(test)]
pub fn write_parquet_file(path: &Path, events: &[Event]) -> Result<()> {
    use arrow::array::{Int64Array, StringArray, UInt32Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use std::sync::Arc;

    let schema = Arc::new(Schema::new(vec![
        Field::new("v", DataType::UInt32, false),
        Field::new("ts_ns", DataType::Int64, false),
        Field::new("received_ns", DataType::Int64, false),
        Field::new("event_type", DataType::Utf8, false),
        Field::new("market_type", DataType::Utf8, false),
        Field::new("market_slug", DataType::Utf8, true),
        Field::new("asset_id", DataType::Utf8, true),
        Field::new("side", DataType::Utf8, true),
        Field::new("price", DataType::Utf8, true),
        Field::new("size", DataType::Utf8, true),
        Field::new("sequence", DataType::Int64, true),
        Field::new("source", DataType::Utf8, false),
        Field::new("raw", DataType::Utf8, false),
    ]));

    let v: Vec<u32> = events.iter().map(|e| e.v).collect();
    let ts_ns: Vec<i64> = events.iter().map(|e| e.ts_ns).collect();
    let received_ns: Vec<i64> = events.iter().map(|e| e.received_ns).collect();
    let event_type: Vec<&str> = events
        .iter()
        .map(|e| event_type_str(e.event_type))
        .collect();
    let market_type: Vec<&str> = events.iter().map(|e| e.market_type.as_str()).collect();
    let market_slug: Vec<Option<&str>> = events.iter().map(|e| e.market_slug.as_deref()).collect();
    let asset_id: Vec<Option<&str>> = events.iter().map(|e| e.asset_id.as_deref()).collect();
    let side: Vec<Option<&str>> = events.iter().map(|e| e.side.as_deref()).collect();
    let price: Vec<Option<&str>> = events.iter().map(|e| e.price.as_deref()).collect();
    let size: Vec<Option<&str>> = events.iter().map(|e| e.size.as_deref()).collect();
    let sequence: Vec<Option<i64>> = events.iter().map(|e| e.sequence).collect();
    let source: Vec<String> = events
        .iter()
        .map(|e| {
            let s: serde_json::Value = serde_json::to_value(&e.source).unwrap();
            s.as_str().unwrap_or("").to_string()
        })
        .collect();
    let raw: Vec<String> = events
        .iter()
        .map(|e| serde_json::to_string(&e.raw).unwrap())
        .collect();

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt32Array::from(v)),
            Arc::new(Int64Array::from(ts_ns)),
            Arc::new(Int64Array::from(received_ns)),
            Arc::new(StringArray::from(event_type)),
            Arc::new(StringArray::from(market_type)),
            Arc::new(StringArray::from(market_slug)),
            Arc::new(StringArray::from(asset_id)),
            Arc::new(StringArray::from(side)),
            Arc::new(StringArray::from(price)),
            Arc::new(StringArray::from(size)),
            Arc::new(Int64Array::from(sequence)),
            Arc::new(StringArray::from(source)),
            Arc::new(StringArray::from(raw)),
        ],
    )?;

    let file = std::fs::File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, schema, None)?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn make_event(received_ns: i64, slug: &str, sequence: i64, et: EventType) -> Event {
        Event {
            v: 1,
            ts_ns: received_ns - 1,
            received_ns,
            event_type: et,
            market_type: "btc_5m".into(),
            market_slug: Some(slug.into()),
            asset_id: Some(format!("asset-{slug}")),
            side: Some("BUY".into()),
            price: Some("0.50".into()),
            size: Some("100".into()),
            sequence: Some(sequence),
            source: Source::PolymarketMarketWs,
            raw: json!({}),
        }
    }

    #[test]
    fn dedupe_keeps_first_drops_duplicates() {
        let a = make_event(100, "x", 1, EventType::BookDelta);
        let b = make_event(100, "x", 1, EventType::BookDelta);
        let c = make_event(100, "x", 2, EventType::BookDelta);
        let result = dedupe_and_sort(vec![a, b, c]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn sort_by_received_ns_then_tiebreak() {
        let a = make_event(200, "a", 1, EventType::Trade);
        let b = make_event(100, "z", 1, EventType::Trade);
        let c = make_event(100, "a", 2, EventType::BookDelta);
        let d = make_event(100, "a", 1, EventType::BookDelta);
        let result = dedupe_and_sort(vec![a, b, c, d]);
        assert_eq!(result[0].received_ns, 100);
        assert_eq!(result[0].market_slug.as_deref(), Some("a"));
        assert_eq!(result[0].sequence, Some(1));
        assert_eq!(result[1].sequence, Some(2));
        assert_eq!(result[2].market_slug.as_deref(), Some("z"));
        assert_eq!(result[3].received_ns, 200);
    }

    #[test]
    fn jsonl_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let events = vec![
            make_event(100, "x", 1, EventType::BookDelta),
            make_event(200, "x", 2, EventType::Trade),
        ];
        write_jsonl_file(&path, &events).unwrap();
        let read = read_jsonl_file(&path).unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].sequence, Some(1));
    }

    #[test]
    fn jsonl_reader_skips_raw_for_high_volume_replay_events() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let mut event = make_event(100, "x", 1, EventType::BookSnapshot);
        event.raw = json!({
            "book": {
                "bids": [{"price": "0.50", "size": "100"}],
                "asks": [{"price": "0.51", "size": "100"}]
            }
        });
        write_jsonl_file(&path, &[event]).unwrap();

        let read = read_jsonl_file(&path).unwrap();

        assert_eq!(read.len(), 1);
        assert_eq!(read[0].event_type, EventType::BookSnapshot);
        assert_eq!(read[0].raw, Value::Null);
    }

    #[test]
    fn jsonl_reader_preserves_raw_for_market_meta() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("meta.jsonl");
        let mut event = make_event(100, "x", 1, EventType::MarketMeta);
        event.raw = json!({
            "asset_ids": ["up-token", "down-token"],
            "strike": 100000.0,
            "end_time_ms": 1_777_788_300_000i64
        });
        write_jsonl_file(&path, &[event]).unwrap();

        let read = read_jsonl_file(&path).unwrap();

        assert_eq!(read.len(), 1);
        assert_eq!(
            read[0]
                .raw
                .get("asset_ids")
                .and_then(|v| v.as_array())
                .map(Vec::len),
            Some(2)
        );
        assert_eq!(
            read[0].raw.get("end_time_ms").and_then(|v| v.as_i64()),
            Some(1_777_788_300_000i64)
        );
    }

    #[test]
    fn raw_value_at_decodes_arrow_struct_metadata() {
        use arrow::array::{ArrayRef, ListBuilder, StringBuilder, StructArray};
        use arrow::datatypes::{DataType, Field, Fields};
        use arrow::record_batch::RecordBatch;
        use std::sync::Arc;

        let mut assets = ListBuilder::new(StringBuilder::new());
        assets.values().append_value("up-token");
        assets.values().append_value("down-token");
        assets.append(true);
        let raw = StructArray::from(vec![
            (
                Arc::new(Field::new(
                    "asset_ids",
                    DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
                    true,
                )),
                Arc::new(assets.finish()) as ArrayRef,
            ),
            (
                Arc::new(Field::new("start_time_ms", DataType::Int64, true)),
                Arc::new(Int64Array::from(vec![Some(1_778_617_800_000i64)])) as ArrayRef,
            ),
            (
                Arc::new(Field::new("source", DataType::Utf8, true)),
                Arc::new(StringArray::from(vec![Some("telonex_markets")])) as ArrayRef,
            ),
        ]);
        let schema = Arc::new(arrow::datatypes::Schema::new(vec![Field::new(
            "raw",
            DataType::Struct(Fields::from(vec![
                Field::new(
                    "asset_ids",
                    DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
                    true,
                ),
                Field::new("start_time_ms", DataType::Int64, true),
                Field::new("source", DataType::Utf8, true),
            ])),
            true,
        )]));
        let batch = RecordBatch::try_new(schema, vec![Arc::new(raw)]).unwrap();

        let value = raw_value_at(batch.column(0).as_ref(), 0);

        assert_eq!(
            value.get("asset_ids").and_then(|value| value.as_array()),
            Some(&vec![
                Value::String("up-token".into()),
                Value::String("down-token".into())
            ])
        );
        assert_eq!(
            value.get("start_time_ms").and_then(|value| value.as_i64()),
            Some(1_778_617_800_000i64)
        );
        assert_eq!(
            value.get("source").and_then(|value| value.as_str()),
            Some("telonex_markets")
        );
    }

    #[test]
    fn parquet_reader_skips_raw_for_high_volume_replay_events() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("evts.parquet");
        let mut event = make_event(100, "x", 1, EventType::Trade);
        event.raw = json!({"large_raw_payload": [{"n": 1}, {"n": 2}]});
        write_parquet_file(&path, &[event]).unwrap();

        let read = read_parquet_file(&path).unwrap();

        assert_eq!(read.len(), 1);
        assert_eq!(read[0].event_type, EventType::Trade);
        assert_eq!(read[0].raw, Value::Null);
    }

    #[test]
    fn read_local_walks_subdirs_and_dedupes_across_files() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("dt=2026-04-01");
        std::fs::create_dir_all(&sub).unwrap();
        let f1 = sub.join("a.jsonl");
        let f2 = sub.join("b.jsonl");
        write_jsonl_file(&f1, &[make_event(100, "x", 1, EventType::BookDelta)]).unwrap();
        write_jsonl_file(
            &f2,
            &[
                make_event(100, "x", 1, EventType::BookDelta),
                make_event(150, "x", 2, EventType::Trade),
            ],
        )
        .unwrap();
        let result = read_local(dir.path()).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn parquet_roundtrip_preserves_canonical_event() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("evts.parquet");
        let mut events = vec![
            make_event(100, "x", 1, EventType::BookDelta),
            make_event(200, "x", 2, EventType::Trade),
        ];
        // Inject a heartbeat with all-null optionals to verify nullable handling.
        events.push(Event {
            v: 1,
            ts_ns: 0,
            received_ns: 50,
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
        });
        write_parquet_file(&path, &events).unwrap();
        let read = read_parquet_file(&path).unwrap();
        assert_eq!(read.len(), 3);
        // After read_local sort, heartbeat (received_ns=50) is first.
        let read_sorted = dedupe_and_sort(read);
        assert_eq!(read_sorted[0].event_type, EventType::Heartbeat);
        assert_eq!(read_sorted[0].market_slug, None);
    }

    #[test]
    fn dedupe_with_intentional_duplicates_across_three_cells() {
        // Schema-contract test: at-least-once duplicates must be removed.
        let mut events = Vec::new();
        for slug in ["a", "b", "c"] {
            for seq in 0..30 {
                let e = make_event(1_000 + seq * 10, slug, seq, EventType::BookDelta);
                events.push(e.clone());
                if seq % 5 == 0 {
                    events.push(e); // duplicate every 5th
                }
            }
        }
        let len_before = events.len();
        let result = dedupe_and_sort(events);
        assert!(result.len() < len_before);
        assert_eq!(result.len(), 90); // 3 cells * 30 unique
                                      // Sorted ascending by received_ns
        for w in result.windows(2) {
            assert!(w[0].received_ns <= w[1].received_ns);
        }
    }
}
