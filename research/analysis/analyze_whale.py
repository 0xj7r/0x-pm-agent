"""Pull + analyze activity for a Polymarket whale wallet on 5-min BTC markets.

Usage: python3 scripts/analyze_whale.py [wallet_address]
Default wallet: the Unlawful-Shear account crushing 5-min BTC markets.
"""
from __future__ import annotations

import json
import sys
import time
from collections import Counter, defaultdict
from pathlib import Path
from statistics import mean, median

import httpx

DATA_API = "https://data-api.polymarket.com"
GAMMA_API = "https://gamma-api.polymarket.com"
DEFAULT_WALLET = "0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82"


def fetch_activity(wallet: str) -> list[dict]:
    rows: list[dict] = []
    limit = 500
    offset = 0
    while True:
        r = httpx.get(
            f"{DATA_API}/activity",
            params={"user": wallet, "limit": limit, "offset": offset},
            timeout=60,
        )
        if r.status_code == 400:
            # API caps offset at 3500. Stop silently.
            break
        r.raise_for_status()
        batch = r.json()
        if not isinstance(batch, list) or not batch:
            break
        rows.extend(batch)
        if len(batch) < limit:
            break
        offset += limit
        time.sleep(0.2)
    return rows


def fetch_event_outcome(slug: str) -> dict | None:
    """Get the event's resolved outcome via gamma-api."""
    try:
        r = httpx.get(f"{GAMMA_API}/events", params={"slug": slug}, timeout=15)
        r.raise_for_status()
        evs = r.json()
        if isinstance(evs, list) and evs:
            return evs[0]
    except Exception:
        return None
    return None


def is_btc_5m(row: dict) -> bool:
    slug = row.get("slug") or ""
    return slug.startswith("btc-updown-5m-")


def window_start_ts(slug: str) -> int | None:
    try:
        return int(slug.rsplit("-", 1)[-1])
    except Exception:
        return None


