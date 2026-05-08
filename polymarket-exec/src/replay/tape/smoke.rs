use crate::replay::tape::book_state::BookState;
use crate::replay::tape::format::{BookEventV1, BtcTickV1, TradeEventV1};
use crate::replay::tape::reader::MappedTape;
use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use std::time::Instant;

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct TapeSmokeStats {
    pub book_events: usize,
    pub book_top_changes: usize,
    pub trade_events: usize,
    pub btc_ticks: usize,
    pub total_events: usize,
    pub elapsed_ms: u128,
    pub events_per_second: u64,
}

pub fn run_tape_smoke(
    book_path: impl AsRef<Path>,
    trades_path: impl AsRef<Path>,
    btc_path: impl AsRef<Path>,
) -> Result<TapeSmokeStats> {
    let book = MappedTape::<BookEventV1>::open(book_path)?;
    let trades = MappedTape::<TradeEventV1>::open(trades_path)?;
    let btc = MappedTape::<BtcTickV1>::open(btc_path)?;

    Ok(run_tape_smoke_records(
        book.records(),
        trades.records(),
        btc.records(),
    ))
}

pub fn run_tape_smoke_records(
    book_events: &[BookEventV1],
    trade_events: &[TradeEventV1],
    btc_ticks: &[BtcTickV1],
) -> TapeSmokeStats {
    let started = Instant::now();
    let mut book_state = BookState::new();
    let mut stats = TapeSmokeStats::default();
    let mut book_idx = 0;
    let mut trade_idx = 0;
    let mut btc_idx = 0;

    while book_idx < book_events.len()
        || trade_idx < trade_events.len()
        || btc_idx < btc_ticks.len()
    {
        let book_ts = book_events
            .get(book_idx)
            .map(|event| event.ts_ns)
            .unwrap_or(u64::MAX);
        let trade_ts = trade_events
            .get(trade_idx)
            .map(|event| event.ts_ns)
            .unwrap_or(u64::MAX);
        let btc_ts = btc_ticks
            .get(btc_idx)
            .map(|event| event.ts_ns)
            .unwrap_or(u64::MAX);

        if book_ts <= trade_ts && book_ts <= btc_ts {
            if book_state.apply(&book_events[book_idx]) {
                stats.book_top_changes += 1;
            }
            stats.book_events += 1;
            book_idx += 1;
        } else if trade_ts <= btc_ts {
            stats.trade_events += 1;
            trade_idx += 1;
        } else {
            stats.btc_ticks += 1;
            btc_idx += 1;
        }
    }

    stats.total_events = stats.book_events + stats.trade_events + stats.btc_ticks;
    stats.elapsed_ms = started.elapsed().as_millis();
    stats.events_per_second = if stats.elapsed_ms == 0 {
        stats.total_events as u64
    } else {
        ((stats.total_events as u128 * 1_000) / stats.elapsed_ms) as u64
    };
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::tape::format::{EVENT_UPDATE, LEG_YES, SIDE_BID};

    #[test]
    fn smoke_loop_merges_streams_and_tracks_top_changes() {
        let stats = run_tape_smoke_records(
            &[BookEventV1 {
                ts_ns: 2,
                price_ticks: 5_000,
                size_lots: 100,
                leg: LEG_YES,
                side: SIDE_BID,
                event_type: EVENT_UPDATE,
                _pad: [0; 5],
            }],
            &[TradeEventV1 {
                ts_ns: 3,
                price_ticks: 5_000,
                size_lots: 10,
                leg: LEG_YES,
                taker_side: 0,
                _pad: [0; 6],
            }],
            &[BtcTickV1 {
                ts_ns: 1,
                price_cents: 10_000_000,
                qty_lots: 1,
                _pad: [0; 4],
            }],
        );

        assert_eq!(stats.book_events, 1);
        assert_eq!(stats.book_top_changes, 1);
        assert_eq!(stats.trade_events, 1);
        assert_eq!(stats.btc_ticks, 1);
        assert_eq!(stats.total_events, 3);
    }
}
