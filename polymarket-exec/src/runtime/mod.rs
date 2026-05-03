//! Core runtime state machine: signal ingestion, strategy evaluation, and order lifecycle.

mod attribution;
mod audit;
mod btc_signals;
mod checkpoint;
mod dashboard;
mod execution_policy;
mod live_auth;
mod live_health;
mod market_universe;
pub mod order_store;
mod paper_fill;
pub mod reconcile;
pub mod runner;
pub mod types;

use std::collections::{HashMap, HashSet};
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
use crate::runtime::btc_signals::BtcSignalStore;
use crate::runtime::order_store::{OrderRecord, OrderStore};
pub use crate::runtime::types::{
    ManagedOrder, ManagedOrderStatus, RuntimeConfig, RuntimeError, RuntimeOutcome,
};
use crate::signals::{
    BtcRegimeSnapshot as GateBtcRegimeSnapshot, MarketActivitySignal as GateMarketActivitySignal,
};
use crate::strategy::{
    Strategy, StrategyContext, StrategyDecision, StrategyDecisionSuppressionKind, VenueMarketRules,
};
use crate::types::{
    ClientOrderId, CloseMethod, EpochMillis, FillLiquidity, FillReport, InstrumentId, MarketId,
    MarketSnapshot, MergeIntent, OrderId, OrderIntent, TradeSide,
};
use crate::types::{RuntimeCommand, RuntimeStatus};
pub use checkpoint::{RuntimeCheckpoint, RuntimeCheckpointOrder};
use tracing::{info, warn};

const BLOCKED_MERGE_RETRY_AFTER_MS: u64 = 15_000;
const ACCOUNTING_QTY_EPSILON: f64 = 1e-9;

pub struct Runtime<S: Strategy> {
    strategy: S,
    inventory: InventoryState,
    starting_cash_usd: f64,
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
    min_merge_notional_usd: f64,
    btc_signals: BtcSignalStore,
    market_activity: HashMap<InstrumentId, GateMarketActivitySignal>,
    first_fill_by_market: HashMap<MarketId, EpochMillis>,
    first_merge_by_market: HashMap<MarketId, EpochMillis>,
    pending_merge_by_market: HashMap<MarketId, MergeIntent>,
    accepted_merge_by_market: HashMap<MarketId, AcceptedMerge>,
    blocked_merge_by_market: HashMap<MarketId, BlockedMerge>,
    condition_id_by_market: HashMap<MarketId, String>,
    venue_market_rules: HashMap<MarketId, VenueMarketRules>,
    markets_with_unresolved_drift: HashSet<MarketId>,
    /// Timestamp of first detection of local-flat/venue-nonflat drift per
    /// market. Used to hold off engaging the drift block for transient
    /// post-fill state lag (WS fill event lands ~50ms before local
    /// position state catches up). Cleared when drift resolves.
    markets_with_drift_first_seen_ms: HashMap<MarketId, EpochMillis>,
    require_initial_reconcile_before_entry: bool,
    /// True once `reconcile_venue_positions` has run for the first time.
    /// Used to distinguish "venue has positions we don't know about because
    /// we just started up and haven't synced yet" (install silently) from
    /// "venue has positions we don't know about mid-session" (drift incident,
    /// engage protective block).
    initial_reconcile_complete: bool,
    order_store: Option<Box<dyn OrderStore>>,
}

#[derive(Clone, Debug, PartialEq)]
struct MergeSignature {
    condition_id: Option<String>,
    yes_instrument_id: InstrumentId,
    no_instrument_id: InstrumentId,
    quantity_units: u64,
}

impl MergeSignature {
    fn from_intent(intent: &MergeIntent) -> Self {
        Self {
            condition_id: intent.condition_id.clone(),
            yes_instrument_id: intent.yes_instrument_id.clone(),
            no_instrument_id: intent.no_instrument_id.clone(),
            quantity_units: (intent.quantity * 1_000_000.0).round().max(0.0) as u64,
        }
    }
}

#[derive(Clone, Debug)]
struct BlockedMerge {
    signature: MergeSignature,
    reason: String,
    blocked_at_ms: EpochMillis,
}

#[derive(Clone, Debug)]
struct AcceptedMerge {
    signature: MergeSignature,
    accepted_at_ms: EpochMillis,
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
            starting_cash_usd: config.starting_cash_usd,
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
            min_merge_notional_usd: config.min_merge_notional_usd.max(0.0),
            btc_signals: BtcSignalStore::default(),
            market_activity: HashMap::new(),
            first_fill_by_market: HashMap::new(),
            first_merge_by_market: HashMap::new(),
            pending_merge_by_market: HashMap::new(),
            accepted_merge_by_market: HashMap::new(),
            blocked_merge_by_market: HashMap::new(),
            condition_id_by_market: HashMap::new(),
            venue_market_rules: HashMap::new(),
            markets_with_unresolved_drift: HashSet::new(),
            markets_with_drift_first_seen_ms: HashMap::new(),
            require_initial_reconcile_before_entry: config.require_initial_reconcile_before_entry,
            initial_reconcile_complete: false,
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

    /// Cache venue-authoritative market rules. Called by the runner after a
    /// successful `fetch_market_metadata` round-trip. Subsequent calls to
    /// `strategy_context()` will pass these rules to the strategy so it can
    /// honour the venue's per-market minimum order size and tick size
    /// without duplicating those facts as operator-tunable env vars.
    pub fn set_venue_market_rules(&mut self, market_id: MarketId, rules: VenueMarketRules) {
        self.venue_market_rules.insert(market_id, rules);
    }

