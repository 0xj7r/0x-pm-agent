"""Materialized feature store for fast backtesting.

Precomputes features from raw snapshots once, stores as contiguous
NumPy arrays with a manifest. Backtests read the store directly
instead of rebuilding from SQLite each time.

Layout:
    backtesting/features/<coin>/
        manifest.json       market metadata + offsets into feature arrays
        move_pct.npy        flat float64 array (all markets concatenated)
        abs_move.npy
        velocity.npy
        consistency.npy
        volatility.npy
        token_skew.npy
        elapsed_pct.npy
        acceleration.npy
        price_up.npy
        price_down.npy

Usage:
    python backtesting/eval/feature_store.py build --coin btc
    python backtesting/eval/feature_store.py build --coin eth --coin sol
"""
from __future__ import annotations

import argparse
import json
import logging
import sys
import time
from dataclasses import dataclass
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent.parent.parent))

from shared.db import get_connection

logger = logging.getLogger(__name__)

FEATURES_DIR = Path(__file__).parent / "features"

# Computed feature arrays (derived from raw snapshot prices)
COMPUTED_FEATURE_NAMES = [
    "move_pct", "abs_move", "velocity", "consistency",
    "volatility", "token_skew", "elapsed_pct", "acceleration",
    "price_up", "price_down",
]

# Raw orderbook passthroughs straight from snapshots, used by the
# honest execution simulator. NaN indicates the snapshot had no
# orderbook data (PolyBackTest returned the row without book, or
# the row was written by the live recorder before the bid/ask
# columns existed). The simulator must skip rows with NaN best_ask
# rather than fall back to midpoint.
ORDERBOOK_FEATURE_NAMES = [
    "best_bid_up", "best_ask_up", "bid_size_up", "ask_size_up",
    "best_bid_down", "best_ask_down", "bid_size_down", "ask_size_down",
]

FEATURE_NAMES = COMPUTED_FEATURE_NAMES + ORDERBOOK_FEATURE_NAMES


@dataclass
class MarketMeta:
    market_id: str
    winner: str
    offset: int
    length: int


def store_dir(coin: str) -> Path:
    return FEATURES_DIR / coin


def _detect_columns(db_path: Path) -> tuple[str, str]:
    conn = get_connection(db_path)
    snap_cols = [c[1] for c in conn.execute("PRAGMA table_info(snapshots)").fetchall()]
    market_cols = [c[1] for c in conn.execute("PRAGMA table_info(markets)").fetchall()]
    conn.close()
    price_col = "price" if "price" in snap_cols else "btc_price"
    start_col = "price_start" if "price_start" in market_cols else "btc_price_start"
    return price_col, start_col


def _compute_features(prices: np.ndarray, btc_open: float,
                      price_up: np.ndarray, price_down: np.ndarray) -> dict[str, np.ndarray]:
    """Vectorized feature computation for one market."""
    n = len(prices)
    idx = np.arange(n, dtype=np.int32)

    move_pct = ((prices - btc_open) / btc_open * 100) if btc_open else np.zeros(n)
    abs_move = np.abs(move_pct)
    elapsed_pct = idx / 2500.0

    # Velocity: change over last 20 snapshots
    prev_idx = np.maximum(idx - 20, 0)
    prev_prices = prices[prev_idx]
    velocity = np.zeros(n)
    mask = (idx > 0) & (prev_prices != 0)
    velocity[mask] = (prices[mask] - prev_prices[mask]) / prev_prices[mask] * 100

    # Tick directions for consistency
    deltas = np.diff(prices, prepend=prices[0])
    tick_dirs = np.sign(deltas).astype(np.int8)

    # Consistency via prefix sums
    pos_prefix = np.zeros(n + 1, dtype=np.int32)
    neg_prefix = np.zeros(n + 1, dtype=np.int32)
    pos_prefix[1:] = np.cumsum(tick_dirs == 1)
    neg_prefix[1:] = np.cumsum(tick_dirs == -1)

    cw = np.minimum(idx, 30)
    consistency = np.full(n, 0.5)
    cmask = (cw > 1) & (move_pct != 0)
    cstart = idx - cw + 1
    pos_same = pos_prefix[idx + 1] - pos_prefix[cstart]
    neg_same = neg_prefix[idx + 1] - neg_prefix[cstart]
    same = np.where(move_pct > 0, pos_same, neg_same)
    consistency[cmask] = same[cmask] / cw[cmask]

    # Volatility via prefix sums of tick changes
    tick_changes = np.zeros(n)
    prev_p = prices[:-1]
    nz = prev_p != 0
    tick_changes[1:][nz] = (prices[1:][nz] - prev_p[nz]) / prev_p[nz] * 100

    tc_prefix = np.zeros(n + 1)
    tc_sq_prefix = np.zeros(n + 1)
    tc_prefix[1:] = np.cumsum(tick_changes)
    tc_sq_prefix[1:] = np.cumsum(tick_changes ** 2)

    vw = np.minimum(idx, 50)
    volatility = np.zeros(n)
    vmask = vw > 2
    vstart = idx - vw + 1
    counts = idx - vstart + 1
    totals = tc_prefix[idx + 1] - tc_prefix[vstart]
    totals_sq = tc_sq_prefix[idx + 1] - tc_sq_prefix[vstart]
    means = np.divide(totals, counts, out=np.zeros_like(totals), where=counts != 0)
    sample_var = np.divide(
        totals_sq - counts * means ** 2, counts - 1,
        out=np.zeros_like(totals), where=(counts - 1) != 0,
    )
    volatility[vmask] = np.sqrt(np.maximum(sample_var[vmask], 0))

    # Token skew
    token_skew = np.zeros(n)
    token_skew[move_pct > 0] = price_up[move_pct > 0] - 0.5
    token_skew[move_pct < 0] = price_down[move_pct < 0] - 0.5

    # Acceleration
    acceleration = np.zeros(n)
    al = np.minimum(idx, 10)
    amask = al > 2
    ai = idx[amask]
    a_al = al[amask]
    mid = ai - a_al // 2
    early = ai - a_al
    ep, mp = prices[early], prices[mid]
    v1 = np.divide(mp - ep, ep, out=np.zeros_like(ep), where=ep != 0) * 100
    v2 = np.divide(prices[ai] - mp, mp, out=np.zeros_like(mp), where=mp != 0) * 100
    acceleration[amask] = v2 - v1

    return {
        "move_pct": move_pct, "abs_move": abs_move,
        "velocity": velocity, "consistency": consistency,
        "volatility": volatility, "token_skew": token_skew,
        "elapsed_pct": elapsed_pct, "acceleration": acceleration,
        "price_up": price_up, "price_down": price_down,
    }


