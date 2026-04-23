"""Market data models: markets, order books, and price points."""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum


class MarketCategory(str, Enum):
    WEATHER = "weather"
    SPORTS = "sports"
    CRYPTO = "crypto"
    POLITICS = "politics"
    OTHER = "other"


class Outcome(str, Enum):
    YES = "Yes"
    NO = "No"


@dataclass
class PricePoint:
    price: float  # 0.0 - 1.0
    size: float  # quantity available


@dataclass
class OrderBook:
    bids: list[PricePoint] = field(default_factory=list)  # buy orders
    asks: list[PricePoint] = field(default_factory=list)  # sell orders

    @property
    def best_bid(self) -> float | None:
        return self.bids[0].price if self.bids else None

    @property
    def best_ask(self) -> float | None:
        return self.asks[0].price if self.asks else None

    @property
    def spread(self) -> float | None:
        if self.best_bid is not None and self.best_ask is not None:
            return self.best_ask - self.best_bid
        return None


@dataclass
class Market:
    id: str
    question: str
    description: str
    category: MarketCategory
    end_date: datetime | None
    active: bool
    # Token IDs for YES and NO outcomes
    yes_token_id: str = ""
    no_token_id: str = ""
    # Current prices
    yes_price: float = 0.5
    no_price: float = 0.5
    # Order books
    yes_book: OrderBook = field(default_factory=OrderBook)
    no_book: OrderBook = field(default_factory=OrderBook)
    # Volume
    volume: float = 0.0
    liquidity: float = 0.0
    # Raw data from API
    raw: dict = field(default_factory=dict)

    @property
    def complement_cost(self) -> float:
        """Cost to buy both YES and NO (should be ~$1 if no arb)."""
        yes_ask = self.yes_book.best_ask
        no_ask = self.no_book.best_ask
        if yes_ask is not None and no_ask is not None:
            return yes_ask + no_ask
        # Fallback for scans that only include top-level yes/no prices.
        if yes_ask is None and no_ask is None:
            return self.yes_price + self.no_price
        return 1.0

    @property
    def arb_opportunity(self) -> float:
        """Profit from buying YES + NO if complement < $1. Returns 0 if no arb."""
        cost = self.complement_cost
        return max(0.0, 1.0 - cost)


@dataclass
class MarketWindow:
    """A single 5-minute Bitcoin Up/Down market window."""

    market_id: str
    question: str
    start_time: datetime
    end_time: datetime
    up_token_id: str
    down_token_id: str
    up_price: float = 0.5
    down_price: float = 0.5
    slug: str = ""
    price_to_beat: float | None = None
    condition_id: str = ""
    event_id: str = ""

    def time_remaining(self, now: datetime) -> float:
        return max(0.0, (self.end_time - now).total_seconds())

    def elapsed_seconds(self, now: datetime) -> float:
        return max(0.0, (now - self.start_time).total_seconds())

    def is_active(self, now: datetime) -> bool:
        return self.start_time <= now < self.end_time
