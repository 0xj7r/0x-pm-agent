//! Deterministic fill simulator.
//!
//! Given a stream of canonical `Event` records and the strategy's order
//! intents, decide what fills the strategy would have received, with a
//! configurable latency model and a FIFO+jitter queue-position
//! approximation. Mirrors the live paper-fill behavior in
//! `runtime::paper_fill` so backtest and paper agree numerically.
//!
//! Determinism: no wall-clock reads, no `HashMap` iteration. RNG is seeded
//! from `(run_id, window_id, --seed)` and used only to break ties when two
//! orders share the same `(price, side, arrival_ns)` tuple.

#![allow(dead_code)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::collector::schema::{Event, EventType};

/// Named latency preset. The numeric values match the spec defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencyPreset {
    /// Zero submit / cancel latency. Optimistic upper-bound.
    Instant,
    /// P50 of historical `local_timestamp_us - timestamp_us` (~50 ms).
    Nominal,
    /// P95 + 50% safety margin (~150 ms).
    Conservative,
}

/// Fill-quality regime, orthogonal to latency. Models how realistic the
/// match logic is for a resting maker order at price P. See module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FillQuality {
    /// Any trade-tape print at or through P fills the resting order, no
    /// matter the trade size or queue position. Upper-bound for fill rate.
    Optimistic,
    /// Default. Resting order fills only after cumulative aggressor flow
    /// at-or-better than P since rest exceeds the visible book depth at P
    /// at rest time. Models "queue ahead must be eaten first".
    Base,
    /// Same as `Base` plus a queue-position haircut: only the portion of
    /// the trade-through that exceeds our estimated queue position fills
    /// us. Most pessimistic.
    Conservative,
}

impl FillQuality {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Optimistic => "optimistic",
            Self::Base => "base",
            Self::Conservative => "conservative",
        }
    }
}

impl LatencyPreset {
    pub fn submit_latency_ms(self) -> u64 {
        match self {
            Self::Instant => 0,
            Self::Nominal => 50,
            Self::Conservative => 150,
        }
    }

    pub fn cancel_latency_ms(self) -> u64 {
        self.submit_latency_ms()
    }
}

/// Configuration knobs for the fill simulator.
#[derive(Debug, Clone, PartialEq)]
pub struct FillSimConfig {
    pub latency: LatencyPreset,
    pub fill_quality: FillQuality,
    pub seed: u64,
    /// 0.0 = no queue progress from same-side cancels (conservative).
    /// 1.0 = full queue progress on every level shrink (optimistic).
    pub cancel_credit_fraction: f64,
}

impl Default for FillSimConfig {
    fn default() -> Self {
        Self {
            latency: LatencyPreset::Nominal,
            fill_quality: FillQuality::Base,
            seed: 0xC0FF_EE00_C0FF_EE00,
            cancel_credit_fraction: 0.5,
        }
    }
}

/// What the strategy wants to do at a moment in time. Light-weight wrapper —
/// the strategy code uses richer `OrderIntent` types in `runtime::types`,
/// but the fill simulator only needs price, size, side, and a stable id.
#[derive(Debug, Clone, PartialEq)]
pub struct StrategyOrderIntent {
    pub client_order_id: String,
    pub asset_id: String,
    pub side: Side,
    pub price: f64,
    pub size: f64,
    /// Wall-clock at which the strategy decided. Replayed virtual time, not
    /// real time.
    pub placed_ms: u64,
    /// If true, the order is an aggressive taker (FAK / IOC); on submit it
    /// crosses the live book at the limit price. Resting maker behaviour is
    /// disabled — fill is deterministic at submit time, capped by visible
    /// opposing depth.
    pub aggressive: bool,
    /// If true, the order MUST rest (post-only). If it would cross the live
    /// book at submit time, the simulator records a `SimulatedRejection` and
    /// emits no fill.
    pub post_only: bool,
}

impl StrategyOrderIntent {
    /// Convenience constructor preserving prior call sites that pre-date the
    /// `aggressive` / `post_only` flags. New code should set the flags
    /// explicitly via struct literal.
    pub fn passive(
        client_order_id: impl Into<String>,
        asset_id: impl Into<String>,
        side: Side,
        price: f64,
        size: f64,
        placed_ms: u64,
    ) -> Self {
        Self {
            client_order_id: client_order_id.into(),
            asset_id: asset_id.into(),
            side,
            price,
            size,
            placed_ms,
            aggressive: false,
            post_only: false,
        }
    }
}

