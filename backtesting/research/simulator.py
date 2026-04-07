"""Strategy simulation engine for autoresearch.

Provides simulate_strategy() for running check functions over
precomputed markets, plus build_strategy_grid() which returns
the full combinatorial grid of strategy configurations.
"""
from __future__ import annotations

from dataclasses import dataclass

from backtesting.eval.evaluator import find_first_trade
from backtesting.precompute import PrecomputedMarket
from strategies.registry import build_strategy_grid as build_registry_strategy_grid


@dataclass
class StrategyResult:
    name: str
    params: dict
    train_trades: int
    train_wins: int
    train_pnl: float
    test_trades: int
    test_wins: int
    test_pnl: float
    avg_entry: float
    avg_profit_per_trade: float

    @property
    def train_win_rate(self) -> float:
        return self.train_wins / self.train_trades if self.train_trades else 0

    @property
    def test_win_rate(self) -> float:
        return self.test_wins / self.test_trades if self.test_trades else 0

    @property
    def test_pnl_per_trade(self) -> float:
        return self.test_pnl / self.test_trades if self.test_trades else 0


def simulate_strategy(markets: list[PrecomputedMarket], check_fn) -> tuple[int, int, float, float]:
    """Run strategy over precomputed markets.

    check_fn(pm, i) returns:
      - "Up"/"Down": take the trade
      - "SKIP": signal fired but entry too expensive (stop scanning this market)
      - None: signal hasn't fired yet (keep scanning)
    """
    trades = wins = 0
    total_pnl = 0.0
    entry_sum = 0.0

    for pm in markets:
        trade = find_first_trade(pm, check_fn)
        if trade is None:
            continue
        trades += 1
        wins += int(trade.won)
        total_pnl += trade.pnl
        entry_sum += trade.entry_price

    avg_entry = entry_sum / trades if trades else 0
    return trades, wins, total_pnl, avg_entry


def build_strategy_grid() -> list[tuple[str, dict, object]]:
    """Build strategy configs from the canonical strategy registry."""
    return build_registry_strategy_grid()
