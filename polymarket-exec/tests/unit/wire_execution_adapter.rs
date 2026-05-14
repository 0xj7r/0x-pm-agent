use super::*;
use crate::types::FillLiquidity;

fn submit_req(time_in_force: TimeInForce, post_only: bool) -> SubmitOrderRequest {
    SubmitOrderRequest {
        client_order_id: ClientOrderId::from("client-1"),
        market_id: MarketId::from("market-1"),
        instrument_id: InstrumentId::from("asset-1"),
        side: TradeSide::Buy,
        limit_price: 0.5,
        quantity: 1.0,
        post_only,
        time_in_force,
        expires_at_ms: None,
        strategy_tag: "strategy-a".to_string(),
        quote_level_tag: None,
        submitted_at_ms: now_unix_ms(),
    }
}

#[test]
fn parse_market_metadata_extracts_min_size_tick_and_flags() {
    let body = serde_json::json!({
        "condition_id": "0xabc123",
        "question": "Will BTC be up at 5pm UTC?",
        "minimum_order_size": "5",
        "minimum_tick_size": "0.001",
        "neg_risk": false,
        "active": true,
        "closed": false,
        "tokens": []
    })
    .to_string();
    let parsed = parse_market_metadata(&body).expect("parse ok");
    assert_eq!(parsed.condition_id, "0xabc123");
    assert!((parsed.minimum_order_size - 5.0).abs() < 1e-9);
    assert!((parsed.minimum_tick_size - 0.001).abs() < 1e-9);
    assert!(!parsed.neg_risk);
    assert!(parsed.active);
    assert!(!parsed.closed);
}

#[test]
fn parse_market_metadata_tolerates_numeric_min_fields() {
    let body = serde_json::json!({
        "condition_id": "0xdef456",
        "minimum_order_size": 10,
        "minimum_tick_size": 0.01,
        "neg_risk": true,
        "active": false,
        "closed": true
    })
    .to_string();
    let parsed = parse_market_metadata(&body).expect("parse ok");
    assert!((parsed.minimum_order_size - 10.0).abs() < 1e-9);
    assert!((parsed.minimum_tick_size - 0.01).abs() < 1e-9);
    assert!(parsed.neg_risk);
    assert!(!parsed.active);
    assert!(parsed.closed);
}

#[test]
fn parse_market_metadata_rejects_invalid_json() {
    let err = parse_market_metadata("not-json").unwrap_err();
    match err {
        ExecutionError::VenueRejection(msg) => assert!(msg.contains("failed to decode")),
        other => panic!("expected VenueRejection, got {other:?}"),
    }
}

#[test]
fn data_api_position_maps_to_runtime_venue_position() {
    let raw = serde_json::json!({
        "proxyWallet": "0x1234567890abcdef1234567890abcdef12345678",
        "asset": "0x1111111111111111111111111111111111111111111111111111111111111111",
        "conditionId": "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
        "size": 6.5,
        "avgPrice": 0.80,
        "initialValue": 5.20,
        "currentValue": 6.50,
        "cashPnl": 1.30,
        "percentPnl": 25.0,
        "totalBought": 6.5,
        "realizedPnl": 0.0,
        "percentRealizedPnl": 0.0,
        "curPrice": 1.0,
        "redeemable": false,
        "mergeable": false,
        "title": "Bitcoin Up or Down",
        "slug": "btc-updown-5m",
        "icon": "https://example.com/btc.png",
        "eventSlug": "btc-updown",
        "outcome": "Down",
        "outcomeIndex": 1,
        "oppositeOutcome": "Up",
        "oppositeAsset": "0x2222222222222222222222222222222222222222222222222222222222222222",
        "endDate": "2026-04-24",
        "negativeRisk": false
    });
    let position: DataPosition = serde_json::from_value(raw).expect("data position");
    let venue =
        PolymarketExecutionAdapter::venue_position_from_data_position(&position, &HashMap::new());

    assert_eq!(
        venue.instrument_id,
        InstrumentId::from(
            "7719472615821079694904732333912527190217998977709370935963838933860875309329"
        )
    );
    assert_eq!(
        venue.market_id,
        MarketId::from("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890")
    );
    assert_eq!(venue.quantity, 6.5);
    assert_eq!(venue.average_cost_usd, 0.80);
}

