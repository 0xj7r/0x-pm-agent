"""Bayesian signal engine for BTC 5-minute Up/Down markets.

Computes P(UP) from the current state of a 5-minute window using
weighted features in log-odds space. The log_odds value is recomputed
from scratch on each update (not accumulated), preventing runaway
values from thousands of Binance trades per window.
"""
from __future__ import annotations

import math
import logging

from strategies.strategy_config import SignalConfig

logger = logging.getLogger(__name__)


class BayesianSignalEngine:
    """Real-time Bayesian probability estimator for BTC direction.

    Unlike an accumulating model, this recomputes log_odds from the
    current window state on each update. This prevents unbounded growth
    from thousands of trades per window.
    """

    def __init__(self, config: SignalConfig) -> None:
        self._w1 = config.w1_order_flow
        self._w2 = config.w2_microprice
        self._w3 = config.w3_price_delta
        self._w4 = config.w4_acceleration
        self._threshold = config.confidence_threshold
        self.log_odds: float = 0.0

    @property
    def p_up(self) -> float:
        return 1.0 / (1.0 + math.exp(-self.log_odds))

    @property
    def p_down(self) -> float:
        return 1.0 - self.p_up

    @property
    def direction(self) -> str | None:
        if self.p_up >= self._threshold:
            return "UP"
        if self.p_down >= self._threshold:
            return "DOWN"
        return None

    @property
    def confident(self) -> bool:
        return self.direction is not None

    def set_state(
        self,
        order_flow_imbalance: float,
        microprice_deviation: float,
        price_delta: float,
        acceleration: float,
    ) -> None:
        """Recompute log_odds from current window state (not accumulated)."""
        self.log_odds = (
            self._w1 * order_flow_imbalance
            + self._w2 * microprice_deviation
            + self._w3 * price_delta
            + self._w4 * acceleration
        )

    def update(
        self,
        order_flow_imbalance: float,
        microprice_deviation: float,
        price_delta: float,
        acceleration: float,
    ) -> None:
        """Backward-compatible alias for set_state."""
        self.set_state(order_flow_imbalance, microprice_deviation, price_delta, acceleration)

    def reset(self) -> None:
        self.log_odds = 0.0