def analyze(rows: list[dict]) -> None:
    btc5 = [r for r in rows if is_btc_5m(r)]
    print(f"Total activity rows: {len(rows)}")
    print(f"BTC 5-min market rows: {len(btc5)}")
    if not btc5:
        return

    by_type = Counter(r.get("type") for r in btc5)
    print(f"\nBy type: {dict(by_type)}")
    by_side = Counter(r.get("side") for r in btc5 if r.get("type") == "TRADE")
    print(f"By side: {dict(by_side)}")
    by_outcome = Counter(r.get("outcome") for r in btc5 if r.get("type") == "TRADE")
    print(f"By outcome (Up/Down): {dict(by_outcome)}")

    ts_min = min(r["timestamp"] for r in btc5)
    ts_max = max(r["timestamp"] for r in btc5)
    from datetime import datetime, timezone
    print(
        f"\nDate range: "
        f"{datetime.fromtimestamp(ts_min, tz=timezone.utc).isoformat()} → "
        f"{datetime.fromtimestamp(ts_max, tz=timezone.utc).isoformat()}"
    )

    buys = [r for r in btc5 if r.get("type") == "TRADE" and r.get("side") == "BUY"]
    sells = [r for r in btc5 if r.get("type") == "TRADE" and r.get("side") == "SELL"]
    redeems = [r for r in btc5 if r.get("type") == "REDEEM"]

    print(f"\nBUYs: {len(buys)}  SELLs: {len(sells)}  REDEEMs: {len(redeems)}")

    if buys:
        prices = [float(r["price"]) for r in buys if r.get("price") is not None]
        sizes_usd = [float(r["usdcSize"]) for r in buys if r.get("usdcSize") is not None]
        print(f"\nBUY entry-price — min/med/mean/max: "
              f"{min(prices):.3f} / {median(prices):.3f} / "
              f"{mean(prices):.3f} / {max(prices):.3f}")
        print(f"BUY size (USDC)  — min/med/mean/max/sum: "
              f"${min(sizes_usd):.2f} / ${median(sizes_usd):.2f} / "
              f"${mean(sizes_usd):.2f} / ${max(sizes_usd):.2f} / "
              f"${sum(sizes_usd):.2f}")

    # Timing within 5-min window: seconds from window start to trade timestamp.
    offsets = []
    for r in buys:
        ws = window_start_ts(r.get("slug", ""))
        if ws is None:
            continue
        off = int(r["timestamp"]) - ws
        offsets.append(off)
    if offsets:
        print(f"\nEntry timing (seconds into window):")
        print(f"  min/med/mean/max: "
              f"{min(offsets)}s / {int(median(offsets))}s / "
              f"{int(mean(offsets))}s / {max(offsets)}s")
        # Histogram buckets (window is 300s).
        buckets = Counter()
        for off in offsets:
            if off < 0:
                buckets["<0 (pre-window)"] += 1
            elif off < 60:
                buckets["0-60s"] += 1
            elif off < 120:
                buckets["60-120s"] += 1
            elif off < 180:
                buckets["120-180s"] += 1
            elif off < 240:
                buckets["180-240s"] += 1
            elif off < 300:
                buckets["240-300s"] += 1
            else:
                buckets[">300s (late)"] += 1
        print(f"  Distribution:")
        for k in ["<0 (pre-window)", "0-60s", "60-120s", "120-180s",
                 "180-240s", "240-300s", ">300s (late)"]:
            if buckets.get(k):
                print(f"    {k:20s} {buckets[k]}")

    # Per-window aggregation: one wallet may do multiple BUYs per window.
    by_window = defaultdict(list)
    for r in buys:
        by_window[r.get("slug")].append(r)
    print(f"\nDistinct windows traded: {len(by_window)}")
    multi = {k: v for k, v in by_window.items() if len(v) > 1}
    print(f"Windows with >1 BUY: {len(multi)}")

    # Proceeds by conditionId — include REDEEM (winner payout), MERGE (pair→$1),
    # and SELL TRADE rows.
    redeem_by_cond = defaultdict(float)
    for r in redeems:
        cid = r.get("conditionId")
        redeem_by_cond[cid] += float(r.get("usdcSize") or 0)

    merge_by_cond = defaultdict(float)
    merges = [r for r in btc5 if r.get("type") == "MERGE"]
    for r in merges:
        cid = r.get("conditionId")
        merge_by_cond[cid] += float(r.get("usdcSize") or 0)
    print(f"\nMERGEs: {len(merges)}  total merge proceeds: "
          f"${sum(merge_by_cond.values()):,.2f}")

    # Per-side BUY totals per window (Up vs Down exposure).
    up_cost_by_slug = defaultdict(float)
    dn_cost_by_slug = defaultdict(float)
    up_shares_by_slug = defaultdict(float)
    dn_shares_by_slug = defaultdict(float)
    for r in buys:
        slug = r.get("slug")
        cost = float(r.get("usdcSize") or 0)
        shares = float(r.get("size") or 0)
        if r.get("outcome") == "Up":
            up_cost_by_slug[slug] += cost
            up_shares_by_slug[slug] += shares
        else:
            dn_cost_by_slug[slug] += cost
            dn_shares_by_slug[slug] += shares

    # P&L per window.
    wins = losses = pushes = 0
    total_pnl = 0.0
    detail: list[tuple[str, float, float, float, float, float]] = []
    for slug, rs in by_window.items():
        cid = rs[0].get("conditionId")
        cost = sum(float(r.get("usdcSize") or 0) for r in rs)
        sell_proceeds = sum(
            float(r.get("usdcSize") or 0)
            for r in btc5
            if r.get("conditionId") == cid
            and r.get("type") == "TRADE"
            and r.get("side") == "SELL"
        )
        redeem = redeem_by_cond.get(cid, 0.0)
        merge = merge_by_cond.get(cid, 0.0)
        proceeds = sell_proceeds + redeem + merge
        pnl = proceeds - cost
        total_pnl += pnl
        up_c = up_cost_by_slug.get(slug, 0.0)
        dn_c = dn_cost_by_slug.get(slug, 0.0)
        detail.append((slug, cost, proceeds, pnl, up_c, dn_c))
        if pnl > 0.01:
            wins += 1
        elif pnl < -0.01:
            losses += 1
        else:
            pushes += 1

    print(f"\nPer-window P&L (BUY cost vs SELL+REDEEM+MERGE proceeds):")
    print(f"  Wins:   {wins}")
    print(f"  Losses: {losses}")
    print(f"  Pushes/unresolved: {pushes}")
    if wins + losses > 0:
        print(f"  Win-rate (of resolved): {wins/(wins+losses)*100:.1f}%")
    print(f"  Total realized P&L: ${total_pnl:+.2f}")
    # Per-window: is he hedged (both sides) or directional?
    hedged = sum(1 for d in detail if d[4] > 1 and d[5] > 1)
    print(f"  Hedged windows (Up>$1 and Down>$1): {hedged}/{len(detail)}")

    detail.sort(key=lambda x: x[3], reverse=True)
    print(f"\nAll {len(detail)} windows (sorted by P&L, newest data):")
    print(f"  {'slug':35s} {'upBUY':>9s} {'dnBUY':>9s} {'proceeds':>9s} {'pnl':>9s}")
    for slug, cost, proceeds, pnl, up_c, dn_c in detail:
        print(f"  {slug:35s} ${up_c:8.2f} ${dn_c:8.2f} ${proceeds:8.2f} ${pnl:+8.2f}")


def main() -> None:
    wallet = sys.argv[1] if len(sys.argv) > 1 else DEFAULT_WALLET
    print(f"Fetching activity for {wallet} ...")
    rows = fetch_activity(wallet)
    print(f"Fetched {len(rows)} rows")
    # Dump raw for later inspection.
    out = Path(__file__).parent.parent / "data" / f"whale_activity_{wallet[-6:]}.json"
    out.parent.mkdir(exist_ok=True)
    out.write_text(json.dumps(rows, indent=2))
    print(f"Saved raw to {out}")
    analyze(rows)


if __name__ == "__main__":
    main()
