//! Phase 3c scenario tests.
//!
//! Drives `ReplayStrategyAdapter` over each of the six scenario fixtures
//! produced by `scenario_builder.rs` and asserts every invariant
//! declared in the `ScenarioExpectations`. The live-failure replay
//! (`flat_model_book_disagreement`) is additionally exercised under all
//! three FillQuality regimes.
//!
//! Failure messages format as
//! "scenario X expected fills_paired_entry==0 but got 7" so a grep over
//! test output flags exactly which invariant tripped.

mod scenario_builder;

use std::path::Path;

use polymarket_exec::collector::schema::Event;
use polymarket_exec::replay::fill_sim::{FillQuality, FillSimConfig, LatencyPreset, Side};
use polymarket_exec::replay::runner::{run_window, RunnerConfig, WindowStatus, WindowSummary};
use polymarket_exec::replay::strategy_adapter::ReplayStrategyAdapter;
use polymarket_exec::strategy_profile::StrategyProfile;

use scenario_builder::{
    flat_50_50_paired_mm, flat_model_book_disagreement, late_window_ev_rescue,
    missing_price_to_beat_no_trade, pair_completing_buy_and_merge, stale_btc_feed_no_trade,
    ScenarioExpectations, ASSET_DOWN, ASSET_UP,
};

fn paired_mm_profile() -> StrategyProfile {
    StrategyProfile::load(Path::new(
        "tests/fixtures/replay/profiles/paired_mm_test.yaml",
    ))
    .expect("load paired_mm test profile")
}

fn pair_cost_arb_profile() -> StrategyProfile {
    StrategyProfile::load(Path::new(
        "tests/fixtures/replay/profiles/pair_cost_arb_test.yaml",
    ))
    .expect("load pair_cost_arb test profile")
}

fn run(events: &[Event], profile: StrategyProfile, fill_quality: FillQuality) -> WindowSummary {
    let cfg = RunnerConfig {
        window_id: "scenario".into(),
        fill_sim: FillSimConfig {
            latency: LatencyPreset::Nominal,
            fill_quality,
            seed: 0xC0FFEE,
            cancel_credit_fraction: 0.5,
        },
        max_window_failures: 0,
    };
    let mut adapter = ReplayStrategyAdapter::from_profile(profile);
    run_window(&mut adapter, events, &cfg)
}

fn fills_paired_entry_for_asset(s: &WindowSummary, asset_id: &str, side: Side) -> u64 {
    s.fills
        .iter()
        .filter(|f| f.asset_id == asset_id && f.side == side)
        .count() as u64
}

fn fills_maker_count(s: &WindowSummary) -> u64 {
    use polymarket_exec::replay::fill_sim::MakerOrTaker;
    s.fills
        .iter()
        .filter(|f| f.maker_or_taker == MakerOrTaker::Maker)
        .count() as u64
}

fn intents_emitted(s: &WindowSummary) -> u64 {
    s.intents_submitted
}

fn risk_rejections_total(s: &WindowSummary) -> u64 {
    s.risk_rejections.len() as u64
}

