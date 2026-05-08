use crate::collector::schema::{Event, EventType, Source};
use crate::replay::raw_parquet::RawReplayMarket;
use crate::replay::reader::dedupe_and_sort;
use crate::replay::tape::format::{
    lots_to_size, ticks_to_price, BookEventV1, BtcTickV1, TradeEventV1, LEG_NO, LEG_YES, SIDE_ASK,
    SIDE_BID, TAKER_BUY, TAKER_SELL,
};
use crate::replay::tape::reader::MappedTape;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct TapeReplayOptions {
    pub input_prefix: PathBuf,
    pub window_start_ns: i64,
    pub window_end_ns: i64,
    pub market_filter: String,
    pub markets: Vec<RawReplayMarket>,
}

pub fn read_tape_replay(options: &TapeReplayOptions) -> Result<Vec<Event>> {
    ensure!(!options.markets.is_empty(), "missing tape replay markets");
    let mut events = Vec::new();
    for (idx, market) in options.markets.iter().enumerate() {
        let market_prefix = tape_market_prefix(options, market);
        events.extend(read_market_tape_events(
            options,
            market,
            idx,
            &market_prefix,
        )?);
    }

    Ok(dedupe_and_sort(events))
}

fn read_market_tape_events(
    options: &TapeReplayOptions,
    market: &RawReplayMarket,
    market_index: usize,
    market_prefix: &PathBuf,
) -> Result<Vec<Event>> {
    let book_path = market_prefix.join("book.bin");
    let trades_path = market_prefix.join("trades.bin");
    let btc_path = market_prefix.join("btc.bin");

    let book = MappedTape::<BookEventV1>::open(&book_path)
        .with_context(|| format!("opening book tape {}", book_path.display()))?;
    let trades = MappedTape::<TradeEventV1>::open(&trades_path)
        .with_context(|| format!("opening trades tape {}", trades_path.display()))?;
    let btc = MappedTape::<BtcTickV1>::open(&btc_path)
        .with_context(|| format!("opening BTC tape {}", btc_path.display()))?;

    let mut events =
        Vec::with_capacity(book.records().len() + trades.records().len() + btc.records().len() + 1);
    events.push(market_meta_event(
        options,
        market,
        market_index,
        btc.records(),
    ));

    for (idx, event) in book.records().iter().enumerate() {
        if !in_window(event.ts_ns, options) {
            continue;
        }
        events.push(book_event(event, market, idx as i64));
    }
    for (idx, event) in trades.records().iter().enumerate() {
        if !in_window(event.ts_ns, options) {
            continue;
        }
        events.push(trade_event(event, market, idx as i64));
    }
    for (idx, tick) in btc.records().iter().enumerate() {
        if !in_window(tick.ts_ns, options) {
            continue;
        }
        events.push(btc_event(tick, market, idx as i64));
    }

    Ok(events)
}

fn tape_market_prefix(options: &TapeReplayOptions, market: &RawReplayMarket) -> PathBuf {
    let direct_book = options.input_prefix.join("book.bin");
    if options.markets.len() == 1 && direct_book.exists() {
        options.input_prefix.clone()
    } else {
        options.input_prefix.join(&market.slug)
    }
}

fn market_meta_event(
    options: &TapeReplayOptions,
    market: &RawReplayMarket,
    market_index: usize,
    btc_ticks: &[BtcTickV1],
) -> Event {
    let start_ms = market_start_ms(&market.slug).unwrap_or(options.window_start_ns / 1_000_000);
    let end_ms = market_end_ms(&market.slug).unwrap_or(options.window_end_ns / 1_000_000);
    let start_ns = start_ms.saturating_mul(1_000_000);
    let mut raw = json!({
        "slug": market.slug,
        "market_type": market_type_from_slug(&market.slug),
        "asset_ids": market.asset_ids,
        "outcomes": ["Up", "Down"],
        "start_time_ms": start_ms,
        "end_time_ms": end_ms,
        "source": "packed_tape_replay_market_meta"
    });
    if let Some((strike, source)) = market_strike(market, start_ns, btc_ticks) {
        raw["strike"] = json!(strike);
        raw["strike_source"] = json!(source);
    }
    Event {
        v: 1,
        ts_ns: start_ns,
        received_ns: start_ns,
        event_type: EventType::MarketMeta,
        market_type: market_type_from_slug(&market.slug),
        market_slug: Some(market.slug.clone()),
        asset_id: None,
        side: None,
        price: None,
        size: None,
        sequence: Some(-10_000 + market_index as i64),
        source: Source::PolymarketDataApi,
        raw,
    }
}

