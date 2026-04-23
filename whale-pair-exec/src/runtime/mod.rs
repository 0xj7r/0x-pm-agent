pub mod runner;
pub mod reconcile;
pub mod order_store;
pub mod types;

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::event_log::{EventCategory, EventLog, EventMetrics, EventRecord};
use crate::inventory::InventoryState;
use crate::merge_executor::MergeExecutor;
use crate::quote_engine::{DesiredQuoteSet, QuoteEngineConfig, StaleMode};
use crate::quote_reconciler::{QuoteAction, QuoteReconciler};
use crate::runtime::order_store::{OrderRecord, OrderStore};
use crate::market_context::MarketContextStore;
use crate::risk::{RiskContext, RiskEngine, RiskLimits};
use crate::strategy::{Strategy, StrategyContext, StrategyDecision};
use crate::types::{
    ClientOrderId, CloseMethod, EpochMillis, FillReport, InstrumentId, MarketId, MarketSnapshot,
    OrderIntent, TradeSide,
};
use serde::Serialize;
pub use crate::runtime::types::{
    ManagedOrder, ManagedOrderStatus, RuntimeConfig, RuntimeError, RuntimeOutcome,
};
use crate::types::{RuntimeCommand, RuntimeStatus};
use tracing::{debug, info, warn};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RuntimeCheckpointOrder {
    pub client_order_id: ClientOrderId,
    pub venue_order_id: Option<crate::types::OrderId>,
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub side: crate::types::TradeSide,
    pub limit_price: f64,
    pub reduce_only: bool,
    pub original_qty: f64,
    pub remaining_qty: f64,
    pub filled_qty: f64,
    pub status: String,
    pub submitted_at_ms: EpochMillis,
    pub last_update_ms: EpochMillis,
    pub reason: Option<String>,
    pub strategy_tag: String,
    pub quote_level_tag: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RuntimeCheckpoint {
    pub observed_at_ms: EpochMillis,
    pub run_id: String,
    pub name: String,
    pub runtime_status: RuntimeStatus,
    pub open_orders: Vec<RuntimeCheckpointOrder>,
    pub needs_reconcile_orders: usize,
    pub event_seq_checkpoint: u64,
}

impl ManagedOrderStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ManagedOrderStatus::Filled
                | ManagedOrderStatus::Cancelled
                | ManagedOrderStatus::Rejected
        )
    }

    pub fn can_transition_to(self, next: ManagedOrderStatus) -> bool {
        use ManagedOrderStatus::*;
        if self == next {
            return true;
        }

        match self {
            PendingSubmit => matches!(
                next,
                Submitted
                    | Working
                    | CancelRequested
                    | Filled
                    | Cancelled
                    | Rejected
                    | NeedsReconcile
            ),
            Submitted => matches!(
                next,
                Working | CancelRequested | Filled | Cancelled | Rejected | NeedsReconcile
            ),
            Working => matches!(next, CancelRequested | Filled | Cancelled | Rejected | NeedsReconcile),
            CancelRequested => matches!(next, Cancelled | Filled | Rejected | NeedsReconcile),
            Filled | Cancelled | Rejected => false,
            NeedsReconcile => matches!(
                next,
                PendingSubmit
                    | Submitted
                    | Working
                    | CancelRequested
                    | Filled
                    | Cancelled
                    | Rejected
            ),
        }
    }
}

pub struct Runtime<S: Strategy> {
    strategy: S,
    inventory: InventoryState,
    risk: RiskEngine,
    event_log: EventLog,
    run_id: String,
    status: RuntimeStatus,
    open_orders: HashMap<ClientOrderId, ManagedOrder>,
    last_quotes: HashMap<InstrumentId, crate::types::QuoteSnapshot>,
    market_contexts: MarketContextStore,
    merge_executor: MergeExecutor,
    quote_reconciler: QuoteReconciler,
    quote_engine_config: QuoteEngineConfig,
    quote_stale_ms: u64,
    order_store: Option<Box<dyn OrderStore>>,
}

impl<S: Strategy> Runtime<S> {
    pub fn new(
        config: RuntimeConfig,
        risk_limits: RiskLimits,
        strategy: S,
        market_contexts: MarketContextStore,
    ) -> Self {
        Self::new_with_order_store(
            config,
            risk_limits,
            strategy,
            market_contexts,
            None,
            generate_run_id(),
        )
    }

