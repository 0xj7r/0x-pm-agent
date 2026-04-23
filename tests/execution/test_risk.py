"""Tests for RiskManager."""

from __future__ import annotations

import time
from datetime import UTC, datetime, timedelta

import pytest

from config import Config
from core.risk import RejectReason, RiskManager
from models.market import Outcome
from models.trade import Side, Signal, SignalSource


def _make_signal(edge: float = 0.20, confidence: float = 0.7, market_price: float = 0.5) -> Signal:
    return Signal(
        market_id="m1",
        market_question="Test?",
        outcome=Outcome.YES,
        side=Side.BUY,
        source=SignalSource.WEATHER,
        fair_value=market_price + edge,
        market_price=market_price,
        edge=edge,
        confidence=confidence,
    )


class TestPassesFilters:
    def test_accepts_good_signal(self):
        rm = RiskManager(Config())
        assert rm.passes_filters(_make_signal(edge=0.20)) is True

    def test_rejects_low_edge(self):
        rm = RiskManager(Config())
        assert rm.passes_filters(_make_signal(edge=0.05)) is False

    def test_rejects_low_confidence(self):
        rm = RiskManager(Config())
        assert rm.passes_filters(_make_signal(confidence=0.2)) is False

    def test_rejects_max_positions(self):
        rm = RiskManager(Config())
        rm.set_open_positions(10)
        assert rm.passes_filters(_make_signal()) is False

    def test_daily_loss_limit(self):
        rm = RiskManager(Config())
        rm.set_bankroll(100.0)
        # Lose more than 20% of bankroll
        rm.record_trade_result(-25.0)
        assert rm.passes_filters(_make_signal()) is False

    def test_cooldown_after_consecutive_losses(self):
        rm = RiskManager(Config())
        rm.set_bankroll(1000.0)  # large bankroll so daily limit isn't hit
        for _ in range(3):
            rm.record_trade_result(-1.0)
        assert rm.passes_filters(_make_signal()) is False


class TestKellySize:
    def test_basic_sizing(self):
        rm = RiskManager(Config())
        size = rm.kelly_size(edge=0.20, odds=0.40, confidence=0.5, bankroll=100.0)
        assert size > 0
        assert size <= 2.0  # MAX_POSITION_USD default

    def test_zero_bankroll(self):
        rm = RiskManager(Config())
        assert rm.kelly_size(edge=0.2, odds=0.4, confidence=0.5, bankroll=0) == 0.0

    def test_zero_edge(self):
        rm = RiskManager(Config())
        # With 0 edge at odds=0.5, kelly = (1*0.5 - 0.5)/1 = 0
        assert rm.kelly_size(edge=0.0, odds=0.5, confidence=0.5, bankroll=100.0) == 0.0

    def test_negative_kelly_returns_zero(self):
        rm = RiskManager(Config())
        # Negative edge means kelly < 0
        assert rm.kelly_size(edge=-0.3, odds=0.5, confidence=0.5, bankroll=100.0) == 0.0

    def test_caps_at_max_position(self):
        rm = RiskManager(Config())
        size = rm.kelly_size(edge=0.5, odds=0.3, confidence=0.5, bankroll=10000.0)
        assert size <= 2.0  # MAX_POSITION_USD

    def test_dust_trade_returns_zero(self):
        rm = RiskManager(Config())
        # Very small bankroll → size < $1 → returns 0
        assert rm.kelly_size(edge=0.2, odds=0.4, confidence=0.5, bankroll=5.0) == 0.0


class TestShouldDie:
    def test_kills_at_low_balance(self):
        rm = RiskManager(Config())
        assert rm.should_die(4.0) is True

    def test_survives_above_kill(self):
        rm = RiskManager(Config())
        assert rm.should_die(10.0) is False


class TestDailyTradeCap:
    def test_disabled_by_default(self):
        rm = RiskManager(Config())
        for _ in range(100):
            rm.record_trade_entry()
        assert rm.is_daily_trade_cap_reached() is False

    def test_fires_at_n_plus_one(self):
        cfg = Config()
        cfg.MAX_DAILY_TRADES = 3
        rm = RiskManager(cfg)
        for _ in range(3):
            rm.record_trade_entry()
        assert rm.is_daily_trade_cap_reached() is True

    def test_under_limit_allows_trades(self):
        cfg = Config()
        cfg.MAX_DAILY_TRADES = 3
        rm = RiskManager(cfg)
        rm.record_trade_entry()
        rm.record_trade_entry()
        assert rm.is_daily_trade_cap_reached() is False

    def test_resets_after_24h(self):
        cfg = Config()
        cfg.MAX_DAILY_TRADES = 2
        rm = RiskManager(cfg)
        old = datetime.now(UTC) - timedelta(hours=25)
        rm._entry_timestamps.append(old)
        rm._entry_timestamps.append(old)
        assert rm.is_daily_trade_cap_reached() is False
        assert len(rm._entry_timestamps) == 0


class TestRejectStreak:
    def test_disabled_by_default(self):
        rm = RiskManager(Config())
        for _ in range(10):
            rm.record_order_rejection()
        assert rm.is_reject_streak_tripped() is False

    def test_fires_at_n_consecutive(self):
        cfg = Config()
        cfg.REJECT_STREAK_LIMIT = 3
        cfg.REJECT_COOLDOWN_SECONDS = 60
        cfg.REJECT_STREAK_COOLDOWN_SECONDS = 60
        rm = RiskManager(cfg)
        rm.record_order_rejection()
        rm.record_order_rejection()
        # Unknown rejects trigger a short cooldown by design; clear it to
        # test the streak threshold behavior itself.
        rm._reject_cooldown_until = time.time() - 1
        assert rm.is_reject_streak_tripped() is False
        rm.record_order_rejection()
        assert rm.is_reject_streak_tripped() is True

    def test_resets_on_success(self):
        cfg = Config()
        cfg.REJECT_STREAK_LIMIT = 2
        cfg.REJECT_COOLDOWN_SECONDS = 60
        cfg.REJECT_STREAK_COOLDOWN_SECONDS = 60
        rm = RiskManager(cfg)
        rm.record_order_rejection()
        rm.record_order_rejection()
        assert rm.is_reject_streak_tripped() is True
        rm.record_order_success()
        assert rm.is_reject_streak_tripped() is False
        assert rm._reject_streak == 0

    def test_benign_reject_does_not_count(self):
        cfg = Config()
        cfg.REJECT_STREAK_LIMIT = 2
        rm = RiskManager(cfg)
        rm.record_order_rejection(reason=RejectReason.UNKNOWN)
        rm.record_order_rejection(reason=RejectReason.PRICE_CROSSED)
        # Benign reject resets streak and should not trip
        rm._reject_cooldown_until = time.time() - 1
        assert rm.is_reject_streak_tripped() is False
        assert rm._reject_streak == 0

    def test_fatal_reject_halts_immediately(self):
        rm = RiskManager(Config())
        rm.record_order_rejection(
            reason=RejectReason.INSUFFICIENT_FUNDS,
            message="not enough balance",
        )
        assert rm.is_reject_streak_tripped() is True
