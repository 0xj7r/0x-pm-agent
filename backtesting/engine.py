"""Backtesting engine.

Replays historical market data against strategies to validate edge
before risking real money.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from datetime import datetime

from backtesting.data import HistoricalDataClient, HistoricalSnapshot, MarketResolution
from core.risk import RiskManager
from config import Config
from models.market import Market, MarketCategory, OrderBook
from models.trade import Signal

logger = logging.getLogger(__name__)


@dataclass
class BacktestTrade:
    market_id: str
    question: str
    strategy: str
    outcome: str
    side: str
    entry_price: float
    size_usd: float
    edge: float
    confidence: float
    resolved_to: str
    won: bool
    pnl_usd: float


@dataclass
class BacktestResult:
    """Results of a backtest run."""

    strategy: str
    start_date: datetime
    end_date: datetime
    initial_balance: float
    final_balance: float
    trades: list[BacktestTrade] = field(default_factory=list)

    @property
    def num_trades(self) -> int:
        return len(self.trades)

    @property
    def wins(self) -> int:
        return sum(1 for t in self.trades if t.won)

    @property
    def losses(self) -> int:
        return self.num_trades - self.wins

    @property
    def win_rate(self) -> float:
        return self.wins / self.num_trades if self.num_trades > 0 else 0.0

    @property
    def total_pnl(self) -> float:
        return sum(t.pnl_usd for t in self.trades)

    @property
    def avg_pnl(self) -> float:
        return self.total_pnl / self.num_trades if self.num_trades > 0 else 0.0

    @property
    def max_drawdown(self) -> float:
        """Calculate maximum drawdown from peak."""
        if not self.trades:
            return 0.0
        balance = self.initial_balance
        peak = balance
        max_dd = 0.0
        for trade in self.trades:
            balance += trade.pnl_usd
            peak = max(peak, balance)
            dd = (peak - balance) / peak if peak > 0 else 0
            max_dd = max(max_dd, dd)
        return max_dd

    def summary(self) -> str:
        return (
            f"\n{'='*60}\n"
            f"BACKTEST RESULTS: {self.strategy}\n"
            f"{'='*60}\n"
            f"Period: {self.start_date.date()} to {self.end_date.date()}\n"
            f"Initial balance: ${self.initial_balance:.2f}\n"
            f"Final balance: ${self.final_balance:.2f}\n"
            f"Total PnL: ${self.total_pnl:+.2f} ({self.total_pnl/self.initial_balance*100:+.1f}%)\n"
            f"Trades: {self.num_trades} ({self.wins}W / {self.losses}L)\n"
            f"Win rate: {self.win_rate:.1%}\n"
            f"Avg PnL/trade: ${self.avg_pnl:+.2f}\n"
            f"Max drawdown: {self.max_drawdown:.1%}\n"
            f"{'='*60}"
        )


class BacktestEngine:
    def __init__(self, config: Config):
        self.config = config
        self.data_client = HistoricalDataClient()
        self.risk = RiskManager(config)

    async def run(
        self,
        strategy,
        initial_balance: float = 100.0,
        max_markets: int = 200,
    ) -> BacktestResult:
        """Run a backtest for a single strategy against resolved markets.

        Flow:
        1. Fetch resolved markets
        2. For each market, get historical price at a point BEFORE resolution
        3. Run strategy evaluation
        4. Simulate trades and check against actual resolution
        """
        logger.info(f"Starting backtest for strategy: {strategy.name}")

        # Fetch resolved markets
        resolutions = await self.data_client.get_resolved_markets(limit=max_markets)
        logger.info(f"Got {len(resolutions)} resolved markets for backtesting")

        balance = initial_balance
        trades: list[BacktestTrade] = []
        dates = []

        for resolution in resolutions:
            # Get historical price snapshots
            snapshots = await self.data_client.get_price_history(resolution.market_id)
            if not snapshots:
                continue

            # Use a snapshot from before resolution (e.g., midpoint of history)
            mid_idx = len(snapshots) // 2
            snapshot = snapshots[mid_idx]

            # Build a Market object from the snapshot
            market = Market(
                id=resolution.market_id,
                question=resolution.question,
                description="",
                category=MarketCategory.OTHER,
                end_date=resolution.resolved_at,
                active=True,
                yes_price=snapshot.yes_price,
                no_price=snapshot.no_price,
                yes_book=snapshot.yes_book,
                no_book=snapshot.no_book,
            )

            # Run strategy
            try:
                signals = await strategy.evaluate([market])
            except Exception as e:
                logger.debug(f"Strategy failed on {resolution.market_id}: {e}")
                continue

            # Process signals
            for signal in signals:
                if not self.risk.passes_filters(signal):
                    continue

                size_usd = self.risk.size_position(signal, balance)
                if size_usd <= 0:
                    continue

                # Determine if trade won
                won = (
                    (signal.outcome.value == resolution.resolved_to and signal.side.value == "BUY")
                    or (signal.outcome.value != resolution.resolved_to and signal.side.value == "SELL")
                )

                # Calculate PnL
                if won:
                    pnl = size_usd * (1.0 / signal.market_price - 1.0)
                else:
                    pnl = -size_usd

                balance += pnl
                dates.append(snapshot.timestamp)

                bt_trade = BacktestTrade(
                    market_id=resolution.market_id,
                    question=resolution.question,
                    strategy=strategy.name,
                    outcome=signal.outcome.value,
                    side=signal.side.value,
                    entry_price=signal.market_price,
                    size_usd=size_usd,
                    edge=signal.edge,
                    confidence=signal.confidence,
                    resolved_to=resolution.resolved_to,
                    won=won,
                    pnl_usd=pnl,
                )
                trades.append(bt_trade)

                logger.info(
                    f"BT: {'WIN' if won else 'LOSS'} ${pnl:+.2f} | "
                    f"{signal.outcome.value} @ {signal.market_price:.3f} | "
                    f"Edge: {signal.edge:+.3f} | "
                    f"Balance: ${balance:.2f}"
                )

        result = BacktestResult(
            strategy=strategy.name,
            start_date=min(dates) if dates else datetime.utcnow(),
            end_date=max(dates) if dates else datetime.utcnow(),
            initial_balance=initial_balance,
            final_balance=balance,
            trades=trades,
        )

        logger.info(result.summary())
        return result

    async def close(self):
        await self.data_client.close()
