"""Penny-tail paper bot: emulate w4's strategy, no real orders.

Strategy (reverse-engineered from w4 activity):
  - For each active BTC 5-min window, watch the book.
  - In the last 60s of the window, if either side's price <= $0.03 and
    we haven't already entered, record a paper BUY of $18 notional at
    that price on that side.
  - After the window closes, look up the outcome via Polymarket's
    past-results API. Shares pay $1 if we picked the winner, $0 else.
  - Store decisions + resolutions in SQLite; print a running P&L.

This is strictly read-only against Polymarket — no private key, no orders.
The point is to measure the edge cheaply before risking USDC.

Run once:  python3 scripts/penny_paper_bot.py
Run loop:  python3 scripts/penny_paper_bot.py --loop 5

DB schema (penny_paper_trades):
  id, decided_ts, window_slug, window_start, side, entry_price,
  shares, notional_usd, seconds_to_close, resolved (0/1),
  winning_outcome, won (0/1), pnl_usd
"""
from __future__ import annotations

import argparse
import json
import logging
import sqlite3
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path

import httpx

logger = logging.getLogger("penny_paper")

GAMMA = "https://gamma-api.polymarket.com"
CLOB_BOOK = "https://clob.polymarket.com/book"
PAST_RESULTS = "https://polymarket.com/api/past-results"
BINANCE_KLINES = "https://api.binance.com/api/v3/klines"
SLUG_PREFIX = "btc-updown-5m-"
WINDOW_SECS = 300
ENTRY_WINDOW_LAST_N = 60  # only consider entries in last N seconds
MAX_ENTRY_PRICE = 0.03
STAKE_USD = 18.0
# Regime gate: stddev of 1-min log returns over last 60m, in bps.
# Engine uses 8 bps/min as the low/high vol cutoff. We require high-vol for entry.
MIN_VOL_BPS = 8.0
VOL_LOOKBACK_MIN = 60
VOL_REFRESH_SECS = 60

_VOL_CACHE: dict[str, float] = {"bps": 0.0, "last_fetched_ts": 0.0, "n": 0}


@dataclass
class Candidate:
    slug: str
    window_start: int  # unix seconds
    up_price: float
    down_price: float
    up_token_id: str
    down_token_id: str


