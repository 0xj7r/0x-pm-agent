# Methodology — how this data was pulled

Captured: 2026-04-22

Reproducible in any Python env with `httpx`. Everything below was hit from a
normal residential IP; no API key required.

## 1. Wallet resolution

Polymarket profile URLs use one of two formats:

- `https://polymarket.com/@<pseudonym>` e.g. `@xuanxuan008`
- `https://polymarket.com/@<wallet_with_suffix>` e.g. `@0xb27b...`

The canonical identifier is the on-chain **proxy wallet** (Ethereum address).
Given a pseudonym or a truncated address, resolve it by fetching the HTML page
and pulling the most-frequent `0x[a-f0-9]{40}` match (it appears ~60× in the
hydration JSON):

```python
import httpx, re
from collections import Counter

def resolve(handle: str) -> str:
    r = httpx.get(
        f"https://polymarket.com/@{handle}",
        follow_redirects=True,
        timeout=20,
        headers={"User-Agent": "Mozilla/5.0"},
    )
    matches = re.findall(r"0x[a-fA-F0-9]{40}", r.text)
    return Counter(matches).most_common(1)[0][0]
```

Wallets used in this dataset:

| Label | Pseudonym | Wallet |
|-------|-----------|--------|
| w1 Unlawful-Shear | — | `0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82` |
| w2 xuanxuan008   | `xuanxuan008` | `0xcfb103c37c0234f524c632d964ed31f117b5f694` |
| w3 SPLIT/SELL    | — | `0xe51b3d64da5b0b8a07a55f8bb3c3170237f73cad` |
| w4 penny-tail    | — | `0x7da07b2a8b009a406198677debda46ad651b6be2` |

## 2. Activity rows — `data-api.polymarket.com`

Endpoint used to populate `<wallet-dir>/activity.json`:

```
GET https://data-api.polymarket.com/activity
    ?user=<wallet>
    &limit=500
    &offset=<0,500,1000,...>
```

Each row is one of: `TRADE` (side = BUY or SELL), `MERGE`, `SPLIT`, `REDEEM`.
Sort order is newest-first by `timestamp`.

Row schema (fields we use):
```
proxyWallet, timestamp, conditionId, type, size, usdcSize,
transactionHash, price, asset, side, outcomeIndex, title, slug,
eventSlug, outcome
```

**Known caps / caveats:**

- **Offset cap at 3500.** Requesting `offset=3500` returns HTTP 400. This bounds
  the fetchable window to the most recent 3500 events for a heavy trader — for
  w1 that's only ~1 hour of history. Time-based filters (`start`, `endTime`,
  `before`, `startDate`, `startTimestamp`) were all tested; **none are honored**
  by this endpoint — it always returns newest-first, no time slicing.
- **SPLIT/MERGE/REDEEM rows** have `side=""` and `outcome=""`. `usdcSize` still
  carries the USDC value of the operation (minted/merged/redeemed).
- For a SPLIT, `usdcSize` = USDC paid in to mint Up+Down pairs (1:1).
- For a MERGE, `usdcSize` = USDC returned when pairing Up+Down back to $1.
- For a REDEEM, `usdcSize` = USDC paid out for the winning side.
- TRADE rows: `usdcSize` = `size × price`, the quote value of the fill.
- **5-min BTC markets** are identifiable by slug prefix `btc-updown-5m-`. The
  trailing integer is the window start unix timestamp.

Pagination loop:
```python
rows, offset, limit = [], 0, 500
while True:
    r = httpx.get(
        "https://data-api.polymarket.com/activity",
        params={"user": wallet, "limit": limit, "offset": offset},
        timeout=60,
    )
    if r.status_code == 400:        # offset cap reached
        break
    r.raise_for_status()
    batch = r.json()
    if not batch:
        break
    rows.extend(batch)
    if len(batch) < limit:
        break
    offset += limit
```

## 3. P&L time series — `user-pnl-api.polymarket.com`

This is the **authoritative** long-horizon P&L source; it's what renders the
profile P&L chart. Populates `<wallet-dir>/pnl_timeseries.json`.

```
GET https://user-pnl-api.polymarket.com/user-pnl
    ?user_address=<wallet>
    &interval=all
```

Response: list of `{"t": <unix_seconds>, "p": <pnl_usdc>}` points.
Fidelity observed at 1 hour. `interval` values observed to work: `all` (full
history). Other fidelity params (`fidelity=1h`) were rejected.

This series is **cumulative P&L**, starting at the wallet's first trading
activity. A positive slope = net USDC gained; flat = idle hours.

Not yet explicitly confirmed but likely includes:
- Realized P&L from trades + redemptions
- Mark-to-market of open positions at the tick time
- Maker rebates / incentive rewards if Polymarket credits them on-chain

## 4. Current positions value — `data-api.polymarket.com/value`

Quick cross-check of current exposed inventory (used as sanity check):
```
GET https://data-api.polymarket.com/value?user=<wallet>
→ [{"user":"<wallet>","value":<usdc>}]
```

## 5. Data files in this directory

| File | Source | Notes |
|------|--------|-------|
| `<wallet-dir>/activity.json` | `/activity` endpoint | Up to 3500 newest rows |
| `<wallet-dir>/pnl_timeseries.json` | `/user-pnl` endpoint | Full-history hourly |
| `<wallet-dir>/daily_pnl.json` | derived from above | EOD UTC snapshots |
| `comparison.json` | derived | Aggregate stats across wallets |

Scripts used:
- `scripts/analyze_whale.py` — fetches `/activity` for one wallet and prints
  per-window P&L including MERGE/REDEEM accounting.
- `scripts/save_whale_analysis.py` — fetches `/user-pnl` for all four wallets
  and writes the artifacts in this directory.

## 6. Known limitations of the analysis

- **3500-row cap on /activity** means micro-level (per-order) analysis is
  limited to the most recent ~1 hour for w1, ~day for w2, ~week for w3/w4.
  Use `user-pnl` for longer horizons.
- **P&L accounting from /activity alone is incomplete**: it may miss maker
  rebates, incentive payouts, or any USDC movements outside the TRADE path.
  For long-horizon P&L, `user-pnl` is the SSOT.
- **Entry-timing analysis** uses `timestamp - window_start_unix`. Blockchain
  timestamps are confirmation times, not submit times; expect 1-3s latency
  between actual order submit and observed `timestamp`.
- **"Outcome" field** is Up/Down for TRADE rows but the market's resolved
  outcome is not in the row — resolve via `gamma-api.polymarket.com/events
  ?slug=<slug>` if needed.
