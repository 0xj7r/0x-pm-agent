use std::collections::HashMap;

use crate::event_log::{EventCategory, EventLog, EventMetrics, EventRecord};
use crate::inventory::InventoryState;
use crate::risk::{RiskContext, RiskEngine, RiskLimits};
use crate::strategy::{Strategy, StrategyContext, StrategyDecision};
use crate::types::{
    ClientOrderId, EpochMillis, FillReport, InstrumentId, MarketId, MarketSnapshot, OrderIntent,
};
pub use crate::runtime_types::{
    ManagedOrder, ManagedOrderStatus, RuntimeConfig, RuntimeError, RuntimeOutcome,
};
use crate::types::{RuntimeCommand, RuntimeStatus};

pub struct Runtime<S: Strategy> {
    strategy: S,
    inventory: InventoryState,
    risk: RiskEngine,
    event_log: EventLog,
    status: RuntimeStatus,
    open_orders: HashMap<ClientOrderId, ManagedOrder>,
    last_quotes: HashMap<InstrumentId, crate::types::QuoteSnapshot>,
}

impl<S: Strategy> Runtime<S> {
    pub fn new(config: RuntimeConfig, risk_limits: RiskLimits, strategy: S) -> Self {
        Self {
            strategy,
            inventory: InventoryState::new(config.starting_cash_usd),
            risk: RiskEngine::new(risk_limits),
            event_log: EventLog::new(config.event_log_capacity),
            status: config.initial_status,
            open_orders: HashMap::new(),
            last_quotes: HashMap::new(),
        }
    }

    pub fn status(&self) -> RuntimeStatus {
        self.status
    }

    pub fn inventory(&self) -> &InventoryState {
        &self.inventory
    }

    pub fn risk(&self) -> &RiskEngine {
        &self.risk
    }

    pub fn event_log(&self) -> &EventLog {
        &self.event_log
    }

    pub fn open_orders(&self) -> impl Iterator<Item = &ManagedOrder> {
        self.open_orders.values()
    }

    pub fn last_quote(
        &self,
        instrument_id: &InstrumentId,
    ) -> Option<&crate::types::QuoteSnapshot> {
        self.last_quotes.get(instrument_id)
    }

    pub fn start(&mut self, now_ms: EpochMillis) -> RuntimeOutcome {
        self.status = RuntimeStatus::Running;
        let mut outcome = RuntimeOutcome::default();
        outcome.push_event(self.event_log.push(EventRecord::runtime_status(
            now_ms,
            self.status,
        )));
        let decision = self.strategy.on_start(&self.strategy_context(now_ms));
        outcome.extend(self.accept_strategy_decision(decision, now_ms));
        outcome
    }

    pub fn on_market_snapshot(
        &mut self,
        snapshot: MarketSnapshot,
    ) -> Result<RuntimeOutcome, RuntimeError> {
        let now_ms = snapshot.quote.observed_at_ms;
        self.last_quotes
            .insert(snapshot.instrument_id.clone(), snapshot.quote.clone());
        if let Some(mark) = snapshot.mark_price() {
            let adjustment = self.inventory.mark_price(
                &snapshot.market_id,
                &snapshot.instrument_id,
                mark,
                now_ms,
            );
            let mut outcome = RuntimeOutcome::default();
            outcome.push_event(self.event_log.push(
                adjustment.to_event("inventory mark refreshed from market snapshot"),
            ));
            let decision = self
                .strategy
                .on_market_snapshot(&self.strategy_context(now_ms), &snapshot);
            outcome.extend(self.accept_strategy_decision(decision, now_ms));
            Ok(outcome)
        } else {
            let decision = self
                .strategy
                .on_market_snapshot(&self.strategy_context(now_ms), &snapshot);
            Ok(self.accept_strategy_decision(decision, now_ms))
        }
    }

    pub fn on_book_state(
        &mut self,
        market_id: MarketId,
        instrument_id: InstrumentId,
        book: &crate::book::BookState,
    ) -> Result<RuntimeOutcome, RuntimeError> {
        let best_bid = (book.best_bid > 0.0)
            .then(|| crate::types::BookLevel::new(book.best_bid, book.best_bid_size));
        let best_ask = (book.best_ask > 0.0)
            .then(|| crate::types::BookLevel::new(book.best_ask, book.best_ask_size));
        let last_trade_price = (book.last_trade_price > 0.0).then_some(book.last_trade_price);

        self.on_market_snapshot(MarketSnapshot {
            market_id,
            instrument_id,
            quote: crate::types::QuoteSnapshot {
                best_bid,
                best_ask,
                last_trade_price,
                observed_at_ms: book.last_update_unix_ms,
            },
        })
    }

