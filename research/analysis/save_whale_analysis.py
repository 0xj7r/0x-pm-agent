"""Save whale analysis artifacts for cross-validation.

Outputs under data/research/whale_analysis/<wallet>/:
  - pnl_timeseries.json      — raw user-pnl time series
  - activity.json            — raw /activity rows (copied from data/)
  - comparison.json          — aggregate stats across wallets (kept at wallet root)
  - daily_pnl.json           — EOD P&L series per wallet
  - README.md               — what each file is and how it was fetched
"""
from __future__ import annotations

import json
import shutil
from datetime import datetime, timezone
from pathlib import Path

import httpx

WALLETS = [
    ("Unlawful-Shear (w1)", "0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82"),
    ("xuanxuan008 (w2)",    "0xcfb103c37c0234f524c632d964ed31f117b5f694"),
    ("SPLIT/SELL (w3)",     "0xe51b3d64da5b0b8a07a55f8bb3c3170237f73cad"),
    ("penny-tail (w4)",     "0x7da07b2a8b009a406198677debda46ad651b6be2"),
]

ROOT = Path(__file__).parent.parent
OUT = ROOT / "data" / "research" / "whale_analysis"
OUT.mkdir(parents=True, exist_ok=True)


def fetch_pnl(wallet: str) -> list[dict]:
    r = httpx.get(
        f"https://user-pnl-api.polymarket.com/user-pnl?user_address={wallet}&interval=all",
        timeout=30,
    )
    r.raise_for_status()
    return r.json()


def daily_eod(ts_series: list[dict]) -> dict[str, float]:
    by_day: dict[str, float] = {}
    for d in ts_series:
        day = datetime.fromtimestamp(d["t"], tz=timezone.utc).strftime("%Y-%m-%d")
        by_day[day] = d["p"]
    return dict(sorted(by_day.items()))


def summarize(wallet: str, label: str, series: list[dict]) -> dict:
    if not series:
        return {"label": label, "wallet": wallet, "error": "no data"}
    first_t = datetime.fromtimestamp(series[0]["t"], tz=timezone.utc)
    last_t  = datetime.fromtimestamp(series[-1]["t"], tz=timezone.utc)
    peak = max(d["p"] for d in series)
    trough = min(d["p"] for d in series)
    current = series[-1]["p"]
    running_max = series[0]["p"]
    max_dd = 0.0
    for d in series:
        running_max = max(running_max, d["p"])
        max_dd = max(max_dd, running_max - d["p"])
    eod = daily_eod(series)
    prev = 0.0
    up = down = 0
    best = worst = 0.0
    for day, p in eod.items():
        delta = p - prev
        if delta > 0.5: up += 1
        elif delta < -0.5: down += 1
        best = max(best, delta)
        worst = min(worst, delta)
        prev = p
    return {
        "label": label,
        "wallet": wallet,
        "points": len(series),
        "first_ts": series[0]["t"],
        "last_ts": series[-1]["t"],
        "first_date": first_t.isoformat(),
        "last_date": last_t.isoformat(),
        "days": (last_t - first_t).total_seconds() / 86400,
        "current_pnl": current,
        "peak_pnl": peak,
        "trough_pnl": trough,
        "max_drawdown_abs": max_dd,
        "max_drawdown_pct_of_peak": max_dd / peak * 100 if peak > 0 else 0,
        "up_days": up,
        "down_days": down,
        "best_day": best,
        "worst_day": worst,
    }


def main() -> None:
    comparison = []
    for label, wallet in WALLETS:
        last6 = wallet[-6:]
        wallet_dir = OUT / last6
        wallet_dir.mkdir(parents=True, exist_ok=True)
        print(f"Fetching P&L for {label} ({wallet})...")
        series = fetch_pnl(wallet)
        (wallet_dir / "pnl_timeseries.json").write_text(json.dumps(series, indent=2))
        (wallet_dir / "daily_pnl.json").write_text(json.dumps(daily_eod(series), indent=2))
        # Copy the activity file if it exists.
        act_candidates = [
            wallet_dir / "activity.json",
            ROOT / "data" / f"whale_activity_{last6}.json",
        ]
        act = next((c for c in act_candidates if c.exists()), None)
        if act is not None:
            shutil.copy(act, wallet_dir / "activity.json")
        comparison.append(summarize(wallet, label, series))

    (OUT / "comparison.json").write_text(json.dumps(comparison, indent=2))

    readme = f"""# Whale Analysis Artifacts
Captured: {datetime.now(timezone.utc).isoformat()}

## Wallets analyzed
"""
    for label, w in WALLETS:
        readme += f"- **{label}** — `{w}` — https://polymarket.com/@{w}\n"
    readme += """
## Files

### `pnl_timeseries.json`
Raw time-series P&L from Polymarket's internal endpoint:
```
GET https://user-pnl-api.polymarket.com/user-pnl?user_address=<wallet>&interval=all
```
Each point: `{"t": unix_ts, "p": pnl_usdc}`. Hourly fidelity. This is the same
series that powers the green/purple P&L chart on each profile page.

### `daily_pnl.json`
End-of-UTC-day P&L snapshots derived from the time series above. Useful for
day-level comparison across wallets.

### `activity.json`
Raw activity rows from `data-api.polymarket.com/activity?user=<wallet>`, up to
the 3500-row API cap (newest first). Each row is a TRADE / MERGE / SPLIT / REDEEM
event with conditionId, slug, price, size, usdcSize, outcome, transactionHash.

### `comparison.json`
Aggregate stats per wallet (current P&L, peak, drawdown, up/down days, best/worst days).

## Strategy labels inferred from activity

| Label | Pattern | Edge mechanism |
|-------|---------|----------------|
| Unlawful-Shear (w1) | spray BUYs on both sides < $0.50, MERGE paired fills | bid/ask spread capture via pair-merge arb |
| xuanxuan008 (w2) | same as w1, smaller per-window size | same |
| SPLIT/SELL (w3) | SPLIT $1 USDC → sell one side > $0.50, hold other to REDEEM | implied-prob spread when Up+Down > $1 |
| penny-tail (w4) | single-side BUYs at $0.01-$0.03 in last 60s of window | tail/convexity — buy cheap optionality |

Our live bot runs closest to w4's regime but with `max_entry=0.55` instead of ≤$0.03.
"""
    (OUT / "README.md").write_text(readme)
    print(f"\nSaved to {OUT}")
    for p in sorted(OUT.iterdir()):
        print(f"  {p.name}  ({p.stat().st_size:,} bytes)")


if __name__ == "__main__":
    main()