fn market_strike(
    market: &RawReplayMarket,
    market_start_ns: i64,
    btc_ticks: &[BtcTickV1],
) -> Option<(f64, &'static str)> {
    if let Some(strike) = market
        .strike
        .as_deref()
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
    {
        return Some((strike, "open_price_asset_map"));
    }

    let start_ns = u64::try_from(market_start_ns).ok()?;
    let index = btc_ticks.partition_point(|tick| tick.ts_ns < start_ns);
    let tick = btc_ticks.get(index).or_else(|| btc_ticks.last())?;
    Some((
        tick.price_cents as f64 / 100.0,
        "binance_first_tick_at_or_after_market_start",
    ))
}

fn book_event(event: &BookEventV1, market: &RawReplayMarket, sequence: i64) -> Event {
    Event {
        v: 1,
        ts_ns: event.ts_ns as i64,
        received_ns: event.ts_ns as i64,
        event_type: EventType::BookDelta,
        market_type: market_type_from_slug(&market.slug),
        market_slug: Some(market.slug.clone()),
        asset_id: asset_id_for_leg(event.leg, market),
        side: side_name(event.side).map(ToString::to_string),
        price: Some(ticks_to_price(event.price_ticks).to_string()),
        size: Some(lots_to_size(event.size_lots).to_string()),
        sequence: Some(sequence),
        source: Source::Collector,
        raw: Value::Null,
    }
}

fn trade_event(event: &TradeEventV1, market: &RawReplayMarket, sequence: i64) -> Event {
    Event {
        v: 1,
        ts_ns: event.ts_ns as i64,
        received_ns: event.ts_ns as i64,
        event_type: EventType::Trade,
        market_type: market_type_from_slug(&market.slug),
        market_slug: Some(market.slug.clone()),
        asset_id: asset_id_for_leg(event.leg, market),
        side: taker_side_name(event.taker_side).map(ToString::to_string),
        price: Some(ticks_to_price(event.price_ticks).to_string()),
        size: Some(lots_to_size(event.size_lots).to_string()),
        sequence: Some(sequence),
        source: Source::Collector,
        raw: Value::Null,
    }
}

fn btc_event(tick: &BtcTickV1, market: &RawReplayMarket, sequence: i64) -> Event {
    Event {
        v: 1,
        ts_ns: tick.ts_ns as i64,
        received_ns: tick.ts_ns as i64,
        event_type: EventType::BtcTick,
        market_type: "btc_ref".to_string(),
        market_slug: Some(market.slug.clone()),
        asset_id: Some("BTC".to_string()),
        side: None,
        price: Some((tick.price_cents as f64 / 100.0).to_string()),
        size: None,
        sequence: Some(sequence),
        source: Source::BinanceAggtrade,
        raw: Value::Null,
    }
}

fn in_window(ts_ns: u64, options: &TapeReplayOptions) -> bool {
    let ts_ns = ts_ns as i64;
    ts_ns >= options.window_start_ns && ts_ns < options.window_end_ns
}

fn asset_id_for_leg(leg: u8, market: &RawReplayMarket) -> Option<String> {
    match leg {
        LEG_YES => Some(market.asset_ids[0].clone()),
        LEG_NO => Some(market.asset_ids[1].clone()),
        _ => None,
    }
}

fn side_name(side: u8) -> Option<&'static str> {
    match side {
        SIDE_BID => Some("buy"),
        SIDE_ASK => Some("sell"),
        _ => None,
    }
}

