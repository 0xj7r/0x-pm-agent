"""Tests for autoresearch.forecasters.logistic.LogisticRegression.

The test strategy: generate datasets where we KNOW the right answer
and verify the from-scratch logistic recovers it.
"""
from __future__ import annotations

import numpy as np
import pytest

from autoresearch.forecasters.logistic import LogisticRegression


class TestLogisticRegression:
    def test_recovers_known_separable(self):
        # 2 features. y = 1 if x[0] > 0 else 0. Trivially separable.
        rng = np.random.default_rng(42)
        n = 200
        X = rng.standard_normal((n, 2))
        y = (X[:, 0] > 0).astype(np.float64)
        model = LogisticRegression(C=10.0, max_iter=300).fit(X, y)
        # Predictions on training set should be very accurate
        p = model.predict_proba(X)
        preds = (p > 0.5).astype(np.float64)
        accuracy = float(np.mean(preds == y))
        assert accuracy > 0.95

    def test_intercept_only_for_constant_features(self):
        # All features identical → bias absorbs everything → predicts base rate
        rng = np.random.default_rng(7)
        n = 500
        X = np.ones((n, 3))  # all features = 1
        y = (rng.uniform(0, 1, n) < 0.7).astype(np.float64)
        model = LogisticRegression(C=1.0, max_iter=200).fit(X, y)
        p = model.predict_proba(X)
        # All predictions should be ~equal to base rate (0.7)
        assert np.std(p) < 0.01
        assert np.mean(p) == pytest.approx(0.7, abs=0.1)

    def test_regularisation_shrinks_weights(self):
        rng = np.random.default_rng(11)
        n = 500
        X = rng.standard_normal((n, 5))
        # Strong signal in feature 0
        y = (X[:, 0] + 0.1 * rng.standard_normal(n) > 0).astype(np.float64)
        weak = LogisticRegression(C=0.01, max_iter=300).fit(X, y)
        strong = LogisticRegression(C=100.0, max_iter=300).fit(X, y)
        # Stronger regularisation → smaller weights
        assert np.linalg.norm(weak.weights) < np.linalg.norm(strong.weights)

    def test_handles_nan_features_in_predict(self):
        rng = np.random.default_rng(13)
        n = 100
        X = rng.standard_normal((n, 3))
        y = (X[:, 0] > 0).astype(np.float64)
        model = LogisticRegression(C=1.0, max_iter=100).fit(X, y)
        # Predict on data where some rows have NaN
        X_test = X.copy()
        X_test[0:5, 0] = np.nan
        p = model.predict_proba(X_test)
        # NaN rows should return 0.5
        assert np.all(p[0:5] == 0.5)
        # Other rows should be normal predictions
        assert not np.any(p[5:] == 0.5)

    def test_handles_nan_features_in_fit(self):
        rng = np.random.default_rng(17)
        n = 200
        X = rng.standard_normal((n, 2))
        y = (X[:, 0] > 0).astype(np.float64)
        # Inject NaN into 10% of rows
        X[:20, 0] = np.nan
        # Should still fit on the 180 finite rows
        model = LogisticRegression(C=1.0, max_iter=100).fit(X, y)
        assert model.weights is not None
        assert not np.any(np.isnan(model.weights))

    def test_predict_one_matches_predict_proba(self):
        rng = np.random.default_rng(19)
        n = 100
        X = rng.standard_normal((n, 4))
        y = (X[:, 0] + X[:, 1] > 0).astype(np.float64)
        model = LogisticRegression(C=1.0, max_iter=200).fit(X, y)

        batch = model.predict_proba(X)
        for i in range(10):
            single = model.predict_one(X[i])
            assert single == pytest.approx(batch[i], abs=1e-9)

    def test_predict_one_handles_nan(self):
        rng = np.random.default_rng(23)
        n = 100
        X = rng.standard_normal((n, 3))
        y = (X[:, 0] > 0).astype(np.float64)
        model = LogisticRegression(C=1.0, max_iter=100).fit(X, y)

        nan_row = np.array([np.nan, 1.0, 2.0])
        assert model.predict_one(nan_row) == 0.5

    def test_fit_rejects_invalid_inputs(self):
        with pytest.raises(ValueError):
            LogisticRegression().fit(
                np.zeros((10, 3)), np.array([0.7] * 10)
            )  # non-binary y
        with pytest.raises(ValueError):
            LogisticRegression().fit(
                np.zeros((10, 3)), np.zeros(5)
            )  # length mismatch
        with pytest.raises(ValueError):
            LogisticRegression().fit(
                np.zeros(10), np.zeros(10)
            )  # 1-D X

    def test_must_fit_before_predict(self):
        m = LogisticRegression()
        with pytest.raises(RuntimeError):
            m.predict_proba(np.zeros((1, 3)))

    def test_invalid_C_rejected(self):
        with pytest.raises(ValueError):
            LogisticRegression(C=0)
        with pytest.raises(ValueError):
            LogisticRegression(C=-1)

    def test_xor_not_perfectly_learnable(self):
        # Sanity check: XOR is not learnable by linear logistic.
        # This test exists to confirm the model isn't magically
        # working on non-linearly-separable data — that would be
        # a sign of a bug.
        n = 400
        rng = np.random.default_rng(29)
        X = rng.uniform(-1, 1, (n, 2))
        y = ((X[:, 0] * X[:, 1]) > 0).astype(np.float64)
        model = LogisticRegression(C=1.0, max_iter=200).fit(X, y)
        p = model.predict_proba(X)
        accuracy = float(np.mean((p > 0.5).astype(np.float64) == y))
        # Should be roughly chance level (50%) — definitely not >70%
        assert accuracy < 0.65