    pub fn on_fill(&mut self, fill: FillReport) -> Result<RuntimeOutcome, RuntimeError> {
        let now_ms = fill.observed_at_ms;
        let mut outcome = RuntimeOutcome::default();
        outcome.push_event(self.event_log.push(
            EventRecord::new(
                EventCategory::Execution,
                now_ms,
                "received fill report",
            )
            .with_market(fill.market_id.clone())
            .with_instrument(fill.instrument_id.clone())
            .with_metrics(EventMetrics {
                price: Some(fill.price),
                quantity: Some(fill.quantity),
                notional_usd: Some(fill.notional_usd()),
                cash_delta_usd: None,
                position_delta: Some(fill.quantity * fill.side.sign()),
                free_cash_after_usd: None,
                gross_exposure_after_usd: None,
            }),
        ));

        if let Some(client_order_id) = &fill.client_order_id {
            let remove_after = if let Some(managed) = self.open_orders.get_mut(client_order_id) {
                managed.cumulative_filled_qty += fill.quantity;
                managed.last_update_ms = now_ms;
                if managed.remaining_qty() <= 1e-9 {
                    managed.status = ManagedOrderStatus::Filled;
                    true
                } else {
                    managed.status = ManagedOrderStatus::Working;
                    false
                }
            } else {
                false
            };
            if remove_after {
                self.open_orders.remove(client_order_id);
            }
        }

        let adjustment = self.inventory.apply_fill(&fill)?;
        outcome.push_event(self.event_log.push(
            adjustment.to_event("inventory updated from fill"),
        ));

        let decision = self.strategy.on_fill(&self.strategy_context(now_ms), &fill);
        outcome.extend(self.accept_strategy_decision(decision, now_ms));
        Ok(outcome)
    }

    pub fn on_order_opened(
        &mut self,
        client_order_id: &ClientOrderId,
        now_ms: EpochMillis,
    ) -> RuntimeOutcome {
        let mut outcome = RuntimeOutcome::default();
        if let Some(managed) = self.open_orders.get_mut(client_order_id) {
            managed.status = ManagedOrderStatus::Working;
            managed.last_update_ms = now_ms;
            outcome.push_event(self.event_log.push(
                EventRecord::new(
                    EventCategory::Execution,
                    now_ms,
                    "order acknowledged by downstream execution layer",
                )
                .with_market(managed.intent.market_id.clone())
                .with_instrument(managed.intent.instrument_id.clone())
                .with_client_order(client_order_id.clone()),
            ));
        }
        outcome
    }

    pub fn on_order_rejected(
        &mut self,
        client_order_id: &ClientOrderId,
        reason: impl Into<String>,
        now_ms: EpochMillis,
    ) -> RuntimeOutcome {
        let reason = reason.into();
        let mut outcome = RuntimeOutcome::default();
        if let Some(managed) = self.open_orders.remove(client_order_id) {
            if let Some(release) = self.inventory.release_reservation(client_order_id, now_ms) {
                outcome.push_event(self.event_log.push(
                    release.to_event("released reservation after downstream rejection"),
                ));
            }
            outcome.push_event(self.event_log.push(
                EventRecord::new(EventCategory::Execution, now_ms, reason)
                    .with_market(managed.intent.market_id.clone())
                    .with_instrument(managed.intent.instrument_id.clone())
                    .with_client_order(client_order_id.clone()),
            ));
        }
        outcome
    }

    pub fn on_order_cancelled(
        &mut self,
        client_order_id: &ClientOrderId,
        reason: impl Into<String>,
        now_ms: EpochMillis,
    ) -> RuntimeOutcome {
        let mut outcome = RuntimeOutcome::default();
        if let Some(mut managed) = self.open_orders.remove(client_order_id) {
            managed.status = ManagedOrderStatus::Cancelled;
            managed.last_update_ms = now_ms;
            if let Some(release) = self.inventory.release_reservation(client_order_id, now_ms) {
                outcome.push_event(self.event_log.push(
                    release.to_event("released reservation after cancellation"),
                ));
            }
            outcome.push_event(self.event_log.push(
                EventRecord::new(EventCategory::Execution, now_ms, reason)
                    .with_market(managed.intent.market_id.clone())
                    .with_instrument(managed.intent.instrument_id.clone())
                    .with_client_order(client_order_id.clone()),
            ));
        }
        outcome
    }

