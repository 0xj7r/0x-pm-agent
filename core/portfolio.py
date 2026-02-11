"""Portfolio tracking: balance, positions, P&L, API cost accounting."""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from datetime import datetime

from models.trade import Position, Side, Signal, Trade, TradeResult

logger = logging.getLogger(__name__)


@dataclass
class PortfolioSnapshot:
    balance_usd: float
    total_invested: float
    unrealized_pnl: float
    realized_pnl: float
    total_api_cost: float
    num_open_positions: int
    num_trades: int
    win_rate: float
    timestamp: datetime = field(default_factory=datetime.utcnow)

    @property
    def net_pnl(self) -> float:
        return self.realized_pnl - self.total_api_cost

    @property
    def total_value(self) -> float:
        return self.balance_usd + self.total_invested + self.unrealized_pnl


class Portfolio:
    def __init__(self, initial_balance: float = 0.0):
        self.balance_usd = initial_balance
        self.positions: dict[str, Position] = {}  # market_id -> Position
        self.trades: list[Trade] = []
        self.results: list[TradeResult] = []
        self.total_api_cost = 0.0

    def record_trade(self, trade: Trade):
        """Record a new trade and update positions."""
        self.trades.append(trade)

        if trade.paper:
            logger.info(f"[PAPER] Trade recorded: {trade.side.value} {trade.outcome.value} "
                        f"${trade.size_usd:.2f} @ {trade.price:.3f}")
            return

        # Deduct cost from balance
        self.balance_usd -= trade.size_usd

        # Key by (market_id, outcome) to handle YES and NO separately
        pos_key = f"{trade.signal.market_id}:{trade.outcome.value}"
        new_shares = trade.size_usd / trade.price if trade.price > 0 else 0

        if pos_key in self.positions:
            pos = self.positions[pos_key]
            if trade.side == Side.BUY:
                total_cost = pos.cost_basis + trade.size_usd
                total_size = pos.size + new_shares
                pos.avg_price = total_cost / total_size if total_size > 0 else 0
                pos.size = total_size
            else:
                # SELL reduces position
                pos.size = max(0, pos.size - new_shares)
                if pos.size == 0:
                    del self.positions[pos_key]
        else:
            self.positions[pos_key] = Position(
                market_id=trade.signal.market_id,
                market_question=trade.signal.market_question,
                outcome=trade.outcome,
                token_id=trade.token_id,
                size=new_shares,
                avg_price=trade.price,
                current_price=trade.price,
                source=trade.signal.source,
            )

        logger.info(
            f"Trade recorded: {trade.side.value} {trade.outcome.value} "
            f"${trade.size_usd:.2f} @ {trade.price:.3f} | "
            f"Balance: ${self.balance_usd:.2f}"
        )

    def record_api_cost(self, cost_usd: float):
        """Track Claude API inference costs."""
        self.total_api_cost += cost_usd
        logger.debug(f"API cost: ${cost_usd:.4f} (total: ${self.total_api_cost:.4f})")

    def record_result(self, result: TradeResult):
        """Record a resolved trade outcome."""
        self.results.append(result)
        if not result.resolved:
            return

        self.balance_usd += result.pnl_usd
        # Remove closed position
        self.positions.pop(result.market_id, None)

        status = "WON" if result.won else "LOST"
        logger.info(
            f"Trade resolved: {status} ${result.pnl_usd:+.2f} | "
            f"Balance: ${self.balance_usd:.2f}"
        )

    def snapshot(self) -> PortfolioSnapshot:
        """Get current portfolio state."""
        total_invested = sum(p.cost_basis for p in self.positions.values())
        unrealized = sum(p.unrealized_pnl for p in self.positions.values())
        resolved = [r for r in self.results if r.resolved]
        realized = sum(r.pnl_usd for r in resolved)
        wins = sum(1 for r in resolved if r.won)
        win_rate = wins / len(resolved) if resolved else 0.0

        return PortfolioSnapshot(
            balance_usd=self.balance_usd,
            total_invested=total_invested,
            unrealized_pnl=unrealized,
            realized_pnl=realized,
            total_api_cost=self.total_api_cost,
            num_open_positions=len(self.positions),
            num_trades=len(self.trades),
            win_rate=win_rate,
        )
