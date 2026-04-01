"""Backtesting engine for BTC 5-minute sniper strategy.

Replays historical market windows against the signal engine with a given
strategy config. Outputs performance metrics used by the autoresearch
loop to evaluate strategy mutations.
"""
from __future__ import annotations

import logging
import math
from dataclasses import dataclass, field

from core.btc_resolution import PaperTradeRecord, resolve_paper_trade
from strategies.btc_sniper import BayesianSignalEngine
from strategies.strategy_config import StrategyConfig, RiskConfig

logger = logging.getLogger(__name__)


@dataclass
class SimulatedWindow:
    """A historical 5-minute window with known outcome."""

    market_id: str
    resolved_direction: str  # "UP" or "DOWN"
    price_move_pct: float  # BTC % change during the window
    up_price: float = 0.02  # price of UP token at entry time
    down_price: float = 0.98  # price of DOWN token at entry time


@dataclass
class BacktestResult:
    num_trades: int = 0
    wins: int = 0
    losses: int = 0
    total_pnl: float = 0.0
    starting_balance: float = 0.0
    ending_balance: float = 0.0
    max_drawdown: float = 0.0
    trades: list[dict] = field(default_factory=list)

    @property
    def win_rate(self) -> float:
        if self.num_trades == 0:
            return 0.0
        return self.wins / self.num_trades

    @property
    def ev_per_trade(self) -> float:
        if self.num_trades == 0:
            return 0.0
        return self.total_pnl / self.num_trades

    @property
    def sharpe(self) -> float:
        if len(self.trades) < 2:
            return 0.0
        pnls = [t["pnl"] for t in self.trades]
        mean = sum(pnls) / len(pnls)
        variance = sum((p - mean) ** 2 for p in pnls) / (len(pnls) - 1)
        std = math.sqrt(variance) if variance > 0 else 0.0
        if std == 0:
            return 0.0
        return mean / std


def _simulate_signal(cfg: StrategyConfig, price_move_pct: float) -> tuple[str | None, float]:
    """Simulate the Bayesian signal engine for a window with a known price move.

    Returns (direction, p_confident_side) or (None, 0.5) if no signal.
    """
    engine = BayesianSignalEngine(cfg.signal)

    # Simulate 20 incremental price updates during the window
    step = price_move_pct / 20.0
    for _ in range(20):
        engine.update(
            order_flow_imbalance=0.0,
            microprice_deviation=0.0,
            price_delta=step,
            acceleration=0.0,
        )

    if engine.confident:
        p = engine.p_up if engine.direction == "UP" else engine.p_down
        return engine.direction, p
    return None, 0.5


def _kelly_size(p_win: float, token_price: float, bankroll: float, risk_cfg: RiskConfig) -> float:
    """Inline Kelly sizing matching core/risk.py logic."""
    if token_price <= 0 or token_price >= 1 or bankroll <= 0 or p_win <= 0:
        return 0.0
    edge = p_win - token_price
    if edge <= 0:
        return 0.0
    kelly_fraction = edge / (1.0 - token_price)
    adjusted = kelly_fraction * risk_cfg.kelly_multiplier
    if token_price <= 0.05:
        adjusted *= risk_cfg.cheap_token_multiplier
    max_by_pct = bankroll * risk_cfg.max_position_pct
    max_size = min(max_by_pct, risk_cfg.max_position_usd)
    position = min(adjusted * bankroll, max_size)
    if position < 1.0:
        return 0.0
    return round(position, 2)


def run_backtest(
    cfg: StrategyConfig,
    windows: list[SimulatedWindow],
    starting_balance: float = 100.0,
) -> BacktestResult:
    """Run the strategy against a list of historical windows."""
    result = BacktestResult(starting_balance=starting_balance, ending_balance=starting_balance)
    balance = starting_balance
    peak_balance = starting_balance

    for window in windows:
        direction, p_win = _simulate_signal(cfg, window.price_move_pct)
        if direction is None:
            continue

        if direction == "UP":
            token_price = window.up_price
        else:
            token_price = window.down_price

        if token_price > cfg.execution.max_entry_price:
            continue

        size_usd = _kelly_size(p_win, token_price, balance, cfg.risk)
        if size_usd <= 0:
            continue

        shares = size_usd / token_price
        record = PaperTradeRecord(
            trade_id=window.market_id,
            market_id=window.market_id,
            direction=direction,
            token_price=token_price,
            size_usd=size_usd,
            shares=shares,
        )
        res = resolve_paper_trade(record, window.resolved_direction)

        balance += res.pnl_usd
        result.num_trades += 1
        if res.won:
            result.wins += 1
        else:
            result.losses += 1
        result.total_pnl += res.pnl_usd
        result.trades.append({
            "market_id": window.market_id,
            "direction": direction,
            "resolved": window.resolved_direction,
            "won": res.won,
            "pnl": res.pnl_usd,
            "size_usd": size_usd,
            "token_price": token_price,
            "p_win": p_win,
            "balance_after": balance,
        })

        peak_balance = max(peak_balance, balance)
        drawdown = (peak_balance - balance) / peak_balance if peak_balance > 0 else 0
        result.max_drawdown = max(result.max_drawdown, drawdown)

    result.ending_balance = balance
    return result
