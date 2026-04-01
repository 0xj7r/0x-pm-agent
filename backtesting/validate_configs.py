"""Quick validation: test multiple configs against real snapshot data."""
from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from backtesting.btc_backtest import run_backtest_snapshots
from strategies.strategy_config import load_strategy_config


def test_config(label: str, cfg_path: str, overrides: dict) -> None:
    cfg = load_strategy_config(cfg_path)
    for key, val in overrides.items():
        parts = key.split(".")
        obj = cfg
        for p in parts[:-1]:
            obj = getattr(obj, p)
        setattr(obj, parts[-1], val)
    r = run_backtest_snapshots(cfg)
    trades_str = ""
    if r.trades:
        for t in r.trades[:3]:
            w = "W" if t["won"] else "L"
            trades_str += f"\n    {t['strategy']} {t['direction']} @{t['token_price']:.3f} -> {w} ${t['pnl']:+.2f}"
    print(f"{label}: {r.num_trades} trades, WR={r.win_rate:.0%}, PnL=${r.total_pnl:+.2f}, EV=${r.ev_per_trade:.2f}{trades_str}")


cfg_path = "strategy_config.json"

print("=== Config Sweep ===\n")

test_config("Current (baseline)", cfg_path, {})

test_config("Low threshold", cfg_path, {
    "signal.confidence_threshold": 0.55,
    "execution.midrange_min_confidence": 0.56,
    "signal.w3_price_delta": 2.0,
    "signal.w1_order_flow": 1.0,
})

test_config("High weights + moderate threshold", cfg_path, {
    "signal.confidence_threshold": 0.65,
    "execution.midrange_min_confidence": 0.70,
    "signal.w3_price_delta": 5.0,
    "signal.w1_order_flow": 3.0,
})

test_config("Snipe-only (high weights)", cfg_path, {
    "signal.confidence_threshold": 0.60,
    "execution.max_entry_price": 0.05,
    "execution.enable_midrange": False,
    "signal.w3_price_delta": 8.0,
    "signal.w1_order_flow": 4.0,
})

test_config("Aggressive midrange", cfg_path, {
    "signal.confidence_threshold": 0.55,
    "execution.midrange_min_confidence": 0.58,
    "execution.midrange_max_price": 0.55,
    "signal.w3_price_delta": 3.0,
    "signal.w1_order_flow": 2.0,
    "signal.w4_acceleration": 1.0,
})
