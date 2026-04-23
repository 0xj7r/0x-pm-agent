from __future__ import annotations

from scripts.backfill_supabase_markets import market_row_from_header


def test_market_row_from_header_maps_coin_specific_fields():
    header = {
        "market_id": "123",
        "slug": "eth-updown-5m",
        "market_type": "5m",
        "start_time": "2026-04-07T12:00:00Z",
        "end_time": "2026-04-07T12:05:00Z",
        "eth_price_start": 2050.5,
        "eth_price_end": 2055.0,
        "winner": "Up",
        "final_volume": 12345.0,
        "final_liquidity": 6789.0,
    }

    row = market_row_from_header("eth", header)

    assert row == {
        "market_id": "123",
        "coin": "eth",
        "slug": "eth-updown-5m",
        "market_type": "5m",
        "start_time": "2026-04-07T12:00:00Z",
        "end_time": "2026-04-07T12:05:00Z",
        "price_start": 2050.5,
        "price_end": 2055.0,
        "winner": "Up",
        "final_volume": 12345.0,
        "final_liquidity": 6789.0,
    }


def test_market_row_from_header_prefers_btc_alias_when_present():
    header = {
        "market_id": "456",
        "btc_price_start": 70000.0,
        "btc_price_end": 70100.0,
    }

    row = market_row_from_header("btc", header)

    assert row["price_start"] == 70000.0
    assert row["price_end"] == 70100.0
