"""Edge accounting and the market-implied baseline.

This module is the home of the "is this trade actually profitable
after I pay everything?" question. It does NOT decide sizing (that
is sizing.py) and it does NOT decide whether to enter (the simulator
combines this with the sizing policy and the MIN_EDGE rule). It just
computes the four numbers needed to make those decisions:

1. break_even_price(p_ask, fee): the probability the contract has
   to resolve YES at to recover what we paid in cost.
2. edge(q, p_ask, fee): how far above break_even our q sits.
3. min_edge_required(spread, model_uncertainty): the policy threshold
   below which the trade is not worth taking even at positive
   nominal edge.
4. market_implied_q(p_ask_yes, p_ask_no): the "the market is right"
   forecaster, used as the mandatory baseline in every research run.

The market-implied baseline is the single most important sanity
check in the whole framework. A forecaster that does not beat
market-implied probability after costs is a forecaster that has
learned the same information the price already embeds, dressed up
to look like alpha.
"""
from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class EdgePolicy:
    """Policy for the MIN_EDGE rule.

    The trade is taken only if:

        edge >= max(spread_at_entry * spread_multiplier,
                    model_uncertainty * sigma_multiplier)

    where spread_at_entry is the YES side bid-ask spread at the
    moment of entry and model_uncertainty is the standard error of
    the q estimate (computed by the calibration step on CV).

    Defaults are deliberately conservative. The preregistration may
    tighten them but should not loosen them without justification.
    """
    spread_multiplier: float = 0.5
    sigma_multiplier: float = 2.0


def break_even_price(p_ask: float, fee: float) -> float:
    """The minimum probability the contract has to resolve YES at to
    cover what we paid in cost (price + fees).

    For a YES contract bought at p_ask with a per-trade fee of `fee`
    dollars, the cost basis is p_ask + fee. The contract pays $1 if
    it resolves YES and $0 otherwise. So we need P(YES) >= p_ask + fee
    for a positive expected value.
    """
    return p_ask + fee


def edge(q: float, p_ask: float, fee: float) -> float:
    """How far above break-even our q sits, in probability units.

    Positive values mean the trade has positive expected value
    after fees. Zero means break even. Negative means the trade
    loses money in expectation.
    """
    return q - break_even_price(p_ask, fee)


def min_edge_required(
    spread_at_entry: float,
    model_uncertainty: float,
    policy: EdgePolicy = EdgePolicy(),
) -> float:
    """The minimum edge required by policy to take the trade.

    The trade is taken only if measured edge exceeds this number.
    See EdgePolicy for the formula. Both spread and uncertainty
    are non-negative; the function is monotonic increasing in
    each.
    """
    if spread_at_entry < 0 or model_uncertainty < 0:
        raise ValueError(
            f"spread_at_entry and model_uncertainty must be non-negative, "
            f"got spread={spread_at_entry}, sigma={model_uncertainty}"
        )
    return max(
        spread_at_entry * policy.spread_multiplier,
        model_uncertainty * policy.sigma_multiplier,
    )


def market_implied_q(p_ask_yes: float, p_ask_no: float) -> float:
    """The 'market is right' baseline forecaster.

    Polymarket binary contracts have YES + NO ≈ 1 by construction
    but with a spread, so p_ask_yes + p_ask_no is typically slightly
    greater than 1 (the spread eats into both sides). The implied
    probability of YES is therefore not simply p_ask_yes; we have
    to normalize.

    The simplest defensible normalization is:

        q_market = p_ask_yes / (p_ask_yes + p_ask_no)

    which removes the spread proportionally. Better alternatives
    exist (use the midpoint of the cross-arbitrage band, etc.) but
    this is sufficient as a baseline because we are testing
    whether forecasters can beat ANY reasonable market-implied
    estimate. If a forecaster beats the proportional version, it
    almost certainly beats more sophisticated normalizations too.
    """
    if p_ask_yes <= 0 and p_ask_no <= 0:
        return 0.5
    if p_ask_yes <= 0:
        return 0.0
    if p_ask_no <= 0:
        return 1.0
    return p_ask_yes / (p_ask_yes + p_ask_no)