fn taker_side_name(side: u8) -> Option<&'static str> {
    match side {
        TAKER_BUY => Some("buy"),
        TAKER_SELL => Some("sell"),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::tape::format::{EVENT_UPDATE, TAKER_BUY, TAKER_SELL};
    use crate::replay::tape::writer::write_tape_file;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn market() -> RawReplayMarket {
        RawReplayMarket {
            slug: "btc-updown-5m-1771119900".to_string(),
            asset_ids: ["UP".to_string(), "DOWN".to_string()],
            strike: Some("70000.00".to_string()),
        }
    }

    fn tape_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "packed-tape-reduction-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn write_tape_fixture(
        dir: &PathBuf,
        book: &[BookEventV1],
        trades: &[TradeEventV1],
        btc: &[BtcTickV1],
    ) {
        let market = market();
        write_tape_file(dir.join("book.bin"), &market.slug, book).unwrap();
        write_tape_file(dir.join("trades.bin"), &market.slug, trades).unwrap();
        write_tape_file(dir.join("btc.bin"), &market.slug, btc).unwrap();
    }

    fn replay_tape(dir: PathBuf) -> Vec<Event> {
        read_tape_replay(&TapeReplayOptions {
            input_prefix: dir,
            window_start_ns: 10,
            window_end_ns: 20,
            market_filter: "btc_5m".to_string(),
            markets: vec![market()],
        })
        .unwrap()
    }

    fn book_update(ts_ns: u64, price_ticks: u32, size_lots: u32) -> BookEventV1 {
        BookEventV1 {
            ts_ns,
            price_ticks,
            size_lots,
            leg: LEG_YES,
            side: SIDE_BID,
            event_type: EVENT_UPDATE,
            _pad: [0; 5],
        }
    }

    #[test]
    fn tape_trade_preserves_taker_side_for_fill_sim() {
        let buy = trade_event(
            &TradeEventV1 {
                ts_ns: 1,
                price_ticks: 5_000,
                size_lots: 100,
                leg: LEG_YES,
                taker_side: TAKER_BUY,
                _pad: [0; 6],
            },
            &market(),
            7,
        );
        assert_eq!(buy.side.as_deref(), Some("buy"));

        let sell = trade_event(
            &TradeEventV1 {
                ts_ns: 2,
                price_ticks: 4_900,
                size_lots: 200,
                leg: LEG_NO,
                taker_side: TAKER_SELL,
                _pad: [0; 6],
            },
            &market(),
            8,
        );
        assert_eq!(sell.side.as_deref(), Some("sell"));
    }

    #[test]
    fn packed_tape_replay_preserves_all_book_deltas() {
        let dir = tape_dir("preserves-all");
        write_tape_fixture(
            &dir,
            &[
                book_update(5, 5_000, 100),
                book_update(11, 4_900, 75),
                book_update(12, 5_000, 125),
            ],
            &[],
            &[],
        );

        let events = replay_tape(dir.clone());
        let book_events: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == EventType::BookDelta)
            .collect();

        assert_eq!(
            book_events
                .iter()
                .map(|event| event.price.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["0.49", "0.5"]
        );
        assert!(book_events.iter().all(|event| event.raw == Value::Null));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn packed_tape_replay_preserves_top_book_changes() {
        let dir = tape_dir("preserves-top");
        write_tape_fixture(
            &dir,
            &[book_update(10, 5_000, 100), book_update(11, 5_100, 50)],
            &[],
            &[],
        );

        let events = replay_tape(dir.clone());
        let book_prices: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == EventType::BookDelta)
            .map(|event| event.price.as_deref().unwrap())
            .collect();

        assert_eq!(book_prices, vec!["0.5", "0.51"]);
        assert!(events
            .iter()
            .filter(|event| event.event_type == EventType::BookDelta)
            .all(|event| event.raw == Value::Null));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn packed_tape_replay_preserves_trades_btc_and_settlement_market_meta() {
        let dir = tape_dir("preserves-non-book");
        write_tape_fixture(
            &dir,
            &[book_update(5, 5_000, 100), book_update(11, 4_900, 75)],
            &[TradeEventV1 {
                ts_ns: 11,
                price_ticks: 4_900,
                size_lots: 200,
                leg: LEG_NO,
                taker_side: TAKER_SELL,
                _pad: [0; 6],
            }],
            &[BtcTickV1 {
                ts_ns: 12,
                price_cents: 7_012_345,
                qty_lots: 1,
                _pad: [0; 4],
            }],
        );

        let events = replay_tape(dir.clone());

        let book_events = events
            .iter()
            .filter(|event| event.event_type == EventType::BookDelta)
            .collect::<Vec<_>>();
        assert_eq!(book_events.len(), 1);
        assert_eq!(book_events[0].raw, Value::Null);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == EventType::Trade)
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == EventType::BtcTick)
                .count(),
            1
        );
        let meta = events
            .iter()
            .find(|event| event.event_type == EventType::MarketMeta)
            .unwrap();
        assert_eq!(
            meta.raw.get("asset_ids").and_then(|value| value.as_array()).unwrap().len(),
            2
        );
        assert_eq!(
            meta.raw.get("strike").and_then(|value| value.as_f64()),
            Some(70_000.0)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tape_market_meta_prefers_explicit_open_price() {
        let options = TapeReplayOptions {
            input_prefix: PathBuf::from("/tmp/unused"),
            window_start_ns: 1_771_119_900_000_000_000,
            window_end_ns: 1_771_120_200_000_000_000,
            market_filter: "btc_5m".to_string(),
            markets: vec![market()],
        };
        let event = market_meta_event(&options, &market(), 0, &[]);

        assert_eq!(event.received_ns, 1_771_119_900_000_000_000);
        assert_eq!(
            event.raw.get("strike").and_then(|value| value.as_f64()),
            Some(70_000.0)
        );
        assert_eq!(
            event
                .raw
                .get("strike_source")
                .and_then(|value| value.as_str()),
            Some("open_price_asset_map")
        );
    }

    #[test]
    fn tape_market_meta_derives_open_price_from_owning_btc_tape_when_missing() {
        let options = TapeReplayOptions {
            input_prefix: PathBuf::from("/tmp/unused"),
            window_start_ns: 1_771_119_900_000_000_000,
            window_end_ns: 1_771_120_200_000_000_000,
            market_filter: "btc_5m".to_string(),
            markets: vec![RawReplayMarket {
                slug: "btc-updown-5m-1771119900".to_string(),
                asset_ids: ["UP".to_string(), "DOWN".to_string()],
                strike: None,
            }],
        };
        let btc_ticks = [
            BtcTickV1 {
                ts_ns: 1_771_119_899_999_000_000,
                price_cents: 6_999_999,
                qty_lots: 1,
                _pad: [0; 4],
            },
            BtcTickV1 {
                ts_ns: 1_771_119_900_001_000_000,
                price_cents: 7_012_345,
                qty_lots: 1,
                _pad: [0; 4],
            },
        ];

        let event = market_meta_event(&options, &options.markets[0], 0, &btc_ticks);

        assert_eq!(
            event.raw.get("strike").and_then(|value| value.as_f64()),
            Some(70_123.45)
        );
        assert_eq!(
            event
                .raw
                .get("strike_source")
                .and_then(|value| value.as_str()),
            Some("binance_first_tick_at_or_after_market_start")
        );
    }

    #[test]
    fn tape_market_prefix_supports_single_and_market_root_layouts() {
        let base = std::env::temp_dir().join(format!(
            "tape-market-prefix-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("book.bin"), []).unwrap();
        let single = TapeReplayOptions {
            input_prefix: base.clone(),
            window_start_ns: 0,
            window_end_ns: 1,
            market_filter: "btc_5m".to_string(),
            markets: vec![market()],
        };
        assert_eq!(tape_market_prefix(&single, &single.markets[0]), base);

        let root = std::env::temp_dir().join(format!(
            "tape-market-root-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let multi = TapeReplayOptions {
            input_prefix: root.clone(),
            window_start_ns: 0,
            window_end_ns: 1,
            market_filter: "btc_5m".to_string(),
            markets: vec![market(), market()],
        };
        assert_eq!(
            tape_market_prefix(&multi, &multi.markets[0]),
            root.join("btc-updown-5m-1771119900")
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
