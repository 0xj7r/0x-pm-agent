"""Precompute features from market snapshot timeseries.

Transforms raw BTC/ETH/SOL price snapshots into feature arrays
(move_pct, velocity, consistency, volatility, etc.) for strategy
evaluation without re-reading the DB on every iteration.
"""
from __future__ import annotations

import math
from dataclasses import dataclass
from pathlib import Path

from shared.db import get_connection


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

    tick_dirs = [0] * n
    for i in range(1, n):
        if btc_prices[i] > btc_prices[i - 1]:
            tick_dirs[i] = 1
        elif btc_prices[i] < btc_prices[i - 1]:
            tick_dirs[i] = -1

    CONS_WIN = 30
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
        elapsed_pct[i] = i / 2500

        lb = min(i, 20)
        if lb > 0:
            prev = btc_prices[i - lb]
            velocity[i] = (btc - prev) / prev * 100 if prev else 0

        cw = min(i, CONS_WIN)
        if cw > 1 and mp != 0:
            target_dir = 1 if mp > 0 else -1
            same = sum(1 for j in range(i - cw + 1, i + 1) if tick_dirs[j] == target_dir)
            consistency[i] = same / cw
        else:
            consistency[i] = 0.5

        vw = min(i, VOL_WIN)
        if vw > 2:
            start = i - vw + 1
            window = tick_changes[start:i + 1]
            mean = sum(window) / len(window)
            var = sum((x - mean) ** 2 for x in window) / (len(window) - 1)
            volatility[i] = math.sqrt(var) if var > 0 else 0

        if mp > 0:
            token_skew[i] = price_up[i] - 0.5
        elif mp < 0:
            token_skew[i] = price_down[i] - 0.5
        else:
            token_skew[i] = 0.0

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


def load_and_precompute(db_path: Path, coin: str = "btc") -> list[PrecomputedMarket]:
    conn = get_connection(db_path)

    try:
        markets = conn.execute(
            "SELECT * FROM markets WHERE winner IS NOT NULL AND (coin = ? OR coin IS NULL) ORDER BY start_time",
            (coin,),
        ).fetchall()
    except Exception:
        markets = conn.execute(
            "SELECT * FROM markets WHERE winner IS NOT NULL ORDER BY start_time"
        ).fetchall()

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