def build_feature_store(db_path: Path, coin: str) -> int:
    """Build (or rebuild) the feature store for a coin. Returns market count."""
    price_col, start_col = _detect_columns(db_path)
    conn = get_connection(db_path)
    conn.row_factory = None

    try:
        market_rows = conn.execute(
            "SELECT market_id, winner, " + start_col + " FROM markets "
            "WHERE winner IS NOT NULL AND (coin = ? OR coin IS NULL) ORDER BY start_time",
            (coin,),
        ).fetchall()
    except Exception:
        market_rows = conn.execute(
            "SELECT market_id, winner, " + start_col + " FROM markets "
            "WHERE winner IS NOT NULL ORDER BY start_time"
        ).fetchall()

    logger.info(f"[{coin.upper()}] {len(market_rows)} resolved markets")

    # Collect all features into flat arrays
    all_features = {name: [] for name in FEATURE_NAMES}
    manifest = []
    offset = 0

    t0 = time.time()
    processed = 0

    for market_id, winner, open_price in market_rows:
        rows = conn.execute(
            f"SELECT {price_col}, price_up, price_down, "
            "best_bid_up, best_ask_up, bid_size_up, ask_size_up, "
            "best_bid_down, best_ask_down, bid_size_down, ask_size_down "
            "FROM snapshots WHERE market_id = ? ORDER BY time",
            (market_id,),
        ).fetchall()

        if len(rows) < 50:
            continue

        prices = np.array([r[0] or (open_price or 0) for r in rows], dtype=np.float64)
        p_up = np.array([r[1] if r[1] is not None else 0.5 for r in rows], dtype=np.float64)
        p_down = np.array([r[2] if r[2] is not None else 0.5 for r in rows], dtype=np.float64)

        effective_open = open_price or prices[0]
        features = _compute_features(prices, effective_open, p_up, p_down)

        # Pass through orderbook columns. None becomes NaN so the
        # simulator can detect missing book data unambiguously.
        ob_arrays = {
            name: np.array(
                [r[3 + i] if r[3 + i] is not None else np.nan for r in rows],
                dtype=np.float64,
            )
            for i, name in enumerate(ORDERBOOK_FEATURE_NAMES)
        }
        features.update(ob_arrays)

        n = len(prices)
        for name in FEATURE_NAMES:
            all_features[name].append(features[name])

        manifest.append({
            "market_id": market_id,
            "winner": winner,
            "offset": offset,
            "length": n,
        })
        offset += n
        processed += 1

        if processed % 200 == 0:
            elapsed = time.time() - t0
            logger.info(f"[{coin.upper()}] {processed} markets in {elapsed:.1f}s")

    conn.close()

    if not manifest:
        logger.warning(f"[{coin.upper()}] No markets with snapshots")
        return 0

    # Write to disk
    out_dir = store_dir(coin)
    out_dir.mkdir(parents=True, exist_ok=True)

    for name in FEATURE_NAMES:
        arr = np.concatenate(all_features[name])
        np.save(out_dir / f"{name}.npy", arr)

    (out_dir / "manifest.json").write_text(json.dumps(manifest, separators=(",", ":")))

    elapsed = time.time() - t0
    logger.info(f"[{coin.upper()}] Built feature store: {processed} markets, "
                f"{offset:,} data points, {elapsed:.1f}s")
    return processed