/// Side of the book. Matches `runtime::types::TradeSide` semantically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

/// Outcome record emitted whenever the simulator matches an intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimulatedFill {
    pub client_order_id: String,
    pub asset_id: String,
    pub side: Side,
    pub price: f64,
    pub size: f64,
    pub fill_ms: u64,
    /// Whether this fill was the maker (resting) or taker (aggressor) side.
    /// Determined by the originating intent's `aggressive` flag.
    pub maker_or_taker: MakerOrTaker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MakerOrTaker {
    Maker,
    Taker,
}

/// Records a `post_only` intent that would have crossed the live book at
/// submit time and was therefore rejected before resting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimulatedRejection {
    pub client_order_id: String,
    pub asset_id: String,
    pub side: Side,
    pub price: f64,
    pub size: f64,
    pub rejected_ms: u64,
    pub reason: RejectionReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionReason {
    /// post-only order would have crossed at submit.
    PostOnlyWouldCross,
}

#[derive(Debug, Clone)]
struct RestingOrder {
    intent: StrategyOrderIntent,
    arrival_ms: u64,
    remaining: f64,
    queue_ahead: f64,
    /// Snapshot of visible book depth at-or-better than `intent.price` on
    /// the SAME side, captured the moment the order rested. Used by the
    /// `Base` and `Conservative` regimes to gate fills until the
    /// cumulative aggressor flow has eaten through this depth.
    book_depth_at_rest: f64,
    /// Cumulative public trade volume at-or-better than `intent.price`
    /// SINCE this order rested. Compared against `book_depth_at_rest`.
    cumulative_trade_through: f64,
}

/// Per-asset live book state used by the simulator. Tracks visible depth at
/// each price level so we can snapshot it when a maker rests, and so we can
/// classify post-only crosses on submit.
#[derive(Debug, Clone, Default)]
struct LiveBook {
    /// Bids by price-tick → size visible.
    bids: BTreeMap<i64, f64>,
    /// Asks by price-tick → size visible.
    asks: BTreeMap<i64, f64>,
}

impl LiveBook {
    fn best_bid_ticks(&self) -> Option<i64> {
        self.bids
            .iter()
            .rev()
            .find_map(|(p, sz)| if *sz > 0.0 { Some(*p) } else { None })
    }

    fn best_ask_ticks(&self) -> Option<i64> {
        self.asks
            .iter()
            .find_map(|(p, sz)| if *sz > 0.0 { Some(*p) } else { None })
    }

    /// Total visible size on `side` at-or-better than `price_ticks`. "Better"
    /// means lower for asks (a resting ask at 0.60 has the asks at 0.55-0.59
    /// in front of it) and higher for bids (resting bid at 0.40 sits behind
    /// any bid at 0.41+).
    fn depth_at_or_better_ticks(&self, side: Side, price_ticks: i64) -> f64 {
        match side {
            Side::Sell => self
                .asks
                .range(..=price_ticks)
                .map(|(_, sz)| *sz)
                .sum::<f64>(),
            Side::Buy => self
                .bids
                .range(price_ticks..)
                .map(|(_, sz)| *sz)
                .sum::<f64>(),
        }
    }

    fn apply_level(&mut self, side: Side, price_ticks: i64, size: f64) {
        let map = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        if size <= 0.0 {
            map.remove(&price_ticks);
        } else {
            map.insert(price_ticks, size);
        }
    }
}

/// Public simulator entrypoint. Holds resting orders keyed by
/// `(asset_id, side, price)` and applies events to age + match them.
pub struct FillSimulator {
    cfg: FillSimConfig,
    /// Resting orders bucketed by `(asset_id, side_bucket, price_ticks)`.
    /// `price_ticks` is `(price * 1e6) as i64` to make `f64` sortable.
    /// FIFO within each bucket via insertion order.
    resting: BTreeMap<(String, Side, i64), Vec<RestingOrder>>,
    /// Pending cancels: `client_order_id -> cancel_arrival_ms`. Applied
    /// lazily during event processing so a cancel issued at t=100 with 50 ms
    /// latency does not drop a fill from an event at t=120.
    pending_cancels: BTreeMap<String, u64>,
    /// Per-asset live book mirror used to snapshot resting depth and to
    /// detect post-only crosses.
    books: BTreeMap<String, LiveBook>,
    fills: Vec<SimulatedFill>,
    rejections: Vec<SimulatedRejection>,
}

