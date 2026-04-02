"""Autoresearch: strategy autodiscovery for BTC 5-minute markets.

Precomputes features from snapshot timeseries, grid-searches over
entry rules, and validates on held-out test data. Only reports
strategies profitable out-of-sample.

Usage:
    python backtesting/autoresearch.py
    python backtesting/autoresearch.py --min-markets 100 --test-pct 0.3
"""
from __future__ import annotations

import argparse
import itertools
import logging
import math
import sqlite3
from dataclasses import dataclass, field
from pathlib import Path

logger = logging.getLogger(__name__)

DB_PATH = Path(__file__).parent / "historical.db"
def taker_fee(price: float) -> float:
    """Polymarket dynamic taker fee: 0.072 * p * (1-p)."""
    return 0.072 * price * (1.0 - price)


@dataclass
class PrecomputedMarket:
    market_id: str
    winner: str
    num_snaps: int
    # Arrays indexed by snapshot position
    move_pct: list[float]
    abs_move: list[float]
    velocity: list[float]
    consistency: list[float]
    volatility: list[float]
    token_skew: list[float]
    elapsed_pct: list[float]
    acceleration: list[float]
    price_up: list[float]
    price_down: list[float]


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


def precompute_market(market_id: str, winner: str, btc_open: float,
                      snaps: list[dict]) -> PrecomputedMarket:
    n = len(snaps)
    move_pct = [0.0] * n
    abs_move = [0.0] * n
    velocity = [0.0] * n
    consistency = [0.5] * n
    volatility = [0.0] * n
    token_skew = [0.0] * n
    elapsed_pct = [0.0] * n
    acceleration = [0.0] * n
    price_up = [0.5] * n
    price_down = [0.5] * n

    btc_prices = [0.0] * n
    for i, s in enumerate(snaps):
        btc_prices[i] = s.get("price", s.get("btc_price", 0)) or btc_open
        price_up[i] = s["price_up"] if s["price_up"] is not None else 0.5
        price_down[i] = s["price_down"] if s["price_down"] is not None else 0.5

    # Incremental tick direction tracking for consistency
    # Track a rolling window of same-direction ticks
    tick_dirs = [0] * n  # 1 = up tick, -1 = down tick, 0 = flat
    for i in range(1, n):
        if btc_prices[i] > btc_prices[i - 1]:
            tick_dirs[i] = 1
        elif btc_prices[i] < btc_prices[i - 1]:
            tick_dirs[i] = -1

    # Running sum for consistency (rolling window of 30)
    CONS_WIN = 30

    # Volatility: incremental variance (Welford's) over rolling window
    VOL_WIN = 50
    tick_changes = [0.0] * n
    for i in range(1, n):
        if btc_prices[i - 1] > 0:
            tick_changes[i] = (btc_prices[i] - btc_prices[i - 1]) / btc_prices[i - 1] * 100

    for i in range(n):
        btc = btc_prices[i]
        mp = (btc - btc_open) / btc_open * 100 if btc_open else 0
        move_pct[i] = mp
        abs_move[i] = abs(mp)
        elapsed_pct[i] = i / 2500  # ~2500 snaps per 5m market; avoids look-ahead on n

        # Velocity: change over last 20 snapshots
        lb = min(i, 20)
        if lb > 0:
            prev = btc_prices[i - lb]
            velocity[i] = (btc - prev) / prev * 100 if prev else 0

        # Consistency: fraction of last CONS_WIN ticks matching move direction
        cw = min(i, CONS_WIN)
        if cw > 1 and mp != 0:
            target_dir = 1 if mp > 0 else -1
            same = sum(1 for j in range(i - cw + 1, i + 1) if tick_dirs[j] == target_dir)
            consistency[i] = same / cw
        else:
            consistency[i] = 0.5

        # Volatility: stddev of last VOL_WIN tick changes
        vw = min(i, VOL_WIN)
        if vw > 2:
            start = i - vw + 1
            window = tick_changes[start:i + 1]
            mean = sum(window) / len(window)
            var = sum((x - mean) ** 2 for x in window) / (len(window) - 1)
            volatility[i] = math.sqrt(var) if var > 0 else 0

        # Token skew
        if mp > 0:
            token_skew[i] = price_up[i] - 0.5
        elif mp < 0:
            token_skew[i] = price_down[i] - 0.5
        else:
            token_skew[i] = 0.0

        # Acceleration
        al = min(i, 10)
        if al > 2:
            mid = i - al // 2
            p_early = btc_prices[i - al]
            p_mid = btc_prices[mid]
            v1 = (p_mid - p_early) / p_early * 100 if p_early else 0
            v2 = (btc - p_mid) / p_mid * 100 if p_mid else 0
            acceleration[i] = v2 - v1

    return PrecomputedMarket(
        market_id=market_id, winner=winner, num_snaps=n,
        move_pct=move_pct, abs_move=abs_move, velocity=velocity,
        consistency=consistency, volatility=volatility,
        token_skew=token_skew, elapsed_pct=elapsed_pct,
        acceleration=acceleration, price_up=price_up, price_down=price_down,
    )


