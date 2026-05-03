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
    BookLevel, InstrumentId, MarketId, MarketSnapshot, QuoteSnapshot, RuntimeCommand, RuntimeStatus,
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
        open_orders_total: 0,
        open_orders_for_market: 0,
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
    let market_id = MarketId::from("btc-5m-test");
    let yes_id = InstrumentId::from("yes-token");
    let no_id = InstrumentId::from("no-token");
    let mut strategy =
        StrategyMode::try_from_name(strategy_name, Some(&StrategyProfile::default()))
            .expect("strategy mode");
    let _ = strategy.on_market_snapshot(ctx, &snapshot(&market_id, &yes_id, yes_quote));
    strategy.on_market_snapshot(ctx, &snapshot(&market_id, &no_id, no_quote))
}

#[test]
fn pair_cost_arb_buys_fair_value_cheap_leg() {
    let market_id = MarketId::from("btc-5m-test");
    let yes_id = InstrumentId::from("yes-token");
    let no_id = InstrumentId::from("no-token");
    let ctx = context(
        120_000,
        &market_id,
        &yes_id,
        &no_id,
        inventory(vec![]),
        101.0,
    );

    let decision = drive_two_books(
        "pair_cost_arb",
        &ctx,
        quote(0.34, 0.36, 120_000),
        quote(0.61, 0.62, 120_000),
    );

    let intents = decision.intents();
    assert_eq!(intents.len(), 1, "{decision:?}");
    assert_eq!(intents[0].instrument_id, yes_id);
    assert_eq!(
        intents[0].quote_level_tag.as_deref(),
        Some("pair-cost-arb:cheap-leg:yes")
    );
}

#[test]
fn pair_cost_arb_buys_no_when_down_leg_is_fair_value_cheap() {
    let market_id = MarketId::from("btc-5m-test");
    let yes_id = InstrumentId::from("yes-token");
    let no_id = InstrumentId::from("no-token");
    let ctx = context(
        120_000,
        &market_id,
        &yes_id,
        &no_id,
        inventory(vec![]),
        99.0,
    );

    let decision = drive_two_books(
        "pair_cost_arb",
        &ctx,
        quote(0.61, 0.62, 120_000),
        quote(0.34, 0.36, 120_000),
    );

    let intents = decision.intents();
    assert_eq!(intents.len(), 1, "{decision:?}");
    assert_eq!(intents[0].instrument_id, no_id);
    assert_eq!(
        intents[0].quote_level_tag.as_deref(),
        Some("pair-cost-arb:cheap-leg:no")
    );
}

#[test]
fn pair_cost_arb_pauses_new_entry_in_extreme_volatility() {
    let market_id = MarketId::from("btc-5m-test");
    let yes_id = InstrumentId::from("yes-token");
    let no_id = InstrumentId::from("no-token");
    let mut ctx = context(
        120_000,
        &market_id,
        &yes_id,
        &no_id,
        inventory(vec![]),
        101.0,
    );
    ctx.btc_regime.realized_vol_5m_bps = Some(20.0);

    let decision = drive_two_books(
        "pair_cost_arb",
        &ctx,
        quote(0.34, 0.36, 120_000),
        quote(0.61, 0.62, 120_000),
    );

    assert!(decision.intents().is_empty(), "{decision:?}");
    assert!(decision
        .notes()
        .iter()
        .any(|note| note.contains("extreme volatility pause")));
}

#[test]
fn pair_cost_arb_buys_light_side_to_recycle_after_whipsaw_fill() {
    let market_id = MarketId::from("btc-5m-test");
    let yes_id = InstrumentId::from("yes-token");
    let no_id = InstrumentId::from("no-token");
    let ctx = context(
        120_000,
        &market_id,
        &yes_id,
        &no_id,
        inventory(vec![position(&market_id, &yes_id, 20.0, 0.30, 120_000)]),
        100.0,
    );

    let decision = drive_two_books(
        "pair_cost_arb",
        &ctx,
        quote(0.66, 0.68, 120_000),
        quote(0.29, 0.30, 120_000),
    );

    let intents = decision.intents();
    assert_eq!(intents.len(), 1, "{decision:?}");
    assert_eq!(intents[0].instrument_id, no_id);
    assert_eq!(intents[0].kind, polymarket_exec::types::IntentKind::Close);
    assert!(intents[0]
        .quote_level_tag
        .as_deref()
        .is_some_and(|tag| tag.starts_with("mm-capital-recycle:no")));
}

#[test]
fn pair_cost_arb_emits_merge_before_new_entry_when_pairs_are_available() {
    let market_id = MarketId::from("btc-5m-test");
    let yes_id = InstrumentId::from("yes-token");
    let no_id = InstrumentId::from("no-token");
    let ctx = context(
        120_000,
        &market_id,
        &yes_id,
        &no_id,
        inventory(vec![
            position(&market_id, &yes_id, 60.0, 0.45, 120_000),
            position(&market_id, &no_id, 60.0, 0.45, 120_000),
        ]),
        100.0,
    );

    let decision = drive_two_books(
        "pair_cost_arb",
        &ctx,
        quote(0.49, 0.51, 120_000),
        quote(0.49, 0.51, 120_000),
    );

    match decision {
        StrategyDecision::Commands { commands, .. } => {
            assert!(commands
                .iter()
                .any(|cmd| matches!(cmd, RuntimeCommand::Merge(_))));
        }
        other => panic!("expected merge command, got {other:?}"),
    }
}

