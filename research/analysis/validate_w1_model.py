"""Layer-1 validation: can we reproduce w1's P&L from his activity data?

Inputs:
  - data/research/whale_analysis/8b5b82/activity.json  (3500 newest activity rows)
  - data/research/whale_analysis/8b5b82/pnl_timeseries.json  (hourly user-pnl from Polymarket)

Model (our thesis of w1's edge):
  - BUY cost:    sum(usdcSize) for TRADE BUYs
  - SELL proc.:  sum(usdcSize) for TRADE SELLs     (w1: always 0)
  - MERGE proc.: sum(usdcSize) for MERGE ops       ($1 per merged pair)
  - REDEEM proc: sum(usdcSize) for REDEEM ops      ($1 per winning share)
  - Net realized = proc - cost  (no fees yet)
  - Unrealized at activity horizon: value of any leftover unhedged inventory
    at then-prevailing mid prices.

Ground truth:
  Delta of `p` from user-pnl series between activity start hour and end hour
  (cumulative P&L chart). Any residual delta between our model and truth
  reveals a gap — typically fees, maker rebates, or accounting differences.
"""
from __future__ import annotations

import json
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path
from statistics import median

import httpx

DATA = Path(__file__).parent.parent / "data" / "research" / "whale_analysis" / "8b5b82"
ACT_PATH = DATA / "activity.json"
PNL_PATH = DATA / "pnl_timeseries.json"
GAMMA = "https://gamma-api.polymarket.com"


def hour_floor(ts: int) -> int:
    return ts - (ts % 3600)


