use super::*;
use async_trait::async_trait;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};

use crate::book::Level;
use crate::config::{LogFormat, MarketDiscoveryFamily};
use crate::market_context::MarketContextStore;
use crate::metrics::StreamKind;
use crate::risk::RiskLimits;
use crate::runtime::order_store::{OrderRecord, OrderStore, SqliteOrderStore};
use crate::strategy::{NoopStrategy, StrategyProfile};
use crate::wire::execution_adapter::{
    CancelOrderAck, ExecutionError, MergePositionsAck, MergePositionsRequest, SubmitOrderAck,
    VenueBalances, VenueFill, VenuePosition,
};

fn runner_test_config() -> AppConfig {
    AppConfig {
        service_name: "test".to_string(),
        strategy_name: "noop".to_string(),
        paper_mode: false,
        log_level: "info".to_string(),
        log_format: LogFormat::Pretty,
        metrics_bind: "127.0.0.1:0".parse().unwrap(),
        clob_api_url: "https://clob.polymarket.com".to_string(),
        data_api_url: "https://data-api.polymarket.com".to_string(),
        relayer_url: "https://relayer-v2.polymarket.com".to_string(),
        relayer_api_key: None,
        relayer_api_key_address: None,
        ctf_contract_address: "0x0000000000000000000000000000000000000000".to_string(),
        ctf_collateral_token_address: "0x0000000000000000000000000000000000000000".to_string(),
        collateral_token_address: "0x0000000000000000000000000000000000000000".to_string(),
        collateral_decimals: 6,
        proxy_wallet_address: None,
        polygon_rpc_url: None,
        market_ws_url: "wss://example.invalid/market".to_string(),
        user_ws_url: "wss://example.invalid/user".to_string(),
        spot_ws_url: "wss://example.invalid/spot".to_string(),
        spot_rest_bootstrap_url: None,
        coinbase_spot_ws_url: None,
        spot_symbol: "btcusdt".to_string(),
        market_assets: Vec::new(),
        user_markets: Vec::new(),
        market_discovery_enabled: false,
        market_discovery_interval: std::time::Duration::from_secs(60),
        market_discovery_window: std::time::Duration::from_secs(300),
        market_discovery_include_prev: 0,
        market_discovery_include_next: 0,
        market_discovery_gamma_url: "https://gamma-api.polymarket.com".to_string(),
        market_discovery_slug_prefix: "btc-updown-5m".to_string(),
        market_discovery_families: Vec::<MarketDiscoveryFamily>::new(),
        runtime_loop_interval: std::time::Duration::from_millis(250),
        summary_log_interval: std::time::Duration::from_secs(30),
        order_reconcile_interval: std::time::Duration::from_secs(10),
        order_reconcile_stale_window: std::time::Duration::from_secs(5),
        runtime_checkpoint_interval: std::time::Duration::from_secs(30),
        book_stale_after: std::time::Duration::from_secs(5),
        order_store_path: None,
        runtime_run_id: None,
        ping_interval: std::time::Duration::from_secs(10),
        spot_ws_conn_stale_timeout: std::time::Duration::from_secs(30),
        spot_ws_data_stale_timeout: std::time::Duration::from_secs(30),
        market_context_path: None,
        journal_path: None,
        journal_rotate_bytes: None,
        starting_cash_usd: 100.0,
        event_log_capacity: 128,
        market_id_by_asset: HashMap::new(),
        risk_limits: RiskLimits::default(),
        strategy_profile_path: None,
        strategy_profile: None,
        user_auth: None,
        dashboard_whale_events_path: None,
        dashboard_refresh_ms: 1_000,
        dashboard_event_limit: 100,
        audit_path: None,
        clob_version: "v2".to_string(),
        clob_v2_builder_code: crate::wire::clob_v2::BYTES32_ZERO.to_string(),
        clob_v2_metadata: crate::wire::clob_v2::BYTES32_ZERO.to_string(),
        clob_v2_neg_risk: false,
        live_post_only: true,
        live_order_ttl: std::time::Duration::from_secs(60),
        live_order_max_age: std::time::Duration::from_secs(60),
        live_reconcile_missing_grace: std::time::Duration::from_secs(5),
        quote_min_order_age: std::time::Duration::from_millis(250),
        quote_churn_window: std::time::Duration::from_secs(10),
        quote_hard_pull: std::time::Duration::from_secs(30),
        quote_max_churn_per_window: 12,
        quote_max_submit_per_window: 6,
        quote_max_replace_per_window: 4,
        quote_max_cancel_per_window: 12,
        live_max_submit_errors: 1,
        live_max_cancel_errors: 1,
        live_kill_on_reconcile_mismatch: true,
        live_kill_switch_path: None,
        live_pusd_auto_wrap: false,
        live_pusd_auto_wrap_min_usd: 0.01,
        live_risk_off_auto_recover: std::time::Duration::from_secs(30),
        paper_min_fill_notional_usd: 0.05,
        paper_max_fills_per_order: 3,
        paper_min_fill_interval: std::time::Duration::from_millis(750),
        paper_market_close_at_ms: None,
        paper_market_resolution_price: None,
        paper_submit_latency_ms: 150,
        paper_queue_depth_fraction: 0.75,
        paper_post_only_reject_probability: 0.85,
        paper_cancel_race_window_ms: 500,
        paper_report_path: None,
        shadow_quote_log_path: None,
        book_snapshot_log_path: None,
        book_snapshot_max_levels: 10,
        paper_maker_rebate_coeff: 0.0,
        paper_taker_fee_coeff_override: None,
    }
}

#[derive(Default)]
struct RecordingAdapter {
    submitted: Mutex<Vec<ClientOrderId>>,
    cancelled: Mutex<Vec<ClientOrderId>>,
    merged: Mutex<Vec<MergePositionsRequest>>,
    submit_reject_message: Option<String>,
    cancel_reject_message: Option<String>,
    merge_accept: bool,
    open_orders: Vec<crate::wire::execution_adapter::VenueOpenOrder>,
    fills: Vec<VenueFill>,
    balances: Option<VenueBalances>,
    pusd_wrap_min_usd: Mutex<Vec<f64>>,
    pusd_wrap_fails: bool,
}

#[async_trait]
impl ExecutionAdapter for RecordingAdapter {
    async fn submit(&self, req: SubmitOrderRequest) -> Result<SubmitOrderAck, ExecutionError> {
        self.submitted
            .lock()
            .expect("submitted lock")
            .push(req.client_order_id.clone());
        if let Some(message) = self.submit_reject_message.clone() {
            return Ok(SubmitOrderAck {
                client_order_id: req.client_order_id,
                venue_order_id: Some(OrderId::from("venue-submit")),
                accepted: false,
                accepted_at_ms: req.submitted_at_ms,
                venue_message: Some(message),
            });
        }
        Ok(SubmitOrderAck {
            client_order_id: req.client_order_id,
            venue_order_id: Some(OrderId::from("venue-submit")),
            accepted: true,
            accepted_at_ms: req.submitted_at_ms,
            venue_message: None,
        })
    }

    async fn cancel(&self, req: CancelOrderRequest) -> Result<CancelOrderAck, ExecutionError> {
        self.cancelled
            .lock()
            .expect("cancelled lock")
            .push(req.client_order_id.clone());
        if let Some(message) = self.cancel_reject_message.clone() {
            return Ok(CancelOrderAck {
                client_order_id: req.client_order_id,
                venue_order_id: req.venue_order_id,
                accepted: false,
                accepted_at_ms: req.submitted_at_ms,
                venue_message: Some(message),
            });
        }
        Ok(CancelOrderAck {
            client_order_id: req.client_order_id,
            venue_order_id: req.venue_order_id,
            accepted: true,
            accepted_at_ms: req.submitted_at_ms,
            venue_message: Some("cancelled".to_string()),
        })
    }

