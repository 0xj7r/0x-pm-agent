//! End-to-end integration scenarios for the trade-tape shadow simulator.
//!
//! Validates the full pipeline (TradeSynthesiser -> ShadowBook -> queue_model)
//! with the bonereaper strategy as the producer of intents. Does NOT integrate
//! through the runtime/runner.rs path (that is Phase 5b). The goal of this file
//! is to prove the simulator's logic produces realistic fills given realistic
//! inputs, before we wire it into the live runner.

use polymarket_exec::paper::queue_model::QueueDecayEstimator;
use polymarket_exec::paper::trade_tape::ShadowBook;
use polymarket_exec::types::{
    BookLevel, ClientOrderId, InstrumentId, IntentKind, MarketId, MarketSnapshot, OrderIntent,
    QuoteSnapshot, RuntimeStatus, TradeSide,
};
use polymarket_exec::wire::trade_ws::TradeSynthesiser;

fn book_with_bid_ask(bid: f64, ask: f64) -> QuoteSnapshot {
    QuoteSnapshot {
        best_bid: Some(BookLevel::new(bid, 100.0)),
        best_ask: Some(BookLevel::new(ask, 100.0)),
        bid_levels: vec![BookLevel::new(bid, 100.0)],
        ask_levels: vec![BookLevel::new(ask, 100.0)],
        depth_observed_at_ms: Some(1_000),
        last_trade_price: Some((bid + ask) / 2.0),
        observed_at_ms: 1_000,
    }
}

fn calibrated_estimator(family: &str) -> QueueDecayEstimator {
    let mut est = QueueDecayEstimator::new(1, 1.0);
    est.update_with_fill(family, 0.0, 1.0, 0.0);
    est
}

#[test]
fn bonereaper_intent_fills_when_matching_synthesised_trade_arrives() {
    let family = "btc-updown-5m";
    let mut shadow_book = ShadowBook::with_estimator(calibrated_estimator(family));
    let mut synthesiser = TradeSynthesiser::new();

    let intent = OrderIntent {
        client_order_id: ClientOrderId::from("br-1"),
        market_id: MarketId::from("m-btc-5m-1"),
        instrument_id: InstrumentId::from("asset-yes"),
        side: TradeSide::Buy,
        limit_price: 0.49,
        quantity: 40.0,
        reduce_only: false,
        reason: "bonereaper-mid-track".to_string(),
        quote_level_tag: Some("bonereaper-mid".to_string()),
        created_at_ms: 1_000,
        pair_id: None,
        kind: IntentKind::Entry,
    };

    let registered = shadow_book.on_submit(intent.clone(), None, 0.0, 1_000, family);
    assert!(registered, "submit must register the intent");
    assert_eq!(shadow_book.open_order_count(), 1);

    let book = book_with_bid_ask(0.49, 0.50);
    synthesiser.observe_price_change("asset-yes", 0.49, 50.0, 4_950);
    let trade = synthesiser
        .synthesise("asset-yes", 0.49, &book, 5_000)
        .expect("synthesised trade event");

    assert_eq!(trade.taker_side, TradeSide::Sell);
    assert!((trade.size - 50.0).abs() < 1e-9);
    assert!(trade.synthesised);

    let fills = shadow_book.on_trade_event(&trade);
    assert_eq!(fills.len(), 1, "expected exactly one fill");
    assert!((fills[0].price - 0.49).abs() < 1e-9);
    assert_eq!(fills[0].side, TradeSide::Buy);
    assert!((fills[0].quantity - 40.0).abs() < 1e-9);
    assert_eq!(
        shadow_book.open_order_count(),
        0,
        "fully filled order is removed"
    );
}

#[test]
fn mid_spread_trade_does_not_produce_a_shadow_fill() {
    let family = "btc-updown-5m";
    let mut shadow_book = ShadowBook::with_estimator(calibrated_estimator(family));
    let synthesiser = TradeSynthesiser::new();

    let intent = OrderIntent {
        client_order_id: ClientOrderId::from("br-1"),
        market_id: MarketId::from("m-btc-5m-1"),
        instrument_id: InstrumentId::from("asset-yes"),
        side: TradeSide::Buy,
        limit_price: 0.49,
        quantity: 40.0,
        reduce_only: false,
        reason: "bonereaper-mid-track".to_string(),
        quote_level_tag: Some("bonereaper-mid".to_string()),
        created_at_ms: 1_000,
        pair_id: None,
        kind: IntentKind::Entry,
    };
    shadow_book.on_submit(intent, None, 0.0, 1_000, family);

    let book = book_with_bid_ask(0.49, 0.51);
    let synthesised = synthesiser.synthesise("asset-yes", 0.50, &book, 5_000);
    assert!(
        synthesised.is_none(),
        "mid-spread trade is ambiguous; synthesiser must refuse to guess"
    );
    assert_eq!(shadow_book.open_order_count(), 1, "intent still standing");
}

#[test]
fn uncalibrated_estimator_emits_no_fills_even_with_matching_trade() {
    let family = "btc-updown-5m";
    // Default min_observations=8, no updates -> stays uncalibrated
    let mut shadow_book = ShadowBook::default();
    let mut synthesiser = TradeSynthesiser::new();

    let intent = OrderIntent {
        client_order_id: ClientOrderId::from("br-1"),
        market_id: MarketId::from("m-btc-5m-1"),
        instrument_id: InstrumentId::from("asset-yes"),
        side: TradeSide::Buy,
        limit_price: 0.49,
        quantity: 40.0,
        reduce_only: false,
        reason: "bonereaper-mid-track".to_string(),
        quote_level_tag: Some("bonereaper-mid".to_string()),
        created_at_ms: 1_000,
        pair_id: None,
        kind: IntentKind::Entry,
    };
    shadow_book.on_submit(intent, None, 0.0, 1_000, family);

    let book = book_with_bid_ask(0.49, 0.50);
    synthesiser.observe_price_change("asset-yes", 0.49, 50.0, 4_950);
    let trade = synthesiser
        .synthesise("asset-yes", 0.49, &book, 5_000)
        .expect("trade event");

    let fills = shadow_book.on_trade_event(&trade);
    assert!(
        fills.is_empty(),
        "uncalibrated estimator must emit zero fills"
    );
    assert_eq!(
        shadow_book.open_order_count(),
        1,
        "intent is preserved for retry once calibrated"
    );
}

// Suppress unused import warning while the tracer is the only test in the file.
fn _types_in_use(_: MarketSnapshot, _: RuntimeStatus) {}
