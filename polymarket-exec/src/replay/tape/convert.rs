use crate::replay::tape::format::{
    btc_price_to_cents, price_to_ticks, size_to_lots, BookEventV1, BtcTickV1, TradeEventV1,
    EVENT_DELETE, EVENT_UPDATE, LEG_NO, LEG_YES, SIDE_ASK, SIDE_BID, TAKER_BUY, TAKER_SELL,
};
use crate::replay::tape::writer::write_tape_file;
use anyhow::{Context, Result};
use arrow::array::{Array, Float64Array, Int64Array, LargeStringArray, StringArray, UInt64Array};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct TapeConvertOptions {
    pub input_prefix: PathBuf,
    pub output_dir: PathBuf,
    pub market_slug: String,
    pub yes_asset_id: String,
    pub no_asset_id: String,
    pub window_start_ns: i64,
    pub window_end_ns: i64,
    pub max_book_levels: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TapeConvertSummary {
    pub book_events: usize,
    pub trade_events: usize,
    pub btc_ticks: usize,
    pub book_path: PathBuf,
    pub trades_path: PathBuf,
    pub btc_path: PathBuf,
}

pub fn convert_raw_prefix(options: &TapeConvertOptions) -> Result<TapeConvertSummary> {
    let mut book_events = Vec::new();
    let mut trade_events = Vec::new();
    let mut btc_ticks = Vec::new();
    let mut book_state = BookDiffState::default();
    let filter = RawTapeFileFilter::new(options);

    for entry in WalkDir::new(&options.input_prefix)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        if !entry.file_type().is_file() || !filter.should_read(entry.path()) {
            continue;
        }
        read_raw_file(
            entry.path(),
            options,
            &mut book_state,
            &mut book_events,
            &mut trade_events,
            &mut btc_ticks,
        )?;
    }

    book_events.sort_by_key(|event| event.ts_ns);
    trade_events.sort_by_key(|event| event.ts_ns);
    btc_ticks.sort_by_key(|event| event.ts_ns);

    std::fs::create_dir_all(&options.output_dir)
        .with_context(|| format!("creating tape output dir {}", options.output_dir.display()))?;
    let book_path = options.output_dir.join("book.bin");
    let trades_path = options.output_dir.join("trades.bin");
    let btc_path = options.output_dir.join("btc.bin");

    write_tape_file(&book_path, &options.market_slug, &book_events)?;
    write_tape_file(&trades_path, &options.market_slug, &trade_events)?;
    write_tape_file(&btc_path, "BTCUSDT", &btc_ticks)?;

    Ok(TapeConvertSummary {
        book_events: book_events.len(),
        trade_events: trade_events.len(),
        btc_ticks: btc_ticks.len(),
        book_path,
        trades_path,
        btc_path,
    })
}

struct RawTapeFileFilter {
    start_date: String,
    end_date: String,
    asset_ids: BTreeSet<String>,
    book_channel: &'static str,
}

impl RawTapeFileFilter {
    fn new(options: &TapeConvertOptions) -> Self {
        Self {
            start_date: date_partition(options.window_start_ns),
            end_date: date_partition(options.window_end_ns.saturating_sub(1)),
            asset_ids: [options.yes_asset_id.clone(), options.no_asset_id.clone()]
                .into_iter()
                .collect(),
            book_channel: selected_book_channel(options.max_book_levels),
        }
    }

    fn should_read(&self, path: &Path) -> bool {
        if path.extension().and_then(|ext| ext.to_str()) != Some("parquet") {
            return false;
        }
        if let Some(date) = partition_value(path, "date").or_else(|| partition_value(path, "dt")) {
            if date < self.start_date || date > self.end_date {
                return false;
            }
        }
        if let Some(asset_id) =
            partition_value(path, "asset_id").or_else(|| partition_value(path, "token_id"))
        {
            if !self.asset_ids.contains(&asset_id) {
                return false;
            }
        }

        let exchange = partition_value(path, "exchange");
        let channel = partition_value(path, "channel").or_else(|| partition_value(path, "dataset"));
        matches!(
            (exchange.as_deref(), channel.as_deref()),
            (Some("polymarket"), Some("trades"))
                | (Some("polymarket"), Some("quotes"))
                | (Some("binance"), Some("agg_trades"))
                | (Some("binance"), Some("trades"))
        ) || matches!(
            (exchange.as_deref(), channel.as_deref()),
            (Some("polymarket"), Some(channel)) if channel == self.book_channel
        )
    }
}

