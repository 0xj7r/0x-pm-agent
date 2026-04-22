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
        "book_snapshot": {"selected": {"token_id": "tok-1"}},
        "btc_volume_60s": 15.0,
        "fee_bps_ceiling": 10,
        "fill_details": {"status": "matched", "filled_shares": 23.809523},
        "strategy": "threshold",
        "timestamp": "2026-04-07T12:00:00+00:00",
    }

    persistence.record_entry("threshold", trade)
    entry_payload = supabase.upsert_trade_safe.call_args_list[0][0][0]

    resolution = MagicMock(won=True, pnl_usd=12.34)
    persistence.record_resolution("threshold", trade, resolution, "UP", source="push")
    resolution_payload = supabase.upsert_trade_safe.call_args_list[1][0][0]
    _, resolution_kwargs = memory.save_event.call_args_list[1]

    assert trade["id"].startswith("btc-threshold-123-")
    assert entry_payload["id"] == trade["id"]
    assert resolution_payload["id"] == trade["id"]
    assert resolution_kwargs["details"]["source"] == "push"


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
        "book_snapshot": {"selected": {"token_id": "tok-2"}},
        "btc_volume_60s": 9.0,
        "fee_bps_ceiling": 0,
        "fill_details": {"status": "matched", "filled_shares": 9.090909},
        "strategy": "threshold",
        "timestamp": "2026-04-07T12:05:00+00:00",
    }

    persistence.record_entry("threshold", trade)
    payload = supabase.upsert_trade_safe.call_args[0][0]

    assert payload["id"] == "eth-threshold-999-existing"


def test_record_paper_shadow_entry_uses_same_trade_and_decision_ids():
    memory = MagicMock()
    supabase = MagicMock()
    persistence = TradePersistence("btc", memory, supabase)

    trade = {
        "decision_id": "dec-123",
        "market_id": "123",
        "direction": "UP",
        "token_price": 0.42,
        "size_usd": 10.0,
        "shares": 23.809523,
        "btc_price": 68000.0,
        "move_pct": 0.08,
        "strategy": "timing",
        "timestamp": "2026-04-07T12:00:00+00:00",
    }

    persistence.record_paper_shadow_entry("timing", trade)

    memory.save_event.assert_called_once()
    _, kwargs = memory.save_event.call_args
    assert kwargs["window_id"] == "123"
    assert kwargs["event_type"] == "paper_shadow_entry"
    assert kwargs["btc_price"] == 68000.0
    assert kwargs["details"]["decision_id"] == "dec-123"
    assert kwargs["details"]["trade_id"] == trade["id"]
    assert kwargs["details"]["execution_mode"] == "paper_shadow"
    assert kwargs["details"]["live_paired"] is True
    supabase.upsert_trade_safe.assert_not_called()