    pub fn request_cancel_all(
        &mut self,
        now_ms: EpochMillis,
        reason: impl Into<String>,
    ) -> RuntimeOutcome {
        let reason = reason.into();
        let ids = self.open_orders.keys().cloned().collect::<Vec<_>>();
        let mut outcome = RuntimeOutcome::default();
        for client_order_id in ids {
            if let Some(managed) = self.open_orders.get_mut(&client_order_id) {
                managed.status = ManagedOrderStatus::CancelRequested;
                managed.last_update_ms = now_ms;
                outcome.push_command(RuntimeCommand::Cancel {
                    client_order_id: client_order_id.clone(),
                    reason: reason.clone(),
                });
                outcome.push_event(self.event_log.push(
                    EventRecord::new(
                        EventCategory::Runtime,
                        now_ms,
                        "requested order cancellation",
                    )
                    .with_market(managed.intent.market_id.clone())
                    .with_instrument(managed.intent.instrument_id.clone())
                    .with_client_order(client_order_id.clone()),
                ));
            }
        }
        outcome
    }

    fn accept_strategy_decision(
        &mut self,
        decision: StrategyDecision,
        now_ms: EpochMillis,
    ) -> RuntimeOutcome {
        let mut outcome = RuntimeOutcome::default();
        for note in decision.notes {
            outcome.push_event(self.event_log.push(EventRecord::new(
                EventCategory::Strategy,
                now_ms,
                note,
            )));
        }
        for intent in decision.intents {
            outcome.extend(self.accept_intent(intent, now_ms));
        }
        outcome
    }

    fn accept_intent(&mut self, intent: OrderIntent, now_ms: EpochMillis) -> RuntimeOutcome {
        // TODO(2026-04-23): integrate execution acknowledgements/fill events from a downstream
        // matcher and remove this placeholder reserve->submit transition assumption.
        let mut outcome = RuntimeOutcome::default();
        if self.open_orders.contains_key(&intent.client_order_id) {
            outcome.push_event(self.event_log.push(
                EventRecord::new(
                    EventCategory::Runtime,
                    now_ms,
                    "duplicate client_order_id rejected before risk",
                )
                .with_market(intent.market_id.clone())
                .with_instrument(intent.instrument_id.clone())
                .with_client_order(intent.client_order_id.clone()),
            ));
            return outcome;
        }

        let risk_context = RiskContext {
            open_orders_total: self.open_orders.len(),
            open_orders_for_market: self.open_orders_for_market(&intent.market_id),
            now_ms,
        };
        let decision = self.risk.evaluate(&self.inventory, &intent, &risk_context);
        outcome.push_event(self.event_log.push(decision.to_event(&intent)));
        if !decision.accepted {
            return outcome;
        }

        match self.inventory.reserve_for_order(&intent) {
            Ok(adjustment) => {
                outcome.push_event(
                    self.event_log
                        .push(adjustment.to_event("reserved inventory for submit")),
                );
                self.open_orders.insert(
                    intent.client_order_id.clone(),
                    ManagedOrder {
                        reserved_cash_usd: if matches!(intent.side, crate::types::TradeSide::Buy)
                        {
                            intent.notional_usd()
                        } else {
                            0.0
                        },
                        last_update_ms: now_ms,
                        cumulative_filled_qty: 0.0,
                        status: ManagedOrderStatus::PendingSubmit,
                        intent: intent.clone(),
                    },
                );
                outcome.push_command(RuntimeCommand::Submit(intent));
            }
            Err(source) => {
                outcome.push_event(self.event_log.push(
                    EventRecord::new(
                        EventCategory::Inventory,
                        now_ms,
                        format!("inventory rejected submit: {source}"),
                    )
                    .with_market(intent.market_id.clone())
                    .with_instrument(intent.instrument_id.clone())
                    .with_client_order(intent.client_order_id.clone()),
                ));
            }
        }
        outcome
    }

