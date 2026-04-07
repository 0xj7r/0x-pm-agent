"""Precompute features from market snapshot timeseries.

Transforms raw BTC/ETH/SOL price snapshots into feature arrays
(move_pct, velocity, consistency, volatility, etc.) for strategy
evaluation without re-reading the DB on every iteration.
"""
from __future__ import annotations

import os
import pickle
import sqlite3
import threading
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path

import numpy as np

from shared.db import get_connection


@dataclass
class PrecomputedMarket:
    market_id: str
    winner: str
    num_snaps: int
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


PRECOMPUTE_CACHE_VERSION = 5
PARALLEL_MARKET_THRESHOLD = 1000
MAX_PRECOMPUTE_WORKERS = 8
_THREAD_LOCAL = threading.local()


def _cache_path(db_path: Path, coin: str) -> Path:
    return db_path.with_name(
        f"{db_path.stem}.{coin}.precomputed.v{PRECOMPUTE_CACHE_VERSION}.pkl"
    )


def _load_cached_precompute(db_path: Path, coin: str) -> list[PrecomputedMarket] | None:
    cache_path = _cache_path(db_path, coin)
    if not cache_path.exists():
        return None

    source_stat = db_path.stat()
    with cache_path.open("rb") as f:
        payload = pickle.load(f)

    if payload.get("version") != PRECOMPUTE_CACHE_VERSION:
        return None
    if payload.get("coin") != coin:
        return None
    if payload.get("db_mtime_ns") != source_stat.st_mtime_ns:
        return None
    if payload.get("db_size") != source_stat.st_size:
        return None
    return payload.get("markets")


def _write_cached_precompute(
    db_path: Path, coin: str, markets: list[PrecomputedMarket]
) -> None:
    source_stat = db_path.stat()
    payload = {
        "version": PRECOMPUTE_CACHE_VERSION,
        "coin": coin,
        "db_mtime_ns": source_stat.st_mtime_ns,
        "db_size": source_stat.st_size,
        "markets": markets,
    }
    with _cache_path(db_path, coin).open("wb") as f:
        pickle.dump(payload, f, protocol=pickle.HIGHEST_PROTOCOL)


def _get_thread_connection(db_path: Path) -> sqlite3.Connection:
    conn = getattr(_THREAD_LOCAL, "conn", None)
    conn_path = getattr(_THREAD_LOCAL, "db_path", None)
    if conn is None or conn_path != str(db_path):
        conn = get_connection(db_path)
        conn.execute("PRAGMA query_only=ON")
        conn.execute("PRAGMA temp_store=MEMORY")
        conn.execute("PRAGMA cache_size=-200000")
        conn.execute("PRAGMA mmap_size=1073741824")
        conn.row_factory = None
        _THREAD_LOCAL.conn = conn
        _THREAD_LOCAL.db_path = str(db_path)
    return conn


def _load_market_precompute(
    task: tuple[Path, str, str, float, str],
) -> PrecomputedMarket | None:
    db_path, market_id, winner, open_price, price_col = task
    conn = _get_thread_connection(db_path)
    rows = conn.execute(
        f"SELECT {price_col} as price, price_up, price_down "
        "FROM snapshots WHERE market_id = ? ORDER BY time",
        (market_id,),
    ).fetchall()
    if len(rows) < 50:
        return None

    snaps = [(row[0], row[1], row[2]) for row in rows]
    effective_open = open_price or snaps[0][0]
    return precompute_market(market_id, winner, effective_open, snaps)


