"""Tests for the threshold-based trading engine."""
from __future__ import annotations

import tempfile
from datetime import datetime, timedelta, timezone
from unittest.mock import MagicMock, patch

import pytest

from clients.binance_ws import TradeUpdate
from core.engine import BTCTradingEngine
from models.market import MarketWindow
from strategies.strategy_config import StrategyConfig, ThresholdConfig


def make_engine(
    move_threshold: float = 0.08,
    max_entry: float = 0.55,
    coin: str = "btc",
) -> BTCTradingEngine:
    cfg = StrategyConfig()
    cfg.paper.enabled = True
    cfg.paper.starting_balance = 100.0
    cfg.coins[coin] = ThresholdConfig(
        move_threshold=move_threshold,
        max_entry=max_entry,
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
        return BTCTradingEngine(cfg, db_path=db_path, coin=coin)


def make_window(
    *,
    up_price: float = 0.02,
    down_price: float = 0.98,
) -> MarketWindow:
    now = datetime.now(timezone.utc)
    return MarketWindow(
        market_id="m1",
        question="BTC Up/Down",
        start_time=now - timedelta(seconds=30),
        end_time=now + timedelta(minutes=4, seconds=30),
        up_token_id="tok_up",
        down_token_id="tok_down",
        up_price=up_price,
        down_price=down_price,
    )


def test_engine_initializes():
    engine = make_engine()
    assert engine.balance == 100.0
    assert engine.current_window is None
    assert engine._strategy.coin == "btc"
    assert engine._strategy.name == "threshold"
    assert engine._strategy.params["move"] == 0.08
    engine.memory.close()


def test_engine_resets_window_state_on_new_window():
    engine = make_engine()
    engine._current_btc_price = 84250.0
    engine._already_traded_this_window = True

    window = make_window()
    engine._on_new_window(window)

    assert engine.current_window == window
    assert engine._window_open_price == 84250.0
    assert engine._already_traded_this_window is False
    engine.memory.close()


@pytest.mark.asyncio
async def test_engine_processes_trade_updates():
    engine = make_engine()

    now = datetime.now(timezone.utc)
    update = TradeUpdate(
        price=84100.0,
        quantity=1.0,
        is_buyer_maker=False,
        timestamp_ms=int(now.timestamp() * 1000),
    )
    await engine._on_binance_trade(update)

    assert engine._current_btc_price == 84100.0
    engine.memory.close()


def test_engine_generates_threshold_trade():
    engine = make_engine()
    engine.current_window = make_window(up_price=0.02, down_price=0.98)
    engine._window_open_price = 100.0
    engine._current_btc_price = 110.0
    engine._already_traded_this_window = False

    engine.poly_ws = MagicMock()
    engine.poly_ws.has_live_book.return_value = True
    engine.poly_ws.get_price.side_effect = lambda tid: (
        0.02 if tid == engine.current_window.up_token_id else 0.98
    )

    trades = engine._check_entry()

    assert len(trades) == 1
    assert trades[0]["direction"] == "UP"
    # token_price now reflects cross-the-spread slippage so paper and live
    # PnL are comparable. Raw book price is preserved as raw_token_price.
    assert trades[0]["raw_token_price"] == 0.02
    assert trades[0]["token_price"] == pytest.approx(0.02 + 0.01)
    # shares are computed against the effective (post-slippage) price.
    assert trades[0]["shares"] == pytest.approx(
        trades[0]["size_usd"] / trades[0]["token_price"]
    )
    assert trades[0]["strategy"] == "threshold"
    assert trades[0]["size_usd"] > 0

    events = engine.memory.get_events_for_window("m1")
    assert len(events) == 1
    assert events[0]["event_type"] == "entry"
    engine.memory.close()


def test_engine_applies_configured_slippage_to_entry_price():
    """Verify the paper entry price picks up a non-default RiskConfig slippage."""
    engine = make_engine()
    # Non-default slippage to prove the engine reads from RiskConfig.
    engine._live_entry_slippage_usd = 0.02
    engine.current_window = make_window(up_price=0.38, down_price=0.62)
    engine._window_open_price = 100.0
    engine._current_btc_price = 110.0
    engine._already_traded_this_window = False

    engine.poly_ws = MagicMock()
    engine.poly_ws.has_live_book.return_value = True
    engine.poly_ws.get_price.side_effect = lambda tid: (
        0.38 if tid == engine.current_window.up_token_id else 0.62
    )

    trades = engine._check_entry()

    assert len(trades) == 1
    assert trades[0]["raw_token_price"] == pytest.approx(0.38)
    assert trades[0]["token_price"] == pytest.approx(0.38 + 0.02)
    engine.memory.close()


def test_engine_caps_buy_price_at_max():
    """token_price + slippage must never exceed MAX_BUY_PRICE (0.99)."""
    from core.engine import MAX_BUY_PRICE

    engine = make_engine(max_entry=1.0)
    engine.current_window = make_window(up_price=0.995, down_price=0.005)
    engine._window_open_price = 100.0
    engine._current_btc_price = 110.0
    engine._already_traded_this_window = False

    engine.poly_ws = MagicMock()
    engine.poly_ws.has_live_book.return_value = True
    engine.poly_ws.get_price.side_effect = lambda tid: (
        0.995 if tid == engine.current_window.up_token_id else 0.005
    )

    trades = engine._check_entry()

    assert len(trades) == 1
    assert trades[0]["token_price"] == pytest.approx(MAX_BUY_PRICE)
    engine.memory.close()


def test_engine_skips_expensive_entry():
    engine = make_engine(max_entry=0.55)
    engine.current_window = make_window(up_price=0.70, down_price=0.30)
    engine._window_open_price = 100.0
    engine._current_btc_price = 110.0

    trades = engine._check_entry()

    assert trades == []
    assert engine._already_traded_this_window is False
    engine.memory.close()


def test_engine_no_double_trade():
    engine = make_engine()
    engine.current_window = make_window(up_price=0.02, down_price=0.98)
    engine._window_open_price = 100.0
    engine._current_btc_price = 110.0
    engine._already_traded_this_window = True

    trades = engine._check_entry()

    assert trades == []
    engine.memory.close()
