"""Pure-numpy L2-regularised logistic regression.

We do this from scratch instead of importing sklearn for two reasons:

1. Dependency control. The framework's deployment surface is small
   (numpy, httpx, sqlite3 from stdlib, plus pytest for tests).
   Adding sklearn pulls in scipy, joblib, threadpoolctl etc., all
   of which we don't otherwise need. The fewer moving parts, the
   fewer ways the audit log can lie about what version of what
   library produced a given candidate.

2. Auditability. Logistic regression is ~50 lines. We control the
   loss, the optimizer, the regularisation, and the early-stopping
   criterion. When the framework's calibration audit later shows
   surprising results, we can debug the model directly without
   wondering whether sklearn changed a default between versions.

Implementation:
- Loss: log-loss + (1 / 2C) * ||w||^2 (sklearn convention: smaller
  C means stronger regularisation)
- Optimizer: L-BFGS-style line search would be ideal but is heavy.
  We use gradient descent with adaptive step size (Armijo-Goldstein
  backtracking) for simplicity and robustness on small samples.
- Convergence: relative loss change < 1e-7 or max_iter reached
- Bias term: explicitly tracked, NOT regularised
- Predict: returns sigmoid(x @ w + b), NOT clipped (caller can
  pipe through isotonic recalibration)
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Optional

import numpy as np


@dataclass
class LogisticRegression:
    """L2-regularised binary logistic regression with feature standardisation.

    Features are z-scored at fit time and the same mean/std are
    applied at predict time. This makes the optimiser robust to
    wildly different feature scales (e.g. velocity ~0.02 alongside
    n_past_markets ~20). Without standardisation, the gradient on
    small-scale features is too small for the optimizer to move
    weights meaningfully in a reasonable number of iterations.

    Attributes set after fit():
        weights:   shape (n_features,) — weights in the standardised space
        bias:      scalar — bias in the standardised space
        feature_means: shape (n_features,) — used to standardise predict inputs
        feature_stds:  shape (n_features,) — used to standardise predict inputs
        n_iter:    iterations to convergence
        final_loss: regularised loss at the converged solution
        feature_names: optional list of feature names for diagnostics
    """
    C: float = 1.0
    max_iter: int = 500
    tol: float = 1e-8
    initial_step: float = 1.0
    backtrack_factor: float = 0.5
    backtrack_max: int = 30

    weights: Optional[np.ndarray] = None
    bias: float = 0.0
    feature_means: Optional[np.ndarray] = None
    feature_stds: Optional[np.ndarray] = None
    n_iter: int = 0
    final_loss: float = float("nan")
    feature_names: Optional[list[str]] = None

    def __post_init__(self) -> None:
        if self.C <= 0:
            raise ValueError(f"C must be > 0, got {self.C}")

    @staticmethod
    def _sigmoid(z: np.ndarray) -> np.ndarray:
        # Numerically stable sigmoid
        out = np.empty_like(z)
        positive = z >= 0
        negative = ~positive
        out[positive] = 1.0 / (1.0 + np.exp(-z[positive]))
        exp_z = np.exp(z[negative])
        out[negative] = exp_z / (1.0 + exp_z)
        return out

    def _loss_and_grad(
        self, X: np.ndarray, y: np.ndarray, w: np.ndarray, b: float
    ) -> tuple[float, np.ndarray, float]:
        z = X @ w + b
        p = self._sigmoid(z)
        # Clip for log
        eps = 1e-15
        p_clipped = np.clip(p, eps, 1 - eps)
        n = len(y)
        # Mean log-loss
        ll = -np.mean(y * np.log(p_clipped) + (1 - y) * np.log(1 - p_clipped))
        reg = 0.5 / self.C * float(np.dot(w, w))
        loss = ll + reg / n  # divide reg by n so loss scale matches sklearn-ish
        # Gradient of mean log-loss wrt w: X^T (p - y) / n
        grad_w = X.T @ (p - y) / n + (1.0 / self.C) * w / n
        grad_b = float(np.mean(p - y))
        return float(loss), grad_w, grad_b

    def fit(self, X: np.ndarray, y: np.ndarray, feature_names: Optional[list[str]] = None) -> "LogisticRegression":
        X = np.asarray(X, dtype=np.float64)
        y = np.asarray(y, dtype=np.float64)
        if X.ndim != 2:
            raise ValueError(f"X must be 2-D, got {X.ndim}-D")
        if y.ndim != 1:
            raise ValueError(f"y must be 1-D, got {y.ndim}-D")
        if len(X) != len(y):
            raise ValueError(f"len mismatch: X={len(X)} y={len(y)}")
        if not np.all((y == 0) | (y == 1)):
            raise ValueError("y must be binary 0/1")

        # Drop rows with any NaN feature so the optimiser stays sane
        finite_mask = np.all(np.isfinite(X), axis=1)
        if not np.any(finite_mask):
            raise ValueError("no rows with all-finite features")
        X = X[finite_mask]
        y = y[finite_mask]

        # Standardise features. Constant or near-constant features get
        # std=1 (which leaves them at zero after centering, so they
        # contribute nothing to the model — the right behaviour for
        # truly constant features that have no information).
        means = np.mean(X, axis=0)
        stds = np.std(X, axis=0)
        stds = np.where(stds < 1e-12, 1.0, stds)
        X_std = (X - means) / stds

        n_features = X_std.shape[1]
        w = np.zeros(n_features, dtype=np.float64)
        b = 0.0
        prev_loss = float("inf")
        new_loss = float("inf")

        for it in range(self.max_iter):
            loss, grad_w, grad_b = self._loss_and_grad(X_std, y, w, b)

            # Backtracking line search with Armijo condition
            step = self.initial_step
            grad_norm_sq = float(np.dot(grad_w, grad_w)) + grad_b * grad_b
            if grad_norm_sq < 1e-20:
                # Already at the optimum
                self.n_iter = it + 1
                new_loss = loss
                break
            for _ in range(self.backtrack_max):
                w_new = w - step * grad_w
                b_new = b - step * grad_b
                new_loss, _, _ = self._loss_and_grad(X_std, y, w_new, b_new)
                if new_loss <= loss - 1e-4 * step * grad_norm_sq:
                    break
                step *= self.backtrack_factor
            w, b = w_new, b_new

            if abs(prev_loss - new_loss) < self.tol * max(abs(new_loss), 1.0):
                self.n_iter = it + 1
                break
            prev_loss = new_loss
        else:
            self.n_iter = self.max_iter

        self.weights = w
        self.bias = b
        self.feature_means = means
        self.feature_stds = stds
        self.final_loss = float(new_loss)
        self.feature_names = feature_names
        return self

    def _standardise(self, X: np.ndarray) -> np.ndarray:
        """Apply the fitted z-score transform to predict inputs."""
        if self.feature_means is None or self.feature_stds is None:
            raise RuntimeError("must call fit() before predict")
        return (X - self.feature_means) / self.feature_stds

    def decision_function(self, X: np.ndarray) -> np.ndarray:
        if self.weights is None:
            raise RuntimeError("must call fit() before decision_function()")
        X = np.asarray(X, dtype=np.float64)
        X_std = self._standardise(X)
        return X_std @ self.weights + self.bias

    def predict_proba(self, X: np.ndarray) -> np.ndarray:
        """Returns P(y=1 | X) as an array of shape (n,).

        Rows with any NaN feature get q=0.5 (no information). The
        framework's edge-after-costs gate will reject these as
        unprofitable trades, which is the correct conservative
        behaviour.
        """
        if self.weights is None:
            raise RuntimeError("must call fit() before predict_proba()")
        X = np.asarray(X, dtype=np.float64)
        out = np.full(X.shape[0], 0.5, dtype=np.float64)
        finite_mask = np.all(np.isfinite(X), axis=1)
        if np.any(finite_mask):
            X_finite = X[finite_mask]
            X_std = self._standardise(X_finite)
            z = X_std @ self.weights + self.bias
            out[finite_mask] = self._sigmoid(z)
        return out

    def predict_one(self, x: np.ndarray) -> float:
        """Convenience for the simulator: predict a single row."""
        if self.weights is None:
            raise RuntimeError("must call fit() before predict_one()")
        x = np.asarray(x, dtype=np.float64)
        if not np.all(np.isfinite(x)):
            return 0.5
        x_std = self._standardise(x)
        z = float(x_std @ self.weights + self.bias)
        return float(self._sigmoid(np.array([z]))[0])
