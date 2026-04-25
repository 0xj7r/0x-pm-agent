use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use polymarket_exec::journal::JournalWriter;
use polymarket_exec::market_context::MarketContextStore;
use polymarket_exec::risk::RiskLimits;
use polymarket_exec::runtime::order_store::SqliteOrderStore;
use polymarket_exec::runtime::{ManagedOrderStatus, Runtime, RuntimeConfig};
use polymarket_exec::strategy::{Strategy, StrategyContext, StrategyDecision};
use polymarket_exec::types::{
    ClientOrderId, CloseMethod, FillLiquidity, FillReport, InstrumentId, MarketId, MarketSnapshot,
    OrderIntent, RuntimeCommand, RuntimeStatus, TradeSide,
};

const FIXTURE_DIR: &str = "tests/fixtures/btc_5m_mm";

#[derive(Debug, Deserialize)]
struct ScenarioFixture {
    name: String,
    description: String,
    market_id: String,
    starting_cash_usd: f64,
    event_log_capacity: usize,
    #[serde(default)]
    paper_mode: bool,
    #[serde(default)]
    adapter: AdapterPlan,
    events: Vec<ScenarioEvent>,
    expected: ScenarioExpected,
}

#[derive(Debug, Deserialize, Default)]
struct AdapterPlan {
    #[serde(default)]
    submit: Vec<AdapterSubmitOutcome>,
    #[serde(default)]
    cancel: Vec<AdapterCancelOutcome>,
    #[serde(default)]
    sync_open_orders: Option<SyncOpenOrders>,
}

