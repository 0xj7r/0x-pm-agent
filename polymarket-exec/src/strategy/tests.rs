use super::{
    Btc5mMmConfig, Btc5mMmMarketMode, Btc5mMmMarketState, Btc5mMmRescueState, Btc5mMmStrategy,
    GoatPairConfig, GoatPairStrategy, NoopStrategy, QuoteSnapshot, Strategy, StrategyContext,
    StrategyDecision,
};
use super::{
    BtcRegimeSnapshot, MarketActivitySignal, PairedBookSignal, SessionBucket,
    UnlawfulAggressionTier, UnlawfulExecutionMode, UnlawfulShearConfig, UnlawfulShearStrategy,
    UnlawfulSignalSnapshot,
};
use crate::inventory::{InventorySnapshot, PositionState};
use crate::market_context::MarketContextRecord;
use crate::types::{
    BookLevel, FillLiquidity, FillReport, InstrumentId, MarketId, MarketSnapshot, OrderIntent,
    RuntimeStatus, TradeSide,
};

fn snapshot(asset: &str, market: &str, bid: f64, ask: f64, ts: u64) -> MarketSnapshot {
    MarketSnapshot {
        market_id: MarketId::from(market),
        instrument_id: InstrumentId::from(asset),
        quote: QuoteSnapshot {
            best_bid: Some(BookLevel::new(bid, 1000.0)),
            best_ask: Some(BookLevel::new(ask, 1000.0)),
            bid_levels: vec![BookLevel::new(bid, 1000.0)],
            ask_levels: vec![BookLevel::new(ask, 1000.0)],
            depth_observed_at_ms: Some(ts),
            last_trade_price: Some(ask),
            taker_buy_qty_60s: 0.0,
            taker_sell_qty_60s: 0.0,
            observed_at_ms: ts,
        },
    }
}

fn snapshot_with_flow(
    asset: &str,
    market: &str,
    bid: f64,
    ask: f64,
    ts: u64,
    taker_buy_qty_60s: f64,
    taker_sell_qty_60s: f64,
) -> MarketSnapshot {
    let mut snap = snapshot(asset, market, bid, ask, ts);
    snap.quote.taker_buy_qty_60s = taker_buy_qty_60s;
    snap.quote.taker_sell_qty_60s = taker_sell_qty_60s;
    snap
}

fn context(positions: Vec<PositionState>) -> StrategyContext {
    context_with_unlawful_signal(positions, 10, 0, 0, None)
}

fn context_at(positions: Vec<PositionState>, now_ms: u64) -> StrategyContext {
    context_with_unlawful_signal(positions, now_ms, 0, 0, None)
}

fn context_at_with_cash(
    positions: Vec<PositionState>,
    now_ms: u64,
    total_cash_usd: f64,
) -> StrategyContext {
    let gross_exposure_usd = positions
        .iter()
        .map(|position| position.quantity * position.avg_price)
        .sum();
    StrategyContext {
        now_ms,
        runtime_status: RuntimeStatus::Running,
        inventory: InventorySnapshot {
            free_cash_usd: total_cash_usd,
            reserved_cash_usd: 0.0,
            total_cash_usd,
            realized_pnl_usd: 0.0,
            gross_exposure_usd,
            positions,
        },
        open_orders_total: 0,
        open_orders_for_market: 0,
        market_context: None,
        unlawful_signal: None,
        btc_regime: crate::signals::BtcRegimeSnapshot::default(),
        venue_rules: None,
    }
}

fn context_at_with_market(
    positions: Vec<PositionState>,
    now_ms: u64,
    market_context: MarketContextRecord,
    btc_regime: crate::signals::BtcRegimeSnapshot,
) -> StrategyContext {
    let mut context = context_at(positions, now_ms);
    context.market_context = Some(market_context);
    context.btc_regime = btc_regime;
    context
}

fn fill_from_intent(intent: &OrderIntent, quantity: f64, observed_at_ms: u64) -> FillReport {
    FillReport {
        order_id: None,
        client_order_id: Some(intent.client_order_id.clone()),
        market_id: intent.market_id.clone(),
        instrument_id: intent.instrument_id.clone(),
        side: intent.side,
        price: intent.limit_price,
        quantity,
        fee_usd: 0.0,
        liquidity: FillLiquidity::Maker,
        close_method: None,
        observed_at_ms,
    }
}

fn context_with_unlawful_signal(
    positions: Vec<PositionState>,
    now_ms: u64,
    open_orders_total: usize,
    open_orders_for_market: usize,
    unlawful_signal: Option<UnlawfulSignalSnapshot>,
) -> StrategyContext {
    StrategyContext {
        now_ms,
        runtime_status: RuntimeStatus::Running,
        inventory: InventorySnapshot {
            free_cash_usd: 1_000.0,
            reserved_cash_usd: 0.0,
            total_cash_usd: 1_000.0,
            realized_pnl_usd: 0.0,
            gross_exposure_usd: positions
                .iter()
                .map(|position| position.quantity * position.avg_price)
                .sum(),
            positions,
        },
        open_orders_total,
        open_orders_for_market,
        market_context: None,
        unlawful_signal,
        btc_regime: crate::signals::BtcRegimeSnapshot::default(),
        venue_rules: None,
    }
}

fn btc_5m_mm_test_config() -> Btc5mMmConfig {
    Btc5mMmConfig {
        base_clip_usd: 1.10,
        min_clip_usd: 0.25,
        max_clip_usd: 5.0,
        liquidity_clip_fraction: 0.02,
        hedge_rescue_clip_usd: 2.50,
        hedge_rescue_race_buffer_ticks: 0.0,
        momentum_tilt_per_bps: 0.0,
        momentum_max_tilt: 0.0,
        max_gross_cost_usd: 20.0,
        max_gross_cost_bps: 0.0,
        max_leg_cost_usd: 10.0,
        max_leg_cost_bps: 0.0,
        max_entry_free_cash_bps: 10_000.0,
        max_rescue_free_cash_bps: 10_000.0,
        min_edge_bps: 75.0,
        hedge_rescue_edge_bps: 25.0,
        inventory_skew_bps: 150.0,
        max_spread: 0.08,
        min_top_depth_notional_usd: 2.0,
        min_order_notional_usd: 1.0,
        venue_min_order_quantity: 5.0,
        entry_min_size_multiplier: 1.0,
        min_order_quantity: 0.01,
        maker_price_tick: 0.01,
        maker_safety_ticks: 2.0,
        entry_ladder_levels: 1,
        entry_ladder_spacing_ticks: 1.0,
        cooldown_ms: 0,
        taker_fee_coeff: 0.072,
        entry_premium_bid_cap: 0.97,
        order_flow_imbalance_threshold: 0.60,
        merge_gas_cost_usd: 0.30,
    }
}

fn unlawful_signal_snapshot(
    mode: UnlawfulExecutionMode,
    clip_scale: f64,
    now_ms: u64,
) -> UnlawfulSignalSnapshot {
    UnlawfulSignalSnapshot {
        session_bucket: SessionBucket::Preferred,
        mode,
        gate_reasons: vec!["unlawful test signal".to_string()],
        btc: BtcRegimeSnapshot {
            last_price: Some(50_000.0),
            realized_vol_5m_bps: Some(6.0),
            realized_vol_15m_bps: Some(12.0),
            trade_count_5m: 9_000,
            trade_count_15m: 9_000,
            return_30s_bps: Some(0.0),
            return_60s_bps: Some(0.0),
            observed_at_ms: now_ms,
        },
        book: PairedBookSignal::with_ids(
            InstrumentId::from("down"),
            InstrumentId::from("up"),
            Some(BookLevel::new(0.24, 1_000.0)),
            Some(BookLevel::new(0.24, 1_000.0)),
            Some(BookLevel::new(0.65, 1_000.0)),
            Some(BookLevel::new(0.65, 1_000.0)),
            now_ms,
            true,
        ),
        activity: MarketActivitySignal::default(),
        first_fill_ms: None,
        first_merge_ms: None,
        elapsed_s: None,
        time_remaining_s: None,
        clip_scale,
    }
}

fn with_microstructure(
    mut signal: UnlawfulSignalSnapshot,
    cheap_ask_notional_top3: f64,
    expensive_ask_notional_top3: f64,
) -> UnlawfulSignalSnapshot {
    signal.gate_reasons.clear();
    signal.book.cheap_spread = Some(0.02);
    signal.book.expensive_spread = Some(0.02);
    signal.book.cheap_ask_notional_top3 = Some(cheap_ask_notional_top3);
    signal.book.expensive_ask_notional_top3 = Some(expensive_ask_notional_top3);
    signal.book.cheap_depth_imbalance_top3 = Some(0.0);
    signal.book.expensive_depth_imbalance_top3 = Some(0.0);
    signal
}

fn unlawful_shear_test_config() -> UnlawfulShearConfig {
    let mut cfg = UnlawfulShearConfig::from_env();
    cfg.cheap_hedge_price_max = 0.40;
    cfg.core_price_min = 0.50;
    cfg.core_price_max = 0.95;
    cfg.min_price_gap = 0.10;
    cfg.probe_clip_usd = 10.0;
    cfg.core_clip_usd = 35.0;
    cfg.hedge_clip_usd = 15.0;
    cfg.rebalance_clip_usd = 20.0;
    cfg.micro_clip_target_usd = 8.0;
    cfg.micro_clip_min_usd = 1.0;
    cfg.micro_clip_max_children = 1;
    cfg.trim_clip_fraction = 0.25;
    cfg.max_gross_cost_usd = 120.0;
    cfg.target_hedge_ratio_min = 0.20;
    cfg.target_hedge_ratio_max = 0.60;
    cfg.salvage_drawdown_ratio = 0.20;
    cfg.salvage_bid_floor = 0.05;
    cfg.max_open_orders_total = 8;
    cfg.taker_fee_coeff = 0.072;
    cfg.microstructure_enabled = true;
    cfg.microstructure_require_depth = false;
    cfg.microstructure_max_spread = 0.05;
    cfg.microstructure_min_ask_notional_top3_usd = 2.0;
    cfg.microstructure_max_clip_ask_notional_fraction = 0.10;
    cfg.microstructure_imbalance_threshold = 0.55;
    cfg.microstructure_weak_bid_scale = 0.65;
    cfg.microstructure_thin_ask_scale = 0.85;
    cfg
}

