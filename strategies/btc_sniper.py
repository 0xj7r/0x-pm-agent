"""Bayesian signal engine for BTC 5-minute Up/Down markets.

Maintains a posterior probability P(UP) in log-odds space,
updated additively from real-time Binance data. When confidence
exceeds threshold, produces a directional signal for sniping
cheap tokens on Polymarket.
"""
from __future__ import annotations

import math
import logging

from strategies.strategy_config import SignalConfig

logger = logging.getLogger(__name__)


class BayesianSignalEngine:
    """Real-time Bayesian probability estimator for BTC direction."""

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

    def update(
        self,
        order_flow_imbalance: float,
        microprice_deviation: float,
        price_delta: float,
        acceleration: float,
    ) -> None:
        delta = (
            self._w1 * order_flow_imbalance
            + self._w2 * microprice_deviation
            + self._w3 * price_delta
            + self._w4 * acceleration
        )
        self.log_odds += delta

    def reset(self) -> None:
        self.log_odds = 0.0
