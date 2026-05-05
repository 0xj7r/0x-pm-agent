use super::{ManagedOrderStatus, Runtime, RuntimeConfig};
use crate::inventory::VenuePositionSnapshot;
use crate::market_context::{MarketContextRecord, MarketContextStore};
use crate::risk::RiskLimits;
use crate::runtime::order_store::{OrderRecord, OrderStore, SqliteOrderStore};
use crate::strategy::{
    NoopStrategy, Strategy, StrategyContext, StrategyDecision, VenueMarketRules,
};
use crate::types::{
    BookLevel, ClientOrderId, CloseMethod, FillLiquidity, FillReport, InstrumentId, MarketId,
    MarketSnapshot, MergeIntent, OrderIntent, QuoteSnapshot, RuntimeCommand, RuntimeStatus,
    TradeSide,
};

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
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
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
        })
    }
}

struct PassiveSingleShotStrategy {
    fired: bool,
}

impl Strategy for PassiveSingleShotStrategy {
    fn name(&self) -> &str {
        "passive-single-shot"
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
            limit_price: snapshot.quote.best_bid.as_ref().unwrap().price,
            quantity: 10.0,
            reduce_only: false,
            reason: "enter-passive".into(),
            quote_level_tag: None,
            created_at_ms: snapshot.quote.observed_at_ms,
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
        })
    }
}

struct StatefulTestStrategy {
    restored: Arc<AtomicBool>,
    state: serde_json::Value,
}

impl Strategy for StatefulTestStrategy {
    fn name(&self) -> &str {
        "stateful-test"
    }

    fn checkpoint_state(&self) -> Option<serde_json::Value> {
        Some(self.state.clone())
    }

    fn restore_checkpoint_state(
        &mut self,
        state: &serde_json::Value,
    ) -> std::result::Result<(), String> {
        if state.get("marker").and_then(|value| value.as_str()) == Some("persisted") {
            self.restored.store(true, Ordering::SeqCst);
            Ok(())
        } else {
            Err("missing persisted marker".to_string())
        }
    }
}

struct FillRescueStrategy {
    rescue: OrderIntent,
}

impl Strategy for FillRescueStrategy {
    fn name(&self) -> &str {
        "fill-rescue"
    }

    fn on_fill(&mut self, _context: &StrategyContext, fill: &FillReport) -> StrategyDecision {
        assert_eq!(fill.instrument_id, InstrumentId::from("up"));
        StrategyDecision::reactive(
            vec![self.rescue.clone()],
            vec!["on-fill IOC rescue emitted".to_string()],
        )
    }
}

struct SnapshotRescueStrategy {
    rescue: OrderIntent,
    fired: bool,
}

impl Strategy for SnapshotRescueStrategy {
    fn name(&self) -> &str {
        "snapshot-rescue"
    }

    fn on_market_snapshot(
        &mut self,
        _context: &StrategyContext,
        _snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        if self.fired {
            return StrategyDecision::reactive(Vec::new(), Vec::new());
        }
        self.fired = true;
        StrategyDecision::reactive(
            vec![self.rescue.clone()],
            vec!["snapshot rescue".to_string()],
        )
    }
}

struct MergeCommandStrategy;

impl Strategy for MergeCommandStrategy {
    fn name(&self) -> &str {
        "merge-command-test"
    }

