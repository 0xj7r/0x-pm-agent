"""Stationary block bootstrap with ACF-derived block length.

Per-trade returns from a 5-minute prediction-market strategy are
NOT iid: consecutive trades on the same underlying are correlated
because BTC was either rising or falling during the window in which
both trades happened. Naive iid bootstrap CIs on metrics like log
growth per trade are too tight, which leads to overconfident
acceptance of strategies whose edge is just an autocorrelation
artifact.

Politis and Romano (1994) introduced the stationary block bootstrap,
which resamples geometrically-distributed-length contiguous blocks
of the original time series. The expected block length is a single
hyperparameter that controls how much of the time-series structure
the bootstrap preserves.

The block length is selected from the autocorrelation function of
the input series: find the smallest lag k where the ACF drops below
1/sqrt(n), then use 2k as the expected block length. This is the
Politis-White (2004) automatic selection rule for the stationary
bootstrap, simplified.

The framework's spec section 5.3 declares the block length in the
preregistration, derived from the TRAIN partition's returns under
the baseline forecaster. The block length itself is not chosen
post-hoc.
"""
from __future__ import annotations

from dataclasses import dataclass

import numpy as np


def autocorrelation(x: np.ndarray, max_lag: int = 50) -> np.ndarray:
    """Compute the (sample) autocorrelation function up to max_lag.

    Returns an array of length min(max_lag, len(x) - 1) with the
    autocorrelation at lags 1, 2, ..., max_lag. Lag 0 is
    excluded because it is always 1.

    Uses the biased estimator (divide by n, not n-k) which is the
    standard convention for ACF and gives a smoother decay.
    """
    x = np.asarray(x, dtype=np.float64)
    n = len(x)
    if n < 2:
        return np.array([])
    max_lag = min(max_lag, n - 1)
    x_centered = x - np.mean(x)
    var = float(np.var(x))
    # Use a finite epsilon rather than > 0 because constant inputs
    # produce float-precision artifacts (var ~ 1e-32) where the
    # naive ratio numerator/denominator collapses to a misleading
    # 1.0 instead of 0/0 = NaN.
    if var < 1e-12:
        return np.zeros(max_lag)
    acf = np.zeros(max_lag)
    for k in range(1, max_lag + 1):
        acf[k - 1] = float(np.mean(x_centered[: n - k] * x_centered[k:])) / var
    return acf


def acf_block_length(
    x: np.ndarray,
    max_lag: int = 50,
    min_length: int = 2,
) -> int:
    """Block length for stationary bootstrap, from the ACF.

    Algorithm: compute ACF, find the smallest lag k where the ACF
    drops below the noise band 1/sqrt(n), return 2k. If the ACF
    never drops below the band within max_lag, return 2 * max_lag
    as a conservative fallback.

    Returns at least min_length (default 2). The expected block
    length parameter for the stationary bootstrap is the reciprocal
    of the geometric distribution's success probability, so passing
    this number to stationary_block_bootstrap as `block_length`
    means each block is geometrically distributed with mean
    `block_length`.
    """
    x = np.asarray(x, dtype=np.float64)
    n = len(x)
    if n < 4:
        return min_length
    acf = autocorrelation(x, max_lag=max_lag)
    if len(acf) == 0:
        return min_length
    threshold = 1.0 / np.sqrt(n)
    below = np.where(np.abs(acf) < threshold)[0]
    if len(below) == 0:
        return max(min_length, 2 * max_lag)
    k = int(below[0]) + 1  # +1 because acf array index 0 is lag 1
    return max(min_length, 2 * k)


@dataclass(frozen=True)
class BootstrapResult:
    point_estimate: float
    lower_ci: float
    upper_ci: float
    n_resamples: int
    block_length: int
    confidence_level: float


def stationary_block_bootstrap(
    x: np.ndarray,
    statistic,
    block_length: int,
    n_resamples: int = 1000,
    confidence_level: float = 0.95,
    seed: int | None = None,
) -> BootstrapResult:
    """Politis-Romano stationary block bootstrap CI for a statistic.

    Resamples blocks of geometrically-distributed lengths (mean
    block_length) from x and recomputes `statistic` on each
    resample. Returns the point estimate (statistic on the
    original sample) and the confidence interval from the
    resampled distribution.

    Parameters:
    - x: 1-D array of observations (e.g. per-trade log returns)
    - statistic: callable taking a 1-D array and returning a scalar
    - block_length: expected block length (use acf_block_length to derive)
    - n_resamples: number of bootstrap iterations
    - confidence_level: e.g. 0.95 for a 95% CI
    - seed: optional RNG seed

    Returns a BootstrapResult.
    """
    x = np.asarray(x, dtype=np.float64)
    n = len(x)
    if n < 2:
        raise ValueError(f"need at least 2 observations, got {n}")
    if block_length < 1:
        raise ValueError(f"block_length must be >= 1, got {block_length}")
    if not 0 < confidence_level < 1:
        raise ValueError(f"confidence_level must be in (0, 1), got {confidence_level}")

    rng = np.random.default_rng(seed)
    p = 1.0 / block_length  # geometric distribution success probability
    point = float(statistic(x))

    estimates = np.empty(n_resamples, dtype=np.float64)
    for i in range(n_resamples):
        # Construct a resample of length n by sampling blocks
        resample = np.empty(n, dtype=np.float64)
        pos = 0
        while pos < n:
            start = int(rng.integers(0, n))
            # Block length is at least 1, drawn from a geometric
            # distribution truncated to fit the remaining slots.
            length = 1 + int(rng.geometric(p)) - 1  # geometric(p) returns >=1
            length = min(length, n - pos)
            # Wrap around the original series for circular blocks
            for j in range(length):
                resample[pos + j] = x[(start + j) % n]
            pos += length
        estimates[i] = float(statistic(resample))

    alpha = 1 - confidence_level
    lower = float(np.quantile(estimates, alpha / 2))
    upper = float(np.quantile(estimates, 1 - alpha / 2))
    return BootstrapResult(
        point_estimate=point,
        lower_ci=lower,
        upper_ci=upper,
        n_resamples=n_resamples,
        block_length=block_length,
        confidence_level=confidence_level,
    )