    fn strategy_context(&self, now_ms: EpochMillis) -> StrategyContext {
        StrategyContext {
            now_ms,
            runtime_status: self.status,
            inventory: self.inventory.snapshot(),
            open_orders_total: self.open_orders.len(),
        }
    }

    fn open_orders_for_market(&self, market_id: &crate::types::MarketId) -> usize {
        self.open_orders
            .values()
            .filter(|managed| &managed.intent.market_id == market_id)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::{Runtime, RuntimeConfig};
    use crate::risk::RiskLimits;
    use crate::strategy::{Strategy, StrategyContext, StrategyDecision};
    use crate::types::{
        BookLevel, ClientOrderId, FillLiquidity, FillReport, InstrumentId, MarketId,
        MarketSnapshot, OrderIntent, QuoteSnapshot, RuntimeCommand, RuntimeStatus, TradeSide,
    };

    struct SingleShotStrategy {
        fired: bool,
    }

    impl Strategy for SingleShotStrategy {
        fn name(&self) -> &str {
            "single-shot"
        }

        fn on_market_snapshot(
            &mut self,
            context: &StrategyContext,
            snapshot: &MarketSnapshot,
        ) -> StrategyDecision {
            if self.fired || context.runtime_status != RuntimeStatus::Running {
                return StrategyDecision::none();
            }
            self.fired = true;
            StrategyDecision::single(OrderIntent {
                client_order_id: ClientOrderId::from("client-1"),
                market_id: snapshot.market_id.clone(),
                instrument_id: snapshot.instrument_id.clone(),
                side: TradeSide::Buy,
                limit_price: snapshot.quote.best_ask.as_ref().unwrap().price,
                quantity: 10.0,
                reduce_only: false,
                reason: "enter".into(),
                created_at_ms: snapshot.quote.observed_at_ms,
            })
        }
    }

    #[test]
    fn runtime_reserves_then_applies_fill() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Starting,
            },
            RiskLimits::default(),
            SingleShotStrategy { fired: false },
        );

        let started = runtime.start(1);
        assert!(started.commands.is_empty());

        let snapshot = MarketSnapshot {
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new(0.39, 100.0)),
                best_ask: Some(BookLevel::new(0.40, 100.0)),
                last_trade_price: Some(0.40),
                observed_at_ms: 2,
            },
        };
        let outcome = runtime.on_market_snapshot(snapshot).expect("quote");
        assert_eq!(outcome.commands.len(), 1);
        match &outcome.commands[0] {
            RuntimeCommand::Submit(intent) => {
                assert_eq!(intent.client_order_id.as_str(), "client-1");
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!((runtime.inventory().free_cash_usd() - 96.0).abs() < 1e-9);
        assert_eq!(runtime.open_orders().count(), 1);

        runtime
            .on_fill(FillReport {
                order_id: None,
                client_order_id: Some(ClientOrderId::from("client-1")),
                market_id: MarketId::from("market-1"),
                instrument_id: InstrumentId::from("token-1"),
                side: TradeSide::Buy,
                price: 0.40,
                quantity: 10.0,
                fee_usd: 0.10,
                liquidity: FillLiquidity::Taker,
                observed_at_ms: 3,
            })
            .expect("fill");

        assert_eq!(runtime.open_orders().count(), 0);
        assert_eq!(runtime.inventory().position_qty(&InstrumentId::from("token-1")), 10.0);
        assert!((runtime.inventory().free_cash_usd() - 95.9).abs() < 1e-9);
    }

    #[test]
    fn on_book_state_caches_latest_quote() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 0.0,
                event_log_capacity: 32,
                initial_status: RuntimeStatus::Starting,
            },
            RiskLimits::default(),
            crate::strategy::NoopStrategy,
        );

        runtime.start(1);

        let book = crate::book::BookState::from_top_of_book(
            "token-up",
            0.41,
            12.0,
            0.44,
            7.0,
            0.43,
            25,
        );

        let outcome = runtime
            .on_book_state(
                MarketId::from("market-1"),
                InstrumentId::from("token-up"),
                &book,
            )
            .expect("book state");

        assert!(outcome.commands.is_empty());
        let quote = runtime
            .last_quote(&InstrumentId::from("token-up"))
            .expect("cached quote");
        assert_eq!(quote.observed_at_ms, 25);
        assert_eq!(quote.best_bid.as_ref().unwrap().price, 0.41);
        assert_eq!(quote.best_ask.as_ref().unwrap().price, 0.44);
    }
}