    fn on_start(&mut self, _context: &StrategyContext) -> StrategyDecision {
        StrategyDecision::commands(
            vec![RuntimeCommand::Merge(MergeIntent {
                command_id: ClientOrderId::from("merge-1"),
                market_id: MarketId::from("market-1"),
                condition_id: Some("condition-1".to_string()),
                yes_instrument_id: InstrumentId::from("yes"),
                no_instrument_id: InstrumentId::from("no"),
                quantity: 3.0,
                expected_cash_usd: 3.0,
                expected_cost_usd: 2.85,
                expected_fee_usd: 0.0,
                expected_gas_usd: 0.0,
                reason: "test merge command".to_string(),
                created_at_ms: 1,
            })],
            vec!["merge command emitted".to_string()],
        )
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
        PassiveSingleShotStrategy { fired: false },
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
            taker_buy_qty_60s: 0.0,
            taker_sell_qty_60s: 0.0,
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
    assert!((runtime.inventory().free_cash_usd() - 96.1).abs() < 1e-9);
    assert_eq!(runtime.open_orders().count(), 1);

    runtime
        .on_fill(FillReport {
            order_id: None,
            client_order_id: Some(ClientOrderId::from("client-1")),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            price: 0.39,
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
    assert!((runtime.inventory().free_cash_usd() - 96.0).abs() < 1e-9);
}

#[test]
fn strategy_commands_emit_merge_without_quote_reconciliation() {
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Starting,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        MergeCommandStrategy,
        MarketContextStore::empty(),
    );

    let started = runtime.start(1);
    assert_eq!(started.commands.len(), 1);
    match &started.commands[0] {
        RuntimeCommand::Merge(intent) => {
            assert_eq!(intent.market_id, MarketId::from("market-1"));
            assert_eq!(intent.quantity, 3.0);
        }
        other => panic!("expected merge command, got {other:?}"),
    }
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
        PassiveSingleShotStrategy { fired: false },
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
            taker_buy_qty_60s: 0.0,
            taker_sell_qty_60s: 0.0,
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
fn durable_dust_fill_terminal_removes_active_order_before_cancel() {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("polymarket-exec-dust-fill-{ts}.sqlite"));
    let store = SqliteOrderStore::open(&path).unwrap();
    let client_order_id = ClientOrderId::from("client-1");

    let mut runtime = Runtime::new_with_order_store(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Starting,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        PassiveSingleShotStrategy { fired: false },
        MarketContextStore::empty(),
        Some(Box::new(store)),
        "run-dust-fill".to_string(),
    );

    runtime.start(1);
    let quote_outcome = runtime
        .on_market_snapshot(MarketSnapshot {
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new(0.39, 100.0)),
                best_ask: Some(BookLevel::new(0.40, 100.0)),
                bid_levels: vec![BookLevel::new(0.39, 100.0)],
                ask_levels: vec![BookLevel::new(0.40, 100.0)],
                depth_observed_at_ms: Some(2),
                last_trade_price: Some(0.40),
                taker_buy_qty_60s: 0.0,
                taker_sell_qty_60s: 0.0,
                observed_at_ms: 2,
            },
        })
        .expect("quote");
    assert_eq!(
        quote_outcome.commands.len(),
        1,
        "events: {:?}",
        runtime
            .event_log()
            .recent(8)
            .iter()
            .map(|event| event.message.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(runtime.open_orders().count(), 1);
    runtime.on_order_opened(&client_order_id, 3);

    runtime
        .on_fill(FillReport {
            order_id: None,
            client_order_id: Some(client_order_id.clone()),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            price: 0.40,
            quantity: 9.995,
            fee_usd: 0.0,
            liquidity: FillLiquidity::Maker,
            close_method: None,
            observed_at_ms: 4,
        })
        .expect("fill");

    assert!(
        runtime.open_order_snapshots().is_empty(),
        "durable dust-complete fill must remove stale active order before quote churn can cancel it"
    );
    let cancel_outcome = runtime.request_cancel(&client_order_id, "desired changed", 5);
    assert!(
        cancel_outcome.commands.is_empty(),
        "terminal durable fill must not emit a venue cancel"
    );

    let record = SqliteOrderStore::open(&path)
        .unwrap()
        .get(&client_order_id)
        .unwrap()
        .expect("order record");
    assert_eq!(record.status, ManagedOrderStatus::Filled);
    assert_eq!(record.remaining_qty, 0.0);
    let _ = std::fs::remove_file(path);
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
fn venue_position_reconciliation_uses_active_market_id_for_token_inventory() {
    let market_contexts = MarketContextStore::from_records(
        vec![MarketContextRecord {
            market_id: "2099163".to_string(),
            instrument_ids: vec!["down-token".to_string(), "up-token".to_string()],
            ..MarketContextRecord::default()
        }],
        Some("test".to_string()),
        Some(10),
    );
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        NoopStrategy,
        market_contexts,
    );

    runtime
        .reconcile_venue_positions(
            &[VenuePositionSnapshot {
                // Polymarket Data API can report condition id as the
                // market key; strategy book snapshots use the CLOB market
                // id. Runtime inventory must use the active CLOB market id
                // so strategy exposure gates count venue-held inventory.
                market_id: MarketId::from(
                    "0xfb04b40894b43ad326be88f91e66cfb5d65dd31bed8dd85384404f8b2a23fe4f",
                ),
                condition_id: Some(
                    "0xfb04b40894b43ad326be88f91e66cfb5d65dd31bed8dd85384404f8b2a23fe4f"
                        .to_string(),
                ),
                instrument_id: InstrumentId::from("down-token"),
                quantity: 45.0,
                average_cost_usd: 0.3644,
                mark_price: Some(0.03),
                observed_at_ms: 10,
            }],
            11,
        )
        .expect("runtime venue reconciliation");

    let position = runtime
        .inventory()
        .positions()
        .find(|position| position.instrument_id == InstrumentId::from("down-token"))
        .expect("venue position should be installed");
    assert_eq!(position.market_id, MarketId::from("2099163"));
    assert_eq!(
        runtime.stranded_inventory()[0].market_id,
        MarketId::from("2099163")
    );
}

#[test]
fn venue_fill_uses_active_market_id_for_token_inventory() {
    let market_contexts = MarketContextStore::from_records(
        vec![MarketContextRecord {
            market_id: "2099163".to_string(),
            instrument_ids: vec!["down-token".to_string(), "up-token".to_string()],
            ..MarketContextRecord::default()
        }],
        Some("test".to_string()),
        Some(10),
    );
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        NoopStrategy,
        market_contexts,
    );

    runtime
        .on_fill(FillReport {
            order_id: None,
            client_order_id: None,
            market_id: MarketId::from(
                "0xfb04b40894b43ad326be88f91e66cfb5d65dd31bed8dd85384404f8b2a23fe4f",
            ),
            instrument_id: InstrumentId::from("down-token"),
            side: TradeSide::Buy,
            price: 0.21,
            quantity: 5.0,
            fee_usd: 0.0,
            liquidity: FillLiquidity::Maker,
            close_method: None,
            observed_at_ms: 11,
        })
        .expect("fill");

    let position = runtime
        .inventory()
        .positions()
        .find(|position| position.instrument_id == InstrumentId::from("down-token"))
        .expect("venue fill should be installed");
    assert_eq!(position.market_id, MarketId::from("2099163"));
    assert_eq!(
        runtime.stranded_inventory()[0].market_id,
        MarketId::from("2099163")
    );
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
fn sub_venue_min_single_leg_dust_is_not_actionable_inventory() {
    let market_id = MarketId::from("market-mm");
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
    runtime.set_venue_market_rules(
        market_id.clone(),
        VenueMarketRules {
            minimum_order_size: 5.0,
            minimum_tick_size: 0.01,
            neg_risk: false,
        },
    );

    runtime
        .reconcile_venue_positions(
            &[VenuePositionSnapshot {
                market_id: market_id.clone(),
                condition_id: None,
                instrument_id: InstrumentId::from("up"),
                quantity: 0.1882,
                average_cost_usd: 0.88,
                mark_price: Some(0.15),
                observed_at_ms: 10,
            }],
            11,
        )
        .expect("runtime venue reconciliation");

    assert!(
        !runtime.market_has_inventory(&market_id),
        "sub-min single-leg residual should stay in accounting but not pin inventory mode"
    );
}

#[test]
fn plan_merge_respects_profile_min_merge_notional() {
    let market_id = MarketId::from("market-mm");
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            min_merge_notional_usd: 2.0,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        NoopStrategy,
        MarketContextStore::empty(),
    );
    // Seed two tiny paired legs ($1.50 paired notional ~ 1.5 paired qty).
    // The configured merge-notional gate is a dust/noise control, not a
    // Polygon gas-friction rule.
    runtime
        .on_fill(FillReport {
            order_id: None,
            client_order_id: None,
            market_id: market_id.clone(),
            instrument_id: InstrumentId::from("up"),
            side: TradeSide::Buy,
            price: 0.30,
            quantity: 1.50,
            fee_usd: 0.0,
            liquidity: FillLiquidity::Maker,
            close_method: None,
            observed_at_ms: 10,
        })
        .expect("up leg");
    runtime
        .on_fill(FillReport {
            order_id: None,
            client_order_id: None,
            market_id: market_id.clone(),
            instrument_id: InstrumentId::from("down"),
            side: TradeSide::Buy,
            price: 0.65,
            quantity: 1.50,
            fee_usd: 0.0,
            liquidity: FillLiquidity::Maker,
            close_method: None,
            observed_at_ms: 11,
        })
        .expect("down leg");

    let outcome = runtime.plan_merge_command_for_market(
        &market_id,
        12,
        "test trigger after paired inventory fills",
    );

    assert!(
        outcome.commands.is_empty(),
        "tiny paired inventory should respect the configured min merge notional. \
             Got commands: {:?}",
        outcome.commands.len()
    );
    assert!(
        runtime
            .event_log()
            .recent(20)
            .iter()
            .any(|event| event.message.contains("below min merge notional")),
        "merge skip should emit a recognizable event for observability"
    );
}

#[test]
fn plan_merge_allows_tiny_paired_inventory_when_min_merge_notional_is_zero() {
    let market_id = MarketId::from("market-mm");
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            min_merge_notional_usd: 0.0,
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
            market_id: market_id.clone(),
            instrument_id: InstrumentId::from("up"),
            side: TradeSide::Buy,
            price: 0.30,
            quantity: 1.50,
            fee_usd: 0.0,
            liquidity: FillLiquidity::Maker,
            close_method: None,
            observed_at_ms: 10,
        })
        .expect("up leg");
    let outcome = runtime
        .on_fill(FillReport {
            order_id: None,
            client_order_id: None,
            market_id: market_id.clone(),
            instrument_id: InstrumentId::from("down"),
            side: TradeSide::Buy,
            price: 0.65,
            quantity: 1.50,
            fee_usd: 0.0,
            liquidity: FillLiquidity::Maker,
            close_method: None,
            observed_at_ms: 11,
        })
        .expect("down leg");

    assert_eq!(outcome.commands.len(), 1);
}

