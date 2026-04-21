"""Integration tests for threshold-based paper trading."""
from __future__ import annotations

import tempfile
from datetime import datetime, timedelta, timezone
from unittest.mock import MagicMock, patch

import pytest

from clients.binance_ws import TradeUpdate
from core.engine import BTCTradingEngine
from models.market import MarketWindow
from strategies.strategy_config import StrategyConfig, ThresholdConfig


def make_engine(move_threshold: float = 0.08) -> BTCTradingEngine:
    cfg = StrategyConfig()
    cfg.paper.enabled = True
    cfg.paper.starting_balance = 100.0
    cfg.coins["btc"] = ThresholdConfig(
        move_threshold=move_threshold,
        max_entry=0.55,
    )
    cfg.risk.max_position_usd = 10.0
    cfg.risk.max_position_pct = 0.10
    cfg.risk.kelly_multiplier = 0.25
    cfg.risk.cheap_token_multiplier = 2.0

    with tempfile.NamedTemporaryFile(suffix=".db", delete=False) as f:
        db_path = f.name

    mock_supa = MagicMock()
    mock_supa.health_check.return_value = True
    with patch("shared.supabase_client.SupabaseClient", return_value=mock_supa):
        return BTCTradingEngine(cfg, db_path=db_path)


def make_window(up_price: float = 0.02, down_price: float = 0.98) -> MarketWindow:
    now = datetime.now(timezone.utc)
    return MarketWindow(
        market_id="integration_test_001",
        question="Bitcoin Up or Down - Test",
        start_time=now - timedelta(seconds=30),
        end_time=now + timedelta(minutes=4, seconds=30),
        up_token_id="tok_up_001",
        down_token_id="tok_down_001",
        up_price=up_price,
        down_price=down_price,
    )


@pytest.mark.asyncio
async def test_full_pipeline_paper_trade():
    engine = make_engine()
    engine.current_window = make_window()
    engine._window_open_price = 84000.0

    engine.poly_ws = MagicMock()
    engine.poly_ws.has_live_book.return_value = True
    engine.poly_ws.get_price.side_effect = lambda tid: (
        0.02 if tid == engine.current_window.up_token_id else 0.98
    )

    update = TradeUpdate(
        price=92400.0,
        quantity=0.5,
        is_buyer_maker=False,
        timestamp_ms=int(datetime.now(timezone.utc).timestamp() * 1000),
    )
    await engine._on_binance_trade(update)

    trades = engine._check_entry()

    assert len(trades) == 1
    trade = trades[0]
    assert trade["direction"] == "UP"
    # token_price is now the effective fill price (raw + live_entry_slippage_usd).
    assert trade["raw_token_price"] == 0.02
    assert trade["token_price"] == pytest.approx(0.02 + 0.01)
    assert trade["size_usd"] > 0

    events = engine.memory.get_events_for_window("integration_test_001")
    assert len(events) >= 1
    assert events[-1]["event_type"] == "entry"

    trades2 = engine._check_entry()
    assert trades2 == []
    engine.memory.close()


@pytest.mark.asyncio
async def test_full_pipeline_no_trade_when_move_below_threshold():
    engine = make_engine(move_threshold=15.0)
    engine.current_window = make_window()
    engine._window_open_price = 84000.0

    update = TradeUpdate(
        price=84840.0,
        quantity=0.3,
        is_buyer_maker=False,
        timestamp_ms=int(datetime.now(timezone.utc).timestamp() * 1000),
    )
    await engine._on_binance_trade(update)

    trades = engine._check_entry()

    assert trades == []
    assert engine.memory.get_events_for_window("integration_test_001") == []
    engine.memory.close()