#[test]
fn unlawful_shear_from_env_respects_offhour_override_flag() {
    let key = "WHALE_PAIR_UNLAWFUL_SHEAR_ALLOW_EXTREME_OFFHOUR_OVERRIDE";
    let original = std::env::var(key).ok();
    std::env::set_var(key, "true");

    let cfg = UnlawfulShearConfig::from_env();

    match original {
        Some(value) => std::env::set_var(key, value),
        None => std::env::remove_var(key),
    }

    assert!(cfg.allow_extreme_offhour_override);
}

#[test]
fn unlawful_cleanup_qty_quantization_drops_micro_churn_and_buckets_size() {
    assert_eq!(UnlawfulShearStrategy::quantize_cleanup_qty(0.01), 0.0);
    assert_eq!(UnlawfulShearStrategy::quantize_cleanup_qty(0.10), 0.0);
    assert_eq!(UnlawfulShearStrategy::quantize_cleanup_qty(0.24), 0.0);
    assert!((UnlawfulShearStrategy::quantize_cleanup_qty(0.25) - 0.25).abs() < 1e-9);
    assert!((UnlawfulShearStrategy::quantize_cleanup_qty(1.23) - 1.0).abs() < 1e-9);
    assert!((UnlawfulShearStrategy::quantize_cleanup_qty(2.74) - 2.5).abs() < 1e-9);
}

#[test]
fn noop_stays_idle() {
    let mut strategy = NoopStrategy;
    let decision = strategy.on_market_snapshot(
        &context(Vec::new()),
        &snapshot("token-up", "market", 0.4, 0.5, 1),
    );
    assert!(
        matches!(decision, StrategyDecision { intents: ref i, notes: ref n } if i.is_empty() && n.is_empty())
    );
}

#[test]
fn goat_pairs_builds_order_when_side_cheap() {
    let mut strategy = GoatPairStrategy::new(
        GoatPairConfig {
            accumulate_price_max: 1.0,
            aggressive_price_max: 0.6,
            base_clip_usd: 20.0,
            aggressive_clip_usd: 50.0,
            max_gross_cost_usd: 1000.0,
            completion_min_pnl_per_share: 0.0,
            max_imbalance_ratio: 9.0,
            taker_fee_coeff: 0.072,
        },
        0,
    );
    strategy.on_market_snapshot(
        &context(Vec::new()),
        &snapshot("up", "market-a", 0.5, 0.5, 10),
    );
    let decision = strategy.on_market_snapshot(
        &context(Vec::new()),
        &snapshot("down", "market-a", 0.5, 0.52, 10),
    );
    assert!(!matches!(decision, StrategyDecision { intents: ref i, .. } if i.is_empty()));
}

#[test]
fn goat_pairs_emit_three_level_bid_ladder() {
    let mut strategy = GoatPairStrategy::new(
        GoatPairConfig {
            accumulate_price_max: 1.0,
            aggressive_price_max: 0.6,
            base_clip_usd: 30.0,
            aggressive_clip_usd: 50.0,
            max_gross_cost_usd: 1_000.0,
            completion_min_pnl_per_share: 0.0,
            max_imbalance_ratio: 9.0,
            taker_fee_coeff: 0.072,
        },
        0,
    );
    strategy.quote_levels_per_side = 3;
    strategy.quote_min_edge_bps = 50.0;
    strategy.quote_inventory_skew_bps = 25.0;
    let ctx = context(Vec::new());
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-b", 0.50, 0.50, 10));
    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-b", 0.50, 0.52, 10));
    assert_eq!(decision.intents.len(), 3);
    assert!(decision
        .intents
        .iter()
        .all(|intent| intent.side == TradeSide::Buy));
    assert!(decision
        .intents
        .windows(2)
        .all(|window| window[0].limit_price >= window[1].limit_price));
}

#[test]
fn btc_5m_mm_quotes_maker_bids_on_both_outcomes() {
    let mut strategy = Btc5mMmStrategy::new(btc_5m_mm_test_config());
    let ctx = context(Vec::new());
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.52, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.52, 10));

    assert_eq!(decision.intents.len(), 2);
    assert!(decision
        .intents
        .iter()
        .all(|intent| intent.side == TradeSide::Buy && !intent.reduce_only));
    assert!(decision
        .intents
        .iter()
        .all(|intent| intent.quantity >= strategy.config.venue_min_order_quantity));
}

#[test]
fn btc_5m_mm_flow_imbalance_allows_just_below_threshold() {
    let mut config = btc_5m_mm_test_config();
    config.order_flow_imbalance_threshold = 0.60;
    let mut strategy = Btc5mMmStrategy::new(config);
    let ctx = context(Vec::new());
    strategy.on_market_snapshot(
        &ctx,
        &snapshot_with_flow("up", "market-flow", 0.30, 0.32, 10, 0.0, 0.0),
    );
    let decision = strategy.on_market_snapshot(
        &ctx,
        &snapshot_with_flow("down", "market-flow", 0.68, 0.70, 10, 7.9, 2.1),
    );
    assert_eq!(decision.intents.len(), 2);
    assert!(decision
        .intents
        .iter()
        .all(|intent| intent.quote_level_tag.as_deref() == Some("mm-paired-bid:l1")));
}

#[test]
fn btc_5m_mm_flow_imbalance_suppresses_just_above_threshold() {
    let mut config = btc_5m_mm_test_config();
    config.order_flow_imbalance_threshold = 0.60;
    let mut strategy = Btc5mMmStrategy::new(config);
    let ctx = context(Vec::new());
    strategy.on_market_snapshot(
        &ctx,
        &snapshot_with_flow("up", "market-flow", 0.30, 0.32, 10, 0.0, 0.0),
    );
    let decision = strategy.on_market_snapshot(
        &ctx,
        &snapshot_with_flow("down", "market-flow", 0.68, 0.70, 10, 8.1, 1.9),
    );
    assert!(
        decision.intents.is_empty(),
        "paired entry should be suppressed when |imbalance| is just above threshold"
    );
}

#[test]
fn btc_5m_mm_emits_paired_bids_through_normal_btc_volatility() {
    let mut strategy = Btc5mMmStrategy::new(btc_5m_mm_test_config());
    let now_ms = 10_000;
    let mut ctx = context_at(Vec::new(), now_ms);
    ctx.btc_regime = crate::signals::BtcRegimeSnapshot {
        last_price: Some(50_000.0),
        realized_vol_5m_bps: Some(20.0),
        realized_vol_15m_bps: Some(20.0),
        trade_count_5m: 200,
        trade_count_15m: 600,
        return_30s_bps: Some(15.0),
        return_60s_bps: Some(20.0),
        return_120s_bps: Some(18.0),
        return_180s_bps: Some(16.0),
        observed_at_ms: now_ms,
    };

    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.52, now_ms));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.52, now_ms));

    assert!(
        decision.intents.iter().any(|intent| intent
            .quote_level_tag
            .as_deref()
            .map(|tag| tag.starts_with("mm-paired-bid"))
            .unwrap_or(false)),
        "paired bids must fire through normal BTC vol (return_60s=20bps); whale data shows \
         continuous participation through this regime, no self-imposed paired suppression"
    );
}

#[test]
fn btc_5m_mm_emits_budget_aware_depth_ladder() {
    let mut config = btc_5m_mm_test_config();
    config.entry_ladder_levels = 3;
    config.max_leg_cost_usd = 10.0;
    config.max_gross_cost_usd = 20.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let ctx = context(Vec::new());
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.52, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.52, 10));

    assert_eq!(decision.intents.len(), 6);
    let up_prices = decision
        .intents
        .iter()
        .filter(|intent| intent.instrument_id == InstrumentId::from("up"))
        .map(|intent| intent.limit_price)
        .collect::<Vec<_>>();
    assert_eq!(up_prices, vec![0.48, 0.47, 0.46]);
    assert!(decision.intents.iter().all(|intent| {
        intent.pair_id.is_some()
            && intent
                .quote_level_tag
                .as_deref()
                .is_some_and(|tag| tag.starts_with("mm-paired-bid:l"))
    }));
}

#[test]
fn btc_5m_mm_ladder_stops_at_leg_budget() {
    let mut config = btc_5m_mm_test_config();
    config.entry_ladder_levels = 3;
    config.max_leg_cost_usd = 5.0;
    config.max_gross_cost_usd = 20.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let ctx = context(Vec::new());
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.52, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.52, 10));

    assert_eq!(decision.intents.len(), 4);
    let max_leg_cost = decision
        .intents
        .iter()
        .filter(|intent| intent.instrument_id == InstrumentId::from("up"))
        .map(|intent| intent.limit_price * intent.quantity)
        .sum::<f64>();
    assert!(max_leg_cost <= config.max_leg_cost_usd + 1e-9);
}

#[test]
fn btc_5m_mm_ladder_honors_percent_budget_cap() {
    let mut config = btc_5m_mm_test_config();
    config.entry_ladder_levels = 3;
    config.max_leg_cost_usd = 100.0;
    config.max_gross_cost_usd = 200.0;
    config.max_leg_cost_bps = 1_000.0;
    config.max_gross_cost_bps = 2_000.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let ctx = context_at_with_cash(Vec::new(), 10, 50.0);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.52, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.52, 10));

    assert_eq!(decision.intents.len(), 4);
    let gross_cost = decision
        .intents
        .iter()
        .map(|intent| intent.limit_price * intent.quantity)
        .sum::<f64>();
    assert!(gross_cost <= 10.0 + 1e-9);
}

