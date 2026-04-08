"""Tests for autoresearch.forecasters.regime_gated.RegimeGatedForecaster.

The hardest test here is the integration test: build a synthetic
dataset where UP-direction continuation works perfectly and
DOWN-direction continuation is pure noise, then verify the dual
forecaster correctly learns the asymmetry — UP model has high
predictive accuracy, DOWN model collapses to base rate.
"""
from __future__ import annotations

import math

import numpy as np
import pytest

from autoresearch.forecasters.regime_gated import (
    ALL_FEATURE_NAMES,
    PER_SNAPSHOT_FEATURES,
    REGIME_FEATURES,
    RegimeGatedForecaster,
    build_training_matrix,
    default_entry_index_fn,
    extract_feature_row,
)
from autoresearch.methods.simulator import MarketSlice


def make_synthetic_market(
    market_id: str,
    winner: str,
    move_at_5: float,
    n: int = 30,
    extra_features: dict | None = None,
) -> MarketSlice:
    """Build a market slice where snapshot index 5 has a known move.

    Earlier indices (0-4) get tiny moves below the threshold so the
    entry index function picks index 5.
    """
    move_arr = np.zeros(n, dtype=np.float64)
    move_arr[5:] = move_at_5  # constant from index 5 onwards
    abs_arr = np.abs(move_arr)

    features = {
        "move_pct": move_arr,
        "abs_move": abs_arr,
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
    }
    if extra_features:
        features.update(extra_features)

    return MarketSlice(
        market_id=market_id,
        winner=winner,
        best_ask_up=np.full(n, 0.5),
        best_ask_down=np.full(n, 0.5),
        best_bid_up=np.full(n, 0.49),
        best_bid_down=np.full(n, 0.49),
        features=features,
    )


class TestExtractFeatureRow:
    def test_returns_correct_length(self):
        s = make_synthetic_market("m", "Up", move_at_5=0.08)
        x = extract_feature_row(s, 5)
        assert len(x) == len(ALL_FEATURE_NAMES)
        assert x.dtype == np.float64

    def test_values_match_features(self):
        s = make_synthetic_market("m", "Up", move_at_5=0.08)
        x = extract_feature_row(s, 5)
        # move_pct is the first feature in ALL_FEATURE_NAMES
        assert x[0] == pytest.approx(0.08)
        # hour_of_day should be 12
        idx = ALL_FEATURE_NAMES.index("hour_of_day")
        assert x[idx] == 12.0

    def test_nan_for_missing_feature(self):
        s = make_synthetic_market("m", "Up", move_at_5=0.08)
        # Drop one feature from the dict
        del s.features["volatility"]
        x = extract_feature_row(s, 5)
        idx = ALL_FEATURE_NAMES.index("volatility")
        assert math.isnan(x[idx])


class TestEntryIndexFn:
    def test_finds_first_threshold_crossing(self):
        s = make_synthetic_market("m", "Up", move_at_5=0.08)
        fn = default_entry_index_fn(min_abs_move=0.05)
        i = fn(s)
        # First crossing is at index 5 (where move jumps from 0 to 0.08)
        # but the function starts at i=10 to mimic the simulator
        assert i == 10

    def test_no_crossing_returns_negative(self):
        s = make_synthetic_market("m", "Up", move_at_5=0.001)
        fn = default_entry_index_fn(min_abs_move=0.05)
        assert fn(s) == -1


class TestBuildTrainingMatrix:
    def test_filters_by_direction_up(self):
        slices = [
            make_synthetic_market("u1", "Up", move_at_5=0.08),
            make_synthetic_market("u2", "Up", move_at_5=0.09),
            make_synthetic_market("d1", "Down", move_at_5=-0.08),
            make_synthetic_market("d2", "Up", move_at_5=-0.07),  # down move, up winner
        ]
        fn = default_entry_index_fn(min_abs_move=0.05)
        X, y = build_training_matrix(slices, "up", fn)
        # Only 2 markets had up moves
        assert len(y) == 2
        # Both up-move markets had Up winners
        assert np.all(y == 1.0)

    def test_filters_by_direction_down(self):
        slices = [
            make_synthetic_market("u1", "Up", move_at_5=0.08),
            make_synthetic_market("d1", "Down", move_at_5=-0.08),
            make_synthetic_market("d2", "Up", move_at_5=-0.07),
        ]
        fn = default_entry_index_fn(min_abs_move=0.05)
        X, y = build_training_matrix(slices, "down", fn)
        assert len(y) == 2
        # d1 went down (continuation worked → 1)
        # d2 went up (continuation failed → 0)
        assert sorted(y.tolist()) == [0.0, 1.0]


