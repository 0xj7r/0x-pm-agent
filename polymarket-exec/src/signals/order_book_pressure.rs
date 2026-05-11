//! Polymarket order-book pressure signal.
//!
//! This is execution/microstructure context, not BTC regime. It summarizes
//! visible depth and recent taker flow so strategies can distinguish a cheap
//! leg from a potentially toxic falling knife.

use crate::market_making::pairing::types::{LadderLeg, PairedMarketSnapshot};

use super::momentum::SignalDirection;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OrderBookPressureConfig {
    pub imbalance_deadband: f64,
    pub min_depth_notional_usd: f64,
}

impl Default for OrderBookPressureConfig {
    fn default() -> Self {
        Self {
            imbalance_deadband: 0.10,
            min_depth_notional_usd: 10.0,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct OrderBookPressureSignal {
    pub direction: SignalDirection,
    pub imbalance: f64,
    pub yes_bid_notional: f64,
    pub yes_ask_notional: f64,
    pub no_bid_notional: f64,
    pub no_ask_notional: f64,
    pub yes_taker_buy_qty_60s: f64,
    pub yes_taker_sell_qty_60s: f64,
    pub no_taker_buy_qty_60s: f64,
    pub no_taker_sell_qty_60s: f64,
    pub thin_book: bool,
}

impl OrderBookPressureSignal {
    pub fn pressure_leg(&self) -> Option<LadderLeg> {
        match self.direction {
            SignalDirection::Up => Some(LadderLeg::Yes),
            SignalDirection::Down => Some(LadderLeg::No),
            SignalDirection::Neutral => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OrderBookPressureEngine {
    config: OrderBookPressureConfig,
}

impl OrderBookPressureEngine {
    pub fn new(config: OrderBookPressureConfig) -> Self {
        Self { config }
    }

    pub fn compute(&self, snapshot: &PairedMarketSnapshot) -> OrderBookPressureSignal {
        let yes_bid_notional = top_bid_notional(&snapshot.yes_quote);
        let yes_ask_notional = top_ask_notional(&snapshot.yes_quote);
        let no_bid_notional = top_bid_notional(&snapshot.no_quote);
        let no_ask_notional = top_ask_notional(&snapshot.no_quote);

        let yes_flow = snapshot.yes_quote.taker_buy_qty_60s - snapshot.yes_quote.taker_sell_qty_60s;
        let no_flow = snapshot.no_quote.taker_buy_qty_60s - snapshot.no_quote.taker_sell_qty_60s;
        let flow_pressure = yes_flow - no_flow;

        let depth_pressure =
            (yes_bid_notional + no_ask_notional) - (no_bid_notional + yes_ask_notional);
        let denom = (flow_pressure.abs()
            + depth_pressure.abs()
            + yes_bid_notional
            + yes_ask_notional
            + no_bid_notional
            + no_ask_notional)
            .max(1e-9);
        let imbalance = ((flow_pressure + depth_pressure) / denom).clamp(-1.0, 1.0);
        let direction = SignalDirection::from_signed(imbalance, self.config.imbalance_deadband);
        let thin_book = [
            yes_bid_notional,
            yes_ask_notional,
            no_bid_notional,
            no_ask_notional,
        ]
        .iter()
        .all(|notional| *notional < self.config.min_depth_notional_usd);

        OrderBookPressureSignal {
            direction,
            imbalance,
            yes_bid_notional,
            yes_ask_notional,
            no_bid_notional,
            no_ask_notional,
            yes_taker_buy_qty_60s: snapshot.yes_quote.taker_buy_qty_60s,
            yes_taker_sell_qty_60s: snapshot.yes_quote.taker_sell_qty_60s,
            no_taker_buy_qty_60s: snapshot.no_quote.taker_buy_qty_60s,
            no_taker_sell_qty_60s: snapshot.no_quote.taker_sell_qty_60s,
            thin_book,
        }
    }
}

impl Default for OrderBookPressureEngine {
    fn default() -> Self {
        Self::new(OrderBookPressureConfig::default())
    }
}

fn top_bid_notional(quote: &crate::types::QuoteSnapshot) -> f64 {
    quote
        .best_bid
        .as_ref()
        .map(|level| level.price.max(0.0) * level.quantity.max(0.0))
        .unwrap_or_default()
}

fn top_ask_notional(quote: &crate::types::QuoteSnapshot) -> f64 {
    quote
        .best_ask
        .as_ref()
        .map(|level| level.price.max(0.0) * level.quantity.max(0.0))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{BookLevel, InstrumentId, MarketId, QuoteSnapshot};

    fn quote(bid: f64, bid_qty: f64, ask: f64, ask_qty: f64, buy: f64, sell: f64) -> QuoteSnapshot {
        QuoteSnapshot {
            best_bid: Some(BookLevel::new(bid, bid_qty)),
            best_ask: Some(BookLevel::new(ask, ask_qty)),
            taker_buy_qty_60s: buy,
            taker_sell_qty_60s: sell,
            ..Default::default()
        }
    }

    #[test]
    fn pressure_points_toward_yes_when_yes_flow_and_depth_dominate() {
        let snapshot = PairedMarketSnapshot {
            market_id: MarketId::new("m"),
            yes_instrument_id: InstrumentId::new("yes"),
            no_instrument_id: InstrumentId::new("no"),
            yes_quote: quote(0.50, 100.0, 0.51, 20.0, 50.0, 5.0),
            no_quote: quote(0.49, 10.0, 0.50, 100.0, 2.0, 30.0),
        };

        let signal = OrderBookPressureEngine::default().compute(&snapshot);

        assert_eq!(signal.direction, SignalDirection::Up);
        assert_eq!(signal.pressure_leg(), Some(LadderLeg::Yes));
        assert!(signal.imbalance > 0.0);
        assert!(!signal.thin_book);
    }

    #[test]
    fn pressure_marks_thin_books() {
        let snapshot = PairedMarketSnapshot {
            market_id: MarketId::new("m"),
            yes_instrument_id: InstrumentId::new("yes"),
            no_instrument_id: InstrumentId::new("no"),
            yes_quote: quote(0.50, 1.0, 0.51, 1.0, 0.0, 0.0),
            no_quote: quote(0.49, 1.0, 0.50, 1.0, 0.0, 0.0),
        };

        let signal = OrderBookPressureEngine::default().compute(&snapshot);

        assert!(signal.thin_book);
    }
}
