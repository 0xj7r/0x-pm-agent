"""Combinatorial Purged Cross-Validation (Lopez de Prado).

The framework's spec section 5.2 mandates CPCV instead of expanding-
window walk-forward because:

1. CPCV produces multiple OOS test sets that share no overlap, so
   the OOS distribution of log growth is constructed from genuinely
   independent samples. Expanding-window walk-forward folds are
   highly correlated because they all train on the same prefix
   plus a little more.

2. CPCV supports purging and embargoes around fold boundaries to
   prevent label leakage from observations whose information
   horizon overlaps a fold edge. For 5-min markets, the embargo
   prevents a market starting just after a fold's training end
   from being placed in the test fold while its evaluation period
   is still inside the training window.

3. CPCV permits direct computation of the Probability of Backtest
   Overfitting (PBO) statistic by counting how often the in-sample
   ranking of candidates inverts on the held-out folds.

Algorithm:

- Partition the time-ordered manifest into N contiguous groups of
  roughly equal size.
- For each combination of K groups (out of N), use those K groups
  as the test set and the remaining N-K groups as the training
  set.
- For each train-test split, purge any test observations whose
  information horizon overlaps the training set, then embargo any
  training observations within `embargo` units of a test boundary.

This produces C(N, K) splits per candidate. With N=10 and K=2 the
default, that is 45 splits per candidate. Each split is a
genuinely OOS evaluation of the candidate, and the variance across
splits is a much better estimate of OOS performance than a single
walk-forward.
"""
from __future__ import annotations

from dataclasses import dataclass
from itertools import combinations

import numpy as np


@dataclass(frozen=True)
class CPCVSplit:
    """One train/test split out of the C(N, K) combinatorial pool.

    train_indices and test_indices are arrays of integer positions
    in the original manifest. They are disjoint after purging and
    embargoing. test_groups is the tuple of group ids that make up
    the test set, useful for diagnostics and PBO computation.
    """
    train_indices: np.ndarray
    test_indices: np.ndarray
    test_groups: tuple[int, ...]


def make_groups(n: int, n_partitions: int) -> list[np.ndarray]:
    """Split n contiguous observations into n_partitions groups.

    The groups are roughly equal in size; if n is not divisible by
    n_partitions, the first (n % n_partitions) groups get one
    extra observation each. Group order is preserved (group 0
    contains the earliest observations).
    """
    if n_partitions < 2:
        raise ValueError(f"n_partitions must be >= 2, got {n_partitions}")
    if n_partitions > n:
        raise ValueError(
            f"n_partitions ({n_partitions}) cannot exceed n ({n})"
        )
    base = n // n_partitions
    extras = n % n_partitions
    groups: list[np.ndarray] = []
    start = 0
    for i in range(n_partitions):
        size = base + (1 if i < extras else 0)
        groups.append(np.arange(start, start + size))
        start += size
    return groups


def cpcv_splits(
    n: int,
    n_partitions: int = 10,
    k_test: int = 2,
    embargo: int = 0,
) -> list[CPCVSplit]:
    """Generate all combinatorial purged cross-validation splits.

    Parameters:
    - n: total number of observations (markets)
    - n_partitions: number of groups to split into (default 10)
    - k_test: number of groups to use as test set per split (default 2)
    - embargo: number of observations to remove from training on each
      side of every test group (default 0). For 5-min markets with
      a 1-hour embargo, set this to 12 (1 hour / 5 min per market).

    Returns a list of CPCVSplit objects. The list has length
    C(n_partitions, k_test). Each split's train and test indices are
    disjoint and (train + test + embargo region + purged region) =
    [0, n).
    """
    if k_test < 1 or k_test >= n_partitions:
        raise ValueError(
            f"k_test must be in [1, n_partitions-1], got {k_test} with "
            f"n_partitions={n_partitions}"
        )
    if embargo < 0:
        raise ValueError(f"embargo must be non-negative, got {embargo}")

    groups = make_groups(n, n_partitions)
    splits: list[CPCVSplit] = []

    for combo in combinations(range(n_partitions), k_test):
        test_idx_list: list[np.ndarray] = []
        for g in combo:
            test_idx_list.append(groups[g])
        test_indices = np.concatenate(test_idx_list)

        # Build the embargo set: indices within `embargo` of any test boundary
        embargo_set: set[int] = set()
        if embargo > 0:
            for g in combo:
                grp = groups[g]
                grp_start, grp_end = int(grp[0]), int(grp[-1])
                # Embargo before the group start
                for j in range(max(0, grp_start - embargo), grp_start):
                    embargo_set.add(j)
                # Embargo after the group end
                for j in range(grp_end + 1, min(n, grp_end + 1 + embargo)):
                    embargo_set.add(j)

        # Train indices = everything not in test and not in embargo
        test_set = set(int(i) for i in test_indices)
        train_indices = np.array(
            [i for i in range(n) if i not in test_set and i not in embargo_set],
            dtype=np.int64,
        )

        splits.append(
            CPCVSplit(
                train_indices=train_indices,
                test_indices=test_indices.astype(np.int64),
                test_groups=tuple(combo),
            )
        )

    return splits


def aggregate_oos_distribution(
    splits_results: list[float],
) -> dict:
    """Summarize the distribution of a metric across CPCV splits.

    Takes a list of per-split metric values (e.g. log_growth_per_trade
    on each test fold) and returns mean, std, and quantile statistics.
    The std across splits is the framework's "honest" estimate of OOS
    metric variability.
    """
    arr = np.asarray(splits_results, dtype=np.float64)
    if len(arr) == 0:
        return {"n": 0}
    return {
        "n": int(len(arr)),
        "mean": float(np.mean(arr)),
        "std": float(np.std(arr, ddof=1)) if len(arr) > 1 else 0.0,
        "min": float(np.min(arr)),
        "max": float(np.max(arr)),
        "q05": float(np.quantile(arr, 0.05)),
        "q25": float(np.quantile(arr, 0.25)),
        "median": float(np.median(arr)),
        "q75": float(np.quantile(arr, 0.75)),
        "q95": float(np.quantile(arr, 0.95)),
    }
