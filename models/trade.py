"""Trade data models: signals, trades, positions, and results."""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum

from models.market import Outcome


class Side(str, Enum):
    BUY = "BUY"
    SELL = "SELL"


class SignalSource(str, Enum):
    WEATHER = "weather"
    ARBITRAGE = "arbitrage"
    COPY_TRADING = "copy_trading"
    CLAUDE_FAIR_VALUE = "claude_fair_value"


@dataclass
class Signal:
    """A trading signal produced by a strategy."""

    market_id: str
    market_question: str
    outcome: Outcome  # YES or NO
    side: Side  # BUY or SELL
    source: SignalSource
    # Estimated edge
    fair_value: float  # our estimated probability (0-1)
    market_price: float  # current market price (0-1)
    edge: float  # fair_value - market_price (positive = underpriced)
    confidence: float  # 0-1, how confident we are in the estimate
    # Reasoning
    reasoning: str = ""
    timestamp: datetime = field(default_factory=datetime.utcnow)
    # Optional token IDs (for weather/event markets not in standard cache)
    yes_token_id: str = ""
    no_token_id: str = ""

    @property
    def edge_pct(self) -> float:
        return abs(self.edge) * 100


@dataclass
class Trade:
    """An executed (or simulated) trade."""

    id: str
    signal: Signal
    # Execution details
    size_usd: float
    price: float
    outcome: Outcome
    side: Side
    # Token
    token_id: str
    # Status
    executed: bool = False
    paper: bool = False
    order_id: str = ""
    timestamp: datetime = field(default_factory=datetime.utcnow)
    # Cost tracking
    api_cost_usd: float = 0.0  # Claude inference cost for this trade


@dataclass
class TradeResult:
    """The resolved outcome of a trade."""

    trade_id: str
    market_id: str
    resolved: bool = False
    won: bool = False
    pnl_usd: float = 0.0
    resolved_at: datetime | None = None


@dataclass
class Position:
    """An open position."""

    market_id: str
    market_question: str
    outcome: Outcome
    token_id: str
    size: float  # number of shares
    avg_price: float  # average entry price
    current_price: float = 0.0
    source: SignalSource = SignalSource.WEATHER
    opened_at: datetime = field(default_factory=datetime.utcnow)

    @property
    def cost_basis(self) -> float:
        return self.size * self.avg_price

    @property
    def current_value(self) -> float:
        return self.size * self.current_price

    @property
    def unrealized_pnl(self) -> float:
        return self.current_value - self.cost_basis
