"""Stacked forecaster: market-implied baseline first, regime model fallback.

The first wave 1a research run revealed two distinct, non-overlapping
sources of edge in the ETH 5-min market data:

1. Cross-arbitrage opportunities (best_ask_up + best_ask_down < 1.0
   after fees). The market-implied baseline catches these on ~47%
   of CV markets with low win rate but asymmetric payouts that
   produce +0.005 log growth per trade.

2. Direction continuation signal (regime-gated forecaster) catches
   weak +0.002 log growth per trade across the full CV partition.

Every market the baseline trades is also a market the regime model
trades, but the regime model fires on 115 additional markets where
no cross-arb is available. The two strategies are complementary: the
baseline catches pure-arb opportunities, and the regime model catches
continuation signal where no arb exists.

This file implements the obvious combination: try the baseline q
first, and if the baseline would not produce a profitable trade,
fall through to the regime model's q. The simulator's existing edge
gate handles the actual decision; this forecaster only chooses which
q to feed it.

The result is a forecaster that captures both edges in a single pass,
without needing the simulator to do anything special. The framework
treats it as just another candidate that has to beat the
market-implied baseline on the edge gate — and now the bar is
"baseline + regime fallback" vs "baseline alone", which is a fair
test of whether the regime signal adds value on top of the cross-arb
that's already free.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Callable, Optional

import numpy as np

from autoresearch.forecasters.baseline import MarketImpliedBaseline
from autoresearch.forecasters.regime_gated import RegimeGatedForecaster
from autoresearch.methods.edge import edge as edge_after_costs
from autoresearch.methods.simulator import MarketSlice, fee_model_default


@dataclass
class StackedBaselineRegime:
    """Baseline q first; regime model q if baseline has no profitable edge.

    Configuration:
        regime: a fitted RegimeGatedForecaster (must be fit before use)
        baseline: a MarketImpliedBaseline instance (defaults to a fresh one)
        fee_fn: fee model used to test if baseline has positive edge
        baseline_threshold: minimum baseline edge to prefer baseline over
                            regime. Default 0.0 (any positive baseline
                            edge wins). A larger value (e.g. 0.005)
                            makes the stack only defer to baseline when
                            the cross-arb is decisively profitable.

    The behaviour is parameterless beyond what's already in the
    constituent forecasters: the regime model's hyperparameters are
    set when it's fit, and the baseline is stateless.
    """
    regime: RegimeGatedForecaster
    baseline: MarketImpliedBaseline = field(default_factory=MarketImpliedBaseline)
    fee_fn: Callable[[float], float] = fee_model_default
    baseline_threshold: float = 0.0

    def as_simulator_forecaster(self) -> Callable[[MarketSlice, int], float]:
        regime_callable = self.regime.as_simulator_forecaster()

        def fn(slice_: MarketSlice, i: int) -> float:
            ask_yes = float(slice_.best_ask_up[i])
            ask_no = float(slice_.best_ask_down[i])
            if not (np.isfinite(ask_yes) and np.isfinite(ask_no)):
                return regime_callable(slice_, i)
            if ask_yes <= 0 or ask_yes >= 1 or ask_no <= 0 or ask_no >= 1:
                return regime_callable(slice_, i)

            q_base = self.baseline(slice_, i)

            # Compute baseline's hypothetical edge on each side
            fee_yes = self.fee_fn(ask_yes)
            fee_no = self.fee_fn(ask_no)
            edge_yes_base = edge_after_costs(q_base, ask_yes, fee_yes)
            edge_no_base = edge_after_costs(1.0 - q_base, ask_no, fee_no)
            best_baseline_edge = max(edge_yes_base, edge_no_base)

            if best_baseline_edge > self.baseline_threshold:
                return q_base
            # Fall through to regime model
            return regime_callable(slice_, i)

        return fn

    def __repr__(self) -> str:
        return (
            f"StackedBaselineRegime(regime={type(self.regime).__name__}, "
            f"baseline_threshold={self.baseline_threshold})"
        )