    pub fn new_with_order_store(
        config: RuntimeConfig,
        risk_limits: RiskLimits,
        strategy: S,
        market_contexts: MarketContextStore,
        order_store: Option<Box<dyn OrderStore>>,
        run_id: String,
    ) -> Self {
        Self {
            strategy,
            inventory: InventoryState::new(config.starting_cash_usd),
            risk: RiskEngine::new(risk_limits),
            event_log: EventLog::new(config.event_log_capacity),
            run_id,
            status: config.initial_status,
            open_orders: HashMap::new(),
            last_quotes: HashMap::new(),
            market_contexts,
            merge_executor: MergeExecutor::new(),
            quote_reconciler: QuoteReconciler::default(),
            quote_engine_config: config.quote_engine_config,
            quote_stale_ms: config.quote_stale_ms,
            order_store,
        }
    }

    pub fn status(&self) -> RuntimeStatus {
        self.status
    }

    pub fn run_id(&self) -> &str {
        self.run_id.as_str()
    }

    pub fn market_context_version(&self) -> &str {
        self.market_contexts.version.as_str()
    }

    pub fn recover_from_store(
        &mut self,
        now_ms: EpochMillis,
        stale_after_ms: u64,
    ) -> RuntimeOutcome {
        let mut outcome = RuntimeOutcome::default();
        let Some(order_store) = self.order_store.as_mut() else {
            return outcome;
        };
        let records = match order_store.list_open() {
            Ok(records) => records,
            Err(error) => {
                warn!(
                    error = ?error,
                    run_id = %self.run_id,
                    "failed to load durable open orders during recovery"
                );
                return outcome;
            }
        };
        self.open_orders.clear();
        for record in records {
            let managed = Self::managed_from_record(record);
            info!(
                run_id = %self.run_id,
                client_order_id = %managed.intent.client_order_id,
                status = ?managed.status,
                "restored order from durable store"
            );
            let client_order_id = managed.intent.client_order_id.clone();
            self.open_orders.insert(client_order_id, managed.clone());
            outcome.push_event(self.event_log.push(
                EventRecord::new(
                    EventCategory::Runtime,
                    now_ms,
                    format!(
                        "restored order from durable store status={:?}",
                        managed.status
                    ),
                )
                .with_market(managed.intent.market_id)
                .with_instrument(managed.intent.instrument_id)
                .with_client_order(managed.intent.client_order_id),
            ));
        }
        outcome.extend(self.reconcile_open_orders(now_ms, stale_after_ms));
        outcome
    }