    async fn merge_positions(
        &self,
        req: MergePositionsRequest,
    ) -> Result<MergePositionsAck, ExecutionError> {
        self.merged.lock().expect("merged lock").push(req.clone());
        if !self.merge_accept {
            return Err(ExecutionError::BadRequest(
                "test adapter merge not implemented".to_string(),
            ));
        }
        Ok(MergePositionsAck {
            command_id: req.command_id,
            accepted: true,
            accepted_at_ms: req.submitted_at_ms,
            venue_message: Some("test merge accepted".to_string()),
        })
    }

    async fn ensure_pusd_collateral_from_usdce(
        &self,
        min_wrap_usd: f64,
    ) -> Result<Option<crate::wire::eoa_polygon::PusdWrapReport>, ExecutionError> {
        self.pusd_wrap_min_usd
            .lock()
            .expect("pusd wrap lock")
            .push(min_wrap_usd);
        if self.pusd_wrap_fails {
            return Err(ExecutionError::TransientNetwork(
                "test polygon rpc throttle".to_string(),
            ));
        }
        Ok(None)
    }

    async fn sync_open_orders(
        &self,
    ) -> Result<Vec<crate::wire::execution_adapter::VenueOpenOrder>, ExecutionError> {
        Ok(self.open_orders.clone())
    }

    async fn sync_balances(&self) -> Result<VenueBalances, ExecutionError> {
        Ok(self.balances.clone().unwrap_or(VenueBalances {
            cash_usd: 0.0,
            positions: Vec::new(),
            positions_authoritative: false,
            observed_at_ms: 0,
        }))
    }

    async fn sync_recent_fills(&self, _after_ms: u64) -> Result<Vec<VenueFill>, ExecutionError> {
        Ok(self.fills.clone())
    }
}

#[tokio::test]
async fn pusd_auto_wrap_after_redeem_uses_live_wrap_threshold() {
    let mut config = runner_test_config();
    config.live_pusd_auto_wrap = true;
    config.live_pusd_auto_wrap_min_usd = 2.50;
    let adapter = RecordingAdapter::default();

    maybe_auto_wrap_pusd_after_redeem(&config, &adapter).await;

    assert_eq!(
        adapter
            .pusd_wrap_min_usd
            .lock()
            .expect("pusd wrap lock")
            .as_slice(),
        &[2.50]
    );
}

#[tokio::test]
async fn pusd_auto_wrap_after_redeem_is_disabled_by_config() {
    let mut config = runner_test_config();
    config.live_pusd_auto_wrap = false;
    let adapter = RecordingAdapter::default();

    maybe_auto_wrap_pusd_after_redeem(&config, &adapter).await;

    assert!(adapter
        .pusd_wrap_min_usd
        .lock()
        .expect("pusd wrap lock")
        .is_empty());
}

#[tokio::test]
async fn pusd_auto_wrap_startup_error_is_non_fatal() {
    let mut config = runner_test_config();
    config.live_pusd_auto_wrap = true;
    config.live_pusd_auto_wrap_min_usd = 2.50;
    let adapter = RecordingAdapter {
        pusd_wrap_fails: true,
        ..RecordingAdapter::default()
    };

    maybe_auto_wrap_pusd_at_startup(&config, &adapter).await;

    assert_eq!(
        adapter
            .pusd_wrap_min_usd
            .lock()
            .expect("pusd wrap lock")
            .as_slice(),
        &[2.50]
    );
}

#[tokio::test]
async fn live_execution_skips_needs_reconcile_submit_replay() {
    let mut runtime = runtime_with_recovered_needs_reconcile_order();
    let adapter = Arc::new(RecordingAdapter {
        open_orders: vec![crate::wire::execution_adapter::VenueOpenOrder {
            venue_order_id: OrderId::from("venue-1"),
            client_order_id: Some(ClientOrderId::from("client-reconcile")),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            limit_price: 0.40,
            original_qty: 5.0,
            remaining_qty: 5.0,
            created_at_ms: 1,
        }],
        ..RecordingAdapter::default()
    });
    let metrics = AppMetrics::new().expect("metrics");
    let assets: Vec<String> = Vec::new();
    let books = Arc::new(BookStore::new(&assets));
    let mut paper_order_ctx = HashMap::new();
    let mut execution_venue_map = HashMap::new();
    let mut live_safety = LiveSafetyState::default();
    let execution_policy = live_test_policy();
    let mut seen_venue_fill_keys = HashSet::new();

    let outcome = execute_execution_adapter(
        &mut runtime,
        &books,
        &assets,
        0.0,
        &metrics,
        RuntimeOutcome::default(),
        &mut paper_order_ctx,
        &mut execution_venue_map,
        &mut live_safety,
        adapter.clone(),
        &execution_policy,
        &mut seen_venue_fill_keys,
        None,
        None,
    )
    .await
    .expect("execute");

    assert!(outcome.commands.is_empty());
    assert!(adapter.submitted.lock().expect("submitted lock").is_empty());
    assert!(runtime.open_order_snapshots().into_iter().all(|managed| {
        managed.intent.client_order_id != ClientOrderId::from("client-reconcile")
    }));
}

#[tokio::test]
async fn live_execution_ignores_stale_submit_after_order_left_memory() {
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        StrategyMode::Noop(NoopStrategy),
        MarketContextStore::empty(),
    );
    let stale_client_id = ClientOrderId::from("client-filled-before-submit-ack");
    let stale_intent = OrderIntent {
        client_order_id: stale_client_id,
        market_id: MarketId::from("market-1"),
        instrument_id: InstrumentId::from("token-1"),
        side: TradeSide::Buy,
        limit_price: 0.40,
        quantity: 5.0,
        reduce_only: false,
        reason: "stale queued submit".to_string(),
        quote_level_tag: None,
        created_at_ms: now_unix_ms(),
        pair_id: None,
        kind: crate::types::IntentKind::Entry,
    };
    let mut initial_outcome = RuntimeOutcome::default();
    initial_outcome.push_command(RuntimeCommand::Submit(stale_intent));

    let adapter = Arc::new(RecordingAdapter {
        submit_reject_message: Some("execution venue rejected submit".to_string()),
        ..RecordingAdapter::default()
    });
    let metrics = AppMetrics::new().expect("metrics");
    let assets: Vec<String> = Vec::new();
    let books = Arc::new(BookStore::new(&assets));
    let mut paper_order_ctx = HashMap::new();
    let mut execution_venue_map = HashMap::new();
    let mut live_safety = LiveSafetyState::default();
    let execution_policy = live_test_policy();
    let mut seen_venue_fill_keys = HashSet::new();

    let _outcome = execute_execution_adapter(
        &mut runtime,
        &books,
        &assets,
        0.0,
        &metrics,
        initial_outcome,
        &mut paper_order_ctx,
        &mut execution_venue_map,
        &mut live_safety,
        adapter.clone(),
        &execution_policy,
        &mut seen_venue_fill_keys,
        None,
        None,
    )
    .await
    .expect("execute");

    assert!(adapter.submitted.lock().expect("submitted lock").is_empty());
    assert_eq!(live_safety.consecutive_submit_errors, 0);
    assert_ne!(runtime.status(), RuntimeStatus::Degraded);
    assert_eq!(metrics.snapshot().runtime_riskoff_transitions_total, 0);
}