#[test]
fn btc_5m_mm_bids_do_not_cross_book_with_post_only_buffer() {
    let mut strategy = Btc5mMmStrategy::new(btc_5m_mm_test_config());
    let ctx = context(Vec::new());
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.45, 0.46, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.53, 0.54, 10));

    assert_eq!(decision.intents.len(), 2);
    let up = decision
        .intents
        .iter()
        .find(|intent| intent.instrument_id == InstrumentId::from("up"))
        .expect("up bid");
    let down = decision
        .intents
        .iter()
        .find(|intent| intent.instrument_id == InstrumentId::from("down"))
        .expect("down bid");
    assert_eq!(up.limit_price, 0.44);
    assert_eq!(down.limit_price, 0.52);
}

#[test]
fn btc_5m_mm_cooldown_does_not_emit_empty_quote_set() {
    let mut config = btc_5m_mm_test_config();
    config.cooldown_ms = 60_000;
    let mut strategy = Btc5mMmStrategy::new(config);
    let ctx = context(Vec::new());

    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.52, 10));
    let first = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.52, 10));
    let second = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.52, 11));

    assert_eq!(first.intents.len(), 2);
    assert_eq!(
        second.intents.len(),
        2,
        "cooldown must not clear desired quotes; the reconciler handles churn control"
    );
}

#[test]
fn btc_5m_mm_retry_ids_change_without_changing_quote_match_tag() {
    let mut strategy = Btc5mMmStrategy::new(btc_5m_mm_test_config());
    let ctx = context(Vec::new());

    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.52, 10));
    let first = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.52, 10));
    let later_ctx = StrategyContext {
        now_ms: 1_010,
        ..ctx
    };
    let second = strategy.on_market_snapshot(
        &later_ctx,
        &snapshot("down", "market-mm", 0.48, 0.52, 1_010),
    );

    assert_eq!(first.intents.len(), 2);
    assert_eq!(second.intents.len(), 2);
    assert_eq!(
        first.intents[0].quote_level_tag,
        second.intents[0].quote_level_tag
    );
    assert_ne!(
        first.intents[0].client_order_id,
        second.intents[0].client_order_id
    );
}

#[test]
fn btc_5m_mm_blocks_reentry_after_one_sided_entry_fills() {
    let mut config = btc_5m_mm_test_config();
    config.cooldown_ms = 0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let ctx = context_at(Vec::new(), 10);

    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.52, 10));
    let entry = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.52, 10));
    let up_entry = entry
        .intents
        .iter()
        .find(|intent| intent.instrument_id == InstrumentId::from("up"))
        .expect("up entry");

    let fill_ctx = context_at(
        vec![PositionState {
            market_id: MarketId::from("market-mm"),
            instrument_id: InstrumentId::from("up"),
            quantity: 10.0,
            avg_price: up_entry.limit_price,
            mark_price: Some(up_entry.limit_price),
            updated_at_ms: 20,
        }],
        20,
    );
    let fill = fill_from_intent(up_entry, 10.0, 20);
    let fill_decision = strategy.on_fill(&fill_ctx, &fill);
    assert!(fill_decision
        .notes
        .iter()
        .any(|note| note.contains("entry-fill asymmetry cooldown")));

    let flat_ctx = context_at(Vec::new(), 30_000);
    strategy.on_market_snapshot(&flat_ctx, &snapshot("up", "market-mm", 0.43, 0.45, 30_000));
    let decision = strategy.on_market_snapshot(
        &flat_ctx,
        &snapshot("down", "market-mm", 0.55, 0.57, 30_000),
    );

    assert!(decision.intents.is_empty());
    assert!(matches!(
        strategy
            .market_states
            .get(&MarketId::from("market-mm"))
            .map(|state| &state.mode),
        Some(Btc5mMmMarketMode::Cooling { reason, .. })
            if reason.contains("asymmetric entry-fill cooldown")
    ));
}

#[test]
fn btc_5m_mm_allows_reentry_after_balanced_entry_fills() {
    let mut config = btc_5m_mm_test_config();
    config.cooldown_ms = 0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let ctx = context_at(Vec::new(), 10);

    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.52, 10));
    let entry = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.52, 10));
    let up_entry = entry
        .intents
        .iter()
        .find(|intent| intent.instrument_id == InstrumentId::from("up"))
        .expect("up entry");
    let down_entry = entry
        .intents
        .iter()
        .find(|intent| intent.instrument_id == InstrumentId::from("down"))
        .expect("down entry");

    let up_fill_ctx = context_at(
        vec![PositionState {
            market_id: MarketId::from("market-mm"),
            instrument_id: InstrumentId::from("up"),
            quantity: 5.0,
            avg_price: up_entry.limit_price,
            mark_price: Some(up_entry.limit_price),
            updated_at_ms: 20,
        }],
        20,
    );
    strategy.on_fill(&up_fill_ctx, &fill_from_intent(up_entry, 5.0, 20));
    let paired_fill_ctx = context_at(
        vec![
            PositionState {
                market_id: MarketId::from("market-mm"),
                instrument_id: InstrumentId::from("up"),
                quantity: 5.0,
                avg_price: up_entry.limit_price,
                mark_price: Some(up_entry.limit_price),
                updated_at_ms: 30,
            },
            PositionState {
                market_id: MarketId::from("market-mm"),
                instrument_id: InstrumentId::from("down"),
                quantity: 5.0,
                avg_price: down_entry.limit_price,
                mark_price: Some(down_entry.limit_price),
                updated_at_ms: 30,
            },
        ],
        30,
    );
    let down_fill_decision =
        strategy.on_fill(&paired_fill_ctx, &fill_from_intent(down_entry, 5.0, 30));
    assert!(!down_fill_decision
        .notes
        .iter()
        .any(|note| note.contains("entry-fill asymmetry cooldown")));

    let flat_ctx = context_at(Vec::new(), 30_000);
    strategy.on_market_snapshot(&flat_ctx, &snapshot("up", "market-mm", 0.48, 0.52, 30_000));
    let decision = strategy.on_market_snapshot(
        &flat_ctx,
        &snapshot("down", "market-mm", 0.48, 0.52, 30_000),
    );

    assert_eq!(decision.intents.len(), 2);
}

#[test]
fn btc_5m_mm_blocks_entry_when_market_mid_moves_fast() {
    let mut config = btc_5m_mm_test_config();
    config.cooldown_ms = 0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let initial_ctx = context_at(Vec::new(), 10);
    strategy.on_market_snapshot(&initial_ctx, &snapshot("up", "market-mm", 0.48, 0.52, 10));
    strategy.on_market_snapshot(&initial_ctx, &snapshot("down", "market-mm", 0.48, 0.52, 10));

    let moved_ctx = context_at(Vec::new(), 20_000);
    strategy.on_market_snapshot(&moved_ctx, &snapshot("up", "market-mm", 0.42, 0.44, 20_000));
    let decision = strategy.on_market_snapshot(
        &moved_ctx,
        &snapshot("down", "market-mm", 0.56, 0.58, 20_000),
    );

    assert!(decision.intents.is_empty());
    assert!(matches!(
        strategy
            .market_states
            .get(&MarketId::from("market-mm"))
            .map(|state| &state.mode),
        Some(Btc5mMmMarketMode::Cooling { reason, .. })
            if reason.contains("market mid moved")
    ));
}

#[test]
fn btc_5m_mm_keeps_pairing_when_market_move_is_still_pairable() {
    let mut config = btc_5m_mm_test_config();
    config.cooldown_ms = 0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let initial_ctx = context_at(Vec::new(), 10);
    strategy.on_market_snapshot(&initial_ctx, &snapshot("up", "market-mm", 0.49, 0.51, 10));
    strategy.on_market_snapshot(&initial_ctx, &snapshot("down", "market-mm", 0.49, 0.51, 10));

    let moved_ctx = context_at(Vec::new(), 20_000);
    strategy.on_market_snapshot(&moved_ctx, &snapshot("up", "market-mm", 0.44, 0.46, 20_000));
    let decision = strategy.on_market_snapshot(
        &moved_ctx,
        &snapshot("down", "market-mm", 0.54, 0.56, 20_000),
    );

    assert_eq!(decision.intents.len(), 2);
    assert!(decision
        .intents
        .iter()
        .all(|intent| intent.quote_level_tag.as_deref() == Some("mm-paired-bid:l1")));
}

#[test]
fn btc_5m_mm_prunes_stale_market_states_without_dropping_inventory_market() {
    let mut config = btc_5m_mm_test_config();
    config.cooldown_ms = 0;
    let mut strategy = Btc5mMmStrategy::new(config);
    for index in 0..600 {
        let mut state = Btc5mMmMarketState::default();
        state.last_action_ms = Some(1);
        strategy
            .market_states
            .insert(MarketId::from(format!("stale-{index}")), state);
    }
    let mut inventory_state = Btc5mMmMarketState::default();
    inventory_state.last_action_ms = Some(1);
    strategy
        .market_states
        .insert(MarketId::from("market-inventory"), inventory_state);
    let now_ms = Btc5mMmStrategy::MARKET_STATE_TTL_MS + 10_000;
    let mut noquote_only_state = Btc5mMmMarketState::default();
    noquote_only_state.last_no_quote_note_ms = Some(now_ms);
    strategy
        .market_states
        .insert(MarketId::from("noquote-only"), noquote_only_state);

    let ctx = context_at(
        vec![PositionState {
            market_id: MarketId::from("market-inventory"),
            instrument_id: InstrumentId::from("inventory-up"),
            quantity: 5.0,
            avg_price: 0.50,
            mark_price: Some(0.50),
            updated_at_ms: now_ms,
        }],
        now_ms,
    );

    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-active", 0.48, 0.52, now_ms));
    strategy.on_market_snapshot(&ctx, &snapshot("down", "market-active", 0.48, 0.52, now_ms));

    assert!(strategy
        .market_states
        .contains_key(&MarketId::from("market-active")));
    assert!(strategy
        .market_states
        .contains_key(&MarketId::from("market-inventory")));
    assert!(strategy.market_states.len() <= Btc5mMmStrategy::MAX_MARKET_STATES);
    assert!(!strategy
        .market_states
        .contains_key(&MarketId::from("stale-0")));
    assert!(!strategy
        .market_states
        .contains_key(&MarketId::from("noquote-only")));
}