#[derive(Debug, Deserialize)]
struct AdapterSubmitOutcome {
    outcome: String,
    client_order_id: String,
    #[serde(default)]
    venue_order_id: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdapterCancelOutcome {
    outcome: String,
    client_order_id: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SyncOpenOrders {
    open_orders: Vec<SyncOpenOrder>,
}

#[derive(Debug, Deserialize)]
struct SyncOpenOrder {
    client_order_id: String,
    venue_order_id: String,
    market_id: String,
    instrument_id: String,
    side: FixtureSide,
    limit_price: f64,
    original_qty: f64,
    remaining_qty: f64,
    created_at_ms: u64,
}

#[derive(Debug, Deserialize)]
struct ScenarioExpected {
    runtime_status: String,
    free_cash_usd: f64,
    open_order_count: usize,
    #[serde(default)]
    min_event_log_len: Option<usize>,
    required_checkpoint_keys: Vec<String>,
    required_event_categories: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ScenarioEvent {
    Snapshot {
        at_ms: u64,
        instrument_id: String,
        market_id: String,
        best_bid: PriceLevel,
        best_ask: PriceLevel,
        last_trade_price: f64,
        #[serde(default)]
        decisions: Vec<FixtureDecision>,
        #[serde(default)]
        skip_if_stale: bool,
        #[serde(default)]
        end_of_window: bool,
    },
    Fill {
        at_ms: u64,
        client_order_id: String,
        market_id: String,
        instrument_id: String,
        side: FixtureSide,
        price: f64,
        quantity: f64,
        fee_usd: f64,
        liquidity: FixtureLiquidity,
        #[serde(default)]
        close_method: Option<String>,
    },
    Crash {
        at_ms: u64,
        label: String,
    },
    Restart {
        at_ms: u64,
        starting_cash_usd: f64,
    },
    Reconcile {
        at_ms: u64,
        label: String,
        expected: ReconcileExpected,
    },
    Checkpoint {
        at_ms: u64,
        label: String,
        expected: CheckpointExpected,
    },
    JournalAssert {
        min_lines: usize,
        must_contain_kinds: Vec<String>,
    },
    /// Cancel an outstanding order at the recorded timestamp. Used by the
    /// late-fill-after-cancel scenario to exercise terminal_fill_correction
    /// (Cancelled -> Filled when a late Fill event follows a Cancel).
    Cancel {
        at_ms: u64,
        client_order_id: String,
        #[serde(default)]
        reason: String,
    },
    /// Phase 1 paper market close: drives runtime.plan_paper_close at the
    /// recorded timestamp with the supplied resolution price. Exercises
    /// the cancel-all + merge-paired + redeem-stranded settlement path.
    PaperMarketClose {
        at_ms: u64,
        #[serde(default)]
        resolution_price: Option<f64>,
    },
}

#[derive(Debug, Deserialize)]
struct PriceLevel {
    price: f64,
    quantity: f64,
}

#[derive(Debug, Deserialize)]
struct FixtureDecision {
    client_order_id: String,
    instrument_id: String,
    side: FixtureSide,
    limit_price: f64,
    quantity: f64,
    reduce_only: bool,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct ReconcileExpected {
    #[serde(default)]
    venue_only: Vec<String>,
    #[serde(default)]
    local_only: Vec<String>,
    #[serde(default)]
    needs_reconcile: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CheckpointExpected {
    runtime_status: String,
    inventory: CheckpointInventoryExpected,
    #[serde(default)]
    open_orders: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct CheckpointInventoryExpected {
    #[serde(default)]
    free_cash_usd: Option<f64>,
    #[serde(default)]
    positions: Option<Vec<Value>>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "PascalCase")]
enum FixtureSide {
    Buy,
    Sell,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "PascalCase")]
enum FixtureLiquidity {
    Maker,
    Taker,
    Unknown,
}

impl From<FixtureSide> for TradeSide {
    fn from(value: FixtureSide) -> Self {
        match value {
            FixtureSide::Buy => Self::Buy,
            FixtureSide::Sell => Self::Sell,
        }
    }
}

impl From<FixtureLiquidity> for FillLiquidity {
    fn from(value: FixtureLiquidity) -> Self {
        match value {
            FixtureLiquidity::Maker => Self::Maker,
            FixtureLiquidity::Taker => Self::Taker,
            FixtureLiquidity::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone)]
struct FixtureStrategy {
    decisions_by_key: HashMap<(u64, String), Vec<OrderIntent>>,
}

impl FixtureStrategy {
    fn from_fixture(fixture: &ScenarioFixture) -> Self {
        let mut decisions_by_key = HashMap::new();
        for event in &fixture.events {
            if let ScenarioEvent::Snapshot {
                at_ms,
                instrument_id,
                decisions,
                ..
            } = event
            {
                let intents = decisions
                    .iter()
                    .map(|decision| OrderIntent {
                        client_order_id: ClientOrderId::from(decision.client_order_id.clone()),
                        market_id: MarketId::from(fixture.market_id.clone()),
                        instrument_id: InstrumentId::from(decision.instrument_id.clone()),
                        side: decision.side.into(),
                        limit_price: decision.limit_price,
                        quantity: decision.quantity,
                        reduce_only: decision.reduce_only,
                        reason: decision.reason.clone(),
                        quote_level_tag: None,
                        created_at_ms: *at_ms,
                        pair_id: None,
                    })
                    .collect::<Vec<_>>();
                decisions_by_key.insert((*at_ms, instrument_id.clone()), intents);
            }
        }
        Self { decisions_by_key }
    }
}

impl Strategy for FixtureStrategy {
    fn name(&self) -> &str {
        "fixture"
    }

    fn on_market_snapshot(
        &mut self,
        _context: &StrategyContext,
        snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        let intents = self
            .decisions_by_key
            .remove(&(
                snapshot.quote.observed_at_ms,
                snapshot.instrument_id.as_str().to_string(),
            ))
            .unwrap_or_default();
        StrategyDecision {
            intents,
            notes: Vec::new(),
        }
    }
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(FIXTURE_DIR)
        .join(format!("{name}.json"))
}

fn load_fixture(name: &str) -> ScenarioFixture {
    let path = fixture_path(name);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read fixture {}: {error}", path.display()));
    serde_json::from_str(&raw)
        .unwrap_or_else(|error| panic!("failed to parse fixture {}: {error}", path.display()))
}

fn unique_workspace_path(prefix: &str, suffix: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{unique}{suffix}"))
}

fn build_runtime(
    fixture: &ScenarioFixture,
    store_path: &Path,
    starting_cash_usd: f64,
) -> Runtime<FixtureStrategy> {
    let order_store = SqliteOrderStore::open(store_path).unwrap_or_else(|error| {
        panic!(
            "failed to open order store {}: {error}",
            store_path.display()
        )
    });
    Runtime::new_with_order_store(
        RuntimeConfig {
            starting_cash_usd,
            event_log_capacity: fixture.event_log_capacity,
            initial_status: RuntimeStatus::Starting,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        FixtureStrategy::from_fixture(fixture),
        MarketContextStore::empty(),
        Some(Box::new(order_store)),
        fixture.name.clone(),
    )
}

fn drain_event_log(
    runtime: &Runtime<FixtureStrategy>,
    journal: &mut JournalWriter,
    seen_categories: &mut BTreeSet<String>,
    last_seq: &mut u64,
) {
    for event in runtime.event_log().snapshot_since(*last_seq) {
        seen_categories.insert(format!("{:?}", event.category));
        journal.append_event(&event).unwrap();
    }
    *last_seq = runtime.event_log().latest_seq();
    journal.flush().unwrap();
}

fn checkpoint_artifact(
    runtime: &Runtime<FixtureStrategy>,
    checkpoint_id: &str,
    observed_at_ms: u64,
) -> Value {
    let snapshot = runtime.inventory().snapshot();
    let open_orders = runtime
        .open_order_snapshots()
        .into_iter()
        .map(|order| {
            json!({
                "client_order_id": order.intent.client_order_id.as_str(),
                "market_id": order.intent.market_id.as_str(),
                "instrument_id": order.intent.instrument_id.as_str(),
                "status": format!("{:?}", order.status),
                "remaining_qty": order.remaining_qty(),
                "cumulative_filled_qty": order.cumulative_filled_qty,
                "last_update_ms": order.last_update_ms,
            })
        })
        .collect::<Vec<_>>();
    let positions = snapshot
        .positions
        .into_iter()
        .map(|position| {
            json!({
                "market_id": position.market_id.as_str(),
                "instrument_id": position.instrument_id.as_str(),
                "quantity": position.quantity,
                "avg_price": position.avg_price,
                "mark_price": position.mark_price,
                "updated_at_ms": position.updated_at_ms,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "checkpoint_id": checkpoint_id,
        "run_id": runtime.run_id(),
        "created_at_ms": observed_at_ms,
        "runtime_status": format!("{:?}", runtime.status()),
        "inventory_json": {
            "free_cash_usd": snapshot.free_cash_usd,
            "reserved_cash_usd": snapshot.reserved_cash_usd,
            "total_cash_usd": snapshot.total_cash_usd,
            "realized_pnl_usd": snapshot.realized_pnl_usd,
            "gross_exposure_usd": snapshot.gross_exposure_usd,
            "positions": positions,
        },
        "open_orders_json": open_orders,
        "pair_state_json": {
            "open_order_count": runtime.open_order_snapshots().len(),
        }
    })
}

fn assert_checkpoint(
    runtime: &Runtime<FixtureStrategy>,
    fixture: &ScenarioFixture,
    expected: &CheckpointExpected,
    at_ms: u64,
) {
    let artifact = checkpoint_artifact(runtime, &format!("{}-{at_ms}", fixture.name), at_ms);
    let artifact_object = artifact.as_object().expect("checkpoint artifact object");
    for key in &fixture.expected.required_checkpoint_keys {
        assert!(
            artifact_object.contains_key(key),
            "missing checkpoint key {key} in {artifact:?}"
        );
    }
    assert_eq!(artifact_object["runtime_status"], expected.runtime_status);

    if let Some(positions) = &expected.inventory.positions {
        let actual_positions = artifact_object["inventory_json"]["positions"]
            .as_array()
            .expect("positions array");
        assert_eq!(actual_positions.len(), positions.len());
    }
}

fn assert_runtime_expectations(
    fixture: &ScenarioFixture,
    _runtime: &Runtime<FixtureStrategy>,
    seen_categories: &BTreeSet<String>,
) {
    assert_eq!(
        format!("{:?}", _runtime.status()),
        fixture.expected.runtime_status
    );
    for required in &fixture.expected.required_event_categories {
        assert!(
            seen_categories.contains(required),
            "missing event category {required} in {seen_categories:?}"
        );
    }
}

fn assert_submit_intent(command: &RuntimeCommand, decision: &FixtureDecision) {
    match command {
        RuntimeCommand::Submit(intent) => {
            assert_eq!(intent.client_order_id.as_str(), decision.client_order_id);
            assert_eq!(intent.instrument_id.as_str(), decision.instrument_id);
            assert_eq!(intent.side, decision.side.into());
            assert!((intent.limit_price - decision.limit_price).abs() < 1e-9);
            assert!((intent.quantity - decision.quantity).abs() < 1e-9);
            assert_eq!(intent.reduce_only, decision.reduce_only);
            assert_eq!(intent.reason, decision.reason);
        }
        other => panic!("expected submit command, got {other:?}"),
    }
}

fn handle_submit_ack(
    runtime: &mut Runtime<FixtureStrategy>,
    command: &RuntimeCommand,
    outcome: &AdapterSubmitOutcome,
    at_ms: u64,
) -> Option<polymarket_exec::runtime::RuntimeOutcome> {
    let RuntimeCommand::Submit(intent) = command else {
        return None;
    };

    match outcome.outcome.as_str() {
        "accepted" => Some(runtime.on_order_opened(&intent.client_order_id, at_ms)),
        "uncertain" => Some(runtime.mark_order_needs_reconcile(
            &intent.client_order_id,
            at_ms,
            "submission uncertain; moving to needs-reconcile",
        )),
        "rejected" => Some(
            runtime.on_order_rejected(
                &intent.client_order_id,
                outcome
                    .reason
                    .clone()
                    .unwrap_or_else(|| "rejected by adapter".to_string()),
                at_ms,
            ),
        ),
        other => panic!("unsupported submit outcome {other}"),
    }
}

fn handle_cancel_ack(
    runtime: &mut Runtime<FixtureStrategy>,
    command: &RuntimeCommand,
    outcome: &AdapterCancelOutcome,
    at_ms: u64,
) -> Option<polymarket_exec::runtime::RuntimeOutcome> {
    let RuntimeCommand::Cancel {
        client_order_id, ..
    } = command
    else {
        return None;
    };

    match outcome.outcome.as_str() {
        "cancelled" => Some(
            runtime.on_order_cancelled(
                client_order_id,
                outcome
                    .reason
                    .clone()
                    .unwrap_or_else(|| "cancelled by adapter".to_string()),
                at_ms,
            ),
        ),
        other => panic!("unsupported cancel outcome {other}"),
    }
}

fn run_fixture(name: &str) {
    let fixture = load_fixture(name);
    let store_path = unique_workspace_path(&format!("polymarket-exec-{name}"), ".sqlite");
    let journal_path = unique_workspace_path(&format!("polymarket-exec-{name}"), ".jsonl");
    let mut journal = JournalWriter::open(&journal_path).unwrap();
    let mut runtime = build_runtime(&fixture, &store_path, fixture.starting_cash_usd);
    let mut last_seq = 0_u64;
    let mut seen_categories = BTreeSet::new();
    let submit_outcomes_by_id = fixture
        .adapter
        .submit
        .iter()
        .map(|outcome| (outcome.client_order_id.clone(), outcome))
        .collect::<HashMap<_, _>>();
    let cancel_outcomes_by_id = fixture
        .adapter
        .cancel
        .iter()
        .map(|outcome| (outcome.client_order_id.clone(), outcome))
        .collect::<HashMap<_, _>>();

    let startup_outcome = runtime.start(1);
    assert!(startup_outcome.commands.is_empty());
    drain_event_log(&runtime, &mut journal, &mut seen_categories, &mut last_seq);

    for event in &fixture.events {
        match event {
            ScenarioEvent::Snapshot {
                at_ms,
                instrument_id,
                market_id,
                best_bid,
                best_ask,
                last_trade_price,
                decisions,
                skip_if_stale,
                ..
            } => {
                if *skip_if_stale {
                    continue;
                }
                let outcome = runtime
                    .on_book_state(
                        MarketId::from(market_id.clone()),
                        InstrumentId::from(instrument_id.clone()),
                        &polymarket_exec::book::BookState::from_top_of_book(
                            instrument_id.clone(),
                            best_bid.price,
                            best_bid.quantity,
                            best_ask.price,
                            best_ask.quantity,
                            *last_trade_price,
                            *at_ms,
                        ),
                    )
                    .expect("snapshot");
                drain_event_log(&runtime, &mut journal, &mut seen_categories, &mut last_seq);
                let decision_lookup = decisions
                    .iter()
                    .map(|decision| (decision.client_order_id.clone(), decision))
                    .collect::<HashMap<_, _>>();
                let mut submitted_decisions = HashSet::new();
                for command in outcome.commands {
                    journal.append_command(&command).unwrap();
                    match &command {
                        RuntimeCommand::Submit(_) => {
                            let submit_client_order_id = match &command {
                                RuntimeCommand::Submit(intent) => {
                                    intent.client_order_id.as_str().to_string()
                                }
                                _ => unreachable!(),
                            };
                            let adapter_outcome = submit_outcomes_by_id
                                .get(&submit_client_order_id)
                                .unwrap_or_else(|| {
                                    panic!(
                                        "missing submit outcome for client_order_id {}",
                                        submit_client_order_id
                                    )
                                });
                            if let Some(decision) = decision_lookup.get(&submit_client_order_id) {
                                assert_submit_intent(&command, decision);
                                submitted_decisions.insert(submit_client_order_id.clone());
                            }
                            let ack_outcome = handle_submit_ack(
                                &mut runtime,
                                &command,
                                adapter_outcome,
                                *at_ms + 1,
                            );
                            if let Some(ack_outcome) = ack_outcome {
                                drain_event_log(
                                    &runtime,
                                    &mut journal,
                                    &mut seen_categories,
                                    &mut last_seq,
                                );
                                for nested in ack_outcome.commands {
                                    journal.append_command(&nested).unwrap();
                                }
                                drain_event_log(
                                    &runtime,
                                    &mut journal,
                                    &mut seen_categories,
                                    &mut last_seq,
                                );
                            } else {
                                drain_event_log(
                                    &runtime,
                                    &mut journal,
                                    &mut seen_categories,
                                    &mut last_seq,
                                );
                            }
                        }
                        RuntimeCommand::Cancel {
                            client_order_id, ..
                        } => {
                            let adapter_outcome = cancel_outcomes_by_id
                                .get(client_order_id.as_str())
                                .unwrap_or_else(|| {
                                    panic!(
                                        "missing cancel outcome for client_order_id {}",
                                        client_order_id
                                    )
                                });
                            let ack_outcome = handle_cancel_ack(
                                &mut runtime,
                                &command,
                                adapter_outcome,
                                *at_ms + 1,
                            );
                            if let Some(ack_outcome) = ack_outcome {
                                drain_event_log(
                                    &runtime,
                                    &mut journal,
                                    &mut seen_categories,
                                    &mut last_seq,
                                );
                                for nested in ack_outcome.commands {
                                    journal.append_command(&nested).unwrap();
                                }
                                drain_event_log(
                                    &runtime,
                                    &mut journal,
                                    &mut seen_categories,
                                    &mut last_seq,
                                );
                            } else {
                                drain_event_log(
                                    &runtime,
                                    &mut journal,
                                    &mut seen_categories,
                                    &mut last_seq,
                                );
                            }
                        }
                        RuntimeCommand::Noop => {
                            drain_event_log(
                                &runtime,
                                &mut journal,
                                &mut seen_categories,
                                &mut last_seq,
                            );
                        }
                        RuntimeCommand::Merge(_) | RuntimeCommand::Redeem(_) => {
                            drain_event_log(
                                &runtime,
                                &mut journal,
                                &mut seen_categories,
                                &mut last_seq,
                            );
                        }
                    }
                }
                if submitted_decisions.len() != decisions.len() {
                    assert_eq!(
                        submitted_decisions.len(),
                        decisions.len(),
                        "not all decisions were submitted"
                    );
                }
            }
            ScenarioEvent::Fill {
                at_ms,
                client_order_id,
                market_id,
                instrument_id,
                side,
                price,
                quantity,
                fee_usd,
                liquidity,
                close_method,
            } => {
                let fill = FillReport {
                    order_id: None,
                    client_order_id: Some(ClientOrderId::from(client_order_id.clone())),
                    market_id: MarketId::from(market_id.clone()),
                    instrument_id: InstrumentId::from(instrument_id.clone()),
                    side: (*side).into(),
                    price: *price,
                    quantity: *quantity,
                    fee_usd: *fee_usd,
                    liquidity: (*liquidity).into(),
                    close_method: close_method.as_deref().map(CloseMethod::from_raw),
                    observed_at_ms: *at_ms,
                };
                let outcome = runtime.on_fill(fill).expect("fill");
                drain_event_log(&runtime, &mut journal, &mut seen_categories, &mut last_seq);
                for command in outcome.commands {
                    journal.append_command(&command).unwrap();
                }
                drain_event_log(&runtime, &mut journal, &mut seen_categories, &mut last_seq);
            }
            ScenarioEvent::Cancel {
                at_ms,
                client_order_id,
                reason,
            } => {
                let coid = ClientOrderId::from(client_order_id.clone());
                let outcome = runtime.on_order_cancelled(
                    &coid,
                    if reason.is_empty() { "scenario cancel" } else { reason.as_str() }
                        .to_string(),
                    *at_ms,
                );
                drain_event_log(&runtime, &mut journal, &mut seen_categories, &mut last_seq);
                for command in outcome.commands {
                    journal.append_command(&command).unwrap();
                }
                drain_event_log(&runtime, &mut journal, &mut seen_categories, &mut last_seq);
            }
            ScenarioEvent::PaperMarketClose {
                at_ms,
                resolution_price,
            } => {
                let outcome = runtime.plan_paper_close(*at_ms, *resolution_price);
                drain_event_log(&runtime, &mut journal, &mut seen_categories, &mut last_seq);
                for command in outcome.commands {
                    journal.append_command(&command).unwrap();
                }
                drain_event_log(&runtime, &mut journal, &mut seen_categories, &mut last_seq);
            }
            ScenarioEvent::Crash { at_ms: _, label: _ } => {}
            ScenarioEvent::Restart {
                at_ms,
                starting_cash_usd,
            } => {
                runtime = build_runtime(&fixture, &store_path, *starting_cash_usd);
                let startup_outcome = runtime.start(*at_ms);
                assert!(startup_outcome.commands.is_empty());
                last_seq = 0;
                drain_event_log(&runtime, &mut journal, &mut seen_categories, &mut last_seq);
                let recover_outcome = runtime.recover_from_store(*at_ms, 5_000);
                for command in recover_outcome.commands {
                    journal.append_command(&command).unwrap();
                }
                drain_event_log(&runtime, &mut journal, &mut seen_categories, &mut last_seq);
            }
            ScenarioEvent::Reconcile {
                at_ms,
                label: _,
                expected,
            } => {
                let outcome = runtime.reconcile_open_orders(*at_ms, 5_000);
                for command in outcome.commands {
                    journal.append_command(&command).unwrap();
                }
                drain_event_log(&runtime, &mut journal, &mut seen_categories, &mut last_seq);

                let open_orders = runtime.open_order_snapshots();
                let local_ids = open_orders
                    .iter()
                    .map(|order| order.intent.client_order_id.as_str().to_string())
                    .collect::<BTreeSet<_>>();

                for client_order_id in &expected.local_only {
                    assert!(
                        local_ids.contains(client_order_id),
                        "expected local_only order {client_order_id} to remain in memory"
                    );
                }

                for client_order_id in &expected.needs_reconcile {
                    assert!(
                        local_ids.contains(client_order_id),
                        "expected {client_order_id} to remain open for reconciliation"
                    );
                }

                if let Some(sync) = &fixture.adapter.sync_open_orders {
                    let venue_ids = sync
                        .open_orders
                        .iter()
                        .map(|order| order.client_order_id.as_str().to_string())
                        .collect::<BTreeSet<_>>();
                    for client_order_id in &expected.venue_only {
                        assert!(
                            venue_ids.contains(client_order_id),
                            "expected venue_only order {client_order_id} in adapter sync"
                        );
                    }
                }

                if fixture.name == "uncertain_submit" {
                    let open_order = open_orders.first().expect("uncertain submit open order");
                    assert_eq!(open_order.status, ManagedOrderStatus::NeedsReconcile);
                }

                if fixture.name == "reconnect_partial_fills" {
                    let synced = fixture
                        .adapter
                        .sync_open_orders
                        .as_ref()
                        .expect("sync open orders");
                    let local = open_orders.first().expect("local open order");
                    assert!(
                        (local.remaining_qty() - synced.open_orders[0].remaining_qty).abs() < 1e-9
                    );
                }

                if fixture.name == "replay_reconcile_merge_recovery" {
                    let synced = fixture
                        .adapter
                        .sync_open_orders
                        .as_ref()
                        .expect("sync open orders");
                    let local = open_orders.first().expect("local open order");
                    assert_eq!(local.intent.client_order_id.as_str(), "merge-replay-1");
                    assert_eq!(synced.open_orders[0].client_order_id, "merge-replay-1");
                }
            }
            ScenarioEvent::Checkpoint {
                at_ms,
                label: _,
                expected,
            } => {
                assert_checkpoint(&runtime, &fixture, expected, *at_ms);
                let checkpoint_line =
                    checkpoint_artifact(&runtime, &format!("{}-{}", fixture.name, at_ms), *at_ms);
                assert!(checkpoint_line
                    .as_object()
                    .unwrap()
                    .contains_key("inventory_json"));
                let journal_checkpoint_name = format!("checkpoint-{}-{}", fixture.name, at_ms);
                let checkpoint_path = journal_path_for_checkpoint(&fixture.name, *at_ms);
                let mut checkpoint_journal = JournalWriter::open(checkpoint_path)
                    .unwrap_or_else(|error| panic!("failed to open checkpoint journal: {error}"));
                checkpoint_journal
                    .append_checkpoint(
                        *at_ms,
                        runtime.run_id(),
                        &journal_checkpoint_name,
                        runtime.open_order_snapshots().len(),
                        runtime
                            .open_order_snapshots()
                            .iter()
                            .filter(|order| order.status == ManagedOrderStatus::NeedsReconcile)
                            .count(),
                        runtime.event_log().latest_seq(),
                    )
                    .unwrap();
                checkpoint_journal.flush().unwrap();
                drain_event_log(&runtime, &mut journal, &mut seen_categories, &mut last_seq);
            }
            ScenarioEvent::JournalAssert {
                min_lines,
                must_contain_kinds,
            } => {
                journal.flush().unwrap();
                let contents = fs::read_to_string(&journal_path).unwrap();
                let lines = contents.lines().collect::<Vec<_>>();
                assert!(
                    lines.len() >= *min_lines,
                    "expected at least {min_lines} journal lines, got {}",
                    lines.len()
                );
                let kinds = lines
                    .iter()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .filter_map(|value| {
                        value
                            .get("kind")
                            .and_then(Value::as_str)
                            .map(|kind| kind.to_string())
                    })
                    .collect::<BTreeSet<_>>();
                for kind in must_contain_kinds {
                    assert!(
                        kinds.contains(kind),
                        "expected journal kind {kind} in {kinds:?}"
                    );
                }
            }
        }
    }

    drain_event_log(&runtime, &mut journal, &mut seen_categories, &mut last_seq);
    assert_runtime_expectations(&fixture, &runtime, &seen_categories);

    let _ = fs::remove_file(&journal_path);
    let _ = fs::remove_file(&store_path);
}

fn journal_path_for_checkpoint(name: &str, at_ms: u64) -> PathBuf {
    unique_workspace_path(&format!("{name}-checkpoint-{at_ms}"), ".jsonl")
}

#[test]
fn adapter_lifecycle_submit_ack_fill_and_cancel() {
    run_fixture("one_sided_fill_reversal");
}

#[test]
fn clean_passive_fills_replay() {
    run_fixture("clean_passive_fills");
}

#[test]
fn one_sided_fill_reversal_replay() {
    run_fixture("one_sided_fill_reversal");
}

#[test]
fn uncertain_submit_reconcile() {
    run_fixture("uncertain_submit");
}

#[test]
fn stale_book_risk_gate() {
    run_fixture("stale_book");
}

#[test]
fn reconnect_after_partial_fills_recovery() {
    run_fixture("reconnect_partial_fills");
}

#[test]
fn end_of_window_cleanup() {
    run_fixture("end_of_window_cleanup");
}

#[test]
fn replay_reconcile_merge_recovery() {
    run_fixture("replay_reconcile_merge_recovery");
}

#[test]
fn unlawful_entry_window_valid_core_entry_and_hedge_probe() {
    run_fixture("unlawful_entry_window");
}

#[test]
fn unlawful_regime_closed_no_buy_intents() {
    run_fixture("unlawful_regime_closed");
}

#[test]
fn unlawful_merge_stall_drives_cleanup_only_actions() {
    run_fixture("unlawful_merge_stall_cleanup");
}

#[test]
fn unlawful_late_window_only_reduce_only_cleanup() {
    run_fixture("unlawful_late_window_cleanup_only");
}

#[test]
fn late_fill_after_cancel_applies_via_terminal_fill_correction() {
    run_fixture("late_fill_after_cancel");
}

#[test]
fn paper_market_close_with_redeem_settles_stranded_inventory() {
    run_fixture("paper_market_close_with_redeem");
}