#[test]
fn plan_merge_fires_normally_for_substantial_paired_inventory() {
    let market_id = MarketId::from("market-mm");
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
    // Paired qty = 6.5 -> $6.50 release.
    runtime
        .on_fill(FillReport {
            order_id: None,
            client_order_id: None,
            market_id: market_id.clone(),
            instrument_id: InstrumentId::from("up"),
            side: TradeSide::Buy,
            price: 0.30,
            quantity: 6.5,
            fee_usd: 0.0,
            liquidity: FillLiquidity::Maker,
            close_method: None,
            observed_at_ms: 10,
        })
        .expect("up leg");
    let second_outcome = runtime
        .on_fill(FillReport {
            order_id: None,
            client_order_id: None,
            market_id: market_id.clone(),
            instrument_id: InstrumentId::from("down"),
            side: TradeSide::Buy,
            price: 0.65,
            quantity: 6.5,
            fee_usd: 0.0,
            liquidity: FillLiquidity::Maker,
            close_method: None,
            observed_at_ms: 11,
        })
        .expect("down leg");

    // The second leg's on_fill should auto-plan the merge for substantial
    // paired inventory.
    assert!(
        second_outcome
            .commands
            .iter()
            .any(|cmd| matches!(cmd, RuntimeCommand::Merge(_))),
        "$6.50 paired notional should fire merge on second leg fill (gas friction ~5%, within threshold). \
             Got commands: {:?}",
        second_outcome.commands.len()
    );
}

