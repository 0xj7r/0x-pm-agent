//! Regression anchor: bonereaper intents must flow through the existing
//! per-instrument position cap in core/risk.rs. Bonereaper holds to
//! resolution rather than merging, so without the cap a long run of
//! aggressive cancel-replace fills could accumulate unbounded inventory
//! on a single market. This test pins the existing gate to bonereaper's
//! intent shape.

use polymarket_exec::inventory::InventoryState;
use polymarket_exec::risk::{RiskContext, RiskEngine, RiskLimits, RiskRejectReason};
use polymarket_exec::types::{
    ClientOrderId, FillLiquidity, FillReport, InstrumentId, IntentKind, MarketId, OrderIntent,
    TradeSide,
};

fn bonereaper_intent(client_order_id: &str, qty: f64) -> OrderIntent {
    OrderIntent {
        client_order_id: ClientOrderId::from(client_order_id),
        market_id: MarketId::from("m-btc-5m-1"),
        instrument_id: InstrumentId::from("asset-yes"),
        side: TradeSide::Buy,
        limit_price: 0.49,
        quantity: qty,
        reduce_only: false,
        reason: "bonereaper-mid-track".to_string(),
        quote_level_tag: Some("bonereaper-mid".to_string()),
        created_at_ms: 1_000,
        pair_id: None,
        kind: IntentKind::Entry,
    }
}

fn fill_for(client_order_id: &str, qty: f64) -> FillReport {
    FillReport {
        order_id: None,
        client_order_id: Some(ClientOrderId::from(client_order_id)),
        market_id: MarketId::from("m-btc-5m-1"),
        instrument_id: InstrumentId::from("asset-yes"),
        side: TradeSide::Buy,
        price: 0.49,
        quantity: qty,
        fee_usd: 0.0,
        liquidity: FillLiquidity::Maker,
        close_method: None,
        observed_at_ms: 1_500,
    }
}

#[test]
fn bonereaper_intent_rejected_when_projected_position_exceeds_cap() {
    let limits = RiskLimits {
        max_position_quantity_per_instrument: 100.0,
        max_order_notional_usd: 1_000.0,
        max_gross_notional_usd: 10_000.0,
        max_net_notional_per_market_usd: 10_000.0,
        ..RiskLimits::default()
    };
    let engine = RiskEngine::new(limits);

    // Inventory holds 95 units of asset-yes (from prior buys).
    let mut inventory = InventoryState::new(5_000.0);
    inventory.apply_fill(&fill_for("br-prior", 95.0)).unwrap();
    assert!(
        (inventory.position_qty(&InstrumentId::from("asset-yes")) - 95.0).abs() < 1e-9,
        "fixture: inventory must hold 95 units before the test intent"
    );

    // Intent for 10 more would push to 105 — over the 100 cap.
    let intent = bonereaper_intent("br-2", 10.0);
    let context = RiskContext {
        open_orders_total: 0,
        open_orders_for_market: 0,
        starting_cash_usd: 5_000.0,
        now_ms: 2_000,
    };
    let decision = engine.evaluate(&inventory, &intent, &context);
    assert!(!decision.accepted, "intent must be rejected at the cap");
    assert_eq!(
        decision.reject_reason,
        Some(RiskRejectReason::PositionQuantityTooLarge),
    );
}

#[test]
fn bonereaper_intent_accepted_when_projected_position_at_or_below_cap() {
    let limits = RiskLimits {
        max_position_quantity_per_instrument: 100.0,
        max_order_notional_usd: 1_000.0,
        max_gross_notional_usd: 10_000.0,
        max_net_notional_per_market_usd: 10_000.0,
        ..RiskLimits::default()
    };
    let engine = RiskEngine::new(limits);

    let mut inventory = InventoryState::new(5_000.0);
    inventory.apply_fill(&fill_for("br-prior", 90.0)).unwrap();

    // Intent for 10 more pushes to exactly 100 — within the cap.
    let intent = bonereaper_intent("br-2", 10.0);
    let context = RiskContext {
        open_orders_total: 0,
        open_orders_for_market: 0,
        starting_cash_usd: 5_000.0,
        now_ms: 2_000,
    };
    let decision = engine.evaluate(&inventory, &intent, &context);
    assert!(
        decision.accepted,
        "intent at exactly the cap must be accepted ({:?}: {})",
        decision.reject_reason, decision.message
    );
}
