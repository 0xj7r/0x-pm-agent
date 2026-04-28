//! Trade-tape synthesiser: derives `TradeEvent`s from Polymarket's
//! `last_trade_price` events plus correlated `price_change` depth deltas.
//!
//! Polymarket's market WS does not expose a per-trade topic with size +
//! taker side. We reconstruct those by:
//!   - taker_side: comparing trade price to last-known best_bid/best_ask
//!   - size: matching against recent `price_change` depth deltas
//! Every synthesised event has `synthesised: true` so downstream consumers
//! can distinguish reconstructed events from a future direct trade feed.

use std::collections::VecDeque;

use crate::core::types::{EpochMillis, InstrumentId, QuoteSnapshot, TradeSide};
use crate::paper::trade_tape::TradeEvent;

const SIZE_MATCH_WINDOW_MS: u64 = 100;

#[derive(Debug, Clone, Copy)]
struct DepthDelta {
    price: f64,
    delta: f64,
    observed_at_ms: EpochMillis,
}

#[derive(Debug, Default)]
pub struct TradeSynthesiser {
    recent_deltas: std::collections::HashMap<String, VecDeque<DepthDelta>>,
}

impl TradeSynthesiser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe_price_change(
        &mut self,
        asset: &str,
        price: f64,
        depth_delta: f64,
        observed_at_ms: EpochMillis,
    ) {
        let buf = self
            .recent_deltas
            .entry(asset.to_string())
            .or_insert_with(VecDeque::new);
        buf.push_back(DepthDelta {
            price,
            delta: depth_delta,
            observed_at_ms,
        });
        let cutoff = observed_at_ms.saturating_sub(SIZE_MATCH_WINDOW_MS);
        while buf.front().map(|d| d.observed_at_ms < cutoff).unwrap_or(false) {
            buf.pop_front();
        }
    }

    fn size_for(&self, asset: &str, price: f64, observed_at_ms: EpochMillis) -> f64 {
        let Some(buf) = self.recent_deltas.get(asset) else {
            return 0.0;
        };
        let cutoff = observed_at_ms.saturating_sub(SIZE_MATCH_WINDOW_MS);
        buf.iter()
            .rev()
            .find(|d| d.observed_at_ms >= cutoff && (d.price - price).abs() < 1e-9)
            .map(|d| d.delta)
            .unwrap_or(0.0)
    }

    pub fn synthesise(
        &self,
        asset: &str,
        price: f64,
        book: &QuoteSnapshot,
        observed_at_ms: EpochMillis,
    ) -> Option<TradeEvent> {
        let taker_side = if let Some(ask) = book.best_ask.as_ref() {
            if price >= ask.price {
                TradeSide::Buy
            } else if let Some(bid) = book.best_bid.as_ref() {
                if price <= bid.price {
                    TradeSide::Sell
                } else {
                    return None;
                }
            } else {
                return None;
            }
        } else if let Some(bid) = book.best_bid.as_ref() {
            if price <= bid.price {
                TradeSide::Sell
            } else {
                return None;
            }
        } else {
            return None;
        };
        let size = self.size_for(asset, price, observed_at_ms);
        Some(TradeEvent {
            asset_id: InstrumentId::from(asset),
            taker_side,
            price,
            size,
            event_at_ms: observed_at_ms,
            trade_id: format!("syn-{}-{}-{:.6}", asset, observed_at_ms, price),
            synthesised: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::BookLevel;

    fn book_with(bid: Option<f64>, ask: Option<f64>) -> QuoteSnapshot {
        QuoteSnapshot {
            best_bid: bid.map(|p| BookLevel::new(p, 100.0)),
            best_ask: ask.map(|p| BookLevel::new(p, 100.0)),
            bid_levels: vec![],
            ask_levels: vec![],
            depth_observed_at_ms: None,
            last_trade_price: None,
            observed_at_ms: 0,
        }
    }

    #[test]
    fn synthesise_returns_taker_buy_when_price_at_or_above_best_ask() {
        let s = TradeSynthesiser::new();
        let book = book_with(Some(0.49), Some(0.50));
        let event = s
            .synthesise("asset-1", 0.51, &book, 1_000)
            .expect("expected Some when book is well-formed");
        assert_eq!(event.taker_side, TradeSide::Buy);
        assert!(event.synthesised);
    }

    #[test]
    fn synthesise_returns_taker_sell_when_price_at_or_below_best_bid() {
        let s = TradeSynthesiser::new();
        let book = book_with(Some(0.49), Some(0.50));
        let event = s
            .synthesise("asset-1", 0.49, &book, 1_000)
            .expect("expected Some when book is well-formed");
        assert_eq!(event.taker_side, TradeSide::Sell);
    }

    #[test]
    fn synthesise_returns_none_when_price_strictly_between_bid_and_ask() {
        let s = TradeSynthesiser::new();
        let book = book_with(Some(0.49), Some(0.51));
        assert!(s.synthesise("asset-1", 0.50, &book, 1_000).is_none());
    }

    #[test]
    fn synthesise_returns_none_when_book_has_neither_bid_nor_ask() {
        let s = TradeSynthesiser::new();
        let book = book_with(None, None);
        assert!(s.synthesise("asset-1", 0.50, &book, 1_000).is_none());
    }

    #[test]
    fn synthesise_size_pulled_from_recent_observed_depth_delta() {
        let mut s = TradeSynthesiser::new();
        let book = book_with(Some(0.49), Some(0.50));
        // A depth-delta at the trade price observed 50ms before the trade
        // arrives should be attributed as the trade size.
        s.observe_price_change("asset-1", 0.51, 25.0, 950);
        let event = s
            .synthesise("asset-1", 0.51, &book, 1_000)
            .expect("expected Some");
        assert!((event.size - 25.0).abs() < 1e-9);
    }

    #[test]
    fn synthesise_size_zero_when_only_stale_depth_deltas_available() {
        let mut s = TradeSynthesiser::new();
        let book = book_with(Some(0.49), Some(0.50));
        // 200ms old: outside the 100ms window
        s.observe_price_change("asset-1", 0.51, 25.0, 800);
        let event = s
            .synthesise("asset-1", 0.51, &book, 1_000)
            .expect("expected Some");
        assert_eq!(event.size, 0.0);
    }

    #[test]
    fn synthesise_size_zero_when_recent_delta_is_at_different_price() {
        let mut s = TradeSynthesiser::new();
        let book = book_with(Some(0.49), Some(0.50));
        s.observe_price_change("asset-1", 0.55, 25.0, 950);
        let event = s
            .synthesise("asset-1", 0.51, &book, 1_000)
            .expect("expected Some");
        assert_eq!(event.size, 0.0);
    }

    #[test]
    fn synthesise_trade_id_is_unique_across_prices_at_same_timestamp() {
        let s = TradeSynthesiser::new();
        let book = book_with(Some(0.40), Some(0.60));
        let a = s.synthesise("asset-1", 0.61, &book, 1_000).unwrap();
        let b = s.synthesise("asset-1", 0.62, &book, 1_000).unwrap();
        assert_ne!(
            a.trade_id, b.trade_id,
            "trade_id must distinguish trades at different prices in the same ms"
        );
    }
}