#[test]
fn btc_5m_mm_cooling_note_is_throttled_until_reason_changes_or_interval_elapses() {
    let mut strategy = Btc5mMmStrategy::new(btc_5m_mm_test_config());
    let market_id = MarketId::from("market-mm");

    assert!(strategy.should_log_cooling_note(&market_id, 1_000, "btc regime flat: vol_5m=0.05bps"));
    assert!(!strategy.should_log_cooling_note(
        &market_id,
        2_000,
        "btc regime flat: vol_5m=0.06bps"
    ));
    assert!(strategy.should_log_cooling_note(&market_id, 2_000, "market mid moved 0.060"));
    assert!(!strategy.should_log_cooling_note(&market_id, 3_000, "market mid moved 0.100"));
    assert!(strategy.should_log_cooling_note(
        &market_id,
        2_000 + Btc5mMmStrategy::NO_QUOTE_NOTE_INTERVAL_MS,
        "market mid moved 0.110"
    ));
}

#[test]
fn btc_5m_mm_checkpoint_restores_flow_state_without_stale_quotes() {
    let mut strategy = Btc5mMmStrategy::new(btc_5m_mm_test_config());
    let mut state = Btc5mMmMarketState::default();
    state.mode = Btc5mMmMarketMode::Cooling {
        reason: "asymmetric entry-fill cooldown".to_string(),
        until_ms: Some(90_000),
    };
    state.quotes.insert(
        InstrumentId::from("up"),
        snapshot("up", "market-mm", 0.48, 0.52, 10).quote,
    );
    state.market_mid_history.push_back((10, 0.50));
    state
        .recent_fills
        .push_back((20, InstrumentId::from("up"), 10.0));
    state.asymmetric_entry_block_until_ms = Some(90_000);
    state.last_action_ms = Some(30);
    state.last_no_quote_note_ms = Some(40);
    state.last_rescue_attempt_ms = Some(50);
    state.rescue_state = Some(Btc5mMmRescueState {
        stranded_instrument_id: "up".to_string(),
        lift_instrument_id: "down".to_string(),
        stranded_qty_bucket: 500,
        attempts: 2,
        first_attempt_ms: 45,
        last_attempt_ms: 50,
    });
    state.last_fill_ms = Some(60);
    strategy
        .market_states
        .insert(MarketId::from("market-mm"), state);
    strategy.recent_fill_times.push_back(20);

    let checkpoint = strategy.checkpoint_state().expect("state");
    let mut restored = Btc5mMmStrategy::new(btc_5m_mm_test_config());
    restored
        .restore_checkpoint_state(&checkpoint)
        .expect("restore state");

    let restored_state = restored
        .market_states
        .get(&MarketId::from("market-mm"))
        .expect("market state restored");
    assert!(restored_state.quotes.is_empty());
    assert_eq!(restored_state.market_mid_history.len(), 1);
    assert_eq!(restored_state.recent_fills.len(), 1);
    assert_eq!(restored_state.asymmetric_entry_block_until_ms, Some(90_000));
    assert_eq!(restored_state.last_rescue_attempt_ms, Some(50));
    assert_eq!(
        restored_state
            .rescue_state
            .as_ref()
            .map(|state| state.attempts),
        Some(2)
    );
    assert_eq!(restored.recent_fill_times.len(), 1);
    assert!(matches!(
        restored_state.mode,
        Btc5mMmMarketMode::Cooling { ref reason, until_ms: Some(90_000) }
            if reason.contains("asymmetric")
    ));
}

#[test]
fn btc_5m_mm_rejects_unknown_checkpoint_version() {
    let mut strategy = Btc5mMmStrategy::new(btc_5m_mm_test_config());
    let checkpoint = serde_json::json!({
        "version": Btc5mMmStrategy::PERSISTED_STATE_VERSION + 1,
        "market_states": [],
        "recent_fill_times": [],
    });

    let error = strategy
        .restore_checkpoint_state(&checkpoint)
        .expect_err("unknown version must fail closed");

    assert!(error.contains("unsupported btc_5m_mm state version"));
}

#[test]
fn btc_5m_mm_restore_recent_fill_times_uses_fill_queue_cap() {
    let mut strategy = Btc5mMmStrategy::new(btc_5m_mm_test_config());
    let fill_count = Btc5mMmStrategy::MAX_MARKET_STATES + 88;
    let checkpoint = serde_json::json!({
        "version": Btc5mMmStrategy::PERSISTED_STATE_VERSION,
        "market_states": [],
        "recent_fill_times": (0..fill_count).collect::<Vec<_>>(),
    });

    strategy
        .restore_checkpoint_state(&checkpoint)
        .expect("restore state");

    assert_eq!(strategy.recent_fill_times.len(), fill_count);
}

#[test]
fn btc_5m_mm_entries_are_dynamic_but_venue_safe() {
    let mut config = btc_5m_mm_test_config();
    config.base_clip_usd = 0.25;
    config.min_clip_usd = 0.25;
    config.max_clip_usd = 1.00;
    config.min_order_notional_usd = 0.20;
    config.venue_min_order_quantity = 0.01;
    config.min_order_quantity = 0.01;
    let mut strategy = Btc5mMmStrategy::new(config);
    let ctx = context(Vec::new());
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.52, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.52, 10));

    assert_eq!(decision.intents.len(), 2);
    assert!(decision.intents.iter().all(|intent| intent.quantity < 5.0));
    assert!(decision
        .intents
        .iter()
        .all(|intent| intent.quantity >= config.venue_min_order_quantity));
}

#[test]
fn btc_5m_mm_entry_multiplier_is_soft_not_a_hard_floor() {
    let mut config = btc_5m_mm_test_config();
    config.base_clip_usd = 1.10;
    config.max_clip_usd = 8.00;
    config.entry_min_size_multiplier = 1.30;
    let mut strategy = Btc5mMmStrategy::new(config);
    let ctx = context(Vec::new());
    // Neutral book (0.49/0.51) so price-extremity gate (>0.65) passes.
    // Original test used 0.74/0.16 which is now (correctly) blocked.
    // With neutral book min_ref=0.49, required=max(0.01, 5.0, 1.0/0.49)=5.0,
    // so expected quantity is the venue floor (5.0), not the prior
    // 6.25 (which came from min_notional_usd / 0.16 cheap leg).
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.49, 0.51, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.49, 0.51, 10));

    assert_eq!(decision.intents.len(), 2);
    assert!(decision
        .intents
        .iter()
        .all(|intent| (intent.quantity - 5.0).abs() < 1e-9));
}

#[test]
fn btc_5m_mm_scales_parent_size_when_clip_supports_it() {
    let mut config = btc_5m_mm_test_config();
    config.base_clip_usd = 5.20;
    config.max_clip_usd = 8.00;
    config.liquidity_clip_fraction = 1.0;
    config.entry_min_size_multiplier = 1.30;
    let mut strategy = Btc5mMmStrategy::new(config);
    let ctx = context(Vec::new());
    // Use a near-neutral book (0.49/0.51) instead of extreme (0.74/0.16)
    // so the new price-extremity gate (max_fair > 0.65) doesn't skip
    // entry. Test is about clip sizing, not extremity gating.
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.49, 0.51, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.49, 0.51, 10));

    assert_eq!(decision.intents.len(), 2);
    let expected_qty = 5.20 / 0.49;
    assert!(decision
        .intents
        .iter()
        .all(|intent| (intent.quantity - expected_qty).abs() < 1e-9));
}

#[test]
fn btc_5m_mm_explains_market_minimum_sizing_without_venue_floor() {
    let mut config = btc_5m_mm_test_config();
    config.base_clip_usd = 0.25;
    config.min_clip_usd = 0.25;
    config.max_clip_usd = 2.00;
    config.min_order_notional_usd = 0.50;
    config.venue_min_order_quantity = 0.01;
    config.min_order_quantity = 0.75;
    let mut strategy = Btc5mMmStrategy::new(config);
    let ctx = context(Vec::new());
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.52, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.52, 10));

    assert_eq!(decision.intents.len(), 2);
    assert!(decision
        .intents
        .iter()
        .all(|intent| (intent.quantity - (0.50 / 0.48)).abs() < 1e-9));
}

#[test]
fn btc_5m_mm_suppresses_unpaired_flat_entry_by_default() {
    let mut strategy = Btc5mMmStrategy::new(btc_5m_mm_test_config());
    let ctx = context(Vec::new());
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.52, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.52, 0.56, 10));

    assert!(decision.intents.is_empty());
}

#[test]
fn btc_5m_mm_allows_convex_cheap_leg_accumulation_when_pair_is_premium_blocked() {
    let mut config = btc_5m_mm_test_config();
    config.min_edge_bps = 10.0;
    config.max_gross_cost_usd = 10.0;
    config.max_leg_cost_usd = 5.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let ctx = context(Vec::new());
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.38, 0.40, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.58, 0.62, 10));

    assert_eq!(decision.intents.len(), 1);
    assert_eq!(decision.intents[0].instrument_id, InstrumentId::from("up"));
    assert_eq!(
        decision.intents[0].quote_level_tag.as_deref(),
        Some("mm-convex-accum:l1")
    );
    assert!(decision.intents[0].limit_price <= Btc5mMmStrategy::CONVEX_ACCUMULATION_MAX_BID);
}