fn read_raw_file(
    path: &Path,
    options: &TapeConvertOptions,
    book_state: &mut BookDiffState,
    book_events: &mut Vec<BookEventV1>,
    trade_events: &mut Vec<TradeEventV1>,
    btc_ticks: &mut Vec<BtcTickV1>,
) -> Result<()> {
    let exchange = partition_value(path, "exchange");
    let channel = partition_value(path, "channel").or_else(|| partition_value(path, "dataset"));
    let Some(channel) = channel else {
        return Ok(());
    };

    let file = File::open(path).with_context(|| format!("opening raw parquet {}", path.display()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .with_context(|| format!("reading parquet metadata {}", path.display()))?
        .with_batch_size(65_536)
        .build()
        .with_context(|| format!("building parquet reader {}", path.display()))?;

    for batch in reader {
        let batch = batch.with_context(|| format!("reading parquet batch {}", path.display()))?;
        for row in 0..batch.num_rows() {
            let Some(received_ns) = row_received_ns(&batch, row) else {
                continue;
            };
            if received_ns < options.window_start_ns || received_ns >= options.window_end_ns {
                continue;
            }

            match (exchange.as_deref(), channel.as_str()) {
                (Some("polymarket"), "trades") => {
                    if let Some(event) = trade_event(path, &batch, row, received_ns, options)? {
                        trade_events.push(event);
                    }
                }
                (Some("polymarket"), "quotes") => {
                    quote_events(path, &batch, row, received_ns, options, book_events)?;
                }
                (Some("polymarket"), "book_snapshot_5")
                | (Some("polymarket"), "book_snapshot_25")
                | (Some("polymarket"), "book_snapshot_full") => book_snapshot_events(
                    path,
                    &batch,
                    row,
                    received_ns,
                    options,
                    book_state,
                    book_events,
                )?,
                (Some("binance"), "agg_trades") | (Some("binance"), "trades") => {
                    if let Some(tick) = btc_tick(&batch, row, received_ns)? {
                        btc_ticks.push(tick);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn quote_events(
    path: &Path,
    batch: &RecordBatch,
    row: usize,
    ts_ns: i64,
    options: &TapeConvertOptions,
    out: &mut Vec<BookEventV1>,
) -> Result<()> {
    let Some(leg) = row_leg(path, batch, row, options) else {
        return Ok(());
    };
    if let (Some(price), Some(size)) = (
        value_f64(batch, "bid_price", row).or_else(|| value_f64(batch, "bid_price_0", row)),
        value_f64(batch, "bid_size", row).or_else(|| value_f64(batch, "bid_size_0", row)),
    ) {
        out.push(book_event(ts_ns, leg, SIDE_BID, price, size)?);
    }
    if let (Some(price), Some(size)) = (
        value_f64(batch, "ask_price", row).or_else(|| value_f64(batch, "ask_price_0", row)),
        value_f64(batch, "ask_size", row).or_else(|| value_f64(batch, "ask_size_0", row)),
    ) {
        out.push(book_event(ts_ns, leg, SIDE_ASK, price, size)?);
    }
    Ok(())
}

fn book_snapshot_events(
    path: &Path,
    batch: &RecordBatch,
    row: usize,
    ts_ns: i64,
    options: &TapeConvertOptions,
    book_state: &mut BookDiffState,
    out: &mut Vec<BookEventV1>,
) -> Result<()> {
    let Some(leg) = row_leg(path, batch, row, options) else {
        return Ok(());
    };
    let asset_id = row_asset_id(path, batch, row).unwrap_or_default();
    for (side, prefix) in [(SIDE_BID, "bid"), (SIDE_ASK, "ask")] {
        let levels = flattened_levels(batch, row, prefix, options.max_book_levels);
        for (price, size) in book_state.diff(&asset_id, side, levels) {
            out.push(book_event(ts_ns, leg, side, price, size)?);
        }
    }
    Ok(())
}

fn trade_event(
    path: &Path,
    batch: &RecordBatch,
    row: usize,
    ts_ns: i64,
    options: &TapeConvertOptions,
) -> Result<Option<TradeEventV1>> {
    let Some(leg) = row_leg(path, batch, row, options) else {
        return Ok(None);
    };
    let Some(price) = value_f64(batch, "price", row)
        .or_else(|| value_f64(batch, "last_trade_price", row))
    else {
        return Ok(None);
    };
    let Some(size) = value_f64(batch, "size", row)
        .or_else(|| value_f64(batch, "quantity", row))
        .or_else(|| value_f64(batch, "amount", row))
    else {
        return Ok(None);
    };
    let taker_side = match row_side(batch, row).as_deref() {
        Some("sell") | Some("ask") => TAKER_SELL,
        _ => TAKER_BUY,
    };
    Ok(Some(TradeEventV1 {
        ts_ns: ts_ns as u64,
        price_ticks: price_to_ticks(price)?,
        size_lots: size_to_lots(size)?,
        leg,
        taker_side,
        _pad: [0; 6],
    }))
}

fn btc_tick(batch: &RecordBatch, row: usize, ts_ns: i64) -> Result<Option<BtcTickV1>> {
    let Some(price) = value_f64(batch, "price", row).or_else(|| value_f64(batch, "last_price", row))
    else {
        return Ok(None);
    };
    let qty = value_f64(batch, "quantity", row)
        .or_else(|| value_f64(batch, "qty", row))
        .unwrap_or(0.0);
    Ok(Some(BtcTickV1 {
        ts_ns: ts_ns as u64,
        price_cents: btc_price_to_cents(price)?,
        qty_lots: size_to_lots(qty).unwrap_or(0),
        _pad: [0; 4],
    }))
}

fn book_event(ts_ns: i64, leg: u8, side: u8, price: f64, size: f64) -> Result<BookEventV1> {
    let size_lots = size_to_lots(size)?;
    Ok(BookEventV1 {
        ts_ns: ts_ns as u64,
        price_ticks: price_to_ticks(price)?,
        size_lots,
        leg,
        side,
        event_type: if size_lots == 0 {
            EVENT_DELETE
        } else {
            EVENT_UPDATE
        },
        _pad: [0; 5],
    })
}

#[derive(Default)]
struct BookDiffState {
    levels: BTreeMap<(String, u8), BTreeMap<String, String>>,
}

impl BookDiffState {
    fn diff(&mut self, asset_id: &str, side: u8, levels: Vec<(String, String)>) -> Vec<(f64, f64)> {
        let key = (asset_id.to_string(), side);
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
            let size = new.cloned().unwrap_or_else(|| "0".to_string());
            if let (Ok(price), Ok(size)) = (price.parse::<f64>(), size.parse::<f64>()) {
                out.push((price, size));
            }
        }
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        self.levels.insert(key, current);
        out
    }
}

fn flattened_levels(
    batch: &RecordBatch,
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

fn row_leg(
    path: &Path,
    batch: &RecordBatch,
    row: usize,
    options: &TapeConvertOptions,
) -> Option<u8> {
    let asset_id = row_asset_id(path, batch, row)?;
    if asset_id == options.yes_asset_id {
        Some(LEG_YES)
    } else if asset_id == options.no_asset_id {
        Some(LEG_NO)
    } else {
        None
    }
}

fn row_asset_id(path: &Path, batch: &RecordBatch, row: usize) -> Option<String> {
    value_string(batch, "asset_id", row)
        .or_else(|| value_string(batch, "token_id", row))
        .or_else(|| value_string(batch, "asset", row))
        .or_else(|| partition_value(path, "asset_id"))
        .or_else(|| partition_value(path, "token_id"))
}

fn row_side(batch: &RecordBatch, row: usize) -> Option<String> {
    value_string(batch, "side", row)
        .or_else(|| value_string(batch, "trader_side", row))
        .map(|side| side.to_ascii_lowercase())
}

fn row_received_ns(batch: &RecordBatch, row: usize) -> Option<i64> {
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

fn timestamp_to_ns(value: i64) -> i64 {
    if value > 10_000_000_000_000 {
        value * 1_000
    } else if value > 10_000_000_000 {
        value * 1_000_000
    } else {
        value * 1_000_000_000
    }
}

fn value_f64(batch: &RecordBatch, name: &str, row: usize) -> Option<f64> {
    let idx = batch.schema().index_of(name).ok()?;
    let arr = batch.column(idx);
    if arr.is_null(row) {
        return None;
    }
    if let Some(a) = arr.as_any().downcast_ref::<Float64Array>() {
        return Some(a.value(row));
    }
    value_string(batch, name, row).and_then(|value| value.parse::<f64>().ok())
}

fn value_i64(batch: &RecordBatch, name: &str, row: usize) -> Option<i64> {
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

fn value_string(batch: &RecordBatch, name: &str, row: usize) -> Option<String> {
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
    let seconds = ts_ns / 1_000_000_000;
    let days = seconds.div_euclid(86_400);
    civil_from_days(days)
}

fn civil_from_days(days_since_epoch: i64) -> String {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    format!("{year:04}-{m:02}-{d:02}")
}

fn partition_value(path: &Path, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    path.components().find_map(|part| {
        let text = part.as_os_str().to_string_lossy();
        text.strip_prefix(&prefix).map(ToString::to_string)
    })
}