#[test]
fn data_api_position_prefers_runtime_market_id_by_asset() {
    let raw = serde_json::json!({
        "proxyWallet": "0x1234567890abcdef1234567890abcdef12345678",
        "asset": "0x1111111111111111111111111111111111111111111111111111111111111111",
        "conditionId": "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
        "size": 2.25,
        "avgPrice": 0.33,
        "initialValue": 0.7425,
        "currentValue": 1.0,
        "cashPnl": 0.2575,
        "percentPnl": 34.68,
        "totalBought": 2.25,
        "realizedPnl": 0.0,
        "percentRealizedPnl": 0.0,
        "curPrice": 0.44,
        "redeemable": false,
        "mergeable": false,
        "title": "Bitcoin Up or Down",
        "slug": "btc-updown-5m",
        "icon": "https://example.com/btc.png",
        "eventSlug": "btc-updown",
        "outcome": "Down",
        "outcomeIndex": 1,
        "oppositeOutcome": "Up",
        "oppositeAsset": "0x2222222222222222222222222222222222222222222222222222222222222222",
        "endDate": "2026-04-24",
        "negativeRisk": false
    });
    let position: DataPosition = serde_json::from_value(raw).expect("data position");
    let mut market_id_by_asset = HashMap::new();
    market_id_by_asset.insert(
        "7719472615821079694904732333912527190217998977709370935963838933860875309329".to_string(),
        "runtime-market-1".to_string(),
    );

    let venue = PolymarketExecutionAdapter::venue_position_from_data_position(
        &position,
        &market_id_by_asset,
    );

    assert_eq!(venue.market_id, MarketId::from("runtime-market-1"));
    assert_eq!(venue.quantity, 2.25);
    assert_eq!(venue.average_cost_usd, 0.33);
}

#[test]
fn data_api_position_derives_avg_price_from_total_bought_when_avg_is_zero() {
    let raw = serde_json::json!({
        "proxyWallet": "0x1234567890abcdef1234567890abcdef12345678",
        "asset": "0x1111111111111111111111111111111111111111111111111111111111111111",
        "conditionId": "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
        "size": 10.0,
        "avgPrice": 0.0,
        "initialValue": 5.0,
        "currentValue": 4.0,
        "cashPnl": -1.0,
        "percentPnl": -20.0,
        "totalBought": 5.0,
        "realizedPnl": 0.0,
        "percentRealizedPnl": 0.0,
        "curPrice": 0.40,
        "redeemable": false,
        "mergeable": true,
        "title": "Bitcoin Up or Down",
        "slug": "btc-updown-5m",
        "icon": "https://example.com/btc.png",
        "eventSlug": "btc-updown",
        "outcome": "Up",
        "outcomeIndex": 0,
        "oppositeOutcome": "Down",
        "oppositeAsset": "0x2222222222222222222222222222222222222222222222222222222222222222",
        "endDate": "2026-04-24",
        "negativeRisk": false
    });
    let position: DataPosition = serde_json::from_value(raw).expect("data position");
    let venue =
        PolymarketExecutionAdapter::venue_position_from_data_position(&position, &HashMap::new());

    assert!(
        (venue.average_cost_usd - 0.5).abs() < 1e-9,
        "venue avg_price=0 with total_bought=5.0/size=10 should derive avg_cost=0.5, got {}",
        venue.average_cost_usd
    );
}

#[test]
fn parse_raw_trades_tolerates_empty_fee_rate_bps() {
    let trade_address = SdkAddress::from_str("0x4444444444444444444444444444444444444444").unwrap();
    let body = serde_json::json!({
        "data": [{
            "id": "trade-1",
            "taker_order_id": "taker-order-1",
            "market": "0x000000000000000000000000000000000000000000000000000000006d61726b",
            "asset_id": "123",
            "side": "BUY",
            "size": "10",
            "fee_rate_bps": "",
            "price": "0.51",
            "status": "MATCHED",
            "match_time": "1710000000",
            "maker_orders": [{
                "order_id": "maker-order-1",
                "maker_address": "0x4444444444444444444444444444444444444444",
                "matched_amount": "5",
                "price": "0.50",
                "fee_rate_bps": "",
                "asset_id": "123",
                "side": "BUY"
            }],
            "trader_side": "TAKER"
        }],
        "next_cursor": "LTE=",
        "limit": 100,
        "count": 1
    })
    .to_string();

    let maker_fills = parse_raw_trades_page(&body, RawTradeFilter::Maker, trade_address).unwrap();
    assert_eq!(maker_fills.len(), 1);
    assert_eq!(
        maker_fills[0].venue_order_id,
        OrderId::from("maker-order-1")
    );
    assert_eq!(maker_fills[0].side, TradeSide::Buy);
    assert_eq!(maker_fills[0].price, 0.50);
    assert_eq!(maker_fills[0].quantity, 5.0);
    assert_eq!(maker_fills[0].liquidity, FillLiquidity::Maker);
    assert_eq!(maker_fills[0].observed_at_ms, 1_710_000_000_000);

    let taker_fills = parse_raw_trades_page(&body, RawTradeFilter::Taker, trade_address).unwrap();
    assert_eq!(taker_fills.len(), 1);
    assert_eq!(
        taker_fills[0].venue_order_id,
        OrderId::from("taker-order-1")
    );
    assert_eq!(taker_fills[0].price, 0.51);
    assert_eq!(taker_fills[0].quantity, 10.0);
    assert_eq!(taker_fills[0].liquidity, FillLiquidity::Taker);
}