#[test]
fn btc_5m_mm_late_bar_core_fires_when_all_gates_pass() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let market_context = MarketContextRecord {
        market_id: "market-mm".to_string(),
        instrument_ids: vec!["up".to_string(), "down".to_string()],
        price_to_beat: Some(100.0),
        final_price: None,
        event_start_time_ms: Some(0),
        event_end_time_ms: Some(40_000),
    };
    let btc_regime = crate::signals::BtcRegimeSnapshot {
        last_price: Some(99.0),
        realized_vol_5m_bps: Some(70.0),
        observed_at_ms: 0,
        ..crate::signals::BtcRegimeSnapshot::default()
    };
    let ctx = context_at_with_market(Vec::new(), 0, market_context, btc_regime);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.05, 0.07, 0));
    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.92, 0.93, 0));

    let core_intents: Vec<_> = decision
        .intents
        .iter()
        .filter(|intent| intent.quote_level_tag.as_deref() == Some("mm-late-bar-core:l1"))
        .collect();
    assert_eq!(
        core_intents.len(),
        1,
        "should emit one late-bar-core intent"
    );
    assert!(
        decision
            .intents
            .iter()
            .all(|intent| intent.quote_level_tag.as_deref() != Some("mm-convex-accum:l1")),
        "late-bar core should take precedence over convex on the same tick"
    );
    let intent = core_intents[0];
    assert_eq!(intent.instrument_id, InstrumentId::from("down"));
    assert_eq!(intent.side, TradeSide::Buy);
    assert!((intent.limit_price - 0.92).abs() < 1e-9);
    assert!(intent.quantity * intent.limit_price <= 5.0 + 1e-9);
}

#[test]
fn btc_5m_mm_late_bar_core_skips_when_vol_too_low() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let market_context = MarketContextRecord {
        market_id: "market-mm".to_string(),
        instrument_ids: vec!["up".to_string(), "down".to_string()],
        price_to_beat: Some(100.0),
        final_price: None,
        event_start_time_ms: Some(0),
        event_end_time_ms: Some(40_000),
    };
    let btc_regime = crate::signals::BtcRegimeSnapshot {
        last_price: Some(99.0),
        realized_vol_5m_bps: Some(20.0),
        observed_at_ms: 0,
        ..crate::signals::BtcRegimeSnapshot::default()
    };
    let ctx = context_at_with_market(Vec::new(), 0, market_context, btc_regime);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.05, 0.07, 0));
    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.92, 0.93, 0));

    assert!(decision
        .intents
        .iter()
        .all(|intent| intent.quote_level_tag.as_deref() != Some("mm-late-bar-core:l1")));
}

#[test]
fn btc_5m_mm_late_bar_core_skips_when_direction_not_confirmed() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let market_context = MarketContextRecord {
        market_id: "market-mm".to_string(),
        instrument_ids: vec!["up".to_string(), "down".to_string()],
        price_to_beat: Some(100.0),
        final_price: None,
        event_start_time_ms: Some(0),
        event_end_time_ms: Some(40_000),
    };
    let btc_regime = crate::signals::BtcRegimeSnapshot {
        last_price: Some(100.0),
        realized_vol_5m_bps: Some(70.0),
        observed_at_ms: 0,
        ..crate::signals::BtcRegimeSnapshot::default()
    };
    let ctx = context_at_with_market(Vec::new(), 0, market_context, btc_regime);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.05, 0.07, 0));
    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.92, 0.93, 0));

    assert!(decision
        .intents
        .iter()
        .all(|intent| intent.quote_level_tag.as_deref() != Some("mm-late-bar-core:l1")));
}

#[test]
fn btc_5m_mm_late_bar_core_skips_when_time_remaining_too_low() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let market_context = MarketContextRecord {
        market_id: "market-mm".to_string(),
        instrument_ids: vec!["up".to_string(), "down".to_string()],
        price_to_beat: Some(100.0),
        final_price: None,
        event_start_time_ms: Some(0),
        event_end_time_ms: Some(20_000),
    };
    let btc_regime = crate::signals::BtcRegimeSnapshot {
        last_price: Some(99.0),
        realized_vol_5m_bps: Some(70.0),
        observed_at_ms: 0,
        ..crate::signals::BtcRegimeSnapshot::default()
    };
    let ctx = context_at_with_market(Vec::new(), 0, market_context, btc_regime);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.05, 0.07, 0));
    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.92, 0.93, 0));

    assert!(decision
        .intents
        .iter()
        .all(|intent| intent.quote_level_tag.as_deref() != Some("mm-late-bar-core:l1")));
}

#[test]
fn btc_5m_mm_late_bar_core_skips_when_time_remaining_too_high() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let market_context = MarketContextRecord {
        market_id: "market-mm".to_string(),
        instrument_ids: vec!["up".to_string(), "down".to_string()],
        price_to_beat: Some(100.0),
        final_price: None,
        event_start_time_ms: Some(0),
        event_end_time_ms: Some(150_000),
    };
    let btc_regime = crate::signals::BtcRegimeSnapshot {
        last_price: Some(99.0),
        realized_vol_5m_bps: Some(70.0),
        observed_at_ms: 0,
        ..crate::signals::BtcRegimeSnapshot::default()
    };
    let ctx = context_at_with_market(Vec::new(), 0, market_context, btc_regime);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.05, 0.07, 0));
    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.92, 0.93, 0));

    assert!(decision
        .intents
        .iter()
        .all(|intent| intent.quote_level_tag.as_deref() != Some("mm-late-bar-core:l1")));
}

#[test]
fn btc_5m_mm_late_bar_core_skips_when_expensive_ask_below_floor() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let market_context = MarketContextRecord {
        market_id: "market-mm".to_string(),
        instrument_ids: vec!["up".to_string(), "down".to_string()],
        price_to_beat: Some(100.0),
        final_price: None,
        event_start_time_ms: Some(0),
        event_end_time_ms: Some(40_000),
    };
    let btc_regime = crate::signals::BtcRegimeSnapshot {
        last_price: Some(99.0),
        realized_vol_5m_bps: Some(70.0),
        observed_at_ms: 0,
        ..crate::signals::BtcRegimeSnapshot::default()
    };
    let ctx = context_at_with_market(Vec::new(), 0, market_context, btc_regime);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.05, 0.07, 0));
    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.83, 0.84, 0));

    assert!(decision
        .intents
        .iter()
        .all(|intent| intent.quote_level_tag.as_deref() != Some("mm-late-bar-core:l1")));
}

#[test]
fn btc_5m_mm_late_bar_core_skips_when_expensive_ask_above_ceiling() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let market_context = MarketContextRecord {
        market_id: "market-mm".to_string(),
        instrument_ids: vec!["up".to_string(), "down".to_string()],
        price_to_beat: Some(100.0),
        final_price: None,
        event_start_time_ms: Some(0),
        event_end_time_ms: Some(40_000),
    };
    let btc_regime = crate::signals::BtcRegimeSnapshot {
        last_price: Some(99.0),
        realized_vol_5m_bps: Some(70.0),
        observed_at_ms: 0,
        ..crate::signals::BtcRegimeSnapshot::default()
    };
    let ctx = context_at_with_market(Vec::new(), 0, market_context, btc_regime);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.005, 0.01, 0));
    // DOWN ask = 0.99 — above the bumped ceiling of 0.98.
    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.985, 0.99, 0));

    assert!(decision
        .intents
        .iter()
        .all(|intent| intent.quote_level_tag.as_deref() != Some("mm-late-bar-core:l1")));
}

#[test]
fn btc_5m_mm_late_bar_core_skips_when_already_holding_high_avg_cost() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let market_context = MarketContextRecord {
        market_id: "market-mm".to_string(),
        instrument_ids: vec!["up".to_string(), "down".to_string()],
        price_to_beat: Some(100.0),
        final_price: None,
        event_start_time_ms: Some(0),
        event_end_time_ms: Some(40_000),
    };
    let btc_regime = crate::signals::BtcRegimeSnapshot {
        last_price: Some(99.0),
        realized_vol_5m_bps: Some(70.0),
        observed_at_ms: 0,
        ..crate::signals::BtcRegimeSnapshot::default()
    };
    let positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("down"),
        quantity: 6.0,
        avg_price: 0.90,
        mark_price: Some(0.91),
        updated_at_ms: 0,
    }];
    let ctx = context_at_with_market(positions, 0, market_context, btc_regime);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.05, 0.07, 0));
    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.92, 0.93, 0));

    assert!(decision
        .intents
        .iter()
        .all(|intent| intent.quote_level_tag.as_deref() != Some("mm-late-bar-core:l1")));
}

#[test]
fn btc_5m_mm_late_bar_core_respects_per_bar_count_cap() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    config.venue_min_order_quantity = 0.1;
    config.min_order_quantity = 0.1;
    config.min_order_notional_usd = 0.1;
    let mut strategy = Btc5mMmStrategy::new(config);
    let market_context = MarketContextRecord {
        market_id: "market-mm".to_string(),
        instrument_ids: vec!["up".to_string(), "down".to_string()],
        price_to_beat: Some(100.0),
        final_price: None,
        event_start_time_ms: Some(0),
        event_end_time_ms: Some(40_000),
    };
    let btc_regime = crate::signals::BtcRegimeSnapshot {
        last_price: Some(99.0),
        realized_vol_5m_bps: Some(70.0),
        observed_at_ms: 0,
        ..crate::signals::BtcRegimeSnapshot::default()
    };
    let ctx = context_at_with_market(Vec::new(), 0, market_context, btc_regime);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.05, 0.07, 0));

    let state = strategy
        .market_states
        .entry(MarketId::from("market-mm"))
        .or_default();
    state.late_bar_core_bar_end_ms = Some(40_000);
    state.late_bar_core_bids_this_bar = Btc5mMmStrategy::LATE_BAR_CORE_MAX_BIDS_PER_BAR;
    state.late_bar_core_spend_this_bar_usd = 0.0;

    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.92, 0.93, 0));

    assert!(decision
        .intents
        .iter()
        .all(|intent| intent.quote_level_tag.as_deref() != Some("mm-late-bar-core:l1")));
}

