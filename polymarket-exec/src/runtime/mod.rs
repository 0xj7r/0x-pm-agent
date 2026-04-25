//! Core runtime state machine: signal ingestion, strategy evaluation, and order lifecycle.

mod audit;
mod live_auth;
pub mod order_store;
pub mod reconcile;
pub mod runner;
pub mod types;

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::event_log::{EventCategory, EventLog, EventMetrics, EventRecord};
use crate::inventory::{
    InventoryReconciliationReport, InventoryState, StrandedMarketInventory, VenuePositionSnapshot,
};
use crate::market_context::MarketContextStore;
use crate::merge_executor::MergeExecutor;
use crate::quote_engine::{DesiredQuoteSet, QuoteEngineConfig, StaleMode};
use crate::quote_reconciler::{QuoteAction, QuoteReconciler};
use crate::risk::{RiskContext, RiskEngine, RiskLimits};
use crate::runtime::order_store::{OrderRecord, OrderStore, SignalSnapshotRecord};
pub use crate::runtime::types::{
    ManagedOrder, ManagedOrderStatus, RuntimeConfig, RuntimeError, RuntimeOutcome,
};
use crate::signals::{
    evaluate_unlawful_mode, BtcRegimeSnapshot as GateBtcRegimeSnapshot,
    MarketActivitySignal as GateMarketActivitySignal, PairedBookSignal as GatePairedBookSignal,
    SessionBucket as GateSessionBucket, UnlawfulExecutionMode as GateExecutionMode,
    UnlawfulGateConfig, UnlawfulGateInputs, UnlawfulSignalSnapshot as GateSignalSnapshot,
};
use crate::strategy::{
    BtcRegimeSnapshot as StrategyBtcRegimeSnapshot,
    MarketActivitySignal as StrategyMarketActivitySignal,
    PairedBookSignal as StrategyPairedBookSignal, SessionBucket as StrategySessionBucket, Strategy,
    StrategyContext, StrategyDecision, UnlawfulExecutionMode as StrategyExecutionMode,
    UnlawfulSignalSnapshot as StrategyUnlawfulSignalSnapshot,
};
use crate::types::{
    ClientOrderId, CloseMethod, EpochMillis, FillReport, InstrumentId, MarketId, MarketSnapshot,
    MergeIntent, OrderId, OrderIntent, TradeSide,
};
use crate::types::{RuntimeCommand, RuntimeStatus};
use serde::Serialize;
use tracing::{info, warn};

const BTC_SIGNAL_WINDOW_5M_MS: u64 = 5 * 60 * 1_000;
const BTC_SIGNAL_WINDOW_15M_MS: u64 = 15 * 60 * 1_000;
const BTC_SIGNAL_WINDOW_20M_MS: u64 = 20 * 60 * 1_000;
const MAX_BTC_PRICE_SAMPLES: usize = 20_000;
const SIGNAL_SNAPSHOT_PERSIST_INTERVAL_MS: u64 = 5_000;

fn top_n_depth_qty(levels: &[crate::types::BookLevel], n: usize) -> Option<f64> {
    let total: f64 = levels
        .iter()
        .take(n)
        .filter(|level| level.price.is_finite() && level.quantity.is_finite())
        .filter(|level| level.price > 0.0 && level.quantity > 0.0)
        .map(|level| level.quantity)
        .sum();
    (total > 0.0).then_some(total)
}

fn top_n_depth_notional(levels: &[crate::types::BookLevel], n: usize) -> Option<f64> {
    let total: f64 = levels
        .iter()
        .take(n)
        .filter(|level| level.price.is_finite() && level.quantity.is_finite())
        .filter(|level| level.price > 0.0 && level.quantity > 0.0)
        .map(|level| level.price * level.quantity)
        .sum();
    (total > 0.0).then_some(total)
}

fn depth_imbalance(bid_depth_qty: Option<f64>, ask_depth_qty: Option<f64>) -> Option<f64> {
    let bid = bid_depth_qty?;
    let ask = ask_depth_qty?;
    let denom = bid + ask;
    (denom > 0.0).then_some((bid - ask) / denom)
}

#[derive(Debug, Default)]
struct BtcSignalStore {
    last_price: Option<f64>,
    observed_at_ms: u64,
    price_samples: VecDeque<(u64, f64)>,
    trade_times: VecDeque<u64>,
}

impl BtcSignalStore {
    fn record_trade(&mut self, price: f64, observed_at_ms: u64) {
        if !price.is_finite() || price <= 0.0 {
            return;
        }
        self.last_price = Some(price);
        self.observed_at_ms = observed_at_ms;
        self.price_samples.push_back((observed_at_ms, price));
        self.trade_times.push_back(observed_at_ms);
        self.prune(observed_at_ms);
    }

    fn snapshot(&self, now_ms: u64) -> GateBtcRegimeSnapshot {
        let realized_vol_5m_bps = self.realized_vol_bps(now_ms, BTC_SIGNAL_WINDOW_5M_MS);
        let realized_vol_15m_bps = self.realized_vol_bps(now_ms, BTC_SIGNAL_WINDOW_15M_MS);
        let trade_count_5m = self.trade_count(now_ms, BTC_SIGNAL_WINDOW_5M_MS);
        let trade_count_15m = self.trade_count(now_ms, BTC_SIGNAL_WINDOW_15M_MS);
        let return_30s_bps = self.return_bps(now_ms, 30_000);
        let return_60s_bps = self.return_bps(now_ms, 60_000);

        GateBtcRegimeSnapshot {
            last_price: self.last_price,
            realized_vol_5m_bps,
            realized_vol_15m_bps,
            trade_count_5m,
            trade_count_15m,
            return_30s_bps,
            return_60s_bps,
            observed_at_ms: self.observed_at_ms,
        }
    }

    fn prune(&mut self, now_ms: u64) {
        while let Some((sample_ms, _)) = self.price_samples.front().copied() {
            if now_ms.saturating_sub(sample_ms) <= BTC_SIGNAL_WINDOW_20M_MS {
                break;
            }
            self.price_samples.pop_front();
        }
        while let Some(sample_ms) = self.trade_times.front().copied() {
            if now_ms.saturating_sub(sample_ms) <= BTC_SIGNAL_WINDOW_20M_MS {
                break;
            }
            self.trade_times.pop_front();
        }
        while self.price_samples.len() > MAX_BTC_PRICE_SAMPLES {
            self.price_samples.pop_front();
        }
    }

    fn trade_count(&self, now_ms: u64, window_ms: u64) -> u64 {
        self.trade_times
            .iter()
            .rev()
            .take_while(|&&sample_ms| now_ms.saturating_sub(sample_ms) <= window_ms)
            .count() as u64
    }

    fn realized_vol_bps(&self, now_ms: u64, window_ms: u64) -> Option<f64> {
        let points = self
            .price_samples
            .iter()
            .copied()
            .filter(|(sample_ms, price)| {
                now_ms.saturating_sub(*sample_ms) <= window_ms && price.is_finite() && *price > 0.0
            })
            .collect::<Vec<_>>();
        if points.len() < 2 {
            return None;
        }
        let mut returns = Vec::with_capacity(points.len().saturating_sub(1));
        for pair in points.windows(2) {
            let prev = pair[0].1;
            let next = pair[1].1;
            if prev > 0.0 && next > 0.0 {
                returns.push(((next / prev) - 1.0) * 10_000.0);
            }
        }
        if returns.len() < 2 {
            return None;
        }
        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let var = returns
            .iter()
            .map(|value| {
                let d = *value - mean;
                d * d
            })
            .sum::<f64>()
            / returns.len() as f64;
        Some(var.sqrt())
    }

    fn return_bps(&self, now_ms: u64, horizon_ms: u64) -> Option<f64> {
        let current = self.last_price?;
        let target_ms = now_ms.saturating_sub(horizon_ms);
        let baseline = self
            .price_samples
            .iter()
            .rev()
            .find(|(sample_ms, _)| *sample_ms <= target_ms)
            .map(|(_, price)| *price)
            .or_else(|| {
                self.price_samples
                    .iter()
                    .find(|(sample_ms, _)| *sample_ms >= target_ms)
                    .map(|(_, price)| *price)
            })?;
        if baseline <= 0.0 {
            return None;
        }
        Some(((current / baseline) - 1.0) * 10_000.0)
    }
}

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
                | ManagedOrderStatus::Quarantined
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
                    | Quarantined
            ),
            Submitted => matches!(
                next,
                Working
                    | CancelRequested
                    | Filled
                    | Cancelled
                    | Rejected
                    | NeedsReconcile
                    | Quarantined
            ),
            Working => matches!(
                next,
                CancelRequested | Filled | Cancelled | Rejected | NeedsReconcile | Quarantined
            ),
            CancelRequested => {
                matches!(
                    next,
                    Cancelled | Filled | Rejected | NeedsReconcile | Quarantined
                )
            }
            Filled | Cancelled | Rejected | Quarantined => false,
            NeedsReconcile => matches!(
                next,
                PendingSubmit
                    | Submitted
                    | Working
                    | CancelRequested
                    | Filled
                    | Cancelled
                    | Rejected
                    | Quarantined
            ),
        }
    }
}

