"""Whale copy-trading strategy.

Monitors high win-rate wallets on Polymarket and mirrors their trades.
Edge: smart money moves before the crowd reprices.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from datetime import datetime

import httpx

from models.market import Market, Outcome
from models.trade import Side, Signal, SignalSource
from strategies.base import Strategy

logger = logging.getLogger(__name__)

# Polymarket leaderboard / profile API
PROFILE_URL = "https://gamma-api.polymarket.com/profiles"
ACTIVITY_URL = "https://gamma-api.polymarket.com/activity"


@dataclass
class TrackedWallet:
    address: str
    alias: str = ""
    win_rate: float = 0.0
    total_pnl: float = 0.0
    num_trades: int = 0
    last_checked: datetime = field(default_factory=datetime.utcnow)


@dataclass
class WalletTrade:
    wallet: str
    market_id: str
    market_question: str
    outcome: str  # "Yes" or "No"
    side: str  # "BUY" or "SELL"
    price: float
    size: float
    timestamp: datetime


class CopyTradingStrategy(Strategy):
    def __init__(self, tracked_wallets: list[str] | None = None):
        self._http = httpx.AsyncClient(timeout=30.0)
        # Wallets to track - can be loaded from config or discovered
        self.tracked_wallets: dict[str, TrackedWallet] = {}
        if tracked_wallets:
            for addr in tracked_wallets:
                self.tracked_wallets[addr] = TrackedWallet(address=addr)
        self._seen_trades: set[str] = set()  # dedup

    @property
    def name(self) -> str:
        return "copy_trading"

    async def discover_whales(self, min_win_rate: float = 0.65, limit: int = 20):
        """Discover high-performing wallets from Polymarket leaderboard.

        This queries the Gamma API for top traders by PnL and filters
        by win rate.
        """
        try:
            resp = await self._http.get(
                f"{PROFILE_URL}/leaderboard",
                params={"limit": limit, "sortBy": "pnl"},
            )
            resp.raise_for_status()
            profiles = resp.json()

            for profile in profiles:
                address = profile.get("address", "")
                win_rate = float(profile.get("winRate", 0))
                pnl = float(profile.get("pnl", 0))
                trades = int(profile.get("numTrades", 0))

                if win_rate >= min_win_rate and trades >= 10:
                    self.tracked_wallets[address] = TrackedWallet(
                        address=address,
                        alias=profile.get("username", address[:8]),
                        win_rate=win_rate,
                        total_pnl=pnl,
                        num_trades=trades,
                    )

            logger.info(
                f"Discovered {len(self.tracked_wallets)} whales "
                f"with >{min_win_rate:.0%} win rate"
            )

        except Exception as e:
            logger.error(f"Failed to discover whales: {e}")

    async def get_recent_trades(self, wallet: str) -> list[WalletTrade]:
        """Get recent trades for a specific wallet."""
        try:
            resp = await self._http.get(
                ACTIVITY_URL,
                params={"address": wallet, "limit": 20},
            )
            resp.raise_for_status()
            activities = resp.json()

            trades = []
            for activity in activities:
                if activity.get("type") != "trade":
                    continue

                trade = WalletTrade(
                    wallet=wallet,
                    market_id=activity.get("marketId", ""),
                    market_question=activity.get("question", ""),
                    outcome=activity.get("outcome", "Yes"),
                    side=activity.get("side", "BUY"),
                    price=self._safe_float(activity.get("price"), 0.0),
                    size=self._safe_float(activity.get("size"), 0.0),
                    timestamp=self._parse_activity_timestamp(activity.get("timestamp")),
                )
                trades.append(trade)

            return trades

        except Exception as e:
            logger.error(f"Failed to get trades for {wallet[:8]}...: {e}")
            return []

    async def evaluate(self, markets: list[Market]) -> list[Signal]:
        """Check tracked wallets for new trades and generate copy signals."""
        # Auto-discover whales if none tracked
        if not self.tracked_wallets:
            await self.discover_whales()

        if not self.tracked_wallets:
            logger.warning("No wallets to track")
            return []

        signals = []
        market_lookup = {m.id: m for m in markets}

        for address, wallet_info in self.tracked_wallets.items():
            trades = await self.get_recent_trades(address)

            for trade in trades:
                # Dedup: skip trades we've already seen
                trade_key = f"{trade.wallet}:{trade.market_id}:{trade.timestamp.isoformat()}"
                if trade_key in self._seen_trades:
                    continue
                self._seen_trades.add(trade_key)

                # Only copy BUY trades (not sells/exits)
                if trade.side != "BUY":
                    continue

                # Look up market data
                market = market_lookup.get(trade.market_id)
                if not market or not market.active:
                    continue

                outcome = Outcome.YES if trade.outcome == "Yes" else Outcome.NO
                market_price = market.yes_price if outcome == Outcome.YES else market.no_price

                # Edge estimate based on whale's historical win rate
                estimated_edge = wallet_info.win_rate - 0.5  # edge over random

                signal = Signal(
                    market_id=trade.market_id,
                    market_question=trade.market_question,
                    outcome=outcome,
                    side=Side.BUY,
                    source=SignalSource.COPY_TRADING,
                    fair_value=market_price + estimated_edge,
                    market_price=market_price,
                    edge=estimated_edge,
                    confidence=min(wallet_info.win_rate, 0.8),
                    reasoning=(
                        f"Copy trade from {wallet_info.alias or address[:8]}... | "
                        f"Win rate: {wallet_info.win_rate:.0%} | "
                        f"PnL: ${wallet_info.total_pnl:+,.0f} | "
                        f"They bought {outcome.value} @ {trade.price:.3f}"
                    ),
                )

                logger.info(
                    f"Copy signal: {wallet_info.alias or address[:8]} bought "
                    f"{outcome.value} on '{trade.market_question[:50]}' "
                    f"@ {trade.price:.3f}"
                )
                signals.append(signal)

        return signals

    @staticmethod
    def _parse_activity_timestamp(raw_timestamp: str | None) -> datetime:
        if not raw_timestamp:
            return datetime.utcnow()
        try:
            return datetime.fromisoformat(raw_timestamp.replace("Z", "+00:00"))
        except (TypeError, ValueError):
            return datetime.utcnow()

    @staticmethod
    def _safe_float(value, default: float) -> float:
        try:
            return float(value)
        except (TypeError, ValueError):
            return default
