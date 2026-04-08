"""Tests for autoresearch.forecasters.regime_confidence_gated.RegimeConfidenceGated."""
from __future__ import annotations

import math

import numpy as np
import pytest

from autoresearch.forecasters.regime_confidence_gated import RegimeConfidenceGated
from autoresearch.forecasters.regime_gated import RegimeGatedForecaster
from autoresearch.methods.simulator import MarketSlice


def make_slice(
    move_at_5: float = 0.08,
    n: int = 30,
    winner: str = "Up",
    lag_n_up_frac: float = 0.7,
    lag_n_signed: float = 0.05,
    lag_n_abs: float = 0.05,
) -> MarketSlice:
    move_arr = np.zeros(n)
    move_arr[5:] = move_at_5
    return MarketSlice(
        market_id="test",
        winner=winner,
        best_ask_up=np.full(n, 0.5),
        best_ask_down=np.full(n, 0.5),
        best_bid_up=np.full(n, 0.49),
        best_bid_down=np.full(n, 0.49),
        features={
            "move_pct": move_arr,
            "abs_move": np.abs(move_arr),
            "velocity": np.full(n, 0.01),
            "consistency": np.full(n, 0.5),
            "volatility": np.full(n, 0.001),
            "token_skew": np.zeros(n),
            "elapsed_pct": np.linspace(0, 0.012, n),
            "acceleration": np.zeros(n),
            "lag_n_up_frac": np.full(n, lag_n_up_frac),
            "lag_n_mean_abs_move": np.full(n, lag_n_abs),
            "lag_n_mean_signed_move": np.full(n, lag_n_signed),
            "lag_n_up_magnitude": np.full(n, 0.05),
            "lag_n_down_magnitude": np.full(n, 0.05),
            "hour_of_day": np.full(n, 12.0),
            "day_of_week": np.full(n, 2.0),
            "n_past_markets": np.full(n, 20.0),
        },
    )


def trained_regime() -> RegimeGatedForecaster:
    """Build a regime forecaster fit on a small synthetic dataset.

    Train it on data where high velocity → Up wins.
    """
    rng = np.random.default_rng(42)
    train = []
    for i in range(100):
        v = rng.uniform(-0.02, 0.02)
        winner = "Up" if v > 0 else "Down"
        move = 0.08 if winner == "Up" else -0.08
        s = make_slice(move_at_5=move, winner=winner)
        # Override velocity feature
        s.features["velocity"] = np.full(30, v)
        train.append(s)
    return RegimeGatedForecaster(C=1.0, max_iter=200).fit(train, recalibrate=False)


class TestRegimeConfidenceGated:
    def test_returns_nan_when_up_frac_too_central(self):
        regime = trained_regime()
        gated = RegimeConfidenceGated(
            regime=regime, min_up_frac_distance=0.20, min_q_conviction=0.05
        )
        callable_fn = gated.as_simulator_forecaster()
        # lag_n_up_frac = 0.55 → distance from 0.5 = 0.05 < 0.20 → NaN
        s = make_slice(lag_n_up_frac=0.55)
        q = callable_fn(s, 10)
        assert math.isnan(q)

    def test_passes_when_up_frac_strongly_directional(self):
        regime = trained_regime()
        gated = RegimeConfidenceGated(
            regime=regime,
            min_up_frac_distance=0.20,
            min_trend_strength=0.0,
            min_q_conviction=0.0,
        )
        callable_fn = gated.as_simulator_forecaster()
        # lag_n_up_frac = 0.80 → distance 0.30 > 0.20 → passes
        s = make_slice(lag_n_up_frac=0.80)
        q = callable_fn(s, 10)
        assert not math.isnan(q)
        assert 0 <= q <= 1

    def test_returns_nan_when_trend_strength_too_low(self):
        regime = trained_regime()
        gated = RegimeConfidenceGated(
            regime=regime,
            min_up_frac_distance=0.0,
            min_trend_strength=0.5,
            min_q_conviction=0.0,
        )
        callable_fn = gated.as_simulator_forecaster()
        # trend_strength = |signed| / abs = 0.01 / 0.10 = 0.10 < 0.5 → NaN
        s = make_slice(lag_n_signed=0.01, lag_n_abs=0.10)
        q = callable_fn(s, 10)
        assert math.isnan(q)

    def test_passes_when_trend_strength_high(self):
        regime = trained_regime()
        gated = RegimeConfidenceGated(
            regime=regime,
            min_up_frac_distance=0.0,
            min_trend_strength=0.5,
            min_q_conviction=0.0,
        )
        callable_fn = gated.as_simulator_forecaster()
        # trend_strength = 0.08 / 0.10 = 0.8 > 0.5 → passes
        s = make_slice(lag_n_signed=0.08, lag_n_abs=0.10)
        q = callable_fn(s, 10)
        assert not math.isnan(q)

    def test_returns_nan_when_model_low_conviction(self):
        regime = trained_regime()
        # Set conviction threshold so high that no realistic q can clear it
        gated = RegimeConfidenceGated(
            regime=regime,
            min_up_frac_distance=0.0,
            min_q_conviction=0.49,
        )
        callable_fn = gated.as_simulator_forecaster()
        # |q - 0.5| < 0.49 means q must be in (0.01, 0.99) to fail.
        # The trained logistic doesn't produce extreme values on synthetic
        # data, so this gate should reject everything.
        s = make_slice(lag_n_up_frac=0.7)
        q = callable_fn(s, 10)
        assert math.isnan(q)

    def test_simulator_skips_nan_returns(self):
        """Verify the integration with the simulator: NaN q means skip."""
        from autoresearch.methods.simulator import simulate_market
        from autoresearch.methods.sizing import SizingPolicy

        regime = trained_regime()
        # Gate so tight nothing fires
        gated = RegimeConfidenceGated(
            regime=regime,
            min_up_frac_distance=0.45,  # almost impossible
            min_q_conviction=0.0,
        )
        s = make_slice(lag_n_up_frac=0.50)
        result = simulate_market(s, gated.as_simulator_forecaster())
        assert result is None  # no trade because every snapshot is gated out

    def test_nan_features_return_nan(self):
        regime = trained_regime()
        gated = RegimeConfidenceGated(regime=regime)
        callable_fn = gated.as_simulator_forecaster()
        s = make_slice()
        s.features["lag_n_up_frac"] = np.full(30, np.nan)
        q = callable_fn(s, 10)
        assert math.isnan(q)

    def test_missing_features_return_nan(self):
        regime = trained_regime()
        gated = RegimeConfidenceGated(regime=regime)
        callable_fn = gated.as_simulator_forecaster()
        s = make_slice()
        del s.features["lag_n_up_frac"]
        q = callable_fn(s, 10)
        assert math.isnan(q)
