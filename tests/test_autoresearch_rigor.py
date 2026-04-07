"""Tests for autoresearch validation rigor.

The current autoresearch has these methodological flaws:
  1. Minimum test trades = 3 (way too small for any statistical claim)
  2. Sorts by PnL/trade, not risk-adjusted return (Sharpe)
  3. No statistical significance check
  4. No Sharpe ratio computation
  5. No max drawdown computation

These tests specify the correct behavior and drive the fix.
"""
from __future__ import annotations

import math
import pytest


class TestSharpeComputation:
    """Sharpe ratio = mean(returns) / std(returns)."""

    def test_sharpe_zero_for_no_variance(self):
        from autoresearch.runner import compute_sharpe
        assert compute_sharpe([1.0, 1.0, 1.0, 1.0]) == 0.0

    def test_sharpe_positive_for_positive_mean(self):
        from autoresearch.runner import compute_sharpe
        sharpe = compute_sharpe([1.0, 0.5, 1.5, 0.5, 1.5])
        assert sharpe > 0

    def test_sharpe_negative_for_negative_mean(self):
        from autoresearch.runner import compute_sharpe
        sharpe = compute_sharpe([-1.0, -0.5, -1.5, -0.5, -1.5])
        assert sharpe < 0

    def test_sharpe_empty_is_zero(self):
        from autoresearch.runner import compute_sharpe
        assert compute_sharpe([]) == 0.0

    def test_sharpe_single_value_is_zero(self):
        from autoresearch.runner import compute_sharpe
        assert compute_sharpe([0.5]) == 0.0

    def test_sharpe_greater_for_tighter_distribution(self):
        from autoresearch.runner import compute_sharpe
        tight = compute_sharpe([0.4, 0.5, 0.6, 0.5, 0.4])
        wide = compute_sharpe([-0.5, 0.5, 1.5, -0.5, 1.5])
        assert tight > wide, "Same mean, tighter distribution should have higher Sharpe"


class TestMaxDrawdownComputation:
    """Max drawdown from cumulative trade P&L."""

    def test_no_drawdown_if_monotonic(self):
        from autoresearch.runner import compute_max_drawdown
        assert compute_max_drawdown([1.0, 1.0, 1.0, 1.0]) == 0.0

    def test_drawdown_from_peak(self):
        from autoresearch.runner import compute_max_drawdown
        assert compute_max_drawdown([1.0, 1.0, -1.5, -0.5]) == 2.0

    def test_empty_is_zero(self):
        from autoresearch.runner import compute_max_drawdown
        assert compute_max_drawdown([]) == 0.0


class TestScoreTrades:
    """score_trades computes all stats for a list of trade PnLs."""

    def test_score_includes_sharpe_and_drawdown(self):
        from autoresearch.runner import score_trades
        pnls = [0.5, 0.5, -0.5, 0.5, 0.5, -0.5, 0.5, 0.5]
        s = score_trades(pnls)
        required = {"trades", "wins", "losses", "pnl", "win_rate", "sharpe", "max_drawdown", "pnl_per_trade"}
        assert required <= set(s.keys())
        assert s["trades"] == 8
        assert s["wins"] == 6
        assert s["losses"] == 2
        assert s["win_rate"] == 0.75

    def test_score_empty(self):
        from autoresearch.runner import score_trades
        s = score_trades([])
        assert s["trades"] == 0
        assert s["wins"] == 0
        assert s["pnl"] == 0.0
        assert s["sharpe"] == 0.0


class TestValidationMinimums:
    """Results with insufficient sample sizes should be filtered out."""

    def test_rejects_too_few_train_trades(self):
        from autoresearch.runner import is_result_significant
        result = {
            "train_trades": 50, "test_trades": 30, "test_sharpe": 1.5,
            "train_pnl": 5.0, "test_pnl": 3.0,
        }
        assert is_result_significant(result) is False, (
            "50 train trades is too few (need at least 100)"
        )

    def test_rejects_too_few_test_trades(self):
        from autoresearch.runner import is_result_significant
        result = {
            "train_trades": 200, "test_trades": 10, "test_sharpe": 1.5,
            "train_pnl": 5.0, "test_pnl": 3.0,
        }
        assert is_result_significant(result) is False, (
            "10 test trades is too few (need at least 30)"
        )

    def test_rejects_low_sharpe(self):
        from autoresearch.runner import is_result_significant
        result = {
            "train_trades": 200, "test_trades": 50, "test_sharpe": 0.3,
            "train_pnl": 5.0, "test_pnl": 3.0,
        }
        assert is_result_significant(result) is False, (
            "Sharpe 0.3 is too low (need at least 0.5 for 'probably profitable')"
        )

    def test_rejects_negative_train_pnl(self):
        from autoresearch.runner import is_result_significant
        result = {
            "train_trades": 200, "test_trades": 50, "test_sharpe": 1.5,
            "train_pnl": -1.0, "test_pnl": 3.0,
        }
        assert is_result_significant(result) is False

    def test_accepts_strong_config(self):
        from autoresearch.runner import is_result_significant
        result = {
            "train_trades": 200, "test_trades": 50, "test_sharpe": 1.5,
            "train_pnl": 10.0, "test_pnl": 5.0,
        }
        assert is_result_significant(result) is True


class TestResultsSortedBySharpe:
    """Best configs should be sorted by test_sharpe, not raw PnL."""

    def test_higher_sharpe_ranks_higher(self):
        from autoresearch.runner import sort_results
        results = [
            {"test_sharpe": 0.5, "test_pnl_per_trade": 0.8, "name": "noisy"},
            {"test_sharpe": 1.5, "test_pnl_per_trade": 0.4, "name": "consistent"},
            {"test_sharpe": 1.0, "test_pnl_per_trade": 0.6, "name": "middle"},
        ]
        sorted_r = sort_results(results)
        assert sorted_r[0]["name"] == "consistent"
        assert sorted_r[-1]["name"] == "noisy"