class TestRegimeGatedForecaster:
    def _make_dataset(self, n_per_direction: int, seed: int = 42) -> list[MarketSlice]:
        """Build a dataset with a learnable signal in UP direction
        and pure noise in DOWN direction.

        UP markets: their `velocity` feature predicts the winner.
        High velocity → Up wins, low velocity → Down wins.

        DOWN markets: winner is random regardless of features.
        """
        rng = np.random.default_rng(seed)
        slices = []
        for i in range(n_per_direction):
            v = rng.uniform(-0.02, 0.02)
            # UP-direction market with velocity-driven winner
            winner = "Up" if v > 0 else "Down"
            s = make_synthetic_market(
                f"u{i}", winner, move_at_5=0.08,
                extra_features={"velocity": np.full(30, v)},
            )
            slices.append(s)
        for i in range(n_per_direction):
            v = rng.uniform(-0.02, 0.02)
            winner = rng.choice(["Up", "Down"])
            s = make_synthetic_market(
                f"d{i}", winner, move_at_5=-0.08,
                extra_features={"velocity": np.full(30, v)},
            )
            slices.append(s)
        return slices

    def test_learns_up_direction_predictive_signal(self):
        train = self._make_dataset(n_per_direction=200, seed=1)
        cv = self._make_dataset(n_per_direction=100, seed=2)

        forecaster = RegimeGatedForecaster(C=1.0, max_iter=200).fit(
            train, cv_slices=cv, recalibrate=False
        )
        assert forecaster.model_up is not None
        assert forecaster.model_down is not None

        # Test on unseen UP markets with extreme velocities
        test_up = make_synthetic_market(
            "test", "Up", move_at_5=0.08,
            extra_features={"velocity": np.full(30, 0.018)},
        )
        q = forecaster.q_up(test_up, 10)
        # High velocity → should predict Up wins (q > 0.6)
        assert q > 0.6, f"Expected q > 0.6 for high-velocity up market, got {q}"

        test_up_down_v = make_synthetic_market(
            "test", "Up", move_at_5=0.08,
            extra_features={"velocity": np.full(30, -0.018)},
        )
        q2 = forecaster.q_up(test_up_down_v, 10)
        # Negative velocity → should predict Down wins (q < 0.4)
        assert q2 < 0.4, f"Expected q < 0.4 for low-velocity up market, got {q2}"

    def test_down_direction_collapses_to_base_rate_on_pure_noise(self):
        train = self._make_dataset(n_per_direction=300, seed=1)
        forecaster = RegimeGatedForecaster(C=0.1, max_iter=200).fit(
            train, recalibrate=False
        )
        # Predictions on DOWN markets should not strongly differ
        # from base rate (around 0.5) because the signal isn't there
        rng = np.random.default_rng(99)
        qs = []
        for i in range(50):
            v = rng.uniform(-0.02, 0.02)
            s = make_synthetic_market(
                f"t{i}", "Up", move_at_5=-0.08,
                extra_features={"velocity": np.full(30, v)},
            )
            qs.append(forecaster.q_down(s, 10))
        # Average q should be near 0.5 (within 0.15)
        assert abs(np.mean(qs) - 0.5) < 0.15

    def test_simulator_callable_routes_correctly(self):
        train = self._make_dataset(n_per_direction=200, seed=1)
        forecaster = RegimeGatedForecaster(C=1.0).fit(train, recalibrate=False)
        callable_fn = forecaster.as_simulator_forecaster()

        # Up-move market: callable should return q_up directly
        s_up = make_synthetic_market(
            "test", "Up", move_at_5=0.08,
            extra_features={"velocity": np.full(30, 0.018)},
        )
        q_route = callable_fn(s_up, 10)
        q_direct = forecaster.q_up(s_up, 10)
        assert q_route == pytest.approx(q_direct)

        # Down-move market: callable should return 1 - q_down
        s_dn = make_synthetic_market(
            "test", "Down", move_at_5=-0.08,
            extra_features={"velocity": np.full(30, 0.018)},
        )
        q_route = callable_fn(s_dn, 10)
        q_direct = forecaster.q_down(s_dn, 10)
        assert q_route == pytest.approx(1.0 - q_direct)

    def test_zero_move_returns_half(self):
        train = self._make_dataset(n_per_direction=100, seed=1)
        forecaster = RegimeGatedForecaster().fit(train, recalibrate=False)
        callable_fn = forecaster.as_simulator_forecaster()
        s_flat = make_synthetic_market("flat", "Up", move_at_5=0.0)
        assert callable_fn(s_flat, 10) == 0.5

    def test_unfit_returns_half(self):
        f = RegimeGatedForecaster()
        s = make_synthetic_market("m", "Up", move_at_5=0.08)
        assert f.q_up(s, 10) == 0.5
        assert f.q_down(s, 10) == 0.5

    def test_isotonic_recalibration_applied(self):
        train = self._make_dataset(n_per_direction=200, seed=1)
        cv = self._make_dataset(n_per_direction=100, seed=2)
        f_recal = RegimeGatedForecaster(C=1.0).fit(train, cv_slices=cv, recalibrate=True)
        assert f_recal.isotonic_up is not None
        assert f_recal.isotonic_down is not None

    def test_train_stats_populated(self):
        train = self._make_dataset(n_per_direction=200, seed=1)
        f = RegimeGatedForecaster().fit(train, recalibrate=False)
        assert f.train_stats["n_train_up"] > 0
        assert f.train_stats["n_train_down"] > 0
        assert 0 <= f.train_stats["train_up_base_rate"] <= 1
        assert 0 <= f.train_stats["train_down_base_rate"] <= 1


class TestFeatureLists:
    def test_per_snapshot_and_regime_disjoint(self):
        assert set(PER_SNAPSHOT_FEATURES).isdisjoint(set(REGIME_FEATURES))

    def test_all_features_is_concat(self):
        assert ALL_FEATURE_NAMES == PER_SNAPSHOT_FEATURES + REGIME_FEATURES