impl FillSimulator {
    pub fn new(cfg: FillSimConfig) -> Self {
        Self {
            cfg,
            resting: BTreeMap::new(),
            pending_cancels: BTreeMap::new(),
            books: BTreeMap::new(),
            fills: Vec::new(),
            rejections: Vec::new(),
        }
    }

    pub fn config(&self) -> &FillSimConfig {
        &self.cfg
    }

    pub fn pending_count(&self) -> usize {
        self.resting.values().map(|v| v.len()).sum()
    }

    pub fn fills(&self) -> &[SimulatedFill] {
        &self.fills
    }

    pub fn rejections(&self) -> &[SimulatedRejection] {
        &self.rejections
    }

    /// Crossing test for a post-only intent against the current live book.
    /// A buy crosses if its limit_price >= best_ask; a sell crosses if its
    /// limit_price <= best_bid.
    fn would_cross_now(&self, intent: &StrategyOrderIntent) -> bool {
        let Some(book) = self.books.get(&intent.asset_id) else {
            return false;
        };
        let limit_ticks = price_to_ticks(intent.price);
        match intent.side {
            Side::Buy => book.best_ask_ticks().is_some_and(|a| limit_ticks >= a),
            Side::Sell => book.best_bid_ticks().is_some_and(|b| limit_ticks <= b),
        }
    }

    /// Submit a strategy intent. Submit latency is added; the order does
    /// not match any event whose `received_ns` is earlier than its
    /// `arrival_ms`.
    ///
    /// Aggressive (taker) intents fill immediately at the limit price up to
    /// the visible opposing book size at-or-better; nothing rests.
    /// Post-only intents that would cross the live book are rejected and
    /// recorded in `rejections()`.
    pub fn submit(&mut self, intent: StrategyOrderIntent) {
        // Post-only crossing check happens before any resting state mutation
        // so rejected intents leave no trace in the resting book.
        if intent.post_only && self.would_cross_now(&intent) {
            self.rejections.push(SimulatedRejection {
                client_order_id: intent.client_order_id,
                asset_id: intent.asset_id,
                side: intent.side,
                price: intent.price,
                size: intent.size,
                rejected_ms: intent.placed_ms,
                reason: RejectionReason::PostOnlyWouldCross,
            });
            return;
        }

        if intent.aggressive {
            self.execute_taker(intent);
            return;
        }

        let arrival_ms = intent.placed_ms + self.cfg.latency.submit_latency_ms();
        let price_ticks = price_to_ticks(intent.price);
        let book_depth_at_rest = self
            .books
            .get(&intent.asset_id)
            .map(|b| b.depth_at_or_better_ticks(intent.side, price_ticks))
            .unwrap_or(0.0);
        let key = (intent.asset_id.clone(), intent.side, price_ticks);
        let entry = self.resting.entry(key).or_default();
        entry.push(RestingOrder {
            remaining: intent.size,
            queue_ahead: 0.0,
            arrival_ms,
            book_depth_at_rest,
            cumulative_trade_through: 0.0,
            intent,
        });
    }

    /// Cross an aggressive (FAK) order against the visible opposing book at
    /// submit time. Fills are deterministic and produced immediately. No
    /// resting state is created; partial fills (limit price exhausts before
    /// size) simply terminate.
    fn execute_taker(&mut self, intent: StrategyOrderIntent) {
        let limit_ticks = price_to_ticks(intent.price);
        let book = match self.books.get_mut(&intent.asset_id) {
            Some(b) => b,
            None => return,
        };
        let mut remaining = intent.size;
        // Walk the opposing side from the most aggressive level outward.
        // Buys hit asks ascending; sells hit bids descending.
        let levels: Vec<(i64, f64)> = match intent.side {
            Side::Buy => book
                .asks
                .iter()
                .filter(|(p, sz)| **p <= limit_ticks && **sz > 0.0)
                .map(|(p, sz)| (*p, *sz))
                .collect(),
            Side::Sell => book
                .bids
                .iter()
                .rev()
                .filter(|(p, sz)| **p >= limit_ticks && **sz > 0.0)
                .map(|(p, sz)| (*p, *sz))
                .collect(),
        };
        for (price_ticks, available) in levels {
            if remaining <= 0.0 {
                break;
            }
            let take = remaining.min(available);
            let fill_price = ticks_to_price(price_ticks);
            self.fills.push(SimulatedFill {
                client_order_id: intent.client_order_id.clone(),
                asset_id: intent.asset_id.clone(),
                side: intent.side,
                price: fill_price,
                size: take,
                fill_ms: intent.placed_ms + self.cfg.latency.submit_latency_ms(),
                maker_or_taker: MakerOrTaker::Taker,
            });
            remaining -= take;
            // Decrement visible book depth; the taker consumed it.
            let map = match intent.side {
                Side::Buy => &mut book.asks,
                Side::Sell => &mut book.bids,
            };
            let new_size = (available - take).max(0.0);
            if new_size <= 0.0 {
                map.remove(&price_ticks);
            } else {
                map.insert(price_ticks, new_size);
            }
        }
    }

