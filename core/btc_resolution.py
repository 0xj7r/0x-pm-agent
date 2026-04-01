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


def resolve_paper_trade(trade: PaperTradeRecord, resolved_direction: str) -> ResolutionResult:
    """Calculate P&L for a resolved paper trade.

    If direction matches, payout = shares * $1.00 (each share resolves to $1).
    If direction doesn't match, payout = $0 (total loss of cost basis).
    """
    won = trade.direction == resolved_direction

    if won:
        payout = trade.shares * 1.0
        pnl = payout - trade.size_usd
    else:
        pnl = -trade.size_usd

    return ResolutionResult(
        trade_id=trade.trade_id,
        market_id=trade.market_id,
        won=won,
        pnl_usd=pnl,
        resolved_direction=resolved_direction,
        resolved_at=datetime.now(timezone.utc),
    )