    pub fn reconcile_open_orders(
        &mut self,
        now_ms: EpochMillis,
        stale_after_ms: u64,
    ) -> RuntimeOutcome {
        crate::runtime::reconcile::reconcile_open_orders(self, now_ms, stale_after_ms)
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

    pub fn open_order_snapshots(&self) -> Vec<ManagedOrder> {
        self.open_orders.values().cloned().collect()
    }

    pub fn sync_open_orders_from_store(&mut self, now_ms: EpochMillis) -> RuntimeOutcome {
        let mut outcome = RuntimeOutcome::default();
        let Some(order_store) = self.order_store.as_ref() else {
            return outcome;
        };

        let records = match order_store.list_open() {
            Ok(records) => records,
            Err(error) => {
                warn!(
                    error = ?error,
                    run_id = %self.run_id,
                    "failed to sync open orders from durable store"
                );
                return outcome;
            }
        };

        let mut seen = std::collections::HashSet::new();
        for record in records {
            seen.insert(record.client_order_id.clone());
            let mut mark_needs_reconcile = false;
            match self.open_orders.get_mut(&record.client_order_id) {
                Some(managed) => {
                    let store_status = record.status;
                    if managed.status != store_status {
                        if managed.status.can_transition_to(store_status) {
                            let previous_status = managed.status;
                            managed.status = store_status;
                            outcome.push_event(self.event_log.push(
                                EventRecord::new(
                                    EventCategory::Runtime,
                                    now_ms,
                                    format!(
                                        "order status {:?} -> {:?} after durable sync",
                                        previous_status, store_status
                                    ),
                                )
                                .with_client_order(record.client_order_id.clone())
                                .with_market(record.market_id.clone())
                                .with_instrument(record.instrument_id.clone()),
                            ));
                        } else {
                            mark_needs_reconcile = true;
                        }
                    }
                    managed.cumulative_filled_qty = record.filled_qty;
                    managed.last_update_ms = managed.last_update_ms.max(record.last_update_ms);
                }
                None => {
                    let managed = Self::managed_from_record(record.clone());
                    self.open_orders
                        .insert(record.client_order_id.clone(), managed.clone());
                    outcome.push_event(self.event_log.push(
                        EventRecord::new(
                            EventCategory::Runtime,
                            now_ms,
                            "restored missing open order during durable sync",
                        )
                        .with_market(managed.intent.market_id.clone())
                        .with_instrument(managed.intent.instrument_id.clone())
                        .with_client_order(managed.intent.client_order_id.clone()),
                    ));
                }
            }
            if mark_needs_reconcile {
                            outcome.extend(self.mark_order_needs_reconcile(
                                &record.client_order_id,
                                now_ms,
                                "durable store diverged from in-memory order state",
                            ));
            }
        }

        let missing_from_store = self
            .open_orders
            .keys()
            .cloned()
            .filter(|client_order_id| !seen.contains(client_order_id))
            .collect::<Vec<_>>();
        for client_order_id in missing_from_store {
            let needs_reconcile = self
                .open_orders
                .get(&client_order_id)
                .map(|managed| !managed.status.is_terminal())
                .unwrap_or(false);
            if needs_reconcile {
                outcome.extend(self.mark_order_needs_reconcile(
                    &client_order_id,
                    now_ms,
                    "order missing from durable open-order set",
                ));
            }
        }

        outcome
    }

    pub fn checkpoint_snapshot(
        &self,
        observed_at_ms: EpochMillis,
        name: impl Into<String>,
        event_seq_checkpoint: u64,
    ) -> RuntimeCheckpoint {
        let strategy_tag = self.strategy.name().to_string();
        let open_orders = if let Some(order_store) = self.order_store.as_ref() {
            match order_store.list_open() {
                Ok(records) => records.into_iter().map(Self::checkpoint_order_from_record).collect(),
                Err(error) => {
                    warn!(
                        error = ?error,
                        run_id = %self.run_id,
                        "failed to build checkpoint from store, falling back to memory"
                    );
                    self.open_orders
                        .values()
                        .map(|managed| Self::checkpoint_order_from_managed(managed, &strategy_tag))
                        .collect()
                }
            }
        } else {
            self.open_orders
                .values()
                .map(|managed| Self::checkpoint_order_from_managed(managed, &strategy_tag))
                .collect()
        };

        RuntimeCheckpoint {
            observed_at_ms,
            run_id: self.run_id.clone(),
            name: name.into(),
            runtime_status: self.status,
            needs_reconcile_orders: self
                .open_orders
                .values()
                .filter(|managed| managed.status == ManagedOrderStatus::NeedsReconcile)
                .count(),
            event_seq_checkpoint,
            open_orders,
        }
    }

    pub fn restore_from_checkpoint(&mut self, checkpoint: RuntimeCheckpoint) -> RuntimeOutcome {
        let RuntimeCheckpoint {
            observed_at_ms,
            run_id,
            name: _,
            runtime_status,
            open_orders,
            needs_reconcile_orders: _,
            event_seq_checkpoint: _,
        } = checkpoint;

        self.run_id = run_id;
        self.status = runtime_status;
        self.open_orders.clear();

        let mut outcome = RuntimeOutcome::default();
        for record in open_orders {
            let managed = Self::managed_from_checkpoint_order(record);
            let client_order_id = managed.intent.client_order_id.clone();
            self.open_orders.insert(client_order_id.clone(), managed.clone());
            outcome.push_event(self.event_log.push(
                EventRecord::new(
                    EventCategory::Runtime,
                    observed_at_ms,
                    "restored order from checkpoint",
                )
                .with_market(managed.intent.market_id.clone())
                .with_instrument(managed.intent.instrument_id.clone())
                .with_client_order(client_order_id),
            ));
        }

        outcome.push_event(self.event_log.push(EventRecord::runtime_status(
            observed_at_ms,
            self.status,
        )));
        outcome
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
        let decision = self.strategy.on_start(&self.strategy_context(now_ms, None));
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
                .on_market_snapshot(&self.strategy_context(now_ms, Some(&snapshot.market_id)), &snapshot);
            outcome.extend(self.accept_strategy_decision(decision, now_ms));
            Ok(outcome)
        } else {
            let decision = self
                .strategy
                .on_market_snapshot(&self.strategy_context(now_ms, Some(&snapshot.market_id)), &snapshot);
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
        let merge_flow = matches!(
            fill.close_method,
            Some(CloseMethod::Merge) | Some(CloseMethod::Settle) | Some(CloseMethod::Settlement)
        );
        let mut outcome = RuntimeOutcome::default();
        outcome.push_event(self.event_log.push(
            EventRecord::new(
                EventCategory::Execution,
                now_ms,
                if let Some(close_method) = fill.close_method {
                    format!("received fill report via close_method={}", close_method.as_str())
                } else {
                    "received fill report".to_string()
                },
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
                risk_reject_reason: None,
            }),
        ));

        let mut executed_qty = 0.0;
        if merge_flow {
            if let Some(execution) = self.merge_executor.apply_merge(&fill, &mut self.inventory)? {
                executed_qty = execution.merged_qty;
                outcome.push_event(self.event_log.push(
                    EventRecord::new(
                        EventCategory::Execution,
                        now_ms,
                        format!(
                            "merge started: market={} requested_qty={:.8} merged_qty={:.8} yes={} no={}",
                            execution.market_id,
                            execution.requested_qty,
                            execution.merged_qty,
                            execution.yes_instrument_id,
                            execution.no_instrument_id
                        ),
                    )
                    .with_market(execution.market_id.clone())
                    .with_instrument(execution.yes_instrument_id.clone())
                    .with_metrics(EventMetrics {
                        price: None,
                        quantity: Some(execution.merged_qty),
                        notional_usd: Some(execution.expected_cash_usd),
                        cash_delta_usd: Some(execution.adjustment.cash_delta_usd),
                        position_delta: Some(0.0),
                        free_cash_after_usd: Some(execution.adjustment.free_cash_after_usd),
                        gross_exposure_after_usd: Some(execution.adjustment.gross_exposure_after_usd),
                        risk_reject_reason: None,
                    }),
                ));
                outcome.push_event(self.event_log.push(
                    EventRecord::new(
                        EventCategory::Execution,
                        now_ms,
                        format!(
                            "merge completed: market={} qty={:.8} cash={:.4} cost={:.4} fee={:.4} net_gain={:.4}",
                            execution.market_id,
                            execution.merged_qty,
                            execution.expected_cash_usd,
                            execution.expected_cost_usd,
                            execution.expected_fee_usd,
                            execution.net_gain_usd
                        ),
                    )
                    .with_market(execution.market_id.clone())
                    .with_instrument(execution.yes_instrument_id.clone())
                    .with_metrics(EventMetrics {
                        price: None,
                        quantity: Some(execution.merged_qty),
                        notional_usd: Some(execution.expected_cash_usd),
                        cash_delta_usd: Some(execution.adjustment.cash_delta_usd),
                        position_delta: Some(0.0),
                        free_cash_after_usd: Some(execution.adjustment.free_cash_after_usd),
                        gross_exposure_after_usd: Some(execution.adjustment.gross_exposure_after_usd),
                        risk_reject_reason: None,
                    }),
                ));
                outcome.push_event(self.event_log.push(
                    execution.adjustment.to_event("inventory updated from merge completion"),
                ));
            } else {
                outcome.push_event(self.event_log.push(
                    EventRecord::new(
                        EventCategory::Execution,
                        now_ms,
                        format!("merge skipped: no mergeable quantity for {}", fill.market_id),
                    )
                    .with_market(fill.market_id.clone())
                    .with_instrument(fill.instrument_id.clone()),
                ));
            }
        } else {
            let adjustment = self.inventory.apply_fill(&fill)?;
            self.merge_executor.on_fill(&fill);
            executed_qty = fill.quantity;
            outcome.push_event(self.event_log.push(
                adjustment.to_event("inventory updated from fill"),
            ));
        }

        if let Some(client_order_id) = &fill.client_order_id {
            let mut remove_after = false;
            let mut next_status = None;
            if let Some(managed) = self.open_orders.get_mut(client_order_id) {
                managed.cumulative_filled_qty += executed_qty;
                managed.last_update_ms = now_ms;
                if managed.remaining_qty() <= 1e-9 {
                    remove_after = true;
                    next_status = Some(ManagedOrderStatus::Filled);
                } else {
                    next_status = Some(ManagedOrderStatus::Working);
                }
            } else if let Some(store) = self.order_store.as_mut() {
                if let Err(error) = store.apply_fill(client_order_id, executed_qty, now_ms) {
                    warn!(
                        run_id = %self.run_id,
                        error = ?error,
                        client_order_id = %client_order_id,
                        "unable to persist fill for missing in-memory order"
                    );
                }
            }
            if let Some(status) = next_status {
                outcome.extend(self.set_order_status(
                    client_order_id,
                    status,
                    now_ms,
                    "fill received",
                ));
            }
            if remove_after {
                info!(
                    run_id = %self.run_id,
                    client_order_id = %client_order_id,
                    "order transitioned to Filled and removed from memory"
                );
                if let Some(release) = self.inventory.release_reservation(client_order_id, now_ms) {
                    outcome.push_event(self.event_log.push(
                        release.to_event("released reservation after full fill"),
                    ));
                }
                self.open_orders.remove(client_order_id);
            }
        }

        let decision = self
            .strategy
            .on_fill(&self.strategy_context(now_ms, Some(&fill.market_id)), &fill);
        outcome.extend(self.accept_strategy_decision(decision, now_ms));
        Ok(outcome)
    }