#[test]
fn blocked_merge_waits_for_inventory_change_not_timer_backoff() {
    let market_id = MarketId::from("market-mm");
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
            market_id: market_id.clone(),
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
            market_id: market_id.clone(),
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
    runtime.block_pending_merge(&market_id, 12, "ctf revert");

    let suppressed = runtime.plan_merge_command_for_market(&market_id, 13, "retry too soon");
    assert!(
        suppressed.commands.is_empty(),
        "identical merge should still be suppressed during short backoff"
    );

    let still_blocked = runtime.plan_merge_command_for_market(&market_id, 30_000, "timer retry");
    assert!(
        still_blocked.commands.is_empty(),
        "identical reverted merge should not retry just because time passed"
    );

    runtime
        .reconcile_venue_positions(
            &[
                VenuePositionSnapshot {
                    market_id: market_id.clone(),
                    condition_id: Some("condition-1".to_string()),
                    instrument_id: InstrumentId::from("up"),
                    quantity: 5.0,
                    average_cost_usd: 0.20,
                    mark_price: Some(0.20),
                    observed_at_ms: 31_000,
                },
                VenuePositionSnapshot {
                    market_id: market_id.clone(),
                    condition_id: Some("condition-1".to_string()),
                    instrument_id: InstrumentId::from("down"),
                    quantity: 5.0,
                    average_cost_usd: 0.70,
                    mark_price: Some(0.70),
                    observed_at_ms: 31_000,
                },
            ],
            31_000,
        )
        .expect("inventory-changing reconcile");

    let retry = runtime.plan_merge_command_for_market(&market_id, 31_001, "retry after reconcile");
    assert!(
        retry
            .commands
            .iter()
            .any(|command| matches!(command, RuntimeCommand::Merge(_))),
        "blocked merge must not be suppressed forever"
    );
}

#[test]
fn accepted_merge_latch_clears_after_post_accept_venue_reconcile() {
    let market_id = MarketId::from("market-mm");
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
            market_id: market_id.clone(),
            instrument_id: InstrumentId::from("up"),
            side: TradeSide::Buy,
            price: 0.20,
            quantity: 5.0,
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
            market_id: market_id.clone(),
            instrument_id: InstrumentId::from("down"),
            side: TradeSide::Buy,
            price: 0.70,
            quantity: 5.0,
            fee_usd: 0.0,
            liquidity: FillLiquidity::Maker,
            close_method: None,
            observed_at_ms: 11,
        })
        .expect("second leg");

    runtime.mark_pending_merge_accepted(&market_id, 12);
    let duplicate_before_reconcile =
        runtime.plan_merge_command_for_market(&market_id, 13, "duplicate before reconcile");
    assert!(
        duplicate_before_reconcile.commands.is_empty(),
        "identical merge should still be suppressed before a post-accept venue reconcile"
    );

    runtime
        .reconcile_venue_positions(
            &[
                VenuePositionSnapshot {
                    market_id: market_id.clone(),
                    condition_id: Some("condition-1".to_string()),
                    instrument_id: InstrumentId::from("up"),
                    quantity: 5.0,
                    average_cost_usd: 0.20,
                    mark_price: Some(0.20),
                    observed_at_ms: 14,
                },
                VenuePositionSnapshot {
                    market_id: market_id.clone(),
                    condition_id: Some("condition-1".to_string()),
                    instrument_id: InstrumentId::from("down"),
                    quantity: 5.0,
                    average_cost_usd: 0.70,
                    mark_price: Some(0.70),
                    observed_at_ms: 14,
                },
            ],
            14,
        )
        .expect("venue reconcile after accepted merge");

    let fresh_merge =
        runtime.plan_merge_command_for_market(&market_id, 15, "remaining paired inventory");
    assert!(
        fresh_merge
            .commands
            .iter()
            .any(|command| matches!(command, RuntimeCommand::Merge(_))),
        "post-accept venue reconcile must clear the accepted latch so remaining paired inventory can merge"
    );
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
            pair_id: None,
            kind: crate::types::IntentKind::Close,
        },
        12,
    );

    assert!(outcome.commands.is_empty());
    assert!(runtime
        .open_orders()
        .all(|managed| managed.intent.client_order_id != ClientOrderId::from("cleanup-sell-1")));
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
    let kind = if level.starts_with("mm-hedge-rescue") {
        crate::types::IntentKind::Close
    } else {
        crate::types::IntentKind::Entry
    };
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
        pair_id: None,
        kind,
    }
}

