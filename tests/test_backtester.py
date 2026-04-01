"""Tests for the BTC backtesting engine."""
from __future__ import annotations

import pytest

from backtesting.btc_backtest import (
    BacktestResult,
    SimulatedWindow,
    run_backtest,
)
from strategies.strategy_config import StrategyConfig


def make_window(
    direction: str,
    price_move_pct: float,
    up_price: float = 0.02,
    down_price: float = 0.98,
) -> SimulatedWindow:
    """Create a simulated 5-min window with known outcome."""
    return SimulatedWindow(
        market_id=f"test_{direction}_{price_move_pct}",
        resolved_direction=direction,
        price_move_pct=price_move_pct,
        up_price=up_price,
        down_price=down_price,
    )


def test_backtest_strong_signals_win():
    """Strong BTC moves in one direction should produce winning trades."""
    cfg = StrategyConfig()
    cfg.signal.w3_price_delta = 1.0
    cfg.signal.w1_order_flow = 0.0
    cfg.signal.w2_microprice = 0.0
    cfg.signal.w4_acceleration = 0.0
    cfg.signal.confidence_threshold = 0.80
    cfg.execution.max_entry_price = 0.05
    cfg.risk.kelly_multiplier = 0.25
    cfg.risk.cheap_token_multiplier = 2.0
    cfg.risk.max_position_usd = 10.0
    cfg.risk.max_position_pct = 0.10

    windows = [
        make_window("UP", 3.0),     # strong up move (3% BTC move)
        make_window("DOWN", -3.0),  # strong down move
        make_window("UP", 2.5),     # moderate up
    ]

    result = run_backtest(cfg, windows, starting_balance=100.0)
    assert result.num_trades > 0
    assert result.total_pnl > 0  # should be profitable on strong signals


def test_backtest_no_trades_on_sideways():
    """Sideways markets shouldn't trigger trades."""
    cfg = StrategyConfig()
    cfg.signal.w3_price_delta = 0.5
    cfg.signal.confidence_threshold = 0.95  # very high threshold
    cfg.signal.w1_order_flow = 0.0
    cfg.signal.w2_microprice = 0.0
    cfg.signal.w4_acceleration = 0.0
    cfg.execution.max_entry_price = 0.05

    windows = [
        make_window("UP", 0.01),    # barely moved
        make_window("DOWN", -0.01), # barely moved
        make_window("UP", 0.02),    # barely moved
    ]

    result = run_backtest(cfg, windows, starting_balance=100.0)
    assert result.num_trades == 0


def test_backtest_no_trade_when_tokens_expensive():
    """Should skip when no cheap tokens available."""
    cfg = StrategyConfig()
    cfg.signal.w3_price_delta = 1.0
    cfg.signal.confidence_threshold = 0.80
    cfg.signal.w1_order_flow = 0.0
    cfg.signal.w2_microprice = 0.0
    cfg.signal.w4_acceleration = 0.0
    cfg.execution.max_entry_price = 0.05

    windows = [
        make_window("UP", 0.5, up_price=0.50, down_price=0.50),  # no cheap tokens
    ]

    result = run_backtest(cfg, windows, starting_balance=100.0)
    assert result.num_trades == 0


def test_backtest_result_metrics():
    cfg = StrategyConfig()
    cfg.signal.w3_price_delta = 1.0
    cfg.signal.confidence_threshold = 0.80
    cfg.signal.w1_order_flow = 0.0
    cfg.signal.w2_microprice = 0.0
    cfg.signal.w4_acceleration = 0.0
    cfg.execution.max_entry_price = 0.05
    cfg.risk.kelly_multiplier = 0.25
    cfg.risk.cheap_token_multiplier = 2.0
    cfg.risk.max_position_usd = 10.0
    cfg.risk.max_position_pct = 0.10

    windows = [
        make_window("UP", 0.5),
        make_window("DOWN", -0.4),
        make_window("UP", 0.6),
        make_window("DOWN", 0.3),  # resolved DOWN but price went up = loss
    ]

    result = run_backtest(cfg, windows, starting_balance=100.0)
    assert result.num_trades >= 0
    assert isinstance(result.win_rate, float)
    assert isinstance(result.total_pnl, float)
    assert isinstance(result.ev_per_trade, float)
    assert result.ending_balance >= 0