#[test]
fn parse_raw_trades_skips_fills_with_empty_essential_decimals() {
    let trade_address = SdkAddress::from_str("0x4444444444444444444444444444444444444444").unwrap();
    let body = serde_json::json!({
        "data": [{
            "id": "trade-1",
            "taker_order_id": "taker-order-1",
            "market": "0x000000000000000000000000000000000000000000000000000000006d61726b",
            "asset_id": "123",
            "side": "BUY",
            "size": "",
            "price": "0.51",
            "match_time": "1710000000",
            "maker_orders": [{
                "order_id": "maker-order-1",
                "maker_address": "0x4444444444444444444444444444444444444444",
                "matched_amount": "5",
                "price": "",
                "asset_id": "123",
                "side": "BUY"
            }],
            "trader_side": "TAKER"
        }],
        "next_cursor": "LTE=",
        "limit": 100,
        "count": 1
    })
    .to_string();

    assert!(
        parse_raw_trades_page(&body, RawTradeFilter::Maker, trade_address)
            .unwrap()
            .is_empty()
    );
    assert!(
        parse_raw_trades_page(&body, RawTradeFilter::Taker, trade_address)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn post_only_market_order_types_are_rejected_before_venue() {
    assert!(
        PolymarketExecutionAdapter::map_order_type(&submit_req(TimeInForce::Gtc, true)).is_ok()
    );
    assert!(
        PolymarketExecutionAdapter::map_order_type(&submit_req(TimeInForce::Ioc, false)).is_ok()
    );
    assert!(
        PolymarketExecutionAdapter::map_order_type(&submit_req(TimeInForce::Fok, false)).is_ok()
    );
    assert!(matches!(
        PolymarketExecutionAdapter::map_order_type(&submit_req(TimeInForce::Ioc, true)),
        Err(ExecutionError::BadRequest(_))
    ));
    assert!(matches!(
        PolymarketExecutionAdapter::map_order_type(&submit_req(TimeInForce::Fok, true)),
        Err(ExecutionError::BadRequest(_))
    ));
    assert!(
        PolymarketExecutionAdapter::map_order_type(&submit_req(TimeInForce::Gtd, true)).is_ok()
    );
}

#[test]
fn v2_sdk_expiration_is_zero_for_non_gtd_orders() {
    let now_ms = 1_800_000_000_000;
    let future_expiration_ms = now_ms + 300_000;

    for time_in_force in [TimeInForce::Gtc, TimeInForce::Ioc, TimeInForce::Fok] {
        let mut req = submit_req(time_in_force, false);
        req.expires_at_ms = Some(future_expiration_ms);

        assert_eq!(
            PolymarketExecutionAdapter::v2_sdk_expiration_dt(&req, now_ms).timestamp_millis(),
            0,
            "{time_in_force:?} must not send a non-zero V2 SDK expiration"
        );
    }
}

#[test]
fn v2_sdk_expiration_uses_valid_gtd_expiration() {
    let now_ms = 1_800_000_000_000;
    let future_expiration_ms = now_ms + 300_000;
    let mut req = submit_req(TimeInForce::Gtd, true);
    req.expires_at_ms = Some(future_expiration_ms);

    assert_eq!(
        PolymarketExecutionAdapter::v2_sdk_expiration_dt(&req, now_ms).timestamp_millis(),
        PolymarketExecutionAdapter::venue_gtd_expiration_ms(future_expiration_ms) as i64
    );
}

#[test]
fn v2_sdk_expiration_pads_short_live_gtd_ttl_instead_of_defaulting_to_one_hour() {
    let now_ms = 1_800_000_000_000;
    let mut req = submit_req(TimeInForce::Gtd, true);
    req.expires_at_ms = Some(now_ms + 20_000);

    assert_eq!(
        PolymarketExecutionAdapter::v2_sdk_expiration_dt(&req, now_ms).timestamp_millis(),
        (now_ms + 80_000) as i64
    );
}

#[test]
fn v2_market_buy_amount_is_rounded_up_to_usdc_cents() {
    assert_eq!(
        PolymarketExecutionAdapter::v2_market_buy_amount_usdc(0.81, 5.88).unwrap(),
        "4.77"
    );
    assert_eq!(
        PolymarketExecutionAdapter::v2_market_buy_amount_usdc(0.27, 10.5263).unwrap(),
        "2.85"
    );
    assert_eq!(
        PolymarketExecutionAdapter::v2_market_buy_amount_usdc(0.77, 19.85811406628941).unwrap(),
        "15.30"
    );
}

#[test]
fn v2_decimal_string_rejects_non_positive_values() {
    assert!(matches!(
        PolymarketExecutionAdapter::v2_market_buy_amount_usdc(0.0, 5.0),
        Err(ExecutionError::BadRequest(_))
    ));
    assert!(matches!(
        PolymarketExecutionAdapter::v2_market_buy_amount_usdc(0.50, 0.0),
        Err(ExecutionError::BadRequest(_))
    ));
}

#[test]
fn signature_type_parser_accepts_polymarket_codes() {
    assert_eq!(
        PolymarketSignatureType::parse("0").unwrap(),
        PolymarketSignatureType::Eoa
    );
    assert_eq!(
        PolymarketSignatureType::parse("POLY_PROXY").unwrap(),
        PolymarketSignatureType::Proxy
    );
    assert_eq!(
        PolymarketSignatureType::parse("1         # poly_proxy").unwrap(),
        PolymarketSignatureType::Proxy
    );
    assert_eq!(
        PolymarketSignatureType::parse("gnosis_safe").unwrap(),
        PolymarketSignatureType::GnosisSafe
    );
    assert!(PolymarketSignatureType::parse("bad").is_err());
}

#[test]
fn poly1271_uses_wallet_mode_for_ctf_relayer() {
    assert_eq!(PolymarketSignatureType::Poly1271.as_polymarket_code(), 3);
    assert_eq!(PolymarketSignatureType::Poly1271.as_ctf_relayer_code(), 3);
    assert_eq!(PolymarketSignatureType::GnosisSafe.as_ctf_relayer_code(), 2);
}

#[test]
fn relayer_config_prefers_explicit_proxy_wallet_over_funder() {
    let config = PolymarketConfig {
        proxy_wallet_address: Some("0x1111111111111111111111111111111111111111".to_string()),
        ..PolymarketConfig::default()
    };
    let funder = Some("0x2222222222222222222222222222222222222222".to_string());
    assert_eq!(
        relayer_proxy_wallet_address(&config, &funder, PolymarketSignatureType::Poly1271)
            .as_deref(),
        Some("0x1111111111111111111111111111111111111111")
    );

    let config = PolymarketConfig::default();
    assert_eq!(
        relayer_proxy_wallet_address(&config, &funder, PolymarketSignatureType::Proxy).as_deref(),
        None
    );
    assert_eq!(
        relayer_proxy_wallet_address(&config, &funder, PolymarketSignatureType::Poly1271)
            .as_deref(),
        Some("0x2222222222222222222222222222222222222222")
    );
}

#[test]
fn live_price_decimal_matches_polymarket_cent_tick() {
    let price = PolymarketExecutionAdapter::decimal_from_f64(0.01, 2, "limit_price").unwrap();
    assert_eq!(price.to_string(), "0.01");
}

#[test]
fn gtd_expiration_includes_polymarket_security_threshold() {
    assert_eq!(
        PolymarketExecutionAdapter::venue_gtd_expiration_ms(1_000),
        61_000
    );
}

#[test]
fn usdc_balance_parser_handles_base_units_and_decimal_units() {
    assert!(
        (PolymarketExecutionAdapter::usdc_balance_to_usd("106484140") - 106.48414).abs() < 1e-9
    );
    assert!(
        (PolymarketExecutionAdapter::usdc_balance_to_usd("106.48414") - 106.48414).abs() < 1e-9
    );
}

#[test]
fn sdk_validation_errors_are_not_auth_failures() {
    let error = polymarket_client_sdk::error::Error::status(
        polymarket_client_sdk::error::StatusCode::BAD_REQUEST,
        polymarket_client_sdk::error::Method::POST,
        "/order".to_string(),
        "{\"error\":\"order 0xabc is invalid. Size (1.64) lower than the minimum: 5\"}",
    );

    assert!(matches!(
        map_sdk_error(error),
        ExecutionError::BadRequest(_)
    ));
}