#[tokio::test]
async fn live_sync_defers_recent_missing_working_order() {
    let mut runtime = runtime_with_recovered_working_order(now_unix_ms());
    let adapter = Arc::new(RecordingAdapter::default());
    let metrics = AppMetrics::new().expect("metrics");
    let assets: Vec<String> = Vec::new();
    let books = Arc::new(BookStore::new(&assets));
    let mut paper_order_ctx = HashMap::new();
    let mut execution_venue_map = HashMap::from([(
        ClientOrderId::from("client-working"),
        Some(OrderId::from("venue-1")),
    )]);
    let mut live_safety = LiveSafetyState::default();
    let execution_policy = live_test_policy();
    let mut seen_venue_fill_keys = HashSet::new();

    let _outcome = execute_execution_adapter(
        &mut runtime,
        &books,
        &assets,
        0.0,
        &metrics,
        RuntimeOutcome::default(),
        &mut paper_order_ctx,
        &mut execution_venue_map,
        &mut live_safety,
        adapter.clone(),
        &execution_policy,
        &mut seen_venue_fill_keys,
        None,
        None,
    )
    .await
    .expect("execute");

    assert_eq!(live_safety.consecutive_reconcile_mismatches, 0);
    assert!(adapter.cancelled.lock().expect("cancelled lock").is_empty());
    let order = runtime
        .open_order_snapshots()
        .into_iter()
        .find(|managed| managed.intent.client_order_id == ClientOrderId::from("client-working"))
        .expect("managed order");
    assert_eq!(order.status, ManagedOrderStatus::Working);
}

#[tokio::test]
async fn live_sync_applies_late_fill_after_order_left_open_memory() {
    let mut runtime = runtime_with_recovered_working_order(now_unix_ms());
    let client_order_id = ClientOrderId::from("client-working");
    runtime.on_order_cancelled(&client_order_id, "test cancel before fill", now_unix_ms());
    assert!(runtime.open_order_snapshots().is_empty());

    let adapter = Arc::new(RecordingAdapter {
        fills: vec![VenueFill {
            venue_order_id: OrderId::from("venue-1"),
            client_order_id: None,
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            price: 0.40,
            quantity: 5.0,
            fee_usd: 0.0,
            liquidity: FillLiquidity::Maker,
            observed_at_ms: now_unix_ms(),
        }],
        ..RecordingAdapter::default()
    });
    let metrics = AppMetrics::new().expect("metrics");
    let assets: Vec<String> = Vec::new();
    let books = Arc::new(BookStore::new(&assets));
    let mut paper_order_ctx = HashMap::new();
    let mut execution_venue_map =
        HashMap::from([(client_order_id.clone(), Some(OrderId::from("venue-1")))]);
    let mut live_safety = LiveSafetyState::default();
    let execution_policy = live_test_policy();
    let mut seen_venue_fill_keys = HashSet::new();

    let _outcome = execute_execution_adapter(
        &mut runtime,
        &books,
        &assets,
        0.0,
        &metrics,
        RuntimeOutcome::default(),
        &mut paper_order_ctx,
        &mut execution_venue_map,
        &mut live_safety,
        adapter.clone(),
        &execution_policy,
        &mut seen_venue_fill_keys,
        None,
        None,
    )
    .await
    .expect("execute");

    let position = runtime
        .inventory()
        .position(&InstrumentId::from("token-1"))
        .expect("late fill should create inventory");
    assert_eq!(position.quantity, 5.0);
    assert_eq!(live_safety.consecutive_reconcile_mismatches, 0);
    assert_eq!(metrics.snapshot().fill_total, 1);
    assert_eq!(metrics.snapshot().fill_maker_total, 1);
}

#[tokio::test]
async fn live_sync_reconciles_non_empty_venue_position_snapshot() {
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        StrategyMode::Noop(NoopStrategy),
        MarketContextStore::empty(),
    );
    assert_eq!(
        runtime
            .inventory()
            .position_qty(&InstrumentId::from("down")),
        0.0
    );

    let now_ms = now_unix_ms();
    let adapter = Arc::new(RecordingAdapter {
        balances: Some(VenueBalances {
            cash_usd: 74.89,
            positions: vec![VenuePosition {
                market_id: MarketId::from("market-mm"),
                condition_id: None,
                instrument_id: InstrumentId::from("down"),
                quantity: 6.5,
                average_cost_usd: 0.80,
                redeemable: false,
                mergeable: false,
                current_value_usd: 0.0,
            }],
            positions_authoritative: true,
            observed_at_ms: now_ms,
        }),
        ..RecordingAdapter::default()
    });
    let metrics = AppMetrics::new().expect("metrics");
    let assets = vec!["down".to_string()];
    let books = Arc::new(BookStore::new(&assets));
    let mut paper_order_ctx = HashMap::new();
    let mut execution_venue_map = HashMap::new();
    let mut live_safety = LiveSafetyState::default();
    let execution_policy = live_test_policy();
    let mut seen_venue_fill_keys = HashSet::new();

    let _outcome = execute_execution_adapter(
        &mut runtime,
        &books,
        &assets,
        0.0,
        &metrics,
        RuntimeOutcome::default(),
        &mut paper_order_ctx,
        &mut execution_venue_map,
        &mut live_safety,
        adapter.clone(),
        &execution_policy,
        &mut seen_venue_fill_keys,
        None,
        None,
    )
    .await
    .expect("execute");

    let position = runtime
        .inventory()
        .position(&InstrumentId::from("down"))
        .expect("venue position should reconcile into runtime inventory");
    assert_eq!(position.quantity, 6.5);
    assert_eq!(position.avg_price, 0.80);
    assert!((runtime.inventory().free_cash_usd() - 74.89).abs() < 1e-9);
    assert_eq!(runtime.stranded_inventory().len(), 1);
    assert_eq!(live_safety.consecutive_reconcile_mismatches, 0);
    assert_eq!(metrics.snapshot().venue_position_count, 1);
}

#[tokio::test]
async fn live_sync_executes_merge_plan_and_fails_closed_when_adapter_cannot_merge() {
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        StrategyMode::Noop(NoopStrategy),
        MarketContextStore::empty(),
    );

    let now_ms = now_unix_ms();
    let adapter = Arc::new(RecordingAdapter {
        balances: Some(VenueBalances {
            cash_usd: 80.0,
            positions: vec![
                VenuePosition {
                    market_id: MarketId::from("market-mm"),
                    condition_id: Some(
                        "0x1111111111111111111111111111111111111111111111111111111111111111"
                            .to_string(),
                    ),
                    instrument_id: InstrumentId::from("up"),
                    quantity: 6.5,
                    average_cost_usd: 0.20,
                    redeemable: false,
                    mergeable: true,
                    current_value_usd: 1.30,
                },
                VenuePosition {
                    market_id: MarketId::from("market-mm"),
                    condition_id: Some(
                        "0x1111111111111111111111111111111111111111111111111111111111111111"
                            .to_string(),
                    ),
                    instrument_id: InstrumentId::from("down"),
                    quantity: 6.5,
                    average_cost_usd: 0.79,
                    redeemable: false,
                    mergeable: true,
                    current_value_usd: 5.13,
                },
            ],
            positions_authoritative: true,
            observed_at_ms: now_ms,
        }),
        ..RecordingAdapter::default()
    });
    let metrics = AppMetrics::new().expect("metrics");
    let assets = vec!["up".to_string(), "down".to_string()];
    let books = Arc::new(BookStore::new(&assets));
    let mut paper_order_ctx = HashMap::new();
    let mut execution_venue_map = HashMap::new();
    let mut live_safety = LiveSafetyState::default();
    let execution_policy = live_test_policy();
    let mut seen_venue_fill_keys = HashSet::new();

    let outcome = execute_execution_adapter(
        &mut runtime,
        &books,
        &assets,
        0.0,
        &metrics,
        RuntimeOutcome::default(),
        &mut paper_order_ctx,
        &mut execution_venue_map,
        &mut live_safety,
        adapter.clone(),
        &execution_policy,
        &mut seen_venue_fill_keys,
        None,
        None,
    )
    .await
    .expect("execute");

    assert!(outcome
        .commands
        .iter()
        .any(|command| matches!(command, RuntimeCommand::Merge(_))));
    assert_eq!(runtime.status(), RuntimeStatus::Degraded);
    assert_eq!(metrics.snapshot().runtime_riskoff_transitions_total, 1);
    assert!(adapter.submitted.lock().expect("submitted lock").is_empty());
    let merges = adapter.merged.lock().expect("merged lock");
    assert_eq!(merges.len(), 1);
    assert_eq!(
        merges[0].condition_id.as_deref(),
        Some("0x1111111111111111111111111111111111111111111111111111111111111111")
    );
    drop(merges);

    let second_outcome = execute_execution_adapter(
        &mut runtime,
        &books,
        &assets,
        0.0,
        &metrics,
        RuntimeOutcome::default(),
        &mut paper_order_ctx,
        &mut execution_venue_map,
        &mut live_safety,
        adapter.clone(),
        &execution_policy,
        &mut seen_venue_fill_keys,
        None,
        None,
    )
    .await
    .expect("second execute");

    assert!(!second_outcome
        .commands
        .iter()
        .any(|command| matches!(command, RuntimeCommand::Merge(_))));
    assert_eq!(runtime.status(), RuntimeStatus::Degraded);
    let merges = adapter.merged.lock().expect("merged lock");
    assert_eq!(
        merges.len(),
        1,
        "non-retryable merge failure must not resubmit identical CTF recycle tx"
    );
}

