"""Tests for circuit breaker, rate limiter, drawdown protection, and stop loss.

These prevent catastrophic trading losses.
"""
from __future__ import annotations

import time
from unittest.mock import MagicMock

import pytest

from core.risk import RiskManager
from config import Config


def _make_risk() -> RiskManager:
    cfg = Config()
    cfg.MAX_POSITION_PCT = 0.06
    cfg.MAX_POSITION_USD = 50.0
    cfg.MIN_EDGE_THRESHOLD = 0.15
    cfg.KILL_BALANCE_USD = 5.0
    cfg.DAILY_LOSS_LIMIT_PCT = 0.20
    cfg.MAX_CONCURRENT_POSITIONS = 10
    cfg.LOSS_COOLDOWN_TRADES = 3
    cfg.LOSS_COOLDOWN_SECONDS = 1800
    rm = RiskManager(cfg)
    rm.set_bankroll(100.0)
    return rm


class TestMaxDrawdownCircuitBreaker:
    """Halt trading if cumulative P&L drops X% from peak."""

    def test_trading_halted_after_drawdown_threshold(self):
        rm = _make_risk()
        rm.set_peak_balance(200.0)

        rm.record_trade_result(-30.0)
        rm.record_trade_result(-30.0)

        assert rm.is_drawdown_breaker_tripped(current_balance=140.0) is True, (
            "Should trip circuit breaker: balance dropped 30% from peak ($200 -> $140)"
        )

    def test_trading_allowed_within_drawdown_threshold(self):
        rm = _make_risk()
        rm.set_peak_balance(200.0)

        rm.record_trade_result(-10.0)

        assert rm.is_drawdown_breaker_tripped(current_balance=190.0) is False

    def test_peak_balance_updates_on_new_high(self):
        rm = _make_risk()
        rm.set_peak_balance(100.0)
        rm.update_peak_balance(150.0)

        assert rm._peak_balance == 150.0

    def test_peak_balance_does_not_decrease(self):
        rm = _make_risk()
        rm.set_peak_balance(150.0)
        rm.update_peak_balance(120.0)

        assert rm._peak_balance == 150.0


class TestRateLimiter:
    """Max N trades per hour per coin to prevent runaway loops."""

    def test_blocks_after_max_trades_per_hour(self):
        rm = _make_risk()

        for _ in range(20):
            rm.record_trade_entry()

        assert rm.is_rate_limited() is True, (
            "Should block trading after 20 trades in one hour"
        )

    def test_allows_trading_under_limit(self):
        rm = _make_risk()

        for _ in range(5):
            rm.record_trade_entry()

        assert rm.is_rate_limited() is False

    def test_old_entries_expire(self):
        rm = _make_risk()

        old_time = time.time() - 3700
        rm._trade_timestamps = [old_time] * 25

        assert rm.is_rate_limited() is False, (
            "Entries older than 1 hour should not count toward rate limit"
        )


class TestStopLoss:
    """Mid-window exit when token price drops significantly after entry."""

    def test_stop_loss_triggers_on_large_drop(self):
        rm = _make_risk()

        should_exit = rm.should_stop_loss(
            entry_price=0.50,
            current_price=0.15,
            stop_loss_pct=0.50,
        )

        assert should_exit is True, (
            "Should trigger stop loss: price dropped from $0.50 to $0.15 (70% drop, threshold 50%)"
        )

    def test_stop_loss_does_not_trigger_on_small_drop(self):
        rm = _make_risk()

        should_exit = rm.should_stop_loss(
            entry_price=0.50,
            current_price=0.45,
            stop_loss_pct=0.50,
        )

        assert should_exit is False

    def test_stop_loss_does_not_trigger_on_price_increase(self):
        rm = _make_risk()

        should_exit = rm.should_stop_loss(
            entry_price=0.50,
            current_price=0.80,
            stop_loss_pct=0.50,
        )

        assert should_exit is False

    def test_stop_loss_handles_zero_entry(self):
        rm = _make_risk()

        should_exit = rm.should_stop_loss(
            entry_price=0.0,
            current_price=0.50,
            stop_loss_pct=0.50,
        )

        assert should_exit is False
