use polymarket_exec::inventory::{InventorySnapshot, PositionState};
use polymarket_exec::market_context::MarketContextRecord;
use polymarket_exec::market_making::pairing::{
    choose_rescue, LadderLeg, RescueAction, RescueConfig, RescueInputs,
};
use polymarket_exec::signals::BtcRegimeSnapshot;
use polymarket_exec::strategy::{
    Strategy, StrategyContext, StrategyDecision, StrategyMode, StrategyProfile, VenueMarketRules,
};
use polymarket_exec::types::{
    BookLevel, InstrumentId, MarketId, MarketLedgerState, MarketSnapshot, QuoteSnapshot,
    RuntimeStatus,
};

fn quote(bid: f64, ask: f64, now_ms: u64) -> QuoteSnapshot {
    QuoteSnapshot {
        best_bid: Some(BookLevel::new(bid, 1_000.0)),
        best_ask: Some(BookLevel::new(ask, 1_000.0)),
        bid_levels: vec![BookLevel::new(bid, 1_000.0)],
        ask_levels: vec![BookLevel::new(ask, 1_000.0)],
        depth_observed_at_ms: Some(now_ms),
        last_trade_price: Some((bid + ask) * 0.5),
        taker_buy_qty_60s: 0.0,
        taker_sell_qty_60s: 0.0,
        observed_at_ms: now_ms,
    }
}

fn snapshot(
    market_id: &MarketId,
    instrument_id: &InstrumentId,
    quote: QuoteSnapshot,
) -> MarketSnapshot {
    MarketSnapshot {
        market_id: market_id.clone(),
        instrument_id: instrument_id.clone(),
        quote,
    }
}

fn market_context(
    market_id: &MarketId,
    yes_id: &InstrumentId,
    no_id: &InstrumentId,
) -> MarketContextRecord {
    MarketContextRecord {
        market_id: market_id.as_str().to_string(),
        instrument_ids: vec![yes_id.as_str().to_string(), no_id.as_str().to_string()],
        price_to_beat: Some(100.0),
        event_start_time_ms: Some(0),
        event_end_time_ms: Some(300_000),
        ..MarketContextRecord::default()
    }
}

fn inventory(positions: Vec<PositionState>) -> InventorySnapshot {
    InventorySnapshot {
        free_cash_usd: 100.0,
        reserved_cash_usd: 0.0,
        total_cash_usd: 100.0,
        realized_pnl_usd: 0.0,
        gross_exposure_usd: positions
            .iter()
            .map(PositionState::gross_notional_usd)
            .sum(),
        positions,
    }
}

fn position(
    market_id: &MarketId,
    instrument_id: &InstrumentId,
    quantity: f64,
    avg_price: f64,
    now_ms: u64,
) -> PositionState {
    PositionState {
        market_id: market_id.clone(),
        instrument_id: instrument_id.clone(),
        quantity,
        avg_price,
        mark_price: Some(avg_price),
        updated_at_ms: now_ms,
    }
}

fn context(
    now_ms: u64,
    market_id: &MarketId,
    yes_id: &InstrumentId,
    no_id: &InstrumentId,
    inventory: InventorySnapshot,
    spot: f64,
) -> StrategyContext {
    StrategyContext {
        now_ms,
        runtime_status: RuntimeStatus::Running,
        inventory,
        paired_core_inventory: None,
        directional_inventory: Vec::new(),
        late_fav_inventory: Vec::new(),
        cheap_tail_inventory: Vec::new(),
        open_orders: Vec::new(),
        open_orders_total: 0,
        open_orders_for_market: 0,
        market_ledger_state: MarketLedgerState::Flat,
        market_context: Some(market_context(market_id, yes_id, no_id)),
        btc_regime: BtcRegimeSnapshot {
            last_price: Some(spot),
            realized_vol_5m_bps: Some(5.0),
            realized_vol_15m_bps: Some(30.0),
            trade_count_5m: 200,
            trade_count_15m: 600,
            return_30s_bps: Some(5.0),
            return_60s_bps: Some(10.0),
            return_120s_bps: Some(15.0),
            return_180s_bps: Some(20.0),
            observed_at_ms: now_ms,
        },
        momentum: polymarket_exec::signals::MomentumSignal::default(),
        venue_rules: Some(VenueMarketRules {
            minimum_order_size: 5.0,
            minimum_tick_size: 0.01,
            neg_risk: false,
        }),
    }
}

fn drive_two_books(
    strategy_name: &str,
    ctx: &StrategyContext,
    yes_quote: QuoteSnapshot,
    no_quote: QuoteSnapshot,
) -> StrategyDecision {
    drive_two_books_with_profile(
        strategy_name,
        &StrategyProfile::default(),
        ctx,
        yes_quote,
        no_quote,
    )
}

