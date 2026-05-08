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
    events.extend(market_meta_events(options)?);
    for market in &options.markets {
        let market_prefix = tape_market_prefix(options, market);
        events.extend(read_market_tape_events(options, market, &market_prefix)?);
    }

    Ok(dedupe_and_sort(events))
}

fn read_market_tape_events(
    options: &TapeReplayOptions,
    market: &RawReplayMarket,
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

    let mut events = Vec::with_capacity(
        book.records().len() + trades.records().len() + btc.records().len() + 1,
    );

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
        events.push(btc_event(tick, idx as i64));
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

fn market_meta_events(options: &TapeReplayOptions) -> Result<Vec<Event>> {
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
            "source": "packed_tape_replay_market_meta"
        });
        if let Some(strike) = market.strike.as_ref() {
            raw["strike"] = json!(strike.parse::<f64>().unwrap_or(0.0));
            raw["strike_source"] = json!("market_metadata");
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

fn btc_event(tick: &BtcTickV1, sequence: i64) -> Event {
    Event {
        v: 1,
        ts_ns: tick.ts_ns as i64,
        received_ns: tick.ts_ns as i64,
        event_type: EventType::BtcTick,
        market_type: "btc_ref".to_string(),
        market_slug: Some("btcusdt".to_string()),
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
    use crate::replay::tape::format::{TAKER_BUY, TAKER_SELL};

    fn market() -> RawReplayMarket {
        RawReplayMarket {
            slug: "btc-updown-5m-1771119900".to_string(),
            asset_ids: ["UP".to_string(), "DOWN".to_string()],
            strike: Some("70000.00".to_string()),
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