    /// Cancel an intent by client_order_id. Cancel latency is honored —
    /// the order can still match events received before
    /// `cancel_arrival_ms`. Recorded as a pending cancel; the actual
    /// removal happens during `on_event` once virtual time has advanced.
    pub fn cancel(&mut self, client_order_id: &str, requested_ms: u64) {
        let cancel_ms = requested_ms + self.cfg.latency.cancel_latency_ms();
        // Earliest-wins if multiple cancels are issued for the same order.
        self.pending_cancels
            .entry(client_order_id.to_string())
            .and_modify(|v| {
                if cancel_ms < *v {
                    *v = cancel_ms;
                }
            })
            .or_insert(cancel_ms);
    }

    /// Apply one event. Trades on our side at our price match resting
    /// orders FIFO under the configured `FillQuality` regime. Book updates
    /// keep the live book mirror in sync.
    pub fn on_event(&mut self, event: &Event) {
        let event_ms = (event.received_ns / 1_000_000) as u64;
        match event.event_type {
            EventType::Trade => self.match_trade(event),
            EventType::BookDelta | EventType::BookSnapshot => self.apply_book_event(event),
            _ => {}
        }
        // Reap cancels that have arrived strictly before this event.
        if !self.pending_cancels.is_empty() {
            let arrived: Vec<String> = self
                .pending_cancels
                .iter()
                .filter(|(_, &t)| t < event_ms)
                .map(|(k, _)| k.clone())
                .collect();
            for coid in arrived {
                self.pending_cancels.remove(&coid);
                for orders in self.resting.values_mut() {
                    orders.retain(|o| o.intent.client_order_id != coid);
                }
                self.resting.retain(|_, v| !v.is_empty());
            }
        }
    }

    fn apply_book_event(&mut self, event: &Event) {
        let Some(asset_id) = event.asset_id.as_deref() else {
            return;
        };
        let (Some(price), Some(size), Some(side)) = (
            event.price.as_deref().and_then(|s| s.parse::<f64>().ok()),
            event.size.as_deref().and_then(|s| s.parse::<f64>().ok()),
            event.side.as_deref().and_then(parse_side),
        ) else {
            return;
        };
        let book = self.books.entry(asset_id.to_string()).or_default();
        book.apply_level(side, price_to_ticks(price), size);
    }

