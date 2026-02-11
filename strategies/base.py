"""Base strategy interface. All strategies implement this."""

from __future__ import annotations

from abc import ABC, abstractmethod

from models.market import Market
from models.trade import Signal


class Strategy(ABC):
    @property
    @abstractmethod
    def name(self) -> str:
        """Human-readable strategy name."""
        ...

    @abstractmethod
    async def evaluate(self, markets: list[Market]) -> list[Signal]:
        """Evaluate markets and return trading signals.

        Args:
            markets: All active markets from Polymarket

        Returns:
            List of Signal objects for markets where we see an edge
        """
        ...
