"""Tests for the BTC sniper trading engine."""
from __future__ import annotations

import tempfile
from datetime import datetime, timedelta, timezone

import pytest

from clients.binance_ws import TradeUpdate
from core.btc_engine import BTCTradingEngine
from models.market import MarketWindow
from strategies.strategy_config import StrategyConfig


def make_engine(paper: bool = True) -> BTCTradingEngine:
    cfg = StrategyConfig()
    cfg.paper.enabled = paper
    cfg.paper.starting_balance = 100.0
    cfg.signal.confidence_threshold = 0.80
    cfg.signal.w3_price_delta = 1.0
    cfg.signal.w1_order_flow = 0.0
    cfg.signal.w2_microprice = 0.0
    cfg.signal.w4_acceleration = 0.0
    cfg.execution.max_entry_price = 0.05
    cfg.risk.max_position_usd = 10.0
    cfg.risk.max_position_pct = 0.10

    with tempfile.NamedTemporaryFile(suffix=".db", delete=False) as f:
        db_path = f.name

    engine = BTCTradingEngine(cfg, db_path=db_path)
    return engine


def test_engine_initializes():
    engine = make_engine()
    assert engine.balance == 100.0
    assert engine.signal_engine.p_up == 0.5
    assert engine.current_window is None


def test_engine_resets_signal_on_new_window():
    engine = make_engine()
    engine.signal_engine.update(0, 0, 5.0, 0)
    assert engine.signal_engine.p_up != 0.5

    now = datetime.now(timezone.utc)
    window = MarketWindow(
        market_id="m1",
        question="BTC Up/Down",
        start_time=now,
        end_time=now + timedelta(minutes=5),
        up_token_id="tok_up",
        down_token_id="tok_down",
    )
    engine._on_new_window(window)
    assert engine.signal_engine.p_up == 0.5
    assert engine.current_window == window


@pytest.mark.asyncio
async def test_engine_processes_trade_updates():
    engine = make_engine()

    now = datetime.now(timezone.utc)
    engine.current_window = MarketWindow(
        market_id="m1",
        question="BTC Up/Down",
        start_time=now - timedelta(seconds=10),
        end_time=now + timedelta(minutes=4, seconds=50),
        up_token_id="tok_up",
        down_token_id="tok_down",
    )
    engine._window_open_price = 84000.0

    update = TradeUpdate(
        price=84100.0,
        quantity=1.0,
        is_buyer_maker=False,
        timestamp_ms=int(now.timestamp() * 1000),
    )
    await engine._on_binance_trade(update)
    assert engine.signal_engine.p_up > 0.5


@pytest.mark.asyncio
async def test_engine_generates_paper_trade():
    engine = make_engine()

    now = datetime.now(timezone.utc)
    engine.current_window = MarketWindow(
        market_id="m1",
        question="BTC Up/Down",
        start_time=now - timedelta(seconds=30),
        end_time=now + timedelta(minutes=4, seconds=30),
        up_token_id="tok_up",
        down_token_id="tok_down",
        up_price=0.02,
        down_price=0.98,
    )
    engine._window_open_price = 84000.0
    engine._already_traded_this_window = False

    engine.signal_engine.log_odds = 3.0  # p_up ~ 0.953

    trades = engine._check_entry()
    assert len(trades) == 1
    assert trades[0]["direction"] == "UP"
    assert trades[0]["token_price"] <= 0.05


@pytest.mark.asyncio
async def test_engine_no_trade_when_no_cheap_tokens():
    engine = make_engine()

    now = datetime.now(timezone.utc)
    engine.current_window = MarketWindow(
        market_id="m1",
        question="BTC Up/Down",
        start_time=now - timedelta(seconds=30),
        end_time=now + timedelta(minutes=4, seconds=30),
        up_token_id="tok_up",
        down_token_id="tok_down",
        up_price=0.50,
        down_price=0.50,
    )
    engine._window_open_price = 84000.0
    engine._already_traded_this_window = False
    engine.signal_engine.log_odds = 3.0

    trades = engine._check_entry()
    assert len(trades) == 0


@pytest.mark.asyncio
async def test_engine_no_double_trade():
    engine = make_engine()

    now = datetime.now(timezone.utc)
    engine.current_window = MarketWindow(
        market_id="m1",
        question="BTC Up/Down",
        start_time=now - timedelta(seconds=30),
        end_time=now + timedelta(minutes=4, seconds=30),
        up_token_id="tok_up",
        down_token_id="tok_down",
        up_price=0.02,
        down_price=0.98,
    )
    engine._window_open_price = 84000.0
    engine._already_traded_this_window = True
    engine.signal_engine.log_odds = 3.0

    trades = engine._check_entry()
    assert len(trades) == 0