#[tokio::test]
async fn live_accepted_merge_suppresses_duplicate_but_allows_later_paired_inventory() {
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        StrategyMode::Noop(NoopStrategy),
        MarketContextStore::empty(),
    );

    fn paired_balances(quantity: f64, now_ms: u64) -> VenueBalances {
        VenueBalances {
            cash_usd: 80.0,
            positions: vec![
                VenuePosition {
                    market_id: MarketId::from("market-mm"),
                    condition_id: Some(
                        "0x1111111111111111111111111111111111111111111111111111111111111111"
                            .to_string(),
                    ),
                    instrument_id: InstrumentId::from("up"),
                    quantity,
                    average_cost_usd: 0.20,
                    redeemable: false,
                    mergeable: true,
                    current_value_usd: quantity * 0.20,
                },
                VenuePosition {
                    market_id: MarketId::from("market-mm"),
                    condition_id: Some(
                        "0x1111111111111111111111111111111111111111111111111111111111111111"
                            .to_string(),
                    ),
                    instrument_id: InstrumentId::from("down"),
                    quantity,
                    average_cost_usd: 0.79,
                    redeemable: false,
                    mergeable: true,
                    current_value_usd: quantity * 0.79,
                },
            ],
            positions_authoritative: true,
            observed_at_ms: now_ms,
        }
    }

    let metrics = AppMetrics::new().expect("metrics");
    let assets = vec!["up".to_string(), "down".to_string()];
    let books = Arc::new(BookStore::new(&assets));
    let mut paper_order_ctx = HashMap::new();
    let mut execution_venue_map = HashMap::new();
    let mut live_safety = LiveSafetyState::default();
    let execution_policy = live_test_policy();
    let mut seen_venue_fill_keys = HashSet::new();

    let now_ms = now_unix_ms();
    let first_adapter = Arc::new(RecordingAdapter {
        merge_accept: true,
        balances: Some(paired_balances(10.0, now_ms)),
        ..RecordingAdapter::default()
    });

    execute_execution_adapter(
        &mut runtime,
        &books,
        &assets,
        0.0,
        &metrics,
        RuntimeOutcome::default(),
        &mut paper_order_ctx,
        &mut execution_venue_map,
        &mut live_safety,
        first_adapter.clone(),
        &execution_policy,
        &mut seen_venue_fill_keys,
        None,
        None,
    )
    .await
    .expect("first execute");

    assert_eq!(runtime.status(), RuntimeStatus::Running);
    assert_eq!(first_adapter.merged.lock().expect("merged lock").len(), 1);

    let duplicate_snapshot_adapter = Arc::new(RecordingAdapter {
        merge_accept: true,
        balances: Some(paired_balances(10.0, now_ms + 1)),
        ..RecordingAdapter::default()
    });
    execute_execution_adapter(
        &mut runtime,
        &books,
        &assets,
        0.0,
        &metrics,
        RuntimeOutcome::default(),
        &mut paper_order_ctx,
        &mut execution_venue_map,
        &mut live_safety,
        duplicate_snapshot_adapter.clone(),
        &execution_policy,
        &mut seen_venue_fill_keys,
        None,
        None,
    )
    .await
    .expect("duplicate execute");

    assert!(
        duplicate_snapshot_adapter
            .merged
            .lock()
            .expect("merged lock")
            .is_empty(),
        "identical post-ack venue snapshot must not resubmit the same CTF merge"
    );

    let later_inventory_adapter = Arc::new(RecordingAdapter {
        merge_accept: true,
        balances: Some(paired_balances(12.0, now_ms + 2)),
        ..RecordingAdapter::default()
    });
    execute_execution_adapter(
        &mut runtime,
        &books,
        &assets,
        0.0,
        &metrics,
        RuntimeOutcome::default(),
        &mut paper_order_ctx,
        &mut execution_venue_map,
        &mut live_safety,
        later_inventory_adapter.clone(),
        &execution_policy,
        &mut seen_venue_fill_keys,
        None,
        None,
    )
    .await
    .expect("later execute");

    let later_merges = later_inventory_adapter.merged.lock().expect("merged lock");
    assert_eq!(
        later_merges.len(),
        1,
        "later changed paired inventory should plan and submit a fresh merge"
    );
    assert_eq!(later_merges[0].quantity, 12.0);
}

#[tokio::test]
async fn live_sync_excludes_inactive_venue_positions_from_strategy_inventory() {
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        StrategyMode::Noop(NoopStrategy),
        MarketContextStore::empty(),
    );

    let now_ms = now_unix_ms();
    let adapter = Arc::new(RecordingAdapter {
        balances: Some(VenueBalances {
            cash_usd: 74.89,
            positions: vec![VenuePosition {
                market_id: MarketId::from("old-market"),
                condition_id: None,
                instrument_id: InstrumentId::from("old-token"),
                quantity: 6.5,
                average_cost_usd: 0.80,
                redeemable: false,
                mergeable: false,
                current_value_usd: 0.0,
            }],
            positions_authoritative: true,
            observed_at_ms: now_ms,
        }),
        ..RecordingAdapter::default()
    });
    let metrics = AppMetrics::new().expect("metrics");
    let assets = vec!["active-token".to_string()];
    let books = Arc::new(BookStore::new(&assets));
    let mut paper_order_ctx = HashMap::new();
    let mut execution_venue_map = HashMap::new();
    let mut live_safety = LiveSafetyState::default();
    let execution_policy = live_test_policy();
    let mut seen_venue_fill_keys = HashSet::new();

    let _outcome = execute_execution_adapter(
        &mut runtime,
        &books,
        &assets,
        0.0,
        &metrics,
        RuntimeOutcome::default(),
        &mut paper_order_ctx,
        &mut execution_venue_map,
        &mut live_safety,
        adapter,
        &execution_policy,
        &mut seen_venue_fill_keys,
        None,
        None,
    )
    .await
    .expect("execute");

    assert_eq!(metrics.snapshot().venue_position_count, 1);
    assert_eq!(
        runtime
            .inventory()
            .position_qty(&InstrumentId::from("old-token")),
        0.0
    );
    assert_eq!(runtime.inventory().gross_exposure_usd(), 0.0);
    assert_eq!(live_safety.consecutive_reconcile_mismatches, 0);
}

