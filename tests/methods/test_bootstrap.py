"""Tests for autoresearch.methods.bootstrap.

Verifies the ACF estimator on data with known structure, the block
length selection rule, and the stationary block bootstrap CI on a
sample where we know the true population parameter.
"""
from __future__ import annotations

import math

import numpy as np
import pytest

from autoresearch.methods.bootstrap import (
    BootstrapResult,
    acf_block_length,
    autocorrelation,
    stationary_block_bootstrap,
)


class TestAutocorrelation:
    def test_iid_noise_acf_near_zero(self):
        rng = np.random.default_rng(42)
        x = rng.standard_normal(2000)
        acf = autocorrelation(x, max_lag=10)
        assert len(acf) == 10
        # All lags should be small, well under 0.1 in absolute value
        assert np.all(np.abs(acf) < 0.1)

    def test_ar1_acf_decays_geometrically(self):
        # AR(1) process: x_t = 0.7 * x_{t-1} + noise
        rng = np.random.default_rng(7)
        n = 5000
        phi = 0.7
        x = np.zeros(n)
        x[0] = rng.standard_normal()
        for t in range(1, n):
            x[t] = phi * x[t - 1] + rng.standard_normal()
        acf = autocorrelation(x, max_lag=5)
        # Theoretical ACF for AR(1) is phi^k
        expected = np.array([phi ** k for k in range(1, 6)])
        assert np.allclose(acf, expected, atol=0.05)

    def test_constant_input_returns_zeros(self):
        x = np.full(100, 3.14)
        acf = autocorrelation(x, max_lag=10)
        assert np.allclose(acf, 0.0)

    def test_short_input(self):
        assert len(autocorrelation(np.array([1.0]))) == 0


class TestAcfBlockLength:
    def test_iid_returns_small_block(self):
        rng = np.random.default_rng(42)
        x = rng.standard_normal(2000)
        bl = acf_block_length(x, max_lag=20)
        # IID data: ACF drops below 1/sqrt(n) ~= 0.022 immediately
        # Block length should be 2 * 1 = 2
        assert bl <= 6

    def test_strongly_correlated_returns_larger_block(self):
        # AR(1) with phi=0.9: ACF decays slowly, block should be larger
        rng = np.random.default_rng(7)
        n = 2000
        phi = 0.9
        x = np.zeros(n)
        for t in range(1, n):
            x[t] = phi * x[t - 1] + rng.standard_normal()
        bl_strong = acf_block_length(x, max_lag=50)
        # Compare to iid case
        x_iid = rng.standard_normal(n)
        bl_iid = acf_block_length(x_iid, max_lag=50)
        assert bl_strong > bl_iid

    def test_minimum_length_enforced(self):
        x = np.array([1.0, 2.0, 3.0])
        bl = acf_block_length(x, min_length=4)
        assert bl >= 4


class TestStationaryBlockBootstrap:
    def test_iid_mean_ci_contains_truth(self):
        # IID mean should be unbiased and the CI should usually contain
        # the true mean (which is 0 for standard normal).
        rng = np.random.default_rng(42)
        x = rng.standard_normal(500)
        result = stationary_block_bootstrap(
            x, np.mean, block_length=2, n_resamples=500, seed=1
        )
        assert isinstance(result, BootstrapResult)
        # The point estimate should be close to 0 (sample mean of 500 normals)
        assert abs(result.point_estimate) < 0.2
        # The CI should contain 0
        assert result.lower_ci < 0 < result.upper_ci

    def test_ar1_ci_wider_than_iid_for_same_n(self):
        # The whole point of block bootstrap: with autocorrelated data,
        # the CI on the mean should be wider than naive iid would suggest.
        rng = np.random.default_rng(7)
        n = 500
        # AR(1) data
        phi = 0.8
        x_ar = np.zeros(n)
        for t in range(1, n):
            x_ar[t] = phi * x_ar[t - 1] + rng.standard_normal()
        # IID data with the same marginal variance
        var_ar = float(np.var(x_ar))
        x_iid = rng.standard_normal(n) * math.sqrt(var_ar)

        # Use a sensibly large block for AR data, length 1 for iid
        ar_block = acf_block_length(x_ar)
        ar_result = stationary_block_bootstrap(
            x_ar, np.mean, block_length=ar_block, n_resamples=500, seed=2
        )
        iid_result = stationary_block_bootstrap(
            x_iid, np.mean, block_length=2, n_resamples=500, seed=3
        )
        ar_width = ar_result.upper_ci - ar_result.lower_ci
        iid_width = iid_result.upper_ci - iid_result.lower_ci
        # AR data needs a wider CI to honestly capture uncertainty
        assert ar_width > iid_width

    def test_invalid_inputs(self):
        with pytest.raises(ValueError):
            stationary_block_bootstrap(np.array([1.0]), np.mean, block_length=2)
        with pytest.raises(ValueError):
            stationary_block_bootstrap(
                np.array([1.0, 2.0]), np.mean, block_length=0
            )
        with pytest.raises(ValueError):
            stationary_block_bootstrap(
                np.array([1.0, 2.0]),
                np.mean,
                block_length=2,
                confidence_level=1.5,
            )

    def test_returns_record_fields(self):
        x = np.array([1.0, 2.0, 3.0, 4.0, 5.0])
        result = stationary_block_bootstrap(
            x, np.mean, block_length=2, n_resamples=100, seed=11
        )
        assert result.n_resamples == 100
        assert result.block_length == 2
        assert result.confidence_level == 0.95
        assert result.lower_ci <= result.point_estimate <= result.upper_ci
