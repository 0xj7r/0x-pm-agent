use polymarket_exec::signals::fair_value::{
    estimate_fair_value, estimate_fair_value_with_momentum, NoSignalReason,
};
use polymarket_exec::signals::{BtcRegime, BtcRegimeSnapshot, FairValueModel};

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