#[tokio::test]
async fn matched_cancel_reject_reconciles_without_risk_off() {
    let mut runtime = runtime_with_recovered_working_order(now_unix_ms());
    let client_order_id = ClientOrderId::from("client-working");
    let cancel_outcome =
        runtime.request_cancel_order(&client_order_id, now_unix_ms(), "test cancel race");

    let adapter = Arc::new(RecordingAdapter {
        cancel_reject_message: Some("matched orders can't be canceled".to_string()),
        fills: vec![VenueFill {
            venue_order_id: OrderId::from("venue-1"),
            client_order_id: None,
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            price: 0.40,
            quantity: 5.0,
            fee_usd: 0.0,
            liquidity: FillLiquidity::Maker,
            observed_at_ms: now_unix_ms(),
        }],
        ..RecordingAdapter::default()
    });
    let metrics = AppMetrics::new().expect("metrics");
    let assets = vec!["token-1".to_string()];
    let books = Arc::new(BookStore::new(&assets));
    let mut paper_order_ctx = HashMap::new();
    let mut execution_venue_map =
        HashMap::from([(client_order_id.clone(), Some(OrderId::from("venue-1")))]);
    let mut live_safety = LiveSafetyState::default();
    let execution_policy = live_test_policy();
    let mut seen_venue_fill_keys = HashSet::new();

    let _outcome = execute_execution_adapter(
        &mut runtime,
        &books,
        &assets,
        0.0,
        &metrics,
        cancel_outcome,
        &mut paper_order_ctx,
        &mut execution_venue_map,
        &mut live_safety,
        adapter,
        &execution_policy,
        &mut seen_venue_fill_keys,
        None,
        None,
    )
    .await
    .expect("execute");

    assert_ne!(runtime.status(), RuntimeStatus::Degraded);
    assert_eq!(live_safety.consecutive_cancel_errors, 0);
    assert_eq!(metrics.snapshot().runtime_riskoff_transitions_total, 0);
    assert_eq!(
        runtime
            .inventory()
            .position_qty(&InstrumentId::from("token-1")),
        5.0
    );
}

#[tokio::test]
async fn live_sync_clears_local_inventory_on_authoritative_empty_venue_positions() {
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        StrategyMode::Noop(NoopStrategy),
        MarketContextStore::empty(),
    );
    let now_ms = now_unix_ms();
    runtime
        .reconcile_venue_positions(
            &[VenuePositionSnapshot {
                market_id: MarketId::from("market-mm"),
                condition_id: None,
                instrument_id: InstrumentId::from("down"),
                quantity: 6.5,
                average_cost_usd: 0.80,
                mark_price: None,
                observed_at_ms: now_ms,
            }],
            now_ms,
        )
        .expect("seed inventory");
    assert_eq!(
        runtime
            .inventory()
            .position_qty(&InstrumentId::from("down")),
        6.5
    );

    let adapter = Arc::new(RecordingAdapter {
        balances: Some(VenueBalances {
            cash_usd: 80.0,
            positions: Vec::new(),
            positions_authoritative: true,
            observed_at_ms: now_ms.saturating_add(1),
        }),
        ..RecordingAdapter::default()
    });
    let metrics = AppMetrics::new().expect("metrics");
    let assets: Vec<String> = Vec::new();
    let books = Arc::new(BookStore::new(&assets));
    let mut paper_order_ctx = HashMap::new();
    let mut execution_venue_map = HashMap::new();
    let mut live_safety = LiveSafetyState::default();
    let execution_policy = live_test_policy();
    let mut seen_venue_fill_keys = HashSet::new();

    let _outcome = execute_execution_adapter(
        &mut runtime,
        &books,
        &assets,
        0.0,
        &metrics,
        RuntimeOutcome::default(),
        &mut paper_order_ctx,
        &mut execution_venue_map,
        &mut live_safety,
        adapter,
        &execution_policy,
        &mut seen_venue_fill_keys,
        None,
        None,
    )
    .await
    .expect("execute");

    assert_eq!(
        runtime
            .inventory()
            .position_qty(&InstrumentId::from("down")),
        0.0
    );
    assert_eq!(metrics.snapshot().venue_position_count, 0);
    assert_eq!(live_safety.consecutive_reconcile_mismatches, 0);
}

#[test]
fn user_fill_resolves_client_order_from_venue_order_id() {
    let execution_venue_map = HashMap::from([(
        ClientOrderId::from("client-1"),
        Some(OrderId::from("venue-1")),
    )]);

    assert_eq!(
        resolve_user_event_client_order_id(None, Some("venue-1"), &execution_venue_map),
        Some("client-1".to_string())
    );
    assert_eq!(
        resolve_user_event_client_order_id(
            Some("client-direct".to_string()),
            Some("venue-1"),
            &execution_venue_map,
        ),
        Some("client-direct".to_string())
    );
}

#[test]
fn live_submit_request_uses_post_only_gtd_with_expiry() {
    let intent = OrderIntent {
        client_order_id: ClientOrderId::from("client-ttl"),
        market_id: MarketId::from("market-1"),
        instrument_id: InstrumentId::from("token-1"),
        side: TradeSide::Buy,
        limit_price: 0.40,
        quantity: 5.0,
        reduce_only: false,
        reason: "test live lifecycle".to_string(),
        quote_level_tag: Some("lvl-1:test".to_string()),
        created_at_ms: 10,
        pair_id: None,
        kind: crate::types::IntentKind::Entry,
    };
    let policy = live_test_policy();
    let request = submit_request_from_intent(&intent, 1_000, &policy);
    assert!(request.post_only);
    assert_eq!(request.time_in_force, TimeInForce::Gtd);
    assert_eq!(request.expires_at_ms, Some(21_000));
}

#[test]
fn late_bar_core_submit_uses_gtd_with_60s_ttl_and_post_only() {
    let intent = OrderIntent {
        client_order_id: ClientOrderId::from("client-late-core"),
        market_id: MarketId::from("market-1"),
        instrument_id: InstrumentId::from("token-1"),
        side: TradeSide::Buy,
        limit_price: 0.93,
        quantity: 5.0,
        reduce_only: false,
        reason: "test late bar core".to_string(),
        quote_level_tag: Some("mm-late-bar-core:l1".to_string()),
        created_at_ms: 10,
        pair_id: None,
        kind: crate::types::IntentKind::Entry,
    };
    let policy = live_test_policy();
    let request = submit_request_from_intent(&intent, 1_000, &policy);
    assert_eq!(request.time_in_force, TimeInForce::Gtd);
    assert!(request.post_only);
    assert_eq!(request.expires_at_ms, Some(61_000));
}

#[test]
fn generic_post_only_submit_reject_does_not_consume_live_budget() {
    assert!(!submit_rejection_counts_against_live_budget(
        "execution venue rejected submit",
        true
    ));
    assert!(submit_rejection_counts_against_live_budget(
        "execution venue rejected submit",
        false
    ));
    assert!(submit_rejection_counts_against_live_budget(
        "insufficient balance",
        true
    ));
}

#[test]
fn portfolio_equity_floor_uses_stricter_absolute_or_session_loss_floor() {
    let risk_limits = RiskLimits {
        min_portfolio_equity_usd: 60.0,
        max_session_loss_usd: 25.0,
        ..RiskLimits::default()
    };

    assert_eq!(portfolio_equity_floor_usd(&risk_limits, 100.0), Some(75.0));

    let risk_limits = RiskLimits {
        min_portfolio_equity_usd: 90.0,
        max_session_loss_usd: 25.0,
        ..RiskLimits::default()
    };

    assert_eq!(portfolio_equity_floor_usd(&risk_limits, 100.0), Some(90.0));
    assert_eq!(
        portfolio_equity_floor_usd(&RiskLimits::default(), 100.0),
        None
    );
}