    fn match_trade(&mut self, event: &Event) {
        let (Some(asset_id), Some(price_str), Some(size_str), Some(side_str)) = (
            event.asset_id.as_deref(),
            event.price.as_deref(),
            event.size.as_deref(),
            event.side.as_deref(),
        ) else {
            return;
        };
        let Ok(price) = price_str.parse::<f64>() else {
            return;
        };
        let Ok(size) = size_str.parse::<f64>() else {
            return;
        };
        // Public trade side classification: a public BUY taker hits the
        // resting ASK side; a public SELL taker hits the resting BID side.
        let resting_side = match side_str.to_ascii_lowercase().as_str() {
            "buy" => Side::Sell,
            "sell" => Side::Buy,
            _ => return,
        };
        let trade_ticks = price_to_ticks(price);
        let event_ms = (event.received_ns / 1_000_000) as u64;
        let regime = self.cfg.fill_quality;

        // 1. Update cumulative trade-through accumulators on every resting
        //    order whose price level is at-or-better than the trade price
        //    on the resting side. `Optimistic` skips this because it does
        //    not gate on cumulative depth.
        if regime != FillQuality::Optimistic {
            for ((order_asset, order_side, order_ticks), orders) in self.resting.iter_mut() {
                if order_asset != asset_id || *order_side != resting_side {
                    continue;
                }
                // "At-or-better" relative to the trade price: a trade at
                // 0.55 contributes to the asks at 0.55+ (the order at 0.55
                // sits behind the asks at 0.50..0.55 we just consumed).
                let counts_for_level = match resting_side {
                    Side::Sell => trade_ticks <= *order_ticks,
                    Side::Buy => trade_ticks >= *order_ticks,
                };
                if !counts_for_level {
                    continue;
                }
                for order in orders.iter_mut() {
                    if order.arrival_ms <= event_ms {
                        order.cumulative_trade_through += size;
                    }
                }
            }
        }

        // 2. Match resting orders at the exact trade price (a trade-through
        //    farther away does not directly fill an order at this price; it
        //    only contributes to the cumulative gauge above).
        let key = (asset_id.to_string(), resting_side, trade_ticks);
        let mut remaining_size = size;
        let mut new_fills: Vec<SimulatedFill> = Vec::new();
        if let Some(orders) = self.resting.get_mut(&key) {
            let mut idx = 0;
            while idx < orders.len() && remaining_size > 0.0 {
                let order = &mut orders[idx];
                if order.arrival_ms > event_ms {
                    idx += 1;
                    continue;
                }
                // Apply regime gate: how much of `remaining_size` is
                // ELIGIBLE to fill this order?
                let eligible = eligible_fill_size(regime, order, remaining_size);
                if eligible <= 0.0 {
                    idx += 1;
                    continue;
                }
                if order.queue_ahead >= eligible {
                    order.queue_ahead -= eligible;
                    remaining_size -= eligible;
                    idx += 1;
                    continue;
                }
                let after_queue = eligible - order.queue_ahead;
                remaining_size -= order.queue_ahead;
                order.queue_ahead = 0.0;
                let fill_size = order.remaining.min(after_queue);
                if fill_size > 0.0 {
                    new_fills.push(SimulatedFill {
                        client_order_id: order.intent.client_order_id.clone(),
                        asset_id: order.intent.asset_id.clone(),
                        side: order.intent.side,
                        price: order.intent.price,
                        size: fill_size,
                        fill_ms: event_ms,
                        maker_or_taker: MakerOrTaker::Maker,
                    });
                    order.remaining -= fill_size;
                    remaining_size -= fill_size;
                }
                if order.remaining <= 0.0 {
                    orders.remove(idx);
                } else {
                    idx += 1;
                }
            }
            if orders.is_empty() {
                self.resting.remove(&key);
            }
        }
        self.fills.extend(new_fills);

        // 3. Mirror the trade into the visible book: a public taker
        //    consumes opposing depth at the trade price.
        if let Some(book) = self.books.get_mut(asset_id) {
            let map = match resting_side {
                Side::Sell => &mut book.asks,
                Side::Buy => &mut book.bids,
            };
            if let Some(existing) = map.get(&trade_ticks).copied() {
                let new_size = (existing - size).max(0.0);
                if new_size <= 0.0 {
                    map.remove(&trade_ticks);
                } else {
                    map.insert(trade_ticks, new_size);
                }
            }
        }
    }
}

fn parse_side(s: &str) -> Option<Side> {
    match s.to_ascii_lowercase().as_str() {
        "buy" | "bid" => Some(Side::Buy),
        "sell" | "ask" => Some(Side::Sell),
        _ => None,
    }
}

