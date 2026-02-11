"""Risk management: Kelly Criterion position sizing, exposure limits, and safeguards."""

from __future__ import annotations

import logging
import time
from datetime import datetime, timedelta

from config import Config
from models.trade import Signal

logger = logging.getLogger(__name__)


class RiskManager:
    def __init__(self, config: Config):
        self.max_position_pct = config.MAX_POSITION_PCT
        self.max_position_usd = config.MAX_POSITION_USD
        self.min_edge = config.MIN_EDGE_THRESHOLD
        self.kill_balance = config.KILL_BALANCE_USD
        self.daily_loss_limit_pct = config.DAILY_LOSS_LIMIT_PCT
        self.max_concurrent_positions = config.MAX_CONCURRENT_POSITIONS
        self.loss_cooldown_trades = config.LOSS_COOLDOWN_TRADES
        self.loss_cooldown_seconds = config.LOSS_COOLDOWN_SECONDS

        # State tracking for safeguards
        self._daily_pnl: float = 0.0
        self._daily_reset_date: str = datetime.utcnow().strftime("%Y-%m-%d")
        self._consecutive_losses: int = 0
        self._cooldown_until: float = 0.0  # unix timestamp
        self._open_position_count: int = 0
        self._starting_bankroll: float = 0.0

    def set_bankroll(self, bankroll: float):
        """Set the starting bankroll for daily loss tracking."""
        self._starting_bankroll = bankroll

    def set_open_positions(self, count: int):
        """Update current open position count."""
        self._open_position_count = count

    def record_trade_result(self, pnl: float):
        """Record a resolved trade result for safeguard tracking."""
        # Reset daily PnL if it's a new day
        today = datetime.utcnow().strftime("%Y-%m-%d")
        if today != self._daily_reset_date:
            self._daily_pnl = 0.0
            self._daily_reset_date = today

        self._daily_pnl += pnl

        if pnl < 0:
            self._consecutive_losses += 1
            if self._consecutive_losses >= self.loss_cooldown_trades:
                self._cooldown_until = time.time() + self.loss_cooldown_seconds
                logger.warning(
                    f"COOLDOWN: {self._consecutive_losses} consecutive losses. "
                    f"Pausing for {self.loss_cooldown_seconds}s"
                )
        else:
            self._consecutive_losses = 0

    def should_die(self, balance_usd: float) -> bool:
        """Check if the agent should shut down due to low balance."""
        if balance_usd <= self.kill_balance:
            logger.critical(
                f"KILL SWITCH: Balance ${balance_usd:.2f} <= "
                f"${self.kill_balance:.2f}. Agent dying."
            )
            return True
        return False

    def _check_daily_loss_limit(self) -> bool:
        """Check if daily loss limit has been exceeded."""
        if self._starting_bankroll <= 0:
            return True

        today = datetime.utcnow().strftime("%Y-%m-%d")
        if today != self._daily_reset_date:
            self._daily_pnl = 0.0
            self._daily_reset_date = today
            return True

        max_daily_loss = self._starting_bankroll * self.daily_loss_limit_pct
        if self._daily_pnl < -max_daily_loss:
            logger.warning(
                f"DAILY LOSS LIMIT: Lost ${abs(self._daily_pnl):.2f} today "
                f"(limit: ${max_daily_loss:.2f}). Blocking trades."
            )
            return False
        return True

    def _check_cooldown(self) -> bool:
        """Check if we're in a loss cooldown period."""
        if time.time() < self._cooldown_until:
            remaining = int(self._cooldown_until - time.time())
            logger.info(f"COOLDOWN: {remaining}s remaining after {self._consecutive_losses} losses")
            return False
        return True

    def _check_max_positions(self) -> bool:
        """Check if we've hit the max concurrent positions."""
        if self._open_position_count >= self.max_concurrent_positions:
            logger.info(
                f"MAX POSITIONS: {self._open_position_count} open "
                f"(limit: {self.max_concurrent_positions}). Blocking new trades."
            )
            return False
        return True

    def passes_filters(self, signal: Signal) -> bool:
        """Check if a signal meets minimum requirements to trade."""
        # Safeguard checks
        if not self._check_daily_loss_limit():
            return False
        if not self._check_cooldown():
            return False
        if not self._check_max_positions():
            return False

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

        # Cap at max position size (both pct and absolute USD)
        max_size_pct = bankroll * self.max_position_pct
        max_size = min(max_size_pct, self.max_position_usd)
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