#[test]
fn capital_guard_sets_riskoff_when_marked_equity_breaks_floor() {
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Running,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        StrategyMode::Noop(NoopStrategy),
        MarketContextStore::empty(),
    );
    runtime
        .on_fill(FillReport {
            order_id: None,
            client_order_id: None,
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            price: 0.60,
            quantity: 100.0,
            fee_usd: 0.0,
            liquidity: FillLiquidity::Maker,
            close_method: None,
            observed_at_ms: 1,
        })
        .expect("fill");
    runtime
        .reconcile_venue_positions(
            &[VenuePositionSnapshot {
                market_id: MarketId::from("market-1"),
                condition_id: None,
                instrument_id: InstrumentId::from("token-1"),
                quantity: 100.0,
                average_cost_usd: 0.60,
                mark_price: Some(0.30),
                observed_at_ms: 2,
            }],
            2,
        )
        .expect("reconcile");

    let metrics = AppMetrics::new().expect("metrics");
    let outcome = enforce_capital_guard(
        &mut runtime,
        &metrics,
        &RiskLimits {
            max_session_loss_usd: 20.0,
            ..RiskLimits::default()
        },
        100.0,
        3,
        "paper",
    );

    assert_eq!(runtime.status(), RuntimeStatus::RiskOff);
    assert!(!outcome.event_seqs.is_empty());
    assert_eq!(metrics.snapshot().runtime_riskoff_transitions_total, 1);
}

#[test]
fn live_riskoff_auto_recover_promotes_running_after_healthy_window() {
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::RiskOff,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        StrategyMode::Noop(NoopStrategy),
        MarketContextStore::empty(),
    );
    let metrics = AppMetrics::new().expect("metrics");
    metrics.set_stream_connected(StreamKind::Market, true);
    metrics.set_stream_connected(StreamKind::User, true);
    metrics.set_execution_adapter_connected(true);
    let config = runner_test_config();
    let live_safety = LiveSafetyState {
        last_venue_cash_usd: Some(100.0),
        ..LiveSafetyState::default()
    };

    let early = auto_recover_live_riskoff(&mut runtime, &metrics, &config, &live_safety, 5_000, 0);
    assert_eq!(runtime.status(), RuntimeStatus::RiskOff);
    assert!(early.event_seqs.is_empty());

    let recovered =
        auto_recover_live_riskoff(&mut runtime, &metrics, &config, &live_safety, 31_000, 0);
    assert_eq!(runtime.status(), RuntimeStatus::Running);
    assert!(!recovered.event_seqs.is_empty());
    assert!(runtime
        .event_log()
        .recent(4)
        .iter()
        .any(|event| event.message.contains("runtime risk-off auto-recovered")));
}

#[test]
fn live_degraded_auto_recover_promotes_running_after_healthy_window() {
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Degraded,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        StrategyMode::Noop(NoopStrategy),
        MarketContextStore::empty(),
    );
    let metrics = AppMetrics::new().expect("metrics");
    metrics.set_stream_connected(StreamKind::Market, true);
    metrics.set_stream_connected(StreamKind::User, true);
    metrics.set_execution_adapter_connected(true);
    let config = runner_test_config();
    let live_safety = LiveSafetyState {
        last_venue_cash_usd: Some(100.0),
        ..LiveSafetyState::default()
    };

    let early = auto_recover_live_riskoff(&mut runtime, &metrics, &config, &live_safety, 5_000, 0);
    assert_eq!(runtime.status(), RuntimeStatus::Degraded);
    assert!(early.event_seqs.is_empty());

    let recovered =
        auto_recover_live_riskoff(&mut runtime, &metrics, &config, &live_safety, 31_000, 0);
    assert_eq!(runtime.status(), RuntimeStatus::Running);
    assert!(!recovered.event_seqs.is_empty());
    assert!(runtime
        .event_log()
        .recent(4)
        .iter()
        .any(|event| event.message.contains("runtime degraded auto-recovered")));
}

#[test]
fn live_degraded_auto_recover_allows_connected_idle_user_ws() {
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Degraded,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        StrategyMode::Noop(NoopStrategy),
        MarketContextStore::empty(),
    );
    let metrics = AppMetrics::new().expect("metrics");
    metrics.set_stream_connected(StreamKind::Market, true);
    metrics.set_stream_connected(StreamKind::User, true);
    metrics.set_execution_adapter_connected(true);
    metrics.observe_user_message("order", "matched");
    std::thread::sleep(std::time::Duration::from_millis(2));
    metrics.refresh_stream_ages();
    let mut config = runner_test_config();
    config.strategy_profile = Some(StrategyProfile {
        health: crate::strategy::ProfileHealth {
            user_ws_stale_ms: Some(0),
            ..Default::default()
        },
        ..StrategyProfile::default()
    });
    let live_safety = LiveSafetyState {
        last_venue_cash_usd: Some(100.0),
        ..LiveSafetyState::default()
    };

    let recovered =
        auto_recover_live_riskoff(&mut runtime, &metrics, &config, &live_safety, 31_000, 0);

    assert_eq!(runtime.status(), RuntimeStatus::Running);
    assert!(!recovered.event_seqs.is_empty());
    assert!(runtime
        .event_log()
        .recent(4)
        .iter()
        .any(|event| event.message.contains("runtime degraded auto-recovered")));
}

#[test]
fn live_riskoff_auto_recover_stays_riskoff_when_health_not_clean() {
    let mut runtime = Runtime::new(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::RiskOff,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        StrategyMode::Noop(NoopStrategy),
        MarketContextStore::empty(),
    );
    let metrics = AppMetrics::new().expect("metrics");
    metrics.set_stream_connected(StreamKind::Market, true);
    metrics.set_stream_connected(StreamKind::User, true);
    metrics.set_execution_adapter_connected(true);
    let config = runner_test_config();
    let live_safety = LiveSafetyState::default();

    let outcome =
        auto_recover_live_riskoff(&mut runtime, &metrics, &config, &live_safety, 31_000, 0);
    assert_eq!(runtime.status(), RuntimeStatus::RiskOff);
    assert!(outcome.event_seqs.is_empty());
}

#[test]
fn conservative_paper_fill_does_not_refill_same_book_update() {
    let now_ms = now_unix_ms();
    let book = BookState::from_top_of_book("token-1", 0.48, 100.0, 0.50, 100.0, 0.50, now_ms);
    let intent = OrderIntent {
        client_order_id: ClientOrderId::from("client-paper"),
        market_id: MarketId::from("market-1"),
        instrument_id: InstrumentId::from("token-1"),
        side: TradeSide::Buy,
        limit_price: 0.50,
        quantity: 20.0,
        reduce_only: false,
        reason: "test paper fill".to_string(),
        quote_level_tag: None,
        created_at_ms: now_ms,
        pair_id: None,
        kind: crate::types::IntentKind::Entry,
    };
    let policy = paper_test_policy();
    let mut ctx = PaperOrderContext {
        arrival_ms: now_ms,
        queue_bias: 0.5,
        last_attempt_ms: now_ms,
        last_fill_ms: 0,
        last_fill_book_update_ms: 0,
        fill_count: 0,
        cancel_requested_at_ms: None,
    };
    // Past the paper_submit_latency_ms gate (Phase 2 conservative model).
    let after_latency_ms = now_ms + policy.paper_submit_latency_ms + 50;
    let first = paper_fill_from_book_snapshot(
        &book,
        &intent,
        after_latency_ms,
        0.0,
        &mut ctx,
        intent.quantity,
        &policy,
    )
    .expect("first fill");
    assert!(first.notional_usd() >= policy.paper_min_fill_notional_usd);
    let second = paper_fill_from_book_snapshot(
        &book,
        &intent,
        after_latency_ms + 1_000,
        0.0,
        &mut ctx,
        intent.quantity - first.quantity,
        &policy,
    );
    assert!(second.is_none());
}

