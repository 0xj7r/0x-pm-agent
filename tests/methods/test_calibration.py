"""Tests for autoresearch.methods.calibration.

Test strategy:
- known-correct outputs on small synthetic samples
- invariants the math has to satisfy (Murphy identity, isotonic
  monotonicity, calibrator idempotence on already-calibrated data)
- guarding against the most plausible failure modes (degenerate
  inputs, single-bin edge cases)
"""
from __future__ import annotations

import math

import numpy as np
import pytest

from autoresearch.methods.calibration import (
    brier_score,
    isotonic_calibrator,
    murphy_decomposition,
    platt_calibrator,
    reliability_diagram,
)


class TestBrierScore:
    def test_perfect_forecasts(self):
        f = np.array([1.0, 0.0, 1.0, 0.0])
        o = np.array([1.0, 0.0, 1.0, 0.0])
        assert brier_score(f, o) == 0.0

    def test_completely_wrong(self):
        f = np.array([1.0, 0.0, 1.0, 0.0])
        o = np.array([0.0, 1.0, 0.0, 1.0])
        assert brier_score(f, o) == 1.0

    def test_constant_half_forecaster(self):
        f = np.array([0.5, 0.5, 0.5, 0.5])
        o = np.array([1.0, 0.0, 1.0, 0.0])
        assert brier_score(f, o) == 0.25

    def test_shape_mismatch_rejected(self):
        with pytest.raises(ValueError):
            brier_score(np.array([0.5]), np.array([1.0, 0.0]))

    def test_invalid_forecast_range(self):
        with pytest.raises(ValueError):
            brier_score(np.array([1.5]), np.array([1.0]))

    def test_non_binary_outcomes(self):
        with pytest.raises(ValueError):
            brier_score(np.array([0.5]), np.array([0.7]))


class TestMurphyDecomposition:
    def test_identity_self_consistent(self):
        # The .brier field is computed from the identity, so it
        # must equal uncertainty - resolution + calibration exactly.
        rng = np.random.default_rng(42)
        f = rng.uniform(0, 1, size=500)
        o = (rng.uniform(0, 1, size=500) < f).astype(np.float64)
        d = murphy_decomposition(f, o, n_bins=10)
        recomputed_brier = d.uncertainty - d.resolution + d.calibration
        assert recomputed_brier == pytest.approx(d.brier, abs=1e-9)

    def test_decomposition_close_to_raw_brier(self):
        # With quantile bins where forecasts vary within bins, the
        # decomposition has discretization error bounded by the
        # within-bin variance. With 10 bins on uniform forecasts and
        # 500 samples, the gap should be small (< 0.01) but not
        # exactly zero.
        rng = np.random.default_rng(42)
        f = rng.uniform(0, 1, size=500)
        o = (rng.uniform(0, 1, size=500) < f).astype(np.float64)
        d = murphy_decomposition(f, o, n_bins=10)
        raw_brier = brier_score(f, o)
        assert abs(d.brier - raw_brier) < 0.01

    def test_identity_exact_with_constant_per_bin(self):
        # When forecasts within a bin are constant, the
        # decomposition is exactly the Brier score.
        f = np.array([0.2] * 50 + [0.8] * 50)
        o = np.array([0.0] * 40 + [1.0] * 10 + [0.0] * 10 + [1.0] * 40)
        d = murphy_decomposition(f, o, n_bins=2)
        raw_brier = brier_score(f, o)
        assert d.brier == pytest.approx(raw_brier, abs=1e-9)

    def test_constant_forecaster_has_zero_resolution(self):
        # Always predicts 0.5; outcomes are 0/1 mix
        f = np.full(100, 0.5)
        o = np.array([1.0] * 50 + [0.0] * 50)
        d = murphy_decomposition(f, o, n_bins=5)
        assert d.resolution == pytest.approx(0.0)
        # uncertainty = 0.5 * 0.5 = 0.25
        assert d.uncertainty == pytest.approx(0.25)

    def test_perfect_forecaster_has_resolution_equal_uncertainty(self):
        # Forecasts 1 when outcome is 1, 0 when outcome is 0
        f = np.array([1.0] * 50 + [0.0] * 50)
        o = np.array([1.0] * 50 + [0.0] * 50)
        d = murphy_decomposition(f, o, n_bins=2)
        assert d.brier == pytest.approx(0.0)
        assert d.resolution == pytest.approx(d.uncertainty)
        assert d.calibration == pytest.approx(0.0)


