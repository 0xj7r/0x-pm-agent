"""Calibration metrics and recalibration for probability forecasters.

The framework treats calibration as a *diagnostic*, not a promotion
gate (see spec section 5.4). The reason: a forecaster perfectly
calibrated to P(YES wins) can have zero monetisable edge because
the market price embeds the same information. Calibration tells us
whether the forecaster's bins mean what they say; it does not tell
us whether the forecaster has alpha.

Two reasons calibration still matters:

1. Sizing. Kelly sizing on a miscalibrated q is a faster way to go
   broke. We recalibrate (isotonic or Platt) before plugging q into
   the sizing pipeline.
2. Slice-conditional miscalibration. A forecaster calibrated in
   aggregate but miscalibrated in some regime (low liquidity,
   high vol, certain hours) is one slice away from a live blow-up.
   The Red Team check (spec section 5.5) is built on top of these
   primitives.

What this module exposes:

- brier_score(forecasts, outcomes)
- murphy_decomposition(forecasts, outcomes, n_bins=10)
- reliability_diagram(forecasts, outcomes, n_bins=10) returning
  bin centers, mean forecast per bin, mean outcome per bin, and
  count per bin
- isotonic_calibrator(forecasts, outcomes) returning a callable
- platt_calibrator(forecasts, outcomes) returning a callable

The recalibrators learn on TRAIN. The Researcher applies them to CV
and re-evaluates. They are NEVER fit on CV or HOLDOUT.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Callable

import numpy as np


def _validate_forecasts_outcomes(
    forecasts: np.ndarray, outcomes: np.ndarray
) -> tuple[np.ndarray, np.ndarray]:
    f = np.asarray(forecasts, dtype=np.float64)
    o = np.asarray(outcomes, dtype=np.float64)
    if f.shape != o.shape:
        raise ValueError(f"shape mismatch: forecasts {f.shape} vs outcomes {o.shape}")
    if f.ndim != 1:
        raise ValueError(f"forecasts must be 1-D, got {f.ndim}-D")
    if len(f) == 0:
        raise ValueError("forecasts is empty")
    if np.any((f < 0) | (f > 1)):
        raise ValueError("forecasts must be in [0, 1]")
    if not np.all((o == 0) | (o == 1)):
        raise ValueError("outcomes must be binary 0/1")
    return f, o


def brier_score(forecasts: np.ndarray, outcomes: np.ndarray) -> float:
    """Mean squared error between forecast probability and binary outcome.

    Lower is better. The minimum is 0 (perfect forecasts). The
    score for an always-0.5 forecaster is 0.25, which is the
    naive baseline. The score for an always-base-rate forecaster
    is the variance of the outcomes (the "uncertainty" component
    in the Murphy decomposition).
    """
    f, o = _validate_forecasts_outcomes(forecasts, outcomes)
    return float(np.mean((f - o) ** 2))


@dataclass(frozen=True)
class MurphyDecomposition:
    """Brier score split into the three Murphy components.

    Identity (exact only when forecasts are constant within bins):

        brier ≈ uncertainty - resolution + calibration

    where:
    - uncertainty is the irreducible variance of the outcome
      distribution. It depends only on the base rate, not on the
      forecaster.
    - resolution is how much the forecaster's bin means deviate
      from the overall base rate. Higher resolution = the
      forecaster is making meaningfully different predictions for
      different inputs (good).
    - calibration is the weighted MSE between bin means and bin
      observed frequencies. Lower calibration component = the
      forecaster's stated probabilities match observed
      frequencies (good).

    A useless forecaster (always predicts the base rate) has
    resolution=0, calibration=0, brier=uncertainty. A perfect
    forecaster has resolution=uncertainty, calibration=0,
    brier=0.

    The .brier field on this dataclass is the RECONSTRUCTED brier
    from the identity, not the raw Brier score. With quantile bins
    where forecasts vary within bins, the reconstructed brier has
    a small positive bias relative to the raw brier; this is the
    discretization error and is bounded by the within-bin variance
    of the forecasts. For an exact raw Brier score, call
    brier_score() directly.
    """
    uncertainty: float
    resolution: float
    calibration: float
    brier: float
    n_bins: int


def murphy_decomposition(
    forecasts: np.ndarray,
    outcomes: np.ndarray,
    n_bins: int = 10,
) -> MurphyDecomposition:
    """Decompose Brier into uncertainty - resolution + calibration.

    Bins are quantile-based on the forecast distribution so each bin
    has roughly equal sample size, which is more robust on skewed
    forecasts than equal-width bins.
    """
    f, o = _validate_forecasts_outcomes(forecasts, outcomes)
    n = len(f)
    base_rate = float(np.mean(o))
    uncertainty = base_rate * (1 - base_rate)

    # Quantile-based bins. If many forecasts are equal (e.g. degenerate
    # forecasters), np.quantile may produce repeated edges; np.digitize
    # handles this and bins collapse appropriately.
    if n_bins >= n:
        n_bins = max(1, n // 5)
    quantiles = np.linspace(0, 1, n_bins + 1)
    edges = np.quantile(f, quantiles)
    edges[0] = -np.inf
    edges[-1] = np.inf
    bin_idx = np.digitize(f, edges[1:-1])

    resolution = 0.0
    calibration = 0.0
    actual_bins = 0
    for b in range(n_bins):
        mask = bin_idx == b
        nb = int(np.sum(mask))
        if nb == 0:
            continue
        actual_bins += 1
        f_bar = float(np.mean(f[mask]))
        o_bar = float(np.mean(o[mask]))
        weight = nb / n
        resolution += weight * (o_bar - base_rate) ** 2
        calibration += weight * (f_bar - o_bar) ** 2

    brier = uncertainty - resolution + calibration
    return MurphyDecomposition(
        uncertainty=uncertainty,
        resolution=resolution,
        calibration=calibration,
        brier=brier,
        n_bins=actual_bins,
    )


@dataclass(frozen=True)
class ReliabilityDiagram:
    """Reliability diagram bin contents.

    Each entry is one quantile bin. The plot of mean_outcome vs
    mean_forecast should sit on the diagonal y=x for a calibrated
    forecaster.
    """
    mean_forecasts: np.ndarray
    mean_outcomes: np.ndarray
    counts: np.ndarray


def reliability_diagram(
    forecasts: np.ndarray,
    outcomes: np.ndarray,
    n_bins: int = 10,
) -> ReliabilityDiagram:
    """Quantile-bucketed reliability diagram.

    Returns three same-length arrays: per-bin mean forecast, per-bin
    mean outcome, per-bin count. Bins with zero count are dropped.
    """
    f, o = _validate_forecasts_outcomes(forecasts, outcomes)
    n = len(f)
    if n_bins >= n:
        n_bins = max(1, n // 5)
    quantiles = np.linspace(0, 1, n_bins + 1)
    edges = np.quantile(f, quantiles)
    edges[0] = -np.inf
    edges[-1] = np.inf
    bin_idx = np.digitize(f, edges[1:-1])

    mean_f, mean_o, cnt = [], [], []
    for b in range(n_bins):
        mask = bin_idx == b
        nb = int(np.sum(mask))
        if nb == 0:
            continue
        mean_f.append(float(np.mean(f[mask])))
        mean_o.append(float(np.mean(o[mask])))
        cnt.append(nb)
    return ReliabilityDiagram(
        mean_forecasts=np.array(mean_f),
        mean_outcomes=np.array(mean_o),
        counts=np.array(cnt, dtype=np.int64),
    )


def isotonic_calibrator(
    forecasts: np.ndarray,
    outcomes: np.ndarray,
) -> Callable[[np.ndarray], np.ndarray]:
    """Fit an isotonic regression mapping raw forecast -> calibrated probability.

    Isotonic regression finds the monotone non-decreasing function
    that minimizes squared error between mapped forecasts and
    outcomes. It is a non-parametric recalibrator that does not
    assume a sigmoid shape, which is the right choice when the
    forecaster is already a probability (rather than a margin).
    The downside is that it can overfit on small samples.

    Returns a callable that takes a numpy array of new forecasts and
    returns the calibrated values. The calibrator is FIT ON TRAIN
    only; the Researcher must not call this on CV or HOLDOUT data.
    """
    f, o = _validate_forecasts_outcomes(forecasts, outcomes)

    # Sort by forecast and run pool-adjacent-violators
    order = np.argsort(f, kind="stable")
    fs = f[order]
    os_ = o[order]

    # PAV algorithm: walk forward, merging bins that violate monotonicity
    weights = np.ones_like(os_, dtype=np.float64)
    values = os_.astype(np.float64).copy()
    n = len(values)
    indices = list(range(n))
    i = 0
    while i < n - 1:
        if values[i] > values[i + 1]:
            # Merge i and i+1
            total_weight = weights[i] + weights[i + 1]
            merged = (values[i] * weights[i] + values[i + 1] * weights[i + 1]) / total_weight
            values[i] = merged
            weights[i] = total_weight
            # Remove i+1 by shifting
            values = np.concatenate([values[: i + 1], values[i + 2 :]])
            weights = np.concatenate([weights[: i + 1], weights[i + 2 :]])
            fs = np.concatenate([fs[: i + 1], fs[i + 2 :]])
            n -= 1
            # Step back to recheck monotonicity with the merged bin
            if i > 0:
                i -= 1
        else:
            i += 1

    # fs and values are now the calibrator's knots: at forecast value
    # fs[k], the calibrated output is values[k]. For a new forecast x,
    # we find the rightmost knot with fs[k] <= x and return values[k].
    knots_x = fs.copy()
    knots_y = np.clip(values, 0.0, 1.0)

    def predict(x: np.ndarray) -> np.ndarray:
        x = np.asarray(x, dtype=np.float64)
        idx = np.searchsorted(knots_x, x, side="right") - 1
        idx = np.clip(idx, 0, len(knots_y) - 1)
        return knots_y[idx]

    return predict


def platt_calibrator(
    forecasts: np.ndarray,
    outcomes: np.ndarray,
    max_iter: int = 100,
) -> Callable[[np.ndarray], np.ndarray]:
    """Fit a Platt-scaling sigmoid recalibrator.

    Platt scaling fits a logistic function:

        q_calibrated = 1 / (1 + exp(A * f + B))

    by maximum likelihood on (forecasts, outcomes). It is a
    parametric two-parameter recalibrator and is more sample-
    efficient than isotonic on small datasets, but it imposes a
    sigmoid shape that may not match reality.

    Use Platt for high-capacity forecasters (e.g. gradient-boosted
    trees) where output scores are not naturally probabilities.
    Use isotonic for forecasters that already output probabilities
    (e.g. logistic regression) and you have enough sample size.
    """
    f, o = _validate_forecasts_outcomes(forecasts, outcomes)
    # Use logit-of-forecast as input to a logistic, fit by Newton's
    # method on the log-likelihood. To avoid log(0)/log(1), clip f.
    eps = 1e-6
    fc = np.clip(f, eps, 1 - eps)
    x = np.log(fc / (1 - fc))

    # Targets are in {0, 1}. Use the standard logistic regression
    # loss with two parameters: A * x + B.
    A = 1.0
    B = 0.0
    for _ in range(max_iter):
        z = A * x + B
        p = 1.0 / (1.0 + np.exp(-z))
        # Gradient
        residual = p - o
        gA = float(np.sum(residual * x))
        gB = float(np.sum(residual))
        # Hessian
        w = p * (1 - p) + 1e-9
        hAA = float(np.sum(w * x * x))
        hAB = float(np.sum(w * x))
        hBB = float(np.sum(w))
        det = hAA * hBB - hAB * hAB
        if abs(det) < 1e-12:
            break
        dA = (hBB * gA - hAB * gB) / det
        dB = (-hAB * gA + hAA * gB) / det
        A -= dA
        B -= dB
        if abs(dA) < 1e-9 and abs(dB) < 1e-9:
            break

    A_final = A
    B_final = B

    def predict(new_forecasts: np.ndarray) -> np.ndarray:
        nf = np.clip(np.asarray(new_forecasts, dtype=np.float64), eps, 1 - eps)
        nx = np.log(nf / (1 - nf))
        nz = A_final * nx + B_final
        return 1.0 / (1.0 + np.exp(-nz))

    return predict
