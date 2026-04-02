"""Simple threshold strategy for latency arbitrage.

Detects when a coin's price moves > threshold% and the Polymarket
order book hasn't repriced yet (directional token still cheap).
"""
from __future__ import annotations

from dataclasses import dataclass


@dataclass
class ThresholdStrategy:
    """Buy directional token when price move exceeds threshold and entry is cheap."""

    coin: str
    move_threshold: float
    max_entry: float

    def check_signal(
        self,
        price_move_pct: float,
        token_price_up: float,
        token_price_down: float,
    ) -> str | None:
        """Check if entry conditions are met.

        Returns:
            "Up" or "Down": direction to trade
            "SKIP": threshold crossed but entry too expensive (stop scanning)
            None: threshold not yet crossed (keep watching)
        """
        if abs(price_move_pct) < self.move_threshold:
            return None

        if price_move_pct > 0:
            direction, entry = "Up", token_price_up
        else:
            direction, entry = "Down", token_price_down

        if entry <= 0 or entry > self.max_entry:
            return "SKIP"

        return direction

    @classmethod
    def from_config(cls, coin: str, config: dict) -> ThresholdStrategy:
        return cls(
            coin=coin,
            move_threshold=config.get("move_threshold", 0.08),
            max_entry=config.get("max_entry", 0.55),
        )