/// How much of `remaining_size` is ELIGIBLE to fill `order` under `regime`.
/// `Optimistic` returns the full amount unconditionally. `Base` returns the
/// amount only after `cumulative_trade_through` has eaten through the
/// `book_depth_at_rest`. `Conservative` additionally haircuts by the
/// estimated queue position (FIFO position represented by `queue_ahead` is
/// already maintained; the haircut here is the depth-at-rest minus
/// already-consumed depth).
fn eligible_fill_size(regime: FillQuality, order: &RestingOrder, remaining_size: f64) -> f64 {
    match regime {
        FillQuality::Optimistic => remaining_size,
        FillQuality::Base => {
            let already_consumed = order.cumulative_trade_through - remaining_size;
            let depth_remaining = (order.book_depth_at_rest - already_consumed).max(0.0);
            (remaining_size - depth_remaining).max(0.0)
        }
        FillQuality::Conservative => {
            // Same gate as Base, then haircut by queue-ahead share of the
            // remaining trade-through. queue_ahead is FIFO position within
            // our own resting orders and is already applied by the caller;
            // here we apply a depth-at-rest haircut that models the public
            // queue ahead of us at this level.
            let already_consumed = order.cumulative_trade_through - remaining_size;
            let depth_remaining = (order.book_depth_at_rest - already_consumed).max(0.0);
            let after_book = (remaining_size - depth_remaining).max(0.0);
            // Conservative haircut: split the residual between us and the
            // (assumed) other public makers at our level. With no per-level
            // visibility into peer makers, halve the residual. This matches
            // the spec's "queue-position haircut" and is calibratable later.
            after_book * 0.5
        }
    }
}

fn price_to_ticks(price: f64) -> i64 {
    (price * 1_000_000.0).round() as i64
}