    pub fn on_order_opened(
        &mut self,
        client_order_id: &ClientOrderId,
        now_ms: EpochMillis,
    ) -> RuntimeOutcome {
        let mut outcome = RuntimeOutcome::default();
        outcome.extend(
            self.set_order_status(
                client_order_id,
                ManagedOrderStatus::Working,
                now_ms,
                "order acknowledged by downstream execution layer",
            ),
        );
        outcome.push_event(self.event_log.push(
            EventRecord::new(
                EventCategory::Execution,
                now_ms,
                "order acknowledged by downstream execution layer",
            )
            .with_client_order(client_order_id.clone()),
        ));
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
        outcome.extend(self.set_order_status(
            client_order_id,
            ManagedOrderStatus::Rejected,
            now_ms,
            "order rejected by venue",
        ));
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
        let reason = reason.into();
        let mut outcome = RuntimeOutcome::default();
        outcome.extend(self.set_order_status(
            client_order_id,
            ManagedOrderStatus::Cancelled,
            now_ms,
            &reason,
        ));
        if let Some(managed) = self.open_orders.remove(client_order_id) {
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
            outcome.extend(self.request_cancel(&client_order_id, reason.clone(), now_ms));
        }
        outcome
    }

    fn request_cancel(
        &mut self,
        client_order_id: &ClientOrderId,
        reason: impl Into<String>,
        now_ms: EpochMillis,
    ) -> RuntimeOutcome {
        let reason = reason.into();
        let mut outcome = RuntimeOutcome::default();
        let (market_id, instrument_id) = match self.open_orders.get(client_order_id) {
            Some(managed) => (
                managed.intent.market_id.clone(),
                managed.intent.instrument_id.clone(),
            ),
            None => return outcome,
        };

        outcome.extend(self.set_order_status(
            client_order_id,
            ManagedOrderStatus::CancelRequested,
            now_ms,
            reason.as_str(),
        ));
        outcome.push_command(RuntimeCommand::Cancel {
            client_order_id: client_order_id.clone(),
            reason: reason.clone(),
        });
        outcome.push_event(self.event_log.push(
            EventRecord::new(EventCategory::Runtime, now_ms, "requested order cancellation")
                .with_market(market_id)
                .with_instrument(instrument_id)
                .with_client_order(client_order_id.clone()),
        ));
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

        let desired = DesiredQuoteSet::from_intents(decision.intents, &self.quote_engine_config)
            .with_stale_gate(
                now_ms,
                &self.last_quotes,
                StaleMode::Remove,
                |snapshot, now| snapshot.is_none_or(|quote| {
                    now.saturating_sub(quote.observed_at_ms) > self.quote_stale_ms
                }),
            );
        let plan = self.quote_reconciler.plan(desired, &self.open_orders, now_ms);
        outcome.push_event(self.event_log.push(EventRecord::new(
            EventCategory::Strategy,
            now_ms,
            format!(
                "quote reconciliation plan prepared ({} actions)",
                plan.actions.len()
            ),
        )));
        for note in plan.notes {
            outcome.push_event(self.event_log.push(EventRecord::new(
                EventCategory::Strategy,
                now_ms,
                note,
            )));
        }

        for action in plan.actions {
            match action {
                QuoteAction::Keep(intent) => {
                    outcome.push_event(self.event_log.push(
                        EventRecord::new(
                            EventCategory::Runtime,
                            now_ms,
                            "quote keep",
                        )
                        .with_market(intent.market_id.clone())
                        .with_instrument(intent.instrument_id.clone())
                        .with_client_order(intent.client_order_id.clone()),
                    ));
                }
                QuoteAction::Cancel {
                    client_order_id,
                    reason,
                } => {
                    outcome.extend(self.request_cancel(&client_order_id, reason, now_ms));
                }
                QuoteAction::Replace {
                    existing_client_order_id,
                    replacement,
                    cancel_reason,
                } => {
                    outcome.extend(self.request_cancel(&existing_client_order_id, cancel_reason, now_ms));
                    outcome.extend(self.accept_intent(replacement, now_ms));
                }
                QuoteAction::Submit(intent) => {
                    outcome.extend(self.accept_intent(intent, now_ms));
                }
            }
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
                let managed = ManagedOrder {
                    reserved_cash_usd: if matches!(intent.side, crate::types::TradeSide::Buy) {
                        intent.notional_usd()
                    } else {
                        0.0
                    },
                    last_update_ms: now_ms,
                    cumulative_filled_qty: 0.0,
                    status: ManagedOrderStatus::PendingSubmit,
                    intent: intent.clone(),
                };
                if let Some(order_store) = self.order_store.as_mut() {
                    let record =
                        OrderRecord::from_intent(self.run_id.clone(), &managed.intent, self.strategy.name());
                    if let Err(error) = order_store.insert(record) {
                        warn!(
                            run_id = %self.run_id,
                            error = ?error,
                            client_order_id = %intent.client_order_id,
                            "failed to persist pending submit intent; releasing reservation"
                        );
                        if let Some(release) = self
                            .inventory
                            .release_reservation(&intent.client_order_id, now_ms)
                        {
                            outcome.push_event(self.event_log.push(
                                release.to_event(
                                    "released reservation after durable persistence failure",
                                ),
                            ));
                        }
                        outcome.push_event(self.event_log.push(
                            EventRecord::new(
                                EventCategory::Runtime,
                                now_ms,
                                "order not accepted due to durable store failure",
                            )
                            .with_client_order(intent.client_order_id.clone()),
                        ));
                        return outcome;
                    }
                }
                self.open_orders.insert(intent.client_order_id.clone(), managed);
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

    fn strategy_context(&self, now_ms: EpochMillis, market_id: Option<&MarketId>) -> StrategyContext {
        StrategyContext {
            now_ms,
            runtime_status: self.status,
            inventory: self.inventory.snapshot(),
            open_orders_total: self.open_orders.len(),
            market_context: market_id.and_then(|market_id| self.market_contexts.get(market_id).cloned()),
        }
    }

    fn open_orders_for_market(&self, market_id: &crate::types::MarketId) -> usize {
        self.open_orders
            .values()
            .filter(|managed| &managed.intent.market_id == market_id)
            .count()
    }

    fn record_status_persist(
        &mut self,
        client_order_id: &ClientOrderId,
        status: ManagedOrderStatus,
        updated_at_ms: EpochMillis,
    ) {
        if let Some(store) = self.order_store.as_mut() {
            if let Err(error) = store.update_status(client_order_id, status, updated_at_ms) {
                warn!(
                    run_id = %self.run_id,
                    error = ?error,
                    client_order_id = %client_order_id,
                    status = ?status,
                    "failed to persist status transition"
                );
            }
        }
    }

    fn set_order_status(
        &mut self,
        client_order_id: &ClientOrderId,
        status: ManagedOrderStatus,
        now_ms: EpochMillis,
        reason: &str,
    ) -> RuntimeOutcome {
        let mut outcome = RuntimeOutcome::default();
        match self.open_orders.get_mut(client_order_id) {
            Some(managed) => {
                if managed.status == status {
                    return outcome;
                }
                if !managed.status.can_transition_to(status) {
                    warn!(
                        run_id = %self.run_id,
                        client_order_id = %client_order_id,
                        from = ?managed.status,
                        to = ?status,
                        reason,
                        "invalid managed order status transition"
                    );
                    return outcome;
                }
                let old_status = managed.status;
                managed.status = status;
                managed.last_update_ms = now_ms;
                info!(
                    run_id = %self.run_id,
                    client_order_id = %client_order_id,
                    from = ?old_status,
                    to = ?status,
                    reason
                );
                outcome.push_event(self.event_log.push(
                    EventRecord::new(
                        EventCategory::Runtime,
                        now_ms,
                        format!("order status {:?} -> {:?}: {}", old_status, status, reason),
                    )
                    .with_client_order(client_order_id.clone())
                    .with_market(managed.intent.market_id.clone())
                    .with_instrument(managed.intent.instrument_id.clone()),
                ));
            }
            None => {
                warn!(
                    run_id = %self.run_id,
                    client_order_id = %client_order_id,
                    status = ?status,
                    "status transition requested for unknown active order"
                );
            }
        };
        self.record_status_persist(client_order_id, status, now_ms);
        outcome
    }

    pub fn mark_order_needs_reconcile(
        &mut self,
        client_order_id: &ClientOrderId,
        now_ms: EpochMillis,
        reason: impl Into<String>,
    ) -> RuntimeOutcome {
        let reason = reason.into();
        self.set_order_status(
            client_order_id,
            ManagedOrderStatus::NeedsReconcile,
            now_ms,
            reason.as_str(),
        )
    }

    fn checkpoint_status_to_string(status: ManagedOrderStatus) -> String {
        format!("{status:?}")
    }

    fn checkpoint_status_from_string(status: &str) -> ManagedOrderStatus {
        match status {
            "PendingSubmit" => ManagedOrderStatus::PendingSubmit,
            "Submitted" => ManagedOrderStatus::Submitted,
            "Working" => ManagedOrderStatus::Working,
            "CancelRequested" => ManagedOrderStatus::CancelRequested,
            "Filled" => ManagedOrderStatus::Filled,
            "Cancelled" => ManagedOrderStatus::Cancelled,
            "Rejected" => ManagedOrderStatus::Rejected,
            "NeedsReconcile" => ManagedOrderStatus::NeedsReconcile,
            other => {
                warn!(status = %other, "unknown checkpoint order status, defaulting to NeedsReconcile");
                ManagedOrderStatus::NeedsReconcile
            }
        }
    }

    fn checkpoint_order_from_record(record: OrderRecord) -> RuntimeCheckpointOrder {
        RuntimeCheckpointOrder {
            client_order_id: record.client_order_id,
            venue_order_id: record.venue_order_id,
            market_id: record.market_id,
            instrument_id: record.instrument_id,
            side: record.side,
            limit_price: record.limit_price,
            reduce_only: record.reduce_only,
            original_qty: record.original_qty,
            remaining_qty: record.remaining_qty,
            filled_qty: record.filled_qty,
            status: Self::checkpoint_status_to_string(record.status),
            submitted_at_ms: record.submitted_at_ms,
            last_update_ms: record.last_update_ms,
            reason: record.reason,
            strategy_tag: record.strategy_tag,
            quote_level_tag: record.quote_level_tag,
        }
    }

    fn checkpoint_order_from_managed(
        managed: &ManagedOrder,
        strategy_tag: &str,
    ) -> RuntimeCheckpointOrder {
        RuntimeCheckpointOrder {
            client_order_id: managed.intent.client_order_id.clone(),
            venue_order_id: None,
            market_id: managed.intent.market_id.clone(),
            instrument_id: managed.intent.instrument_id.clone(),
            side: managed.intent.side,
            limit_price: managed.intent.limit_price,
            reduce_only: managed.intent.reduce_only,
            original_qty: managed.intent.quantity,
            remaining_qty: managed.remaining_qty(),
            filled_qty: managed.cumulative_filled_qty,
            status: Self::checkpoint_status_to_string(managed.status),
            submitted_at_ms: managed.intent.created_at_ms,
            last_update_ms: managed.last_update_ms,
            reason: Some(managed.intent.reason.clone()),
            strategy_tag: strategy_tag.to_string(),
            quote_level_tag: managed.intent.quote_level_tag.clone(),
        }
    }

    fn managed_from_checkpoint_order(record: RuntimeCheckpointOrder) -> ManagedOrder {
        ManagedOrder {
            intent: OrderIntent {
                client_order_id: record.client_order_id,
                market_id: record.market_id,
                instrument_id: record.instrument_id,
                side: record.side,
                limit_price: record.limit_price,
                quantity: record.original_qty,
                reduce_only: record.reduce_only,
                reason: record.reason.unwrap_or_else(|| "checkpoint recovery".to_string()),
                quote_level_tag: record.quote_level_tag,
                created_at_ms: record.submitted_at_ms,
            },
            status: Self::checkpoint_status_from_string(&record.status),
            cumulative_filled_qty: record.filled_qty,
            reserved_cash_usd: if matches!(record.side, crate::types::TradeSide::Buy) {
                record.limit_price * record.remaining_qty
            } else {
                0.0
            },
            last_update_ms: record.last_update_ms,
        }
    }

    fn managed_from_record(record: OrderRecord) -> ManagedOrder {
        ManagedOrder {
            intent: OrderIntent {
                client_order_id: record.client_order_id,
                market_id: record.market_id,
                instrument_id: record.instrument_id,
                side: record.side,
                limit_price: record.limit_price,
                quantity: record.original_qty,
                reduce_only: record.reduce_only,
                reason: record.reason.unwrap_or_else(|| "recovered".to_string()),
                quote_level_tag: record.quote_level_tag,
                created_at_ms: record.submitted_at_ms,
            },
            status: record.status,
            cumulative_filled_qty: record.filled_qty,
            reserved_cash_usd: if matches!(record.side, TradeSide::Buy) {
                record.limit_price * record.remaining_qty
            } else {
                0.0
            },
            last_update_ms: record.last_update_ms,
        }
    }
}

fn generate_run_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);
    format!("run-{millis}")
}

