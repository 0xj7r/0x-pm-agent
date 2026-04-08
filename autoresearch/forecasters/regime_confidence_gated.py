"""Regime-confidence-gated wrapper around RegimeGatedForecaster.

Wave 1a's first-pass result was: the regime-gated dual-logistic
forecaster fires on every market and produces +0.002 log growth per
trade with a 95% CI that includes zero. The framework correctly
rejected all 8 candidates.

The hypothesis tested by this wrapper: the underlying signal is real
but ONLY in regimes where (a) recent markets have shown a strong
directional bias, AND (b) the recent moves are trending (signed
component dominates absolute component), AND (c) the model itself
has high conviction. By gating the forecaster to fire only when all
three conditions are met, we trade fewer markets but each with
higher conviction and (hopefully) higher per-trade edge.

This converts the forecaster from "trade everything with shrunken
sizing" to "trade rarely with full sizing in known-favorable
regimes". The trade count drops, the per-trade edge rises, and the
bootstrap CI lower bound has a chance of crossing zero.

The three gate parameters are intentionally exposed as constructor
arguments so they can appear in a research preregistration grid.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Callable

import numpy as np

from autoresearch.forecasters.regime_gated import RegimeGatedForecaster
from autoresearch.methods.simulator import MarketSlice


@dataclass
class RegimeConfidenceGated:
    """Wraps a fitted RegimeGatedForecaster with three confidence gates.

    Returns the underlying forecaster's q only when ALL of:

      1. |lag_n_up_frac - 0.5| >= min_up_frac_distance
         The recent past N markets are meaningfully one-sided
         (default 0.15 → at least 65/35 or 35/65 split in the
         last 20 markets).

      2. trend_strength = |lag_n_mean_signed_move| / lag_n_mean_abs_move
         >= min_trend_strength
         The recent moves are trending rather than choppy (default
         0.0 → no trend gate; set higher to filter chop).

      3. |q - 0.5| >= min_q_conviction
         The model itself thinks the prediction is meaningfully
         away from a coin flip (default 0.10).

    When any gate fails, returns NaN → the simulator filters NaN
    via its `if not 0 <= q_hat <= 1` check and skips the snapshot
    entirely. We CANNOT return 0.5 here as a "no trade" sentinel
    because the simulator interprets q=0.5 as a real forecast: on
    markets where one side asks well below 0.5, q=0.5 produces
    positive edge against the cheap side and triggers a trade.
    The 0.5 sentinel was a silent bug that disabled the gate
    entirely on the first run.
    """
    regime: RegimeGatedForecaster
    min_up_frac_distance: float = 0.15
    min_trend_strength: float = 0.0
    min_q_conviction: float = 0.10

    def as_simulator_forecaster(self) -> Callable[[MarketSlice, int], float]:
        regime_callable = self.regime.as_simulator_forecaster()

        def fn(slice_: MarketSlice, i: int) -> float:
            up_frac_arr = slice_.features.get("lag_n_up_frac")
            signed_arr = slice_.features.get("lag_n_mean_signed_move")
            abs_arr = slice_.features.get("lag_n_mean_abs_move")
            if up_frac_arr is None or signed_arr is None or abs_arr is None:
                return float("nan")
            try:
                uf = float(up_frac_arr[i])
                sm = float(signed_arr[i])
                am = float(abs_arr[i])
            except (IndexError, TypeError):
                return float("nan")
            if not (np.isfinite(uf) and np.isfinite(sm) and np.isfinite(am)):
                return float("nan")

            # Gate 1: directional regime
            if abs(uf - 0.5) < self.min_up_frac_distance:
                return float("nan")

            # Gate 2: trend strength
            if am > 0:
                trend_strength = abs(sm) / am
            else:
                trend_strength = 0.0
            if trend_strength < self.min_trend_strength:
                return float("nan")

            # Get model q (with all the regime model's internal logic)
            q = regime_callable(slice_, i)

            # Gate 3: model conviction
            if abs(q - 0.5) < self.min_q_conviction:
                return float("nan")

            return q

        return fn

    def __repr__(self) -> str:
        return (
            f"RegimeConfidenceGated(min_up_frac_distance={self.min_up_frac_distance}, "
            f"min_trend_strength={self.min_trend_strength}, "
            f"min_q_conviction={self.min_q_conviction})"
        )
