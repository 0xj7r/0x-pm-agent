"""Probability of Backtest Overfitting (Bailey, Borwein, Lopez de Prado, Zhu).

When you search a grid of K candidate strategies and pick the one
with the best in-sample metric, the chance that the picked candidate
is actually the best out-of-sample is much less than you naively
think. PBO is the probability that the in-sample best candidate is
worse than the median candidate out-of-sample, computed by
combinatorially symmetric cross-validation:

1. Split the data into S groups (S even, default 16).
2. For each combination of S/2 groups, use those as in-sample and
   the rest as out-of-sample.
3. For each combination, rank all K candidates on the in-sample
   set, find the in-sample winner, and compute its out-of-sample
   rank.
4. PBO is the fraction of combinations where the in-sample winner
   has a below-median out-of-sample rank (i.e. is in the worse half).

PBO close to 0 = the search is finding signal. PBO close to 0.5 = the
search is producing winners that are no better than random when
evaluated on truly out-of-sample data. The framework's spec section
5.6 rejects any grid with PBO > 0.4 — promotion is blocked, no
holdout is touched, the run is logged as a PBO failure.

Implementation note: this function takes the per-candidate per-fold
metrics as input (a 2D array of shape [n_candidates, n_folds]),
not the original data. Computing the metrics is the caller's job.
The function only does the rank-and-count.
"""
from __future__ import annotations

from itertools import combinations

import numpy as np


def probability_of_backtest_overfitting(
    metrics: np.ndarray,
    n_groups: int = 16,
) -> float:
    """Compute PBO from a per-candidate per-fold metric matrix.

    Parameters:
    - metrics: shape [n_candidates, n_folds] where higher is better
    - n_groups: number of CSCV groups to split the folds into; must
      be even and <= n_folds

    Returns a float in [0, 1] representing the probability of
    backtest overfitting. By convention, ties are broken in favor
    of the smaller index (np.argmax behavior).
    """
    metrics = np.asarray(metrics, dtype=np.float64)
    if metrics.ndim != 2:
        raise ValueError(f"metrics must be 2-D, got {metrics.ndim}-D")
    n_candidates, n_folds = metrics.shape
    if n_candidates < 2:
        raise ValueError(f"need at least 2 candidates, got {n_candidates}")
    if n_groups % 2 != 0:
        raise ValueError(f"n_groups must be even, got {n_groups}")
    if n_groups > n_folds:
        raise ValueError(
            f"n_groups ({n_groups}) cannot exceed n_folds ({n_folds})"
        )

    # Partition fold indices into n_groups roughly equal groups
    fold_indices = np.array_split(np.arange(n_folds), n_groups)
    half = n_groups // 2

    overfit_count = 0
    total = 0
    for in_sample_groups in combinations(range(n_groups), half):
        in_sample_set = set(in_sample_groups)
        is_folds = np.concatenate(
            [fold_indices[g] for g in in_sample_groups]
        )
        oos_folds = np.concatenate(
            [fold_indices[g] for g in range(n_groups) if g not in in_sample_set]
        )

        # In-sample mean per candidate
        is_means = np.mean(metrics[:, is_folds], axis=1)
        # Out-of-sample mean per candidate
        oos_means = np.mean(metrics[:, oos_folds], axis=1)

        is_winner = int(np.argmax(is_means))
        # Compute the OOS rank of the in-sample winner. Higher is better,
        # so rank from 1 (worst) to n_candidates (best).
        oos_ranks = np.argsort(np.argsort(oos_means)) + 1  # ranks 1..n
        winner_oos_rank = int(oos_ranks[is_winner])

        # Below-median rank means below the upper half
        median_rank = (n_candidates + 1) / 2
        if winner_oos_rank < median_rank:
            overfit_count += 1
        total += 1

    return overfit_count / total if total else 0.0
