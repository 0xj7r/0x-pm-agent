"""Join Polymarket wallet activity rows to local historical market snapshots.

This is the first research-side primitive for whale reverse-engineering:
take `/activity` rows and align them to contemporaneous local snapshot state
from `backtesting/{btc,eth}.db`.

Usage:
  python3 scripts/join_wallet_to_market_state.py --activity data/research/whale_analysis/f73cad/activity.json
  python3 scripts/join_wallet_to_market_state.py --wallet 0xe51b3d64da5b0b8a07a55f8bb3c3170237f73cad
"""
from __future__ import annotations

import argparse
import json
import sqlite3
from bisect import bisect_right
from collections import Counter, defaultdict
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
WHALE_DIR = ROOT / "data" / "research" / "whale_analysis"
OUTPUT_DIR = ROOT / "data" / "research" / "wallet_research"
DBS_BY_ASSET = {
    "btc": [
        ROOT / "backtesting" / "btc.db",
        ROOT / "data" / "snapshots_bridge" / "btc.db",
    ],
    "eth": [
        ROOT / "backtesting" / "eth.db",
        ROOT / "data" / "snapshots_bridge" / "eth.db",
    ],
}


def ts_to_iso_z(ts: int) -> str:
    return datetime.fromtimestamp(ts, tz=UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def iso_to_ts(value: str | None) -> int | None:
    if not value:
        return None
    normalized = value.replace("Z", "+00:00")
    return int(datetime.fromisoformat(normalized).timestamp())


def infer_asset(slug: str) -> str | None:
    slug = (slug or "").lower()
    if slug.startswith("btc-") or slug.startswith("bitcoin-"):
        return "btc"
    if slug.startswith("eth-") or slug.startswith("ethereum-"):
        return "eth"
    return None


def resolve_activity_path(wallet: str | None, activity_path: str | None) -> Path:
    if activity_path:
        return Path(activity_path)
    if not wallet:
        raise SystemExit("Either --wallet or --activity is required")
    last6 = wallet.lower()[-6:]
    candidates = [
        WHALE_DIR / last6 / "activity.json",
    ]
    for candidate in candidates:
        if candidate.exists():
            return candidate
    raise SystemExit(f"Could not find local activity file for wallet suffix {last6}")


def get_conn(db_path: Path) -> sqlite3.Connection:
    conn = sqlite3.connect(str(db_path))
    conn.row_factory = sqlite3.Row
    return conn


@dataclass
class MarketSnapshots:
    market: dict[str, Any]
    times: list[str]
    rows: list[dict[str, Any]]


class SnapshotStore:
    def __init__(self, db_path: Path) -> None:
        self.db_path = db_path
        self._conn = get_conn(db_path)
        self._market_by_slug: dict[str, dict[str, Any]] = {}
        self._snapshots_by_market_id: dict[str, MarketSnapshots] = {}

    def market_for_slug(self, slug: str) -> dict[str, Any] | None:
        if slug in self._market_by_slug:
            return self._market_by_slug[slug]
        row = self._conn.execute(
            "SELECT * FROM markets WHERE slug = ? LIMIT 1",
            (slug,),
        ).fetchone()
        market = dict(row) if row else None
        if market:
            self._market_by_slug[slug] = market
        return market

    def snapshots_for_market(self, market: dict[str, Any]) -> MarketSnapshots:
        market_id = str(market["market_id"])
        cached = self._snapshots_by_market_id.get(market_id)
        if cached is not None:
            return cached
        rows = [
            dict(r)
            for r in self._conn.execute(
                "SELECT * FROM snapshots WHERE market_id = ? ORDER BY time",
                (market_id,),
            ).fetchall()
        ]
        snap = MarketSnapshots(
            market=market,
            times=[str(r["time"]) for r in rows],
            rows=rows,
        )
        self._snapshots_by_market_id[market_id] = snap
        return snap

    def nearest_before(self, market: dict[str, Any], activity_iso: str) -> dict[str, Any] | None:
        snapshots = self.snapshots_for_market(market)
        idx = bisect_right(snapshots.times, activity_iso) - 1
        if idx < 0:
            return None
        return snapshots.rows[idx]

    def nearest_after(self, market: dict[str, Any], activity_iso: str) -> dict[str, Any] | None:
        snapshots = self.snapshots_for_market(market)
        idx = bisect_right(snapshots.times, activity_iso)
        if idx >= len(snapshots.rows):
            return None
        return snapshots.rows[idx]


class CompositeSnapshotStore:
    def __init__(self, db_paths: list[Path]) -> None:
        self._stores = [SnapshotStore(path) for path in db_paths if path.exists()]

    def market_for_slug(self, slug: str) -> tuple[dict[str, Any] | None, SnapshotStore | None]:
        for store in self._stores:
            market = store.market_for_slug(slug)
            if market is not None:
                return dict(market), store
        return None, None


def compute_joined_row(activity: dict[str, Any], market: dict[str, Any], snap: dict[str, Any] | None) -> dict[str, Any]:
    activity_ts = int(activity["timestamp"])
    activity_iso = ts_to_iso_z(activity_ts)
    start_ts = iso_to_ts(market.get("start_time"))
    end_ts = iso_to_ts(market.get("end_time"))
    outcome = str(activity.get("outcome") or "")
    side = str(activity.get("side") or "")

    joined = {
        "wallet": activity.get("proxyWallet"),
        "timestamp": activity_ts,
        "timestamp_iso": activity_iso,
        "slug": activity.get("slug"),
        "event_slug": activity.get("eventSlug"),
        "type": activity.get("type"),
        "side": side,
        "outcome": outcome,
        "tx_hash": activity.get("transactionHash"),
        "size": activity.get("size"),
        "usdc_size": activity.get("usdcSize"),
        "fill_price": activity.get("price"),
        "market_id": market.get("market_id"),
        "market_start_time": market.get("start_time"),
        "market_end_time": market.get("end_time"),
        "price_start": market.get("price_start"),
        "price_end": market.get("price_end"),
        "winner": market.get("winner"),
        "market_data_source": market.get("market_data_source"),
        "seconds_from_start": activity_ts - start_ts if start_ts is not None else None,
        "seconds_to_end": end_ts - activity_ts if end_ts is not None else None,
        "snapshot_found": snap is not None,
    }

    if snap is None:
        return joined

    snapshot_time = str(snap["time"])
    snapshot_ts = iso_to_ts(snapshot_time)
    best_bid_up = snap.get("best_bid_up")
    best_ask_up = snap.get("best_ask_up")
    best_bid_down = snap.get("best_bid_down")
    best_ask_down = snap.get("best_ask_down")
    chosen_best_bid = None
    chosen_best_ask = None
    if outcome == "Up":
        chosen_best_bid = best_bid_up
        chosen_best_ask = best_ask_up
    elif outcome == "Down":
        chosen_best_bid = best_bid_down
        chosen_best_ask = best_ask_down

    fill_price = activity.get("price")
    fill_vs_best_ask = None
    fill_vs_best_bid = None
    if fill_price is not None and chosen_best_ask is not None:
        fill_vs_best_ask = float(fill_price) - float(chosen_best_ask)
    if fill_price is not None and chosen_best_bid is not None:
        fill_vs_best_bid = float(fill_price) - float(chosen_best_bid)

    joined.update(
        {
            "snapshot_time": snapshot_time,
            "snapshot_lag_seconds": activity_ts - snapshot_ts if snapshot_ts is not None else None,
            "underlying_price": snap.get("price"),
            "price_up": snap.get("price_up"),
            "price_down": snap.get("price_down"),
            "best_bid_up": best_bid_up,
            "best_ask_up": best_ask_up,
            "bid_size_up": snap.get("bid_size_up"),
            "ask_size_up": snap.get("ask_size_up"),
            "best_bid_down": best_bid_down,
            "best_ask_down": best_ask_down,
            "bid_size_down": snap.get("bid_size_down"),
            "ask_size_down": snap.get("ask_size_down"),
            "ask_sum": (
                float(best_ask_up) + float(best_ask_down)
                if best_ask_up is not None and best_ask_down is not None
                else None
            ),
            "bid_sum": (
                float(best_bid_up) + float(best_bid_down)
                if best_bid_up is not None and best_bid_down is not None
                else None
            ),
            "chosen_best_bid": chosen_best_bid,
            "chosen_best_ask": chosen_best_ask,
            "fill_vs_best_ask": fill_vs_best_ask,
            "fill_vs_best_bid": fill_vs_best_bid,
            "fill_near_touch": abs(fill_vs_best_ask or 0.0) <= 0.02 if side == "BUY" and fill_vs_best_ask is not None else None,
        }
    )
    return joined


def summarize(joined_rows: list[dict[str, Any]], unmatched_rows: list[dict[str, Any]], activity_path: Path) -> dict[str, Any]:
    joined_count = sum(1 for row in joined_rows if row.get("snapshot_found"))
    supported = len(joined_rows) + len(unmatched_rows)
    type_counts = Counter(row["type"] for row in joined_rows)
    hedged_markets = defaultdict(set)
    ask_sum_lt_one = 0
    near_touch_buys = 0
    buy_rows_with_touch = 0
    for row in joined_rows:
        if row.get("type") == "TRADE" and row.get("outcome"):
            hedged_markets[row["slug"]].add(row["outcome"])
        if row.get("ask_sum") is not None and row["ask_sum"] < 1.0:
            ask_sum_lt_one += 1
        if row.get("type") == "TRADE" and row.get("side") == "BUY" and row.get("fill_near_touch") is not None:
            buy_rows_with_touch += 1
            if row["fill_near_touch"]:
                near_touch_buys += 1

    return {
        "activity_file": str(activity_path),
        "total_activity_rows": supported,
        "joined_rows": len(joined_rows),
        "joined_snapshot_rows": joined_count,
        "unmatched_rows": len(unmatched_rows),
        "join_rate": joined_count / supported if supported else 0.0,
        "type_counts": dict(type_counts),
        "hedged_market_count": sum(1 for sides in hedged_markets.values() if {"Up", "Down"}.issubset(sides)),
        "market_count": len(hedged_markets),
        "rows_with_ask_sum_lt_one": ask_sum_lt_one,
        "buy_rows_near_touch": near_touch_buys,
        "buy_rows_with_touch_metric": buy_rows_with_touch,
        "latest_joined_timestamp": max((row["timestamp"] for row in joined_rows), default=None),
        "earliest_joined_timestamp": min((row["timestamp"] for row in joined_rows), default=None),
        "unmatched_reasons": dict(Counter(row["reason"] for row in unmatched_rows)),
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--wallet", help="Wallet address used to resolve local activity file")
    ap.add_argument("--activity", help="Explicit local activity JSON path")
    ap.add_argument("--limit", type=int, default=0, help="Optional max number of rows to process")
    ap.add_argument("--output", help="Optional explicit output JSON path")
    args = ap.parse_args()

    activity_path = resolve_activity_path(wallet=args.wallet, activity_path=args.activity)
    rows = json.loads(activity_path.read_text())
    if args.limit:
        rows = rows[: args.limit]

    stores = {asset: CompositeSnapshotStore(paths) for asset, paths in DBS_BY_ASSET.items()}
    joined_rows: list[dict[str, Any]] = []
    unmatched_rows: list[dict[str, Any]] = []

    for row in rows:
        slug = str(row.get("slug") or "")
        asset = infer_asset(slug)
        if asset is None:
            unmatched_rows.append({"slug": slug, "timestamp": row.get("timestamp"), "reason": "unsupported_asset"})
            continue
        market, store = stores[asset].market_for_slug(slug)
        if market is None:
            unmatched_rows.append({"slug": slug, "timestamp": row.get("timestamp"), "reason": "market_not_in_backtest_db"})
            continue
        market["market_data_source"] = str(store.db_path) if store is not None else None
        snap = store.nearest_before(market, ts_to_iso_z(int(row["timestamp"]))) if store is not None else None
        if snap is None:
            unmatched_rows.append({"slug": slug, "timestamp": row.get("timestamp"), "reason": "no_snapshot_before_timestamp"})
            continue
        joined_rows.append(compute_joined_row(row, market, snap))

    summary = summarize(joined_rows, unmatched_rows, activity_path)
    output = {
        "summary": summary,
        "joined_rows": joined_rows,
        "unmatched_rows": unmatched_rows[:500],
    }

    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    wallet_dir = OUTPUT_DIR / (args.wallet.lower()[-6:] if args.wallet else activity_path.parent.name)
    output_dir = wallet_dir if wallet_dir.exists() else OUTPUT_DIR
    if activity_path.name == "activity.json":
        default_name = "market_join.json"
    elif activity_path.name.startswith("activity_"):
        default_name = activity_path.stem.replace("activity_", "") + "_market_join.json"
    else:
        default_name = activity_path.stem + "_market_join.json"
    output_path = Path(args.output) if args.output else output_dir / default_name
    output_path.write_text(json.dumps(output, indent=2))

    print(json.dumps(summary, indent=2))
    print(f"Saved: {output_path}")


if __name__ == "__main__":
    main()