#[test]
fn on_fill_rescue_preserves_working_pair_mate_quote() {
    let left = btc_mm_intent("market-mm", "up", "mm-paired-bid:l1", 0.48);
    let left_client_order_id = left.client_order_id.clone();
    let right = btc_mm_intent("market-mm", "down", "mm-paired-bid:l1", 0.48);
    let right_client_order_id = right.client_order_id.clone();
    let rescue = btc_mm_intent("market-mm", "down", "mm-hedge-rescue:l1", 0.53);
    let rescue_client_order_id = rescue.client_order_id.clone();
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        FillRescueStrategy { rescue },
        MarketContextStore::empty(),
    );

    assert_eq!(runtime.accept_intent(left, 1).commands.len(), 1);
    assert_eq!(runtime.accept_intent(right, 1).commands.len(), 1);

    let outcome = runtime
        .on_fill(FillReport {
            order_id: None,
            client_order_id: Some(left_client_order_id),
            market_id: MarketId::from("market-mm"),
            instrument_id: InstrumentId::from("up"),
            side: TradeSide::Buy,
            price: 0.48,
            quantity: 6.5,
            fee_usd: 0.0,
            liquidity: FillLiquidity::Maker,
            close_method: None,
            observed_at_ms: 2,
        })
        .expect("left fill should trigger rescue");

    assert_eq!(
        outcome
            .commands
            .iter()
            .filter(|command| matches!(command, RuntimeCommand::Submit(_)))
            .count(),
        1
    );
    assert!(
        outcome
            .commands
            .iter()
            .all(|command| !matches!(command, RuntimeCommand::Cancel { .. })),
        "on-fill rescue must not cancel the still-working paired mate"
    );
    assert!(runtime
        .open_orders()
        .any(|managed| managed.intent.client_order_id == right_client_order_id));
    assert!(runtime
        .open_orders()
        .any(|managed| managed.intent.client_order_id == rescue_client_order_id));
}

#[test]
fn on_market_snapshot_rescue_preserves_working_pair_mate_quote() {
    let left = btc_mm_intent("market-mm", "up", "mm-paired-bid:l1", 0.48);
    let left_client_order_id = left.client_order_id.clone();
    let right = btc_mm_intent("market-mm", "down", "mm-paired-bid:l1", 0.52);
    let right_client_order_id = right.client_order_id.clone();
    let rescue = btc_mm_intent("market-mm", "down", "mm-hedge-rescue:l1", 0.53);
    let rescue_client_order_id = rescue.client_order_id.clone();
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        SnapshotRescueStrategy {
            rescue,
            fired: false,
        },
        MarketContextStore::empty(),
    );

    assert_eq!(runtime.accept_intent(left, 1).commands.len(), 1);
    assert_eq!(runtime.accept_intent(right, 1).commands.len(), 1);

    let snapshot = MarketSnapshot {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quote: QuoteSnapshot {
            best_bid: Some(BookLevel::new(0.47, 4.0)),
            best_ask: Some(BookLevel::new(0.48, 4.0)),
            bid_levels: vec![BookLevel::new(0.47, 4.0)],
            ask_levels: vec![BookLevel::new(0.48, 4.0)],
            depth_observed_at_ms: Some(1),
            last_trade_price: Some(0.48),
            taker_buy_qty_60s: 0.0,
            taker_sell_qty_60s: 0.0,
            observed_at_ms: 2,
        },
    };

    let outcome = runtime
        .on_market_snapshot(snapshot)
        .expect("snapshot rescue should submit");

    assert_eq!(
        outcome
            .commands
            .iter()
            .filter(|command| matches!(command, RuntimeCommand::Submit(_)))
            .count(),
        1
    );
    assert!(
        outcome.commands.iter().all(|command| {
            !matches!(
                command,
                RuntimeCommand::Cancel {
                    client_order_id: _,
                    reason: _
                }
            )
        }),
        "snapshot rescue must not cancel the still-working paired mate"
    );
    assert!(runtime
        .open_orders()
        .any(|managed| managed.intent.client_order_id == left_client_order_id));
    assert!(runtime
        .open_orders()
        .any(|managed| managed.intent.client_order_id == right_client_order_id));
    assert!(runtime
        .open_orders()
        .any(|managed| managed.intent.client_order_id == rescue_client_order_id));
}

