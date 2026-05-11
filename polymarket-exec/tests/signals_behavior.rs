use polymarket_exec::market_making::pairing::types::{LadderLeg, PairedMarketSnapshot};
use polymarket_exec::signals::fair_value::{
    estimate_fair_value, estimate_fair_value_with_momentum, NoSignalReason,
};
use polymarket_exec::signals::{
    BtcRegime, BtcRegimeSnapshot, CheapLegSignal, CheapLegSignalEngine, FairValueEstimate,
    FairValueModel, MomentumConfig, MomentumEngine, MomentumSignal, OrderBookPressureEngine,
    OrderBookPressureSignal, SignalDirection,
};
use polymarket_exec::types::{BookLevel, InstrumentId, MarketId, QuoteSnapshot};

#[test]
fn fair_value_moves_with_spot_time_and_volatility() {
    let at_strike = estimate_fair_value(100.0, 100.0, 120.0, 5.0);
    assert_eq!(at_strike.model, FairValueModel::BsmBinary);
    assert!((at_strike.p_up - 0.5).abs() < 1e-7);
    assert!((at_strike.p_up + at_strike.p_down - 1.0).abs() < 1e-9);

    let low_vol = estimate_fair_value(100.02, 100.0, 60.0, 2.0);
    let high_vol = estimate_fair_value(100.02, 100.0, 60.0, 50.0);
    assert!(
        low_vol.p_up > high_vol.p_up,
        "higher vol should soften spot edge: low={low_vol:?} high={high_vol:?}"
    );

    let near_end = estimate_fair_value(100.02, 100.0, 30.0, 5.0);
    let far_from_end = estimate_fair_value(100.02, 100.0, 240.0, 5.0);
    assert!(
        near_end.p_up > far_from_end.p_up,
        "less time remaining should increase confidence for same spot edge"
    );
}

#[test]
fn fair_value_momentum_can_shift_probability_without_book_midpoint() {
    let neutral = estimate_fair_value_with_momentum(100.0, 100.0, 0.5, 0.002, 0.0);
    let positive_momentum = estimate_fair_value_with_momentum(100.0, 100.0, 0.5, 0.002, 0.001);
    let negative_momentum = estimate_fair_value_with_momentum(100.0, 100.0, 0.5, 0.002, -0.001);

    assert!((neutral.p_up - 0.5).abs() < 1e-7);
    assert!(positive_momentum.p_up > neutral.p_up);
    assert!(negative_momentum.p_up < neutral.p_up);
}

#[test]
fn fair_value_invalid_inputs_return_specific_no_signal_reason() {
    let spot = estimate_fair_value(f64::NAN, 100.0, 60.0, 5.0);
    assert_eq!(
        spot.model,
        FairValueModel::NoSignal(NoSignalReason::SpotInvalid)
    );

    let strike = estimate_fair_value(100.0, 0.0, 60.0, 5.0);
    assert_eq!(
        strike.model,
        FairValueModel::NoSignal(NoSignalReason::StrikeInvalid)
    );

    let vol = estimate_fair_value(100.0, 100.0, 60.0, 0.0);
    assert_eq!(
        vol.model,
        FairValueModel::NoSignal(NoSignalReason::VolInvalid)
    );
}

#[test]
fn btc_regime_classifies_whipsaw_vs_directional_tape() {
    let whipsaw = BtcRegimeSnapshot {
        realized_vol_5m_bps: Some(15.0),
        return_180s_bps: Some(10.0),
        ..Default::default()
    };
    assert_eq!(whipsaw.regime(), Some(BtcRegime::Whipsaw));
    assert!(!whipsaw.favors_late_bar_core());

    let directional = BtcRegimeSnapshot {
        realized_vol_5m_bps: Some(2.0),
        return_180s_bps: Some(-30.0),
        ..Default::default()
    };
    assert_eq!(directional.regime(), Some(BtcRegime::DirectionalSmooth));
    assert!(directional.favors_late_bar_core());

    let volatile_trend = BtcRegimeSnapshot {
        realized_vol_5m_bps: Some(20.0),
        return_180s_bps: Some(150.0),
        ..Default::default()
    };
    assert_eq!(volatile_trend.regime(), Some(BtcRegime::TrendingVolatile));
}

#[test]
fn btc_momentum_uses_underlying_windows_not_polymarket_prices() {
    let samples = vec![
        (0, 100.0),
        (300_000, 99.0),
        (600_000, 98.0),
        (900_000, 97.0),
    ];
    let engine = MomentumEngine::new(MomentumConfig {
        lookback_windows: 3,
        window_ms: 300_000,
        decay_factor: 0.75,
        directional_deadband: 0.15,
    });

    let signal = engine.compute(900_000, &samples);

    assert_eq!(signal.direction, SignalDirection::Down);
    assert!(signal.score < -0.99);
    assert_eq!(signal.window_returns_bps.len(), 3);
}

#[test]
fn order_book_pressure_summarizes_execution_pressure_separately_from_btc_regime() {
    let snapshot = PairedMarketSnapshot {
        market_id: MarketId::new("m"),
        yes_instrument_id: InstrumentId::new("yes"),
        no_instrument_id: InstrumentId::new("no"),
        yes_quote: QuoteSnapshot {
            best_bid: Some(BookLevel::new(0.50, 100.0)),
            best_ask: Some(BookLevel::new(0.51, 20.0)),
            taker_buy_qty_60s: 40.0,
            taker_sell_qty_60s: 5.0,
            ..Default::default()
        },
        no_quote: QuoteSnapshot {
            best_bid: Some(BookLevel::new(0.49, 10.0)),
            best_ask: Some(BookLevel::new(0.50, 100.0)),
            taker_buy_qty_60s: 2.0,
            taker_sell_qty_60s: 35.0,
            ..Default::default()
        },
    };

    let pressure = OrderBookPressureEngine::default().compute(&snapshot);

    assert_eq!(pressure.direction, SignalDirection::Up);
    assert_eq!(pressure.pressure_leg(), Some(LadderLeg::Yes));
    assert!(pressure.imbalance > 0.0);
}

#[test]
fn cheap_leg_signal_combines_fair_value_pair_cost_and_momentum() {
    let fair = FairValueEstimate {
        p_up: 0.60,
        p_down: 0.40,
        log_moneyness: 0.0,
        sigma_remaining: 0.0,
        time_remaining_s: 120.0,
        model: FairValueModel::BsmBinary,
    };

    let buy = CheapLegSignalEngine::default().decide(
        &fair,
        &MomentumSignal::default(),
        &OrderBookPressureSignal::default(),
        Some(0.96),
        Some(0.54),
        None,
        None,
    );
    assert!(matches!(buy, CheapLegSignal::Buy { .. }));

    let wait = CheapLegSignalEngine::default().decide(
        &fair,
        &MomentumSignal {
            direction: SignalDirection::Down,
            score: -1.0,
            strength: 1.0,
            ..Default::default()
        },
        &OrderBookPressureSignal::default(),
        Some(0.96),
        Some(0.54),
        None,
        None,
    );
    assert!(matches!(wait, CheapLegSignal::Wait { .. }));
}