pub struct Runtime<S: Strategy> {
    strategy: S,
    unlawful_gate_config: Option<UnlawfulGateConfig>,
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
    btc_signals: BtcSignalStore,
    market_activity: HashMap<InstrumentId, GateMarketActivitySignal>,
    first_fill_by_market: HashMap<MarketId, EpochMillis>,
    first_merge_by_market: HashMap<MarketId, EpochMillis>,
    pending_merge_by_market: HashMap<MarketId, MergeIntent>,
    condition_id_by_market: HashMap<MarketId, String>,
    unlawful_mode_by_market: HashMap<MarketId, StrategyExecutionMode>,
    last_persisted_unlawful_signal_by_market:
        HashMap<MarketId, (EpochMillis, StrategyExecutionMode)>,
    markets_with_unresolved_drift: HashSet<MarketId>,
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
        let unlawful_gate_config = strategy.unlawful_gate_config();
        Self {
            strategy,
            unlawful_gate_config,
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
            btc_signals: BtcSignalStore::default(),
            market_activity: HashMap::new(),
            first_fill_by_market: HashMap::new(),
            first_merge_by_market: HashMap::new(),
            pending_merge_by_market: HashMap::new(),
            condition_id_by_market: HashMap::new(),
            unlawful_mode_by_market: HashMap::new(),
            last_persisted_unlawful_signal_by_market: HashMap::new(),
            markets_with_unresolved_drift: HashSet::new(),
            order_store,
        }
    }

    pub fn status(&self) -> RuntimeStatus {
        self.status
    }

    pub fn has_needs_reconcile_orders(&self) -> bool {
        self.open_orders
            .values()
            .any(|managed| managed.status == ManagedOrderStatus::NeedsReconcile)
    }

    pub fn set_quote_reconciler_config(
        &mut self,
        config: crate::quote_reconciler::ReconcilerConfig,
    ) {
        self.quote_reconciler = QuoteReconciler::new(config);
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
            outcome.push_event(
                self.event_log.push(
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
                ),
            );
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

    pub fn reconcile_venue_positions(
        &mut self,
        venue_positions: &[VenuePositionSnapshot],
        observed_at_ms: EpochMillis,
    ) -> Result<InventoryReconciliationReport, RuntimeError> {
        for venue_position in venue_positions {
            if let Some(condition_id) = venue_position
                .condition_id
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                self.condition_id_by_market
                    .insert(venue_position.market_id.clone(), condition_id.to_string());
            }
        }
        let report = self
            .inventory
            .reconcile_venue_positions(venue_positions, observed_at_ms)?;
        const DRIFT_QTY_EPSILON: f64 = 1e-6;
        for delta in &report.deltas {
            let local_was_flat = delta.local_quantity_before.abs() < DRIFT_QTY_EPSILON;
            let venue_has_position = delta.venue_quantity.abs() > DRIFT_QTY_EPSILON;
            if local_was_flat && venue_has_position {
                if self
                    .markets_with_unresolved_drift
                    .insert(delta.market_id.clone())
                {
                    self.event_log.push(
                        EventRecord::new(
                            EventCategory::Inventory,
                            observed_at_ms,
                            format!(
                                "drift block engaged: local was flat but venue qty={:.8}; \
                                 fresh entry suppressed in this market until drift clears \
                                 (incident #1 guard)",
                                delta.venue_quantity
                            ),
                        )
                        .with_market(delta.market_id.clone())
                        .with_instrument(delta.instrument_id.clone()),
                    );
                }
            } else if delta.quantity_delta.abs() < DRIFT_QTY_EPSILON
                && self
                    .markets_with_unresolved_drift
                    .remove(&delta.market_id)
            {
                self.event_log.push(
                    EventRecord::new(
                        EventCategory::Inventory,
                        observed_at_ms,
                        "drift block cleared: local now matches venue",
                    )
                    .with_market(delta.market_id.clone())
                    .with_instrument(delta.instrument_id.clone()),
                );
            }
            self.event_log.push(
                EventRecord::new(
                    EventCategory::Inventory,
                    observed_at_ms,
                    format!(
                        "venue position reconciled: local_qty={:.8} venue_qty={:.8} delta={:.8}",
                        delta.local_quantity_before, delta.venue_quantity, delta.quantity_delta
                    ),
                )
                .with_market(delta.market_id.clone())
                .with_instrument(delta.instrument_id.clone())
                .with_metrics(EventMetrics {
                    price: Some(delta.venue_avg_price),
                    quantity: Some(delta.venue_quantity),
                    notional_usd: Some(delta.venue_quantity * delta.venue_avg_price),
                    cash_delta_usd: None,
                    position_delta: Some(delta.quantity_delta),
                    free_cash_after_usd: Some(self.inventory.free_cash_usd()),
                    gross_exposure_after_usd: Some(report.gross_exposure_after_usd),
                    risk_reject_reason: None,
                }),
            );
        }
        for stranded in &report.stranded_markets {
            self.event_log.push(
                EventRecord::new(
                    EventCategory::Inventory,
                    observed_at_ms,
                    format!(
                        "venue reconciliation found stranded inventory: paired_qty={:.8} stranded_legs={}",
                        stranded.paired_quantity,
                        stranded.stranded_positions.len()
                    ),
                )
                .with_market(stranded.market_id.clone()),
            );
        }
        Ok(report)
    }

    pub fn stranded_inventory(&self) -> Vec<StrandedMarketInventory> {
        self.inventory.stranded_market_inventory()
    }

    pub fn plan_merge_command_for_market(
        &mut self,
        market_id: &MarketId,
        now_ms: EpochMillis,
        reason: impl Into<String>,
    ) -> RuntimeOutcome {
        let mut outcome = RuntimeOutcome::default();
        if self.pending_merge_by_market.contains_key(market_id) {
            return outcome;
        }

        let reason = reason.into();
        let Some(mut intent) = self
            .merge_executor
            .merge_intent(market_id, now_ms, reason.clone())
            .or_else(|| self.inventory_merge_intent(market_id, now_ms, reason.clone()))
        else {
            return outcome;
        };
        if intent.condition_id.is_none() {
            intent.condition_id = self.condition_id_by_market.get(market_id).cloned();
        }

        self.pending_merge_by_market
            .insert(market_id.clone(), intent.clone());
        outcome.push_event(
            self.event_log.push(
                EventRecord::new(
                    EventCategory::Execution,
                    now_ms,
                    format!(
                        "merge intent planned: qty={:.8} cash={:.4} cost={:.4} net_gain={:.4} reason={}",
                        intent.quantity,
                        intent.expected_cash_usd,
                        intent.expected_cost_usd,
                        intent.expected_net_gain_usd(),
                        intent.reason
                    ),
                )
                .with_market(intent.market_id.clone())
                .with_instrument(intent.yes_instrument_id.clone())
                .with_client_order(intent.command_id.clone()),
            ),
        );
        outcome.push_command(RuntimeCommand::Merge(intent));
        outcome
    }

    fn inventory_merge_intent(
        &self,
        market_id: &MarketId,
        now_ms: EpochMillis,
        reason: String,
    ) -> Option<MergeIntent> {
        let mut positions = self
            .inventory
            .positions()
            .filter(|position| position.market_id == *market_id && position.quantity > 1e-9)
            .collect::<Vec<_>>();
        positions.sort_by(|left, right| left.instrument_id.cmp(&right.instrument_id));
        if positions.len() != 2 {
            return None;
        }

        let quantity = positions[0].quantity.min(positions[1].quantity);
        if quantity <= 1e-9 {
            return None;
        }
        let expected_cost_usd =
            quantity * positions[0].avg_price + quantity * positions[1].avg_price;

        Some(MergeIntent {
            command_id: ClientOrderId::from(format!(
                "merge:{}:{:.8}:{}",
                market_id, quantity, now_ms
            )),
            market_id: market_id.clone(),
            condition_id: self.condition_id_by_market.get(market_id).cloned(),
            yes_instrument_id: positions[0].instrument_id.clone(),
            no_instrument_id: positions[1].instrument_id.clone(),
            quantity,
            expected_cash_usd: quantity,
            expected_cost_usd,
            expected_fee_usd: 0.0,
            expected_gas_usd: 0.0,
            reason,
            created_at_ms: now_ms,
        })
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
                            outcome.push_event(
                                self.event_log.push(
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
                                ),
                            );
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
                    outcome.push_event(
                        self.event_log.push(
                            EventRecord::new(
                                EventCategory::Runtime,
                                now_ms,
                                "restored missing open order during durable sync",
                            )
                            .with_market(managed.intent.market_id.clone())
                            .with_instrument(managed.intent.instrument_id.clone())
                            .with_client_order(managed.intent.client_order_id.clone()),
                        ),
                    );
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
                Ok(records) => records
                    .into_iter()
                    .map(Self::checkpoint_order_from_record)
                    .collect(),
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
            self.open_orders
                .insert(client_order_id.clone(), managed.clone());
            outcome.push_event(
                self.event_log.push(
                    EventRecord::new(
                        EventCategory::Runtime,
                        observed_at_ms,
                        "restored order from checkpoint",
                    )
                    .with_market(managed.intent.market_id.clone())
                    .with_instrument(managed.intent.instrument_id.clone())
                    .with_client_order(client_order_id),
                ),
            );
        }

        outcome.push_event(
            self.event_log
                .push(EventRecord::runtime_status(observed_at_ms, self.status)),
        );
        outcome
    }

    pub fn last_quote(&self, instrument_id: &InstrumentId) -> Option<&crate::types::QuoteSnapshot> {
        self.last_quotes.get(instrument_id)
    }

    pub fn start(&mut self, now_ms: EpochMillis) -> RuntimeOutcome {
        if self.status == RuntimeStatus::Degraded {
            let mut outcome = RuntimeOutcome::default();
            outcome.push_event(self.event_log.push(EventRecord::new(
                EventCategory::Runtime,
                now_ms,
                "runtime start skipped because runtime is degraded",
            )));
            outcome.push_event(
                self.event_log
                    .push(EventRecord::runtime_status(now_ms, self.status)),
            );
            return outcome;
        }

        self.status = RuntimeStatus::Running;
        let mut outcome = RuntimeOutcome::default();
        outcome.push_event(
            self.event_log
                .push(EventRecord::runtime_status(now_ms, self.status)),
        );
        let context = self.strategy_context(now_ms, None);
        let decision = self.strategy.on_start(&context);
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
            outcome.push_event(
                self.event_log
                    .push(adjustment.to_event("inventory mark refreshed from market snapshot")),
            );
            let context = self.strategy_context(now_ms, Some(&snapshot.market_id));
            let decision = self.strategy.on_market_snapshot(&context, &snapshot);
            outcome.extend(self.accept_strategy_decision(decision, now_ms));
            if let Some(signal) = context.unlawful_signal.as_ref() {
                outcome.extend(self.enforce_unlawful_mode(&snapshot.market_id, signal, now_ms));
            }
            Ok(outcome)
        } else {
            let context = self.strategy_context(now_ms, Some(&snapshot.market_id));
            let decision = self.strategy.on_market_snapshot(&context, &snapshot);
            let mut outcome = self.accept_strategy_decision(decision, now_ms);
            if let Some(signal) = context.unlawful_signal.as_ref() {
                outcome.extend(self.enforce_unlawful_mode(&snapshot.market_id, signal, now_ms));
            }
            Ok(outcome)
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
                bid_levels: book
                    .bid_levels()
                    .iter()
                    .map(|level| crate::types::BookLevel::new(level.price, level.size))
                    .collect(),
                ask_levels: book
                    .ask_levels()
                    .iter()
                    .map(|level| crate::types::BookLevel::new(level.price, level.size))
                    .collect(),
                depth_observed_at_ms: (book.depth_update_unix_ms > 0)
                    .then_some(book.depth_update_unix_ms),
                last_trade_price,
                observed_at_ms: book.last_update_unix_ms,
            },
        })
    }

    pub fn on_market_activity(
        &mut self,
        instrument_id: InstrumentId,
        signal: GateMarketActivitySignal,
    ) {
        self.market_activity.insert(instrument_id, signal);
    }

    pub fn on_btc_trade(&mut self, price: f64, observed_at_ms: EpochMillis) {
        self.btc_signals.record_trade(price, observed_at_ms);
    }

    pub fn on_fill(&mut self, fill: FillReport) -> Result<RuntimeOutcome, RuntimeError> {
        let now_ms = fill.observed_at_ms;
        let merge_flow = matches!(
            fill.close_method,
            Some(CloseMethod::Merge) | Some(CloseMethod::Settle) | Some(CloseMethod::Settlement)
        );
        let redeem_flow = matches!(fill.close_method, Some(CloseMethod::Redeem));
        let mut outcome = RuntimeOutcome::default();
        outcome.push_event(
            self.event_log.push(
                EventRecord::new(
                    EventCategory::Execution,
                    now_ms,
                    if let Some(close_method) = fill.close_method {
                        format!(
                            "received fill report via close_method={}",
                            close_method.as_str()
                        )
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
            ),
        );

        if redeem_flow {
            outcome.push_event(
                self.event_log.push(
                    EventRecord::new(
                        EventCategory::Inventory,
                        now_ms,
                        "redeem close event observed; awaiting venue position snapshot reconciliation",
                    )
                    .with_market(fill.market_id)
                    .with_instrument(fill.instrument_id),
                ),
            );
            return Ok(outcome);
        }

        let mut executed_qty = 0.0;
        if merge_flow {
            self.pending_merge_by_market.remove(&fill.market_id);
            if let Some(execution) = self
                .merge_executor
                .apply_merge(&fill, &mut self.inventory)?
            {
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
                outcome.push_event(
                    self.event_log.push(
                        execution
                            .adjustment
                            .to_event("inventory updated from merge completion"),
                    ),
                );
            } else {
                outcome.push_event(
                    self.event_log.push(
                        EventRecord::new(
                            EventCategory::Execution,
                            now_ms,
                            format!(
                                "merge skipped: no mergeable quantity for {}",
                                fill.market_id
                            ),
                        )
                        .with_market(fill.market_id.clone())
                        .with_instrument(fill.instrument_id.clone()),
                    ),
                );
            }
        } else {
            let adjustment = self.inventory.apply_fill(&fill)?;
            self.merge_executor.on_fill(&fill);
            executed_qty = fill.quantity;
            outcome.push_event(
                self.event_log
                    .push(adjustment.to_event("inventory updated from fill")),
            );
            outcome.extend(self.plan_merge_command_for_market(
                &fill.market_id,
                now_ms,
                "paired inventory after fill",
            ));
        }

        if executed_qty > 0.0 {
            if merge_flow {
                self.first_merge_by_market
                    .entry(fill.market_id.clone())
                    .or_insert(now_ms);
            } else {
                self.first_fill_by_market
                    .entry(fill.market_id.clone())
                    .or_insert(now_ms);
            }
        }

        if let Some(client_order_id) = &fill.client_order_id {
            let mut remove_after = false;
            let mut next_status = None;
            let mut persist_fill = false;
            if let Some(managed) = self.open_orders.get_mut(client_order_id) {
                managed.cumulative_filled_qty += executed_qty;
                managed.last_update_ms = now_ms;
                persist_fill = executed_qty > 0.0;
                if managed.remaining_qty() <= 1e-9 {
                    remove_after = true;
                    next_status = Some(ManagedOrderStatus::Filled);
                } else if matches!(managed.status, ManagedOrderStatus::CancelRequested) {
                    next_status = None;
                } else {
                    next_status = Some(ManagedOrderStatus::Working);
                }
            }
            if persist_fill || !self.open_orders.contains_key(client_order_id) {
                if let Some(store) = self.order_store.as_mut() {
                    if let Err(error) = store.apply_fill(client_order_id, executed_qty, now_ms) {
                        warn!(
                            run_id = %self.run_id,
                            error = ?error,
                            client_order_id = %client_order_id,
                            "unable to persist fill in durable order store"
                        );
                    }
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
                    outcome.push_event(
                        self.event_log
                            .push(release.to_event("released reservation after full fill")),
                    );
                }
                self.open_orders.remove(client_order_id);
            }
        }

        let context = self.strategy_context(now_ms, Some(&fill.market_id));
        let decision = self.strategy.on_fill(&context, &fill);
        outcome.extend(self.accept_strategy_decision(decision, now_ms));
        self.maybe_clear_market_timing(&fill.market_id);
        Ok(outcome)
    }

    pub fn on_order_opened(
        &mut self,
        client_order_id: &ClientOrderId,
        now_ms: EpochMillis,
    ) -> RuntimeOutcome {
        self.on_order_opened_with_venue(client_order_id, None, now_ms)
    }

    pub fn on_order_opened_with_venue(
        &mut self,
        client_order_id: &ClientOrderId,
        venue_order_id: Option<OrderId>,
        now_ms: EpochMillis,
    ) -> RuntimeOutcome {
        let mut outcome = RuntimeOutcome::default();
        if let Some(venue_order_id) = venue_order_id {
            if let Some(store) = self.order_store.as_mut() {
                if let Err(error) =
                    store.attach_venue_id(client_order_id, venue_order_id.clone(), now_ms)
                {
                    warn!(
                        run_id = %self.run_id,
                        error = ?error,
                        client_order_id = %client_order_id,
                        venue_order_id = %venue_order_id,
                        "failed to persist venue order id after submit acknowledgement"
                    );
                    outcome.push_event(
                        self.event_log.push(
                            EventRecord::new(
                                EventCategory::Execution,
                                now_ms,
                                format!(
                                    "failed to persist venue order id after submit acknowledgement: {error}"
                                ),
                            )
                            .with_client_order(client_order_id.clone()),
                        ),
                    );
                    outcome.extend(self.mark_order_needs_reconcile(
                        client_order_id,
                        now_ms,
                        "failed to persist venue order id after submit acknowledgement",
                    ));
                    return outcome;
                }
            }
            outcome.push_event(
                self.event_log.push(
                    EventRecord::new(
                        EventCategory::Execution,
                        now_ms,
                        format!("attached venue order id {venue_order_id}"),
                    )
                    .with_client_order(client_order_id.clone()),
                ),
            );
        }
        outcome.extend(self.set_order_status(
            client_order_id,
            ManagedOrderStatus::Working,
            now_ms,
            "order acknowledged by downstream execution layer",
        ));
        outcome.push_event(
            self.event_log.push(
                EventRecord::new(
                    EventCategory::Execution,
                    now_ms,
                    "order acknowledged by downstream execution layer",
                )
                .with_client_order(client_order_id.clone()),
            ),
        );
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
                outcome.push_event(
                    self.event_log
                        .push(release.to_event("released reservation after downstream rejection")),
                );
            }
            outcome.push_event(
                self.event_log.push(
                    EventRecord::new(EventCategory::Execution, now_ms, reason)
                        .with_market(managed.intent.market_id.clone())
                        .with_instrument(managed.intent.instrument_id.clone())
                        .with_client_order(client_order_id.clone()),
                ),
            );
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
                outcome.push_event(
                    self.event_log
                        .push(release.to_event("released reservation after cancellation")),
                );
            }
            outcome.push_event(
                self.event_log.push(
                    EventRecord::new(EventCategory::Execution, now_ms, reason)
                        .with_market(managed.intent.market_id.clone())
                        .with_instrument(managed.intent.instrument_id.clone())
                        .with_client_order(client_order_id.clone()),
                ),
            );
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

    pub fn request_cancel_order(
        &mut self,
        client_order_id: &ClientOrderId,
        now_ms: EpochMillis,
        reason: impl Into<String>,
    ) -> RuntimeOutcome {
        self.request_cancel(client_order_id, reason, now_ms)
    }

    pub fn degrade_and_cancel_all(
        &mut self,
        now_ms: EpochMillis,
        reason: impl Into<String>,
    ) -> RuntimeOutcome {
        let reason = reason.into();
        let mut outcome = RuntimeOutcome::default();
        if self.status != RuntimeStatus::Degraded {
            self.status = RuntimeStatus::Degraded;
            outcome.push_event(self.event_log.push(EventRecord::new(
                EventCategory::Runtime,
                now_ms,
                format!("runtime degraded: {reason}"),
            )));
            outcome.push_event(
                self.event_log
                    .push(EventRecord::runtime_status(now_ms, self.status)),
            );
        }
        outcome.extend(self.request_cancel_all(now_ms, reason));
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
        let (market_id, instrument_id, status) = match self.open_orders.get(client_order_id) {
            Some(managed) => {
                if matches!(
                    managed.status,
                    ManagedOrderStatus::NeedsReconcile
                        | ManagedOrderStatus::CancelRequested
                        | ManagedOrderStatus::Filled
                        | ManagedOrderStatus::Cancelled
                        | ManagedOrderStatus::Rejected
                ) {
                    outcome.push_event(
                        self.event_log.push(
                            EventRecord::new(
                                EventCategory::Runtime,
                                now_ms,
                                format!(
                                    "skipped duplicate/unsafe cancel for status {:?}: {}",
                                    managed.status, reason
                                ),
                            )
                            .with_market(managed.intent.market_id.clone())
                            .with_instrument(managed.intent.instrument_id.clone())
                            .with_client_order(client_order_id.clone()),
                        ),
                    );
                    return outcome;
                }
                if self.should_keep_btc_mm_hedge_order(managed, reason.as_str()) {
                    outcome.push_event(
                        self.event_log.push(
                            EventRecord::new(
                                EventCategory::Runtime,
                                now_ms,
                                format!(
                                    "kept active btc hedge order during one-sided inventory: {}",
                                    reason
                                ),
                            )
                            .with_market(managed.intent.market_id.clone())
                            .with_instrument(managed.intent.instrument_id.clone())
                            .with_client_order(client_order_id.clone()),
                        ),
                    );
                    return outcome;
                }
                (
                    managed.intent.market_id.clone(),
                    managed.intent.instrument_id.clone(),
                    managed.status,
                )
            }
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
        outcome.push_event(
            self.event_log.push(
                EventRecord::new(
                    EventCategory::Runtime,
                    now_ms,
                    format!("requested order cancellation from {:?}", status),
                )
                .with_market(market_id)
                .with_instrument(instrument_id)
                .with_client_order(client_order_id.clone()),
            ),
        );
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
                |snapshot, now| {
                    snapshot.is_none_or(|quote| {
                        now.saturating_sub(quote.observed_at_ms) > self.quote_stale_ms
                    })
                },
            );
        let plan = self
            .quote_reconciler
            .plan(desired, &self.open_orders, now_ms);
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
                    outcome.push_event(
                        self.event_log.push(
                            EventRecord::new(EventCategory::Runtime, now_ms, "quote keep")
                                .with_market(intent.market_id.clone())
                                .with_instrument(intent.instrument_id.clone())
                                .with_client_order(intent.client_order_id.clone()),
                        ),
                    );
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
                    outcome.extend(self.request_cancel(
                        &existing_client_order_id,
                        cancel_reason,
                        now_ms,
                    ));
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
        if self.has_active_btc_mm_buy_for_instrument(&intent) {
            outcome.push_event(
                self.event_log.push(
                    EventRecord::new(
                        EventCategory::Runtime,
                        now_ms,
                        "duplicate active btc buy intent rejected before risk",
                    )
                    .with_market(intent.market_id.clone())
                    .with_instrument(intent.instrument_id.clone())
                    .with_client_order(intent.client_order_id.clone()),
                ),
            );
            return outcome;
        }
        if self.open_orders.contains_key(&intent.client_order_id) {
            outcome.push_event(
                self.event_log.push(
                    EventRecord::new(
                        EventCategory::Runtime,
                        now_ms,
                        "duplicate client_order_id rejected before risk",
                    )
                    .with_market(intent.market_id.clone())
                    .with_instrument(intent.instrument_id.clone())
                    .with_client_order(intent.client_order_id.clone()),
                ),
            );
            return outcome;
        }
        if !intent.reduce_only
            && self
                .markets_with_unresolved_drift
                .contains(&intent.market_id)
        {
            outcome.push_event(
                self.event_log.push(
                    EventRecord::new(
                        EventCategory::Runtime,
                        now_ms,
                        "fresh entry suppressed: market has unresolved local-flat/venue-nonflat \
                         drift (incident #1 guard)",
                    )
                    .with_market(intent.market_id.clone())
                    .with_instrument(intent.instrument_id.clone())
                    .with_client_order(intent.client_order_id.clone()),
                ),
            );
            return outcome;
        }
        if intent.side == TradeSide::Sell && intent.reduce_only {
            if self.pending_merge_by_market.contains_key(&intent.market_id) {
                outcome.push_event(
                    self.event_log.push(
                        EventRecord::new(
                            EventCategory::Runtime,
                            now_ms,
                            "reduce-only sell suppressed because merge is already pending",
                        )
                        .with_market(intent.market_id.clone())
                        .with_instrument(intent.instrument_id.clone())
                        .with_client_order(intent.client_order_id.clone()),
                    ),
                );
                return outcome;
            }
            let merge_outcome = self.plan_merge_command_for_market(
                &intent.market_id,
                now_ms,
                format!(
                    "suppressed sell cleanup {}; mergeable paired inventory exists",
                    intent.client_order_id
                ),
            );
            if !merge_outcome.commands.is_empty() {
                outcome.push_event(
                    self.event_log.push(
                        EventRecord::new(
                            EventCategory::Runtime,
                            now_ms,
                            "reduce-only sell suppressed because paired inventory is mergeable",
                        )
                        .with_market(intent.market_id.clone())
                        .with_instrument(intent.instrument_id.clone())
                        .with_client_order(intent.client_order_id.clone()),
                    ),
                );
                outcome.extend(merge_outcome);
                return outcome;
            }
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
                    let record = OrderRecord::from_intent(
                        self.run_id.clone(),
                        &managed.intent,
                        self.strategy.name(),
                    );
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
                            outcome.push_event(self.event_log.push(release.to_event(
                                "released reservation after durable persistence failure",
                            )));
                        }
                        outcome.push_event(
                            self.event_log.push(
                                EventRecord::new(
                                    EventCategory::Runtime,
                                    now_ms,
                                    "order not accepted due to durable store failure",
                                )
                                .with_client_order(intent.client_order_id.clone()),
                            ),
                        );
                        return outcome;
                    }
                }
                self.open_orders
                    .insert(intent.client_order_id.clone(), managed);
                outcome.push_command(RuntimeCommand::Submit(intent));
            }
            Err(source) => {
                outcome.push_event(
                    self.event_log.push(
                        EventRecord::new(
                            EventCategory::Inventory,
                            now_ms,
                            format!("inventory rejected submit: {source}"),
                        )
                        .with_market(intent.market_id.clone())
                        .with_instrument(intent.instrument_id.clone())
                        .with_client_order(intent.client_order_id.clone()),
                    ),
                );
            }
        }
        outcome
    }

    fn should_keep_btc_mm_hedge_order(&self, managed: &ManagedOrder, reason: &str) -> bool {
        if reason != "no longer desired" || !Self::is_btc_mm_buy_intent(&managed.intent) {
            return false;
        }
        if !matches!(
            managed.status,
            ManagedOrderStatus::PendingSubmit
                | ManagedOrderStatus::Submitted
                | ManagedOrderStatus::Working
        ) {
            return false;
        }

        let mut same_market_positions = self.inventory.positions().filter(|position| {
            position.market_id == managed.intent.market_id && position.quantity > 1e-9
        });
        let Some(position) = same_market_positions.next() else {
            return false;
        };
        if same_market_positions.next().is_some() {
            return false;
        }
        position.instrument_id != managed.intent.instrument_id
    }

    fn has_active_btc_mm_buy_for_instrument(&self, intent: &OrderIntent) -> bool {
        if !Self::is_btc_mm_buy_intent(intent) {
            return false;
        }
        self.open_orders.values().any(|managed| {
            managed.intent.client_order_id != intent.client_order_id
                && Self::is_btc_mm_buy_intent(&managed.intent)
                && managed.intent.market_id == intent.market_id
                && managed.intent.instrument_id == intent.instrument_id
                && matches!(
                    managed.status,
                    ManagedOrderStatus::PendingSubmit
                        | ManagedOrderStatus::Submitted
                        | ManagedOrderStatus::Working
                        | ManagedOrderStatus::CancelRequested
                        | ManagedOrderStatus::NeedsReconcile
                )
        })
    }

    fn is_btc_mm_buy_intent(intent: &OrderIntent) -> bool {
        intent.client_order_id.as_str().starts_with("btc-5m-mm:")
            && intent.side == TradeSide::Buy
            && !intent.reduce_only
    }

    fn strategy_context(
        &mut self,
        now_ms: EpochMillis,
        market_id: Option<&MarketId>,
    ) -> StrategyContext {
        let market_context = market_id.and_then(|id| self.market_contexts.get(id).cloned());
        let unlawful_gate_config = self.unlawful_gate_config.clone();
        let unlawful_signal = match (market_id, unlawful_gate_config.as_ref()) {
            (Some(id), Some(cfg)) => {
                Some(self.build_unlawful_signal(id, market_context.as_ref(), now_ms, cfg))
            }
            _ => None,
        };
        StrategyContext {
            now_ms,
            runtime_status: self.status,
            inventory: self.inventory.snapshot(),
            open_orders_total: self.open_orders.len(),
            open_orders_for_market: market_id
                .map(|id| self.open_orders_for_market(id))
                .unwrap_or(0),
            market_context,
            unlawful_signal,
        }
    }

    fn build_unlawful_signal(
        &mut self,
        market_id: &MarketId,
        market_context: Option<&crate::market_context::MarketContextRecord>,
        now_ms: EpochMillis,
        cfg: &UnlawfulGateConfig,
    ) -> StrategyUnlawfulSignalSnapshot {
        let paired_book = self.build_paired_book_signal(market_id, market_context, now_ms, cfg);
        let activity = self.aggregate_market_activity(&paired_book);
        let btc = self.btc_signals.snapshot(now_ms);
        let session_bucket = self.classify_session_bucket(market_context, cfg);
        let has_inventory = self.market_has_inventory(market_id);
        let cleanup_backlog = self
            .open_orders
            .values()
            .filter(|managed| &managed.intent.market_id == market_id)
            .filter(|managed| {
                matches!(
                    managed.status,
                    ManagedOrderStatus::NeedsReconcile | ManagedOrderStatus::CancelRequested
                )
            })
            .count();
        let cleanup_backlog_exceeded =
            cleanup_backlog > self.risk.limits().max_open_orders_per_market;
        let inventory_imbalance_exceeded =
            self.inventory.net_exposure_for_market_usd(market_id).abs()
                > self.risk.limits().max_net_notional_per_market_usd;

        let inputs = UnlawfulGateInputs {
            session_bucket,
            now_ms,
            market_start_ms: market_context.and_then(|ctx| ctx.event_start_time_ms),
            market_end_ms: market_context.and_then(|ctx| ctx.event_end_time_ms),
            market_context_present: market_context.is_some(),
            has_inventory,
            cleanup_backlog_exceeded,
            first_fill_ms: self.first_fill_by_market.get(market_id).copied(),
            first_merge_ms: self.first_merge_by_market.get(market_id).copied(),
            btc,
            book: paired_book,
            activity,
        };

        let mut signal = evaluate_unlawful_mode(&inputs, cfg);
        if inventory_imbalance_exceeded {
            signal.mode = if signal.mode == GateExecutionMode::Flatten {
                GateExecutionMode::Flatten
            } else {
                GateExecutionMode::Cleanup
            };
            signal.gate_reasons.push(format!(
                "inventory imbalance exceeded hard cap {:.2}",
                self.risk.limits().max_net_notional_per_market_usd
            ));
            signal.clip_scale = 0.0;
        }
        let strategy_signal = self.map_signal_snapshot(signal);
        self.persist_unlawful_signal_snapshot(market_id, now_ms, &strategy_signal);
        strategy_signal
    }

    fn persist_unlawful_signal_snapshot(
        &mut self,
        market_id: &MarketId,
        now_ms: EpochMillis,
        signal: &StrategyUnlawfulSignalSnapshot,
    ) {
        let should_persist = match self.last_persisted_unlawful_signal_by_market.get(market_id) {
            Some((last_ms, last_mode)) => {
                signal.mode != *last_mode
                    || now_ms.saturating_sub(*last_ms) >= SIGNAL_SNAPSHOT_PERSIST_INTERVAL_MS
            }
            None => true,
        };
        if !should_persist {
            return;
        }

        let Some(store) = self.order_store.as_mut() else {
            return;
        };

        let record = SignalSnapshotRecord {
            run_id: self.run_id.clone(),
            market_id: market_id.clone(),
            observed_at_ms: now_ms,
            session_bucket: format!("{:?}", signal.session_bucket),
            mode: format!("{:?}", signal.mode),
            aggression_tier: None,
            cheap_instrument_id: signal.book.cheap_instrument_id.clone(),
            expensive_instrument_id: signal.book.expensive_instrument_id.clone(),
            cheap_bid: signal.book.cheap_bid.as_ref().map(|level| level.price),
            cheap_ask: signal.book.cheap_ask.as_ref().map(|level| level.price),
            expensive_bid: signal.book.expensive_bid.as_ref().map(|level| level.price),
            expensive_ask: signal.book.expensive_ask.as_ref().map(|level| level.price),
            price_gap: signal.book.price_gap,
            books_fresh: signal.book.books_fresh,
            both_sides_present: signal.book.both_sides_present,
            cheap_spread: signal.book.cheap_spread,
            expensive_spread: signal.book.expensive_spread,
            cheap_bid_depth_top3_qty: signal.book.cheap_bid_depth_top3_qty,
            cheap_ask_depth_top3_qty: signal.book.cheap_ask_depth_top3_qty,
            expensive_bid_depth_top3_qty: signal.book.expensive_bid_depth_top3_qty,
            expensive_ask_depth_top3_qty: signal.book.expensive_ask_depth_top3_qty,
            cheap_bid_notional_top3: signal.book.cheap_bid_notional_top3,
            cheap_ask_notional_top3: signal.book.cheap_ask_notional_top3,
            expensive_bid_notional_top3: signal.book.expensive_bid_notional_top3,
            expensive_ask_notional_top3: signal.book.expensive_ask_notional_top3,
            cheap_depth_imbalance_top3: signal.book.cheap_depth_imbalance_top3,
            expensive_depth_imbalance_top3: signal.book.expensive_depth_imbalance_top3,
            btc_last_price: signal.btc.last_price,
            btc_realized_vol_5m_bps: signal.btc.realized_vol_5m_bps,
            btc_realized_vol_15m_bps: signal.btc.realized_vol_15m_bps,
            btc_trade_count_5m: signal.btc.trade_count_5m,
            btc_trade_count_15m: signal.btc.trade_count_15m,
            btc_return_30s_bps: signal.btc.return_30s_bps,
            btc_return_60s_bps: signal.btc.return_60s_bps,
            btc_observed_at_ms: signal.btc.observed_at_ms,
            activity_10s: signal.activity.last_trade_event_count_10s,
            activity_30s: signal.activity.last_trade_event_count_30s,
            activity_60s: signal.activity.last_trade_event_count_60s,
            activity_age_ms: signal.activity.last_trade_event_age_ms,
            first_fill_ms: signal.first_fill_ms,
            first_merge_ms: signal.first_merge_ms,
            elapsed_s: signal.elapsed_s,
            time_remaining_s: signal.time_remaining_s,
            clip_scale: signal.clip_scale,
            gate_reasons: if signal.gate_reasons.is_empty() {
                "none".to_string()
            } else {
                signal.gate_reasons.join(";")
            },
        };

        if let Err(error) = store.insert_signal_snapshot(record) {
            warn!(
                error = ?error,
                run_id = %self.run_id,
                market_id = %market_id,
                "failed to persist unlawful signal snapshot"
            );
            return;
        }

        self.last_persisted_unlawful_signal_by_market
            .insert(market_id.clone(), (now_ms, signal.mode));
    }

    fn classify_session_bucket(
        &self,
        market_context: Option<&crate::market_context::MarketContextRecord>,
        cfg: &UnlawfulGateConfig,
    ) -> GateSessionBucket {
        let Some(start_ms) = market_context.and_then(|ctx| ctx.event_start_time_ms) else {
            return GateSessionBucket::Opportunistic;
        };
        let hour = ((start_ms / 1_000 / 3_600) % 24) as u8;
        if cfg.regime_primary_hours_utc.contains(&hour) {
            GateSessionBucket::Preferred
        } else if cfg.regime_secondary_hours_utc.contains(&hour) {
            GateSessionBucket::Neutral
        } else {
            GateSessionBucket::Opportunistic
        }
    }

    fn market_has_inventory(&self, market_id: &MarketId) -> bool {
        self.inventory
            .positions()
            .any(|position| &position.market_id == market_id && position.quantity.abs() > 1e-9)
    }

    fn build_paired_book_signal(
        &self,
        market_id: &MarketId,
        market_context: Option<&crate::market_context::MarketContextRecord>,
        now_ms: EpochMillis,
        cfg: &UnlawfulGateConfig,
    ) -> GatePairedBookSignal {
        let mut instrument_ids = market_context
            .map(|ctx| ctx.instrument_ids.clone())
            .unwrap_or_default();
        if instrument_ids.len() < 2 {
            for managed in self.open_orders.values() {
                if &managed.intent.market_id != market_id {
                    continue;
                }
                if !instrument_ids
                    .iter()
                    .any(|existing| existing == managed.intent.instrument_id.as_str())
                {
                    instrument_ids.push(managed.intent.instrument_id.as_str().to_string());
                }
                if instrument_ids.len() >= 2 {
                    break;
                }
            }
        }
        if instrument_ids.len() < 2 {
            instrument_ids.resize(2, String::new());
        }

        let left_id = InstrumentId::from(instrument_ids[0].as_str());
        let right_id = InstrumentId::from(instrument_ids[1].as_str());
        let left_quote = self.last_quotes.get(&left_id);
        let right_quote = self.last_quotes.get(&right_id);
        let left_ask = left_quote.and_then(|quote| quote.best_ask.clone());
        let right_ask = right_quote.and_then(|quote| quote.best_ask.clone());
        let left_bid = left_quote.and_then(|quote| quote.best_bid.clone());
        let right_bid = right_quote.and_then(|quote| quote.best_bid.clone());
        let left_depth_fresh = left_quote
            .and_then(|quote| quote.depth_observed_at_ms)
            .is_some_and(|observed| now_ms.saturating_sub(observed) <= cfg.entry_book_max_age_ms);
        let right_depth_fresh = right_quote
            .and_then(|quote| quote.depth_observed_at_ms)
            .is_some_and(|observed| now_ms.saturating_sub(observed) <= cfg.entry_book_max_age_ms);
        let left_bid_levels = left_quote
            .filter(|_| left_depth_fresh)
            .map(|quote| quote.bid_levels.as_slice())
            .unwrap_or(&[]);
        let left_ask_levels = left_quote
            .filter(|_| left_depth_fresh)
            .map(|quote| quote.ask_levels.as_slice())
            .unwrap_or(&[]);
        let right_bid_levels = right_quote
            .filter(|_| right_depth_fresh)
            .map(|quote| quote.bid_levels.as_slice())
            .unwrap_or(&[]);
        let right_ask_levels = right_quote
            .filter(|_| right_depth_fresh)
            .map(|quote| quote.ask_levels.as_slice())
            .unwrap_or(&[]);

        let left_ask_price = left_ask
            .as_ref()
            .map(|level| level.price)
            .unwrap_or(f64::MAX);
        let right_ask_price = right_ask
            .as_ref()
            .map(|level| level.price)
            .unwrap_or(f64::MAX);
        let left_is_cheap = left_ask_price <= right_ask_price;

        let (cheap_id, cheap_bid, cheap_ask, cheap_obs, cheap_bid_levels, cheap_ask_levels) =
            if left_is_cheap {
                (
                    left_id.clone(),
                    left_bid.clone(),
                    left_ask.clone(),
                    left_quote.map(|quote| quote.observed_at_ms),
                    left_bid_levels,
                    left_ask_levels,
                )
            } else {
                (
                    right_id.clone(),
                    right_bid.clone(),
                    right_ask.clone(),
                    right_quote.map(|quote| quote.observed_at_ms),
                    right_bid_levels,
                    right_ask_levels,
                )
            };
        let (
            expensive_id,
            expensive_bid,
            expensive_ask,
            expensive_obs,
            expensive_bid_levels,
            expensive_ask_levels,
        ) = if left_is_cheap {
            (
                right_id,
                right_bid.clone(),
                right_ask.clone(),
                right_quote.map(|quote| quote.observed_at_ms),
                right_bid_levels,
                right_ask_levels,
            )
        } else {
            (
                left_id,
                left_bid.clone(),
                left_ask.clone(),
                left_quote.map(|quote| quote.observed_at_ms),
                left_bid_levels,
                left_ask_levels,
            )
        };

        let observed_at_ms = cheap_obs
            .into_iter()
            .chain(expensive_obs)
            .min()
            .unwrap_or(0);
        let books_fresh = observed_at_ms > 0
            && now_ms.saturating_sub(observed_at_ms) <= cfg.entry_book_max_age_ms;
        let both_sides_present =
            cheap_ask
                .as_ref()
                .zip(expensive_ask.as_ref())
                .is_some_and(|(cheap, expensive)| {
                    cheap.price > 0.0
                        && expensive.price > 0.0
                        && cheap.quantity > 0.0
                        && expensive.quantity > 0.0
                });

        let price_gap = cheap_ask
            .as_ref()
            .zip(expensive_ask.as_ref())
            .map(|(cheap, expensive)| expensive.price - cheap.price);
        let cheap_bid_depth_top3_qty = top_n_depth_qty(cheap_bid_levels, 3);
        let cheap_ask_depth_top3_qty = top_n_depth_qty(cheap_ask_levels, 3);
        let expensive_bid_depth_top3_qty = top_n_depth_qty(expensive_bid_levels, 3);
        let expensive_ask_depth_top3_qty = top_n_depth_qty(expensive_ask_levels, 3);
        let cheap_bid_notional_top3 = top_n_depth_notional(cheap_bid_levels, 3);
        let cheap_ask_notional_top3 = top_n_depth_notional(cheap_ask_levels, 3);
        let expensive_bid_notional_top3 = top_n_depth_notional(expensive_bid_levels, 3);
        let expensive_ask_notional_top3 = top_n_depth_notional(expensive_ask_levels, 3);
        let cheap_spread = cheap_bid
            .as_ref()
            .zip(cheap_ask.as_ref())
            .map(|(bid, ask)| ask.price - bid.price);
        let expensive_spread = expensive_bid
            .as_ref()
            .zip(expensive_ask.as_ref())
            .map(|(bid, ask)| ask.price - bid.price);

        GatePairedBookSignal {
            cheap_instrument_id: cheap_id.as_str().to_string(),
            expensive_instrument_id: expensive_id.as_str().to_string(),
            cheap_bid,
            cheap_ask,
            expensive_bid,
            expensive_ask,
            price_gap,
            observed_at_ms,
            books_fresh,
            both_sides_present,
            cheap_spread,
            expensive_spread,
            cheap_bid_depth_top3_qty,
            cheap_ask_depth_top3_qty,
            expensive_bid_depth_top3_qty,
            expensive_ask_depth_top3_qty,
            cheap_bid_notional_top3,
            cheap_ask_notional_top3,
            expensive_bid_notional_top3,
            expensive_ask_notional_top3,
            cheap_depth_imbalance_top3: depth_imbalance(
                cheap_bid_depth_top3_qty,
                cheap_ask_depth_top3_qty,
            ),
            expensive_depth_imbalance_top3: depth_imbalance(
                expensive_bid_depth_top3_qty,
                expensive_ask_depth_top3_qty,
            ),
        }
    }

    fn aggregate_market_activity(&self, signal: &GatePairedBookSignal) -> GateMarketActivitySignal {
        let cheap = self
            .market_activity
            .get(&InstrumentId::from(signal.cheap_instrument_id.as_str()))
            .cloned()
            .unwrap_or_default();
        let expensive = self
            .market_activity
            .get(&InstrumentId::from(signal.expensive_instrument_id.as_str()))
            .cloned()
            .unwrap_or_default();
        GateMarketActivitySignal {
            last_trade_event_count_10s: cheap
                .last_trade_event_count_10s
                .saturating_add(expensive.last_trade_event_count_10s),
            last_trade_event_count_30s: cheap
                .last_trade_event_count_30s
                .saturating_add(expensive.last_trade_event_count_30s),
            last_trade_event_count_60s: cheap
                .last_trade_event_count_60s
                .saturating_add(expensive.last_trade_event_count_60s),
            last_trade_event_age_ms: match (
                cheap.last_trade_event_age_ms,
                expensive.last_trade_event_age_ms,
            ) {
                (Some(left), Some(right)) => Some(left.min(right)),
                (Some(left), None) => Some(left),
                (None, Some(right)) => Some(right),
                (None, None) => None,
            },
        }
    }

    fn map_signal_snapshot(&self, signal: GateSignalSnapshot) -> StrategyUnlawfulSignalSnapshot {
        StrategyUnlawfulSignalSnapshot {
            session_bucket: Self::map_session_bucket(signal.session_bucket),
            mode: Self::map_execution_mode(signal.mode),
            gate_reasons: signal.gate_reasons,
            btc: StrategyBtcRegimeSnapshot {
                last_price: signal.btc.last_price,
                realized_vol_5m_bps: signal.btc.realized_vol_5m_bps,
                realized_vol_15m_bps: signal.btc.realized_vol_15m_bps,
                trade_count_5m: signal.btc.trade_count_5m,
                trade_count_15m: signal.btc.trade_count_15m,
                return_30s_bps: signal.btc.return_30s_bps,
                return_60s_bps: signal.btc.return_60s_bps,
                observed_at_ms: signal.btc.observed_at_ms,
            },
            book: StrategyPairedBookSignal {
                cheap_instrument_id: InstrumentId::from(signal.book.cheap_instrument_id),
                expensive_instrument_id: InstrumentId::from(signal.book.expensive_instrument_id),
                cheap_bid: signal.book.cheap_bid,
                cheap_ask: signal.book.cheap_ask,
                expensive_bid: signal.book.expensive_bid,
                expensive_ask: signal.book.expensive_ask,
                price_gap: signal.book.price_gap,
                observed_at_ms: signal.book.observed_at_ms,
                books_fresh: signal.book.books_fresh,
                both_sides_present: signal.book.both_sides_present,
                cheap_spread: signal.book.cheap_spread,
                expensive_spread: signal.book.expensive_spread,
                cheap_bid_depth_top3_qty: signal.book.cheap_bid_depth_top3_qty,
                cheap_ask_depth_top3_qty: signal.book.cheap_ask_depth_top3_qty,
                expensive_bid_depth_top3_qty: signal.book.expensive_bid_depth_top3_qty,
                expensive_ask_depth_top3_qty: signal.book.expensive_ask_depth_top3_qty,
                cheap_bid_notional_top3: signal.book.cheap_bid_notional_top3,
                cheap_ask_notional_top3: signal.book.cheap_ask_notional_top3,
                expensive_bid_notional_top3: signal.book.expensive_bid_notional_top3,
                expensive_ask_notional_top3: signal.book.expensive_ask_notional_top3,
                cheap_depth_imbalance_top3: signal.book.cheap_depth_imbalance_top3,
                expensive_depth_imbalance_top3: signal.book.expensive_depth_imbalance_top3,
            },
            activity: StrategyMarketActivitySignal {
                last_trade_event_count_10s: signal.activity.last_trade_event_count_10s,
                last_trade_event_count_30s: signal.activity.last_trade_event_count_30s,
                last_trade_event_count_60s: signal.activity.last_trade_event_count_60s,
                last_trade_event_age_ms: signal.activity.last_trade_event_age_ms,
            },
            first_fill_ms: signal.first_fill_ms,
            first_merge_ms: signal.first_merge_ms,
            elapsed_s: signal.elapsed_s,
            time_remaining_s: signal.time_remaining_s,
            clip_scale: signal.clip_scale,
        }
    }

    fn map_session_bucket(bucket: GateSessionBucket) -> StrategySessionBucket {
        match bucket {
            GateSessionBucket::Preferred => StrategySessionBucket::Preferred,
            GateSessionBucket::Neutral => StrategySessionBucket::Neutral,
            GateSessionBucket::Opportunistic => StrategySessionBucket::Opportunistic,
        }
    }

    fn map_execution_mode(mode: GateExecutionMode) -> StrategyExecutionMode {
        match mode {
            GateExecutionMode::Standby => StrategyExecutionMode::Standby,
            GateExecutionMode::Entry => StrategyExecutionMode::Entry,
            GateExecutionMode::Manage => StrategyExecutionMode::Manage,
            GateExecutionMode::Cleanup => StrategyExecutionMode::Cleanup,
            GateExecutionMode::Flatten => StrategyExecutionMode::Flatten,
        }
    }

    fn enforce_unlawful_mode(
        &mut self,
        market_id: &MarketId,
        signal: &StrategyUnlawfulSignalSnapshot,
        now_ms: EpochMillis,
    ) -> RuntimeOutcome {
        let previous_mode = self
            .unlawful_mode_by_market
            .insert(market_id.clone(), signal.mode);
        let entering_cleanup = matches!(
            signal.mode,
            StrategyExecutionMode::Cleanup | StrategyExecutionMode::Flatten
        ) && !matches!(
            previous_mode,
            Some(StrategyExecutionMode::Cleanup | StrategyExecutionMode::Flatten)
        );
        if !entering_cleanup {
            return RuntimeOutcome::default();
        }

        let mut outcome = RuntimeOutcome::default();
        let to_cancel = self
            .open_orders
            .values()
            .filter(|managed| &managed.intent.market_id == market_id)
            .filter(|managed| !managed.intent.reduce_only)
            .filter(|managed| {
                matches!(
                    managed.status,
                    ManagedOrderStatus::PendingSubmit
                        | ManagedOrderStatus::Submitted
                        | ManagedOrderStatus::Working
                )
            })
            .map(|managed| managed.intent.client_order_id.clone())
            .collect::<Vec<_>>();
        for client_order_id in to_cancel {
            outcome.extend(self.request_cancel(
                &client_order_id,
                format!("unlawful mode transitioned to {:?}", signal.mode),
                now_ms,
            ));
        }
        outcome
    }

    fn maybe_clear_market_timing(&mut self, market_id: &MarketId) {
        if self.market_has_inventory(market_id) {
            return;
        }
        let has_open_orders = self
            .open_orders
            .values()
            .any(|managed| &managed.intent.market_id == market_id && !managed.status.is_terminal());
        if has_open_orders {
            return;
        }
        self.first_fill_by_market.remove(market_id);
        self.first_merge_by_market.remove(market_id);
        self.pending_merge_by_market.remove(market_id);
        self.unlawful_mode_by_market.remove(market_id);
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
                outcome.push_event(
                    self.event_log.push(
                        EventRecord::new(
                            EventCategory::Runtime,
                            now_ms,
                            format!("order status {:?} -> {:?}: {}", old_status, status, reason),
                        )
                        .with_client_order(client_order_id.clone())
                        .with_market(managed.intent.market_id.clone())
                        .with_instrument(managed.intent.instrument_id.clone()),
                    ),
                );
                self.record_status_persist(client_order_id, status, now_ms);
            }
            None => {
                warn!(
                    run_id = %self.run_id,
                    client_order_id = %client_order_id,
                    status = ?status,
                    "status transition requested for unknown active order; ignoring local late event"
                );
            }
        };
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

    pub fn quarantine_stale_needs_reconcile_orders(
        &mut self,
        now_ms: EpochMillis,
        min_age_ms: u64,
    ) -> RuntimeOutcome {
        let mut outcome = RuntimeOutcome::default();
        let to_quarantine = self
            .open_orders
            .values()
            .filter(|managed| managed.status == ManagedOrderStatus::NeedsReconcile)
            .filter(|managed| now_ms.saturating_sub(managed.last_update_ms) >= min_age_ms)
            .map(|managed| managed.intent.client_order_id.clone())
            .collect::<Vec<_>>();

        for client_order_id in to_quarantine {
            outcome.extend(self.set_order_status(
                &client_order_id,
                ManagedOrderStatus::Quarantined,
                now_ms,
                "stale needs-reconcile order moved to execution DLQ",
            ));
            if let Some(managed) = self.open_orders.remove(&client_order_id) {
                outcome.push_event(
                    self.event_log.push(
                        EventRecord::new(
                            EventCategory::Runtime,
                            now_ms,
                            "quarantined stale needs-reconcile order and removed from active memory",
                        )
                        .with_market(managed.intent.market_id)
                        .with_instrument(managed.intent.instrument_id)
                        .with_client_order(client_order_id),
                    ),
                );
            }
        }
        outcome
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
            "Quarantined" => ManagedOrderStatus::Quarantined,
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
                reason: record
                    .reason
                    .unwrap_or_else(|| "checkpoint recovery".to_string()),
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
    use super::{ManagedOrderStatus, Runtime, RuntimeConfig};
    use crate::inventory::VenuePositionSnapshot;
    use crate::market_context::{MarketContextRecord, MarketContextStore};
    use crate::risk::RiskLimits;
    use crate::runtime::order_store::{OrderRecord, OrderStore, SqliteOrderStore};
    use crate::signals::unlawful_gate::UnlawfulGateConfig;
    use crate::strategy::{NoopStrategy, Strategy, StrategyContext, StrategyDecision};
    use crate::types::{
        BookLevel, ClientOrderId, CloseMethod, FillLiquidity, FillReport, InstrumentId, MarketId,
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
                bid_levels: vec![BookLevel::new(0.39, 100.0)],
                ask_levels: vec![BookLevel::new(0.40, 100.0)],
                depth_observed_at_ms: Some(2),
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
        assert_eq!(
            runtime
                .inventory()
                .position_qty(&InstrumentId::from("token-1")),
            10.0
        );
        assert!((runtime.inventory().free_cash_usd() - 95.9).abs() < 1e-9);
    }

    #[test]
    fn late_partial_fill_after_cancel_request_keeps_cancel_pending() {
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
        let snapshot = MarketSnapshot {
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new(0.39, 100.0)),
                best_ask: Some(BookLevel::new(0.40, 100.0)),
                bid_levels: vec![BookLevel::new(0.39, 100.0)],
                ask_levels: vec![BookLevel::new(0.40, 100.0)],
                depth_observed_at_ms: Some(2),
                last_trade_price: Some(0.40),
                observed_at_ms: 2,
            },
        };
        runtime.on_market_snapshot(snapshot).expect("quote");
        runtime.request_cancel(&ClientOrderId::from("client-1"), "test cancel", 3);

        runtime
            .on_fill(FillReport {
                order_id: None,
                client_order_id: Some(ClientOrderId::from("client-1")),
                market_id: MarketId::from("market-1"),
                instrument_id: InstrumentId::from("token-1"),
                side: TradeSide::Buy,
                price: 0.40,
                quantity: 4.0,
                fee_usd: 0.04,
                liquidity: FillLiquidity::Taker,
                close_method: None,
                observed_at_ms: 4,
            })
            .expect("fill");

        let open_order = runtime.open_orders().next().expect("remaining order");
        assert_eq!(open_order.status, ManagedOrderStatus::CancelRequested);
        assert!((open_order.cumulative_filled_qty - 4.0).abs() < 1e-9);
        assert!((open_order.remaining_qty() - 6.0).abs() < 1e-9);
        assert_eq!(
            runtime
                .inventory()
                .position_qty(&InstrumentId::from("token-1")),
            4.0
        );
    }

    #[test]
    fn venue_position_reconciliation_creates_runtime_inventory_without_fill() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Running,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            NoopStrategy,
            MarketContextStore::empty(),
        );

        let report = runtime
            .reconcile_venue_positions(
                &[VenuePositionSnapshot {
                    market_id: MarketId::from("market-mm"),
                    condition_id: None,
                    instrument_id: InstrumentId::from("down"),
                    quantity: 6.5,
                    average_cost_usd: 0.80,
                    mark_price: Some(1.0),
                    observed_at_ms: 10,
                }],
                11,
            )
            .expect("runtime venue reconciliation");

        assert_eq!(
            runtime
                .inventory()
                .position_qty(&InstrumentId::from("down")),
            6.5
        );
        assert_eq!(report.deltas.len(), 1);
        assert_eq!(runtime.stranded_inventory().len(), 1);
        assert_eq!(runtime.open_orders().count(), 0);
    }

    #[test]
    fn redeem_close_event_does_not_apply_as_trade_fill() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Running,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            NoopStrategy,
            MarketContextStore::empty(),
        );

        runtime
            .on_fill(FillReport {
                order_id: None,
                client_order_id: None,
                market_id: MarketId::from("market-mm"),
                instrument_id: InstrumentId::from("up"),
                side: TradeSide::Buy,
                price: 1.0,
                quantity: 6.5,
                fee_usd: 0.0,
                liquidity: FillLiquidity::Unknown,
                close_method: Some(CloseMethod::Redeem),
                observed_at_ms: 10,
            })
            .expect("redeem event");

        assert_eq!(
            runtime.inventory().position_qty(&InstrumentId::from("up")),
            0.0
        );
        assert!((runtime.inventory().free_cash_usd() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn paired_inventory_fill_emits_explicit_merge_command() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Running,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            NoopStrategy,
            MarketContextStore::empty(),
        );

        runtime
            .on_fill(FillReport {
                order_id: None,
                client_order_id: None,
                market_id: MarketId::from("market-mm"),
                instrument_id: InstrumentId::from("up"),
                side: TradeSide::Buy,
                price: 0.20,
                quantity: 6.5,
                fee_usd: 0.0,
                liquidity: FillLiquidity::Maker,
                close_method: None,
                observed_at_ms: 10,
            })
            .expect("first leg");
        let outcome = runtime
            .on_fill(FillReport {
                order_id: None,
                client_order_id: None,
                market_id: MarketId::from("market-mm"),
                instrument_id: InstrumentId::from("down"),
                side: TradeSide::Buy,
                price: 0.70,
                quantity: 6.5,
                fee_usd: 0.0,
                liquidity: FillLiquidity::Maker,
                close_method: None,
                observed_at_ms: 11,
            })
            .expect("second leg");

        let merge = outcome
            .commands
            .iter()
            .find_map(|command| match command {
                RuntimeCommand::Merge(intent) => Some(intent),
                _ => None,
            })
            .expect("merge command");
        assert_eq!(merge.market_id, MarketId::from("market-mm"));
        assert!((merge.quantity - 6.5).abs() < 1e-9);
        assert!((merge.expected_cash_usd - 6.5).abs() < 1e-9);
        assert!((merge.expected_cost_usd - 5.85).abs() < 1e-9);
    }

    #[test]
    fn reduce_only_sell_cleanup_is_suppressed_when_merge_is_pending() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Running,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            NoopStrategy,
            MarketContextStore::empty(),
        );

        runtime
            .on_fill(FillReport {
                order_id: None,
                client_order_id: None,
                market_id: MarketId::from("market-mm"),
                instrument_id: InstrumentId::from("up"),
                side: TradeSide::Buy,
                price: 0.20,
                quantity: 6.5,
                fee_usd: 0.0,
                liquidity: FillLiquidity::Maker,
                close_method: None,
                observed_at_ms: 10,
            })
            .expect("first leg");
        runtime
            .on_fill(FillReport {
                order_id: None,
                client_order_id: None,
                market_id: MarketId::from("market-mm"),
                instrument_id: InstrumentId::from("down"),
                side: TradeSide::Buy,
                price: 0.70,
                quantity: 6.5,
                fee_usd: 0.0,
                liquidity: FillLiquidity::Maker,
                close_method: None,
                observed_at_ms: 11,
            })
            .expect("second leg");

        let outcome = runtime.accept_intent(
            OrderIntent {
                client_order_id: ClientOrderId::from("cleanup-sell-1"),
                market_id: MarketId::from("market-mm"),
                instrument_id: InstrumentId::from("up"),
                side: TradeSide::Sell,
                limit_price: 0.19,
                quantity: 1.0,
                reduce_only: true,
                reason: "fallback cleanup".to_string(),
                quote_level_tag: Some("fallback-cleanup".to_string()),
                created_at_ms: 12,
            },
            12,
        );

        assert!(outcome.commands.is_empty());
        assert!(
            runtime
                .open_orders()
                .all(|managed| managed.intent.client_order_id
                    != ClientOrderId::from("cleanup-sell-1"))
        );
    }

    #[test]
    fn merge_close_event_applies_pending_merge_lifecycle() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Running,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            NoopStrategy,
            MarketContextStore::empty(),
        );

        runtime
            .on_fill(FillReport {
                order_id: None,
                client_order_id: None,
                market_id: MarketId::from("market-mm"),
                instrument_id: InstrumentId::from("up"),
                side: TradeSide::Buy,
                price: 0.20,
                quantity: 6.5,
                fee_usd: 0.0,
                liquidity: FillLiquidity::Maker,
                close_method: None,
                observed_at_ms: 10,
            })
            .expect("first leg");
        runtime
            .on_fill(FillReport {
                order_id: None,
                client_order_id: None,
                market_id: MarketId::from("market-mm"),
                instrument_id: InstrumentId::from("down"),
                side: TradeSide::Buy,
                price: 0.70,
                quantity: 6.5,
                fee_usd: 0.0,
                liquidity: FillLiquidity::Maker,
                close_method: None,
                observed_at_ms: 11,
            })
            .expect("second leg");

        runtime
            .on_fill(FillReport {
                order_id: None,
                client_order_id: Some(ClientOrderId::from("merge:market-mm:6.50000000:11")),
                market_id: MarketId::from("market-mm"),
                instrument_id: InstrumentId::from("up"),
                side: TradeSide::Buy,
                price: 1.0,
                quantity: 6.5,
                fee_usd: 0.0,
                liquidity: FillLiquidity::Unknown,
                close_method: Some(CloseMethod::Merge),
                observed_at_ms: 12,
            })
            .expect("merge event");

        assert_eq!(
            runtime.inventory().position_qty(&InstrumentId::from("up")),
            0.0
        );
        assert_eq!(
            runtime
                .inventory()
                .position_qty(&InstrumentId::from("down")),
            0.0
        );
        assert!((runtime.inventory().free_cash_usd() - 100.65).abs() < 1e-9);
    }

    #[test]
    fn merge_close_event_without_pair_does_not_apply_as_trade_fill() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Running,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            NoopStrategy,
            MarketContextStore::empty(),
        );

        runtime
            .on_fill(FillReport {
                order_id: None,
                client_order_id: None,
                market_id: MarketId::from("market-mm"),
                instrument_id: InstrumentId::from("up"),
                side: TradeSide::Buy,
                price: 1.0,
                quantity: 6.5,
                fee_usd: 0.0,
                liquidity: FillLiquidity::Unknown,
                close_method: Some(CloseMethod::Merge),
                observed_at_ms: 10,
            })
            .expect("merge event");

        assert_eq!(
            runtime.inventory().position_qty(&InstrumentId::from("up")),
            0.0
        );
        assert!((runtime.inventory().free_cash_usd() - 100.0).abs() < 1e-9);
    }

    fn btc_mm_intent(market_id: &str, instrument_id: &str, level: &str, price: f64) -> OrderIntent {
        OrderIntent {
            client_order_id: ClientOrderId::from(format!(
                "btc-5m-mm:{market_id}:{instrument_id}:b:n:{level}:{price:.8}:6.50000000"
            )),
            market_id: MarketId::from(market_id),
            instrument_id: InstrumentId::from(instrument_id),
            side: TradeSide::Buy,
            limit_price: price,
            quantity: 6.5,
            reduce_only: false,
            reason: format!("btc-5m-mm {level}"),
            quote_level_tag: Some(level.to_string()),
            created_at_ms: 1,
        }
    }

    #[test]
    fn btc_mm_keeps_existing_opposite_buy_when_one_leg_fills() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Running,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            NoopStrategy,
            MarketContextStore::empty(),
        );

        let hedge_order = btc_mm_intent("market-mm", "down", "mm-paired-bid", 0.44);
        let hedge_client_order_id = hedge_order.client_order_id.clone();
        let accepted = runtime.accept_intent(hedge_order, 1);
        assert_eq!(accepted.commands.len(), 1);

        runtime
            .on_fill(FillReport {
                order_id: None,
                client_order_id: None,
                market_id: MarketId::from("market-mm"),
                instrument_id: InstrumentId::from("up"),
                side: TradeSide::Buy,
                price: 0.50,
                quantity: 6.5,
                fee_usd: 0.0,
                liquidity: FillLiquidity::Maker,
                close_method: None,
                observed_at_ms: 2,
            })
            .expect("one leg fill");

        let cancel = runtime.request_cancel(&hedge_client_order_id, "no longer desired", 3);
        assert!(cancel.commands.is_empty());
        let remaining = runtime
            .open_orders()
            .find(|managed| managed.intent.client_order_id == hedge_client_order_id)
            .expect("hedge order kept");
        assert_eq!(remaining.status, ManagedOrderStatus::PendingSubmit);
    }

    #[test]
    fn btc_mm_rejects_duplicate_active_buy_for_same_instrument() {
        let mut runtime = Runtime::new(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Running,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            NoopStrategy,
            MarketContextStore::empty(),
        );

        let first =
            runtime.accept_intent(btc_mm_intent("market-mm", "down", "mm-paired-bid", 0.44), 1);
        assert_eq!(first.commands.len(), 1);

        let duplicate = runtime.accept_intent(
            btc_mm_intent("market-mm", "down", "mm-hedge-rescue", 0.48),
            2,
        );
        assert!(duplicate.commands.is_empty());
        assert_eq!(runtime.open_orders().count(), 1);
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

        let book =
            crate::book::BookState::from_top_of_book("token-up", 0.41, 12.0, 0.44, 7.0, 0.43, 25);

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
        assert_eq!(quote.depth_observed_at_ms, Some(25));
        assert_eq!(quote.bid_levels.len(), 1);
        assert_eq!(quote.ask_levels.len(), 1);
        assert_eq!(quote.bid_levels[0].quantity, 12.0);
        assert_eq!(quote.ask_levels[0].quantity, 7.0);
    }

    #[test]
    fn paired_book_signal_derives_top3_depth_features() {
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

        let market_id = MarketId::from("market-1");
        let cheap_id = InstrumentId::from("cheap-token");
        let expensive_id = InstrumentId::from("expensive-token");

        let mut cheap_book = crate::book::BookState::from_top_of_book(
            cheap_id.as_str(),
            0.37,
            100.0,
            0.39,
            200.0,
            0.38,
            10,
        );
        cheap_book.bids = vec![
            crate::book::Level {
                price: 0.37,
                size: 100.0,
            },
            crate::book::Level {
                price: 0.36,
                size: 50.0,
            },
            crate::book::Level {
                price: 0.35,
                size: 25.0,
            },
            crate::book::Level {
                price: 0.34,
                size: 10.0,
            },
        ];
        cheap_book.asks = vec![
            crate::book::Level {
                price: 0.39,
                size: 200.0,
            },
            crate::book::Level {
                price: 0.40,
                size: 100.0,
            },
            crate::book::Level {
                price: 0.41,
                size: 50.0,
            },
        ];

        let mut expensive_book = crate::book::BookState::from_top_of_book(
            expensive_id.as_str(),
            0.58,
            10.0,
            0.60,
            40.0,
            0.59,
            10,
        );
        expensive_book.bids = vec![
            crate::book::Level {
                price: 0.58,
                size: 10.0,
            },
            crate::book::Level {
                price: 0.57,
                size: 20.0,
            },
            crate::book::Level {
                price: 0.56,
                size: 30.0,
            },
        ];
        expensive_book.asks = vec![
            crate::book::Level {
                price: 0.60,
                size: 40.0,
            },
            crate::book::Level {
                price: 0.61,
                size: 50.0,
            },
            crate::book::Level {
                price: 0.62,
                size: 60.0,
            },
        ];

        runtime
            .on_book_state(market_id.clone(), cheap_id.clone(), &cheap_book)
            .expect("cheap book");
        runtime
            .on_book_state(market_id.clone(), expensive_id.clone(), &expensive_book)
            .expect("expensive book");

        let market_context = MarketContextRecord {
            market_id: market_id.as_str().to_string(),
            instrument_ids: vec![
                cheap_id.as_str().to_string(),
                expensive_id.as_str().to_string(),
            ],
            ..MarketContextRecord::default()
        };
        let signal = runtime.build_paired_book_signal(
            &market_id,
            Some(&market_context),
            100,
            &UnlawfulGateConfig::default(),
        );

        assert!((signal.cheap_spread.unwrap() - 0.02).abs() < 1e-9);
        assert!((signal.expensive_spread.unwrap() - 0.02).abs() < 1e-9);
        assert_eq!(signal.cheap_bid_depth_top3_qty, Some(175.0));
        assert_eq!(signal.cheap_ask_depth_top3_qty, Some(350.0));
        assert_eq!(signal.expensive_bid_depth_top3_qty, Some(60.0));
        assert_eq!(signal.expensive_ask_depth_top3_qty, Some(150.0));
        assert!((signal.cheap_bid_notional_top3.unwrap() - 63.75).abs() < 1e-9);
        assert!((signal.cheap_ask_notional_top3.unwrap() - 138.5).abs() < 1e-9);
        assert!((signal.cheap_depth_imbalance_top3.unwrap() + (1.0 / 3.0)).abs() < 1e-9);
        assert!((signal.expensive_depth_imbalance_top3.unwrap() + (3.0 / 7.0)).abs() < 1e-9);
    }

    #[test]
    fn paired_book_signal_drops_stale_depth_features() {
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

        let market_id = MarketId::from("market-1");
        let cheap_id = InstrumentId::from("cheap-token");
        let expensive_id = InstrumentId::from("expensive-token");
        runtime.last_quotes.insert(
            cheap_id.clone(),
            QuoteSnapshot {
                best_bid: Some(BookLevel::new(0.37, 100.0)),
                best_ask: Some(BookLevel::new(0.39, 100.0)),
                bid_levels: vec![BookLevel::new(0.37, 100.0)],
                ask_levels: vec![BookLevel::new(0.39, 100.0)],
                depth_observed_at_ms: Some(10),
                last_trade_price: Some(0.38),
                observed_at_ms: 2_000,
            },
        );
        runtime.last_quotes.insert(
            expensive_id.clone(),
            QuoteSnapshot {
                best_bid: Some(BookLevel::new(0.58, 100.0)),
                best_ask: Some(BookLevel::new(0.60, 100.0)),
                bid_levels: vec![BookLevel::new(0.58, 100.0)],
                ask_levels: vec![BookLevel::new(0.60, 100.0)],
                depth_observed_at_ms: Some(10),
                last_trade_price: Some(0.59),
                observed_at_ms: 2_000,
            },
        );

        let market_context = MarketContextRecord {
            market_id: market_id.as_str().to_string(),
            instrument_ids: vec![
                cheap_id.as_str().to_string(),
                expensive_id.as_str().to_string(),
            ],
            ..MarketContextRecord::default()
        };
        let mut cfg = UnlawfulGateConfig::default();
        cfg.entry_book_max_age_ms = 1_000;
        let signal =
            runtime.build_paired_book_signal(&market_id, Some(&market_context), 2_000, &cfg);

        assert_eq!(signal.cheap_ask_depth_top3_qty, None);
        assert_eq!(signal.expensive_ask_depth_top3_qty, None);
        assert_eq!(signal.cheap_ask_notional_top3, None);
        assert_eq!(signal.expensive_ask_notional_top3, None);
        assert_eq!(signal.cheap_depth_imbalance_top3, None);
        assert_eq!(signal.expensive_depth_imbalance_top3, None);
        assert!((signal.cheap_spread.unwrap() - 0.02).abs() < 1e-9);
        assert!((signal.expensive_spread.unwrap() - 0.02).abs() < 1e-9);
    }

    #[test]
    fn recover_from_store_reconstructs_orders_and_marks_uncertain_submits() {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("polymarket-exec-order-store-startup-{ts}.sqlite"));
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
                quote_level_tag: None,
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
        assert_eq!(
            recovered[0].status,
            crate::runtime::types::ManagedOrderStatus::NeedsReconcile
        );
        let recent_messages = runtime
            .event_log()
            .recent(8)
            .into_iter()
            .map(|event| event.message)
            .collect::<Vec<_>>();
        assert!(
            recent_messages
                .iter()
                .any(|message| message.contains("fail-closed stale submit state PendingSubmit")),
            "missing fail-closed pending-submit event in {recent_messages:?}"
        );
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

    #[test]
    fn reconcile_open_orders_marks_stale_cancel_requested_orders() {
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

        let snapshot = MarketSnapshot {
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new(0.39, 100.0)),
                best_ask: Some(BookLevel::new(0.40, 100.0)),
                bid_levels: vec![BookLevel::new(0.39, 100.0)],
                ask_levels: vec![BookLevel::new(0.40, 100.0)],
                depth_observed_at_ms: Some(2),
                last_trade_price: Some(0.40),
                observed_at_ms: 2,
            },
        };
        runtime.on_market_snapshot(snapshot).expect("quote");
        runtime.request_cancel(&ClientOrderId::from("client-1"), "test cancel", 3);

        let outcome = runtime.reconcile_open_orders(2_000, 500);
        assert!(!outcome.event_seqs.is_empty());
        let order = runtime
            .open_order_snapshots()
            .into_iter()
            .find(|managed| managed.intent.client_order_id == ClientOrderId::from("client-1"))
            .expect("managed order");
        assert_eq!(
            order.status,
            crate::runtime::types::ManagedOrderStatus::NeedsReconcile
        );
        let recent_messages = runtime
            .event_log()
            .recent(8)
            .into_iter()
            .map(|event| event.message)
            .collect::<Vec<_>>();
        assert!(
            recent_messages
                .iter()
                .any(|message| message.contains("fail-closed stale cancel state CancelRequested")),
            "missing fail-closed cancel event in {recent_messages:?}"
        );
    }

    #[test]
    fn reconcile_open_orders_marks_stale_submit_states_with_fail_closed_events() {
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

        let snapshot = MarketSnapshot {
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new(0.39, 100.0)),
                best_ask: Some(BookLevel::new(0.40, 100.0)),
                bid_levels: vec![BookLevel::new(0.39, 100.0)],
                ask_levels: vec![BookLevel::new(0.40, 100.0)],
                depth_observed_at_ms: Some(2),
                last_trade_price: Some(0.40),
                observed_at_ms: 2,
            },
        };
        runtime.on_market_snapshot(snapshot).expect("quote");
        runtime.set_order_status(
            &ClientOrderId::from("client-1"),
            ManagedOrderStatus::Submitted,
            3,
            "adapter accepted submit but user stream has not opened it",
        );

        let outcome = runtime.reconcile_open_orders(2_000, 500);
        assert!(!outcome.event_seqs.is_empty());
        let order = runtime
            .open_order_snapshots()
            .into_iter()
            .find(|managed| managed.intent.client_order_id == ClientOrderId::from("client-1"))
            .expect("managed order");
        assert_eq!(order.status, ManagedOrderStatus::NeedsReconcile);
        let recent_messages = runtime
            .event_log()
            .recent(8)
            .into_iter()
            .map(|event| event.message)
            .collect::<Vec<_>>();
        assert!(
            recent_messages
                .iter()
                .any(|message| message.contains("fail-closed stale submit state Submitted")),
            "missing fail-closed submit event in {recent_messages:?}"
        );
    }

    #[test]
    fn degraded_runtime_does_not_restart_or_cancel_needs_reconcile_orders() {
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

        let snapshot = MarketSnapshot {
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new(0.39, 100.0)),
                best_ask: Some(BookLevel::new(0.40, 100.0)),
                bid_levels: vec![BookLevel::new(0.39, 100.0)],
                ask_levels: vec![BookLevel::new(0.40, 100.0)],
                depth_observed_at_ms: Some(2),
                last_trade_price: Some(0.40),
                observed_at_ms: 2,
            },
        };
        runtime.on_market_snapshot(snapshot).expect("quote");
        runtime.mark_order_needs_reconcile(
            &ClientOrderId::from("client-1"),
            3,
            "uncertain live state",
        );

        let degraded =
            runtime.degrade_and_cancel_all(4, "startup has orders requiring reconciliation");
        assert_eq!(runtime.status(), RuntimeStatus::Degraded);
        assert!(runtime.has_needs_reconcile_orders());
        assert!(
            degraded.commands.is_empty(),
            "NeedsReconcile orders must not produce venue cancel commands"
        );

        let restarted = runtime.start(5);
        assert_eq!(runtime.status(), RuntimeStatus::Degraded);
        assert!(restarted.commands.is_empty());
        let recent_messages = runtime
            .event_log()
            .recent(8)
            .into_iter()
            .map(|event| event.message)
            .collect::<Vec<_>>();
        assert!(
            recent_messages.iter().any(
                |message| message.contains("runtime start skipped because runtime is degraded")
            ),
            "missing degraded-start guard event in {recent_messages:?}"
        );
    }
}
