"""Backtesting engine for BTC 5-minute sniper strategy.

Replays historical market windows against the signal engine with a given
strategy config. Outputs performance metrics used by the autoresearch
loop to evaluate strategy mutations.

Supports two modes:
- synthetic: uses SimulatedWindow with proportional feature modeling
- real: uses actual Binance kline data from historical.db
"""
from __future__ import annotations

import logging
import math
from dataclasses import dataclass, field
from pathlib import Path

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
class RealWindow:
    """A historical window backed by actual Binance kline data."""

    market_id: str
    slug: str
    resolved_direction: str
    up_price: float
    down_price: float
    price_delta: float  # % change from kline open to close
    order_flow_imbalance: float  # (taker_buy_vol / total_vol) * 2 - 1


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

    In production, a strong BTC price move is accompanied by correlated order
    flow imbalance and microprice deviation. We model these as proportional
    to price_delta so the backtest produces realistic signal strength.
    """
    engine = BayesianSignalEngine(cfg.signal)

    engine.set_state(
        order_flow_imbalance=price_move_pct * 6.0,
        microprice_deviation=price_move_pct * 4.0,
        price_delta=price_move_pct,
        acceleration=0.0,
    )

    if engine.confident:
        p = engine.p_up if engine.direction == "UP" else engine.p_down
        return engine.direction, p
    return None, 0.5


def _real_signal(cfg: StrategyConfig, window: RealWindow) -> tuple[str | None, float]:
    """Run the signal engine against real kline-derived features."""
    engine = BayesianSignalEngine(cfg.signal)

    engine.set_state(
        order_flow_imbalance=window.order_flow_imbalance,
        microprice_deviation=0.0,
        price_delta=window.price_delta,
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


def _execute_trade(
    cfg: StrategyConfig,
    direction: str,
    p_win: float,
    up_price: float,
    down_price: float,
    market_id: str,
    resolved_direction: str,
    balance: float,
    result: BacktestResult,
) -> float:
    """Place a trade and update result. Returns new balance."""
    token_price = up_price if direction == "UP" else down_price

    if token_price > cfg.execution.max_entry_price:
        return balance

    size_usd = _kelly_size(p_win, token_price, balance, cfg.risk)
    if size_usd <= 0:
        return balance

    shares = size_usd / token_price
    record = PaperTradeRecord(
        trade_id=market_id,
        market_id=market_id,
        direction=direction,
        token_price=token_price,
        size_usd=size_usd,
        shares=shares,
    )
    res = resolve_paper_trade(record, resolved_direction)

    balance += res.pnl_usd
    result.num_trades += 1
    if res.won:
        result.wins += 1
    else:
        result.losses += 1
    result.total_pnl += res.pnl_usd
    result.trades.append({
        "market_id": market_id,
        "direction": direction,
        "resolved": resolved_direction,
        "won": res.won,
        "pnl": res.pnl_usd,
        "size_usd": size_usd,
        "token_price": token_price,
        "p_win": p_win,
        "balance_after": balance,
    })
    return balance


def run_backtest(
    cfg: StrategyConfig,
    windows: list[SimulatedWindow],
    starting_balance: float = 100.0,
) -> BacktestResult:
    """Run the strategy against a list of synthetic windows."""
    result = BacktestResult(starting_balance=starting_balance, ending_balance=starting_balance)
    balance = starting_balance
    peak_balance = starting_balance

    for window in windows:
        direction, p_win = _simulate_signal(cfg, window.price_move_pct)
        if direction is None:
            continue

        balance = _execute_trade(
            cfg, direction, p_win,
            window.up_price, window.down_price,
            window.market_id, window.resolved_direction,
            balance, result,
        )
        peak_balance = max(peak_balance, balance)
        drawdown = (peak_balance - balance) / peak_balance if peak_balance > 0 else 0
        result.max_drawdown = max(result.max_drawdown, drawdown)

    result.ending_balance = balance
    return result


def run_backtest_real(
    cfg: StrategyConfig,
    windows: list[RealWindow],
    starting_balance: float = 100.0,
) -> BacktestResult:
    """Run the strategy against real historical windows from Binance kline data."""
    result = BacktestResult(starting_balance=starting_balance, ending_balance=starting_balance)
    balance = starting_balance
    peak_balance = starting_balance

    for window in windows:
        direction, p_win = _real_signal(cfg, window)
        if direction is None:
            continue

        balance = _execute_trade(
            cfg, direction, p_win,
            window.up_price, window.down_price,
            window.market_id, window.resolved_direction,
            balance, result,
        )
        peak_balance = max(peak_balance, balance)
        drawdown = (peak_balance - balance) / peak_balance if peak_balance > 0 else 0
        result.max_drawdown = max(result.max_drawdown, drawdown)

    result.ending_balance = balance
    return result


def load_real_windows(db_path: Path | None = None) -> list[RealWindow]:
    """Load RealWindow objects from historical.db."""
    from backtesting.historical_data import load_windows, DB_PATH

    path = db_path or DB_PATH
    raw = load_windows(path)
    return [
        RealWindow(
            market_id=w["market_id"],
            slug=w["slug"],
            resolved_direction=w["resolved_direction"],
            up_price=w["up_price"],
            down_price=w["down_price"],
            price_delta=w["price_delta"],
            order_flow_imbalance=w["order_flow_imbalance"],
        )
        for w in raw
    ]
