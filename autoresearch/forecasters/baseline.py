"""Market-implied baseline forecaster.

q = best_ask_yes / (best_ask_yes + best_ask_no)

This is the "the market is right" hypothesis. It assumes Polymarket's
order book already prices in everything we know, so the live ask
spread itself is the best estimate of P(YES wins). After accounting
for fees, this forecaster should produce zero edge by construction
on average — there is no monetisable signal beyond what the price
already contains.

Every other forecaster in the framework MUST beat this baseline on
log-growth-per-trade with bootstrap lower CI strictly above zero
to be promoted. A forecaster that fails to beat the baseline is
either learning the same information the price already contains
(no alpha), or producing noise that happens to look like alpha on
the training set (overfit).

This is spec section 4.7. The reason it's a separate file: every
research preregistration is required to declare which baseline it's
comparing against, and having a single canonical implementation
avoids "but my baseline was different" arguments.
"""
from __future__ import annotations

import numpy as np

from autoresearch.methods.simulator import MarketSlice


class MarketImpliedBaseline:
    """The mandatory baseline forecaster.

    Stateless: no fit step needed, no parameters to learn. Constructor
    takes no arguments. Callable as forecaster(slice_, i) -> q.
    """

    def __call__(self, slice_: MarketSlice, i: int) -> float:
        ask_yes = float(slice_.best_ask_up[i])
        ask_no = float(slice_.best_ask_down[i])

        if not (np.isfinite(ask_yes) and np.isfinite(ask_no)):
            return 0.5  # no information when book is missing

        if ask_yes <= 0 and ask_no <= 0:
            return 0.5
        if ask_yes <= 0:
            return 0.0
        if ask_no <= 0:
            return 1.0

        # Normalised by the cross-arbitrage band so spread cost is
        # removed proportionally. See methods/edge.py:market_implied_q
        # for the same logic in the methods library.
        return ask_yes / (ask_yes + ask_no)

    def __repr__(self) -> str:
        return "MarketImpliedBaseline()"
