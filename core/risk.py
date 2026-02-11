"""Risk management: Kelly Criterion position sizing and exposure limits."""

from __future__ import annotations

import logging

from config import Config
from models.trade import Signal

logger = logging.getLogger(__name__)


class RiskManager:
    def __init__(self, config: Config):
        self.max_position_pct = config.MAX_POSITION_PCT
        self.min_edge = config.MIN_EDGE_THRESHOLD
        self.kill_balance = config.KILL_BALANCE_USD

    def should_die(self, balance_usd: float) -> bool:
        """Check if the agent should shut down due to low balance."""
        if balance_usd <= self.kill_balance:
            logger.critical(
                f"KILL SWITCH: Balance ${balance_usd:.2f} <= "
                f"${self.kill_balance:.2f}. Agent dying."
            )
            return True
        return False

    def passes_filters(self, signal: Signal) -> bool:
        """Check if a signal meets minimum requirements to trade."""
        if abs(signal.edge) < self.min_edge:
            logger.debug(
                f"Signal rejected: edge {signal.edge:.3f} < "
                f"min {self.min_edge:.3f} for {signal.market_question[:50]}"
            )
            return False

        if signal.confidence < 0.3:
            logger.debug(
                f"Signal rejected: confidence {signal.confidence:.2f} < 0.3"
            )
            return False

        return True

    def kelly_size(
        self,
        edge: float,
        odds: float,
        confidence: float,
        bankroll: float,
    ) -> float:
        """Calculate position size using Kelly Criterion.

        Kelly fraction = (bp - q) / b
        where:
            b = net odds (payout / stake - 1)
            p = probability of winning (our fair value)
            q = probability of losing (1 - p)

        We use fractional Kelly (scaled by confidence) for safety.

        Args:
            edge: our estimated edge (fair_value - market_price)
            odds: market price we're buying at (0-1)
            confidence: how confident we are (0-1), used as Kelly fraction
            bankroll: current total balance in USD
        """
        if odds <= 0 or odds >= 1 or bankroll <= 0:
            return 0.0

        # Our estimated probability of winning
        p = odds + edge  # fair_value = market_price + edge
        p = max(0.01, min(0.99, p))
        q = 1.0 - p

        # Net odds: if we buy YES at price X, we win (1-X)/X if correct
        b = (1.0 - odds) / odds

        # Full Kelly fraction
        kelly = (b * p - q) / b

        if kelly <= 0:
            return 0.0

        # Fractional Kelly: scale by confidence for safety
        fraction = min(confidence, 0.5)  # never more than half Kelly
        adjusted_kelly = kelly * fraction

        # Cap at max position size
        max_size = bankroll * self.max_position_pct
        position = min(adjusted_kelly * bankroll, max_size)

        # Floor at $1 minimum to avoid dust trades
        if position < 1.0:
            return 0.0

        logger.info(
            f"Kelly sizing: edge={edge:.3f}, odds={odds:.3f}, "
            f"kelly={kelly:.3f}, fraction={fraction:.2f}, "
            f"size=${position:.2f} (max ${max_size:.2f})"
        )

        return round(position, 2)

    def size_position(self, signal: Signal, bankroll: float) -> float:
        """Calculate USD position size for a signal."""
        return self.kelly_size(
            edge=signal.edge,
            odds=signal.market_price,
            confidence=signal.confidence,
            bankroll=bankroll,
        )
