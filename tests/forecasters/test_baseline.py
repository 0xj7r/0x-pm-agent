"""Tests for autoresearch.forecasters.baseline.MarketImpliedBaseline."""
from __future__ import annotations

import math

import numpy as np
import pytest

from autoresearch.forecasters.baseline import MarketImpliedBaseline
from autoresearch.methods.simulator import MarketSlice


def make_slice(ask_up: float, ask_down: float, n: int = 10) -> MarketSlice:
    return MarketSlice(
        market_id="test",
        winner="Up",
        best_ask_up=np.full(n, ask_up, dtype=np.float64),
        best_ask_down=np.full(n, ask_down, dtype=np.float64),
        best_bid_up=np.full(n, ask_up - 0.01, dtype=np.float64),
        best_bid_down=np.full(n, ask_down - 0.01, dtype=np.float64),
        features={},
    )


class TestMarketImpliedBaseline:
    def test_balanced_book_returns_half(self):
        b = MarketImpliedBaseline()
        s = make_slice(0.5, 0.5)
        assert b(s, 0) == pytest.approx(0.5)

    def test_yes_favored(self):
        b = MarketImpliedBaseline()
        s = make_slice(0.7, 0.31)
        assert b(s, 0) == pytest.approx(0.7 / (0.7 + 0.31))

    def test_no_favored(self):
        b = MarketImpliedBaseline()
        s = make_slice(0.3, 0.71)
        assert b(s, 0) == pytest.approx(0.3 / (0.3 + 0.71))

    def test_with_spread_normalises_correctly(self):
        # YES asked at 0.51 and NO asked at 0.51 → 1¢ spread on each side
        b = MarketImpliedBaseline()
        s = make_slice(0.51, 0.51)
        assert b(s, 0) == pytest.approx(0.5)

    def test_nan_book_returns_half(self):
        b = MarketImpliedBaseline()
        s = MarketSlice(
            market_id="t",
            winner="Up",
            best_ask_up=np.array([np.nan, 0.5]),
            best_ask_down=np.array([np.nan, 0.5]),
            best_bid_up=np.array([np.nan, 0.5]),
            best_bid_down=np.array([np.nan, 0.5]),
            features={},
        )
        assert b(s, 0) == 0.5
        assert b(s, 1) == 0.5

    def test_degenerate_inputs(self):
        b = MarketImpliedBaseline()
        # Both zero
        assert b(make_slice(0.0, 0.0), 0) == 0.5
        # YES zero, NO positive
        assert b(make_slice(0.0, 0.5), 0) == 0.0
        # YES positive, NO zero
        assert b(make_slice(0.5, 0.0), 0) == 1.0

    def test_callable_at_multiple_indices(self):
        b = MarketImpliedBaseline()
        # Build a slice where ask_up varies across snapshots
        n = 5
        s = MarketSlice(
            market_id="t",
            winner="Up",
            best_ask_up=np.array([0.3, 0.4, 0.5, 0.6, 0.7]),
            best_ask_down=np.array([0.71, 0.61, 0.51, 0.41, 0.31]),
            best_bid_up=np.full(n, 0.0),
            best_bid_down=np.full(n, 0.0),
            features={},
        )
        # Each index should be normalised independently
        for i in range(n):
            expected = s.best_ask_up[i] / (s.best_ask_up[i] + s.best_ask_down[i])
            assert b(s, i) == pytest.approx(expected)
