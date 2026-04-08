"""Direction-asymmetric regime-gated dual-logistic forecaster.

This is the strategy designed in response to the Apr 4-7 ETH paper-
trade pattern. The data showed that continuation strategies' win
rate decayed asymmetrically by direction:

    Apr 4: UP 84.6%  DOWN 90.5%   ← both directions winning big
    Apr 5: UP 79.3%  DOWN 68.2%   ← UP still strong, DOWN softening
    Apr 6: UP 67.5%  DOWN 51.2%   ← DOWN at coinflip, UP still positive
    Apr 7: UP 49.3%  DOWN 69.4%   ← FLIPPED: UP at coinflip, DOWN strong

A single aggregate forecaster would have seen Apr 7's 57% aggregate
WR as "still has edge" and kept trading UP. Two separate forecasters
— one for UP-move continuation and one for DOWN-move continuation —
would have detected that the UP model's calibration drifted off
while the DOWN model's stayed fine, and the framework would have
gated UP off without affecting DOWN.

That's the architecture this file encodes.

Pipeline:

  1. The training data is split by the sign of the underlying
     move at the snapshot's entry index. UP-move snapshots train
     forecaster_up; DOWN-move snapshots train forecaster_down.
     Both use the same feature set.

  2. Each direction gets its own L2 logistic regression fit on
     its slice. The label is "did the market resolve in the
     direction of the move?" — i.e. for an UP-move row, label
     is 1 if winner=Up, 0 if winner=Down. Symmetric for DOWN.

  3. Each direction gets its own isotonic recalibration on the
     CV slice.

  4. At prediction time, the forecaster looks at the current
     snapshot's move sign and routes to the appropriate
     direction-specific model. The output q is interpreted as
     P(continuation pays off in this direction), which the
     framework's edge gate then compares to the relevant ask
     side via the direction-specific Kelly math.

The forecaster exposes two callables for the simulator:
forecaster_up_callable and forecaster_down_callable. The simulator's
existing logic already calls one of these depending on which side
of the book it's evaluating. This is the cleanest integration with
the existing simulator interface.

Features used (hard-coded for v1, configurable in a later version):
  - move_pct, abs_move, velocity, consistency, volatility,
    token_skew, elapsed_pct, acceleration  (per-snapshot)
  - lag_n_up_frac, lag_n_mean_abs_move, lag_n_mean_signed_move,
    lag_n_up_magnitude, lag_n_down_magnitude,
    hour_of_day, day_of_week, n_past_markets  (per-market regime)

That's 17 features. The L2 logistic with C in the preregistration
grid keeps this from overfitting on the CV partition.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Callable, Optional

import numpy as np

from autoresearch.forecasters.logistic import LogisticRegression
from autoresearch.methods.calibration import isotonic_calibrator
from autoresearch.methods.simulator import MarketSlice


# Per-snapshot features the forecaster reads from MarketSlice.features
PER_SNAPSHOT_FEATURES = [
    "move_pct", "abs_move", "velocity", "consistency",
    "volatility", "token_skew", "elapsed_pct", "acceleration",
]

# Regime features (constant within a market, broadcast across snapshots)
REGIME_FEATURES = [
    "lag_n_up_frac", "lag_n_mean_abs_move", "lag_n_mean_signed_move",
    "lag_n_up_magnitude", "lag_n_down_magnitude",
    "hour_of_day", "day_of_week", "n_past_markets",
]

ALL_FEATURE_NAMES = PER_SNAPSHOT_FEATURES + REGIME_FEATURES


def extract_feature_row(slice_: MarketSlice, i: int) -> np.ndarray:
    """Extract the feature vector for a single (market, snapshot) point.

    Returns a 1-D array of length len(ALL_FEATURE_NAMES). NaN values
    are passed through; the logistic predictor handles them by
    returning 0.5.
    """
    feats = slice_.features
    out = np.empty(len(ALL_FEATURE_NAMES), dtype=np.float64)
    for k, name in enumerate(ALL_FEATURE_NAMES):
        arr = feats.get(name)
        if arr is None:
            out[k] = np.nan
        else:
            try:
                out[k] = float(arr[i])
            except (IndexError, TypeError):
                out[k] = np.nan
    return out


def build_training_matrix(
    slices: list[MarketSlice],
    direction: str,
    entry_index_fn: Callable[[MarketSlice], int],
) -> tuple[np.ndarray, np.ndarray]:
    """Build (X, y) for one direction from a list of market slices.

    For each market:
      - Determine the entry index (the snapshot where the strategy
        would have considered entering)
      - Skip if the move at that index doesn't match the requested
        direction (e.g. for direction='up', skip markets where
        move_pct[entry_index] < 0)
      - Skip if any required feature is NaN at that index
      - The label is 1 if the market's winner matches the direction
        ('Up' for direction='up', 'Down' for direction='down'),
        else 0

    The entry_index_fn lets the caller decide where in each market
    to extract the snapshot. For training a continuation forecaster,
    a typical choice is "the first snapshot where abs(move_pct) >= 0.05",
    which mimics the threshold strategy's entry condition.
    """
    if direction not in ("up", "down"):
        raise ValueError(f"direction must be 'up' or 'down', got {direction}")

    rows = []
    labels = []
    for s in slices:
        i = entry_index_fn(s)
        if i is None or i < 0 or i >= len(s.best_ask_up):
            continue
        move_arr = s.features.get("move_pct")
        if move_arr is None:
            continue
        try:
            move_at_i = float(move_arr[i])
        except (IndexError, TypeError):
            continue
        if not np.isfinite(move_at_i):
            continue
        # Filter by direction
        if direction == "up" and move_at_i <= 0:
            continue
        if direction == "down" and move_at_i >= 0:
            continue

        feat_row = extract_feature_row(s, i)
        if not np.all(np.isfinite(feat_row)):
            # Drop rows with any NaN feature; logistic.fit also
            # filters but explicit drop here gives more accurate
            # training counts in the artifact
            continue

        # Label: did the market resolve in our bet's direction?
        if direction == "up":
            label = 1.0 if s.winner == "Up" else 0.0
        else:
            label = 1.0 if s.winner == "Down" else 0.0

        rows.append(feat_row)
        labels.append(label)

    if not rows:
        return np.zeros((0, len(ALL_FEATURE_NAMES))), np.zeros(0)
    return np.array(rows, dtype=np.float64), np.array(labels, dtype=np.float64)


def default_entry_index_fn(min_abs_move: float = 0.05) -> Callable[[MarketSlice], int]:
    """Standard entry index: first snapshot where abs(move_pct) >= threshold.

    Returns -1 if no snapshot in the market crosses the threshold.
    """
    def fn(s: MarketSlice) -> int:
        move_arr = s.features.get("move_pct")
        if move_arr is None:
            return -1
        for i in range(10, len(move_arr)):  # skip first 10 like the simulator does
            if not np.isfinite(move_arr[i]):
                continue
            if abs(float(move_arr[i])) >= min_abs_move:
                # Also require book to be present for this entry to be tradeable
                if (np.isfinite(s.best_ask_up[i]) and np.isfinite(s.best_ask_down[i])):
                    return i
        return -1
    return fn


@dataclass
class RegimeGatedForecaster:
    """Dual-direction regime-gated logistic forecaster.

    After fit(), exposes:
      - q_up(slice_, i): P(YES wins) when betting UP continuation
      - q_down(slice_, i): P(NO wins) when betting DOWN continuation

    For the simulator's interface (a single ForecasterFn that returns
    q given (slice, i)), use as_simulator_forecaster() which returns
    a callable that routes to the right direction model based on the
    sign of move_pct at index i.
    """
    C: float = 1.0
    lag_n: int = 20  # informational; the actual lag is in the feature store
    min_abs_move: float = 0.05
    max_iter: int = 200

    model_up: Optional[LogisticRegression] = None
    model_down: Optional[LogisticRegression] = None
    isotonic_up: Optional[Callable] = None
    isotonic_down: Optional[Callable] = None
    train_stats: dict = field(default_factory=dict)

    def fit(
        self,
        train_slices: list[MarketSlice],
        cv_slices: Optional[list[MarketSlice]] = None,
        recalibrate: bool = True,
    ) -> "RegimeGatedForecaster":
        """Fit both direction models on TRAIN, optionally recalibrate
        with isotonic regression on CV.

        If cv_slices is None or recalibrate=False, the isotonic
        layer is identity (passes raw probs through).
        """
        entry_fn = default_entry_index_fn(self.min_abs_move)

        # Train each direction
        X_up, y_up = build_training_matrix(train_slices, "up", entry_fn)
        X_dn, y_dn = build_training_matrix(train_slices, "down", entry_fn)

        self.train_stats = {
            "n_train_up": int(len(y_up)),
            "n_train_down": int(len(y_dn)),
            "train_up_base_rate": float(np.mean(y_up)) if len(y_up) else float("nan"),
            "train_down_base_rate": float(np.mean(y_dn)) if len(y_dn) else float("nan"),
        }

        if len(y_up) >= 20:
            self.model_up = LogisticRegression(C=self.C, max_iter=self.max_iter).fit(
                X_up, y_up, feature_names=ALL_FEATURE_NAMES
            )
        if len(y_dn) >= 20:
            self.model_down = LogisticRegression(C=self.C, max_iter=self.max_iter).fit(
                X_dn, y_dn, feature_names=ALL_FEATURE_NAMES
            )

        # Optional isotonic recalibration on CV
        if recalibrate and cv_slices is not None:
            X_cv_up, y_cv_up = build_training_matrix(cv_slices, "up", entry_fn)
            X_cv_dn, y_cv_dn = build_training_matrix(cv_slices, "down", entry_fn)
            if self.model_up is not None and len(y_cv_up) >= 20:
                p_cv = self.model_up.predict_proba(X_cv_up)
                self.isotonic_up = isotonic_calibrator(p_cv, y_cv_up)
            if self.model_down is not None and len(y_cv_dn) >= 20:
                p_cv = self.model_down.predict_proba(X_cv_dn)
                self.isotonic_down = isotonic_calibrator(p_cv, y_cv_dn)

        return self

    def q_up(self, slice_: MarketSlice, i: int) -> float:
        if self.model_up is None:
            return 0.5
        x = extract_feature_row(slice_, i)
        if not np.all(np.isfinite(x)):
            return 0.5
        p_raw = self.model_up.predict_one(x)
        if self.isotonic_up is not None:
            return float(self.isotonic_up(np.array([p_raw]))[0])
        return p_raw

    def q_down(self, slice_: MarketSlice, i: int) -> float:
        if self.model_down is None:
            return 0.5
        x = extract_feature_row(slice_, i)
        if not np.all(np.isfinite(x)):
            return 0.5
        p_raw = self.model_down.predict_one(x)
        if self.isotonic_down is not None:
            return float(self.isotonic_down(np.array([p_raw]))[0])
        return p_raw

    def as_simulator_forecaster(self) -> Callable[[MarketSlice, int], float]:
        """Return a callable matching the ForecasterFn signature.

        Routes to q_up or q_down based on the sign of move_pct at i,
        and translates the direction-specific output into a single
        q = P(YES wins) that the simulator can use:

        - When move > 0: we are betting UP continuation. q_up tells
          us P(continuation works) = P(market resolves Up). Return
          q_up directly as P(YES wins).

        - When move < 0: we are betting DOWN continuation. q_down
          tells us P(continuation works) = P(market resolves Down).
          The simulator's q is P(YES wins), so we return 1 - q_down.

        - When move == 0 or NaN: no signal, return 0.5 (the
          framework's edge gate will reject this).
        """
        def fn(slice_: MarketSlice, i: int) -> float:
            move_arr = slice_.features.get("move_pct")
            if move_arr is None:
                return 0.5
            try:
                move_at_i = float(move_arr[i])
            except (IndexError, TypeError):
                return 0.5
            if not np.isfinite(move_at_i):
                return 0.5
            if move_at_i > 0:
                return self.q_up(slice_, i)
            elif move_at_i < 0:
                return 1.0 - self.q_down(slice_, i)
            else:
                return 0.5
        return fn
