"""Tests for autoresearch.methods.edge."""
from __future__ import annotations

import math

import pytest

from autoresearch.methods.edge import (
    EdgePolicy,
    break_even_price,
    edge,
    market_implied_q,
    min_edge_required,
)


class TestBreakEven:
    def test_no_fee(self):
        assert break_even_price(0.5, 0.0) == 0.5

    def test_with_fee(self):
        assert break_even_price(0.5, 0.01) == pytest.approx(0.51)


class TestEdge:
    def test_positive_edge_above_break_even(self):
        # q=0.6, p_ask=0.5, fee=0.01 -> break_even=0.51, edge=0.09
        assert edge(0.6, 0.5, 0.01) == pytest.approx(0.09)

    def test_zero_edge_at_break_even(self):
        assert edge(0.51, 0.5, 0.01) == pytest.approx(0.0)

    def test_negative_edge_below_break_even(self):
        assert edge(0.5, 0.5, 0.01) == pytest.approx(-0.01)


class TestMinEdgeRequired:
    def test_spread_dominates_when_uncertainty_low(self):
        # 0.5 * 0.04 = 0.02; 2 * 0.005 = 0.01; max = 0.02
        assert min_edge_required(0.04, 0.005) == pytest.approx(0.02)

    def test_uncertainty_dominates_when_spread_low(self):
        # 0.5 * 0.01 = 0.005; 2 * 0.02 = 0.04; max = 0.04
        assert min_edge_required(0.01, 0.02) == pytest.approx(0.04)

    def test_zero_inputs_zero_required(self):
        assert min_edge_required(0.0, 0.0) == 0.0

    def test_negative_inputs_rejected(self):
        with pytest.raises(ValueError):
            min_edge_required(-0.01, 0.0)
        with pytest.raises(ValueError):
            min_edge_required(0.0, -0.01)

    def test_custom_policy_multipliers(self):
        # spread_mult=1.0, sigma_mult=3.0
        # 1.0 * 0.04 = 0.04; 3.0 * 0.005 = 0.015; max = 0.04
        policy = EdgePolicy(spread_multiplier=1.0, sigma_multiplier=3.0)
        assert min_edge_required(0.04, 0.005, policy) == pytest.approx(0.04)


class TestMarketImpliedQ:
    def test_balanced_book_returns_half(self):
        # YES + NO = 1, so each side at 0.5
        assert market_implied_q(0.5, 0.5) == pytest.approx(0.5)

    def test_yes_favored(self):
        # p_yes=0.7, p_no=0.31 -> 0.7/(0.7+0.31) ~= 0.693
        assert market_implied_q(0.7, 0.31) == pytest.approx(0.7 / (0.7 + 0.31))

    def test_no_favored(self):
        assert market_implied_q(0.3, 0.71) == pytest.approx(0.3 / (0.3 + 0.71))

    def test_with_spread(self):
        # YES asked at 0.51, NO asked at 0.51 (1 cent spread on each side)
        # q_market = 0.51 / (0.51 + 0.51) = 0.5
        assert market_implied_q(0.51, 0.51) == pytest.approx(0.5)

    def test_degenerate_inputs(self):
        assert market_implied_q(0.0, 0.0) == 0.5
        assert market_implied_q(0.0, 0.5) == 0.0
        assert market_implied_q(0.5, 0.0) == 1.0
