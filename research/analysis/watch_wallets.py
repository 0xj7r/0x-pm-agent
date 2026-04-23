#!/usr/bin/env python3
"""Poll Polymarket Data API activity+positions for a watchlist of wallets.

Appends dedup'd events to research/wallet_profiles/activity_log/<handle>.jsonl.
Designed to be called periodically by cron/systemd/docker — each invocation
runs one poll cycle and exits.

Dedup key: Polygon transactionHash. Events without a transactionHash (rare)
fall back to a composite key of (timestamp, type, asset, side, size).

Usage:
    python3 scripts/watch_wallets.py
    python3 scripts/watch_wallets.py --index research/wallet_profiles/index.json
    python3 scripts/watch_wallets.py --wallet 0xb27b... --handle goat
"""
from __future__ import annotations

import argparse
import json
import pathlib
import sys
import time
from datetime import UTC, datetime

import httpx

DATA_API = "https://data-api.polymarket.com"
DEFAULT_INDEX = pathlib.Path("research/wallet_profiles/index.json")
DEFAULT_LOG_DIR = pathlib.Path("research/wallet_profiles/activity_log")


def event_key(ev: dict) -> str:
    tx = ev.get("transactionHash")
    if tx:
        return f"tx:{tx}"
    return (
        f"comp:{ev.get('timestamp')}:{ev.get('type')}:{ev.get('asset')}:"
        f"{ev.get('side')}:{ev.get('size')}"
    )


def load_existing_keys(path: pathlib.Path) -> set[str]:
    if not path.exists():
        return set()
    keys: set[str] = set()
    with path.open() as f:
        for line in f:
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            ev = row.get("event") or {}
            keys.add(event_key(ev))
    return keys


def fetch_activity(client: httpx.Client, wallet: str, limit: int = 500) -> list[dict]:
    r = client.get(f"{DATA_API}/activity", params={"user": wallet, "limit": limit})
    r.raise_for_status()
    data = r.json()
    return [d for d in data if isinstance(d, dict)]


def fetch_positions(client: httpx.Client, wallet: str) -> list[dict]:
    r = client.get(f"{DATA_API}/positions", params={"user": wallet})
    r.raise_for_status()
    data = r.json()
    return [d for d in data if isinstance(d, dict)]


def poll_wallet(
    client: httpx.Client,
    wallet: str,
    handle: str,
    log_dir: pathlib.Path,
) -> dict:
    log_dir.mkdir(parents=True, exist_ok=True)
    events_path = log_dir / f"{handle}.jsonl"
    positions_path = log_dir / f"{handle}.positions.jsonl"

    existing = load_existing_keys(events_path)
    activity = fetch_activity(client, wallet)
    positions = fetch_positions(client, wallet)
    fetched_at = datetime.now(UTC).isoformat()

    new_events: list[dict] = []
    for ev in activity:
        k = event_key(ev)
        if k in existing:
            continue
        existing.add(k)
        new_events.append(ev)

    # Append new events (append mode keeps the file crash-safe).
    with events_path.open("a") as f:
        for ev in new_events:
            f.write(json.dumps({"fetched_at": fetched_at, "event": ev}) + "\n")

    # Positions snapshot — one JSONL row per poll.
    with positions_path.open("a") as f:
        f.write(
            json.dumps(
                {
                    "fetched_at": fetched_at,
                    "positions": positions,
                }
            )
            + "\n"
        )

    return {
        "handle": handle,
        "wallet": wallet,
        "new_events": len(new_events),
        "total_events_returned": len(activity),
        "positions_count": len(positions),
    }


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--index", type=pathlib.Path, default=DEFAULT_INDEX)
    ap.add_argument("--log-dir", type=pathlib.Path, default=DEFAULT_LOG_DIR)
    ap.add_argument("--wallet", help="Single wallet address (overrides --index).")
    ap.add_argument("--handle", help="Label for the single wallet (with --wallet).")
    ap.add_argument("--timeout", type=float, default=30.0)
    ap.add_argument(
        "--loop-interval-seconds",
        type=float,
        default=0.0,
        help="If > 0, poll continuously with this sleep between cycles.",
    )
    args = ap.parse_args(argv)

    targets: list[tuple[str, str]]
    if args.wallet:
        targets = [(args.wallet, args.handle or args.wallet[:10])]
    else:
        if not args.index.exists():
            print(f"ERROR: index not found: {args.index}", file=sys.stderr)
            return 2
        idx = json.loads(args.index.read_text())
        targets = [(w["address"], w["handle"]) for w in idx.get("wallets", [])]
        if not targets:
            print("ERROR: index has no wallets", file=sys.stderr)
            return 2

    def one_cycle(client: httpx.Client) -> None:
        summary = {
            "run_at": datetime.now(UTC).isoformat(),
            "wallets": [],
        }
        for wallet, handle in targets:
            t0 = time.monotonic()
            try:
                row = poll_wallet(client, wallet, handle, args.log_dir)
                row["elapsed_ms"] = int((time.monotonic() - t0) * 1000)
                summary["wallets"].append(row)
                print(
                    f"  {handle:<18} wallet={wallet[:10]}… "
                    f"new={row['new_events']:>4} total={row['total_events_returned']:>4} "
                    f"pos={row['positions_count']:>2} ({row['elapsed_ms']}ms)",
                    flush=True,
                )
            except Exception as exc:
                err = {"handle": handle, "wallet": wallet, "error": str(exc)}
                summary["wallets"].append(err)
                print(f"  {handle:<18} ERROR: {exc}", file=sys.stderr, flush=True)
        args.log_dir.mkdir(parents=True, exist_ok=True)
        with (args.log_dir / "watch_summary.jsonl").open("a") as f:
            f.write(json.dumps(summary) + "\n")

    with httpx.Client(timeout=args.timeout) as client:
        if args.loop_interval_seconds > 0:
            print(
                f"watch_wallets: looping every {args.loop_interval_seconds}s over "
                f"{len(targets)} wallets",
                flush=True,
            )
            while True:
                cycle_start = time.monotonic()
                print(f"--- {datetime.now(UTC).isoformat()} ---", flush=True)
                one_cycle(client)
                elapsed = time.monotonic() - cycle_start
                sleep_for = max(0.0, args.loop_interval_seconds - elapsed)
                time.sleep(sleep_for)
        else:
            one_cycle(client)

    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