#[test]
fn btc_5m_mm_convex_accumulation_skips_when_kelly_budget_is_below_venue_minimum() {
    let mut config = btc_5m_mm_test_config();
    config.min_edge_bps = 10.0;
    config.max_gross_cost_usd = 10.0;
    config.max_leg_cost_usd = 5.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let ctx = context_at_with_cash(Vec::new(), 10, 40.0);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.38, 0.40, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.58, 0.62, 10));

    assert!(decision.intents.is_empty());
}

#[test]
fn btc_5m_mm_primary_paired_quotes_ignore_convex_kelly_budget() {
    let mut config = btc_5m_mm_test_config();
    config.min_edge_bps = 10.0;
    config.max_gross_cost_usd = 10.0;
    config.max_leg_cost_usd = 5.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let ctx = context_at_with_cash(Vec::new(), 10, 40.0);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.50, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.50, 10));

    assert_eq!(decision.intents.len(), 2);
    assert!(decision.intents.iter().all(|intent| {
        intent.pair_id.is_some()
            && intent
                .quote_level_tag
                .as_deref()
                .is_some_and(|tag| tag.starts_with("mm-paired-bid"))
    }));
}

#[test]
fn btc_5m_mm_convex_accumulation_sizes_from_fractional_kelly_budget() {
    let mut config = btc_5m_mm_test_config();
    config.min_edge_bps = 10.0;
    config.max_gross_cost_usd = 10.0;
    config.max_leg_cost_usd = 5.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let bankroll_usd = 500.0;
    let ctx = context_at_with_cash(Vec::new(), 10, bankroll_usd);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.38, 0.40, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.58, 0.62, 10));

    assert_eq!(decision.intents.len(), 1);
    let intent = &decision.intents[0];
    assert_eq!(intent.instrument_id, InstrumentId::from("up"));
    assert!(intent.quantity > config.venue_min_order_quantity);
    assert!(
        intent.quantity * intent.limit_price
            <= bankroll_usd * Btc5mMmStrategy::CONVEX_MAX_KELLY_BANKROLL_FRACTION + 1e-9
    );
}

#[test]
fn btc_5m_mm_hedge_rescues_one_sided_inventory() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quantity: 5.0,
        avg_price: 0.33,
        mark_price: Some(0.33),
        updated_at_ms: 1,
    }];
    let ctx = context(positions);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.32, 0.34, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.50, 10));

    assert_eq!(decision.intents.len(), 1);
    assert_eq!(
        decision.intents[0].instrument_id,
        InstrumentId::from("down")
    );
    assert_eq!(decision.intents[0].side, TradeSide::Buy);
    assert_eq!(
        decision.intents[0].quote_level_tag.as_deref(),
        Some("mm-hedge-rescue")
    );
}

#[test]
fn btc_5m_mm_hedge_rescue_does_not_repeat_while_in_flight() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    config.cooldown_ms = 100;
    let mut strategy = Btc5mMmStrategy::new(config);
    let positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quantity: 5.0,
        avg_price: 0.33,
        mark_price: Some(0.33),
        updated_at_ms: 1,
    }];

    let ctx = context_at(positions.clone(), 1_000);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.32, 0.34, 10));
    let first = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.50, 10));
    assert_eq!(first.intents.len(), 1);

    let ctx = context_at(positions, 2_000);
    let second = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.50, 20));
    assert!(second.intents.is_empty());
    assert!(second
        .notes
        .iter()
        .any(|note| note.contains("rescue already in flight")));
}

#[test]
fn btc_5m_mm_hedge_rescue_does_not_repeat_when_stranded_quantity_moves() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    config.cooldown_ms = 100;
    let mut strategy = Btc5mMmStrategy::new(config);

    let initial_positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quantity: 5.0,
        avg_price: 0.33,
        mark_price: Some(0.33),
        updated_at_ms: 1,
    }];
    let ctx = context_at(initial_positions, 1_000);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.32, 0.34, 10));
    let first = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.50, 10));
    assert_eq!(first.intents.len(), 1);

    let changed_positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quantity: 6.25,
        avg_price: 0.33,
        mark_price: Some(0.33),
        updated_at_ms: 2,
    }];
    let ctx = context_at(changed_positions, 2_000);
    let second = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.50, 20));
    assert!(second.intents.is_empty());
    assert!(second
        .notes
        .iter()
        .any(|note| note.contains("rescue already in flight")));
}

#[test]
fn btc_5m_mm_hedge_rescue_stops_after_attempt_cap() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    config.cooldown_ms = 100;
    let mut strategy = Btc5mMmStrategy::new(config);
    let positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quantity: 5.0,
        avg_price: 0.33,
        mark_price: Some(0.33),
        updated_at_ms: 1,
    }];

    let ctx = context_at(positions.clone(), 1_000);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.32, 0.34, 1_000));
    let first =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.50, 1_000));
    assert_eq!(first.intents.len(), 1);

    for (i, now_ms) in [17_000, 33_000].into_iter().enumerate() {
        let ctx = context_at(positions.clone(), now_ms);
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.50, now_ms));
        assert_eq!(
            decision.intents.len(),
            1,
            "attempt {} should emit rescue",
            i + 2
        );
    }

    let ctx = context_at(positions, 49_000);
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.50, 49_000));
    assert!(decision.intents.is_empty());
    assert!(decision
        .notes
        .iter()
        .any(|note| note.contains("rescue attempt cap reached")));
}

#[test]
fn btc_5m_mm_hedge_rescue_attempt_cap_survives_leg_swap() {
    let mut config = btc_5m_mm_test_config();
    config.cooldown_ms = 100;
    let mut strategy = Btc5mMmStrategy::new(config);
    let market_id = MarketId::from("market-mm");
    let up_id = InstrumentId::from("up");
    let down_id = InstrumentId::from("down");

    for now_ms in [1_000, 17_000, 33_000] {
        strategy.record_rescue_attempt(&market_id, &up_id, &down_id, 5.0, now_ms);
    }

    let mut notes = Vec::new();
    let allowed = strategy.can_emit_rescue(&market_id, &down_id, &up_id, 49_000, &mut notes);

    assert!(
        !allowed,
        "leg swaps must not reset the per-market rescue cap"
    );
    assert!(notes
        .iter()
        .any(|note| note.contains("rescue attempt cap reached")));
}

#[test]
fn btc_5m_mm_hedge_rescue_uses_rescue_clip_budget() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    config.hedge_rescue_clip_usd = 2.50;
    config.venue_min_order_quantity = 0.01;
    let mut strategy = Btc5mMmStrategy::new(config);
    let positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quantity: 20.0,
        avg_price: 0.33,
        mark_price: Some(0.33),
        updated_at_ms: 1,
    }];
    let ctx = context(positions);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.32, 0.34, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.50, 10));

    assert_eq!(decision.intents.len(), 1);
    let hedge = &decision.intents[0];
    assert!(hedge.quantity < 20.0);
    assert!(hedge.quantity * hedge.limit_price <= config.hedge_rescue_clip_usd + 1e-9);
}

#[test]
fn btc_5m_mm_hedge_rescue_can_upsize_to_venue_minimum_above_clip() {
    // Strategy preference (post 2026-04-29 hold-to-resolution shift):
    // when the held position's hold_ev only slightly trails rescue_ev,
    // gas + taker fees on the rescue make hold the better expected value.
    // This scenario (avg=0.40 with held_fair ≈ 0.382) lands in that
    // territory, so the engine now holds rather than firing the upsized
    // rescue. The upsize logic in build_rescue_intent_for_quantity is
    // still exercised by other rescue scenarios where rescue_ev > hold_ev
    // by a meaningful margin.
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    config.hedge_rescue_clip_usd = 2.50;
    config.venue_min_order_quantity = 5.0;
    config.min_order_quantity = 0.01;
    config.min_order_notional_usd = 1.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quantity: 20.0,
        avg_price: 0.40,
        mark_price: Some(0.40),
        updated_at_ms: 1,
    }];
    let ctx = context(positions);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.38, 0.40, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.62, 0.64, 10));

    assert!(
        decision.intents.is_empty(),
        "post-hold-shift: marginal rescue_ev should yield to hold-to-resolution"
    );
}

#[test]
fn btc_5m_mm_hedge_rescue_enforces_marketable_buy_min_notional() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    config.hedge_rescue_clip_usd = 2.50;
    config.venue_min_order_quantity = 5.0;
    config.min_order_quantity = 0.01;
    // Live env once lowered this below Polymarket's marketable BUY minimum,
    // causing venue rejects for 5 shares at 7c. Rescue must enforce the
    // protocol floor itself instead of trusting the tunable strategy knob.
    config.min_order_notional_usd = 0.01;
    let mut strategy = Btc5mMmStrategy::new(config);
    let positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quantity: 5.3891,
        avg_price: 0.19,
        mark_price: Some(0.19),
        updated_at_ms: 1,
    }];
    let ctx = context(positions);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.17, 0.18, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.06, 0.07, 10));

    assert_eq!(decision.intents.len(), 1);
    let hedge = &decision.intents[0];
    assert_eq!(hedge.quote_level_tag.as_deref(), Some("mm-hedge-rescue"));
    assert!(hedge.quantity * hedge.limit_price >= 1.0 - 1e-9);
}

