//! Pre-trade risk limits and reject reasoning for order intents.

use crate::event_log::{EventCategory, EventMetrics, EventRecord};
use crate::inventory::InventoryState;
use crate::types::{EpochMillis, OrderIntent, TradeSide};

#[derive(Clone, Debug, PartialEq)]
pub struct RiskLimits {
    pub max_order_notional_usd: f64,
    pub max_gross_notional_usd: f64,
    pub max_net_notional_per_market_usd: f64,
    pub max_position_quantity_per_instrument: f64,
    pub min_free_cash_usd: f64,
    pub max_open_orders_total: usize,
    pub max_open_orders_per_market: usize,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_order_notional_usd: 250.0,
            max_gross_notional_usd: 1_000.0,
            max_net_notional_per_market_usd: 500.0,
            max_position_quantity_per_instrument: 10_000.0,
            min_free_cash_usd: 0.0,
            max_open_orders_total: 32,
            max_open_orders_per_market: 8,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RiskContext {
    pub open_orders_total: usize,
    pub open_orders_for_market: usize,
    pub now_ms: EpochMillis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RiskRejectReason {
    InvalidOrder,
    OrderNotionalTooLarge,
    GrossExposureTooLarge,
    MarketNetExposureTooLarge,
    FreeCashTooLow,
    PositionQuantityTooLarge,
    TooManyOpenOrders,
    TooManyOpenOrdersForMarket,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RiskDecision {
    pub accepted: bool,
    pub reject_reason: Option<RiskRejectReason>,
    pub message: String,
    pub evaluated_at_ms: EpochMillis,
    pub projected_free_cash_usd: f64,
    pub projected_gross_notional_usd: f64,
    pub projected_market_net_notional_usd: f64,
}

impl RiskDecision {
    pub fn to_event(&self, order: &OrderIntent) -> EventRecord {
        EventRecord::new(
            EventCategory::Risk,
            self.evaluated_at_ms,
            self.message.clone(),
        )
        .with_market(order.market_id.clone())
        .with_instrument(order.instrument_id.clone())
        .with_client_order(order.client_order_id.clone())
        .with_metrics(EventMetrics {
            price: Some(order.limit_price),
            quantity: Some(order.quantity),
            notional_usd: Some(order.notional_usd()),
            cash_delta_usd: None,
            position_delta: Some(order.quantity * order.side.sign()),
            free_cash_after_usd: Some(self.projected_free_cash_usd),
            gross_exposure_after_usd: Some(self.projected_gross_notional_usd),
            risk_reject_reason: self.reject_reason.map(|reason| format!("{reason:?}")),
        })
    }
}

#[derive(Clone, Debug)]
pub struct RiskEngine {
    limits: RiskLimits,
}

impl RiskEngine {
    pub fn new(limits: RiskLimits) -> Self {
        Self { limits }
    }

    pub fn limits(&self) -> &RiskLimits {
        &self.limits
    }

    pub fn evaluate(
        &self,
        inventory: &InventoryState,
        order: &OrderIntent,
        context: &RiskContext,
    ) -> RiskDecision {
        if order.limit_price <= 0.0 || order.quantity <= 0.0 {
            return self.reject(
                RiskRejectReason::InvalidOrder,
                context.now_ms.max(order.created_at_ms),
                inventory.free_cash_usd(),
                inventory.gross_exposure_usd(),
                inventory
                    .net_exposure_for_market_usd(&order.market_id)
                    .abs(),
                "order price and quantity must be positive",
            );
        }

        // Hedge-rescue intents are CLOSE operations — they manufacture the
        // missing leg of an existing stranded position so the pair can be
        // merged for $1 collateral release. They reduce exposure, not add
        // to it. Entry-time caps (max_open_orders, max_position_quantity,
        // max_order_notional) protect against accumulation runaway and
        // shouldn't apply. The merge will return the rescue's cash within
        // ~30s, and wallet exhaustion is gated upstream by the adapter
        // balance check. Still enforce InvalidOrder above and the
        // sufficient-balance check (rescue can't spend cash we don't have).
        let is_rescue = order
            .quote_level_tag
            .as_deref()
            .is_some_and(|tag| tag.starts_with("mm-hedge-rescue"));

        let notional = order.notional_usd();
        if !is_rescue && notional > self.limits.max_order_notional_usd {
            return self.reject(
                RiskRejectReason::OrderNotionalTooLarge,
                context.now_ms.max(order.created_at_ms),
                inventory.free_cash_usd(),
                inventory.gross_exposure_usd(),
                inventory
                    .net_exposure_for_market_usd(&order.market_id)
                    .abs(),
                "order notional exceeds max_order_notional_usd",
            );
        }

        if !is_rescue && context.open_orders_total >= self.limits.max_open_orders_total {
            return self.reject(
                RiskRejectReason::TooManyOpenOrders,
                context.now_ms.max(order.created_at_ms),
                inventory.free_cash_usd(),
                inventory.gross_exposure_usd(),
                inventory
                    .net_exposure_for_market_usd(&order.market_id)
                    .abs(),
                "open order count exceeds max_open_orders_total",
            );
        }

        if !is_rescue && context.open_orders_for_market >= self.limits.max_open_orders_per_market {
            return self.reject(
                RiskRejectReason::TooManyOpenOrdersForMarket,
                context.now_ms.max(order.created_at_ms),
                inventory.free_cash_usd(),
                inventory.gross_exposure_usd(),
                inventory
                    .net_exposure_for_market_usd(&order.market_id)
                    .abs(),
                "open order count exceeds max_open_orders_per_market",
            );
        }

        let current_position_qty = inventory.position_qty(&order.instrument_id);
        let projected_position_qty = current_position_qty + (order.quantity * order.side.sign());
        if projected_position_qty.abs() > self.limits.max_position_quantity_per_instrument {
            return self.reject(
                RiskRejectReason::PositionQuantityTooLarge,
                context.now_ms.max(order.created_at_ms),
                inventory.free_cash_usd(),
                inventory.gross_exposure_usd(),
                inventory
                    .net_exposure_for_market_usd(&order.market_id)
                    .abs(),
                "projected position quantity exceeds max_position_quantity_per_instrument",
            );
        }

        if order.side == TradeSide::Sell && current_position_qty + 1e-9 < order.quantity {
            return self.reject(
                RiskRejectReason::PositionQuantityTooLarge,
                context.now_ms.max(order.created_at_ms),
                inventory.free_cash_usd(),
                inventory.gross_exposure_usd(),
                inventory
                    .net_exposure_for_market_usd(&order.market_id)
                    .abs(),
                "sell quantity exceeds current long inventory",
            );
        }

        let projected_free_cash_usd = match order.side {
            TradeSide::Buy => inventory.free_cash_usd() - notional,
            TradeSide::Sell => inventory.free_cash_usd(),
        };
        if projected_free_cash_usd < self.limits.min_free_cash_usd {
            return self.reject(
                RiskRejectReason::FreeCashTooLow,
                context.now_ms.max(order.created_at_ms),
                projected_free_cash_usd,
                inventory.gross_exposure_usd(),
                inventory
                    .net_exposure_for_market_usd(&order.market_id)
                    .abs(),
                "projected free cash falls below min_free_cash_usd",
            );
        }

        let current_gross = inventory.gross_exposure_usd();
        let projected_gross_notional_usd = match order.side {
            TradeSide::Buy => current_gross + notional,
            TradeSide::Sell => {
                let mark = inventory
                    .position(&order.instrument_id)
                    .map(|position| position.mark_or_cost())
                    .unwrap_or(order.limit_price);
                let reducible = current_position_qty.min(order.quantity) * mark;
                (current_gross - reducible).max(0.0)
            }
        };
        if projected_gross_notional_usd > self.limits.max_gross_notional_usd {
            return self.reject(
                RiskRejectReason::GrossExposureTooLarge,
                context.now_ms.max(order.created_at_ms),
                projected_free_cash_usd,
                projected_gross_notional_usd,
                inventory
                    .net_exposure_for_market_usd(&order.market_id)
                    .abs(),
                "projected gross exposure exceeds max_gross_notional_usd",
            );
        }

        let projected_market_net_notional_usd =
            (inventory.net_exposure_for_market_usd(&order.market_id) + order.signed_notional_usd())
                .abs();
        if projected_market_net_notional_usd > self.limits.max_net_notional_per_market_usd {
            return self.reject(
                RiskRejectReason::MarketNetExposureTooLarge,
                context.now_ms.max(order.created_at_ms),
                projected_free_cash_usd,
                projected_gross_notional_usd,
                projected_market_net_notional_usd,
                "projected market net exposure exceeds max_net_notional_per_market_usd",
            );
        }

        RiskDecision {
            accepted: true,
            reject_reason: None,
            message: "risk check passed".into(),
            evaluated_at_ms: context.now_ms.max(order.created_at_ms),
            projected_free_cash_usd,
            projected_gross_notional_usd,
            projected_market_net_notional_usd,
        }
    }

    fn reject(
        &self,
        reject_reason: RiskRejectReason,
        evaluated_at_ms: EpochMillis,
        projected_free_cash_usd: f64,
        projected_gross_notional_usd: f64,
        projected_market_net_notional_usd: f64,
        message: impl Into<String>,
    ) -> RiskDecision {
        RiskDecision {
            accepted: false,
            reject_reason: Some(reject_reason),
            message: message.into(),
            evaluated_at_ms,
            projected_free_cash_usd,
            projected_gross_notional_usd,
            projected_market_net_notional_usd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RiskContext, RiskEngine, RiskLimits, RiskRejectReason};
    use crate::inventory::InventoryState;
    use crate::types::{ClientOrderId, InstrumentId, MarketId, OrderIntent, TradeSide};

    #[test]
    fn rejects_buy_that_exceeds_free_cash_floor() {
        let inventory = InventoryState::new(10.0);
        let risk = RiskEngine::new(RiskLimits {
            min_free_cash_usd: 5.0,
            ..RiskLimits::default()
        });
        let order = OrderIntent {
            client_order_id: ClientOrderId::from("order-1"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            limit_price: 0.8,
            quantity: 8.0,
            reduce_only: false,
            reason: "test".into(),
            quote_level_tag: None,
            created_at_ms: 1,
            pair_id: None,
        };

        let decision = risk.evaluate(
            &inventory,
            &order,
            &RiskContext {
                now_ms: 2,
                ..RiskContext::default()
            },
        );

        assert!(!decision.accepted);
        assert_eq!(
            decision.reject_reason,
            Some(RiskRejectReason::FreeCashTooLow)
        );
    }
}