class TestReliabilityDiagram:
    def test_well_calibrated_data(self):
        # Mean forecast in each bin should match mean outcome
        # for a perfectly calibrated forecaster.
        rng = np.random.default_rng(7)
        n = 5000
        f = rng.uniform(0, 1, size=n)
        o = (rng.uniform(0, 1, size=n) < f).astype(np.float64)
        rd = reliability_diagram(f, o, n_bins=10)
        # Mean absolute error between mean_forecasts and mean_outcomes
        # should be small (< 0.05) for well-calibrated synthetic data
        mae = np.mean(np.abs(rd.mean_forecasts - rd.mean_outcomes))
        assert mae < 0.05

    def test_counts_sum_to_n(self):
        f = np.array([0.1, 0.2, 0.5, 0.7, 0.9])
        o = np.array([0.0, 0.0, 1.0, 1.0, 1.0])
        rd = reliability_diagram(f, o, n_bins=3)
        assert int(np.sum(rd.counts)) == 5


class TestIsotonicCalibrator:
    def test_already_calibrated_passthrough(self):
        # If forecasts already match outcome frequencies, isotonic
        # should be approximately the identity on the bins it sees
        rng = np.random.default_rng(13)
        n = 2000
        f = rng.uniform(0, 1, size=n)
        o = (rng.uniform(0, 1, size=n) < f).astype(np.float64)
        cal = isotonic_calibrator(f, o)
        # Predict on the same forecasts (in-sample)
        f_cal = cal(f)
        # Calibrated values should differ by less than 0.1 from raw
        # on average. (Not a tight bound because PAV smooths.)
        assert np.mean(np.abs(f_cal - f)) < 0.1

    def test_monotonic_output(self):
        # Sorted-input -> sorted-output, by construction (PAV)
        rng = np.random.default_rng(99)
        f = rng.uniform(0, 1, size=200)
        o = (rng.uniform(0, 1, size=200) < f).astype(np.float64)
        cal = isotonic_calibrator(f, o)
        x = np.linspace(0, 1, 50)
        y = cal(x)
        assert np.all(np.diff(y) >= -1e-9)  # non-decreasing

    def test_clipped_to_unit_interval(self):
        rng = np.random.default_rng(5)
        f = rng.uniform(0, 1, size=100)
        o = (rng.uniform(0, 1, size=100) < f).astype(np.float64)
        cal = isotonic_calibrator(f, o)
        y = cal(np.linspace(-0.1, 1.1, 20))
        assert np.all(y >= 0.0)
        assert np.all(y <= 1.0)


class TestPlattCalibrator:
    def test_recovers_simple_sigmoid(self):
        # Synthesize data from a known sigmoid and check Platt fits it
        rng = np.random.default_rng(31)
        n = 5000
        x_raw = rng.uniform(0, 1, size=n)
        # True calibration: q_true = sigmoid(2 * (x - 0.5)) -> S-shaped
        true_q = 1.0 / (1.0 + np.exp(-2.0 * (x_raw - 0.5)))
        o = (rng.uniform(0, 1, size=n) < true_q).astype(np.float64)
        cal = platt_calibrator(x_raw, o)
        # Check predictions on a grid - should be close to true_q
        grid = np.linspace(0.05, 0.95, 19)
        pred = cal(grid)
        true_grid = 1.0 / (1.0 + np.exp(-2.0 * (grid - 0.5)))
        mae = np.mean(np.abs(pred - true_grid))
        assert mae < 0.05

    def test_output_in_unit_interval(self):
        rng = np.random.default_rng(11)
        f = rng.uniform(0, 1, size=200)
        o = (rng.uniform(0, 1, size=200) < f).astype(np.float64)
        cal = platt_calibrator(f, o)
        y = cal(np.linspace(0, 1, 50))
        assert np.all(y >= 0.0)
        assert np.all(y <= 1.0)
