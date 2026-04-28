//! Shadow standing-order book + trade-tape replay simulator.
//!
//! Maintains every standing shadow order in memory and decides which orders
//! fill when a real trade prints on the live tape. Uses queue_model.rs to
//! discount fill probability by FIFO queue position.
//!
//! Matching rule: a `TradeEvent { taker_side, price, size }` matches a
//! resting order on the OPPOSITE side at-or-better than the trade price.
//! "At or better" from the resting order's perspective:
//!   - resting BUY  at P_order matches taker SELL at trade_price <= P_order
//!   - resting SELL at P_order matches taker BUY  at trade_price >= P_order
//!
//! Per-order fill quantity for a given trade:
//!   depth_ahead_pre = depth_ahead_at_post
//!                       - decay * elapsed_seconds
//!                       - cumulative_volume_pre_this_trade
//!   fill_qty = max(0, min(remaining_qty, trade.size - depth_ahead_pre))
//!
//! After computing fill_qty, cumulative_volume is incremented by the FULL
//! trade.size for ALL matching orders at the level, since the same trade
//! depletes the queue from every same-level resting order's perspective.

use std::collections::{HashMap, HashSet};

use crate::core::types::{
    ClientOrderId, EpochMillis, FillLiquidity, FillReport, InstrumentId, OrderIntent, TradeSide,
};
use crate::paper::queue_model::{depth_ahead_remaining, DepthInputs, QueueDecayEstimator};

#[derive(Debug, Clone, PartialEq)]
pub struct TradeEvent {
    pub asset_id: InstrumentId,
    pub taker_side: TradeSide,
    pub price: f64,
    pub size: f64,
    pub event_at_ms: EpochMillis,
    pub trade_id: String,
    pub synthesised: bool,
}

#[derive(Debug, Clone)]
pub struct ShadowOrder {
    pub intent: OrderIntent,
    pub posted_at_ms: EpochMillis,
    pub expires_at_ms: Option<EpochMillis>,
    pub depth_ahead_at_post: f64,
    pub cumulative_volume_at_or_better: f64,
    pub remaining_qty: f64,
    pub market_family: String,
}

#[derive(Debug, Default)]
pub struct ShadowBook {
    orders: HashMap<ClientOrderId, ShadowOrder>,
    seen_trade_ids: HashSet<String>,
    estimator: QueueDecayEstimator,
    fifo_violation_count: u64,
}