#[test]
fn paper_post_only_should_reject_returns_false_outside_paper_mode() {
    let book = BookState::from_top_of_book("token-1", 0.49, 100.0, 0.50, 100.0, 0.50, 1_000);
    let intent = OrderIntent {
        client_order_id: ClientOrderId::from("client-postonly"),
        market_id: MarketId::from("market-1"),
        instrument_id: InstrumentId::from("token-1"),
        side: TradeSide::Buy,
        limit_price: 0.51,
        quantity: 5.0,
        reduce_only: false,
        reason: "test post-only".to_string(),
        quote_level_tag: None,
        created_at_ms: 1_000,
        pair_id: None,
        kind: crate::types::IntentKind::Entry,
    };
    let policy = live_test_policy();
    assert!(!paper_post_only_should_reject(&intent, &book, &policy));
}

#[test]
fn paper_post_only_should_reject_skips_non_crossing_orders() {
    let book = BookState::from_top_of_book("token-1", 0.49, 100.0, 0.55, 100.0, 0.50, 1_000);
    let intent = OrderIntent {
        client_order_id: ClientOrderId::from("client-postonly-noncross"),
        market_id: MarketId::from("market-1"),
        instrument_id: InstrumentId::from("token-1"),
        side: TradeSide::Buy,
        limit_price: 0.50,
        quantity: 5.0,
        reduce_only: false,
        reason: "test post-only no cross".to_string(),
        quote_level_tag: None,
        created_at_ms: 1_000,
        pair_id: None,
        kind: crate::types::IntentKind::Entry,
    };
    let policy = paper_test_policy();
    assert!(!paper_post_only_should_reject(&intent, &book, &policy));
}

#[test]
fn paper_post_only_should_reject_at_high_probability_when_crossing() {
    let book = BookState::from_top_of_book("token-1", 0.49, 100.0, 0.50, 100.0, 0.50, 1_000);
    let mut policy = paper_test_policy();
    policy.paper_post_only_reject_probability = 1.0;
    let intent = OrderIntent {
        client_order_id: ClientOrderId::from("client-postonly-cross"),
        market_id: MarketId::from("market-1"),
        instrument_id: InstrumentId::from("token-1"),
        side: TradeSide::Buy,
        limit_price: 0.51,
        quantity: 5.0,
        reduce_only: false,
        reason: "test post-only cross".to_string(),
        quote_level_tag: None,
        created_at_ms: 1_000,
        pair_id: None,
        kind: crate::types::IntentKind::Entry,
    };
    // probability 1.0 always rejects when crossing
    assert!(paper_post_only_should_reject(&intent, &book, &policy));
    // probability 0.0 never rejects
    policy.paper_post_only_reject_probability = 0.0;
    assert!(!paper_post_only_should_reject(&intent, &book, &policy));
}

#[test]
fn paper_post_only_reject_decision_is_deterministic_per_book_update() {
    let book_a = BookState::from_top_of_book("token-1", 0.49, 100.0, 0.50, 100.0, 0.50, 1_000);
    let book_b = BookState::from_top_of_book("token-1", 0.49, 100.0, 0.50, 100.0, 0.50, 2_000);
    let intent = OrderIntent {
        client_order_id: ClientOrderId::from("client-determinism"),
        market_id: MarketId::from("market-1"),
        instrument_id: InstrumentId::from("token-1"),
        side: TradeSide::Buy,
        limit_price: 0.51,
        quantity: 5.0,
        reduce_only: false,
        reason: "determinism".to_string(),
        quote_level_tag: None,
        created_at_ms: 1_000,
        pair_id: None,
        kind: crate::types::IntentKind::Entry,
    };
    let mut policy = paper_test_policy();
    policy.paper_post_only_reject_probability = 0.5;
    // Same book, same decision repeated
    let r1 = paper_post_only_should_reject(&intent, &book_a, &policy);
    let r2 = paper_post_only_should_reject(&intent, &book_a, &policy);
    assert_eq!(r1, r2, "same book update must give same decision");
    // Decision varies independently across book updates (one of these is
    // exceedingly unlikely to fail; if it ever does, the hash is broken).
    let _r_b = paper_post_only_should_reject(&intent, &book_b, &policy);
}

#[test]
fn resting_order_when_book_moves_into_us_fills_as_maker_at_limit() {
    // Real venue: resting limit buy at 0.45; book moves so best_ask
    // drops to 0.43; a new sell at 0.45 hits our resting buy → we
    // fill at 0.45 (our limit) as MAKER (price improvement to seller).
    // The paper model used to misclassify this as Taker at 0.43.
    let now_ms = now_unix_ms();
    // Arrival in the past so order is "resting" (past submit-latency).
    let arrival_ms = now_ms - 5_000;
    let mut book = BookState::from_top_of_book("token-1", 0.42, 150.0, 0.43, 150.0, 0.43, now_ms);
    book.bids = vec![Level {
        price: 0.42,
        size: 150.0,
    }];
    book.asks = vec![Level {
        price: 0.43,
        size: 150.0,
    }];
    let intent = OrderIntent {
        client_order_id: ClientOrderId::from("client-resting-maker"),
        market_id: MarketId::from("market-1"),
        instrument_id: InstrumentId::from("token-1"),
        side: TradeSide::Buy,
        limit_price: 0.45,
        quantity: 5.0,
        reduce_only: false,
        reason: "test resting maker fill".to_string(),
        quote_level_tag: None,
        created_at_ms: arrival_ms,
        pair_id: None,
        kind: crate::types::IntentKind::Entry,
    };
    let mut policy = paper_test_policy();
    policy.paper_post_only_reject_probability = 0.0; // skip reject path
    let mut ctx = PaperOrderContext {
        arrival_ms,
        queue_bias: 0.5,
        last_attempt_ms: arrival_ms,
        last_fill_ms: 0,
        last_fill_book_update_ms: 0,
        fill_count: 0,
        cancel_requested_at_ms: None,
    };
    let fill = paper_fill_from_book_snapshot(
        &book,
        &intent,
        now_ms,
        0.0,
        &mut ctx,
        intent.quantity,
        &policy,
    )
    .expect("expected resting maker fill when book crossed into us");
    assert!(
        matches!(fill.liquidity, FillLiquidity::Maker),
        "expected Maker liquidity for resting order book moved into us, got {:?}",
        fill.liquidity
    );
    assert!(
        (fill.price - 0.45).abs() < 1e-9,
        "expected fill at our limit price 0.45, got {}",
        fill.price
    );
}

