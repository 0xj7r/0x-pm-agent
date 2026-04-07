"""Tests for autoresearch.methods.pbo."""
from __future__ import annotations

import numpy as np
import pytest

from autoresearch.methods.pbo import probability_of_backtest_overfitting


class TestPBO:
    def test_genuine_signal_low_pbo(self):
        # Construct a clear signal: candidate 0 is consistently best
        # across all folds. PBO should be very low.
        rng = np.random.default_rng(42)
        n_candidates = 20
        n_folds = 16
        metrics = rng.normal(0, 0.1, size=(n_candidates, n_folds))
        # Make candidate 0 systematically best
        metrics[0, :] += 1.0
        pbo = probability_of_backtest_overfitting(metrics, n_groups=8)
        assert pbo < 0.2  # very rarely picks an OOS-worse candidate

    def test_pure_noise_high_pbo(self):
        # Pure noise: in-sample winners should be roughly random OOS
        # so PBO should be near 0.5.
        rng = np.random.default_rng(7)
        metrics = rng.normal(0, 1.0, size=(50, 16))
        pbo = probability_of_backtest_overfitting(metrics, n_groups=8)
        assert 0.3 < pbo < 0.7

    def test_anti_signal_pbo_above_half(self):
        # Negative correlation between in-sample and OOS performance
        # PBO should be substantially > 0.5
        rng = np.random.default_rng(11)
        n_folds = 16
        n_candidates = 30
        metrics = rng.normal(0, 0.1, size=(n_candidates, n_folds))
        # Flip the sign on the second half so each candidate's
        # in-sample best becomes its OOS worst
        metrics[:, n_folds // 2 :] = -metrics[:, n_folds // 2 :]
        pbo = probability_of_backtest_overfitting(metrics, n_groups=2)
        assert pbo > 0.5

    def test_returns_value_in_unit_interval(self):
        rng = np.random.default_rng(99)
        metrics = rng.normal(0, 1, size=(10, 8))
        pbo = probability_of_backtest_overfitting(metrics, n_groups=4)
        assert 0.0 <= pbo <= 1.0

    def test_invalid_inputs(self):
        with pytest.raises(ValueError):
            probability_of_backtest_overfitting(
                np.zeros((5,)), n_groups=4
            )  # 1-D
        with pytest.raises(ValueError):
            probability_of_backtest_overfitting(
                np.zeros((1, 8)), n_groups=4
            )  # only one candidate
        with pytest.raises(ValueError):
            probability_of_backtest_overfitting(
                np.zeros((5, 8)), n_groups=3
            )  # odd n_groups
        with pytest.raises(ValueError):
            probability_of_backtest_overfitting(
                np.zeros((5, 4)), n_groups=8
            )  # n_groups > n_folds
