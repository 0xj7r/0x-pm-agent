"""Strategy simulation engine for autoresearch.

Provides simulate_strategy() for running check functions over
precomputed markets, plus build_strategy_grid() which returns
the full combinatorial grid of strategy configurations.
"""
from __future__ import annotations

import itertools
from dataclasses import dataclass

from shared.fees import taker_fee

from backtesting.precompute import PrecomputedMarket


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
        for i in range(10, pm.num_snaps):
            direction = check_fn(pm, i)
            if direction is None:
                continue
            if direction == "SKIP":
                break

            entry = pm.price_up[i] if direction == "Up" else pm.price_down[i]
            if entry <= 0 or entry >= 0.99:
                break

            won = direction == pm.winner
            fee = taker_fee(entry) * entry
            pnl = (1.0 - entry - fee) if won else -(entry + fee)

            trades += 1
            if won:
                wins += 1
            total_pnl += pnl
            entry_sum += entry
            break

    avg_entry = entry_sum / trades if trades else 0
    return trades, wins, total_pnl, avg_entry


def build_strategy_grid() -> list[tuple[str, dict, object]]:
    """Build the full combinatorial grid of strategy configurations.

    Returns list of (name, params_dict, check_fn) tuples.
    """
    move_thresholds = [0.01, 0.02, 0.03, 0.05, 0.08]
    max_entries = [0.55, 0.60, 0.65, 0.70, 0.75, 0.85]

    strategy_defs: list[tuple[str, dict, object]] = []

    # 1. Baseline threshold
    for mt, me in itertools.product(move_thresholds, max_entries):
        def make_fn(mt=mt, me=me):
            def fn(pm, i):
                if pm.abs_move[i] < mt:
                    return None
                d = "Up" if pm.move_pct[i] > 0 else "Down"
                e = pm.price_up[i] if d == "Up" else pm.price_down[i]
                return d if e <= me else "SKIP"
            return fn
        strategy_defs.append(("threshold", {"move": mt, "max_entry": me}, make_fn()))

    # 2. Consistency
    for mt, mc, me in itertools.product(move_thresholds, [0.55, 0.60, 0.65, 0.70, 0.75], max_entries):
        def make_fn(mt=mt, mc=mc, me=me):
            def fn(pm, i):
                if pm.abs_move[i] < mt:
                    return None
                if pm.consistency[i] < mc:
                    return "SKIP"
                d = "Up" if pm.move_pct[i] > 0 else "Down"
                e = pm.price_up[i] if d == "Up" else pm.price_down[i]
                return d if e <= me else "SKIP"
            return fn
        strategy_defs.append(("consistency", {"move": mt, "cons": mc, "max_entry": me}, make_fn()))

    # 3. Velocity
    for mt, mv, me in itertools.product(move_thresholds, [0.005, 0.01, 0.02, 0.03], max_entries):
        def make_fn(mt=mt, mv=mv, me=me):
            def fn(pm, i):
                if pm.abs_move[i] < mt:
                    return None
                if abs(pm.velocity[i]) < mv:
                    return "SKIP"
                d = "Up" if pm.velocity[i] > 0 else "Down"
                e = pm.price_up[i] if d == "Up" else pm.price_down[i]
                return d if e <= me else "SKIP"
            return fn
        strategy_defs.append(("velocity", {"move": mt, "vel": mv, "max_entry": me}, make_fn()))

    # 4. Skew (book lag)
    for mt, ms, me in itertools.product(move_thresholds, [0.02, 0.05, 0.10, 0.15, 0.20], max_entries):
        def make_fn(mt=mt, ms=ms, me=me):
            def fn(pm, i):
                if pm.abs_move[i] < mt:
                    return None
                if pm.token_skew[i] > ms:
                    return "SKIP"
                d = "Up" if pm.move_pct[i] > 0 else "Down"
                e = pm.price_up[i] if d == "Up" else pm.price_down[i]
                return d if e <= me else "SKIP"
            return fn
        strategy_defs.append(("skew", {"move": mt, "skew": ms, "max_entry": me}, make_fn()))

    # 5. Timing
    for mt, te, me in itertools.product(move_thresholds, [0.05, 0.10, 0.20, 0.30, 0.50], max_entries):
        def make_fn(mt=mt, te=te, me=me):
            def fn(pm, i):
                if pm.abs_move[i] < mt:
                    return None
                if pm.elapsed_pct[i] > te:
                    return "SKIP"
                d = "Up" if pm.move_pct[i] > 0 else "Down"
                e = pm.price_up[i] if d == "Up" else pm.price_down[i]
                return d if e <= me else "SKIP"
            return fn
        strategy_defs.append(("timing", {"move": mt, "elapsed": te, "max_entry": me}, make_fn()))

    # 6. Volatility
    for mt, mv, me in itertools.product(move_thresholds, [0.001, 0.002, 0.005, 0.01], max_entries):
        def make_fn(mt=mt, mv=mv, me=me):
            def fn(pm, i):
                if pm.abs_move[i] < mt:
                    return None
                if pm.volatility[i] < mv:
                    return "SKIP"
                d = "Up" if pm.move_pct[i] > 0 else "Down"
                e = pm.price_up[i] if d == "Up" else pm.price_down[i]
                return d if e <= me else "SKIP"
            return fn
        strategy_defs.append(("volatility", {"move": mt, "vol": mv, "max_entry": me}, make_fn()))

    # 7. Acceleration
    for mt, ma, me in itertools.product(move_thresholds, [0.005, 0.01, 0.02, 0.03], max_entries):
        def make_fn(mt=mt, ma=ma, me=me):
            def fn(pm, i):
                if pm.abs_move[i] < mt:
                    return None
                d = "Up" if pm.move_pct[i] > 0 else "Down"
                if d == "Up" and pm.acceleration[i] < ma:
                    return "SKIP"
                if d == "Down" and pm.acceleration[i] > -ma:
                    return "SKIP"
                e = pm.price_up[i] if d == "Up" else pm.price_down[i]
                return d if e <= me else "SKIP"
            return fn
        strategy_defs.append(("acceleration", {"move": mt, "accel": ma, "max_entry": me}, make_fn()))

    # 8. Combo: consistency + skew + timing
    for mt, mc, ms, te, me in itertools.product(
        [0.01, 0.02, 0.03, 0.05],
        [0.55, 0.60, 0.65, 0.70, 0.75],
        [0.02, 0.05, 0.10, 0.15],
        [0.10, 0.20, 0.30, 0.50],
        [0.55, 0.60, 0.65, 0.70, 0.75],
    ):
        def make_fn(mt=mt, mc=mc, ms=ms, te=te, me=me):
            def fn(pm, i):
                if pm.abs_move[i] < mt:
                    return None
                if pm.consistency[i] < mc:
                    return "SKIP"
                if pm.token_skew[i] > ms:
                    return "SKIP"
                if pm.elapsed_pct[i] > te:
                    return "SKIP"
                d = "Up" if pm.move_pct[i] > 0 else "Down"
                e = pm.price_up[i] if d == "Up" else pm.price_down[i]
                return d if e <= me else "SKIP"
            return fn
        strategy_defs.append(("combo", {"move": mt, "cons": mc, "skew": ms, "elapsed": te, "max_entry": me}, make_fn()))

    # 9. Velocity + Consistency combo
    for mt, mv, mc, me in itertools.product(
        [0.01, 0.02, 0.03, 0.05],
        [0.005, 0.01, 0.02],
        [0.55, 0.65, 0.75],
        [0.55, 0.65, 0.75],
    ):
        def make_fn(mt=mt, mv=mv, mc=mc, me=me):
            def fn(pm, i):
                if pm.abs_move[i] < mt:
                    return None
                if abs(pm.velocity[i]) < mv:
                    return "SKIP"
                if pm.consistency[i] < mc:
                    return "SKIP"
                d = "Up" if pm.velocity[i] > 0 else "Down"
                e = pm.price_up[i] if d == "Up" else pm.price_down[i]
                return d if e <= me else "SKIP"
            return fn
        strategy_defs.append(("vel+cons", {"move": mt, "vel": mv, "cons": mc, "max_entry": me}, make_fn()))

    # 10. Acceleration + Timing
    for mt, ma, te, me in itertools.product(
        [0.01, 0.02, 0.03, 0.05],
        [0.005, 0.01, 0.02],
        [0.10, 0.20, 0.30],
        [0.55, 0.65, 0.75],
    ):
        def make_fn(mt=mt, ma=ma, te=te, me=me):
            def fn(pm, i):
                if pm.abs_move[i] < mt:
                    return None
                if pm.elapsed_pct[i] > te:
                    return "SKIP"
                d = "Up" if pm.move_pct[i] > 0 else "Down"
                if d == "Up" and pm.acceleration[i] < ma:
                    return "SKIP"
                if d == "Down" and pm.acceleration[i] > -ma:
                    return "SKIP"
                e = pm.price_up[i] if d == "Up" else pm.price_down[i]
                return d if e <= me else "SKIP"
            return fn
        strategy_defs.append(("accel+time", {"move": mt, "accel": ma, "elapsed": te, "max_entry": me}, make_fn()))

    return strategy_defs