fn drive_two_books_with_profile(
    strategy_name: &str,
    profile: &StrategyProfile,
    ctx: &StrategyContext,
    yes_quote: QuoteSnapshot,
    no_quote: QuoteSnapshot,
) -> StrategyDecision {
    let market_id = MarketId::from("btc-5m-test");
    let yes_id = InstrumentId::from("yes-token");
    let no_id = InstrumentId::from("no-token");
    let mut strategy =
        StrategyMode::try_from_name(strategy_name, Some(profile)).expect("strategy mode");
    let _ = strategy.on_market_snapshot(ctx, &snapshot(&market_id, &yes_id, yes_quote));
    strategy.on_market_snapshot(ctx, &snapshot(&market_id, &no_id, no_quote))
}

#[test]
fn paired_mm_emits_two_sided_ladder_quotes() {
    let market_id = MarketId::from("btc-5m-test");
    let yes_id = InstrumentId::from("yes-token");
    let no_id = InstrumentId::from("no-token");
    let ctx = context(
        120_000,
        &market_id,
        &yes_id,
        &no_id,
        inventory(vec![]),
        100.0,
    );

    let decision = drive_two_books(
        "paired_mm",
        &ctx,
        quote(0.49, 0.51, 120_000),
        quote(0.49, 0.51, 120_000),
    );

    assert!(matches!(decision, StrategyDecision::QuoteSet { .. }));
    assert!(decision
        .intents()
        .iter()
        .any(|intent| intent.instrument_id == yes_id));
    assert!(decision
        .intents()
        .iter()
        .any(|intent| intent.instrument_id == no_id));
}

#[test]
fn rescue_math_uses_sell_fallback_when_buy_to_merge_is_worse() {
    let decision = choose_rescue(
        RescueInputs {
            leg: LadderLeg::Yes,
            stranded_qty: 20.0,
            avg_cost: 0.65,
            fair_win_prob: 0.20,
            best_exit_bid: Some(0.30),
            opposite_best_ask: Some(0.80),
        },
        RescueConfig {
            allow_sell_fallback: true,
            min_edge_bps: 25.0,
            max_rescue_qty: 10.0,
            require_no_guaranteed_loss: false,
        },
    );

    assert_eq!(decision.action, RescueAction::SellStrandedLeg);
    assert!(decision.qty > 0.0);
}

#[test]
fn active_strategy_yaml_profiles_load_and_select_supported_modes() {
    for (path, expected) in [
        (
            "config/strategies/archive/btc_5m_paired_mm.live.yaml",
            "paired_mm",
        ),
        (
            "config/strategies/whale_unlawful_strategy.live.yaml",
            "unlawful_mm",
        ),
        (
            "config/strategies/whale_bonereaper_strategy.live.yaml",
            "bonereaper_mm",
        ),
    ] {
        let profile = StrategyProfile::load(std::path::Path::new(path)).expect(path);
        assert_eq!(profile.strategy.as_deref(), Some(expected));
        StrategyMode::try_from_name(expected, Some(&profile)).expect("supported strategy");
    }
}

