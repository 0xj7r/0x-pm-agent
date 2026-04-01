"""Backtesting engine for BTC 5-minute sniper strategy.

Replays PolyBackTest snapshot timeseries through the signal engine.
For each market, steps through sub-second snapshots computing signal
strength and checking if entry conditions are met at each timestamp.

Supports Strategy A (snipe at <=5c) and Strategy B (midrange at <=55c).
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
        return self.wins / self.num_trades if self.num_trades else 0.0

    @property
    def ev_per_trade(self) -> float:
        return self.total_pnl / self.num_trades if self.num_trades else 0.0

    @property
    def sharpe(self) -> float:
        if len(self.trades) < 2:
            return 0.0
        pnls = [t["pnl"] for t in self.trades]
        mean = sum(pnls) / len(pnls)
        variance = sum((p - mean) ** 2 for p in pnls) / (len(pnls) - 1)
        std = math.sqrt(variance) if variance > 0 else 0.0
        return mean / std if std > 0 else 0.0


def _kelly_size(p_win: float, token_price: float, bankroll: float,
                risk_cfg: RiskConfig, use_fee: bool = False,
                fee_rate: float = 0.04) -> float:
    """Kelly sizing. When use_fee=True, accounts for taker fee (Strategy B)."""
    if token_price <= 0 or token_price >= 1 or bankroll <= 0 or p_win <= 0:
        return 0.0

    if use_fee:
        fee = fee_rate * token_price * (1.0 - token_price)
        edge = p_win - token_price - fee
        net_payout = 1.0 - token_price - fee
    else:
        edge = p_win - token_price
        net_payout = 1.0 - token_price

    if edge <= 0 or net_payout <= 0:
        return 0.0

    kelly_fraction = edge / net_payout
    adjusted = kelly_fraction * risk_cfg.kelly_multiplier
    if not use_fee and token_price <= 0.05:
        adjusted *= risk_cfg.cheap_token_multiplier

    max_size = min(bankroll * risk_cfg.max_position_pct, risk_cfg.max_position_usd)
    position = min(adjusted * bankroll, max_size)
    return round(position, 2) if position >= 1.0 else 0.0


def replay_market(
    cfg: StrategyConfig,
    market: dict,
    snapshots: list[dict],
    balance: float,
) -> tuple[dict | None, float]:
    """Replay one market's snapshots through the signal engine.

    Returns (trade_dict, new_balance) or (None, balance) if no trade.
    """
    if not snapshots or not market.get("winner"):
        return None, balance

    btc_open = market.get("btc_price_start", 0)
    if not btc_open:
        btc_open = snapshots[0].get("btc_price", 0)
    if not btc_open:
        return None, balance

    winner = market["winner"]  # "Up" or "Down"
    resolved_dir = "UP" if winner.lower() == "up" else "DOWN"

    engine = BayesianSignalEngine(cfg.signal)
    prev_btc = btc_open
    buy_vol = 0.0
    sell_vol = 0.0

    # Step through snapshots, simulating what the live engine would see
    for i, snap in enumerate(snapshots):
        btc = snap.get("btc_price") or prev_btc
        price_up = snap.get("price_up")
        price_down = snap.get("price_down")

        if price_up is None or price_down is None:
            prev_btc = btc
            continue

        price_delta = (btc - btc_open) / btc_open * 100 if btc_open else 0
        # Approximate OFI from price direction between snapshots
        if btc > prev_btc:
            buy_vol += abs(btc - prev_btc)
        else:
            sell_vol += abs(btc - prev_btc)
        total = buy_vol + sell_vol
        ofi = (buy_vol - sell_vol) / total if total > 0 else 0.0

        accel = 0.0
        if i > 0 and prev_btc > 0:
            prev_delta = (prev_btc - btc_open) / btc_open * 100
            accel = price_delta - prev_delta

        engine.set_state(
            order_flow_imbalance=ofi,
            microprice_deviation=0.0,
            price_delta=price_delta,
            acceleration=accel,
        )

        # Check entry conditions
        direction = engine.direction
        p_win = engine.p_up if direction == "UP" else engine.p_down
        token_price = price_up if direction == "UP" else price_down

        # Strategy A: cheap token snipe
        if (token_price <= cfg.execution.max_entry_price
                and engine.confident
                and token_price > 0):
            size = _kelly_size(p_win, token_price, balance, cfg.risk)
            if size > 0:
                return _resolve_trade(
                    market["market_id"], direction, token_price, size, p_win,
                    resolved_dir, balance, "snipe", btc,
                )

        # Strategy B: midrange directional
        if (getattr(cfg.execution, "enable_midrange", False)
                and token_price <= getattr(cfg.execution, "midrange_max_price", 0.55)
                and p_win >= getattr(cfg.execution, "midrange_min_confidence", 0.80)
                and token_price > 0):
            fee_rate = getattr(cfg.execution, "midrange_taker_fee_rate", 0.04)
            size = _kelly_size(p_win, token_price, balance, cfg.risk,
                               use_fee=True, fee_rate=fee_rate)
            if size > 0:
                return _resolve_trade(
                    market["market_id"], direction, token_price, size, p_win,
                    resolved_dir, balance, "midrange", btc,
                )

        prev_btc = btc

    return None, balance


def _resolve_trade(
    market_id: str, direction: str, token_price: float, size_usd: float,
    p_win: float, resolved_dir: str, balance: float, strategy: str,
    btc_price: float,
) -> tuple[dict, float]:
    """Execute a trade against the resolution and return (trade_dict, new_balance)."""
    shares = size_usd / token_price
    record = PaperTradeRecord(
        trade_id=market_id, market_id=market_id,
        direction=direction, token_price=token_price,
        size_usd=size_usd, shares=shares,
    )
    res = resolve_paper_trade(record, resolved_dir)
    new_balance = balance + res.pnl_usd

    trade = {
        "market_id": market_id, "direction": direction,
        "resolved": resolved_dir, "won": res.won,
        "pnl": res.pnl_usd, "size_usd": size_usd,
        "token_price": token_price, "p_win": p_win,
        "strategy": strategy, "btc_price": btc_price,
        "balance_after": new_balance,
    }
    return trade, new_balance


def run_backtest_snapshots(
    cfg: StrategyConfig,
    db_path: Path | None = None,
    starting_balance: float = 100.0,
) -> BacktestResult:
    """Run backtest against real PolyBackTest snapshot data."""
    from backtesting.historical_data import load_markets, load_snapshots, DB_PATH

    path = db_path or DB_PATH
    markets = load_markets(path)

    result = BacktestResult(starting_balance=starting_balance, ending_balance=starting_balance)
    balance = starting_balance
    peak = starting_balance

    for m in markets:
        snaps = load_snapshots(m["market_id"], path)
        trade, balance = replay_market(cfg, m, snaps, balance)

        if trade:
            result.num_trades += 1
            if trade["won"]:
                result.wins += 1
            else:
                result.losses += 1
            result.total_pnl += trade["pnl"]
            result.trades.append(trade)

        peak = max(peak, balance)
        dd = (peak - balance) / peak if peak > 0 else 0
        result.max_drawdown = max(result.max_drawdown, dd)

    result.ending_balance = balance
    return result