impl ShadowBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_estimator(estimator: QueueDecayEstimator) -> Self {
        Self {
            estimator,
            ..Self::default()
        }
    }

    pub fn open_order_count(&self) -> usize {
        self.orders.len()
    }

    pub fn rate_for(&self, family: &str) -> f64 {
        self.estimator.rate_for(family)
    }

    pub fn n_observations(&self, family: &str) -> u32 {
        self.estimator.n_observations(family)
    }

    pub fn fifo_violation_count(&self) -> u64 {
        self.fifo_violation_count
    }

    pub fn on_submit(
        &mut self,
        intent: OrderIntent,
        expires_at_ms: Option<EpochMillis>,
        depth_ahead_at_post: f64,
        posted_at_ms: EpochMillis,
        market_family: impl Into<String>,
    ) -> bool {
        if !depth_ahead_at_post.is_finite() || depth_ahead_at_post < 0.0 {
            return false;
        }
        let order = ShadowOrder {
            remaining_qty: intent.quantity,
            posted_at_ms,
            expires_at_ms,
            depth_ahead_at_post,
            cumulative_volume_at_or_better: 0.0,
            market_family: market_family.into(),
            intent,
        };
        self.orders
            .insert(order.intent.client_order_id.clone(), order);
        true
    }

    pub fn on_cancel(&mut self, client_order_id: &ClientOrderId) -> bool {
        self.orders.remove(client_order_id).is_some()
    }

    pub fn sweep_expired(&mut self, now_ms: EpochMillis) -> usize {
        let before = self.orders.len();
        self.orders.retain(|_, o| match o.expires_at_ms {
            Some(exp) => exp > now_ms,
            None => true,
        });
        before - self.orders.len()
    }

    pub fn on_trade_event(&mut self, trade: &TradeEvent) -> Vec<FillReport> {
        if !self.seen_trade_ids.insert(trade.trade_id.clone()) {
            return vec![];
        }
        self.sweep_expired(trade.event_at_ms);

        let resting_side = match trade.taker_side {
            TradeSide::Buy => TradeSide::Sell,
            TradeSide::Sell => TradeSide::Buy,
        };

        let mut matching_ids: Vec<ClientOrderId> = self
            .orders
            .values()
            .filter(|o| {
                o.intent.instrument_id == trade.asset_id
                    && o.intent.side == resting_side
                    && match resting_side {
                        TradeSide::Sell => o.intent.limit_price <= trade.price,
                        TradeSide::Buy => o.intent.limit_price >= trade.price,
                    }
                    && o.remaining_qty > 1e-9
            })
            .map(|o| o.intent.client_order_id.clone())
            .collect();
        matching_ids.sort_by_key(|id| {
            self.orders
                .get(id)
                .map(|o| o.posted_at_ms)
                .unwrap_or(EpochMillis::MAX)
        });

        let mut fills = Vec::new();
        let trade_size = trade.size;
        for id in &matching_ids {
            let Some(order) = self.orders.get_mut(id) else {
                continue;
            };
            let elapsed_seconds = trade
                .event_at_ms
                .saturating_sub(order.posted_at_ms) as f64
                / 1000.0;
            let rate = self.estimator.rate_for(&order.market_family);
            let depth_ahead_pre = depth_ahead_remaining(
                DepthInputs {
                    depth_ahead_at_post: order.depth_ahead_at_post,
                    elapsed_seconds,
                    cumulative_volume_at_or_better: order.cumulative_volume_at_or_better,
                },
                rate,
            );
            order.cumulative_volume_at_or_better += trade_size;
            if depth_ahead_pre.is_nan() {
                continue;
            }
            let fill_qty = (trade_size - depth_ahead_pre)
                .max(0.0)
                .min(order.remaining_qty);
            if fill_qty > 0.0 {
                fills.push(FillReport {
                    order_id: None,
                    client_order_id: Some(order.intent.client_order_id.clone()),
                    market_id: order.intent.market_id.clone(),
                    instrument_id: order.intent.instrument_id.clone(),
                    side: order.intent.side,
                    price: order.intent.limit_price,
                    quantity: fill_qty,
                    fee_usd: 0.0,
                    liquidity: FillLiquidity::Maker,
                    close_method: None,
                    observed_at_ms: trade.event_at_ms,
                });
                order.remaining_qty -= fill_qty;
            }
        }

        self.orders.retain(|_, o| o.remaining_qty > 1e-9);
        fills
    }

    pub fn record_observed_fill(
        &mut self,
        family: &str,
        depth_ahead_at_post: f64,
        elapsed_seconds: f64,
        cumulative_volume_at_or_better: f64,
    ) -> f64 {
        self.estimator.update_with_fill(
            family,
            depth_ahead_at_post,
            elapsed_seconds,
            cumulative_volume_at_or_better,
        )
    }

    pub fn note_fifo_violation(&mut self) {
        self.fifo_violation_count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{IntentKind, MarketId};

    fn intent(coid: &str, asset: &str, side: TradeSide, price: f64, qty: f64) -> OrderIntent {
        OrderIntent {
            client_order_id: ClientOrderId::from(coid.to_string()),
            market_id: MarketId::from("m1"),
            instrument_id: InstrumentId::from(asset.to_string()),
            side,
            limit_price: price,
            quantity: qty,
            reduce_only: false,
            reason: "test".to_string(),
            quote_level_tag: Some("test".to_string()),
            created_at_ms: 1_000,
            pair_id: None,
            kind: IntentKind::Entry,
        }
    }

    fn trade(asset: &str, taker: TradeSide, price: f64, size: f64, ts: EpochMillis) -> TradeEvent {
        TradeEvent {
            asset_id: InstrumentId::from(asset.to_string()),
            taker_side: taker,
            price,
            size,
            event_at_ms: ts,
            trade_id: format!("t-{}", ts),
            synthesised: false,
        }
    }

    fn calibrated_book(rate: f64) -> ShadowBook {
        let mut est = QueueDecayEstimator::new(1, 1.0);
        est.update_with_fill("fam", rate * 10.0, 10.0, 0.0);
        ShadowBook::with_estimator(est)
    }

    #[test]
    fn submit_records_open_order() {
        let mut book = ShadowBook::new();
        let ok = book.on_submit(
            intent("c1", "a1", TradeSide::Buy, 0.49, 100.0),
            Some(2_000),
            50.0,
            1_000,
            "fam",
        );
        assert!(ok);
        assert_eq!(book.open_order_count(), 1);
    }

    #[test]
    fn submit_rejects_invalid_depth() {
        let mut book = ShadowBook::new();
        assert!(!book.on_submit(
            intent("c1", "a1", TradeSide::Buy, 0.49, 100.0),
            None,
            f64::NAN,
            1_000,
            "fam",
        ));
        assert!(!book.on_submit(
            intent("c1", "a1", TradeSide::Buy, 0.49, 100.0),
            None,
            -1.0,
            1_000,
            "fam",
        ));
        assert_eq!(book.open_order_count(), 0);
    }

    #[test]
    fn cancel_removes_order() {
        let mut book = ShadowBook::new();
        let coid = ClientOrderId::from("c1".to_string());
        book.on_submit(
            intent("c1", "a1", TradeSide::Buy, 0.49, 100.0),
            None,
            0.0,
            1_000,
            "fam",
        );
        assert!(book.on_cancel(&coid));
        assert!(!book.on_cancel(&coid));
        assert_eq!(book.open_order_count(), 0);
    }

    #[test]
    fn dedup_by_trade_id() {
        let mut book = calibrated_book(0.0);
        book.on_submit(
            intent("c1", "a1", TradeSide::Buy, 0.49, 100.0),
            None,
            0.0,
            1_000,
            "fam",
        );
        let t = trade("a1", TradeSide::Sell, 0.49, 50.0, 5_000);
        let f1 = book.on_trade_event(&t);
        let f2 = book.on_trade_event(&t);
        assert_eq!(f1.len(), 1);
        assert_eq!(f2.len(), 0);
    }

    #[test]
    fn expired_orders_are_swept_before_match() {
        let mut book = calibrated_book(0.0);
        book.on_submit(
            intent("c1", "a1", TradeSide::Buy, 0.49, 100.0),
            Some(3_000),
            0.0,
            1_000,
            "fam",
        );
        let t = trade("a1", TradeSide::Sell, 0.49, 50.0, 5_000);
        let fills = book.on_trade_event(&t);
        assert_eq!(fills.len(), 0, "expired order must not fill");
        assert_eq!(book.open_order_count(), 0, "expired order must be removed");
    }

    #[test]
    fn taker_sell_hits_resting_buy_at_or_above_price() {
        let mut book = calibrated_book(0.0);
        book.on_submit(
            intent("c1", "a1", TradeSide::Buy, 0.50, 100.0),
            None,
            0.0,
            1_000,
            "fam",
        );
        let t = trade("a1", TradeSide::Sell, 0.49, 50.0, 5_000);
        let fills = book.on_trade_event(&t);
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].side, TradeSide::Buy);
        assert!((fills[0].price - 0.50).abs() < 1e-9);
        assert!((fills[0].quantity - 50.0).abs() < 1e-9);
    }

    #[test]
    fn taker_buy_hits_resting_sell_at_or_below_price() {
        let mut book = calibrated_book(0.0);
        book.on_submit(
            intent("c1", "a1", TradeSide::Sell, 0.50, 100.0),
            None,
            0.0,
            1_000,
            "fam",
        );
        let t = trade("a1", TradeSide::Buy, 0.51, 50.0, 5_000);
        let fills = book.on_trade_event(&t);
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].side, TradeSide::Sell);
    }

    #[test]
    fn mismatched_side_does_not_fill() {
        let mut book = calibrated_book(0.0);
        book.on_submit(
            intent("c1", "a1", TradeSide::Buy, 0.49, 100.0),
            None,
            0.0,
            1_000,
            "fam",
        );
        // Taker buy at our price means resting SELLs got hit, not our BUY.
        let t = trade("a1", TradeSide::Buy, 0.49, 50.0, 5_000);
        assert_eq!(book.on_trade_event(&t).len(), 0);
    }

    #[test]
    fn no_fill_when_estimator_uncalibrated() {
        let mut book = ShadowBook::new();
        book.on_submit(
            intent("c1", "a1", TradeSide::Buy, 0.49, 100.0),
            None,
            0.0,
            1_000,
            "fam",
        );
        let t = trade("a1", TradeSide::Sell, 0.49, 50.0, 5_000);
        assert_eq!(
            book.on_trade_event(&t).len(),
            0,
            "uncalibrated must emit no fills"
        );
        // Order should still be standing for retry once calibrated
        assert_eq!(book.open_order_count(), 1);
    }

    #[test]
    fn fill_qty_capped_by_trade_size_minus_depth_ahead() {
        let mut book = calibrated_book(0.0);
        book.on_submit(
            intent("c1", "a1", TradeSide::Buy, 0.49, 100.0),
            None,
            30.0, // 30 ahead in queue
            1_000,
            "fam",
        );
        let t = trade("a1", TradeSide::Sell, 0.49, 50.0, 5_000);
        let fills = book.on_trade_event(&t);
        assert_eq!(fills.len(), 1);
        // 50 - 30 = 20 fills us
        assert!((fills[0].quantity - 20.0).abs() < 1e-9);
    }

    #[test]
    fn fill_qty_zero_when_trade_size_lt_depth_ahead() {
        let mut book = calibrated_book(0.0);
        book.on_submit(
            intent("c1", "a1", TradeSide::Buy, 0.49, 100.0),
            None,
            100.0,
            1_000,
            "fam",
        );
        let t = trade("a1", TradeSide::Sell, 0.49, 50.0, 5_000);
        assert_eq!(book.on_trade_event(&t).len(), 0);
        // Order remains, and cumulative_volume incremented
        assert_eq!(book.open_order_count(), 1);
    }

    #[test]
    fn fill_qty_capped_by_remaining_qty() {
        let mut book = calibrated_book(0.0);
        book.on_submit(
            intent("c1", "a1", TradeSide::Buy, 0.49, 10.0),
            None,
            0.0,
            1_000,
            "fam",
        );
        let t = trade("a1", TradeSide::Sell, 0.49, 1000.0, 5_000);
        let fills = book.on_trade_event(&t);
        assert_eq!(fills.len(), 1);
        assert!((fills[0].quantity - 10.0).abs() < 1e-9);
        assert_eq!(book.open_order_count(), 0, "fully filled order is removed");
    }

    #[test]
    fn partial_fill_decrements_remaining_qty() {
        let mut book = calibrated_book(0.0);
        book.on_submit(
            intent("c1", "a1", TradeSide::Buy, 0.49, 100.0),
            None,
            0.0,
            1_000,
            "fam",
        );
        let t1 = trade("a1", TradeSide::Sell, 0.49, 30.0, 5_000);
        let f1 = book.on_trade_event(&t1);
        assert!((f1[0].quantity - 30.0).abs() < 1e-9);
        assert_eq!(book.open_order_count(), 1);
        let t2 = trade("a1", TradeSide::Sell, 0.49, 30.0, 5_001);
        let f2 = book.on_trade_event(&t2);
        assert!((f2[0].quantity - 30.0).abs() < 1e-9);
        assert_eq!(book.open_order_count(), 1);
    }

    #[test]
    fn fifo_across_multiple_orders_at_same_level() {
        let mut book = calibrated_book(0.0);
        // Earlier order has less depth ahead (e.g. depth_ahead=0)
        book.on_submit(
            intent("c1", "a1", TradeSide::Buy, 0.49, 30.0),
            None,
            0.0,
            1_000,
            "fam",
        );
        // Later order has more depth ahead (it is behind c1's 30 + 70 others)
        book.on_submit(
            intent("c2", "a1", TradeSide::Buy, 0.49, 30.0),
            None,
            100.0,
            2_000,
            "fam",
        );
        let t = trade("a1", TradeSide::Sell, 0.49, 50.0, 5_000);
        let fills = book.on_trade_event(&t);
        // c1 (FIFO front) takes 30 (its remaining qty); c2 needs 100 ahead
        // depleted before any fill, but only 50 - 30 = 20 left, which is
        // less than its 100 depth_ahead, so c2 gets 0.
        assert_eq!(fills.len(), 1);
        assert_eq!(
            fills[0].client_order_id.as_ref().unwrap().as_str(),
            "c1"
        );
    }
}