#[test]
fn pair_cost_arb_late_window_convexity_does_not_rehedge_winner_excess() {
    let market_id = MarketId::from("btc-5m-test");
    let yes_id = InstrumentId::from("yes-token");
    let no_id = InstrumentId::from("no-token");
    let ctx = context(
        250_000,
        &market_id,
        &yes_id,
        &no_id,
        inventory(vec![
            position(&market_id, &yes_id, 20.0, 0.20, 250_000),
            position(&market_id, &no_id, 5.0, 0.20, 250_000),
        ]),
        101.0,
    );

    let decision = drive_two_books(
        "pair_cost_arb",
        &ctx,
        quote(0.88, 0.90, 250_000),
        quote(0.09, 0.10, 250_000),
    );

    assert!(
        decision
            .notes()
            .iter()
            .any(|note| note.contains("convex rule active")),
        "{decision:?}"
    );
    assert!(decision
        .intents()
        .iter()
        .all(|intent| intent.instrument_id != no_id));
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
fn paired_mm_emits_capital_recycle_when_inventory_is_one_sided() {
    let market_id = MarketId::from("btc-5m-test");
    let yes_id = InstrumentId::from("yes-token");
    let no_id = InstrumentId::from("no-token");
    let ctx = context(
        120_000,
        &market_id,
        &yes_id,
        &no_id,
        inventory(vec![position(&market_id, &yes_id, 20.0, 0.20, 120_000)]),
        100.0,
    );

    let decision = drive_two_books(
        "paired_mm",
        &ctx,
        quote(0.70, 0.71, 120_000),
        quote(0.20, 0.21, 120_000),
    );

    assert!(
        matches!(decision, StrategyDecision::Reactive { .. }),
        "{decision:?}"
    );
    let intents = decision.intents();
    assert_eq!(intents.len(), 1, "{decision:?}");
    assert_eq!(intents[0].instrument_id, no_id);
    assert_eq!(
        intents[0].quote_level_tag.as_deref(),
        Some("mm-capital-recycle:no:CapitalRecycle")
    );
    assert!(decision
        .notes()
        .iter()
        .any(|note| note.contains("paired-mm capital recycle emitted")));
}

#[test]
fn hybrid_mode_combines_pair_cost_and_paired_mm_outputs() {
    let market_id = MarketId::from("btc-5m-test");
    let yes_id = InstrumentId::from("yes-token");
    let no_id = InstrumentId::from("no-token");
    let ctx = context(
        120_000,
        &market_id,
        &yes_id,
        &no_id,
        inventory(vec![]),
        101.0,
    );

    let decision = drive_two_books(
        "pair_cost_arb,paired_mm",
        &ctx,
        quote(0.34, 0.36, 120_000),
        quote(0.61, 0.62, 120_000),
    );

    assert!(
        decision
            .intents()
            .iter()
            .any(|intent| intent.quote_level_tag.as_deref() == Some("pair-cost-arb:cheap-leg:yes")),
        "{decision:?}"
    );
    assert!(decision
        .notes()
        .iter()
        .any(|note| note.contains("paired-mm ladder")));
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
            "config/strategies/btc_5m_pair_cost_arb.live.yaml",
            "pair_cost_arb",
        ),
        ("config/strategies/btc_5m_paired_mm.live.yaml", "paired_mm"),
    ] {
        let profile = StrategyProfile::load(std::path::Path::new(path)).expect(path);
        assert_eq!(profile.strategy.as_deref(), Some(expected));
        StrategyMode::try_from_name(expected, Some(&profile)).expect("supported strategy");
    }
}

#[test]
fn active_strategy_yaml_profiles_drive_strategy_configs() {
    let pair_cost_profile = StrategyProfile::load(std::path::Path::new(
        "config/strategies/btc_5m_pair_cost_arb.live.yaml",
    ))
    .expect("pair-cost profile");
    let pair_cost_config = pair_cost_profile.pair_cost_arb_config();
    assert_eq!(pair_cost_config.pair_cost_threshold, 0.99);
    assert_eq!(pair_cost_config.high_vol_pair_cost_threshold, 0.97);
    assert_eq!(pair_cost_config.base_clip_usd, 1.5);
    assert_eq!(pair_cost_config.max_clip_usd, 5.0);
    assert_eq!(pair_cost_config.rescue_enabled, true);
    assert_eq!(pair_cost_config.rescue_late_window_sec, 90);
    assert_eq!(pair_cost_config.rescue_rehedge_pair_cost_threshold, 1.03);
    assert_eq!(pair_cost_config.recycle_min_imbalance_qty, 5.0);
    assert_eq!(pair_cost_config.recycle_min_time_remaining_ms, 90_000);

    let paired_mm_profile = StrategyProfile::load(std::path::Path::new(
        "config/strategies/btc_5m_paired_mm.live.yaml",
    ))
    .expect("paired-mm profile");
    let paired_mm_config = paired_mm_profile.paired_mm_config();
    assert_eq!(paired_mm_config.ladder.max_depth, 3);
    assert_eq!(paired_mm_config.ladder.base_clip_usd, 1.10);
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
    assert_eq!(paired_mm_config.capital_recycle.max_buy_qty, 25.0);
    assert_eq!(paired_mm_config.capital_recycle.max_buy_notional_usd, 2.50);
    assert_eq!(
        paired_mm_config.capital_recycle.min_time_remaining_ms,
        90_000
    );
    assert_eq!(paired_mm_config.capital_recycle.max_light_side_spread, 0.10);
    assert_eq!(paired_mm_config.capital_recycle.race_buffer_ticks, 3.0);
}