#[cfg(test)]
mod tests {
    use super::{Runtime, RuntimeConfig};
    use crate::market_context::MarketContextStore;
    use crate::runtime::order_store::SqliteOrderStore;
    use crate::runtime::order_store::OrderRecord;
    use crate::risk::RiskLimits;
    use crate::strategy::{Strategy, StrategyContext, StrategyDecision};
    use crate::types::{
        BookLevel, ClientOrderId, FillLiquidity, FillReport, InstrumentId, MarketId,
        MarketSnapshot, OrderIntent, QuoteSnapshot, RuntimeCommand, RuntimeStatus, TradeSide,
    };

    use std::time::{SystemTime, UNIX_EPOCH};

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
                quote_level_tag: None,
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
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            SingleShotStrategy { fired: false },
            MarketContextStore::empty(),
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
                close_method: None,
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
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            crate::strategy::NoopStrategy,
            MarketContextStore::empty(),
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

    #[test]
    fn recover_from_store_reconstructs_orders_and_marks_uncertain_submits() {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("whale-pair-order-store-startup-{ts}.sqlite"));
        let mut store = SqliteOrderStore::open(&path).unwrap();
        let now_ms: u64 = 10;
        let record = OrderRecord::from_intent(
            "run-1",
            &OrderIntent {
                client_order_id: ClientOrderId::from("coid-recover"),
                market_id: MarketId::from("market-1"),
                instrument_id: InstrumentId::from("token-1"),
                side: TradeSide::Buy,
                limit_price: 0.41,
                quantity: 5.0,
                reduce_only: false,
                reason: "recover".to_string(),
                created_at_ms: now_ms,
            },
            "single-shot",
        );
        let mut stale_record = record;
        stale_record.last_update_ms = 11_000;
        store.insert(stale_record).unwrap();

        let mut runtime = Runtime::new_with_order_store(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Starting,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            SingleShotStrategy { fired: false },
            MarketContextStore::empty(),
            Some(Box::new(store)),
            "run-1".to_string(),
        );
        let outcome = runtime.recover_from_store(20_000, 5_000);
        assert!(!outcome.event_seqs.is_empty());
        let recovered = runtime.open_order_snapshots();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].status, crate::runtime::types::ManagedOrderStatus::NeedsReconcile);
    }

    #[test]
    fn reconcile_open_orders_no_active_orders_does_not_panic() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Starting,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            SingleShotStrategy { fired: false },
            MarketContextStore::empty(),
        );
        runtime.start(1);
        let outcome = runtime.reconcile_open_orders(2_000, 500);
        assert!(outcome.event_seqs.is_empty());
        assert!(outcome.commands.is_empty());
    }
}
