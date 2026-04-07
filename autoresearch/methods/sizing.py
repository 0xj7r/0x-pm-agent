"""Position sizing for binary prediction-market trades.

Quarter-Kelly on a shrunk q estimate, with hard caps. The shrinkage
factor and the Kelly fraction are policy parameters: they are set
based on estimation error in q (calibrated externally), not tuned
against out-of-sample log growth. This is the spec's section 4.2 and
4.5 made into code.

The point of shrinking q toward 0.5 before sizing is that small errors
in q near the decision boundary blow up Kelly sizes. A model whose
true q is 0.55 but estimates 0.65 will be sized as if it had 10
percentage points more edge than it actually has. Shrinkage limits
that damage at the cost of underbetting when the model is correct.
For this market, that trade is correct: the cost of underbetting is
linear, the cost of overbetting can be ruinous.

A more sophisticated alternative is to compute a posterior on q given
the calibration uncertainty and use a lower confidence bound (e.g.
the 5th percentile of the posterior). v1 uses the simple shrinkage.
v2 may upgrade to the posterior version.
"""
from __future__ import annotations

from dataclasses import dataclass


# Policy defaults. The preregistration may override these but is
# expected to keep them at or below these values.
DEFAULT_KELLY_FRACTION = 0.25
DEFAULT_SHRINK_ALPHA = 0.5
DEFAULT_MAX_BET_FRACTION = 0.05


@dataclass(frozen=True)
class SizingPolicy:
    kelly_fraction: float = DEFAULT_KELLY_FRACTION
    shrink_alpha: float = DEFAULT_SHRINK_ALPHA
    max_bet_fraction: float = DEFAULT_MAX_BET_FRACTION

    def __post_init__(self) -> None:
        if not 0 < self.kelly_fraction <= 1:
            raise ValueError(
                f"kelly_fraction must be in (0, 1], got {self.kelly_fraction}"
            )
        if not 0 <= self.shrink_alpha <= 1:
            raise ValueError(
                f"shrink_alpha must be in [0, 1], got {self.shrink_alpha}"
            )
        if not 0 < self.max_bet_fraction <= 1:
            raise ValueError(
                f"max_bet_fraction must be in (0, 1], got {self.max_bet_fraction}"
            )


def shrink(q_hat: float, alpha: float) -> float:
    """Shrink a probability estimate toward 0.5 by factor alpha.

    alpha=1.0 returns q_hat unchanged. alpha=0.0 returns 0.5. Values
    in between linearly interpolate. The result is always in [0, 1].
    """
    return 0.5 + alpha * (q_hat - 0.5)


def kelly_fraction_for_yes(q: float, p_ask: float) -> float:
    """Full-Kelly stake fraction for a YES bet at price p_ask.

    Returns 0 if there is no edge or if the price is degenerate. The
    formula is the standard binary Kelly:

        f* = (q - p) / (1 - p)

    Note this assumes the bettor pays p_ask for the contract and
    receives 1 if it resolves YES, 0 otherwise. Fees are NOT included
    here; the caller is responsible for computing q vs the
    fee-adjusted break-even price before calling this.
    """
    if p_ask <= 0 or p_ask >= 1:
        return 0.0
    edge = q - p_ask
    if edge <= 0:
        return 0.0
    return edge / (1 - p_ask)


def stake_fraction(
    q_hat: float,
    p_ask: float,
    policy: SizingPolicy = SizingPolicy(),
) -> float:
    """Compute the bankroll fraction to stake on a YES bet.

    Pipeline: shrink q_hat toward 0.5 by policy.shrink_alpha, compute
    full Kelly on the shrunk q, scale by policy.kelly_fraction, cap
    at policy.max_bet_fraction. Returns 0 if any step would produce
    a non-positive stake.

    The shrinkage and the kelly_fraction multiplication are NOT the
    same operation: shrinkage adjusts our belief about q; the kelly
    fraction is a separate haircut on top of that for portfolio
    safety. Both are policy.
    """
    q = shrink(q_hat, policy.shrink_alpha)
    full = kelly_fraction_for_yes(q, p_ask)
    if full <= 0:
        return 0.0
    fractional = policy.kelly_fraction * full
    return min(fractional, policy.max_bet_fraction)


def stake_fraction_no(
    q_hat: float,
    p_ask_no: float,
    policy: SizingPolicy = SizingPolicy(),
) -> float:
    """Same as stake_fraction but for the NO side.

    For the NO bet at price p_ask_no, the implied probability is
    (1 - q_hat). The Kelly formula is symmetric: edge_no = (1 - q) - p_ask_no.
    """
    return stake_fraction(1.0 - q_hat, p_ask_no, policy)