    pub fn venue_market_rules(&self, market_id: &MarketId) -> Option<VenueMarketRules> {
        self.venue_market_rules.get(market_id).copied()
    }

    pub fn market_context_record(
        &self,
        market_id: &MarketId,
    ) -> Option<crate::market_context::MarketContextRecord> {
        self.market_contexts.get(market_id).cloned()
    }

    fn canonical_market_id_for_instrument(
        &self,
        instrument_id: &InstrumentId,
        fallback: &MarketId,
    ) -> MarketId {
        self.market_contexts
            .market_id_for_asset(instrument_id.as_str())
            .unwrap_or_else(|| fallback.clone())
    }

    /// Clear the pending-merge dedup entry for a market. Called by the
    /// runner when a merge submission FAILS (retryable or otherwise) so
    /// the next reconcile sweep can re-attempt. Without this, a single
    /// transient merge failure (RPC down, gas issue) permanently blocks
    /// merging on that market — pending_merge_by_market only got cleared
    /// on successful fill arrival.
    pub fn clear_pending_merge(&mut self, market_id: &MarketId, now_ms: EpochMillis) {
        if self.pending_merge_by_market.remove(market_id).is_some() {
            self.event_log.push(
                EventRecord::new(
                    EventCategory::Execution,
                    now_ms,
                    "pending merge cleared (likely after submit failure); next reconcile sweep can retry",
                )
                .with_market(market_id.clone()),
            );
        }
    }

    pub fn mark_pending_merge_accepted(&mut self, market_id: &MarketId, now_ms: EpochMillis) {
        let Some(intent) = self.pending_merge_by_market.remove(market_id) else {
            return;
        };
        let signature = MergeSignature::from_intent(&intent);
        self.accepted_merge_by_market.insert(
            market_id.clone(),
            AcceptedMerge {
                signature,
                accepted_at_ms: now_ms,
            },
        );
        self.event_log.push(
            EventRecord::new(
                EventCategory::Execution,
                now_ms,
                "pending merge accepted; suppressing only identical CTF recycle until venue \
                 reconciliation changes paired inventory",
            )
            .with_market(market_id.clone()),
        );
    }

    pub fn block_pending_merge(
        &mut self,
        market_id: &MarketId,
        now_ms: EpochMillis,
        reason: impl Into<String>,
    ) {
        let Some(intent) = self.pending_merge_by_market.remove(market_id) else {
            return;
        };
        let signature = MergeSignature::from_intent(&intent);
        self.blocked_merge_by_market.insert(
            market_id.clone(),
            BlockedMerge {
                signature,
                reason: reason.into(),
                blocked_at_ms: now_ms,
            },
        );
        self.event_log.push(
            EventRecord::new(
                EventCategory::Execution,
                now_ms,
                "merge recycle blocked after non-retryable failure; will not resubmit \
                 identical CTF merge until inventory changes",
            )
            .with_market(market_id.clone()),
        );
    }

    pub fn run_id(&self) -> &str {
        self.run_id.as_str()
    }

    pub fn market_context_version(&self) -> &str {
        self.market_contexts.version.as_str()
    }

    pub fn replace_market_contexts(
        &mut self,
        market_contexts: MarketContextStore,
        observed_at_ms: EpochMillis,
        reason: impl Into<String>,
    ) -> RuntimeOutcome {
        let market_count = market_contexts.len();
        let source = market_contexts
            .source
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        self.market_contexts = market_contexts;
        let mut outcome = RuntimeOutcome::default();
        outcome.push_event(self.event_log.push(EventRecord::new(
            EventCategory::Strategy,
            observed_at_ms,
            format!(
                "market context replaced: rows={} source={} reason={}",
                market_count,
                source,
                reason.into()
            ),
        )));
        outcome
    }

