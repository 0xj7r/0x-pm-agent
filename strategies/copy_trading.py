"""Whale copy-trading strategy.

Monitors high win-rate wallets on Polymarket and mirrors their trades.
Edge: smart money moves before the crowd reprices.
"""

from __future__ import annotations

import logging
from collections import defaultdict
from dataclasses import dataclass, field
from datetime import datetime, timedelta

import httpx

from config import Config
from models.market import Market, Outcome
from models.trade import Side, Signal, SignalSource
from strategies.base import Strategy

logger = logging.getLogger(__name__)

# Polymarket leaderboard / profile API
PROFILE_URL = "https://gamma-api.polymarket.com/profiles"
ACTIVITY_URL = "https://gamma-api.polymarket.com/activity"


@dataclass
class TrackedWallet:
    """A wallet being tracked for copy-trading signals."""

    address: str
    alias: str = ""
    win_rate: float = 0.0
    total_pnl: float = 0.0
    num_trades: int = 0
    last_checked: datetime = field(default_factory=datetime.utcnow)


@dataclass
class WalletTrade:
    """A single trade observed from a tracked wallet."""

    wallet: str
    market_id: str
    market_question: str
    outcome: str  # "Yes" or "No"
    side: str  # "BUY" or "SELL"
    price: float
    size: float
    timestamp: datetime


@dataclass
class ClusterSignal:
    """A cluster of wallets entering the same position within a time window."""

    market_id: str
    market_question: str
    outcome: str
    wallets: list[str] = field(default_factory=list)
    combined_pnl: float = 0.0
    combined_volume: float = 0.0
    avg_win_rate: float = 0.0
    first_trade_at: datetime = field(default_factory=datetime.utcnow)

    @property
    def wallet_count(self) -> int:
        """Number of unique wallets in this cluster."""
        return len(self.wallets)

    @property
    def conviction_score(self) -> float:
        """Score 0-100 based on wallet count, combined PnL, and volume."""
        count_score = min(60, max(0, (self.wallet_count - 4) * 10 + 30))
        pnl_score = min(20, max(0, self.combined_pnl / 500 * 20))
        vol_score = min(20, max(0, self.combined_volume / 10000 * 20))
        return min(100, count_score + pnl_score + vol_score)