def load_and_precompute(db_path: Path = DB_PATH, coin: str = "btc") -> list[PrecomputedMarket]:
    conn = sqlite3.connect(str(db_path))
    conn.row_factory = sqlite3.Row

    # Try coin-filtered query first, fall back to unfiltered for per-coin DBs
    try:
        markets = conn.execute(
            "SELECT * FROM markets WHERE winner IS NOT NULL AND (coin = ? OR coin IS NULL) ORDER BY start_time",
            (coin,),
        ).fetchall()
    except sqlite3.OperationalError:
        markets = conn.execute(
            "SELECT * FROM markets WHERE winner IS NOT NULL ORDER BY start_time"
        ).fetchall()

    # Detect schema: new per-coin DBs use 'price'/'price_start', old uses 'btc_price'/'btc_price_start'
    snap_cols = [c[1] for c in conn.execute("PRAGMA table_info(snapshots)").fetchall()]
    price_col = "price" if "price" in snap_cols else "btc_price"
    market_cols = [c[1] for c in conn.execute("PRAGMA table_info(markets)").fetchall()]
    start_col = "price_start" if "price_start" in market_cols else "btc_price_start"

    result = []
    for m in markets:
        snaps = conn.execute(
            f"SELECT {price_col} as price, price_up, price_down FROM snapshots "
            "WHERE market_id = ? ORDER BY time",
            (m["market_id"],),
        ).fetchall()
        if len(snaps) < 50:
            continue
        open_price = m[start_col] or snaps[0]["price"]
        pm = precompute_market(
            m["market_id"], m["winner"], open_price,
            [dict(s) for s in snaps],
        )
        result.append(pm)

    conn.close()
    return result


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


def run_autoresearch(
    db_path: Path = DB_PATH,
    test_pct: float = 0.3,
    min_markets: int = 30,
    coin: str = "btc",
) -> list[StrategyResult]:
    logger.info(f"Loading and precomputing features for {coin.upper()}...")
    all_markets = load_and_precompute(db_path, coin=coin)
    logger.info(f"Precomputed {len(all_markets)} {coin.upper()} markets")

    if len(all_markets) < min_markets:
        logger.error(f"Need at least {min_markets} markets, have {len(all_markets)}")
        return []

    split = int(len(all_markets) * (1 - test_pct))
    train = all_markets[:split]
    test = all_markets[split:]
    logger.info(f"Train: {len(train)}, Test: {len(test)}")

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

    logger.info(f"Testing {len(strategy_defs)} strategy configurations...")

    results = []
    for i, (name, params, fn) in enumerate(strategy_defs):
        tr_trades, tr_wins, tr_pnl, tr_avg = simulate_strategy(train, fn)
        if tr_trades < 5 or tr_pnl <= 0:
            continue

        te_trades, te_wins, te_pnl, te_avg = simulate_strategy(test, fn)
        if te_trades < 3 or te_pnl <= 0:
            continue

        avg_entry = (tr_avg + te_avg) / 2
        total_trades = tr_trades + te_trades
        total_pnl = tr_pnl + te_pnl

        results.append(StrategyResult(
            name=name, params=params,
            train_trades=tr_trades, train_wins=tr_wins, train_pnl=tr_pnl,
            test_trades=te_trades, test_wins=te_wins, test_pnl=te_pnl,
            avg_entry=avg_entry,
            avg_profit_per_trade=total_pnl / total_trades if total_trades else 0,
        ))

        if (i + 1) % 1000 == 0:
            logger.info(f"  [{i+1}/{len(strategy_defs)}] {len(results)} profitable so far")

    results.sort(key=lambda r: r.test_pnl_per_trade, reverse=True)
    logger.info(f"Done: {len(results)} strategies profitable on both train and test")
    return results


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", type=str, default=str(DB_PATH))
    parser.add_argument("--test-pct", type=float, default=0.3)
    parser.add_argument("--min-markets", type=int, default=30)
    parser.add_argument("--coin", type=str, default="btc", choices=["btc", "eth", "sol"])
    args = parser.parse_args()

    results = run_autoresearch(Path(args.db), args.test_pct, args.min_markets, args.coin)

    if not results:
        print("\nNo profitable strategies found on both train and test sets.")
        return

    print(f"\n{'='*100}")
    print(f"AUTORESEARCH RESULTS: Top strategies profitable on held-out test data")
    print(f"{'='*100}\n")

    top = results[:30]
    print(f"{'Rank':<5} {'Strategy':<14} {'Train':<20} {'Test':<20} "
          f"{'Avg Entry':<10} {'Test $/trade':<12} {'Params'}")
    print("-" * 120)

    for i, r in enumerate(top):
        train_str = f"{r.train_wins}/{r.train_trades} ${r.train_pnl:+.2f}"
        test_str = f"{r.test_wins}/{r.test_trades} ${r.test_pnl:+.2f}"
        print(f"{i+1:<5} {r.name:<14} {train_str:<20} {test_str:<20} "
              f"{r.avg_entry:<10.3f} {r.test_pnl_per_trade:<+12.4f} {r.params}")

    print(f"\n{'='*100}")
    print("TOP 5 DETAILED")
    print(f"{'='*100}")

    for i, r in enumerate(top[:5]):
        print(f"\n#{i+1}: {r.name} {r.params}")
        print(f"  Train: {r.train_wins}/{r.train_trades} wins "
              f"({r.train_win_rate*100:.0f}%), P&L ${r.train_pnl:+.2f}")
        print(f"  Test:  {r.test_wins}/{r.test_trades} wins "
              f"({r.test_win_rate*100:.0f}%), P&L ${r.test_pnl:+.2f}")
        print(f"  Avg entry: ${r.avg_entry:.3f}")
        print(f"  Test P&L per trade: ${r.test_pnl_per_trade:+.4f}")
        fee_at_entry = taker_fee(r.avg_entry) * r.avg_entry
        breakeven_wr = (r.avg_entry + fee_at_entry) / (1.0 - fee_at_entry + r.avg_entry)
        print(f"  Breakeven win rate at this entry: {breakeven_wr*100:.0f}%")
        print(f"  Actual test win rate: {r.test_win_rate*100:.0f}%")
        margin = r.test_win_rate - breakeven_wr
        print(f"  Edge over breakeven: {margin*100:+.1f}pp")


if __name__ == "__main__":
    main()