    pub fn recover_from_store(
        &mut self,
        now_ms: EpochMillis,
        stale_after_ms: u64,
    ) -> RuntimeOutcome {
        let mut outcome = RuntimeOutcome::default();
        if self.require_initial_reconcile_before_entry {
            info!(
                run_id = %self.run_id,
                "recover_from_store: fresh entries require first venue position reconcile before \
                 drift-sensitive live trading can start"
            );
        }
        if self.order_store.is_none() {
            return outcome;
        }
        let runtime_status = self
            .order_store
            .as_ref()
            .expect("checked order_store")
            .latest_runtime_status();
        match runtime_status {
            Ok(Some(RuntimeStatus::RiskOff)) => {
                self.status = RuntimeStatus::RiskOff;
                outcome.push_event(self.event_log.push(EventRecord::new(
                    EventCategory::Runtime,
                    now_ms,
                    "restored runtime status from durable store status=RiskOff",
                )));
                outcome.push_event(
                    self.event_log
                        .push(EventRecord::runtime_status(now_ms, self.status)),
                );
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(error) => {
                warn!(
                    error = ?error,
                    run_id = %self.run_id,
                    "failed to load durable runtime status during recovery"
                );
            }
        }
        let strategy_tag = self.strategy.name().to_string();
        let strategy_state = self
            .order_store
            .as_ref()
            .expect("checked order_store")
            .latest_strategy_state(&strategy_tag);
        match strategy_state {
            Ok(Some(state)) => match self.strategy.restore_checkpoint_state(&state) {
                Ok(()) => {
                    outcome.push_event(self.event_log.push(EventRecord::new(
                        EventCategory::Runtime,
                        now_ms,
                        format!("restored strategy state strategy={strategy_tag}"),
                    )));
                }
                Err(error) => {
                    warn!(
                        error = %error,
                        strategy = %strategy_tag,
                        run_id = %self.run_id,
                        "failed to restore durable strategy state"
                    );
                    outcome.push_event(self.event_log.push(EventRecord::new(
                        EventCategory::Runtime,
                        now_ms,
                        format!(
                            "failed to restore strategy state strategy={strategy_tag}: {error}"
                        ),
                    )));
                }
            },
            Ok(None) => {}
            Err(error) => {
                warn!(
                    error = ?error,
                    strategy = %strategy_tag,
                    run_id = %self.run_id,
                    "failed to load durable strategy state during recovery"
                );
            }
        }
        let records = match self
            .order_store
            .as_mut()
            .expect("checked order_store")
            .list_open()
        {
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

    pub fn persist_strategy_state(&mut self, observed_at_ms: EpochMillis) {
        let Some(state) = self.strategy.checkpoint_state() else {
            return;
        };
        let strategy_tag = self.strategy.name().to_string();
        let run_id = self.run_id.clone();
        let Some(order_store) = self.order_store.as_mut() else {
            return;
        };
        if let Err(error) =
            order_store.put_strategy_state(&run_id, &strategy_tag, observed_at_ms, &state)
        {
            warn!(
                error = ?error,
                strategy = %strategy_tag,
                run_id = %run_id,
                "failed to persist strategy state"
            );
        }
    }

    pub fn persist_runtime_status(&mut self, observed_at_ms: EpochMillis) {
        let run_id = self.run_id.clone();
        let status = self.status;
        let Some(order_store) = self.order_store.as_mut() else {
            return;
        };
        if let Err(error) = order_store.put_runtime_status(&run_id, observed_at_ms, status) {
            warn!(
                error = ?error,
                status = ?status,
                run_id = %run_id,
                "failed to persist runtime status"
            );
        }
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

    pub fn reconcile_venue_cash(
        &mut self,
        authoritative_total_cash_usd: f64,
        observed_at_ms: EpochMillis,
    ) -> Result<crate::inventory::InventoryAdjustment, RuntimeError> {
        Ok(self
            .inventory
            .reconcile_venue_cash(authoritative_total_cash_usd, observed_at_ms)?)
    }

    pub fn reconcile_venue_positions(
        &mut self,
        venue_positions: &[VenuePositionSnapshot],
        observed_at_ms: EpochMillis,
    ) -> Result<InventoryReconciliationReport, RuntimeError> {
        let venue_positions = venue_positions
            .iter()
            .map(|position| {
                let canonical_market_id = self.canonical_market_id_for_instrument(
                    &position.instrument_id,
                    &position.market_id,
                );
                VenuePositionSnapshot {
                    market_id: canonical_market_id.clone(),
                    condition_id: position.condition_id.clone(),
                    instrument_id: position.instrument_id.clone(),
                    quantity: position.quantity,
                    average_cost_usd: self.resolved_venue_cost_basis_usd(
                        &canonical_market_id,
                        &position.instrument_id,
                        position.average_cost_usd,
                    ),
                    mark_price: position.mark_price,
                    observed_at_ms: position.observed_at_ms,
                }
            })
            .collect::<Vec<_>>();
        for venue_position in &venue_positions {
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
            .reconcile_venue_positions(&venue_positions, observed_at_ms)?;
        const DRIFT_QTY_EPSILON: f64 = 1e-6;
        // Hold-off before engaging the drift block. WS fill events arrive
        // ~50ms ahead of the next reconcile pass, so a transient
        // local-flat/venue-nonflat state is normal post-fill and should not
        // trip the incident #1 guard. We only engage the block once the
        // discrepancy persists past this window — long enough that it
        // can no longer be explained by event-loop latency.
        const DRIFT_HOLD_OFF_MS: u64 = 5_000;
        let is_startup_reconcile = !self.initial_reconcile_complete;
        for delta in &report.deltas {
            let local_was_flat = delta.local_quantity_before.abs() < DRIFT_QTY_EPSILON;
            let venue_has_position = delta.venue_quantity.abs() > DRIFT_QTY_EPSILON;
            if local_was_flat && venue_has_position && !is_startup_reconcile {
                let first_seen_ms = *self
                    .markets_with_drift_first_seen_ms
                    .entry(delta.market_id.clone())
                    .or_insert(observed_at_ms);
                let drift_age_ms = observed_at_ms.saturating_sub(first_seen_ms);
                if drift_age_ms >= DRIFT_HOLD_OFF_MS
                    && self
                        .markets_with_unresolved_drift
                        .insert(delta.market_id.clone())
                {
                    self.event_log.push(
                        EventRecord::new(
                            EventCategory::Inventory,
                            observed_at_ms,
                            format!(
                                "drift block engaged: local was flat but venue qty={:.8} \
                                 for {drift_age_ms}ms; fresh entry suppressed in this \
                                 market until drift clears (incident #1 guard)",
                                delta.venue_quantity
                            ),
                        )
                        .with_market(delta.market_id.clone())
                        .with_instrument(delta.instrument_id.clone()),
                    );
                }
            } else if local_was_flat && venue_has_position && is_startup_reconcile {
                self.event_log.push(
                    EventRecord::new(
                        EventCategory::Inventory,
                        observed_at_ms,
                        format!(
                            "startup reconcile installed venue position qty={:.8} (no drift block; \
                             local was flat because runtime just started, not because of mid-session loss)",
                            delta.venue_quantity
                        ),
                    )
                    .with_market(delta.market_id.clone())
                    .with_instrument(delta.instrument_id.clone()),
                );
            } else if delta.quantity_delta.abs() < DRIFT_QTY_EPSILON {
                // Drift resolved (or never engaged). Clear both the
                // pending-detection tracker and the engaged-block set.
                self.markets_with_drift_first_seen_ms
                    .remove(&delta.market_id);
                if self.markets_with_unresolved_drift.remove(&delta.market_id) {
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
        let active_markets = venue_positions
            .iter()
            .filter(|position| position.quantity.abs() > DRIFT_QTY_EPSILON)
            .map(|position| position.market_id.clone())
            .collect::<HashSet<_>>();
        self.accepted_merge_by_market
            .retain(|market_id, _| active_markets.contains(market_id));
        self.initial_reconcile_complete = true;
        Ok(report)
    }

    fn resolved_venue_cost_basis_usd(
        &self,
        market_id: &MarketId,
        instrument_id: &InstrumentId,
        venue_average_cost_usd: f64,
    ) -> f64 {
        if venue_average_cost_usd.is_finite() && venue_average_cost_usd > 0.0 {
            return venue_average_cost_usd;
        }
        let Some(order_store) = self.order_store.as_ref() else {
            return venue_average_cost_usd;
        };
        match order_store.filled_buy_cost_basis(market_id, instrument_id) {
            Ok(Some(cost_basis)) if cost_basis.is_finite() && cost_basis > 0.0 => {
                info!(
                    run_id = %self.run_id,
                    market_id = %market_id,
                    instrument_id = %instrument_id,
                    venue_average_cost_usd,
                    resolved_average_cost_usd = cost_basis,
                    "resolved missing venue position cost basis from durable fills"
                );
                cost_basis
            }
            Ok(_) => venue_average_cost_usd,
            Err(error) => {
                warn!(
                    run_id = %self.run_id,
                    market_id = %market_id,
                    instrument_id = %instrument_id,
                    error = ?error,
                    "failed to resolve venue position cost basis from durable fills"
                );
                venue_average_cost_usd
            }
        }
    }

    pub fn stranded_inventory(&self) -> Vec<StrandedMarketInventory> {
        self.inventory.stranded_market_inventory()
    }

    /// Paper-mode market close handler. When the runtime clock crosses
    /// `paper_market_close_at_ms`, the runner calls this once. It returns a
    /// `RuntimeOutcome` containing:
    ///
    /// 1. `RuntimeCommand::Cancel` for every currently-open order.
    /// 2. `RuntimeCommand::Merge` for every market with paired inventory
    ///    (via `plan_merge_command_for_market`).
    /// 3. Synthetic `FillReport`s with `CloseMethod::Redeem` applied for each
    ///    stranded position at `resolution_price` (0.0 = "no" wins, 1.0 = "yes"
    ///    wins, 0.5 = unknown). When `resolution_price` is None and stranded
    ///    inventory exists, an Inventory event is logged but the inventory is
    ///    NOT settled (operator must intervene).
    /// 4. A single Runtime event marking the close.
    ///
    /// Phase 1 of the paper environment design doc
    /// (docs/architecture/2026-04-25-paper-env-design.md).
    pub fn plan_paper_close(
        &mut self,
        now_ms: EpochMillis,
        resolution_price: Option<f64>,
    ) -> RuntimeOutcome {
        let mut outcome = RuntimeOutcome::default();

        let open_coids: Vec<(ClientOrderId, MarketId, InstrumentId)> = self
            .open_orders
            .iter()
            .map(|(coid, managed)| {
                (
                    coid.clone(),
                    managed.intent.market_id.clone(),
                    managed.intent.instrument_id.clone(),
                )
            })
            .collect();
        for (coid, market_id, instrument_id) in open_coids {
            outcome.push_event(
                self.event_log.push(
                    EventRecord::new(
                        EventCategory::Runtime,
                        now_ms,
                        "paper market close: cancelling open order",
                    )
                    .with_market(market_id)
                    .with_instrument(instrument_id)
                    .with_client_order(coid.clone()),
                ),
            );
            outcome.push_command(RuntimeCommand::Cancel {
                client_order_id: coid,
                reason: "paper market close".to_string(),
            });
        }

        let paired_market_ids: Vec<MarketId> = self
            .inventory
            .stranded_market_inventory()
            .iter()
            .filter(|s| s.paired_quantity > 1e-9)
            .map(|s| s.market_id.clone())
            .collect();
        let mut all_market_ids: std::collections::HashSet<MarketId> = self
            .inventory
            .positions()
            .map(|p| p.market_id.clone())
            .collect();
        for mid in paired_market_ids {
            outcome.extend(self.plan_merge_command_for_market(&mid, now_ms, "paper market close"));
            all_market_ids.remove(&mid);
        }

        let stranded = self.inventory.stranded_market_inventory();
        for strand in stranded {
            for position in &strand.stranded_positions {
                if position.quantity.abs() <= 1e-9 {
                    continue;
                }
                match resolution_price {
                    Some(price) => {
                        let synthetic_fill = FillReport {
                            order_id: None,
                            client_order_id: None,
                            market_id: strand.market_id.clone(),
                            instrument_id: position.instrument_id.clone(),
                            side: TradeSide::Sell,
                            price,
                            quantity: position.quantity,
                            fee_usd: 0.0,
                            liquidity: FillLiquidity::Unknown,
                            close_method: Some(CloseMethod::Redeem),
                            observed_at_ms: now_ms,
                        };
                        match self.on_fill(synthetic_fill) {
                            Ok(fill_outcome) => outcome.extend(fill_outcome),
                            Err(error) => {
                                outcome.push_event(
                                    self.event_log.push(
                                        EventRecord::new(
                                            EventCategory::Inventory,
                                            now_ms,
                                            format!(
                                                "paper market close: redeem fill rejected: {error}"
                                            ),
                                        )
                                        .with_market(strand.market_id.clone())
                                        .with_instrument(position.instrument_id.clone()),
                                    ),
                                );
                            }
                        }
                    }
                    None => {
                        outcome.push_event(
                            self.event_log.push(
                                EventRecord::new(
                                    EventCategory::Inventory,
                                    now_ms,
                                    format!(
                                        "paper market close: stranded position requires \
                                         resolution_price (qty={:.8}); operator must settle manually",
                                        position.quantity
                                    ),
                                )
                                .with_market(strand.market_id.clone())
                                .with_instrument(position.instrument_id.clone()),
                            ),
                        );
                    }
                }
            }
        }

        outcome.push_event(self.event_log.push(EventRecord::new(
            EventCategory::Runtime,
            now_ms,
            format!(
                "paper market close at_ms={now_ms} resolution_price={:?} \
                     (Phase 1 paper env)",
                resolution_price
            ),
        )));

        outcome
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

        // Dust/noise gate: keep this profile-driven. Polygon merge fees are
        // negligible for us, so tiny-live and paper should be able to recycle
        // very small paired inventory while production can still avoid dust.
        if intent.expected_cash_usd + 1e-9 < self.min_merge_notional_usd {
            outcome.push_event(
                self.event_log.push(
                    EventRecord::new(
                        EventCategory::Execution,
                        now_ms,
                        format!(
                            "merge skipped: paired notional ${:.4} below min merge notional ${:.4}",
                            intent.expected_cash_usd, self.min_merge_notional_usd
                        ),
                    )
                    .with_market(market_id.clone()),
                ),
            );
            return outcome;
        }

        let signature = MergeSignature::from_intent(&intent);
        if let Some((blocked_at_ms, blocked_reason)) = self
            .blocked_merge_by_market
            .get(market_id)
            .and_then(|blocked| {
                (blocked.signature == signature)
                    .then(|| (blocked.blocked_at_ms, blocked.reason.clone()))
            })
        {
            let blocked_for_ms = now_ms.saturating_sub(blocked_at_ms);
            if blocked_for_ms < BLOCKED_MERGE_RETRY_AFTER_MS {
                outcome.push_event(
                    self.event_log.push(
                        EventRecord::new(
                            EventCategory::Execution,
                            now_ms,
                            format!(
                                "merge intent suppressed: matching CTF recycle is blocked \
                                 since {} reason={}",
                                blocked_at_ms, blocked_reason
                            ),
                        )
                        .with_market(market_id.clone()),
                    ),
                );
                return outcome;
            }
            self.blocked_merge_by_market.remove(market_id);
            outcome.push_event(
                self.event_log.push(
                    EventRecord::new(
                        EventCategory::Execution,
                        now_ms,
                        format!(
                            "blocked merge retry backoff elapsed after {blocked_for_ms}ms; \
                             retrying CTF recycle reason={blocked_reason}"
                        ),
                    )
                    .with_market(market_id.clone()),
                ),
            );
        }
        if let Some(accepted) = self.accepted_merge_by_market.get(market_id) {
            if accepted.signature == signature {
                outcome.push_event(
                    self.event_log.push(
                        EventRecord::new(
                            EventCategory::Execution,
                            now_ms,
                            format!(
                                "merge intent suppressed: matching CTF recycle was already \
                                 accepted at {}; awaiting venue reconciliation",
                                accepted.accepted_at_ms
                            ),
                        )
                        .with_market(market_id.clone()),
                    ),
                );
                return outcome;
            }
            self.accepted_merge_by_market.remove(market_id);
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
        if matches!(
            self.status,
            RuntimeStatus::Degraded | RuntimeStatus::RiskOff
        ) {
            let mut outcome = RuntimeOutcome::default();
            let message = match self.status {
                RuntimeStatus::Degraded => "runtime start skipped because runtime is degraded",
                RuntimeStatus::RiskOff => "runtime start skipped because runtime is risk-off",
                _ => unreachable!("start guard only handles degraded/risk-off statuses"),
            };
            outcome.push_event(self.event_log.push(EventRecord::new(
                EventCategory::Runtime,
                now_ms,
                message,
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
            Ok(outcome)
        } else {
            let context = self.strategy_context(now_ms, Some(&snapshot.market_id));
            let decision = self.strategy.on_market_snapshot(&context, &snapshot);
            let outcome = self.accept_strategy_decision(decision, now_ms);
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
        let (taker_buy_qty_60s, taker_sell_qty_60s) =
            book.taker_flow_qty_60s(book.last_update_unix_ms);

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
                taker_buy_qty_60s,
                taker_sell_qty_60s,
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

    pub fn btc_regime_snapshot(&self, now_ms: EpochMillis) -> GateBtcRegimeSnapshot {
        self.btc_signals.snapshot(now_ms)
    }

    pub fn on_fill(&mut self, mut fill: FillReport) -> Result<RuntimeOutcome, RuntimeError> {
        fill.market_id =
            self.canonical_market_id_for_instrument(&fill.instrument_id, &fill.market_id);
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
        let mut rejected_pair_id: Option<String> = None;
        if let Some(managed) = self.open_orders.remove(client_order_id) {
            rejected_pair_id = managed.intent.pair_id.clone();
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
        if let Some(pair_id) = rejected_pair_id {
            let mates: Vec<(ClientOrderId, MarketId, InstrumentId)> = self
                .open_orders
                .iter()
                .filter_map(|(coid, managed)| {
                    if managed.intent.pair_id.as_deref() == Some(pair_id.as_str()) {
                        Some((
                            coid.clone(),
                            managed.intent.market_id.clone(),
                            managed.intent.instrument_id.clone(),
                        ))
                    } else {
                        None
                    }
                })
                .collect();
            for (mate_coid, market_id, instrument_id) in mates {
                outcome.push_event(
                    self.event_log.push(
                        EventRecord::new(
                            EventCategory::Runtime,
                            now_ms,
                            format!(
                                "paired-entry guard: cancelling mate {mate_coid} after \
                                 rejection of {client_order_id} (pair={pair_id}, incident #4)"
                            ),
                        )
                        .with_market(market_id)
                        .with_instrument(instrument_id)
                        .with_client_order(mate_coid.clone()),
                    ),
                );
                outcome.push_command(RuntimeCommand::Cancel {
                    client_order_id: mate_coid,
                    reason: format!("paired-entry guard: mate of rejected {client_order_id}"),
                });
            }
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

    pub fn request_cancel_entry_orders(
        &mut self,
        now_ms: EpochMillis,
        reason: impl Into<String>,
    ) -> RuntimeOutcome {
        let reason = reason.into();
        let ids = self
            .open_orders
            .values()
            .filter(|managed| managed.intent.kind != crate::types::IntentKind::Close)
            .map(|managed| managed.intent.client_order_id.clone())
            .collect::<Vec<_>>();
        let mut outcome = RuntimeOutcome::default();
        for client_order_id in ids {
            outcome.extend(self.request_cancel(&client_order_id, reason.clone(), now_ms));
        }
        outcome
    }

    pub fn request_cancel_orders_not_in_instruments(
        &mut self,
        active_instruments: &HashSet<InstrumentId>,
        now_ms: EpochMillis,
        reason: impl Into<String>,
    ) -> RuntimeOutcome {
        let reason = reason.into();
        let ids = self
            .open_orders
            .values()
            .filter(|managed| !active_instruments.contains(&managed.intent.instrument_id))
            .map(|managed| managed.intent.client_order_id.clone())
            .collect::<Vec<_>>();
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

    pub fn riskoff_and_cancel_entry_orders(
        &mut self,
        now_ms: EpochMillis,
        reason: impl Into<String>,
    ) -> RuntimeOutcome {
        let reason = reason.into();
        let mut outcome = RuntimeOutcome::default();
        // RiskOff remains sticky inside the core runtime. Live auto-recovery
        // is an explicit runner policy that calls recover_from_riskoff only
        // after its health/risk checks pass for the configured window.
        if self.status != RuntimeStatus::RiskOff {
            self.status = RuntimeStatus::RiskOff;
            outcome.push_event(self.event_log.push(EventRecord::new(
                EventCategory::Runtime,
                now_ms,
                format!("runtime risk-off: {reason}"),
            )));
            outcome.push_event(
                self.event_log
                    .push(EventRecord::runtime_status(now_ms, self.status)),
            );
        }
        outcome.extend(self.request_cancel_entry_orders(now_ms, reason));
        outcome
    }

    pub fn recover_from_riskoff(
        &mut self,
        now_ms: EpochMillis,
        reason: impl Into<String>,
    ) -> RuntimeOutcome {
        self.recover_live_blocked_status(now_ms, reason)
    }

    pub fn recover_live_blocked_status(
        &mut self,
        now_ms: EpochMillis,
        reason: impl Into<String>,
    ) -> RuntimeOutcome {
        let mut outcome = RuntimeOutcome::default();
        let previous_status = self.status;
        if matches!(
            previous_status,
            RuntimeStatus::RiskOff | RuntimeStatus::Degraded
        ) {
            self.status = RuntimeStatus::Running;
            let label = match previous_status {
                RuntimeStatus::RiskOff => "risk-off",
                RuntimeStatus::Degraded => "degraded",
                _ => "blocked",
            };
            outcome.push_event(self.event_log.push(EventRecord::new(
                EventCategory::Runtime,
                now_ms,
                format!("runtime {label} auto-recovered: {}", reason.into()),
            )));
            outcome.push_event(
                self.event_log
                    .push(EventRecord::runtime_status(now_ms, self.status)),
            );
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
        for note in decision.notes() {
            outcome.push_event(self.event_log.push(EventRecord::new(
                EventCategory::Strategy,
                now_ms,
                note,
            )));
        }

        match decision {
            StrategyDecision::Noop { .. } => {}
            StrategyDecision::Reactive { intents, .. } => {
                if !intents.is_empty() {
                    outcome.push_event(self.event_log.push(EventRecord::new(
                        EventCategory::Strategy,
                        now_ms,
                        "quote reconciliation skipped: preserving working quotes during close-side strategy reaction",
                    )));
                }
                for intent in intents {
                    outcome.extend(self.accept_intent(intent, now_ms));
                }
            }
            StrategyDecision::Suppress { kind, .. } => match kind {
                StrategyDecisionSuppressionKind::SoftPause => {
                    outcome.push_event(self.event_log.push(EventRecord::new(
                        EventCategory::Strategy,
                        now_ms,
                        "strategy suppression: soft pause requested (preserving open entry quotes)",
                    )));
                }
                StrategyDecisionSuppressionKind::HardRiskOff => {
                    outcome.extend(self.riskoff_and_cancel_entry_orders(
                        now_ms,
                        "strategy suppression: hard risk-off",
                    ));
                }
            },
            StrategyDecision::Commands { commands, .. } => {
                for command in commands {
                    match command {
                        RuntimeCommand::Submit(intent) => {
                            outcome.extend(self.accept_intent(intent, now_ms));
                        }
                        RuntimeCommand::Cancel {
                            client_order_id,
                            reason,
                        } => {
                            outcome.extend(self.request_cancel(&client_order_id, reason, now_ms));
                        }
                        RuntimeCommand::Merge(intent) => {
                            outcome.push_command(RuntimeCommand::Merge(intent));
                        }
                        RuntimeCommand::Redeem(intent) => {
                            outcome.push_command(RuntimeCommand::Redeem(intent));
                        }
                        RuntimeCommand::Noop => {}
                    }
                }
            }
            StrategyDecision::QuoteSet { intents, .. } => {
                let intents_in = intents.len();
                let level_tags_in: Vec<String> = intents
                    .iter()
                    .map(|i| i.quote_level_tag.clone().unwrap_or_default())
                    .collect();
                let desired_pre = DesiredQuoteSet::from_intents(intents, &self.quote_engine_config);
                let quotes_after_from_intents = desired_pre.quotes.len();
                let desired = desired_pre.with_stale_gate(
                    now_ms,
                    &self.last_quotes,
                    StaleMode::Remove,
                    |snapshot, now| {
                        snapshot.is_none_or(|quote| {
                            now.saturating_sub(quote.observed_at_ms) > self.quote_stale_ms
                        })
                    },
                );
                let quotes_after_stale_gate = desired.quotes.len();
                if intents_in > 0 {
                    tracing::info!(
                        target: "ladder.diag",
                        intents_in,
                        quotes_after_from_intents,
                        quotes_after_stale_gate,
                        level_tags_in = ?level_tags_in,
                        "ladder pipeline counts"
                    );
                }
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
            }
        }
        outcome
    }

    fn accept_intent(&mut self, intent: OrderIntent, now_ms: EpochMillis) -> RuntimeOutcome {
        // TODO(2026-04-23): integrate execution acknowledgements/fill events from a downstream
        // matcher and remove this placeholder reserve->submit transition assumption.
        let mut outcome = RuntimeOutcome::default();
        // Hedge-rescue intents are CLOSE operations that intentionally lift
        // the OPPOSITE leg's ask via IOC. They are NOT duplicates of any
        // existing maker paired-bid on that same instrument — different
        // prices, different intent kind (taker vs maker), different goal.
        // Suppressing them here leaves us stranded long. Bypass for rescue.
        let is_rescue_intent = intent.kind == crate::types::IntentKind::Close;
        if self.status != RuntimeStatus::Running && !is_rescue_intent {
            outcome.push_event(
                self.event_log.push(
                    EventRecord::new(
                        EventCategory::Runtime,
                        now_ms,
                        "fresh entry suppressed: runtime is not running",
                    )
                    .with_market(intent.market_id.clone())
                    .with_instrument(intent.instrument_id.clone())
                    .with_client_order(intent.client_order_id.clone()),
                ),
            );
            return outcome;
        }
        if self.require_initial_reconcile_before_entry
            && !self.initial_reconcile_complete
            && !is_rescue_intent
        {
            outcome.push_event(
                self.event_log.push(
                    EventRecord::new(
                        EventCategory::Runtime,
                        now_ms,
                        "fresh entry suppressed: initial venue position reconcile has not completed",
                    )
                    .with_market(intent.market_id.clone())
                    .with_instrument(intent.instrument_id.clone())
                    .with_client_order(intent.client_order_id.clone()),
                ),
            );
            return outcome;
        }
        if !is_rescue_intent && self.has_active_btc_mm_buy_for_instrument(&intent) {
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
        if intent.kind == crate::types::IntentKind::Entry
            && intent.side == TradeSide::Buy
            && !is_rescue_intent
            && self
                .last_quotes
                .get(&intent.instrument_id)
                .and_then(|quote| quote.best_ask.as_ref())
                .is_some_and(|ask| intent.limit_price >= ask.price)
        {
            outcome.push_event(
                self.event_log.push(
                    EventRecord::new(
                        EventCategory::Runtime,
                        now_ms,
                        "maker entry suppressed: latest book would cross best ask",
                    )
                    .with_market(intent.market_id.clone())
                    .with_instrument(intent.instrument_id.clone())
                    .with_client_order(intent.client_order_id.clone()),
                ),
            );
            return outcome;
        }
        // Drift block — incident #1 guard. Suppresses FRESH ENTRIES on
        // markets where local was flat but venue had positions (typically
        // post-restart before reconcile populated). MUST bypass for rescue
        // intents: they're close-side operations that manufacture the
        // missing leg to enable a merge — exactly the OPPOSITE of "fresh
        // entry". Without this bypass, stranded inventory in drifted
        // markets sits naked forever (998 rescue intents silently dropped
        // / 0 hedge-rescue ever appeared at venue in v27 logs over 1h).
        // Same pattern as the other 4 cap-bypass layers per CLAUDE.md
        // "Distinguishing entry vs close intents".
        if !intent.reduce_only
            && !is_rescue_intent
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
            open_buy_notional_total_usd: self.open_buy_notional_total_usd(),
            open_signed_notional_for_market_usd: self
                .open_signed_notional_for_market_usd(&intent.market_id),
            open_position_qty_for_instrument: self
                .open_position_qty_for_instrument(&intent.instrument_id),
            starting_cash_usd: self.starting_cash_usd,
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
            position.market_id == managed.intent.market_id
                && self.position_is_actionable_inventory(position)
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
        // Match also by quote_level_tag so distinct ladder levels (l1, l2, ...)
        // don't dedupe against each other. Pre-ladder, this filter compared by
        // (market, instrument) only — fine for one-quote-per-instrument, but it
        // collapses N-level ladders into level 0 because l2..lN all see l1
        // active and get rejected as duplicates. Adding level_tag preserves the
        // original "no two identical quotes" intent without blocking legitimate
        // multi-level ladder emission.
        self.open_orders.values().any(|managed| {
            managed.intent.client_order_id != intent.client_order_id
                && Self::is_btc_mm_buy_intent(&managed.intent)
                && managed.intent.market_id == intent.market_id
                && managed.intent.instrument_id == intent.instrument_id
                && managed.intent.quote_level_tag == intent.quote_level_tag
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
        let venue_rules = market_id.and_then(|id| self.venue_market_rules.get(id).copied());
        StrategyContext {
            now_ms,
            runtime_status: self.status,
            inventory: self.inventory.snapshot(),
            open_orders_total: self.open_orders.len(),
            open_orders_for_market: market_id
                .map(|id| self.open_orders_for_market(id))
                .unwrap_or(0),
            market_context,
            btc_regime: self.btc_signals.snapshot(now_ms),
            venue_rules,
        }
    }

    fn actionable_order_qty_for_market(&self, market_id: &MarketId) -> f64 {
        self.venue_market_rules
            .get(market_id)
            .map(|rules| rules.minimum_order_size)
            .filter(|quantity| quantity.is_finite() && *quantity > ACCOUNTING_QTY_EPSILON)
            .unwrap_or(ACCOUNTING_QTY_EPSILON)
    }

    fn position_is_actionable_inventory(&self, position: &crate::inventory::PositionState) -> bool {
        position.quantity.abs() + ACCOUNTING_QTY_EPSILON
            >= self.actionable_order_qty_for_market(&position.market_id)
    }

    fn market_has_inventory(&self, market_id: &MarketId) -> bool {
        let positions = self
            .inventory
            .positions()
            .filter(|position| {
                &position.market_id == market_id && position.quantity.abs() > ACCOUNTING_QTY_EPSILON
            })
            .collect::<Vec<_>>();
        if positions.is_empty() {
            return false;
        }

        if positions.len() >= 2 {
            let paired_quantity = positions
                .iter()
                .map(|position| position.quantity.abs())
                .fold(f64::INFINITY, f64::min);
            if paired_quantity.is_finite() && paired_quantity > ACCOUNTING_QTY_EPSILON {
                return true;
            }
        }

        positions
            .iter()
            .any(|position| self.position_is_actionable_inventory(position))
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
        self.accepted_merge_by_market.remove(market_id);
        self.blocked_merge_by_market.remove(market_id);
    }

    fn open_orders_for_market(&self, market_id: &crate::types::MarketId) -> usize {
        self.open_orders
            .values()
            .filter(|managed| &managed.intent.market_id == market_id)
            .count()
    }

    fn open_buy_notional_total_usd(&self) -> f64 {
        self.open_orders
            .values()
            .filter(|managed| {
                !managed.status.is_terminal()
                    && matches!(managed.intent.side, crate::types::TradeSide::Buy)
                    && !managed.intent.reduce_only
            })
            .map(|managed| managed.intent.limit_price * managed.remaining_qty())
            .sum()
    }

    fn open_signed_notional_for_market_usd(&self, market_id: &crate::types::MarketId) -> f64 {
        self.open_orders
            .values()
            .filter(|managed| {
                !managed.status.is_terminal() && &managed.intent.market_id == market_id
            })
            .map(|managed| {
                managed.intent.limit_price * managed.remaining_qty() * managed.intent.side.sign()
            })
            .sum()
    }

    fn open_position_qty_for_instrument(&self, instrument_id: &crate::types::InstrumentId) -> f64 {
        self.open_orders
            .values()
            .filter(|managed| {
                !managed.status.is_terminal() && &managed.intent.instrument_id == instrument_id
            })
            .map(|managed| managed.remaining_qty() * managed.intent.side.sign())
            .sum()
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
        // Old persisted records pre-date IntentKind. Infer from
        // quote_level_tag for backwards compat: rescue tag → Close,
        // anything else → Entry.
        let kind = if record
            .quote_level_tag
            .as_deref()
            .is_some_and(|tag| tag.starts_with("mm-hedge-rescue"))
            || record.reduce_only
        {
            crate::types::IntentKind::Close
        } else {
            crate::types::IntentKind::Entry
        };
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
                pair_id: None,
                kind,
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
        let kind = if record
            .quote_level_tag
            .as_deref()
            .is_some_and(|tag| tag.starts_with("mm-hedge-rescue"))
            || record.reduce_only
        {
            crate::types::IntentKind::Close
        } else {
            crate::types::IntentKind::Entry
        };
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
                pair_id: None,
                kind,
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
#[path = "../../tests/unit/runtime_core.rs"]
mod tests;