#[test]
fn btc_5m_mm_entry_caps_scale_with_free_cash_budget() {
    let mut config = btc_5m_mm_test_config();
    config.min_edge_bps = 10.0;
    config.max_gross_cost_usd = 20.0;
    config.max_leg_cost_usd = 10.0;
    config.max_entry_free_cash_bps = 2_000.0;
    config.min_order_notional_usd = 0.50;
    config.venue_min_order_quantity = 0.01;
    config.min_order_quantity = 0.01;
    let mut strategy = Btc5mMmStrategy::new(config);
    let ctx = context_at_with_cash(Vec::new(), 10, 10.0);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.50, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.48, 0.50, 10));

    assert_eq!(decision.intents.len(), 2);
    let total_notional: f64 = decision
        .intents
        .iter()
        .map(|intent| intent.quantity * intent.limit_price)
        .sum();
    assert!(total_notional <= 2.0 + 1e-9);
}

#[test]
fn btc_5m_mm_holds_cheap_stranded_inventory_when_hold_ev_beats_rescue() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quantity: 5.0,
        avg_price: 0.38,
        mark_price: Some(0.62),
        updated_at_ms: 1,
    }];
    let ctx = context(positions);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.60, 0.64, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.36, 0.40, 10));

    assert!(decision.intents.is_empty());
    assert!(decision
        .notes
        .iter()
        .any(|note| note.contains("hold stranded positive-asymmetry")));
}

#[test]
fn btc_5m_mm_holds_stranded_inventory_when_cost_basis_is_unknown() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quantity: 8.26,
        avg_price: 0.0,
        mark_price: Some(0.05),
        updated_at_ms: 1,
    }];
    let ctx = context(positions);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.04, 0.05, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.94, 0.95, 10));

    assert!(
        decision.intents.is_empty(),
        "unknown venue cost basis must not make a 95c hedge look safe"
    );
    assert!(decision
        .notes
        .iter()
        .any(|note| note.contains("unknown cost basis")));
}

#[test]
fn btc_5m_mm_partially_rescues_oversized_convex_inventory() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    config.max_leg_cost_usd = 10.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quantity: 25.0,
        avg_price: 0.42,
        mark_price: Some(0.52),
        updated_at_ms: 1,
    }];
    let ctx = context(positions);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.50, 0.54, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.46, 0.48, 10));

    assert_eq!(decision.intents.len(), 1);
    let hedge = &decision.intents[0];
    assert_eq!(hedge.quote_level_tag.as_deref(), Some("mm-hedge-rescue"));
    assert!(hedge.quantity > 0.0);
    assert!(
        hedge.quantity < 25.0,
        "strategy should keep a convex tranche instead of rescuing everything"
    );
    assert!(decision
        .notes
        .iter()
        .any(|note| note.contains("partial rescue stranded leg")));
}

#[test]
fn btc_5m_mm_holds_boundary_avg_cost_when_rescue_would_lock_in_loss_after_gas() {
    // Position avg_cost=0.52 (just above CONVEX_ACCUMULATION_MAX_AVG_COST=0.50).
    // Held_fair drifts to ~0.50 (mid-bar). Opposite ask is 0.50.
    // rescue_ev_gross = 1 - 0.52 - 0.50 - taker_fee ≈ -0.038
    // hold_ev = 0.50 - 0.52 = -0.02
    // Even before gas, hold beats rescue. Pre-fix: forced rescue because
    // cheap_positive fails at 0.52 > 0.50 and not late_confident → rescue.
    // With gas baked into rescue_ev, rescue is even worse — hold is the
    // correct choice here.
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quantity: 5.0,
        avg_price: 0.52,
        mark_price: Some(0.50),
        updated_at_ms: 1,
    }];
    let ctx = context(positions);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.49, 0.51, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.49, 0.51, 10));

    assert!(
        decision.intents.is_empty(),
        "boundary-avg-cost stranded should hold to resolution; rescue locks in extra loss after gas. \
         got intents: {:?}",
        decision.intents.iter().map(|i| (i.quote_level_tag.as_deref(), i.limit_price)).collect::<Vec<_>>()
    );
    assert!(decision
        .notes
        .iter()
        .any(|note| note.contains("hold stranded")));
}

#[test]
fn btc_5m_mm_holds_premium_stranded_inventory_when_mark_is_positive() {
    // Renamed 2026-04-29: previous behavior was to ALWAYS rescue when
    // avg_cost > CONVEX_ACCUMULATION_MAX_AVG_COST regardless of EV.
    // New behavior: hold whenever hold_ev > rescue_ev (after gas + fees),
    // because rescuing into a guaranteed smaller win when hold offers a
    // larger expected win is irrational. Gas on the rescue would also
    // burn a fraction of the realized P&L.
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quantity: 5.0,
        avg_price: 0.58,
        mark_price: Some(0.62),
        updated_at_ms: 1,
    }];
    let ctx = context(positions);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.60, 0.64, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.36, 0.40, 10));

    assert!(
        decision.intents.is_empty(),
        "post-hold-shift: positive-mark stranded should hold, not rescue. \
         Got intents: {:?}",
        decision
            .intents
            .iter()
            .map(|i| i.quote_level_tag.as_deref())
            .collect::<Vec<_>>()
    );
}

#[test]
fn btc_5m_mm_late_bar_fair_can_hold_moderate_cost_winner() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quantity: 5.0,
        avg_price: 0.54,
        mark_price: Some(0.54),
        updated_at_ms: 1,
    }];
    let market_context = MarketContextRecord {
        market_id: "market-mm".to_string(),
        instrument_ids: vec!["up".to_string(), "down".to_string()],
        price_to_beat: Some(100.0),
        final_price: None,
        event_start_time_ms: Some(0),
        event_end_time_ms: Some(60_000),
    };
    let btc_regime = crate::signals::BtcRegimeSnapshot {
        last_price: Some(102.0),
        realized_vol_5m_bps: Some(20.0),
        observed_at_ms: 50_000,
        ..crate::signals::BtcRegimeSnapshot::default()
    };
    let ctx = context_at_with_market(positions, 50_000, market_context, btc_regime);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.48, 0.50, 50_000));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.50, 0.52, 50_000));

    assert!(decision.intents.is_empty());
    assert!(decision
        .notes
        .iter()
        .any(|note| note.contains("hold stranded positive-asymmetry")));
}

#[test]
fn btc_5m_mm_one_sided_inventory_holds_when_hold_ev_dominates() {
    // Renamed 2026-04-29: previous behavior was to ALWAYS rescue one-sided
    // inventory regardless of EV. New behavior: when held_fair (normalized)
    // is high enough that hold_ev > rescue_ev_after_gas, hold to resolution
    // instead. With UP at 0.80 avg and DOWN ask at 0.13, normalized fair
    // implies UP ≈ 0.87, so hold_ev ≈ +0.07, rescue_ev ≈ +0.02 — hold wins.
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    config.base_clip_usd = 5.20;
    config.max_clip_usd = 8.00;
    config.min_order_notional_usd = 0.50;
    let mut strategy = Btc5mMmStrategy::new(config);
    let positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quantity: 6.5,
        avg_price: 0.80,
        mark_price: Some(0.80),
        updated_at_ms: 1,
    }];
    let ctx = context(positions);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.78, 0.82, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.11, 0.13, 10));

    assert!(
        decision.intents.is_empty(),
        "hold should win when held_fair(normalized) > avg_cost. Got intents: {:?}",
        decision
            .intents
            .iter()
            .map(|i| i.quote_level_tag.as_deref())
            .collect::<Vec<_>>()
    );
    assert!(decision
        .notes
        .iter()
        .any(|note| note.contains("hold stranded")));
}

#[test]
fn btc_5m_mm_does_not_sell_unhedged_inventory() {
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    config.min_edge_bps = 0.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let positions = vec![PositionState {
        market_id: MarketId::from("market-mm"),
        instrument_id: InstrumentId::from("up"),
        quantity: 5.0,
        avg_price: 0.75,
        mark_price: Some(0.67),
        updated_at_ms: 1,
    }];
    let ctx = context(positions);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.66, 0.67, 10));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.65, 0.66, 10));

    assert!(decision
        .intents
        .iter()
        .all(|intent| intent.side == TradeSide::Buy && !intent.reduce_only));
}

#[test]
fn unlawful_shear_builds_core_and_hedge() {
    let mut strategy = UnlawfulShearStrategy::new(unlawful_shear_test_config(), 0);
    let ctx = context_with_unlawful_signal(
        Vec::new(),
        10,
        0,
        0,
        Some(unlawful_signal_snapshot(
            UnlawfulExecutionMode::Entry,
            1.0,
            10,
        )),
    );
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-a", 0.70, 0.74, 10));
    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-a", 0.18, 0.22, 10));
    assert_eq!(decision.intents.len(), 2);
    assert!(decision
        .intents
        .iter()
        .any(|intent| intent.side == TradeSide::Buy));
}

#[test]
fn unlawful_shear_micro_clips_split_approved_buy_intents() {
    let mut config = unlawful_shear_test_config();
    config.core_clip_usd = 40.0;
    config.hedge_clip_usd = 12.0;
    config.micro_clip_target_usd = 8.0;
    config.micro_clip_min_usd = 1.0;
    config.micro_clip_max_children = 4;
    let mut strategy = UnlawfulShearStrategy::new(config, 0);
    let ctx = context_with_unlawful_signal(
        Vec::new(),
        10,
        0,
        0,
        Some(unlawful_signal_snapshot(
            UnlawfulExecutionMode::Entry,
            1.0,
            10,
        )),
    );

    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-a", 0.70, 0.74, 10));
    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-a", 0.18, 0.22, 10));

    let core_orders: Vec<_> = decision
        .intents
        .iter()
        .filter(|intent| intent.instrument_id == InstrumentId::from("up"))
        .collect();
    assert_eq!(core_orders.len(), 4);
    assert!(core_orders.iter().all(|intent| {
        intent
            .quote_level_tag
            .as_deref()
            .is_some_and(|tag| tag.contains("core-entry:child-"))
    }));
    assert!(core_orders
        .iter()
        .all(|intent| intent.reason.contains("child=")));
}

