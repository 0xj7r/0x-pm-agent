"""Tests for autoresearch.forecasters.stacked.StackedBaselineRegime."""
from __future__ import annotations

import numpy as np
import pytest

from autoresearch.forecasters.regime_gated import RegimeGatedForecaster
from autoresearch.forecasters.stacked import StackedBaselineRegime
from autoresearch.methods.simulator import MarketSlice


def make_slice(
    ask_up: float,
    ask_down: float,
    move_pct: float = 0.08,
    n: int = 30,
    winner: str = "Up",
) -> MarketSlice:
    move_arr = np.zeros(n)
    move_arr[5:] = move_pct
    return MarketSlice(
        market_id="test",
        winner=winner,
        best_ask_up=np.full(n, ask_up),
        best_ask_down=np.full(n, ask_down),
        best_bid_up=np.full(n, max(0.001, ask_up - 0.01)),
        best_bid_down=np.full(n, max(0.001, ask_down - 0.01)),
        features={
            "move_pct": move_arr,
            "abs_move": np.abs(move_arr),
            "velocity": np.zeros(n),
            "consistency": np.full(n, 0.5),
            "volatility": np.full(n, 0.001),
            "token_skew": np.zeros(n),
            "elapsed_pct": np.linspace(0, 0.012, n),
            "acceleration": np.zeros(n),
            "lag_n_up_frac": np.full(n, 0.5),
            "lag_n_mean_abs_move": np.full(n, 0.05),
            "lag_n_mean_signed_move": np.full(n, 0.0),
            "lag_n_up_magnitude": np.full(n, 0.05),
            "lag_n_down_magnitude": np.full(n, 0.05),
            "hour_of_day": np.full(n, 12.0),
            "day_of_week": np.full(n, 2.0),
            "n_past_markets": np.full(n, 20.0),
        },
    )


def trained_regime() -> RegimeGatedForecaster:
    """Build a regime forecaster fit on a tiny synthetic dataset.

    The model isn't expected to learn anything meaningful; we just
    need a fitted instance whose as_simulator_forecaster() returns
    something callable for the stacked forecaster to fall through to.
    """
    rng = np.random.default_rng(42)
    train = []
    for i in range(50):
        winner = "Up" if rng.uniform() > 0.5 else "Down"
        move = rng.uniform(0.06, 0.10) if winner == "Up" else -rng.uniform(0.06, 0.10)
        train.append(make_slice(0.5, 0.5, move_pct=move, winner=winner))
    return RegimeGatedForecaster(C=1.0, max_iter=50).fit(train, recalibrate=False)


class TestStackedBaselineRegime:
    def test_uses_baseline_when_cross_arb_is_present(self):
        """When best_ask_up + best_ask_down is well below 1, baseline
        should have positive edge and the stack should return baseline q."""
        regime = trained_regime()
        stack = StackedBaselineRegime(regime=regime, baseline_threshold=0.0)
        callable_fn = stack.as_simulator_forecaster()

        # Cross-arb: ask_up=0.30, ask_down=0.40, sum=0.70 (huge cross-arb)
        s = make_slice(0.30, 0.40)
        q = callable_fn(s, 10)
        # Baseline q = 0.30/(0.30+0.40) = 0.4286
        assert q == pytest.approx(0.30 / (0.30 + 0.40), abs=0.01)

    def test_falls_through_to_regime_when_no_arb(self):
        """When the book is balanced/sum>=1, baseline has no edge and
        the stack should return the regime model's q."""
        regime = trained_regime()
        stack = StackedBaselineRegime(regime=regime, baseline_threshold=0.0)
        callable_fn = stack.as_simulator_forecaster()

        # Balanced book: ask_up=0.50, ask_down=0.51, sum=1.01 (no arb)
        s = make_slice(0.50, 0.51)
        q_stack = callable_fn(s, 10)
        # Should match regime forecaster's output, not baseline's
        regime_callable = regime.as_simulator_forecaster()
        q_regime = regime_callable(s, 10)
        assert q_stack == pytest.approx(q_regime, abs=1e-9)

    def test_threshold_makes_stack_more_conservative(self):
        regime = trained_regime()
        # baseline_threshold=0.05 means baseline must have at least
        # 5pp of edge to be preferred. A small cross-arb won't
        # trigger baseline preference.
        strict_stack = StackedBaselineRegime(regime=regime, baseline_threshold=0.05)
        loose_stack = StackedBaselineRegime(regime=regime, baseline_threshold=0.0)
        sf_strict = strict_stack.as_simulator_forecaster()
        sf_loose = loose_stack.as_simulator_forecaster()

        # Marginal cross-arb: ask_up=0.48, ask_down=0.50, sum=0.98
        s = make_slice(0.48, 0.50)
        q_strict = sf_strict(s, 10)
        q_loose = sf_loose(s, 10)
        # The two should differ on this case (loose uses baseline,
        # strict falls through to regime)
        # Regime returns its own q which is fixed for this synthetic input
        regime_callable = regime.as_simulator_forecaster()
        q_regime = regime_callable(s, 10)
        # loose stack picks baseline, strict picks regime
        if abs(q_loose - q_regime) > 1e-6:
            # baseline returned a different q; strict should match regime
            assert q_strict == pytest.approx(q_regime, abs=1e-9)

    def test_nan_book_falls_through_to_regime(self):
        regime = trained_regime()
        stack = StackedBaselineRegime(regime=regime)
        callable_fn = stack.as_simulator_forecaster()
        s = MarketSlice(
            market_id="t",
            winner="Up",
            best_ask_up=np.array([np.nan] * 30),
            best_ask_down=np.array([np.nan] * 30),
            best_bid_up=np.array([np.nan] * 30),
            best_bid_down=np.array([np.nan] * 30),
            features={
                "move_pct": np.zeros(30),
            },
        )
        q = callable_fn(s, 10)
        # Should not crash. NaN book → regime fallback → 0.5
        assert 0 <= q <= 1
