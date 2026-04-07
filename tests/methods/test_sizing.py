"""Tests for autoresearch.methods.sizing.

Verifies the shrinkage and Kelly math against known correct outputs
and the policy bounds (max bet, kelly fraction range, shrink range).
"""
from __future__ import annotations

import math

import pytest

from autoresearch.methods.sizing import (
    SizingPolicy,
    kelly_fraction_for_yes,
    shrink,
    stake_fraction,
    stake_fraction_no,
)


class TestShrink:
    def test_alpha_one_returns_input_unchanged(self):
        assert shrink(0.7, 1.0) == pytest.approx(0.7)
        assert shrink(0.3, 1.0) == pytest.approx(0.3)

    def test_alpha_zero_returns_half(self):
        assert shrink(0.7, 0.0) == 0.5
        assert shrink(0.3, 0.0) == 0.5

    def test_half_alpha_pulls_halfway_to_half(self):
        assert shrink(0.7, 0.5) == pytest.approx(0.6)
        assert shrink(0.3, 0.5) == pytest.approx(0.4)

    def test_input_at_half_unchanged(self):
        assert shrink(0.5, 0.7) == 0.5


class TestKellyFractionForYes:
    def test_no_edge_returns_zero(self):
        assert kelly_fraction_for_yes(q=0.5, p_ask=0.5) == 0.0
        assert kelly_fraction_for_yes(q=0.4, p_ask=0.5) == 0.0

    def test_positive_edge_matches_formula(self):
        # f* = (q - p) / (1 - p) = (0.6 - 0.5) / (1 - 0.5) = 0.2
        assert kelly_fraction_for_yes(0.6, 0.5) == pytest.approx(0.2)
        # (0.7 - 0.4) / (1 - 0.4) = 0.5
        assert kelly_fraction_for_yes(0.7, 0.4) == pytest.approx(0.5)

    def test_degenerate_prices_return_zero(self):
        assert kelly_fraction_for_yes(0.9, 0.0) == 0.0
        assert kelly_fraction_for_yes(0.9, 1.0) == 0.0
        assert kelly_fraction_for_yes(0.9, -0.1) == 0.0


class TestStakeFraction:
    def test_no_edge_after_shrinkage(self):
        # Shrunk q = 0.5 + 0.5 * (0.55 - 0.5) = 0.525
        # Edge at p_ask=0.5 = 0.025; full kelly = 0.025 / 0.5 = 0.05
        # Quarter-kelly = 0.0125; below max_bet 0.05 so returns 0.0125
        policy = SizingPolicy(kelly_fraction=0.25, shrink_alpha=0.5, max_bet_fraction=0.05)
        assert stake_fraction(0.55, 0.5, policy) == pytest.approx(0.0125)

    def test_capped_at_max_bet_fraction(self):
        # q_hat 0.95 vs p_ask 0.5: shrunk at alpha 1.0 -> q=0.95
        # full kelly = (0.95 - 0.5)/0.5 = 0.9; quarter = 0.225
        # capped at 0.05
        policy = SizingPolicy(kelly_fraction=0.25, shrink_alpha=1.0, max_bet_fraction=0.05)
        assert stake_fraction(0.95, 0.5, policy) == pytest.approx(0.05)

    def test_negative_edge_returns_zero(self):
        # q_hat 0.45 < p_ask 0.5; even without shrinkage, no bet
        policy = SizingPolicy(shrink_alpha=1.0)
        assert stake_fraction(0.45, 0.5, policy) == 0.0

    def test_exact_edge_returns_zero(self):
        policy = SizingPolicy(shrink_alpha=1.0)
        assert stake_fraction(0.5, 0.5, policy) == 0.0

    def test_shrinkage_kills_marginal_edge(self):
        # q_hat 0.51, p_ask 0.5: tiny edge before shrinkage.
        # shrunk q = 0.5 + 0.5*(0.51-0.5) = 0.505
        # full kelly = 0.005/0.5 = 0.01; quarter = 0.0025
        policy = SizingPolicy(kelly_fraction=0.25, shrink_alpha=0.5, max_bet_fraction=0.05)
        assert stake_fraction(0.51, 0.5, policy) == pytest.approx(0.0025)


class TestStakeFractionNo:
    def test_no_side_symmetry(self):
        # Believing P(YES)=0.4 implies P(NO)=0.6.
        # NO bet at p_ask_no=0.5: edge = 0.6 - 0.5 = 0.1
        # full kelly = 0.1/0.5 = 0.2; quarter = 0.05; cap = 0.05
        policy = SizingPolicy(kelly_fraction=0.25, shrink_alpha=1.0, max_bet_fraction=0.05)
        assert stake_fraction_no(0.4, 0.5, policy) == pytest.approx(0.05)


class TestSizingPolicyValidation:
    def test_invalid_kelly_fraction_rejected(self):
        with pytest.raises(ValueError):
            SizingPolicy(kelly_fraction=0.0)
        with pytest.raises(ValueError):
            SizingPolicy(kelly_fraction=1.5)

    def test_invalid_shrink_alpha_rejected(self):
        with pytest.raises(ValueError):
            SizingPolicy(shrink_alpha=-0.1)
        with pytest.raises(ValueError):
            SizingPolicy(shrink_alpha=1.5)

    def test_invalid_max_bet_rejected(self):
        with pytest.raises(ValueError):
            SizingPolicy(max_bet_fraction=0.0)
        with pytest.raises(ValueError):
            SizingPolicy(max_bet_fraction=1.5)