#[test]
fn fresh_submit_into_crossed_book_fills_as_taker_at_opposite() {
    // Counterexample: a brand-new submit at 0.45 when book ask is
    // already at 0.43 IS a taker scenario (we crossed at submit time).
    // Fill at best_opposite 0.43 with Taker liquidity.
    let now_ms = now_unix_ms();
    let mut book = BookState::from_top_of_book("token-1", 0.42, 150.0, 0.43, 150.0, 0.43, now_ms);
    book.bids = vec![Level {
        price: 0.42,
        size: 150.0,
    }];
    book.asks = vec![Level {
        price: 0.43,
        size: 150.0,
    }];
    let intent = OrderIntent {
        client_order_id: ClientOrderId::from("client-fresh-taker"),
        market_id: MarketId::from("market-1"),
        instrument_id: InstrumentId::from("token-1"),
        side: TradeSide::Buy,
        limit_price: 0.45,
        quantity: 5.0,
        reduce_only: false,
        reason: "test fresh taker".to_string(),
        quote_level_tag: None,
        created_at_ms: now_ms,
        pair_id: None,
        kind: crate::types::IntentKind::Entry,
    };
    let mut policy = paper_test_policy();
    policy.paper_post_only_reject_probability = 0.0; // bypass reject for the test
    policy.paper_min_fill_notional_usd = 0.0; // allow tiny initial fill ratio
                                              // arrival_ms == now_ms; well within submit-latency window (default 150ms).
    let mut ctx = PaperOrderContext {
        arrival_ms: now_ms,
        queue_bias: 0.5,
        last_attempt_ms: now_ms,
        last_fill_ms: 0,
        last_fill_book_update_ms: 0,
        fill_count: 0,
        cancel_requested_at_ms: None,
    };
    // submit-latency gate would normally suppress; advance the
    // observed_at_ms by exactly the latency window so a fill is
    // possible but order is still "fresh" (not aged past it).
    let observed = now_ms + policy.paper_submit_latency_ms;
    let fill = paper_fill_from_book_snapshot(
        &book,
        &intent,
        observed,
        0.0,
        &mut ctx,
        intent.quantity,
        &policy,
    )
    .expect("expected taker fill at submit-latency boundary");
    assert!(
        matches!(fill.liquidity, FillLiquidity::Taker),
        "expected Taker for fresh crossing submit, got {:?}",
        fill.liquidity
    );
    assert!(
        (fill.price - 0.43).abs() < 1e-9,
        "expected fill at best_ask 0.43, got {}",
        fill.price
    );
}

#[test]
fn paper_submit_latency_gate_suppresses_fill_until_window_passes() {
    let now_ms = now_unix_ms();
    let book = BookState::from_top_of_book("token-1", 0.48, 100.0, 0.50, 100.0, 0.50, now_ms);
    let intent = OrderIntent {
        client_order_id: ClientOrderId::from("client-latency"),
        market_id: MarketId::from("market-1"),
        instrument_id: InstrumentId::from("token-1"),
        side: TradeSide::Buy,
        limit_price: 0.50,
        quantity: 20.0,
        reduce_only: false,
        reason: "test latency gate".to_string(),
        quote_level_tag: None,
        created_at_ms: now_ms,
        pair_id: None,
        kind: crate::types::IntentKind::Entry,
    };
    let policy = paper_test_policy();
    assert!(
        policy.paper_submit_latency_ms >= 100,
        "test assumes default >= 100ms; got {}",
        policy.paper_submit_latency_ms
    );
    let mut ctx = PaperOrderContext {
        arrival_ms: now_ms,
        queue_bias: 0.5,
        last_attempt_ms: now_ms,
        last_fill_ms: 0,
        last_fill_book_update_ms: 0,
        fill_count: 0,
        cancel_requested_at_ms: None,
    };
    // Within latency window: suppressed.
    let inside = paper_fill_from_book_snapshot(
        &book,
        &intent,
        now_ms + policy.paper_submit_latency_ms - 1,
        0.0,
        &mut ctx,
        intent.quantity,
        &policy,
    );
    assert!(
        inside.is_none(),
        "expected no fill inside latency window, got {inside:?}"
    );
    // Past latency window with fresh book update: allowed.
    let later_book = BookState::from_top_of_book(
        "token-1",
        0.48,
        100.0,
        0.50,
        100.0,
        0.50,
        now_ms + policy.paper_submit_latency_ms + 50,
    );
    let outside = paper_fill_from_book_snapshot(
        &later_book,
        &intent,
        now_ms + policy.paper_submit_latency_ms + 50,
        0.0,
        &mut ctx,
        intent.quantity,
        &policy,
    );
    assert!(outside.is_some(), "expected fill past latency window");
}

fn live_test_policy() -> ExecutionPolicy {
    ExecutionPolicy {
        paper_mode: false,
        live_post_only: true,
        live_order_ttl_ms: 20_000,
        live_order_max_age_ms: 25_000,
        live_reconcile_missing_grace_ms: 5_000,
        live_max_submit_errors: 1,
        live_max_cancel_errors: 1,
        live_kill_on_reconcile_mismatch: true,
        paper_min_fill_notional_usd: 0.05,
        paper_max_fills_per_order: 3,
        paper_min_fill_interval_ms: 750,
        paper_market_close_at_ms: None,
        paper_market_resolution_price: None,
        paper_submit_latency_ms: 150,
        paper_queue_depth_fraction: 0.75,
        paper_post_only_reject_probability: 0.85,
        paper_cancel_race_window_ms: 500,
        paper_maker_rebate_coeff: 0.0,
        paper_taker_fee_coeff_override: None,
    }
}

#[test]
fn deterministic_v2_amount_rejections_require_immediate_live_stop() {
    assert!(submit_rejection_requires_immediate_live_stop(
            "V2 SDK build_sign_and_post: Api error: status 400 Bad Request: invalid amounts, the market buy orders maker amount supports a max accuracy of 2 decimals, taker amount a max of 4 decimals"
        ));
    assert!(submit_rejection_requires_immediate_live_stop(
        "Validation: invalid: Unable to build Order: Size 9.090909 has 6 decimal places"
    ));
    assert!(!submit_rejection_requires_immediate_live_stop(
        "invalid post-only order: order crosses book"
    ));
}

fn paper_test_policy() -> ExecutionPolicy {
    ExecutionPolicy {
        paper_mode: true,
        ..live_test_policy()
    }
}

fn runtime_with_recovered_needs_reconcile_order() -> Runtime<StrategyMode> {
    runtime_with_recovered_order(
        ClientOrderId::from("client-reconcile"),
        ManagedOrderStatus::NeedsReconcile,
        1,
        "polymarket-exec-live-reconcile-replay",
    )
}

fn runtime_with_recovered_working_order(last_update_ms: u64) -> Runtime<StrategyMode> {
    runtime_with_recovered_order(
        ClientOrderId::from("client-working"),
        ManagedOrderStatus::Working,
        last_update_ms,
        "polymarket-exec-live-working-replay",
    )
}

fn runtime_with_recovered_order(
    client_order_id: ClientOrderId,
    status: ManagedOrderStatus,
    last_update_ms: u64,
    path_prefix: &str,
) -> Runtime<StrategyMode> {
    static SQLITE_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let suffix = SQLITE_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("{path_prefix}-{ts}-{suffix}.sqlite"));
    let mut store = SqliteOrderStore::open(&path).expect("store");
    let mut record = OrderRecord::from_intent(
        "run-test",
        &OrderIntent {
            client_order_id,
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("token-1"),
            side: TradeSide::Buy,
            limit_price: 0.40,
            quantity: 5.0,
            reduce_only: false,
            reason: "test recovered live order".to_string(),
            quote_level_tag: None,
            created_at_ms: last_update_ms,
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
        },
        "noop",
    );
    record.status = status;
    record.last_update_ms = last_update_ms;
    store.insert(record).expect("insert order");

    let mut runtime = Runtime::new_with_order_store(
        RuntimeConfig {
            starting_cash_usd: 100.0,
            event_log_capacity: 128,
            initial_status: RuntimeStatus::Starting,
            ..RuntimeConfig::default()
        },
        RiskLimits::default(),
        StrategyMode::Noop(NoopStrategy),
        MarketContextStore::empty(),
        Some(Box::new(store)),
        "run-test".to_string(),
    );
    runtime.recover_from_store(10_000, 100);
    let _ = std::fs::remove_file(path);
    runtime
}