def precompute_market(
    market_id: str,
    winner: str,
    btc_open: float,
    snaps: list[tuple[float, float | None, float | None]],
) -> PrecomputedMarket:
    n = len(snaps)
    prices = np.fromiter(
        ((price or btc_open) for price, _, _ in snaps), dtype=np.float64, count=n
    )
    price_up_arr = np.fromiter(
        ((up if up is not None else 0.5) for _, up, _ in snaps),
        dtype=np.float64,
        count=n,
    )
    price_down_arr = np.fromiter(
        ((down if down is not None else 0.5) for _, _, down in snaps),
        dtype=np.float64,
        count=n,
    )

    idx = np.arange(n, dtype=np.int32)

    if btc_open:
        move_pct_arr = ((prices - btc_open) / btc_open) * 100.0
    else:
        move_pct_arr = np.zeros(n, dtype=np.float64)
    abs_move_arr = np.abs(move_pct_arr)
    elapsed_pct_arr = idx.astype(np.float64) / 2500.0

    prev_idx = np.maximum(idx - 20, 0)
    prev_prices = prices[prev_idx]
    velocity_arr = np.zeros(n, dtype=np.float64)
    velocity_mask = idx > 0
    velocity_arr[velocity_mask] = (
        np.divide(
            prices[velocity_mask] - prev_prices[velocity_mask],
            prev_prices[velocity_mask],
            out=np.zeros_like(prices[velocity_mask]),
            where=prev_prices[velocity_mask] != 0,
        )
        * 100.0
    )

    price_deltas = np.diff(prices, prepend=prices[0])
    tick_dirs = np.sign(price_deltas).astype(np.int8)

    tick_changes = np.zeros(n, dtype=np.float64)
    prev_tick_prices = prices[:-1]
    tick_changes[1:] = (
        np.divide(
            prices[1:] - prev_tick_prices,
            prev_tick_prices,
            out=np.zeros(n - 1, dtype=np.float64),
            where=prev_tick_prices != 0,
        )
        * 100.0
    )

    pos_prefix = np.zeros(n + 1, dtype=np.int32)
    neg_prefix = np.zeros(n + 1, dtype=np.int32)
    pos_prefix[1:] = np.cumsum(tick_dirs == 1)
    neg_prefix[1:] = np.cumsum(tick_dirs == -1)

    consistency_arr = np.full(n, 0.5, dtype=np.float64)
    cw = np.minimum(idx, 30)
    consistency_mask = (cw > 1) & (move_pct_arr != 0)
    consistency_start = idx - cw + 1
    pos_same = pos_prefix[idx + 1] - pos_prefix[consistency_start]
    neg_same = neg_prefix[idx + 1] - neg_prefix[consistency_start]
    target_is_up = move_pct_arr > 0
    same = np.where(target_is_up, pos_same, neg_same)
    consistency_arr[consistency_mask] = same[consistency_mask] / cw[consistency_mask]

    tick_prefix = np.zeros(n + 1, dtype=np.float64)
    tick_sq_prefix = np.zeros(n + 1, dtype=np.float64)
    tick_prefix[1:] = np.cumsum(tick_changes)
    tick_sq_prefix[1:] = np.cumsum(tick_changes * tick_changes)

    volatility_arr = np.zeros(n, dtype=np.float64)
    vw = np.minimum(idx, 50)
    volatility_mask = vw > 2
    volatility_start = idx - vw + 1
    counts = idx - volatility_start + 1
    totals = tick_prefix[idx + 1] - tick_prefix[volatility_start]
    totals_sq = tick_sq_prefix[idx + 1] - tick_sq_prefix[volatility_start]
    means = np.divide(totals, counts, out=np.zeros_like(totals), where=counts != 0)
    sample_var = np.divide(
        totals_sq - (counts * means * means),
        counts - 1,
        out=np.zeros_like(totals),
        where=(counts - 1) != 0,
    )
    volatility_arr[volatility_mask] = np.sqrt(
        np.maximum(sample_var[volatility_mask], 0.0)
    )

    token_skew_arr = np.zeros(n, dtype=np.float64)
    up_mask = move_pct_arr > 0
    down_mask = move_pct_arr < 0
    token_skew_arr[up_mask] = price_up_arr[up_mask] - 0.5
    token_skew_arr[down_mask] = price_down_arr[down_mask] - 0.5

    acceleration_arr = np.zeros(n, dtype=np.float64)
    al = np.minimum(idx, 10)
    accel_mask = al > 2
    accel_idx = idx[accel_mask]
    accel_al = al[accel_mask]
    mid_idx = accel_idx - (accel_al // 2)
    early_idx = accel_idx - accel_al
    early_prices = prices[early_idx]
    mid_prices = prices[mid_idx]
    v1 = (
        np.divide(
            mid_prices - early_prices,
            early_prices,
            out=np.zeros_like(early_prices),
            where=early_prices != 0,
        )
        * 100.0
    )
    v2 = (
        np.divide(
            prices[accel_idx] - mid_prices,
            mid_prices,
            out=np.zeros_like(mid_prices),
            where=mid_prices != 0,
        )
        * 100.0
    )
    acceleration_arr[accel_mask] = v2 - v1

    return PrecomputedMarket(
        market_id=market_id,
        winner=winner,
        num_snaps=n,
        move_pct=move_pct_arr.tolist(),
        abs_move=abs_move_arr.tolist(),
        velocity=velocity_arr.tolist(),
        consistency=consistency_arr.tolist(),
        volatility=volatility_arr.tolist(),
        token_skew=token_skew_arr.tolist(),
        elapsed_pct=elapsed_pct_arr.tolist(),
        acceleration=acceleration_arr.tolist(),
        price_up=price_up_arr.tolist(),
        price_down=price_down_arr.tolist(),
    )


def _load_and_precompute_serial(db_path: Path, coin: str) -> list[PrecomputedMarket]:
    conn = get_connection(db_path)
    conn.execute("PRAGMA query_only=ON")
    conn.execute("PRAGMA temp_store=MEMORY")
    conn.execute("PRAGMA cache_size=-200000")
    conn.execute("PRAGMA mmap_size=1073741824")

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

    market_map: dict[str, sqlite3.Row] = {m["market_id"]: m for m in markets}
    market_order = {m["market_id"]: idx for idx, m in enumerate(markets)}

    result: list[tuple[int, PrecomputedMarket]] = []
    current_market_id: str | None = None
    current_snaps: list[tuple[float, float | None, float | None]] = []

    conn.row_factory = None
    snap_rows = conn.execute(
        f"SELECT market_id, {price_col} as price, price_up, price_down "
        "FROM snapshots ORDER BY market_id, time"
    )

    def flush_market(
        market_id: str | None, snaps: list[tuple[float, float | None, float | None]]
    ) -> None:
        if not market_id or market_id not in market_map or len(snaps) < 50:
            return
        market = market_map[market_id]
        open_price = market[start_col] or snaps[0][0]
        pm = precompute_market(market_id, market["winner"], open_price, snaps)
        result.append((market_order[market_id], pm))

    for row in snap_rows:
        market_id = row[0]
        if market_id not in market_map:
            continue
        if current_market_id is None:
            current_market_id = market_id
        if market_id != current_market_id:
            flush_market(current_market_id, current_snaps)
            current_market_id = market_id
            current_snaps = []
        current_snaps.append((row[1], row[2], row[3]))

    flush_market(current_market_id, current_snaps)

    conn.close()
    result.sort(key=lambda item: item[0])
    return [pm for _, pm in result]


def _load_and_precompute_parallel(db_path: Path, coin: str) -> list[PrecomputedMarket]:
    conn = get_connection(db_path)
    conn.execute("PRAGMA query_only=ON")
    conn.execute("PRAGMA temp_store=MEMORY")
    conn.execute("PRAGMA cache_size=-200000")
    conn.execute("PRAGMA mmap_size=1073741824")

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
    conn.close()

    tasks = [
        (db_path, market["market_id"], market["winner"], market[start_col], price_col)
        for market in markets
    ]
    workers = min(MAX_PRECOMPUTE_WORKERS, os.cpu_count() or 4)
    with ThreadPoolExecutor(max_workers=workers) as executor:
        result = list(executor.map(_load_market_precompute, tasks))
    return [pm for pm in result if pm is not None]


def load_and_precompute(db_path: Path, coin: str = "btc") -> list[PrecomputedMarket]:
    cached = _load_cached_precompute(db_path, coin)
    if cached is not None:
        return cached

    conn = get_connection(db_path)
    try:
        market_count = conn.execute(
            "SELECT COUNT(*) FROM markets WHERE winner IS NOT NULL AND (coin = ? OR coin IS NULL)",
            (coin,),
        ).fetchone()[0]
    except Exception:
        market_count = conn.execute(
            "SELECT COUNT(*) FROM markets WHERE winner IS NOT NULL"
        ).fetchone()[0]
    conn.close()

    if market_count >= PARALLEL_MARKET_THRESHOLD:
        markets_out = _load_and_precompute_parallel(db_path, coin)
    else:
        markets_out = _load_and_precompute_serial(db_path, coin)

    _write_cached_precompute(db_path, coin, markets_out)
    return markets_out