class CopyTradingStrategy(Strategy):
    """Copy trades from high-performing wallets with cluster detection."""

    # Minimum wallets entering same position within CLUSTER_WINDOW to trigger cluster signal
    CLUSTER_MIN_WALLETS: int = 5
    CLUSTER_WINDOW: timedelta = timedelta(hours=1)

    def __init__(self, tracked_wallets: list[str] | None = None) -> None:
        self._http = httpx.AsyncClient(timeout=30.0)
        self.tracked_wallets: dict[str, TrackedWallet] = {}
        if tracked_wallets:
            for addr in tracked_wallets:
                self.tracked_wallets[addr] = TrackedWallet(address=addr)
        self._seen_trades: set[str] = set()
        self._recent_wallet_trades: dict[str, list[tuple[str, datetime, float]]] = defaultdict(list)

    @property
    def name(self) -> str:
        """Human-readable strategy name."""
        return "copy_trading"

    def _load_wallets_from_config(self) -> None:
        """Load wallet addresses from COPY_TRADING_WALLETS config var.

        Expects a comma-separated string of Ethereum addresses.
        """
        config = Config()
        raw = config.COPY_TRADING_WALLETS.strip()
        if not raw:
            return

        addresses = [addr.strip() for addr in raw.split(",") if addr.strip()]
        for addr in addresses:
            if addr not in self.tracked_wallets:
                self.tracked_wallets[addr] = TrackedWallet(
                    address=addr,
                    alias=addr[:8],
                )
        if addresses:
            logger.info(f"Loaded {len(addresses)} wallets from COPY_TRADING_WALLETS config")

    async def discover_whales(self, min_win_rate: float = 0.65, limit: int = 20) -> None:
        """Discover high-performing wallets from Polymarket leaderboard.

        Queries the Gamma API for top traders by PnL and filters by win rate.
        Falls back gracefully if the leaderboard endpoint is unavailable.
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

        except httpx.HTTPStatusError as e:
            logger.warning(
                f"Leaderboard API returned {e.response.status_code} — "
                f"whale discovery unavailable, using configured wallets only"
            )
        except Exception as e:
            logger.warning(f"Whale discovery failed (non-fatal): {e}")

    async def get_recent_trades(self, wallet: str) -> list[WalletTrade]:
        """Get recent trades for a specific wallet from the activity API."""
        try:
            resp = await self._http.get(
                ACTIVITY_URL,
                params={"address": wallet, "limit": 20},
            )
            resp.raise_for_status()
            activities = resp.json()

            trades: list[WalletTrade] = []
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

    def _record_for_clustering(self, wallet_addr: str, trade: WalletTrade) -> None:
        """Record a trade for cluster detection."""
        key = f"{trade.market_id}:{trade.outcome}"
        self._recent_wallet_trades[key].append(
            (wallet_addr, trade.timestamp, trade.size * trade.price)
        )
        cutoff = datetime.utcnow() - self.CLUSTER_WINDOW
        self._recent_wallet_trades[key] = [
            (w, t, v) for w, t, v in self._recent_wallet_trades[key] if t >= cutoff
        ]

    def _detect_clusters(self, market_lookup: dict[str, Market]) -> list[Signal]:
        """Detect wallet clusters and generate high-conviction signals."""
        signals: list[Signal] = []
        cutoff = datetime.utcnow() - self.CLUSTER_WINDOW

        for key, entries in self._recent_wallet_trades.items():
            recent = [(w, t, v) for w, t, v in entries if t >= cutoff]
            unique_wallets = list({w for w, _, _ in recent})

            if len(unique_wallets) < self.CLUSTER_MIN_WALLETS:
                continue

            market_id, outcome_str = key.rsplit(":", 1)
            market = market_lookup.get(market_id)
            if not market or not market.active:
                continue

            outcome = Outcome.YES if outcome_str == "Yes" else Outcome.NO
            market_price = market.yes_price if outcome == Outcome.YES else market.no_price

            combined_pnl = sum(
                self.tracked_wallets[w].total_pnl
                for w in unique_wallets
                if w in self.tracked_wallets
            )
            combined_volume = sum(v for _, _, v in recent)
            rated = [
                self.tracked_wallets[w].win_rate
                for w in unique_wallets
                if w in self.tracked_wallets
            ]
            avg_win_rate = sum(rated) / len(rated) if rated else 0.0

            cluster = ClusterSignal(
                market_id=market_id,
                market_question=market.question,
                outcome=outcome_str,
                wallets=unique_wallets,
                combined_pnl=combined_pnl,
                combined_volume=combined_volume,
                avg_win_rate=avg_win_rate,
            )

            conviction = cluster.conviction_score
            estimated_edge = avg_win_rate - 0.5 + (conviction / 100 * 0.1)

            signal = Signal(
                market_id=market_id,
                market_question=market.question,
                outcome=outcome,
                side=Side.BUY,
                source=SignalSource.COPY_TRADING,
                fair_value=min(0.99, market_price + estimated_edge),
                market_price=market_price,
                edge=estimated_edge,
                confidence=min(conviction / 100, 0.95),
                reasoning=(
                    f"CLUSTER: {cluster.wallet_count} wallets entered {outcome_str} "
                    f"within 1h | Conviction: {conviction:.0f}/100 | "
                    f"Combined PnL: ${combined_pnl:+,.0f} | "
                    f"Volume: ${combined_volume:,.0f} | "
                    f"Avg win rate: {avg_win_rate:.0%}"
                ),
            )

            logger.info(
                f"Cluster signal: {cluster.wallet_count} wallets on "
                f"'{market.question[:50]}' {outcome_str} | "
                f"Conviction: {conviction:.0f}/100"
            )
            signals.append(signal)

        return signals

    async def evaluate(self, markets: list[Market]) -> list[Signal]:
        """Check tracked wallets for new trades and generate copy signals."""
        # Load configured wallets first
        if not self.tracked_wallets:
            self._load_wallets_from_config()

        # Auto-discover whales if still none tracked
        if not self.tracked_wallets:
            await self.discover_whales()

        if not self.tracked_wallets:
            logger.warning("No wallets to track (discovery failed and none configured)")
            return []

        signals: list[Signal] = []
        market_lookup: dict[str, Market] = {m.id: m for m in markets}

        for address, wallet_info in self.tracked_wallets.items():
            trades = await self.get_recent_trades(address)

            for trade in trades:
                trade_key = f"{trade.wallet}:{trade.market_id}:{trade.timestamp.isoformat()}"
                if trade_key in self._seen_trades:
                    continue
                self._seen_trades.add(trade_key)

                if trade.side != "BUY":
                    continue

                self._record_for_clustering(address, trade)

                market = market_lookup.get(trade.market_id)
                if not market or not market.active:
                    continue

                outcome = Outcome.YES if trade.outcome == "Yes" else Outcome.NO
                market_price = market.yes_price if outcome == Outcome.YES else market.no_price

                estimated_edge = wallet_info.win_rate - 0.5

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

        cluster_signals = self._detect_clusters(market_lookup)
        signals.extend(cluster_signals)

        return signals

    @staticmethod
    def _parse_activity_timestamp(raw_timestamp: str | None) -> datetime:
        """Parse an ISO timestamp string, falling back to now."""
        if not raw_timestamp:
            return datetime.utcnow()
        try:
            return datetime.fromisoformat(raw_timestamp.replace("Z", "+00:00"))
        except (TypeError, ValueError):
            return datetime.utcnow()

    @staticmethod
    def _safe_float(value: object, default: float) -> float:
        """Safely convert a value to float with a fallback."""
        try:
            return float(value)  # type: ignore[arg-type]
        except (TypeError, ValueError):
            return default
