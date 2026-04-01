"""Resolution checker for BTC paper trades.

Polls Gamma API for resolved markets and calculates P&L
for paper trades recorded in the event log.
"""
from __future__ import annotations

import logging
from dataclasses import dataclass
from datetime import datetime, timezone

logger = logging.getLogger(__name__)


@dataclass
class PaperTradeRecord:
    trade_id: str
    market_id: str
    direction: str  # "UP" or "DOWN"
    token_price: float
    size_usd: float
    shares: float


@dataclass
class ResolutionResult:
    trade_id: str
    market_id: str
    won: bool
    pnl_usd: float
    resolved_direction: str
    resolved_at: datetime | None = None


TAKER_FEE_RATE = 0.072


def _taker_fee(price: float, size_usd: float) -> float:
    """Polymarket dynamic taker fee: C * 0.072 * p * (1-p)."""
    return size_usd * TAKER_FEE_RATE * price * (1.0 - price)


def resolve_paper_trade(trade: PaperTradeRecord, resolved_direction: str) -> ResolutionResult:
    """Calculate P&L for a resolved paper trade, including taker fees.

    If direction matches, payout = shares * $1.00 minus entry fee.
    If direction doesn't match, payout = $0, loss = cost + entry fee.
    """
    won = trade.direction == resolved_direction
    entry_fee = _taker_fee(trade.token_price, trade.size_usd)

    if won:
        payout = trade.shares * 1.0
        pnl = payout - trade.size_usd - entry_fee
    else:
        pnl = -trade.size_usd - entry_fee

    return ResolutionResult(
        trade_id=trade.trade_id,
        market_id=trade.market_id,
        won=won,
        pnl_usd=pnl,
        resolved_direction=resolved_direction,
        resolved_at=datetime.now(timezone.utc),
    )
