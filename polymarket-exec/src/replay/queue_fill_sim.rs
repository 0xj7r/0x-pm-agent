//! Queue-position-aware limit-order fill simulator.
//!
//! Independent of the broader `fill_sim` module. Provides a self-contained
//! `QueuePositionFillSim` keyed by `(market, asset_id, side)` with a tunable
//! `QueueAssumption` (Optimistic / Base / Conservative). On placement we
//! snapshot the visible same-side queue ahead of our price, scale it by the
//! assumption multiplier, and decrement it as subsequent same-side aggressor
//! flow at-or-through our price arrives. When `queue_ahead <= 0`, residual
//! trade size fills our order.
//!
//! Edge cases:
//! - A trade strictly worse than our limit price for the resting side does
//!   not affect our order (could not have filled us either way).
//! - Replace drops the prior tracking entry entirely; the new order takes a
//!   fresh queue snapshot from the supplied book levels.
//! - Cross-market and cross-asset isolation: only trades on the matching
//!   `(market, asset_id, side)` tuple touch a given resting order.

#![allow(dead_code)]

use std::collections::HashMap;

type PriceTicks = i64;

const PRICE_TICK_SCALE: f64 = 100.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OrderId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone)]
pub struct OurOrder {
    pub id: OrderId,
    pub market: String,
    pub asset_id: String,
    pub side: Side,
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct BookLevel {
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAssumption {
    Optimistic,
    Base,
    Conservative,
}

impl QueueAssumption {
    fn multiplier(self) -> f64 {
        match self {
            QueueAssumption::Optimistic => 0.5,
            QueueAssumption::Base => 1.0,
            QueueAssumption::Conservative => 2.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FillEvent {
    Filled {
        id: OrderId,
        qty: f64,
        price: f64,
    },
    PartialFill {
        id: OrderId,
        qty: f64,
        price: f64,
        remaining: f64,
    },
}

#[derive(Debug, Clone)]
struct Tracked {
    order: OurOrder,
    price_ticks: PriceTicks,
    queue_ahead: f64,
    remaining: f64,
    sequence: u64,
}

pub struct QueuePositionFillSim {
    assumption: QueueAssumption,
    orders: HashMap<OrderId, Tracked>,
    next_sequence: u64,
}

impl QueuePositionFillSim {
    pub fn new(assumption: QueueAssumption) -> Self {
        Self {
            assumption,
            orders: HashMap::new(),
            next_sequence: 0,
        }
    }

    pub fn on_place(&mut self, order: OurOrder, book_levels_same_side: &[BookLevel]) {
        let price_ticks = price_ticks(order.price);
        let queue_raw = sum_queue_ahead(order.side, price_ticks, book_levels_same_side);
        let queue_ahead = queue_raw * self.assumption.multiplier();
        let remaining = order.size;
        let id = order.id;
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.orders.insert(
            id,
            Tracked {
                order,
                price_ticks,
                queue_ahead,
                remaining,
                sequence,
            },
        );
    }

    pub fn on_cancel(&mut self, id: OrderId) {
        self.orders.remove(&id);
    }

    pub fn on_replace(
        &mut self,
        old_id: OrderId,
        new_order: OurOrder,
        book_levels_same_side: &[BookLevel],
    ) {
        self.orders.remove(&old_id);
        self.on_place(new_order, book_levels_same_side);
    }

    /// Process an incoming trade on our side at price `p`, qty `q`. Returns any
    /// fills our orders earned. `side` here is the same side as the resting
    /// order (so a public taker that hits our resting ask is reported as
    /// `Side::Sell`, matching our ask's side).
    pub fn on_trade(
        &mut self,
        market: &str,
        asset_id: &str,
        side: Side,
        price: f64,
        qty: f64,
    ) -> Vec<FillEvent> {
        if qty <= 0.0 {
            return Vec::new();
        }
        let trade_price_ticks = price_ticks(price);
        // Stable venue-priority order: price priority, then placement sequence,
        // then id as a deterministic tiebreaker.
        let mut candidates: Vec<(OrderId, PriceTicks, u64)> = self
            .orders
            .iter()
            .filter_map(|(id, tracked)| {
                if tracked.order.market == market
                    && tracked.order.asset_id == asset_id
                    && tracked.order.side == side
                    && trade_reaches_price(side, trade_price_ticks, tracked.price_ticks)
                {
                    Some((*id, tracked.price_ticks, tracked.sequence))
                } else {
                    None
                }
            })
            .collect();
        candidates.sort_by(|left, right| venue_priority(left, right, side));

        let mut remaining_qty = qty;
        let mut fills: Vec<FillEvent> = Vec::new();
        let mut to_remove: Vec<OrderId> = Vec::new();

        for (id, _, _) in candidates {
            if remaining_qty <= 0.0 {
                break;
            }
            let Some(tracked) = self.orders.get_mut(&id) else {
                continue;
            };

            // Eat the queue first.
            if tracked.queue_ahead > 0.0 {
                let eaten = remaining_qty.min(tracked.queue_ahead);
                tracked.queue_ahead -= eaten;
                remaining_qty -= eaten;
                if remaining_qty <= 0.0 {
                    continue;
                }
            }

            // Queue cleared, residual fills our order.
            let fill_qty = remaining_qty.min(tracked.remaining);
            if fill_qty <= 0.0 {
                continue;
            }
            tracked.remaining -= fill_qty;
            remaining_qty -= fill_qty;
            let order_price = tracked.order.price;
            if tracked.remaining <= 1e-12 {
                fills.push(FillEvent::Filled {
                    id,
                    qty: fill_qty,
                    price: order_price,
                });
                to_remove.push(id);
            } else {
                fills.push(FillEvent::PartialFill {
                    id,
                    qty: fill_qty,
                    price: order_price,
                    remaining: tracked.remaining,
                });
            }
        }

        for id in to_remove {
            self.orders.remove(&id);
        }
        fills
    }
}

/// Visible same-side queue at-or-better than `our_price`. For a resting buy,
/// "better" means a higher bid (closer to the touch). For a resting sell,
/// "better" means a lower ask. Equal-price levels also count as "ahead":
/// without per-order timestamps in a snapshot we conservatively assume the
/// existing queue at our price arrived before us.
fn price_ticks(price: f64) -> PriceTicks {
    (price * PRICE_TICK_SCALE).round() as PriceTicks
}

fn sum_queue_ahead(side: Side, our_price_ticks: PriceTicks, levels: &[BookLevel]) -> f64 {
    levels
        .iter()
        .filter(|lvl| match side {
            Side::Buy => price_ticks(lvl.price) >= our_price_ticks,
            Side::Sell => price_ticks(lvl.price) <= our_price_ticks,
        })
        .map(|lvl| lvl.size)
        .sum()
}

/// Does a trade at `trade_price` reach a resting order at `order_price`?
/// Buys rest below the touch; an aggressor sell at-or-below our bid hits us.
/// Sells rest above the touch; an aggressor buy at-or-above our ask hits us.
fn trade_reaches_price(
    side: Side,
    trade_price_ticks: PriceTicks,
    order_price_ticks: PriceTicks,
) -> bool {
    match side {
        Side::Buy => trade_price_ticks <= order_price_ticks,
        Side::Sell => trade_price_ticks >= order_price_ticks,
    }
}

fn venue_priority(
    left: &(OrderId, PriceTicks, u64),
    right: &(OrderId, PriceTicks, u64),
    side: Side,
) -> std::cmp::Ordering {
    let price_order = match side {
        Side::Buy => right.1.cmp(&left.1),
        Side::Sell => left.1.cmp(&right.1),
    };
    price_order
        .then_with(|| left.2.cmp(&right.2))
        .then_with(|| left.0 .0.cmp(&right.0 .0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(id: u64, market: &str, asset: &str, side: Side, price: f64, size: f64) -> OurOrder {
        OurOrder {
            id: OrderId(id),
            market: market.into(),
            asset_id: asset.into(),
            side,
            price,
            size,
        }
    }

    #[test]
    fn top_of_book_no_queue_fills_on_first_trade() {
        let mut sim = QueuePositionFillSim::new(QueueAssumption::Base);
        // Our sell at 0.55 with empty same-side book ahead.
        sim.on_place(order(1, "m1", "a1", Side::Sell, 0.55, 10.0), &[]);
        let fills = sim.on_trade("m1", "a1", Side::Sell, 0.55, 10.0);
        assert_eq!(fills.len(), 1);
        match &fills[0] {
            FillEvent::Filled { id, qty, price } => {
                assert_eq!(*id, OrderId(1));
                assert!((qty - 10.0).abs() < 1e-9);
                assert!((price - 0.55).abs() < 1e-9);
            }
            other => panic!("expected Filled, got {:?}", other),
        }
    }

    #[test]
    fn behind_queue_clears_then_next_trade_fills() {
        let mut sim = QueuePositionFillSim::new(QueueAssumption::Base);
        let book = [BookLevel {
            price: 0.55,
            size: 100.0,
        }];
        sim.on_place(order(1, "m1", "a1", Side::Sell, 0.55, 10.0), &book);

        // 99 units of trade -- only chips queue, no fill.
        let fills = sim.on_trade("m1", "a1", Side::Sell, 0.55, 99.0);
        assert!(fills.is_empty(), "no fill expected, got {:?}", fills);

        // 1 unit clears queue exactly; nothing fills yet.
        let fills = sim.on_trade("m1", "a1", Side::Sell, 0.55, 1.0);
        assert!(fills.is_empty(), "queue head only, got {:?}", fills);

        // Next trade fills us.
        let fills = sim.on_trade("m1", "a1", Side::Sell, 0.55, 10.0);
        assert_eq!(fills.len(), 1);
        match &fills[0] {
            FillEvent::Filled { qty, .. } => assert!((qty - 10.0).abs() < 1e-9),
            other => panic!("expected Filled, got {:?}", other),
        }
    }

    #[test]
    fn optimistic_vs_conservative_diverges_on_identical_input() {
        let book = [BookLevel {
            price: 0.55,
            size: 20.0,
        }];

        let mut opt = QueuePositionFillSim::new(QueueAssumption::Optimistic);
        opt.on_place(order(1, "m1", "a1", Side::Sell, 0.55, 5.0), &book);
        // Optimistic queue = 20 * 0.5 = 10. A 12-unit trade clears 10 then
        // delivers 2 to us.
        let opt_fills = opt.on_trade("m1", "a1", Side::Sell, 0.55, 12.0);
        assert_eq!(opt_fills.len(), 1);

        let mut cons = QueuePositionFillSim::new(QueueAssumption::Conservative);
        cons.on_place(order(1, "m1", "a1", Side::Sell, 0.55, 5.0), &book);
        // Conservative queue = 20 * 2.0 = 40. A 12-unit trade only chips
        // queue; no fill.
        let cons_fills = cons.on_trade("m1", "a1", Side::Sell, 0.55, 12.0);
        assert!(
            cons_fills.is_empty(),
            "conservative should not fill yet, got {:?}",
            cons_fills
        );
    }

    #[test]
    fn partial_fill_then_filled() {
        let mut sim = QueuePositionFillSim::new(QueueAssumption::Base);
        sim.on_place(order(1, "m1", "a1", Side::Sell, 0.55, 10.0), &[]);
        let fills = sim.on_trade("m1", "a1", Side::Sell, 0.55, 7.0);
        assert_eq!(fills.len(), 1);
        match &fills[0] {
            FillEvent::PartialFill {
                id,
                qty,
                price,
                remaining,
            } => {
                assert_eq!(*id, OrderId(1));
                assert!((qty - 7.0).abs() < 1e-9);
                assert!((price - 0.55).abs() < 1e-9);
                assert!((remaining - 3.0).abs() < 1e-9);
            }
            other => panic!("expected PartialFill, got {:?}", other),
        }

        let fills = sim.on_trade("m1", "a1", Side::Sell, 0.55, 3.0);
        assert_eq!(fills.len(), 1);
        match &fills[0] {
            FillEvent::Filled { qty, .. } => assert!((qty - 3.0).abs() < 1e-9),
            other => panic!("expected Filled, got {:?}", other),
        }
    }

    #[test]
    fn cancel_drops_tracking_no_subsequent_fills() {
        let mut sim = QueuePositionFillSim::new(QueueAssumption::Base);
        let book = [BookLevel {
            price: 0.55,
            size: 5.0,
        }];
        sim.on_place(order(1, "m1", "a1", Side::Sell, 0.55, 10.0), &book);
        sim.on_cancel(OrderId(1));
        // Even after enough trade flow to clear the queue and fill, no events.
        let fills = sim.on_trade("m1", "a1", Side::Sell, 0.55, 100.0);
        assert!(
            fills.is_empty(),
            "canceled order should not fill: {:?}",
            fills
        );
    }

    #[test]
    fn replace_forgets_old_order_and_uses_new_book_snapshot() {
        let mut sim = QueuePositionFillSim::new(QueueAssumption::Base);
        let stale_book = [BookLevel {
            price: 0.55,
            size: 100.0,
        }];
        sim.on_place(order(1, "m1", "a1", Side::Sell, 0.55, 10.0), &stale_book);

        // Replace at a fresh price with no queue ahead.
        let fresh_book: [BookLevel; 0] = [];
        sim.on_replace(
            OrderId(1),
            order(2, "m1", "a1", Side::Sell, 0.56, 10.0),
            &fresh_book,
        );

        // A trade at 0.55 must NOT fill the new order (price 0.56 not reached).
        let fills = sim.on_trade("m1", "a1", Side::Sell, 0.55, 10.0);
        assert!(fills.is_empty(), "below new order price, got {:?}", fills);

        // A trade at 0.56 fills the new order immediately, no queue ahead.
        let fills = sim.on_trade("m1", "a1", Side::Sell, 0.56, 10.0);
        assert_eq!(fills.len(), 1);
        match &fills[0] {
            FillEvent::Filled { id, qty, price } => {
                assert_eq!(*id, OrderId(2));
                assert!((qty - 10.0).abs() < 1e-9);
                assert!((price - 0.56).abs() < 1e-9);
            }
            other => panic!("expected Filled, got {:?}", other),
        }
    }

    #[test]
    fn cross_market_isolation_prevents_unrelated_fills() {
        let mut sim = QueuePositionFillSim::new(QueueAssumption::Base);
        sim.on_place(order(1, "market-a", "asset-1", Side::Sell, 0.55, 10.0), &[]);
        // Trades on a different market do nothing.
        let fills = sim.on_trade("market-b", "asset-1", Side::Sell, 0.55, 50.0);
        assert!(fills.is_empty());
        // Trades on the same market but different asset do nothing.
        let fills = sim.on_trade("market-a", "asset-2", Side::Sell, 0.55, 50.0);
        assert!(fills.is_empty());
        // Correct (market, asset) fills.
        let fills = sim.on_trade("market-a", "asset-1", Side::Sell, 0.55, 10.0);
        assert_eq!(fills.len(), 1);
    }

    #[test]
    fn multiple_orders_fill_by_price_priority_before_id_order() {
        let mut sim = QueuePositionFillSim::new(QueueAssumption::Base);
        sim.on_place(order(1, "m1", "a1", Side::Buy, 0.42, 5.0), &[]);
        sim.on_place(order(2, "m1", "a1", Side::Buy, 0.43, 5.0), &[]);

        let fills = sim.on_trade("m1", "a1", Side::Buy, 0.42, 8.0);
        assert_eq!(fills.len(), 2);
        match &fills[0] {
            FillEvent::Filled { id, qty, .. } => {
                assert_eq!(*id, OrderId(2));
                assert!((qty - 5.0).abs() < 1e-9);
            }
            other => panic!("expected better bid to fill first, got {:?}", other),
        }
        match &fills[1] {
            FillEvent::PartialFill {
                id, qty, remaining, ..
            } => {
                assert_eq!(*id, OrderId(1));
                assert!((qty - 3.0).abs() < 1e-9);
                assert!((remaining - 2.0).abs() < 1e-9);
            }
            other => panic!(
                "expected lower bid partial after better bid, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn tick_equivalent_prices_match_despite_float_noise() {
        let mut sim = QueuePositionFillSim::new(QueueAssumption::Base);
        sim.on_place(order(1, "m1", "a1", Side::Sell, 0.55, 10.0), &[]);

        let fills = sim.on_trade("m1", "a1", Side::Sell, 0.549_999_999_999, 10.0);
        assert_eq!(fills.len(), 1);
    }

    #[test]
    fn buy_side_queue_counts_better_or_equal_bids_only() {
        let mut sim = QueuePositionFillSim::new(QueueAssumption::Base);
        // Book ahead of our 0.42 bid: a 0.43 bid (better) and our level 0.42.
        // 0.41 (worse) must NOT count.
        let book = [
            BookLevel {
                price: 0.43,
                size: 5.0,
            },
            BookLevel {
                price: 0.42,
                size: 10.0,
            },
            BookLevel {
                price: 0.41,
                size: 100.0,
            },
        ];
        sim.on_place(order(1, "m1", "a1", Side::Buy, 0.42, 4.0), &book);
        // Queue ahead = 5 + 10 = 15. A 14-unit aggressor sell at 0.42 chips
        // queue; no fill.
        let fills = sim.on_trade("m1", "a1", Side::Buy, 0.42, 14.0);
        assert!(fills.is_empty());
        // Next 5 units: 1 clears queue, 4 fill us.
        let fills = sim.on_trade("m1", "a1", Side::Buy, 0.42, 5.0);
        assert_eq!(fills.len(), 1);
        match &fills[0] {
            FillEvent::Filled { qty, .. } => assert!((qty - 4.0).abs() < 1e-9),
            other => panic!("expected Filled, got {:?}", other),
        }
    }
}
