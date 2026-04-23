"""Tests for BTC Up/Down 5-minute market discovery."""
from __future__ import annotations

from datetime import datetime, timezone

import pytest

from models.market import MarketWindow


def test_market_window_time_remaining():
    now = datetime(2026, 3, 31, 7, 20, 0, tzinfo=timezone.utc)
    window = MarketWindow(
        market_id="abc123",
        question="Bitcoin Up or Down - March 31, 3:20AM-3:25AM ET",
        start_time=datetime(2026, 3, 31, 7, 20, 0, tzinfo=timezone.utc),
        end_time=datetime(2026, 3, 31, 7, 25, 0, tzinfo=timezone.utc),
        up_token_id="tok_up",
        down_token_id="tok_down",
    )
    remaining = window.time_remaining(now)
    assert remaining == 300.0


def test_market_window_is_active():
    start = datetime(2026, 3, 31, 7, 20, 0, tzinfo=timezone.utc)
    end = datetime(2026, 3, 31, 7, 25, 0, tzinfo=timezone.utc)
    window = MarketWindow(
        market_id="abc123",
        question="Bitcoin Up or Down",
        start_time=start,
        end_time=end,
        up_token_id="tok_up",
        down_token_id="tok_down",
    )
    during = datetime(2026, 3, 31, 7, 22, 0, tzinfo=timezone.utc)
    before = datetime(2026, 3, 31, 7, 19, 0, tzinfo=timezone.utc)
    after = datetime(2026, 3, 31, 7, 26, 0, tzinfo=timezone.utc)

    assert window.is_active(during) is True
    assert window.is_active(before) is False
    assert window.is_active(after) is False


def test_market_window_elapsed_seconds():
    start = datetime(2026, 3, 31, 7, 20, 0, tzinfo=timezone.utc)
    end = datetime(2026, 3, 31, 7, 25, 0, tzinfo=timezone.utc)
    window = MarketWindow(
        market_id="abc123",
        question="test",
        start_time=start,
        end_time=end,
        up_token_id="u",
        down_token_id="d",
    )
    at = datetime(2026, 3, 31, 7, 21, 30, tzinfo=timezone.utc)
    assert window.elapsed_seconds(at) == 90.0


def test_signal_source_has_crypto_sniper():
    from models.trade import SignalSource
    assert SignalSource.CRYPTO_SNIPER == "crypto_sniper"
