"""Stable TRAIN / CV / HOLDOUT partition function.

Per spec section 5.1, the manifest of resolved markets is split
into three contiguous partitions ordered by start_time:

    TRAIN  : first  60%
    CV     : next   20%
    HOLDOUT: last   20%

The partition function lives here so its commit hash can be
referenced from preregistrations. A change to the partition
function (different fractions, different boundary handling, a
shift from contiguous to interleaved partitions, etc.) invalidates
every prior preregistration that referenced the old commit hash.
The Researcher and Holdout Burner agents both look up the partition
function commit hash before running.

The split is computed by INDEX in a sorted manifest, NOT by absolute
time. This is intentional: when the snapshot dataset grows by a
day, the partition boundaries shift slightly because the absolute
60/20/20 fractions stretch over the longer window. This is the
correct behavior for a continuously-updating dataset; if you want
fixed time boundaries you should pin the manifest length in the
preregistration (and the dataset fingerprint reflects this).
"""
from __future__ import annotations

from dataclasses import dataclass


# These constants are part of the partition function's identity.
# Changing them invalidates prior preregistrations referencing the
# old commit hash.
TRAIN_FRACTION = 0.60
CV_FRACTION = 0.20
HOLDOUT_FRACTION = 0.20

assert abs(TRAIN_FRACTION + CV_FRACTION + HOLDOUT_FRACTION - 1.0) < 1e-9


@dataclass(frozen=True)
class Partitions:
    """Index ranges for each partition.

    Each range is [start, end) (Python slice convention). The
    train, cv, holdout ranges are contiguous and non-overlapping.
    The total covers the entire manifest: train_end == cv_start,
    cv_end == holdout_start, holdout_end == n.
    """
    n: int
    train_start: int
    train_end: int
    cv_start: int
    cv_end: int
    holdout_start: int
    holdout_end: int

    def train_slice(self) -> slice:
        return slice(self.train_start, self.train_end)

    def cv_slice(self) -> slice:
        return slice(self.cv_start, self.cv_end)

    def holdout_slice(self) -> slice:
        return slice(self.holdout_start, self.holdout_end)

    def train_size(self) -> int:
        return self.train_end - self.train_start

    def cv_size(self) -> int:
        return self.cv_end - self.cv_start

    def holdout_size(self) -> int:
        return self.holdout_end - self.holdout_start


def make_partitions(n: int) -> Partitions:
    """Compute the canonical 60/20/20 partitions for a manifest of length n.

    The boundaries are computed via int(n * fraction) which rounds
    toward zero. The holdout always extends to n exactly so no
    observation is silently dropped from the partition coverage.

    Raises ValueError if n is too small to admit a non-empty
    holdout. Minimum viable n is 5: 3 train, 1 cv, 1 holdout. The
    framework's validation step requires more than this for any
    real research, but the partition function itself only refuses
    when partitioning becomes mathematically impossible.
    """
    if n < 5:
        raise ValueError(
            f"manifest too small for 60/20/20 partition: n={n}, need at least 5"
        )
    train_end = int(n * TRAIN_FRACTION)
    cv_end = int(n * (TRAIN_FRACTION + CV_FRACTION))
    # Ensure each partition has at least one observation
    train_end = max(1, train_end)
    cv_end = max(train_end + 1, cv_end)
    if cv_end >= n:
        # Pull cv_end back so holdout has at least one observation
        cv_end = n - 1
    return Partitions(
        n=n,
        train_start=0,
        train_end=train_end,
        cv_start=train_end,
        cv_end=cv_end,
        holdout_start=cv_end,
        holdout_end=n,
    )
