//! Pre-trade risk limits and reject reasoning for order intents.

use serde::{Deserialize, Serialize};

use crate::event_log::{EventCategory, EventMetrics, EventRecord};
use crate::inventory::InventoryState;
use crate::types::{EpochMillis, OrderIntent, StrategyDecision, TradeSide};

#[derive(Clone, Debug, PartialEq)]
pub struct RiskLimits {
    pub max_order_notional_usd: f64,
    pub max_gross_notional_usd: f64,
    pub max_net_notional_per_market_usd: f64,
    pub max_position_quantity_per_instrument: f64,
    pub min_free_cash_usd: f64,
    pub min_free_cash_bps: f64,
    pub min_portfolio_equity_usd: f64,
    pub min_portfolio_equity_bps: f64,
    pub max_session_loss_usd: f64,
    pub max_session_loss_bps: f64,
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
            min_free_cash_bps: 0.0,
            min_portfolio_equity_usd: 0.0,
            min_portfolio_equity_bps: 0.0,
            max_session_loss_usd: 0.0,
            max_session_loss_bps: 0.0,
            max_open_orders_total: 32,
            max_open_orders_per_market: 8,
        }
    }
}

impl RiskLimits {
    pub fn free_cash_floor_usd(&self, starting_cash_usd: f64) -> f64 {
        let bps_floor = if self.min_free_cash_bps > 0.0 && starting_cash_usd > 0.0 {
            starting_cash_usd * (self.min_free_cash_bps / 10_000.0).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.min_free_cash_usd.max(bps_floor).max(0.0)
    }

    pub fn portfolio_equity_floor_usd(&self, starting_cash_usd: f64) -> Option<f64> {
        let mut floor = self.min_portfolio_equity_usd.max(0.0);
        if self.min_portfolio_equity_bps > 0.0 && starting_cash_usd > 0.0 {
            floor = floor.max(
                starting_cash_usd * (self.min_portfolio_equity_bps / 10_000.0).clamp(0.0, 1.0),
            );
        }
        if self.max_session_loss_usd > 0.0 && starting_cash_usd > 0.0 {
            floor = floor.max(starting_cash_usd - self.max_session_loss_usd);
        }
        if self.max_session_loss_bps > 0.0 && starting_cash_usd > 0.0 {
            let loss_fraction = (self.max_session_loss_bps / 10_000.0).clamp(0.0, 1.0);
            floor = floor.max(starting_cash_usd * (1.0 - loss_fraction));
        }
        (floor > 0.0).then_some(floor)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RiskContext {
    pub open_orders_total: usize,
    pub open_orders_for_market: usize,
    pub open_buy_notional_total_usd: f64,
    pub open_signed_notional_for_market_usd: f64,
    pub open_position_qty_for_instrument: f64,
    pub starting_cash_usd: f64,
    pub now_ms: EpochMillis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskRejectReason {
    InvalidOrder,
    OrderNotionalTooLarge,
    GrossExposureTooLarge,
    MarketNetExposureTooLarge,
    FreeCashTooLow,
    PortfolioEquityTooLow,
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

#[derive(Clone, Debug, PartialEq)]
pub struct StrategyRiskDecision {
    pub accepted_intents: Vec<OrderIntent>,
    pub rejected: Vec<(OrderIntent, RiskDecision)>,
    pub evaluated_at_ms: EpochMillis,
}

impl StrategyRiskDecision {
    pub fn all_accepted(&self) -> bool {
        self.rejected.is_empty()
    }
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
        let is_rescue = order.kind == crate::types::IntentKind::Close;
        let is_paired_core_repair = order
            .quote_level_tag
            .as_deref()
            .is_some_and(|tag| tag.starts_with("paired-core:"));

        let portfolio_equity_usd = inventory.total_cash_usd() + inventory.gross_exposure_usd();
        let equity_floor_usd = self
            .limits
            .portfolio_equity_floor_usd(context.starting_cash_usd);
        if !is_rescue && equity_floor_usd.is_some_and(|floor| portfolio_equity_usd < floor) {
            let floor = equity_floor_usd.unwrap_or_default();
            return self.reject(
                RiskRejectReason::PortfolioEquityTooLow,
                context.now_ms.max(order.created_at_ms),
                inventory.free_cash_usd(),
                inventory.gross_exposure_usd(),
                inventory
                    .net_exposure_for_market_usd(&order.market_id)
                    .abs(),
                format!(
                    "portfolio equity below floor equity={portfolio_equity_usd:.4} floor={floor:.4}"
                ),
            );
        }

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

        let current_position_qty =
            inventory.position_qty(&order.instrument_id) + context.open_position_qty_for_instrument;
        let projected_position_qty = current_position_qty + (order.quantity * order.side.sign());
        if !is_rescue
            && projected_position_qty.abs() > self.limits.max_position_quantity_per_instrument
        {
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
        if is_rescue {
            if projected_free_cash_usd < -1e-9 {
                return self.reject(
                    RiskRejectReason::FreeCashTooLow,
                    context.now_ms.max(order.created_at_ms),
                    projected_free_cash_usd,
                    inventory.gross_exposure_usd(),
                    inventory
                        .net_exposure_for_market_usd(&order.market_id)
                        .abs(),
                    "rescue order exceeds available free cash",
                );
            }
            return RiskDecision {
                accepted: true,
                reject_reason: None,
                message: "risk check passed".into(),
                evaluated_at_ms: context.now_ms.max(order.created_at_ms),
                projected_free_cash_usd,
                projected_gross_notional_usd: inventory.gross_exposure_usd(),
                projected_market_net_notional_usd: inventory
                    .net_exposure_for_market_usd(&order.market_id)
                    .abs(),
            };
        }
        let free_cash_floor_usd = self.limits.free_cash_floor_usd(context.starting_cash_usd);
        if projected_free_cash_usd < free_cash_floor_usd {
            return self.reject(
                RiskRejectReason::FreeCashTooLow,
                context.now_ms.max(order.created_at_ms),
                projected_free_cash_usd,
                inventory.gross_exposure_usd(),
                inventory
                    .net_exposure_for_market_usd(&order.market_id)
                    .abs(),
                format!(
                    "projected free cash falls below floor cash={projected_free_cash_usd:.4} floor={free_cash_floor_usd:.4}"
                ),
            );
        }

        let current_gross =
            inventory.gross_exposure_usd() + context.open_buy_notional_total_usd.max(0.0);
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

        let projected_market_net_notional_usd = (inventory
            .net_exposure_for_market_usd(&order.market_id)
            + context.open_signed_notional_for_market_usd
            + order.signed_notional_usd())
        .abs();
        if !is_paired_core_repair
            && projected_market_net_notional_usd > self.limits.max_net_notional_per_market_usd
        {
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

    pub fn approve_strategy_decision(
        &self,
        inventory: &InventoryState,
        decision: &StrategyDecision,
        context: &RiskContext,
    ) -> StrategyRiskDecision {
        let mut accepted_intents = Vec::new();
        let mut rejected = Vec::new();

        for intent in decision.intents() {
            let risk = self.evaluate(inventory, intent, context);
            if risk.accepted {
                accepted_intents.push(intent.clone());
            } else {
                rejected.push((intent.clone(), risk));
            }
        }

        StrategyRiskDecision {
            accepted_intents,
            rejected,
            evaluated_at_ms: context.now_ms,
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
    use crate::types::{
        ClientOrderId, FillLiquidity, FillReport, InstrumentId, IntentKind, MarketId, OrderIntent,
        TradeSide,
    };

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
            kind: crate::types::IntentKind::Entry,
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

    #[test]
    fn free_cash_floor_uses_stricter_absolute_or_bps_floor() {
        let limits = RiskLimits {
            min_free_cash_usd: 5.0,
            min_free_cash_bps: 1_500.0,
            ..RiskLimits::default()
        };

        assert_eq!(limits.free_cash_floor_usd(30.0), 5.0);
        assert_eq!(limits.free_cash_floor_usd(100.0), 15.0);
        assert_eq!(limits.free_cash_floor_usd(1_000.0), 150.0);
    }

    #[test]
    fn portfolio_equity_floor_supports_session_loss_bps() {
        let limits = RiskLimits {
            max_session_loss_bps: 2_500.0,
            ..RiskLimits::default()
        };

        assert_eq!(limits.portfolio_equity_floor_usd(100.0), Some(75.0));
        assert_eq!(limits.portfolio_equity_floor_usd(1_000.0), Some(750.0));
    }

    #[test]
    fn rejects_entry_when_session_loss_floor_is_breached() {
        let mut inventory = InventoryState::new(100.0);
        inventory
            .apply_fill(&crate::types::FillReport {
                order_id: None,
                client_order_id: Some(ClientOrderId::from("fill-1")),
                market_id: MarketId::from("market-1"),
                instrument_id: InstrumentId::from("token-1"),
                side: TradeSide::Buy,
                price: 0.60,
                quantity: 100.0,
                fee_usd: 0.0,
                liquidity: crate::types::FillLiquidity::Maker,
                close_method: None,
                observed_at_ms: 1,
            })
            .expect("apply fill");
        inventory.mark_price(
            &MarketId::from("market-1"),
            &InstrumentId::from("token-1"),
            0.30,
            2,
        );
        let risk = RiskEngine::new(RiskLimits {
            max_session_loss_usd: 20.0,
            ..RiskLimits::default()
        });
        let order = OrderIntent {
            client_order_id: ClientOrderId::from("order-2"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            limit_price: 0.4,
            quantity: 5.0,
            reduce_only: false,
            reason: "test".into(),
            quote_level_tag: None,
            created_at_ms: 3,
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
        };

        let decision = risk.evaluate(
            &inventory,
            &order,
            &RiskContext {
                starting_cash_usd: 100.0,
                now_ms: 4,
                ..RiskContext::default()
            },
        );

        assert!(!decision.accepted);
        assert_eq!(
            decision.reject_reason,
            Some(RiskRejectReason::PortfolioEquityTooLow)
        );
    }

    #[test]
    fn rejects_entry_when_open_bids_would_push_gross_over_cap() {
        let inventory = InventoryState::new(100.0);
        let risk = RiskEngine::new(RiskLimits {
            max_gross_notional_usd: 25.0,
            max_order_notional_usd: 10.0,
            ..RiskLimits::default()
        });
        let order = OrderIntent {
            client_order_id: ClientOrderId::from("order-open-aware"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("up"),
            side: TradeSide::Buy,
            limit_price: 0.50,
            quantity: 20.0,
            reduce_only: false,
            reason: "test".into(),
            quote_level_tag: None,
            created_at_ms: 3,
            pair_id: None,
            kind: IntentKind::Entry,
        };

        let decision = risk.evaluate(
            &inventory,
            &order,
            &RiskContext {
                open_buy_notional_total_usd: 20.0,
                starting_cash_usd: 100.0,
                now_ms: 4,
                ..RiskContext::default()
            },
        );

        assert!(!decision.accepted);
        assert_eq!(
            decision.reject_reason,
            Some(RiskRejectReason::GrossExposureTooLarge)
        );
        assert_eq!(decision.projected_gross_notional_usd, 30.0);
    }

    #[test]
    fn rejects_entry_when_open_same_market_bids_would_push_net_over_cap() {
        let inventory = InventoryState::new(100.0);
        let risk = RiskEngine::new(RiskLimits {
            max_net_notional_per_market_usd: 25.0,
            max_order_notional_usd: 10.0,
            ..RiskLimits::default()
        });
        let order = OrderIntent {
            client_order_id: ClientOrderId::from("order-open-net-aware"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("up"),
            side: TradeSide::Buy,
            limit_price: 0.50,
            quantity: 20.0,
            reduce_only: false,
            reason: "test".into(),
            quote_level_tag: None,
            created_at_ms: 3,
            pair_id: None,
            kind: IntentKind::Entry,
        };

        let decision = risk.evaluate(
            &inventory,
            &order,
            &RiskContext {
                open_signed_notional_for_market_usd: 20.0,
                starting_cash_usd: 100.0,
                now_ms: 4,
                ..RiskContext::default()
            },
        );

        assert!(!decision.accepted);
        assert_eq!(
            decision.reject_reason,
            Some(RiskRejectReason::MarketNetExposureTooLarge)
        );
        assert_eq!(decision.projected_market_net_notional_usd, 30.0);
    }

    #[test]
    fn accepts_hedge_rescue_even_when_entry_exposure_caps_are_full() {
        let mut inventory = InventoryState::new(50.0);
        inventory
            .apply_fill(&FillReport {
                order_id: None,
                client_order_id: Some(ClientOrderId::from("stranded-fill")),
                market_id: MarketId::from("market-1"),
                instrument_id: InstrumentId::from("up"),
                side: TradeSide::Buy,
                price: 0.42,
                quantity: 25.0,
                fee_usd: 0.0,
                liquidity: FillLiquidity::Maker,
                close_method: None,
                observed_at_ms: 1,
            })
            .expect("seed stranded inventory");
        let risk = RiskEngine::new(RiskLimits {
            max_gross_notional_usd: 10.0,
            max_net_notional_per_market_usd: 10.0,
            max_position_quantity_per_instrument: 20.0,
            min_free_cash_usd: 5.0,
            min_free_cash_bps: 1_500.0,
            ..RiskLimits::default()
        });
        let rescue = OrderIntent {
            client_order_id: ClientOrderId::from("rescue-order"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("down"),
            side: TradeSide::Buy,
            limit_price: 0.48,
            quantity: 25.0,
            reduce_only: false,
            reason: "hedge rescue".into(),
            quote_level_tag: Some("mm-hedge-rescue".into()),
            created_at_ms: 2,
            pair_id: None,
            kind: IntentKind::Close,
        };

        let decision = risk.evaluate(
            &inventory,
            &rescue,
            &RiskContext {
                starting_cash_usd: 50.0,
                now_ms: 3,
                ..RiskContext::default()
            },
        );

        assert!(decision.accepted, "{decision:?}");
    }

    #[test]
    fn rejects_hedge_rescue_when_wallet_cash_cannot_cover_it() {
        let inventory = InventoryState::new(5.0);
        let risk = RiskEngine::new(RiskLimits::default());
        let rescue = OrderIntent {
            client_order_id: ClientOrderId::from("rescue-order"),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("down"),
            side: TradeSide::Buy,
            limit_price: 0.48,
            quantity: 25.0,
            reduce_only: false,
            reason: "hedge rescue".into(),
            quote_level_tag: Some("mm-hedge-rescue".into()),
            created_at_ms: 2,
            pair_id: None,
            kind: IntentKind::Close,
        };

        let decision = risk.evaluate(
            &inventory,
            &rescue,
            &RiskContext {
                starting_cash_usd: 50.0,
                now_ms: 3,
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