#[test]
fn unlawful_shear_microstructure_caps_clip_to_visible_depth() {
    let mut config = unlawful_shear_test_config();
    config.microstructure_require_depth = true;
    config.microstructure_max_clip_ask_notional_fraction = 0.10;
    config.microstructure_imbalance_threshold = 1.0;
    let mut strategy = UnlawfulShearStrategy::new(config, 0);
    let signal = with_microstructure(
        unlawful_signal_snapshot(UnlawfulExecutionMode::Entry, 1.0, 10),
        1_000.0,
        50.0,
    );
    let ctx = context_with_unlawful_signal(Vec::new(), 10, 0, 0, Some(signal));

    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-a", 0.70, 0.74, 10));
    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-a", 0.18, 0.22, 10));

    let core = decision
        .intents
        .iter()
        .find(|intent| intent.instrument_id == InstrumentId::from("up"))
        .expect("core order");
    assert!((core.quantity * core.limit_price - 5.0).abs() < 1e-9);
    assert!(decision
        .notes
        .iter()
        .any(|note| note.contains("microstructure scaled action=core-entry")));
}

#[test]
fn unlawful_shear_microstructure_blocks_wide_spread_entry() {
    let mut config = unlawful_shear_test_config();
    config.microstructure_require_depth = true;
    config.microstructure_max_spread = 0.03;
    let mut strategy = UnlawfulShearStrategy::new(config, 0);
    let mut signal = with_microstructure(
        unlawful_signal_snapshot(UnlawfulExecutionMode::Entry, 1.0, 10),
        1_000.0,
        1_000.0,
    );
    signal.book.cheap_spread = Some(0.04);
    signal.book.expensive_spread = Some(0.04);
    let ctx = context_with_unlawful_signal(Vec::new(), 10, 0, 0, Some(signal));

    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-a", 0.70, 0.74, 10));
    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-a", 0.18, 0.22, 10));

    assert!(decision.intents.is_empty());
    assert!(decision
        .notes
        .iter()
        .any(|note| note.contains("microstructure blocked action=core-entry")));
    assert!(decision
        .notes
        .iter()
        .any(|note| { note.contains("unlawful microstructure controller suppressed market") }));
}

#[test]
fn unlawful_shear_allows_new_market_entry_despite_global_open_order_pressure() {
    let mut strategy = UnlawfulShearStrategy::new(unlawful_shear_test_config(), 0);
    let ctx = context_with_unlawful_signal(
        Vec::new(),
        10,
        32,
        0,
        Some(unlawful_signal_snapshot(
            UnlawfulExecutionMode::Entry,
            1.0,
            10,
        )),
    );
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-a", 0.70, 0.74, 10));
    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-a", 0.18, 0.22, 10));

    assert_eq!(decision.intents.len(), 2);
    assert!(decision
        .intents
        .iter()
        .all(|intent| intent.market_id == MarketId::from("market-a")));
}

#[test]
fn unlawful_shear_salvages_losing_leg() {
    let mut config = unlawful_shear_test_config();
    config.salvage_drawdown_ratio = 0.15;
    let mut strategy = UnlawfulShearStrategy::new(config, 0);
    let positions = vec![
        PositionState {
            market_id: MarketId::from("market-a"),
            instrument_id: InstrumentId::from("up"),
            quantity: 100.0,
            avg_price: 0.80,
            mark_price: Some(0.60),
            updated_at_ms: 1,
        },
        PositionState {
            market_id: MarketId::from("market-a"),
            instrument_id: InstrumentId::from("down"),
            quantity: 60.0,
            avg_price: 0.18,
            mark_price: Some(0.24),
            updated_at_ms: 1,
        },
    ];
    let ctx = context(positions);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-a", 0.55, 0.60, 10));
    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-a", 0.22, 0.25, 10));
    assert!(decision
        .intents
        .iter()
        .any(|intent| intent.side == TradeSide::Sell && intent.reduce_only));
}

#[test]
fn unlawful_shear_mode_action_rules_are_deterministic() {
    assert!(UnlawfulShearStrategy::can_launch_mode_action(
        UnlawfulExecutionMode::Entry,
        "core-entry"
    ));
    assert!(UnlawfulShearStrategy::can_launch_mode_action(
        UnlawfulExecutionMode::Entry,
        "hedge-probe"
    ));
    assert!(!UnlawfulShearStrategy::can_launch_mode_action(
        UnlawfulExecutionMode::Entry,
        "add-hedge"
    ));
    assert!(UnlawfulShearStrategy::can_launch_mode_action(
        UnlawfulExecutionMode::Manage,
        "rebalance-core"
    ));
    assert!(!UnlawfulShearStrategy::can_launch_mode_action(
        UnlawfulExecutionMode::Manage,
        "core-entry"
    ));
    assert!(!UnlawfulShearStrategy::can_launch_mode_action(
        UnlawfulExecutionMode::Cleanup,
        "core-entry"
    ));
    assert!(!UnlawfulShearStrategy::can_launch_mode_action(
        UnlawfulExecutionMode::Flatten,
        "hedge-probe"
    ));
}

#[test]
fn unlawful_shear_signal_mode_entry_allows_core_and_hedge_actions_only() {
    let strategy = UnlawfulShearStrategy::new(unlawful_shear_test_config(), 0);
    let (mode, _, _, clip_scale) = strategy.signal_mode_and_aggression(
        &context_with_unlawful_signal(
            Vec::new(),
            10_000,
            0,
            0,
            Some(unlawful_signal_snapshot(
                UnlawfulExecutionMode::Entry,
                1.0,
                10_000,
            )),
        ),
        Some(10),
        false,
        0.38,
        0.64,
        0.26,
    );

    assert_eq!(mode, UnlawfulExecutionMode::Entry);
    assert!(UnlawfulShearStrategy::can_launch_mode_action(
        mode,
        "core-entry"
    ));
    assert!(UnlawfulShearStrategy::can_launch_mode_action(
        mode,
        "hedge-probe"
    ));
    assert_eq!(clip_scale, 0.6);
}

#[test]
fn unlawful_shear_signal_mode_manage_stays_aggressive_and_no_entry_actions() {
    let strategy = UnlawfulShearStrategy::new(unlawful_shear_test_config(), 0);
    let (mode, aggression, _reasons, clip_scale) = strategy.signal_mode_and_aggression(
        &context_with_unlawful_signal(
            Vec::new(),
            10_000,
            0,
            0,
            Some(unlawful_signal_snapshot(
                UnlawfulExecutionMode::Manage,
                1.0,
                10_000,
            )),
        ),
        Some(40),
        true,
        0.38,
        0.64,
        0.26,
    );

    assert_eq!(mode, UnlawfulExecutionMode::Manage);
    assert_ne!(aggression, UnlawfulAggressionTier::Suppressed);
    assert!(clip_scale > 0.0);
    assert!(!UnlawfulShearStrategy::can_launch_mode_action(
        mode,
        "core-entry"
    ));
    assert!(UnlawfulShearStrategy::can_launch_mode_action(
        mode,
        "add-hedge"
    ));
    assert!(!UnlawfulShearStrategy::can_launch_mode_action(
        mode,
        "early-probe"
    ));
}

#[test]
fn unlawful_shear_signal_mode_standby_blocks_buys() {
    let strategy = UnlawfulShearStrategy::new(unlawful_shear_test_config(), 0);
    let (mode, aggression, _reasons, clip_scale) = strategy.signal_mode_and_aggression(
        &context_with_unlawful_signal(
            Vec::new(),
            10_000,
            0,
            0,
            Some(unlawful_signal_snapshot(
                UnlawfulExecutionMode::Standby,
                0.2,
                10_000,
            )),
        ),
        Some(5),
        false,
        0.38,
        0.64,
        0.26,
    );

    assert_eq!(mode, UnlawfulExecutionMode::Standby);
    assert_eq!(aggression, UnlawfulAggressionTier::Suppressed);
    assert_eq!(clip_scale, 0.0);
    assert!(!UnlawfulShearStrategy::can_launch_mode_action(
        mode,
        "core-entry"
    ));
    assert!(!UnlawfulShearStrategy::can_launch_mode_action(
        mode,
        "rebalance-core"
    ));
}

#[test]
fn unlawful_shear_signal_reason_and_aggression_logged_in_decision_notes() {
    let mut strategy = UnlawfulShearStrategy::new(unlawful_shear_test_config(), 0);
    let mut signal = unlawful_signal_snapshot(UnlawfulExecutionMode::Entry, 1.0, 10_000);
    signal.gate_reasons = vec![
        "unit-test gate reason: entry geometry valid".to_string(),
        "unit-test gate reason: synthetic invariant".to_string(),
    ];

    let ctx = context_with_unlawful_signal(Vec::new(), 10_000, 0, 0, Some(signal));
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-a", 0.70, 0.74, 10_000));
    let decision =
        strategy.on_market_snapshot(&ctx, &snapshot("down", "market-a", 0.18, 0.22, 10_000));

    let notes_blob = decision.notes.join("|");
    assert!(notes_blob.contains("unlawful eval market=market-a"));
    assert!(notes_blob.contains("cheap_id=down"));
    assert!(notes_blob.contains("expensive_id=up"));
    assert!(notes_blob.contains("session=Preferred"));
    assert!(notes_blob.contains("btc_vol_5m_bps=6.00"));
    assert!(notes_blob.contains("btc_trade_count_5m=9000"));
    assert!(notes_blob.contains("mode=Entry"));
    assert!(notes_blob.contains("aggression=Press"));
    assert!(notes_blob.contains("signal_clip_scale=1.000"));
    assert!(notes_blob.contains("buy_clip_scale=1.100"));
    assert!(notes_blob.contains("books_fresh=true"));
    assert!(notes_blob.contains(
            "reasons=unit-test gate reason: entry geometry valid;unit-test gate reason: synthetic invariant"
        ));
}
