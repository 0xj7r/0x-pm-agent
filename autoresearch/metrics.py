"""Canonical scoring and filtering rules for autoresearch."""
from __future__ import annotations


MIN_TRAIN_TRADES = 100
MIN_TEST_TRADES = 30
MIN_SHARPE = 0.5


def compute_sharpe(pnls: list[float]) -> float:
    """Sharpe ratio: mean / std of per-trade PnL. Returns 0 if undefined."""
    if len(pnls) < 2:
        return 0.0
    mean = sum(pnls) / len(pnls)
    variance = sum((p - mean) ** 2 for p in pnls) / (len(pnls) - 1)
    if variance <= 0:
        return 0.0
    std = variance ** 0.5
    return mean / std


def compute_max_drawdown(pnls: list[float]) -> float:
    """Max drawdown from cumulative PnL series. Returns positive number."""
    if not pnls:
        return 0.0
    cumulative = 0.0
    peak = 0.0
    max_dd = 0.0
    for pnl in pnls:
        cumulative += pnl
        if cumulative > peak:
            peak = cumulative
        max_dd = max(max_dd, peak - cumulative)
    return max_dd


def score_trades(pnls: list[float]) -> dict:
    trades = len(pnls)
    if trades == 0:
        return {
            "trades": 0,
            "wins": 0,
            "losses": 0,
            "pnl": 0.0,
            "win_rate": 0.0,
            "pnl_per_trade": 0.0,
            "sharpe": 0.0,
            "max_drawdown": 0.0,
        }
    wins = sum(1 for pnl in pnls if pnl > 0)
    losses = trades - wins
    pnl = sum(pnls)
    return {
        "trades": trades,
        "wins": wins,
        "losses": losses,
        "pnl": round(pnl, 4),
        "win_rate": round(wins / trades, 4),
        "pnl_per_trade": round(pnl / trades, 4),
        "sharpe": round(compute_sharpe(pnls), 4),
        "max_drawdown": round(compute_max_drawdown(pnls), 4),
    }


def is_result_significant(result: dict) -> bool:
    if result.get("train_trades", 0) < MIN_TRAIN_TRADES:
        return False
    if result.get("test_trades", 0) < MIN_TEST_TRADES:
        return False
    if result.get("train_pnl", 0) <= 0:
        return False
    if result.get("test_pnl", 0) <= 0:
        return False
    if result.get("test_sharpe", 0) < MIN_SHARPE:
        return False
    return True


def sort_results(results: list[dict]) -> list[dict]:
    return sorted(results, key=lambda row: row.get("test_sharpe", 0), reverse=True)