#[test]
fn active_strategy_yaml_profiles_drive_strategy_configs() {
    let paired_mm_profile = StrategyProfile::load(std::path::Path::new(
        "config/strategies/archive/btc_5m_paired_mm.live.yaml",
    ))
    .expect("paired-mm profile");
    let paired_mm_config = paired_mm_profile.paired_mm_config();
    assert_eq!(paired_mm_config.ladder.max_depth, 12);
    assert_eq!(paired_mm_config.ladder.base_clip_usd, 1.50);
    assert_eq!(paired_mm_config.ladder.max_clip_usd, 5.0);
    assert_eq!(
        paired_mm_config
            .ladder
            .fair_value_anchoring
            .max_model_divergence,
        0.10
    );
    assert_eq!(
        paired_mm_config
            .ladder
            .fair_value_anchoring
            .model_influence_weight,
        0.30
    );
    assert_eq!(paired_mm_config.capital_recycle.pair_cost_target, 0.99);
    assert_eq!(paired_mm_config.capital_recycle.min_imbalance_qty, 5.0);
    assert_eq!(paired_mm_config.capital_recycle.max_buy_qty, 10.0);
    assert_eq!(paired_mm_config.capital_recycle.max_buy_notional_usd, 1.00);
    assert_eq!(
        paired_mm_config.capital_recycle.min_time_remaining_ms,
        90_000
    );
    assert_eq!(paired_mm_config.capital_recycle.max_light_side_spread, 0.10);
    assert_eq!(paired_mm_config.capital_recycle.race_buffer_ticks, 3.0);

    let unlawful_profile = StrategyProfile::load(std::path::Path::new(
        "config/strategies/whale_unlawful_strategy.live.yaml",
    ))
    .expect("unlawful whale profile");
    let unlawful_config = unlawful_profile.core_hedge_mm_config();
    assert_eq!(unlawful_config.core_hedge.merge_min_qty, 50.0);
    assert_eq!(unlawful_config.core_hedge.merge_batch_cap, 500.0);

    let bonereaper_profile = StrategyProfile::load(std::path::Path::new(
        "config/strategies/whale_bonereaper_strategy.live.yaml",
    ))
    .expect("bonereaper whale profile");
    let bonereaper_paired = bonereaper_profile.core_hedge_mm_config();
    let bonereaper_inventory = bonereaper_profile.risk_limits();
    assert_eq!(bonereaper_inventory.max_order_notional_usd, 250.0);
    assert_eq!(bonereaper_inventory.max_gross_notional_usd, 1200.0);
    assert_eq!(bonereaper_inventory.max_net_notional_per_market_usd, 900.0);
    assert_eq!(
        bonereaper_inventory.max_position_quantity_per_instrument,
        1500.0
    );
    assert_eq!(bonereaper_inventory.min_free_cash_usd, 25.0);
    assert_eq!(bonereaper_inventory.min_free_cash_bps, 500.0);
    assert_eq!(bonereaper_inventory.max_session_loss_bps, 2500.0);
    assert_eq!(bonereaper_inventory.max_open_orders_total, 120);
    assert_eq!(bonereaper_inventory.max_open_orders_per_market, 48);
    assert!(bonereaper_paired.core_hedge.enabled);
    // Live invariant: paired-core is only a tiny center probe plus mate-only
    // repair. The old broad 17-level ladder must not return.
    assert!(bonereaper_paired.core_hedge.center_probe_only);
    assert_eq!(bonereaper_paired.core_hedge.ladder_levels, 3);
    assert_eq!(bonereaper_paired.core_hedge.ladder_span, 0.16);
    assert_eq!(bonereaper_paired.core_hedge.clip_shares, 5.0);
    assert_eq!(bonereaper_paired.core_hedge.ladder_min_price, 0.42);
    assert_eq!(bonereaper_paired.core_hedge.ladder_max_price, 0.58);
    assert_eq!(bonereaper_paired.core_hedge.repair_pair_cost_limit, 0.99);
    assert_eq!(bonereaper_paired.core_hedge.repair_fee_buffer, 0.0);
    assert_eq!(
        bonereaper_paired.core_hedge.disable_after_elapsed_ms,
        Some(180_000)
    );
    assert!(
        bonereaper_paired.core_hedge.max_unpaired_core_qty
            <= bonereaper_paired.core_hedge.clip_shares
    );
    assert_eq!(bonereaper_paired.core_hedge.max_unpaired_core_qty, 5.0);
    let bonereaper_late = bonereaper_profile.late_favorite_config();
    assert!(bonereaper_late.favorite_climb.enabled);
    assert!(bonereaper_late.convex_tail.enabled);
    assert_eq!(bonereaper_late.favorite_climb.window_sec, 300);
    assert_eq!(bonereaper_late.favorite_climb.min_favorite_ask, 0.70);
    assert_eq!(bonereaper_late.favorite_climb.clip_usd, 45.0);
    assert_eq!(bonereaper_late.favorite_climb.max_load_usd, 450.0);
    assert_eq!(
        bonereaper_late.favorite_climb.regime_whipsaw_multiplier,
        0.40
    );
    assert_eq!(bonereaper_late.favorite_climb.reversal_multiplier, 0.55);
    assert_eq!(bonereaper_late.convex_tail.clip_usd, 3.0);
    assert_eq!(bonereaper_late.convex_tail.max_load_usd, 30.0);
    assert_eq!(
        bonereaper_late.convex_tail.max_win_edge_spend_fraction,
        0.45
    );
    assert_eq!(
        bonereaper_late.convex_tail.max_late_fav_spend_fraction,
        0.025
    );
    assert_eq!(bonereaper_late.convex_tail.ultra_cheap_max_ask, 0.03);
    assert_eq!(
        bonereaper_late.convex_tail.ultra_cheap_min_favorite_ask,
        0.90
    );
    assert_eq!(
        bonereaper_late
            .convex_tail
            .ultra_cheap_max_late_fav_spend_fraction,
        0.075
    );
}
