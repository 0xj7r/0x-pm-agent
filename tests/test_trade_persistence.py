from __future__ import annotations

from unittest.mock import MagicMock

from core.trade_persistence import TradePersistence


def test_record_entry_assigns_stable_id_and_resolution_reuses_it():
    memory = MagicMock()
    supabase = MagicMock()
    persistence = TradePersistence("btc", memory, supabase)

    trade = {
        "market_id": "123",
        "direction": "UP",
        "token_price": 0.42,
        "size_usd": 10.0,
        "shares": 23.809523,
        "btc_price": 68000.0,
        "move_pct": 0.08,
        "strategy": "threshold",
        "timestamp": "2026-04-07T12:00:00+00:00",
    }

    persistence.record_entry("threshold", trade)
    entry_payload = supabase.upsert_trade_safe.call_args_list[0][0][0]

    resolution = MagicMock(won=True, pnl_usd=12.34)
    persistence.record_resolution("threshold", trade, resolution, "UP")
    resolution_payload = supabase.upsert_trade_safe.call_args_list[1][0][0]

    assert trade["id"].startswith("btc-threshold-123-")
    assert entry_payload["id"] == trade["id"]
    assert resolution_payload["id"] == trade["id"]


def test_record_entry_respects_existing_trade_id():
    memory = MagicMock()
    supabase = MagicMock()
    persistence = TradePersistence("eth", memory, supabase)

    trade = {
        "id": "eth-threshold-999-existing",
        "market_id": "999",
        "direction": "DOWN",
        "token_price": 0.55,
        "size_usd": 5.0,
        "shares": 9.090909,
        "btc_price": 2100.0,
        "move_pct": -0.07,
        "strategy": "threshold",
        "timestamp": "2026-04-07T12:05:00+00:00",
    }

    persistence.record_entry("threshold", trade)
    payload = supabase.upsert_trade_safe.call_args[0][0]

    assert payload["id"] == "eth-threshold-999-existing"