def load_feature_store(coin: str) -> tuple[list[MarketMeta], dict[str, np.ndarray]]:
    """Load a feature store. Returns (manifest, feature_arrays)."""
    sd = store_dir(coin)
    manifest_raw = json.loads((sd / "manifest.json").read_text())
    manifest = [MarketMeta(**m) for m in manifest_raw]

    features = {}
    for name in FEATURE_NAMES:
        features[name] = np.load(sd / f"{name}.npy", mmap_mode="r")

    return manifest, features


def append_new_markets(db_path: Path, coin: str) -> int:
    """Append only markets not already in the store. Returns count of new markets."""
    sd = store_dir(coin)
    if not (sd / "manifest.json").exists():
        return build_feature_store(db_path, coin)

    manifest_raw = json.loads((sd / "manifest.json").read_text())
    existing_ids = {m["market_id"] for m in manifest_raw}
    current_offset = sum(m["length"] for m in manifest_raw)

    price_col, start_col = _detect_columns(db_path)
    conn = get_connection(db_path)
    conn.row_factory = None

    try:
        market_rows = conn.execute(
            "SELECT market_id, winner, " + start_col + " FROM markets "
            "WHERE winner IS NOT NULL AND (coin = ? OR coin IS NULL) ORDER BY start_time",
            (coin,),
        ).fetchall()
    except Exception:
        market_rows = conn.execute(
            "SELECT market_id, winner, " + start_col + " FROM markets "
            "WHERE winner IS NOT NULL ORDER BY start_time"
        ).fetchall()

    new_markets = [(mid, w, op) for mid, w, op in market_rows if mid not in existing_ids]
    if not new_markets:
        conn.close()
        return 0

    logger.info(f"[{coin.upper()}] Appending {len(new_markets)} new markets")

    new_features = {name: [] for name in FEATURE_NAMES}
    new_manifest = []
    offset = current_offset

    for market_id, winner, open_price in new_markets:
        rows = conn.execute(
            f"SELECT {price_col}, price_up, price_down, "
            "best_bid_up, best_ask_up, bid_size_up, ask_size_up, "
            "best_bid_down, best_ask_down, bid_size_down, ask_size_down "
            "FROM snapshots WHERE market_id = ? ORDER BY time",
            (market_id,),
        ).fetchall()

        if len(rows) < 50:
            continue

        prices = np.array([r[0] or (open_price or 0) for r in rows], dtype=np.float64)
        p_up = np.array([r[1] if r[1] is not None else 0.5 for r in rows], dtype=np.float64)
        p_down = np.array([r[2] if r[2] is not None else 0.5 for r in rows], dtype=np.float64)

        features = _compute_features(prices, open_price or prices[0], p_up, p_down)
        ob_arrays = {
            name: np.array(
                [r[3 + i] if r[3 + i] is not None else np.nan for r in rows],
                dtype=np.float64,
            )
            for i, name in enumerate(ORDERBOOK_FEATURE_NAMES)
        }
        features.update(ob_arrays)
        n = len(prices)

        for name in FEATURE_NAMES:
            new_features[name].append(features[name])

        new_manifest.append({
            "market_id": market_id,
            "winner": winner,
            "offset": offset,
            "length": n,
        })
        offset += n

    conn.close()

    if not new_manifest:
        return 0

    # Append to existing arrays
    for name in FEATURE_NAMES:
        existing = np.load(sd / f"{name}.npy")
        new_arr = np.concatenate(new_features[name])
        combined = np.concatenate([existing, new_arr])
        np.save(sd / f"{name}.npy", combined)

    manifest_raw.extend(new_manifest)
    (sd / "manifest.json").write_text(json.dumps(manifest_raw, separators=(",", ":")))

    logger.info(f"[{coin.upper()}] Appended {len(new_manifest)} markets")
    return len(new_manifest)


def main():
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=["build", "append", "info"])
    parser.add_argument("--coin", action="append", default=[])
    parser.add_argument("--db-dir", type=str, default=str(Path(__file__).parent))
    args = parser.parse_args()

    coins = args.coin or ["btc", "eth", "sol"]

    for coin in coins:
        db = Path(args.db_dir) / f"{coin}.db"
        if not db.exists():
            logger.warning(f"[{coin.upper()}] No DB at {db}")
            continue

        if args.command == "build":
            build_feature_store(db, coin)
        elif args.command == "append":
            n = append_new_markets(db, coin)
            print(f"{coin.upper()}: {n} new markets appended")
        elif args.command == "info":
            sd = store_dir(coin)
            if not (sd / "manifest.json").exists():
                print(f"{coin.upper()}: no feature store")
                continue
            manifest = json.loads((sd / "manifest.json").read_text())
            total_points = sum(m["length"] for m in manifest)
            print(f"{coin.upper()}: {len(manifest)} markets, {total_points:,} data points")


if __name__ == "__main__":
    main()