#[test]
fn degraded_runtime_suppresses_entries_but_accepts_close_intents() {
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Degraded,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        NoopStrategy,
        MarketContextStore::empty(),
    );

    let entry = runtime.accept_intent(btc_mm_intent("market-mm", "up", "mm-paired-bid", 0.44), 1);
    assert!(entry.commands.is_empty());

    let close = runtime.accept_intent(
        btc_mm_intent("market-mm", "down", "mm-hedge-rescue:l1", 0.55),
        2,
    );
    assert_eq!(close.commands.len(), 1);
    assert_eq!(runtime.open_orders().count(), 1);
    assert!(runtime
        .open_orders()
        .all(|managed| managed.intent.kind == crate::types::IntentKind::Close));
}

#[test]
fn live_runtime_suppresses_entries_until_initial_position_reconcile() {
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            require_initial_reconcile_before_entry: true,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        NoopStrategy,
        MarketContextStore::empty(),
    );

    let early_entry =
        runtime.accept_intent(btc_mm_intent("market-mm", "up", "mm-paired-bid", 0.44), 1);
    assert!(early_entry.commands.is_empty());
    assert_eq!(runtime.open_orders().count(), 0);
    assert!(runtime.event_log().recent(4).iter().any(|event| {
        event
            .message
            .contains("initial venue position reconcile has not completed")
    }));

    let rescue = runtime.accept_intent(
        btc_mm_intent("market-mm", "down", "mm-hedge-rescue:l1", 0.55),
        2,
    );
    assert_eq!(rescue.commands.len(), 1);

    runtime
        .reconcile_venue_positions(&[], 3)
        .expect("initial empty venue reconcile");

    let entry = runtime.accept_intent(btc_mm_intent("market-mm", "up", "mm-paired-bid", 0.44), 4);
    assert_eq!(entry.commands.len(), 1);
}