def main() -> None:
    rows = json.loads(ACT_PATH.read_text())
    pnl = json.loads(PNL_PATH.read_text())

    btc5 = [r for r in rows if (r.get("slug") or "").startswith("btc-updown-5m-")]
    trades = [r for r in btc5 if r.get("type") == "TRADE"]
    buys = [r for r in trades if r.get("side") == "BUY"]
    sells = [r for r in trades if r.get("side") == "SELL"]
    merges = [r for r in btc5 if r.get("type") == "MERGE"]
    redeems = [r for r in btc5 if r.get("type") == "REDEEM"]

    print("=== Activity window ===")
    ts_min = min(r["timestamp"] for r in btc5)
    ts_max = max(r["timestamp"] for r in btc5)
    print(f"  span: {datetime.fromtimestamp(ts_min, tz=timezone.utc).isoformat()}")
    print(f"     -> {datetime.fromtimestamp(ts_max, tz=timezone.utc).isoformat()}")
    print(f"  duration: {(ts_max - ts_min)/60:.1f} min")
    print(f"  BUYs={len(buys)}  SELLs={len(sells)}  MERGEs={len(merges)}  REDEEMs={len(redeems)}")

    buy_cost = sum(float(r.get("usdcSize") or 0) for r in buys)
    sell_proc = sum(float(r.get("usdcSize") or 0) for r in sells)
    merge_proc = sum(float(r.get("usdcSize") or 0) for r in merges)
    redeem_proc = sum(float(r.get("usdcSize") or 0) for r in redeems)
    realized = sell_proc + merge_proc + redeem_proc - buy_cost
    print(f"\n=== Our model (realized only) ===")
    print(f"  BUY cost:       ${buy_cost:>12,.2f}")
    print(f"  SELL proceeds:  ${sell_proc:>12,.2f}")
    print(f"  MERGE proceeds: ${merge_proc:>12,.2f}")
    print(f"  REDEEM proc.:   ${redeem_proc:>12,.2f}")
    print(f"  NET realized:   ${realized:>+12,.2f}")

    # Leftover unhedged inventory at end of activity window.
    # Per-outcome net shares bought but NOT yet merged/redeemed.
    # shares_held[slug][outcome] = net shares (BUYs minus merged minus redeemed)
    shares = defaultdict(lambda: defaultdict(float))
    for r in buys:
        shares[r["slug"]][r["outcome"]] += float(r.get("size") or 0)
    # MERGE burns equal shares of Up AND Down in a given market.
    merged_pairs = defaultdict(float)
    for r in merges:
        # MERGE rows are identified by conditionId, not slug. Cross-walk:
        # pick the slug that matches this conditionId.
        cid = r.get("conditionId")
        # Find matching slug from a BUY row in that condition.
        match_slug = None
        for slug, by_out in shares.items():
            for b in buys:
                if b["slug"] == slug and b.get("conditionId") == cid:
                    match_slug = slug
                    break
            if match_slug:
                break
        if match_slug:
            merged_pairs[match_slug] += float(r.get("size") or 0)
    # Actually MERGE 'size' = number of pairs merged. Each pair = 1 Up + 1 Down.
    # So subtract from BOTH outcomes equally.
    for slug, pairs in merged_pairs.items():
        shares[slug]["Up"] -= pairs
        shares[slug]["Down"] -= pairs

    # REDEEM: winning side shares converted to USDC. Subtract winning-outcome
    # shares. We don't know which side won for each REDEEM without gamma lookup,
    # but redeem usdcSize == size of winning-side shares (1:1 at resolution).
    # Simpler proxy: infer from outcome field if present (it's blank for REDEEM).
    # Fall back: subtract min(Up,Down) shares of the "opposite" of the non-merged
    # side — too much guesswork. Skip redemption-side inference and just flag.
    print(f"\n=== Open inventory after MERGEs ===")
    n_markets_with_leftover = 0
    total_up_left = 0.0
    total_dn_left = 0.0
    for slug, by_out in shares.items():
        up = by_out.get("Up", 0.0)
        dn = by_out.get("Down", 0.0)
        if abs(up) > 0.1 or abs(dn) > 0.1:
            n_markets_with_leftover += 1
            total_up_left += max(up, 0)
            total_dn_left += max(dn, 0)
    print(f"  markets with residual shares: {n_markets_with_leftover}")
    print(f"  total Up shares left:   {total_up_left:>12,.2f}")
    print(f"  total Down shares left: {total_dn_left:>12,.2f}")

    # Mark leftover to market by fetching current gamma prices for each window.
    # For resolved windows, the leftover is either $1 or $0 per share.
    # For unresolved, use mid-price from gamma.
    print(f"\n  (pricing leftover inventory via gamma)")
    client = httpx.Client(timeout=15.0)
    unrealized = 0.0
    for slug, by_out in shares.items():
        up = by_out.get("Up", 0.0)
        dn = by_out.get("Down", 0.0)
        if abs(up) < 0.1 and abs(dn) < 0.1:
            continue
        try:
            r = client.get(f"{GAMMA}/events", params={"slug": slug}, timeout=10)
            ev = r.json()[0] if r.status_code == 200 and r.json() else None
        except Exception:
            continue
        if not ev:
            continue
        m = (ev.get("markets") or [{}])[0]
        closed = ev.get("closed")
        op = m.get("outcomePrices")
        if isinstance(op, str):
            op = json.loads(op)
        up_p = float(op[0]) if op and len(op) >= 1 else 0.5
        dn_p = float(op[1]) if op and len(op) >= 2 else 0.5
        # If resolved, price is $1 or $0 exactly; if not, mid.
        unrealized += up * up_p + dn * dn_p
    print(f"  unrealized mark:        ${unrealized:>12,.2f}")
    model_total = realized + unrealized
    print(f"\n  MODEL (realized + unrealized) = ${model_total:>+,.2f}")

    # Ground truth from user-pnl series — pick start/end points closest to
    # the activity bracket.
    print(f"\n=== Ground truth (user-pnl) ===")
    pnl_sorted = sorted(pnl, key=lambda p: p["t"])
    # Find point at or just before ts_min, and point at or just after ts_max.
    before = [p for p in pnl_sorted if p["t"] <= ts_min]
    after = [p for p in pnl_sorted if p["t"] >= ts_max]
    if not before or not after:
        print("  (insufficient pnl coverage)")
        return
    p_start = before[-1]
    p_end = after[0]
    # Pick the last available point in the series if ts_max > last pnl point.
    if pnl_sorted[-1]["t"] < ts_max:
        p_end = pnl_sorted[-1]
    delta = p_end["p"] - p_start["p"]
    print(f"  start: {datetime.fromtimestamp(p_start['t'], tz=timezone.utc).isoformat()}  ${p_start['p']:,.2f}")
    print(f"  end:   {datetime.fromtimestamp(p_end['t'], tz=timezone.utc).isoformat()}  ${p_end['p']:,.2f}")
    print(f"  DELTA (truth):  ${delta:>+,.2f}")

    print(f"\n=== RECONCILIATION ===")
    print(f"  model:  ${model_total:>+,.2f}")
    print(f"  truth:  ${delta:>+,.2f}")
    gap = model_total - delta
    print(f"  gap:    ${gap:>+,.2f}  (model minus truth)")
    if abs(delta) > 1:
        print(f"  gap %:  {gap/delta*100:>+.1f}% of truth")


if __name__ == "__main__":
    main()