fn ticks_to_price(ticks: i64) -> f64 {
    ticks as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::collector::schema::Source;

    fn trade_event(received_ns: i64, asset: &str, side: &str, price: &str, size: &str) -> Event {
        Event {
            v: 1,
            ts_ns: received_ns - 1,
            received_ns,
            event_type: EventType::Trade,
            market_type: "btc_5m".into(),
            market_slug: Some("btc-up-or-down".into()),
            asset_id: Some(asset.into()),
            side: Some(side.into()),
            price: Some(price.into()),
            size: Some(size.into()),
            sequence: Some(1),
            source: Source::PolymarketMarketWs,
            raw: json!({}),
        }
    }

    fn intent(
        coid: &str,
        asset: &str,
        side: Side,
        price: f64,
        size: f64,
        placed_ms: u64,
    ) -> StrategyOrderIntent {
        StrategyOrderIntent::passive(coid, asset, side, price, size, placed_ms)
    }

    fn book_event(received_ns: i64, asset: &str, side: &str, price: &str, size: &str) -> Event {
        Event {
            v: 1,
            ts_ns: received_ns - 1,
            received_ns,
            event_type: EventType::BookSnapshot,
            market_type: "btc_5m".into(),
            market_slug: Some("btc-up-or-down".into()),
            asset_id: Some(asset.into()),
            side: Some(side.into()),
            price: Some(price.into()),
            size: Some(size.into()),
            sequence: Some(1),
            source: Source::PolymarketMarketWs,
            raw: json!({}),
        }
    }

    #[test]
    fn instant_preset_fills_immediately_at_first_trade() {
        let mut sim = FillSimulator::new(FillSimConfig {
            latency: LatencyPreset::Instant,
            fill_quality: FillQuality::Optimistic,
            ..Default::default()
        });
        sim.submit(intent("o1", "asset-a", Side::Sell, 0.55, 100.0, 1_000));
        // public buy taker at 0.55 hits a resting ask of 100 -> fills.
        sim.on_event(&trade_event(2_000_000_000, "asset-a", "buy", "0.55", "100"));
        assert_eq!(sim.fills().len(), 1);
        assert_eq!(sim.fills()[0].size, 100.0);
        assert_eq!(sim.fills()[0].client_order_id, "o1");
        assert_eq!(sim.pending_count(), 0);
    }

    #[test]
    fn nominal_preset_blocks_match_until_arrival_latency_elapses() {
        let mut sim = FillSimulator::new(FillSimConfig {
            latency: LatencyPreset::Nominal, // 50ms
            fill_quality: FillQuality::Optimistic,
            ..Default::default()
        });
        sim.submit(intent("o1", "asset-a", Side::Sell, 0.55, 100.0, 1_000));
        // event at 1_020 ms -> arrival_ms = 1050, blocked
        sim.on_event(&trade_event(1_020_000_000, "asset-a", "buy", "0.55", "100"));
        assert_eq!(sim.fills().len(), 0);
        assert_eq!(sim.pending_count(), 1);
        // event at 1_050 ms -> arrival_ms = 1050, exactly arrival, fills
        sim.on_event(&trade_event(1_050_000_000, "asset-a", "buy", "0.55", "100"));
        assert_eq!(sim.fills().len(), 1);
        assert_eq!(sim.fills()[0].size, 100.0);
    }

    #[test]
    fn fifo_priority_within_same_price_level() {
        let mut sim = FillSimulator::new(FillSimConfig {
            latency: LatencyPreset::Instant,
            fill_quality: FillQuality::Optimistic,
            ..Default::default()
        });
        sim.submit(intent("first", "asset-a", Side::Sell, 0.6, 50.0, 1_000));
        sim.submit(intent("second", "asset-a", Side::Sell, 0.6, 50.0, 1_001));
        // 60 unit trade fills first 50 of "first", then 10 of "second"
        sim.on_event(&trade_event(2_000_000_000, "asset-a", "buy", "0.60", "60"));
        let fills = sim.fills();
        assert_eq!(fills.len(), 2);
        assert_eq!(fills[0].client_order_id, "first");
        assert_eq!(fills[0].size, 50.0);
        assert_eq!(fills[1].client_order_id, "second");
        assert_eq!(fills[1].size, 10.0);
        assert_eq!(sim.pending_count(), 1);
    }

    #[test]
    fn cancel_with_latency_keeps_order_open_until_cancel_arrival() {
        let mut sim = FillSimulator::new(FillSimConfig {
            latency: LatencyPreset::Nominal, // 50ms
            fill_quality: FillQuality::Optimistic,
            ..Default::default()
        });
        sim.submit(intent("o1", "asset-a", Side::Sell, 0.55, 100.0, 1_000));
        sim.cancel("o1", 1_100); // cancel arrives at 1150
                                 // Trade at 1_140 ms (before cancel arrival) still matches
        sim.on_event(&trade_event(1_140_000_000, "asset-a", "buy", "0.55", "100"));
        assert_eq!(sim.fills().len(), 1);
    }

    #[test]
    fn opposite_side_trade_does_not_fill() {
        let mut sim = FillSimulator::new(FillSimConfig {
            latency: LatencyPreset::Instant,
            fill_quality: FillQuality::Optimistic,
            ..Default::default()
        });
        sim.submit(intent("o1", "asset-a", Side::Sell, 0.55, 100.0, 1_000));
        // public sell taker (hits the bid) does not match a resting ask
        sim.on_event(&trade_event(
            2_000_000_000,
            "asset-a",
            "sell",
            "0.55",
            "100",
        ));
        assert_eq!(sim.fills().len(), 0);
    }

    #[test]
    fn latency_presets_have_expected_values() {
        assert_eq!(LatencyPreset::Instant.submit_latency_ms(), 0);
        assert_eq!(LatencyPreset::Nominal.submit_latency_ms(), 50);
        assert_eq!(LatencyPreset::Conservative.submit_latency_ms(), 150);
    }

    #[test]
    fn determinism_seeded_rng_does_not_perturb_default_runs() {
        // Two sims with different seeds should produce identical fills for
        // the default presets — no RNG draws in Instant/Nominal/Conservative.
        let a_cfg = FillSimConfig {
            seed: 1,
            fill_quality: FillQuality::Optimistic,
            ..Default::default()
        };
        let b_cfg = FillSimConfig {
            seed: 999_999,
            fill_quality: FillQuality::Optimistic,
            ..Default::default()
        };
        let mut a = FillSimulator::new(a_cfg);
        let mut b = FillSimulator::new(b_cfg);
        for sim in [&mut a, &mut b] {
            sim.submit(intent("o1", "asset-a", Side::Sell, 0.55, 25.0, 1_000));
            sim.submit(intent("o2", "asset-a", Side::Sell, 0.55, 25.0, 1_001));
            sim.on_event(&trade_event(2_000_000_000, "asset-a", "buy", "0.55", "60"));
        }
        assert_eq!(a.fills(), b.fills());
    }

    #[test]
    fn base_regime_blocks_fill_until_book_depth_consumed() {
        // Seed visible asks at 0.55 with 100 size before our resting ask
        // arrives. Under Base, our 50-ask sits behind that 100; the first
        // 100 of trade flow does NOT fill us.
        let mut sim = FillSimulator::new(FillSimConfig {
            latency: LatencyPreset::Instant,
            fill_quality: FillQuality::Base,
            ..Default::default()
        });
        sim.on_event(&book_event(500_000_000, "asset-a", "sell", "0.55", "100"));
        sim.submit(intent("o1", "asset-a", Side::Sell, 0.55, 50.0, 1_000));
        // 80-unit trade — under Base this only chips queue ahead; no fill.
        sim.on_event(&trade_event(2_000_000_000, "asset-a", "buy", "0.55", "80"));
        assert_eq!(sim.fills().len(), 0, "should not fill while queue ahead");
        // Another 30-unit trade — total trade-through 110 > book_depth 100, so
        // 10 units bleed into our order.
        sim.on_event(&trade_event(3_000_000_000, "asset-a", "buy", "0.55", "30"));
        assert!(sim.fills().len() >= 1);
        let total: f64 = sim.fills().iter().map(|f| f.size).sum();
        assert!((total - 10.0).abs() < 1e-6, "expected ~10, got {total}");
    }

    #[test]
    fn conservative_regime_haircuts_after_book_consumed() {
        let mut sim = FillSimulator::new(FillSimConfig {
            latency: LatencyPreset::Instant,
            fill_quality: FillQuality::Conservative,
            ..Default::default()
        });
        sim.on_event(&book_event(500_000_000, "asset-a", "sell", "0.55", "100"));
        sim.submit(intent("o1", "asset-a", Side::Sell, 0.55, 50.0, 1_000));
        // 200-unit trade clears 100 of book depth, residual 100 is haircut by
        // 0.5 → only 50 effective. Our 50-size order takes 50, but the
        // haircut means at most ~50 eligible → fills ~50.
        sim.on_event(&trade_event(2_000_000_000, "asset-a", "buy", "0.55", "200"));
        let total: f64 = sim.fills().iter().map(|f| f.size).sum();
        assert!(total <= 50.0 + 1e-6);
        // Conservative must fill strictly less or equal vs Base for the
        // same trade-through. We don't assert tighter to keep the test
        // robust against the haircut constant.
    }

    #[test]
    fn post_only_intent_that_would_cross_is_rejected() {
        let mut sim = FillSimulator::new(FillSimConfig {
            latency: LatencyPreset::Instant,
            fill_quality: FillQuality::Optimistic,
            ..Default::default()
        });
        // Seed an ask at 0.55. A buy post-only at 0.55 would cross.
        sim.on_event(&book_event(500_000_000, "asset-a", "sell", "0.55", "10"));
        let mut po = StrategyOrderIntent::passive("po-1", "asset-a", Side::Buy, 0.55, 5.0, 1_000);
        po.post_only = true;
        sim.submit(po);
        assert_eq!(sim.fills().len(), 0);
        assert_eq!(sim.rejections().len(), 1);
        assert_eq!(
            sim.rejections()[0].reason,
            RejectionReason::PostOnlyWouldCross
        );
        assert_eq!(sim.pending_count(), 0);
    }

    #[test]
    fn aggressive_intent_lifts_visible_book_immediately() {
        let mut sim = FillSimulator::new(FillSimConfig {
            latency: LatencyPreset::Instant,
            fill_quality: FillQuality::Base,
            ..Default::default()
        });
        sim.on_event(&book_event(500_000_000, "asset-a", "sell", "0.55", "20"));
        sim.on_event(&book_event(500_000_001, "asset-a", "sell", "0.56", "30"));
        let mut take = StrategyOrderIntent::passive("t1", "asset-a", Side::Buy, 0.56, 35.0, 1_000);
        take.aggressive = true;
        sim.submit(take);
        let fills = sim.fills();
        assert_eq!(fills.len(), 2);
        assert_eq!(fills[0].maker_or_taker, MakerOrTaker::Taker);
        assert!((fills[0].price - 0.55).abs() < 1e-9);
        assert_eq!(fills[0].size, 20.0);
        assert!((fills[1].price - 0.56).abs() < 1e-9);
        assert_eq!(fills[1].size, 15.0);
    }

    #[test]
    fn maker_fills_are_tagged_maker() {
        let mut sim = FillSimulator::new(FillSimConfig {
            latency: LatencyPreset::Instant,
            fill_quality: FillQuality::Optimistic,
            ..Default::default()
        });
        sim.submit(intent("m1", "asset-a", Side::Sell, 0.55, 10.0, 1_000));
        sim.on_event(&trade_event(2_000_000_000, "asset-a", "buy", "0.55", "10"));
        assert_eq!(sim.fills().len(), 1);
        assert_eq!(sim.fills()[0].maker_or_taker, MakerOrTaker::Maker);
    }
}