#[test]
fn capital_guard_riskoff_cancels_entries_without_canceling_close_orders() {
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

    let entry_order = btc_mm_intent("market-mm", "up", "mm-paired-bid", 0.44);
    let entry_id = entry_order.client_order_id.clone();
    let close_order = btc_mm_intent("market-mm", "down", "mm-hedge-rescue:l1", 0.55);
    let close_id = close_order.client_order_id.clone();

    assert_eq!(runtime.accept_intent(entry_order, 1).commands.len(), 1);
    assert_eq!(runtime.accept_intent(close_order, 2).commands.len(), 1);

    let outcome = runtime.riskoff_and_cancel_entry_orders(3, "capital guard test");
    assert_eq!(runtime.status(), RuntimeStatus::RiskOff);
    assert!(outcome.commands.iter().any(|command| matches!(
        command,
        RuntimeCommand::Cancel { client_order_id, .. } if client_order_id == &entry_id
    )));
    assert!(!outcome.commands.iter().any(|command| matches!(
        command,
        RuntimeCommand::Cancel { client_order_id, .. } if client_order_id == &close_id
    )));

    let statuses = runtime
        .open_orders()
        .map(|managed| (managed.intent.client_order_id.clone(), managed.status))
        .collect::<HashMap<_, _>>();
    assert_eq!(
        statuses.get(&entry_id),
        Some(&ManagedOrderStatus::CancelRequested)
    );
    assert_eq!(
        statuses.get(&close_id),
        Some(&ManagedOrderStatus::PendingSubmit)
    );
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
fn plan_paper_close_cancels_open_orders_and_logs_close_event() {
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

    let order_a = btc_mm_intent("market-mm", "up", "mm-paired-bid", 0.55);
    let order_b = btc_mm_intent("market-mm", "down", "mm-paired-bid", 0.44);
    let coid_a = order_a.client_order_id.clone();
    let coid_b = order_b.client_order_id.clone();
    runtime.accept_intent(order_a, 1);
    runtime.accept_intent(order_b, 1);
    assert_eq!(runtime.open_orders().count(), 2);

    let close_outcome = runtime.plan_paper_close(1_500, Some(0.5));

    let cancels: Vec<&ClientOrderId> = close_outcome
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            RuntimeCommand::Cancel {
                client_order_id, ..
            } => Some(client_order_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        cancels.len(),
        2,
        "expected cancel for both open orders, got commands {:?}",
        close_outcome.commands
    );
    assert!(cancels.contains(&&coid_a));
    assert!(cancels.contains(&&coid_b));
    assert!(
        !close_outcome.event_seqs.is_empty(),
        "expected at least the paper-market-close summary event"
    );
}

#[test]
fn paired_entry_rejection_cancels_mate_to_prevent_naked_exposure() {
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

    let pair_id = "pair-test-paired-entry-1".to_string();
    let mut left = btc_mm_intent("market-mm", "up", "mm-paired-bid", 0.55);
    left.pair_id = Some(pair_id.clone());
    let left_client_order_id = left.client_order_id.clone();
    let mut right = btc_mm_intent("market-mm", "down", "mm-paired-bid", 0.44);
    right.pair_id = Some(pair_id);
    let right_client_order_id = right.client_order_id.clone();

    let left_outcome = runtime.accept_intent(left, 1);
    assert_eq!(left_outcome.commands.len(), 1);
    let right_outcome = runtime.accept_intent(right, 1);
    assert_eq!(right_outcome.commands.len(), 1);
    assert_eq!(runtime.open_orders().count(), 2);

    let reject_outcome =
        runtime.on_order_rejected(&right_client_order_id, "venue rejected post-only", 2);

    let cancels: Vec<&ClientOrderId> = reject_outcome
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            RuntimeCommand::Cancel {
                client_order_id, ..
            } => Some(client_order_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        cancels.len(),
        1,
        "expected exactly one cancel command for the mate, got {:?}",
        reject_outcome.commands
    );
    assert_eq!(cancels[0], &left_client_order_id);
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

    let first = runtime.accept_intent(btc_mm_intent("market-mm", "down", "mm-paired-bid", 0.44), 1);
    assert_eq!(first.commands.len(), 1);

    // A SECOND mm-paired-bid on the same instrument is a duplicate and
    // should be rejected — fresh entry accumulation is the failure mode.
    let duplicate_paired =
        runtime.accept_intent(btc_mm_intent("market-mm", "down", "mm-paired-bid", 0.45), 2);
    assert!(duplicate_paired.commands.is_empty());
    assert_eq!(runtime.open_orders().count(), 1);

    // BUT an mm-hedge-rescue on the same instrument is NOT a duplicate —
    // it's a CLOSE operation (taker IOC) that intentionally coexists with
    // the maker paired-bid until the merge fires. The rescue path is the
    // entire point of the architecture; suppressing it leaves us
    // stranded long. This must produce a Submit command.
    let rescue = runtime.accept_intent(
        btc_mm_intent("market-mm", "down", "mm-hedge-rescue", 0.48),
        3,
    );
    assert_eq!(
        rescue.commands.len(),
        1,
        "hedge-rescue intent must coexist with active paired-bid"
    );
    assert_eq!(runtime.open_orders().count(), 2);
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
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
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
fn stale_needs_reconcile_order_uses_durable_terminal_state_instead_of_quarantine() {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("polymarket-exec-terminal-sync-{ts}.sqlite"));
    let mut store = SqliteOrderStore::open(&path).unwrap();
    let client_order_id = ClientOrderId::from("coid-terminal-sync");
    let intent = OrderIntent {
        client_order_id: client_order_id.clone(),
        market_id: MarketId::from("market-1"),
        instrument_id: InstrumentId::from("token-1"),
        side: TradeSide::Buy,
        limit_price: 0.41,
        quantity: 5.0,
        reduce_only: false,
        reason: "recover".to_string(),
        quote_level_tag: None,
        created_at_ms: 10,
        pair_id: None,
        kind: crate::types::IntentKind::Entry,
    };
    let mut record = OrderRecord::from_intent("run-1", &intent, "single-shot");
    record.status = ManagedOrderStatus::NeedsReconcile;
    record.last_update_ms = 10;
    store.insert(record).unwrap();

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
    runtime.recover_from_store(20, 5_000);
    assert_eq!(runtime.open_order_snapshots().len(), 1);

    let mut external_store = SqliteOrderStore::open(&path).unwrap();
    external_store
        .update_status(&client_order_id, ManagedOrderStatus::Filled, 30)
        .unwrap();
    drop(external_store);

    let outcome = runtime.quarantine_stale_needs_reconcile_orders(20_000, 100);

    assert!(!outcome.event_seqs.is_empty());
    assert!(runtime.open_order_snapshots().is_empty());
    let store_record = SqliteOrderStore::open(&path)
        .unwrap()
        .get(&client_order_id)
        .unwrap()
        .expect("order record");
    assert_eq!(store_record.status, ManagedOrderStatus::Filled);
    let _ = std::fs::remove_file(path);
}

#[test]
fn venue_position_reconciliation_recovers_missing_cost_basis_from_filled_buys() {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("polymarket-exec-cost-basis-{ts}.sqlite"));
    let mut store = SqliteOrderStore::open(&path).unwrap();
    let intent = OrderIntent {
        client_order_id: ClientOrderId::from("coid-cost-basis"),
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        side: TradeSide::Buy,
        limit_price: 0.05,
        quantity: 8.26,
        reduce_only: false,
        reason: "convex fill".to_string(),
        quote_level_tag: Some("mm-convex-accum:l1".to_string()),
        created_at_ms: 10,
        pair_id: None,
        kind: crate::types::IntentKind::Entry,
    };
    let mut record = OrderRecord::from_intent("run-1", &intent, "paired_mm");
    record.status = ManagedOrderStatus::Filled;
    record.remaining_qty = 0.0;
    record.filled_qty = 8.26;
    store.insert(record).unwrap();

    let mut runtime = Runtime::new_with_order_store(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        NoopStrategy,
        MarketContextStore::empty(),
        Some(Box::new(store)),
        "run-1".to_string(),
    );
    runtime
        .reconcile_venue_positions(
            &[VenuePositionSnapshot {
                market_id: MarketId::from("market-mm"),
                condition_id: None,
                instrument_id: InstrumentId::from("up"),
                quantity: 8.26,
                average_cost_usd: 0.0,
                mark_price: Some(0.05),
                observed_at_ms: 20,
            }],
            20,
        )
        .expect("reconcile");

    let position = runtime
        .inventory()
        .position(&InstrumentId::from("up"))
        .expect("position");
    assert_eq!(position.quantity, 8.26);
    assert!((position.avg_price - 0.05).abs() < 1e-9);
}