def ensure_db(db_path: Path) -> sqlite3.Connection:
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(str(db_path))
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS penny_paper_trades (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            decided_ts TEXT NOT NULL,
            window_slug TEXT UNIQUE NOT NULL,
            window_start INTEGER NOT NULL,
            side TEXT NOT NULL,
            entry_price REAL NOT NULL,
            shares REAL NOT NULL,
            notional_usd REAL NOT NULL,
            seconds_to_close INTEGER NOT NULL,
            resolved INTEGER NOT NULL DEFAULT 0,
            winning_outcome TEXT,
            won INTEGER,
            pnl_usd REAL
        )
        """
    )
    conn.commit()
    return conn


def current_and_next_slugs(count: int = 3) -> list[str]:
    """Return slugs for the current (in-progress) window and the next few."""
    now = datetime.now(timezone.utc)
    minutes = (now.minute // 5) * 5
    base = now.replace(minute=minutes, second=0, microsecond=0)
    return [f"{SLUG_PREFIX}{int((base + timedelta(minutes=5 * i)).timestamp())}"
            for i in range(-1, count)]


def _refresh_vol(client: httpx.Client) -> float:
    """Refresh (if stale) and return stddev of 1-min BTC log returns over last
    VOL_LOOKBACK_MIN minutes, in basis points."""
    import math
    now = time.time()
    if now - _VOL_CACHE["last_fetched_ts"] < VOL_REFRESH_SECS and _VOL_CACHE["n"]:
        return _VOL_CACHE["bps"]
    try:
        end_ms = int(now * 1000)
        start_ms = end_ms - (VOL_LOOKBACK_MIN + 5) * 60 * 1000
        r = client.get(
            BINANCE_KLINES,
            params={"symbol": "BTCUSDT", "interval": "1m",
                    "startTime": start_ms, "endTime": end_ms},
            timeout=8,
        )
        if r.status_code != 200:
            return _VOL_CACHE["bps"]
        candles = r.json()
        closes = [float(c[4]) for c in candles]
        if len(closes) < 10:
            return _VOL_CACHE["bps"]
        rets = [math.log(closes[i] / closes[i - 1]) * 10000
                for i in range(1, len(closes))]
        m = sum(rets) / len(rets)
        var = sum((x - m) ** 2 for x in rets) / len(rets)
        vol_bps = var ** 0.5
    except Exception as e:
        logger.debug("vol refresh failed: %s", e)
        return _VOL_CACHE["bps"]
    _VOL_CACHE.update({"bps": vol_bps, "last_fetched_ts": now, "n": len(rets)})
    return vol_bps


def _best_ask(client: httpx.Client, token_id: str) -> float | None:
    """Cheapest ask on the CLOB book. None if book missing or empty."""
    if not token_id:
        return None
    try:
        r = client.get(CLOB_BOOK, params={"token_id": token_id}, timeout=5)
        if r.status_code != 200:
            return None
        asks = r.json().get("asks") or []
        if not asks:
            return None
        return min(float(a["price"]) for a in asks)
    except Exception as e:
        logger.debug("best_ask %s failed: %s", token_id[:16], e)
        return None


def fetch_candidate(client: httpx.Client, slug: str) -> Candidate | None:
    """Return a Candidate using CLOB best-ask prices (not gamma midpoints)."""
    try:
        r = client.get(f"{GAMMA}/events", params={"slug": slug}, timeout=10)
        if r.status_code != 200:
            return None
        data = r.json()
        if not isinstance(data, list) or not data:
            return None
        ev = data[0]
    except Exception as e:
        logger.debug("fetch_candidate %s failed: %s", slug, e)
        return None

    if ev.get("closed") or not ev.get("active", True):
        return None
    try:
        window_start = int(slug.replace(SLUG_PREFIX, ""))
    except ValueError:
        return None
    markets = ev.get("markets") or []
    if not markets:
        return None
    m = markets[0]

    # Extract token IDs and outcome labels. gamma sometimes returns
    # `clobTokenIds` as a JSON string and `outcomes` likewise.
    cids_raw = m.get("clobTokenIds")
    cids = json.loads(cids_raw) if isinstance(cids_raw, str) else (cids_raw or [])
    outs_raw = m.get("outcomes")
    outs = json.loads(outs_raw) if isinstance(outs_raw, str) else (outs_raw or [])
    up_id = dn_id = ""
    for tid, o in zip(cids, outs):
        lo = (o or "").lower()
        if lo in ("up", "yes"):
            up_id = tid
        elif lo in ("down", "no"):
            dn_id = tid

    # Price = best ask on CLOB (this is what a taker would actually pay).
    up_p = _best_ask(client, up_id) or 1.0
    dn_p = _best_ask(client, dn_id) or 1.0

    return Candidate(
        slug=slug,
        window_start=window_start,
        up_price=up_p,
        down_price=dn_p,
        up_token_id=up_id,
        down_token_id=dn_id,
    )


def maybe_enter(
    conn: sqlite3.Connection,
    cand: Candidate,
    now_ts: int,
    vol_bps: float,
) -> tuple[str, float, int] | None:
    """If entry condition met and not yet logged, insert paper trade.

    Returns (side, price, seconds_to_close) if entered, else None.
    """
    window_close = cand.window_start + WINDOW_SECS
    seconds_to_close = window_close - now_ts
    # Only fire in the entry window (last N seconds of the 300s window),
    # and not past close.
    if seconds_to_close <= 0 or seconds_to_close > ENTRY_WINDOW_LAST_N:
        return None
    # Already decided?
    cur = conn.execute(
        "SELECT 1 FROM penny_paper_trades WHERE window_slug = ?",
        (cand.slug,),
    )
    if cur.fetchone():
        return None
    # Regime gate: tail-capture only works when vol is high enough that a
    # reversal in the last 60s is plausible. Below cutoff the book stays
    # pinned and the losing side never reverts.
    if vol_bps < MIN_VOL_BPS:
        return None
    # Pick the cheaper side at/under the threshold.
    if cand.up_price <= MAX_ENTRY_PRICE and cand.up_price <= cand.down_price:
        side, price = "Up", cand.up_price
    elif cand.down_price <= MAX_ENTRY_PRICE:
        side, price = "Down", cand.down_price
    else:
        return None
    if price <= 0:
        return None
    shares = STAKE_USD / price
    conn.execute(
        """
        INSERT INTO penny_paper_trades (
            decided_ts, window_slug, window_start, side, entry_price,
            shares, notional_usd, seconds_to_close
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        """,
        (
            datetime.now(timezone.utc).isoformat(),
            cand.slug,
            cand.window_start,
            side,
            price,
            shares,
            STAKE_USD,
            seconds_to_close,
        ),
    )
    conn.commit()
    return side, price, seconds_to_close


def resolve_pending(conn: sqlite3.Connection, client: httpx.Client) -> int:
    """Look up outcomes for closed-but-unresolved windows. Returns count resolved."""
    rows = conn.execute(
        """
        SELECT id, window_slug, window_start, side, shares, notional_usd
        FROM penny_paper_trades
        WHERE resolved = 0
        ORDER BY window_start
        """
    ).fetchall()
    resolved = 0
    now_ts = int(time.time())
    for row_id, slug, window_start, side, shares, stake in rows:
        # Window must be fully closed (+ grace period for past-results lag).
        if now_ts < window_start + WINDOW_SECS + 30:
            continue
        outcome = _fetch_winning_outcome(client, slug, window_start)
        if outcome is None:
            continue
        won = 1 if outcome.lower() == side.lower() else 0
        pnl = (shares * 1.0 - stake) if won else (-stake)
        conn.execute(
            "UPDATE penny_paper_trades SET resolved=1, winning_outcome=?, "
            "won=?, pnl_usd=? WHERE id=?",
            (outcome, won, pnl, row_id),
        )
        conn.commit()
        resolved += 1
    return resolved


def _fetch_winning_outcome(
    client: httpx.Client, slug: str, window_start: int
) -> str | None:
    """Prefer gamma event (has resolved outcome); fall back to past-results."""
    try:
        r = client.get(f"{GAMMA}/events", params={"slug": slug}, timeout=10)
        if r.status_code == 200:
            data = r.json()
            if isinstance(data, list) and data:
                ev = data[0]
                if ev.get("closed"):
                    for m in ev.get("markets") or []:
                        ut = m.get("umaResolutionStatus") or m.get("resolutionStatus")
                        # Winning token is the one whose outcomePrices value == 1.
                        op = m.get("outcomePrices")
                        prices = json.loads(op) if isinstance(op, str) else op
                        outcomes = m.get("outcomes")
                        outs = json.loads(outcomes) if isinstance(outcomes, str) else outcomes
                        if prices and outs:
                            for p, o in zip(prices, outs):
                                try:
                                    if float(p) >= 0.99:
                                        return o
                                except ValueError:
                                    pass
    except Exception as e:
        logger.debug("gamma resolution %s failed: %s", slug, e)
    # Fallback: past-results endpoint with current event's start time + 5m.
    try:
        end = datetime.fromtimestamp(window_start + WINDOW_SECS, tz=timezone.utc)
        r = client.get(
            PAST_RESULTS,
            params={
                "symbol": "BTC",
                "variant": "fiveminute",
                "assetType": "crypto",
                "currentEventStartTime": end.isoformat(timespec="milliseconds").replace("+00:00", "Z"),
            },
            timeout=10,
        )
        if r.status_code != 200:
            return None
        data = r.json()
        if isinstance(data, list) and data:
            latest = data[0]
            outcome = latest.get("outcome") or latest.get("winningOutcome")
            if outcome:
                return outcome
    except Exception as e:
        logger.debug("past-results %s failed: %s", slug, e)
    return None


def print_summary(conn: sqlite3.Connection) -> None:
    cur = conn.execute(
        """
        SELECT
          COUNT(*)                                       AS total,
          SUM(CASE WHEN resolved=1 THEN 1 ELSE 0 END)    AS resolved,
          SUM(CASE WHEN won=1 THEN 1 ELSE 0 END)         AS wins,
          COALESCE(SUM(pnl_usd), 0.0)                    AS pnl,
          SUM(notional_usd)                              AS gross_staked
        FROM penny_paper_trades
        """
    )
    total, resolved, wins, pnl, gross = cur.fetchone()
    if total == 0:
        logger.info("[SUMMARY] no entries yet")
        return
    hit = f"{wins/resolved*100:.1f}%" if resolved else "—"
    logger.info(
        "[SUMMARY] entries=%d resolved=%d/%d wins=%d hit=%s gross=$%.2f pnl=$%+.2f",
        total, resolved, total, wins or 0, hit, gross or 0.0, pnl or 0.0,
    )


def tick(conn: sqlite3.Connection, client: httpx.Client) -> None:
    now_ts = int(time.time())
    vol_bps = _refresh_vol(client)
    # Only fetch book prices for windows in the entry zone (0 < secs_to_close ≤ 60).
    for slug in current_and_next_slugs():
        try:
            ws = int(slug.replace(SLUG_PREFIX, ""))
        except ValueError:
            continue
        secs_to_close = ws + WINDOW_SECS - now_ts
        if secs_to_close <= 0 or secs_to_close > ENTRY_WINDOW_LAST_N:
            continue
        cand = fetch_candidate(client, slug)
        if not cand:
            continue
        result = maybe_enter(conn, cand, now_ts, vol_bps)
        if result:
            side, price, secs = result
            logger.warning(
                "[ENTER] %s  side=%s price=%.3f shares=%.2f stake=$%.2f "
                "secs_to_close=%d vol_bps=%.1f",
                slug, side, price, STAKE_USD / price, STAKE_USD, secs, vol_bps,
            )
        else:
            gate = "GATED" if vol_bps < MIN_VOL_BPS else "READY"
            logger.info(
                "[OBSERVE] %s  secs_to_close=%d  up_ask=%.3f  dn_ask=%.3f  "
                "vol_bps=%.1f %s",
                slug, secs_to_close, cand.up_price, cand.down_price, vol_bps, gate,
            )
    resolved = resolve_pending(conn, client)
    if resolved:
        logger.info("[RESOLVE] marked %d windows resolved", resolved)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", default="data/penny_paper_trades.db")
    ap.add_argument("--loop", type=float, default=0, help="loop interval seconds; 0 = single tick")
    args = ap.parse_args()

    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )

    conn = ensure_db(Path(args.db))
    client = httpx.Client(timeout=15.0, headers={"User-Agent": "penny-paper/1.0"})

    if args.loop <= 0:
        tick(conn, client)
        print_summary(conn)
        return

    logger.info(
        "[START] penny paper bot — entry ≤$%.3f, stake $%.0f, "
        "last %ds, vol-gate ≥%.1f bps (60m), poll every %.1fs",
        MAX_ENTRY_PRICE, STAKE_USD, ENTRY_WINDOW_LAST_N, MIN_VOL_BPS, args.loop,
    )
    last_summary = 0.0
    while True:
        try:
            tick(conn, client)
            if time.time() - last_summary > 60:
                print_summary(conn)
                last_summary = time.time()
        except KeyboardInterrupt:
            break
        except Exception as e:
            logger.exception("tick failed: %s", e)
        time.sleep(args.loop)


if __name__ == "__main__":
    main()