fn assert_invariants(s: &WindowSummary, exp: &ScenarioExpectations) {
    assert_eq!(
        s.status,
        WindowStatus::Ok,
        "scenario {} expected window status Ok, got {:?}",
        exp.name,
        s.status
    );
    if let Some(min) = exp.fills_maker_min {
        assert!(
            fills_maker_count(s) >= min,
            "scenario {} expected fills_maker >= {} but got {}",
            exp.name,
            min,
            fills_maker_count(s)
        );
    }
    if let Some(min) = exp.fills_paired_entry_min {
        let total = s.fills.len() as u64;
        assert!(
            total >= min,
            "scenario {} expected fills_paired_entry >= {} but got {}",
            exp.name,
            min,
            total
        );
    }
    if let Some(max) = exp.fills_paired_entry_max {
        let total = s.fills.len() as u64;
        assert!(
            total <= max,
            "scenario {} expected fills_paired_entry <= {} but got {}",
            exp.name,
            max,
            total
        );
    }
    if let Some(min) = exp.fills_hedge_rescue_min {
        // hedge_rescue fills are tagged Taker by the adapter (see
        // strategy_adapter.rs convert_intent — IntentKind::Close maps
        // to aggressive=true, which produces Taker fills).
        let cnt = s
            .fills
            .iter()
            .filter(|f| {
                matches!(
                    f.maker_or_taker,
                    polymarket_exec::replay::fill_sim::MakerOrTaker::Taker
                )
            })
            .count() as u64;
        assert!(
            cnt >= min,
            "scenario {} expected fills_hedge_rescue (taker) >= {} but got {}",
            exp.name,
            min,
            cnt
        );
    }
    if let Some(min) = exp.intents_emitted_total_min {
        assert!(
            intents_emitted(s) >= min,
            "scenario {} expected intents_emitted_total >= {} but got {}",
            exp.name,
            min,
            intents_emitted(s)
        );
    }
    if let Some(max) = exp.intents_emitted_total_max {
        assert!(
            intents_emitted(s) <= max,
            "scenario {} expected intents_emitted_total <= {} but got {}",
            exp.name,
            max,
            intents_emitted(s)
        );
    }
    if let Some(min) = exp.risk_rejections_total_min {
        assert!(
            risk_rejections_total(s) >= min,
            "scenario {} expected risk_rejections_total >= {} but got {}",
            exp.name,
            min,
            risk_rejections_total(s)
        );
    }
    if let Some(max) = exp.risk_rejections_total_max {
        assert!(
            risk_rejections_total(s) <= max,
            "scenario {} expected risk_rejections_total <= {} but got {}",
            exp.name,
            max,
            risk_rejections_total(s)
        );
    }
    if exp.require_paired_entry_for_up_side {
        let buys = fills_paired_entry_for_asset(s, ASSET_UP, Side::Buy);
        assert!(
            buys >= 1,
            "scenario {} expected at least one paired_entry buy on UP but got {}",
            exp.name,
            buys
        );
    }
    if exp.require_paired_entry_for_down_side {
        let buys = fills_paired_entry_for_asset(s, ASSET_DOWN, Side::Buy);
        assert!(
            buys >= 1,
            "scenario {} expected at least one paired_entry buy on DOWN but got {}",
            exp.name,
            buys
        );
    }
    if exp.up_side_paired_entry_must_be_zero {
        let buys = fills_paired_entry_for_asset(s, ASSET_UP, Side::Buy);
        assert_eq!(
            buys, 0,
            "scenario {} (live-failure replay) expected ZERO paired_entry buys on UP but got {}",
            exp.name, buys
        );
    }
}

#[test]
fn scenario_1_flat_50_50_paired_mm() {
    let (events, exp) = flat_50_50_paired_mm();
    let s = run(&events, paired_mm_profile(), FillQuality::Base);
    assert_invariants(&s, &exp);
}

#[test]
fn scenario_2_flat_model_book_disagreement_base() {
    let (events, exp) = flat_model_book_disagreement();
    let s = run(&events, pair_cost_arb_profile(), FillQuality::Base);
    assert_invariants(&s, &exp);
}

#[test]
fn scenario_2_flat_model_book_disagreement_optimistic() {
    let (events, exp) = flat_model_book_disagreement();
    let s = run(&events, pair_cost_arb_profile(), FillQuality::Optimistic);
    assert_invariants(&s, &exp);
}

#[test]
fn scenario_2_flat_model_book_disagreement_conservative() {
    let (events, exp) = flat_model_book_disagreement();
    let s = run(&events, pair_cost_arb_profile(), FillQuality::Conservative);
    assert_invariants(&s, &exp);
}

#[test]
fn scenario_3_pair_completing_buy_and_merge() {
    let (events, exp) = pair_completing_buy_and_merge();
    let s = run(&events, pair_cost_arb_profile(), FillQuality::Base);
    assert_invariants(&s, &exp);
}

#[test]
fn scenario_4_late_window_ev_rescue() {
    let (events, exp) = late_window_ev_rescue();
    let s = run(&events, paired_mm_profile(), FillQuality::Base);
    assert_invariants(&s, &exp);
}

#[test]
fn scenario_5_missing_price_to_beat_no_trade() {
    let (events, exp) = missing_price_to_beat_no_trade();
    let s = run(&events, paired_mm_profile(), FillQuality::Base);
    assert_invariants(&s, &exp);
}

#[test]
fn scenario_6_stale_btc_feed_no_trade() {
    let (events, exp) = stale_btc_feed_no_trade();
    let s = run(&events, paired_mm_profile(), FillQuality::Base);
    assert_invariants(&s, &exp);
}