#[test]
fn recover_from_store_restores_strategy_checkpoint_state() {
    let path = std::env::temp_dir().join(format!(
        "polymarket-exec-recover-strategy-state-{}.sqlite",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let restored = Arc::new(AtomicBool::new(false));

    {
        let store = SqliteOrderStore::open(path.clone()).expect("open store");
        let mut runtime = Runtime::new_with_order_store(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Running,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            StatefulTestStrategy {
                restored: restored.clone(),
                state: serde_json::json!({"marker": "persisted"}),
            },
            MarketContextStore::empty(),
            Some(Box::new(store)),
            "run-state-save".to_string(),
        );
        runtime.persist_strategy_state(10);
    }

    let store = SqliteOrderStore::open(path).expect("reopen store");
    let mut recovered = Runtime::new_with_order_store(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        StatefulTestStrategy {
            restored: restored.clone(),
            state: serde_json::json!({}),
        },
        MarketContextStore::empty(),
        Some(Box::new(store)),
        "run-state-restore".to_string(),
    );

    let outcome = recovered.recover_from_store(20, 5_000);
    assert!(restored.load(Ordering::SeqCst));
    assert!(!outcome.event_seqs.is_empty());
    assert!(recovered
        .event_log()
        .recent(8)
        .iter()
        .any(|event| event.message.contains("restored strategy state")));
}

#[test]
fn recover_from_store_restores_sticky_riskoff_status() {
    let path = std::env::temp_dir().join(format!(
        "polymarket-exec-recover-runtime-state-{}.sqlite",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));

    {
        let store = SqliteOrderStore::open(path.clone()).expect("open store");
        let mut runtime = Runtime::new_with_order_store(
            RuntimeConfig {
                starting_cash_usd: 100.0,
                event_log_capacity: 128,
                initial_status: RuntimeStatus::Running,
                ..RuntimeConfig::default()
            },
            RiskLimits::default(),
            SingleShotStrategy { fired: false },
            MarketContextStore::empty(),
            Some(Box::new(store)),
            "run-riskoff-save".to_string(),
        );
        runtime.riskoff_and_cancel_entry_orders(10, "test equity floor");
        runtime.persist_runtime_status(10);
    }

    let store = SqliteOrderStore::open(path).expect("reopen store");
    let mut recovered = Runtime::new_with_order_store(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        SingleShotStrategy { fired: false },
        MarketContextStore::empty(),
        Some(Box::new(store)),
        "run-riskoff-restore".to_string(),
    );

    recovered.recover_from_store(20, 5_000);
    assert_eq!(recovered.status(), RuntimeStatus::RiskOff);

    let start_outcome = recovered.start(30);
    assert_eq!(recovered.status(), RuntimeStatus::RiskOff);
    assert!(start_outcome.commands.is_empty());
    assert!(recovered.event_log().recent(8).iter().any(|event| {
        event
            .message
            .contains("restored runtime status from durable store status=RiskOff")
    }));
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
            taker_buy_qty_60s: 0.0,
            taker_sell_qty_60s: 0.0,
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
            taker_buy_qty_60s: 0.0,
            taker_sell_qty_60s: 0.0,
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
            taker_buy_qty_60s: 0.0,
            taker_sell_qty_60s: 0.0,
            observed_at_ms: 2,
        },
    };
    runtime.on_market_snapshot(snapshot).expect("quote");
    runtime.mark_order_needs_reconcile(&ClientOrderId::from("client-1"), 3, "uncertain live state");

    let degraded = runtime.degrade_and_cancel_all(4, "startup has orders requiring reconciliation");
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
        recent_messages
            .iter()
            .any(|message| message.contains("runtime start skipped because runtime is degraded")),
        "missing degraded-start guard event in {recent_messages:?}"
    );
}
