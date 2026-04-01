"""Integration test: full pipeline from Binance trade to paper trade."""
from __future__ import annotations

import tempfile
from datetime import datetime, timedelta, timezone

import pytest

from clients.binance_ws import TradeUpdate
from core.btc_engine import BTCTradingEngine
from models.market import MarketWindow
from strategies.strategy_config import StrategyConfig


@pytest.mark.asyncio
async def test_full_pipeline_paper_trade():
    """Simulate: BTC moves up strongly -> signal fires -> paper trade executed."""
    cfg = StrategyConfig()
    cfg.paper.enabled = True
    cfg.paper.starting_balance = 100.0
    cfg.signal.confidence_threshold = 0.80
    cfg.signal.w3_price_delta = 1.0
    cfg.signal.w1_order_flow = 0.0
    cfg.signal.w2_microprice = 0.0
    cfg.signal.w4_acceleration = 0.0
    cfg.execution.max_entry_price = 0.05
    cfg.risk.max_position_usd = 10.0
    cfg.risk.max_position_pct = 0.10
    cfg.risk.kelly_multiplier = 0.25
    cfg.risk.cheap_token_multiplier = 2.0

    with tempfile.NamedTemporaryFile(suffix=".db", delete=False) as f:
        db_path = f.name

    engine = BTCTradingEngine(cfg, db_path=db_path)

    now = datetime.now(timezone.utc)
    engine.current_window = MarketWindow(
        market_id="integration_test_001",
        question="Bitcoin Up or Down - Test",
        start_time=now - timedelta(seconds=30),
        end_time=now + timedelta(minutes=4, seconds=30),
        up_token_id="tok_up_001",
        down_token_id="tok_down_001",
        up_price=0.02,
        down_price=0.98,
    )
    engine._window_open_price = 84000.0
    engine._last_btc_price = 84000.0

    for i in range(20):
        update = TradeUpdate(
            price=84000.0 + (i + 1) * 10,
            quantity=0.5,
            is_buyer_maker=False,
            timestamp_ms=int((now + timedelta(seconds=i)).timestamp() * 1000),
        )
        await engine._on_binance_trade(update)

    assert engine.signal_engine.p_up > 0.80, f"p_up={engine.signal_engine.p_up}"

    trades = engine._check_entry()
    assert len(trades) == 1
    trade = trades[0]
    assert trade["direction"] == "UP"
    assert trade["token_price"] == 0.02
    assert trade["size_usd"] > 0

    events = engine.memory.get_events_for_window("integration_test_001")
    assert len(events) >= 1
    assert events[-1]["event_type"] == "entry"

    trades2 = engine._check_entry()
    assert len(trades2) == 0

    engine.memory.close()


@pytest.mark.asyncio
async def test_full_pipeline_no_trade_when_uncertain():
    """Simulate: BTC moves sideways -> no signal -> no trade."""
    cfg = StrategyConfig()
    cfg.paper.enabled = True
    cfg.signal.confidence_threshold = 0.90
    cfg.signal.w3_price_delta = 0.5
    cfg.signal.w1_order_flow = 0.0
    cfg.signal.w2_microprice = 0.0
    cfg.signal.w4_acceleration = 0.0
    cfg.execution.max_entry_price = 0.05

    with tempfile.NamedTemporaryFile(suffix=".db", delete=False) as f:
        db_path = f.name

    engine = BTCTradingEngine(cfg, db_path=db_path)

    now = datetime.now(timezone.utc)
    engine.current_window = MarketWindow(
        market_id="integration_test_002",
        question="Bitcoin Up or Down - Test Sideways",
        start_time=now - timedelta(seconds=30),
        end_time=now + timedelta(minutes=4, seconds=30),
        up_token_id="tok_up",
        down_token_id="tok_down",
        up_price=0.02,
        down_price=0.98,
    )
    engine._window_open_price = 84000.0
    engine._last_btc_price = 84000.0

    for i in range(20):
        direction = 1 if i % 2 == 0 else -1
        update = TradeUpdate(
            price=84000.0 + direction * 5,
            quantity=0.3,
            is_buyer_maker=(i % 2 == 1),
            timestamp_ms=int((now + timedelta(seconds=i)).timestamp() * 1000),
        )
        await engine._on_binance_trade(update)

    assert not engine.signal_engine.confident

    trades = engine._check_entry()
    assert len(trades) == 0

    engine.memory.close()
