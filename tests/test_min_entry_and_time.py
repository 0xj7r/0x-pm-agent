"""Tests for min_entry price floor and time-of-day trading filter."""
from __future__ import annotations

import pytest
from backtesting.precompute import precompute_market
from strategies.registry import build_check_fn


def _make_pm(price_up: float, price_down: float, move_pct: float = 0.10):
    """Build a precomputed market with one snapshot."""
    btc_open = 67000.0
    btc_now = btc_open * (1 + move_pct / 100)
    snaps = [(btc_now, price_up, price_down)]
    return precompute_market("test", "Up", btc_open, snaps)


class TestMinEntry:
    """Strategy should reject entries below min_entry price."""

    def test_rejects_entry_below_min_entry(self):
        pm = _make_pm(price_up=0.30, price_down=0.70, move_pct=0.10)
        fn = build_check_fn("threshold", {"move": 0.08, "max_entry": 0.55, "min_entry": 0.48})
        result = fn(pm, 0)
        assert result is None or result == "SKIP", (
            f"Should reject UP entry at $0.30 (below min_entry $0.48), got {result}"
        )

    def test_accepts_entry_at_min_entry(self):
        pm = _make_pm(price_up=0.50, price_down=0.50, move_pct=0.10)
        fn = build_check_fn("threshold", {"move": 0.08, "max_entry": 0.55, "min_entry": 0.48})
        result = fn(pm, 0)
        assert result == "Up", f"Should accept UP entry at $0.50 (above min_entry $0.48), got {result}"

    def test_rejects_down_entry_below_min(self):
        pm = _make_pm(price_up=0.70, price_down=0.35, move_pct=-0.10)
        fn = build_check_fn("threshold", {"move": 0.08, "max_entry": 0.55, "min_entry": 0.48})
        result = fn(pm, 0)
        assert result is None or result == "SKIP", (
            f"Should reject DOWN entry at $0.35 (below min_entry), got {result}"
        )

    def test_no_min_entry_allows_cheap_tokens(self):
        """Backward compatible: no min_entry param means no floor."""
        pm = _make_pm(price_up=0.30, price_down=0.70, move_pct=0.10)
        fn = build_check_fn("threshold", {"move": 0.08, "max_entry": 0.55})
        result = fn(pm, 0)
        assert result == "Up", f"Without min_entry, $0.30 should be accepted, got {result}"

    def test_min_entry_works_with_skew_strategy(self):
        pm = _make_pm(price_up=0.30, price_down=0.70, move_pct=0.10)
        fn = build_check_fn("skew", {"move": 0.08, "max_entry": 0.55, "min_entry": 0.48, "skew": 0.5})
        result = fn(pm, 0)
        assert result is None or result == "SKIP"


class TestTimeOfDayFilter:
    """Strategy should respect trading hour windows."""

    def test_rejects_outside_trading_hours(self):
        pm = _make_pm(price_up=0.50, price_down=0.50, move_pct=0.10)
        fn = build_check_fn("threshold", {
            "move": 0.08, "max_entry": 0.55,
            "hour_start": 18, "hour_end": 22,
        })
        result = fn(pm, 0, current_hour=12)
        assert result is None or result == "SKIP", (
            f"Should reject trade at hour 12 (window 18-22), got {result}"
        )

    def test_accepts_within_trading_hours(self):
        pm = _make_pm(price_up=0.50, price_down=0.50, move_pct=0.10)
        fn = build_check_fn("threshold", {
            "move": 0.08, "max_entry": 0.55,
            "hour_start": 18, "hour_end": 22,
        })
        result = fn(pm, 0, current_hour=19)
        assert result == "Up"

    def test_no_hour_params_allows_all_hours(self):
        """Backward compatible: no hour params means 24/7 trading."""
        pm = _make_pm(price_up=0.50, price_down=0.50, move_pct=0.10)
        fn = build_check_fn("threshold", {"move": 0.08, "max_entry": 0.55})
        result = fn(pm, 0, current_hour=3)
        assert result == "Up"

    def test_wrapping_hours(self):
        """Handle overnight windows like 22-06."""
        pm = _make_pm(price_up=0.50, price_down=0.50, move_pct=0.10)
        fn = build_check_fn("threshold", {
            "move": 0.08, "max_entry": 0.55,
            "hour_start": 22, "hour_end": 6,
        })
        assert fn(pm, 0, current_hour=23) == "Up"
        assert fn(pm, 0, current_hour=2) == "Up"
        result_mid = fn(pm, 0, current_hour=12)
        assert result_mid is None or result_mid == "SKIP"
