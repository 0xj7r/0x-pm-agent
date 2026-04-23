#!/usr/bin/env python3
"""Endgame yield paper bot.

Paper-only strategy: in the last ~60s of each 5-minute BTC Up/Down window,
check whether the "winning" side shows a crossable ask at <= max_ask_threshold.
If so, record a paper trade. On window resolution, compute P&L assuming we
paid ask + entry slippage and took taker fees.

This is a standalone service. It does NOT import core/engine.py and does NOT
submit real orders. All output is decision logging to SQLite and stdout.

Strategy thesis (see feat/endgame-yield-paper PR):
    Market makers camp at 99c bids to collect $1 - 0.99 on resolution (yield).
    Occasionally a holder of the winning side posts a crossable ask in the
    last minute because they want to unload early. If we take it at <= 0.97,
    we collect $1 at resolution for 3%+ in < 60s.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import os
import signal
import sqlite3
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

import httpx


GAMMA_URL = "https://gamma-api.polymarket.com"
CLOB_URL = "https://clob.polymarket.com"
SLUG_PREFIX = "btc-updown-5m-"

DEFAULT_DB_PATH = "data/endgame_paper_trades.db"
DEFAULT_MAX_POSITION_USD = 5.0
DEFAULT_MAX_ASK_THRESHOLD = 0.97
DEFAULT_MIN_SECONDS_REMAINING = 10
DEFAULT_MAX_SECONDS_REMAINING = 60
DEFAULT_PAPER_ENTRY_SLIPPAGE_USD = 0.005
DEFAULT_PAPER_FEE_RATE = 0.02

logger = logging.getLogger("endgame_paper_bot")


@dataclass
class Window:
    market_id: str
    condition_id: str
    event_slug: str
    event_id: str
    question: str
    start_time: datetime
    end_time: datetime
    up_token_id: str
    down_token_id: str

    def seconds_remaining(self, now: datetime) -> float:
        return max(0.0, (self.end_time - now).total_seconds())

    def is_live(self, now: datetime) -> bool:
        return self.start_time <= now < self.end_time


@dataclass
class Decision:
    token_id: str
    side: str
    best_ask: float
    best_ask_size: float
    intended_shares: float
    size_usd: float


def _parse_iso(raw: str | None) -> datetime | None:
    if not raw:
        return None
    try:
        return datetime.fromisoformat(raw.replace("Z", "+00:00"))
    except (ValueError, TypeError):
        return None


def _safe_float(value: Any, default: float = 0.0) -> float:
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


def init_db(db_path: str) -> sqlite3.Connection:
    Path(db_path).parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(db_path)
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS endgame_paper_trades (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts TEXT NOT NULL,
            market_id TEXT NOT NULL UNIQUE,
            event_slug TEXT NOT NULL,
            side TEXT NOT NULL,
            token_id TEXT NOT NULL,
            seconds_remaining_at_entry REAL NOT NULL,
            best_ask REAL NOT NULL,
            best_ask_size REAL NOT NULL,
            intended_shares REAL NOT NULL,
            size_usd REAL NOT NULL,
            resolved INTEGER NOT NULL DEFAULT 0,
            won INTEGER,
            observed_direction TEXT,
            pnl_usd REAL
        )
        """
    )
    conn.commit()
    return conn


def already_recorded(conn: sqlite3.Connection, market_id: str) -> bool:
    cur = conn.execute(
        "SELECT 1 FROM endgame_paper_trades WHERE market_id = ? LIMIT 1",
        (market_id,),
    )
    return cur.fetchone() is not None


def record_decision(
    conn: sqlite3.Connection,
    window: Window,
    decision: Decision,
    seconds_remaining: float,
) -> None:
    conn.execute(
        """
        INSERT OR IGNORE INTO endgame_paper_trades
        (ts, market_id, event_slug, side, token_id,
         seconds_remaining_at_entry, best_ask, best_ask_size,
         intended_shares, size_usd, resolved)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)
        """,
        (
            datetime.now(timezone.utc).isoformat(),
            window.market_id,
            window.event_slug,
            decision.side,
            decision.token_id,
            seconds_remaining,
            decision.best_ask,
            decision.best_ask_size,
            decision.intended_shares,
            decision.size_usd,
        ),
    )
    conn.commit()


def record_resolution(
    conn: sqlite3.Connection,
    market_id: str,
    won: bool,
    observed_direction: str,
    pnl_usd: float,
) -> None:
    conn.execute(
        """
        UPDATE endgame_paper_trades
        SET resolved = 1, won = ?, observed_direction = ?, pnl_usd = ?
        WHERE market_id = ?
        """,
        (1 if won else 0, observed_direction, pnl_usd, market_id),
    )
    conn.commit()


def compute_paper_pnl(
    cost_usd: float,
    size_usd: float,
    shares: float,
    won: bool,
    fee_rate: float,
) -> float:
    """Paper P&L with taker fee on both sides.

    Entry fee applies to cost; exit fee applies to payout (1.0 per share on
    a win, 0 on a loss). This mirrors how a real taker would be charged.
    """
    entry_fee = cost_usd * fee_rate
    if won:
        gross_payout = shares * 1.0
        exit_fee = gross_payout * fee_rate
        return gross_payout - cost_usd - entry_fee - exit_fee
    return -cost_usd - entry_fee


async def generate_candidate_slugs(count: int = 6) -> list[str]:
    """Generate the most recently-started 5m window slugs.

    We look back more than forward because we care about windows that are
    currently running (and entering their final minute). Looking at the most
    recent ~6 aligned starts covers the current live window plus a buffer
    for clock skew.
    """
    now = datetime.now(timezone.utc)
    minutes = (now.minute // 5) * 5
    base = now.replace(minute=minutes, second=0, microsecond=0)
    slugs: list[str] = []
    for i in range(-count, 2):
        ts = base + timedelta(minutes=5 * i)
        slugs.append(f"{SLUG_PREFIX}{int(ts.timestamp())}")
    return slugs


def _parse_event(event: dict[str, Any]) -> Window | None:
    slug = event.get("slug", "")
    if not slug.startswith(SLUG_PREFIX):
        return None
    try:
        start_ts = int(slug.replace(SLUG_PREFIX, ""))
    except ValueError:
        return None
    start_time = datetime.fromtimestamp(start_ts, tz=timezone.utc)
    end_time = start_time + timedelta(minutes=5)

    markets = event.get("markets", [])
    if not markets:
        return None
    market = markets[0]
    if market.get("closed", False):
        return None

    up_tok = ""
    down_tok = ""
    for tok in market.get("tokens", []) or []:
        outcome = (tok.get("outcome") or "").lower()
        token_id = tok.get("token_id", "")
        if outcome in ("up", "yes"):
            up_tok = token_id
        elif outcome in ("down", "no"):
            down_tok = token_id

    if not up_tok or not down_tok:
        raw = market.get("clobTokenIds", "[]")
        try:
            ids = json.loads(raw) if isinstance(raw, str) else raw
        except (json.JSONDecodeError, ValueError):
            ids = []
        if len(ids) >= 2:
            up_tok = up_tok or ids[0]
            down_tok = down_tok or ids[1]

    if not up_tok or not down_tok:
        return None

    return Window(
        market_id=str(market.get("id") or event.get("id") or ""),
        condition_id=str(market.get("conditionId") or ""),
        event_slug=slug,
        event_id=str(event.get("id") or ""),
        question=event.get("title") or market.get("question") or "",
        start_time=start_time,
        end_time=end_time,
        up_token_id=up_tok,
        down_token_id=down_tok,
    )


async def fetch_active_windows(client: httpx.AsyncClient) -> list[Window]:
    slugs = await generate_candidate_slugs()
    windows: list[Window] = []
    for slug in slugs:
        try:
            resp = await client.get(f"{GAMMA_URL}/events", params={"slug": slug})
            if resp.status_code != 200:
                continue
            data = resp.json()
            if not isinstance(data, list) or not data:
                continue
            w = _parse_event(data[0])
            if w is not None:
                windows.append(w)
        except Exception as exc:
            logger.debug("gamma slug query failed for %s: %s", slug, exc)
    return windows


async def fetch_orderbook(
    client: httpx.AsyncClient, token_id: str
) -> dict[str, Any] | None:
    """Fetch the CLOB orderbook for a token via the public REST endpoint."""
    try:
        resp = await client.get(
            f"{CLOB_URL}/book", params={"token_id": token_id}, timeout=10.0
        )
        if resp.status_code == 429:
            logger.warning("clob 429 for token %s; backing off", token_id[:12])
            await asyncio.sleep(2.0)
            return None
        if resp.status_code != 200:
            return None
        data = resp.json()
        if not isinstance(data, dict):
            return None
        return data
    except Exception as exc:
        logger.debug("clob book fetch failed for %s: %s", token_id[:12], exc)
        return None


def best_ask_from_book(book: dict[str, Any]) -> tuple[float | None, float | None]:
    """Return (best_ask_price, best_ask_size) from a CLOB book response.

    The CLOB /book endpoint returns asks in ASCENDING price order in the
    ``asks`` array, so the best ask is the LAST element. Bids are in
    ascending order too; best bid is the last element. We extract the
    whole list and min/max defensively in case that invariant changes.
    """
    asks = book.get("asks") or []
    if not asks:
        return None, None
    best: dict[str, Any] | None = None
    best_price: float | None = None
    for entry in asks:
        price = _safe_float(entry.get("price"))
        if price <= 0:
            continue
        if best_price is None or price < best_price:
            best_price = price
            best = entry
    if best is None or best_price is None:
        return None, None
    return best_price, _safe_float(best.get("size"))


def choose_decision(
    up_book: dict[str, Any],
    down_book: dict[str, Any],
    max_ask_threshold: float,
    max_position_usd: float,
    up_token_id: str,
    down_token_id: str,
) -> Decision | None:
    """Pick the token with the lower ask, if any side shows a crossable ask.

    Spec: "If best_ask of either token <= 0.97: record as a paper trade
    candidate. Pick the token with the lower ask as the 'winning side' bet."
    The "winning side" heuristic is that whichever side is cheaper on the
    ask in the last minute is the one a seller is unloading early, which is
    typically the side that is ahead (they want to lock in < $1 vs waiting).
    """
    up_ask, up_size = best_ask_from_book(up_book)
    down_ask, down_size = best_ask_from_book(down_book)

    candidates: list[tuple[float, float, str, str]] = []
    if up_ask is not None and up_ask <= max_ask_threshold:
        candidates.append((up_ask, up_size or 0.0, "UP", up_token_id))
    if down_ask is not None and down_ask <= max_ask_threshold:
        candidates.append((down_ask, down_size or 0.0, "DOWN", down_token_id))

    if not candidates:
        return None

    candidates.sort(key=lambda c: c[0])
    best_ask, best_size, side, token_id = candidates[0]

    depth_usd = best_ask * max(best_size, 0.0)
    size_usd = min(max_position_usd, depth_usd) if depth_usd > 0 else 0.0
    if size_usd <= 0 or best_ask <= 0:
        return None
    intended_shares = size_usd / best_ask

    return Decision(
        token_id=token_id,
        side=side,
        best_ask=best_ask,
        best_ask_size=best_size,
        intended_shares=intended_shares,
        size_usd=size_usd,
    )


async def fetch_resolution(
    client: httpx.AsyncClient, condition_id: str, market_id: str
) -> tuple[bool, str] | None:
    """Return (won_up, direction_str) once a market has resolved, else None.

    Resolution is detected via the Gamma /markets endpoint: when a market is
    ``closed`` with outcomePrices of 1/0 or 0/1, the non-zero index wins.
    ``direction_str`` is "UP" or "DOWN" from the caller's frame.
    """
    for query in ({"condition_id": condition_id}, None):
        try:
            if query is None and market_id:
                url = f"{GAMMA_URL}/markets/{market_id}"
                resp = await client.get(url, timeout=10.0)
            elif query is not None and condition_id:
                resp = await client.get(
                    f"{GAMMA_URL}/markets", params=query, timeout=10.0
                )
            else:
                continue
            if resp.status_code != 200:
                continue
            data = resp.json()
            markets = data if isinstance(data, list) else [data]
            if not markets:
                continue
            m = markets[0]
            if not m.get("closed"):
                return None
            raw_prices = m.get("outcomePrices", "[]")
            try:
                prices = (
                    json.loads(raw_prices)
                    if isinstance(raw_prices, str)
                    else raw_prices
                )
            except (json.JSONDecodeError, ValueError):
                continue
            if not isinstance(prices, list) or len(prices) < 2:
                continue
            up_price = _safe_float(prices[0])
            down_price = _safe_float(prices[1])
            if up_price >= 0.99:
                return True, "UP"
            if down_price >= 0.99:
                return True, "DOWN"
            return None
        except Exception as exc:
            logger.debug(
                "resolution fetch failed (cid=%s mid=%s): %s",
                condition_id,
                market_id,
                exc,
            )
    return None


async def sweep_resolutions(
    conn: sqlite3.Connection,
    client: httpx.AsyncClient,
    fee_rate: float,
    entry_slippage_usd: float,
) -> int:
    """Resolve any unresolved paper trades whose markets have closed."""
    rows = conn.execute(
        """
        SELECT market_id, side, best_ask, intended_shares, size_usd, event_slug
        FROM endgame_paper_trades
        WHERE resolved = 0
        """
    ).fetchall()
    if not rows:
        return 0

    updated = 0
    for market_id, side, best_ask, shares, size_usd, event_slug in rows:
        # Look up condition_id opportunistically via the event slug.
        condition_id = ""
        try:
            resp = await client.get(
                f"{GAMMA_URL}/events", params={"slug": event_slug}, timeout=10.0
            )
            if resp.status_code == 200:
                data = resp.json()
                if isinstance(data, list) and data:
                    for m in (data[0].get("markets") or []):
                        if str(m.get("id")) == str(market_id):
                            condition_id = str(m.get("conditionId") or "")
                            break
        except Exception as exc:
            logger.debug("event lookup failed for slug %s: %s", event_slug, exc)

        result = await fetch_resolution(client, condition_id, str(market_id))
        if result is None:
            continue
        _, direction = result
        won = side == direction
        effective_entry_price = float(best_ask) + entry_slippage_usd
        effective_cost = float(shares) * effective_entry_price
        pnl = compute_paper_pnl(
            cost_usd=effective_cost,
            size_usd=float(size_usd),
            shares=float(shares),
            won=won,
            fee_rate=fee_rate,
        )
        record_resolution(conn, str(market_id), won, direction, pnl)
        logger.info(
            "resolved market=%s side=%s won=%s dir=%s pnl=$%.4f",
            market_id,
            side,
            won,
            direction,
            pnl,
        )
        updated += 1
    return updated


def print_summary(conn: sqlite3.Connection, limit: int = 10) -> None:
    cur = conn.execute(
        "SELECT COUNT(*), COALESCE(SUM(resolved), 0), "
        "COALESCE(SUM(CASE WHEN won=1 THEN 1 ELSE 0 END), 0), "
        "COALESCE(SUM(pnl_usd), 0) FROM endgame_paper_trades"
    )
    total, resolved, wins, pnl_sum = cur.fetchone()
    hit_rate = (wins / resolved * 100.0) if resolved else 0.0
    print("=== Endgame Paper Bot Summary ===")
    print(f"Total recorded : {total}")
    print(f"Resolved       : {resolved}")
    print(f"Wins           : {wins} ({hit_rate:.1f}% hit rate)")
    print(f"Realized P&L   : ${pnl_sum:.4f}")
    print()
    print(f"Most recent {limit} decisions:")
    cur = conn.execute(
        """
        SELECT ts, side, best_ask, intended_shares, size_usd, resolved, won,
               observed_direction, pnl_usd, event_slug
        FROM endgame_paper_trades
        ORDER BY id DESC LIMIT ?
        """,
        (limit,),
    )
    for row in cur.fetchall():
        (ts, side, ask, shares, size_usd, res, won, dir_, pnl, slug) = row
        status = (
            f"resolved={'W' if won == 1 else 'L'} dir={dir_} pnl=${pnl:.4f}"
            if res
            else "pending"
        )
        print(
            f"  {ts} {slug} side={side} ask={ask:.3f} "
            f"shares={shares:.2f} size=${size_usd:.2f} {status}"
        )


async def scan_once(
    conn: sqlite3.Connection,
    client: httpx.AsyncClient,
    max_position_usd: float,
    max_ask_threshold: float,
    min_seconds_remaining: int,
    max_seconds_remaining: int,
    fee_rate: float,
    entry_slippage_usd: float,
) -> dict[str, int]:
    now = datetime.now(timezone.utc)
    windows = await fetch_active_windows(client)
    live = [w for w in windows if w.is_live(now)]

    recorded = 0
    skipped_duplicate = 0
    skipped_outside_band = 0
    skipped_no_edge = 0

    for w in live:
        sec = w.seconds_remaining(now)
        if sec < min_seconds_remaining or sec > max_seconds_remaining:
            skipped_outside_band += 1
            continue
        if already_recorded(conn, w.market_id):
            skipped_duplicate += 1
            continue

        up_book = await fetch_orderbook(client, w.up_token_id)
        down_book = await fetch_orderbook(client, w.down_token_id)
        if up_book is None and down_book is None:
            continue
        up_book = up_book or {}
        down_book = down_book or {}

        decision = choose_decision(
            up_book=up_book,
            down_book=down_book,
            max_ask_threshold=max_ask_threshold,
            max_position_usd=max_position_usd,
            up_token_id=w.up_token_id,
            down_token_id=w.down_token_id,
        )
        if decision is None:
            skipped_no_edge += 1
            continue

        record_decision(conn, w, decision, sec)
        recorded += 1
        logger.info(
            "RECORDED slug=%s side=%s ask=%.3f shares=%.2f size=$%.2f secs=%.0f",
            w.event_slug,
            decision.side,
            decision.best_ask,
            decision.intended_shares,
            decision.size_usd,
            sec,
        )

    resolved = await sweep_resolutions(conn, client, fee_rate, entry_slippage_usd)

    return {
        "live_windows": len(live),
        "recorded": recorded,
        "skipped_duplicate": skipped_duplicate,
        "skipped_outside_band": skipped_outside_band,
        "skipped_no_edge": skipped_no_edge,
        "resolved": resolved,
    }


async def run(args: argparse.Namespace) -> int:
    logging.basicConfig(
        level=getattr(logging, args.log_level.upper(), logging.INFO),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )

    max_position_usd = float(
        os.environ.get("ENDGAME_MAX_POSITION_USD", args.max_position_usd)
    )
    max_ask_threshold = float(
        os.environ.get("ENDGAME_MAX_ASK_THRESHOLD", args.max_ask_threshold)
    )
    min_secs = int(
        os.environ.get("ENDGAME_MIN_SECONDS_REMAINING", args.min_seconds_remaining)
    )
    max_secs = int(
        os.environ.get("ENDGAME_MAX_SECONDS_REMAINING", args.max_seconds_remaining)
    )
    fee_rate = float(os.environ.get("PAPER_FEE_RATE", DEFAULT_PAPER_FEE_RATE))
    entry_slip = float(
        os.environ.get("PAPER_ENTRY_SLIPPAGE_USD", DEFAULT_PAPER_ENTRY_SLIPPAGE_USD)
    )

    logger.info(
        "starting endgame_paper_bot db=%s loop=%ss max_position_usd=%.2f "
        "max_ask_threshold=%.3f band=[%d,%d]s fee_rate=%.3f slip=%.4f",
        args.db,
        args.loop,
        max_position_usd,
        max_ask_threshold,
        min_secs,
        max_secs,
        fee_rate,
        entry_slip,
    )

    conn = init_db(args.db)

    stop = asyncio.Event()

    def _on_sig(*_: Any) -> None:
        stop.set()

    try:
        asyncio.get_event_loop().add_signal_handler(signal.SIGTERM, _on_sig)
        asyncio.get_event_loop().add_signal_handler(signal.SIGINT, _on_sig)
    except (NotImplementedError, RuntimeError):
        pass

    async with httpx.AsyncClient(timeout=30.0) as client:
        if args.once or args.loop <= 0:
            stats = await scan_once(
                conn,
                client,
                max_position_usd=max_position_usd,
                max_ask_threshold=max_ask_threshold,
                min_seconds_remaining=min_secs,
                max_seconds_remaining=max_secs,
                fee_rate=fee_rate,
                entry_slippage_usd=entry_slip,
            )
            logger.info("once: %s", stats)
            print_summary(conn)
            conn.close()
            return 0

        while not stop.is_set():
            tick_start = time.monotonic()
            try:
                stats = await scan_once(
                    conn,
                    client,
                    max_position_usd=max_position_usd,
                    max_ask_threshold=max_ask_threshold,
                    min_seconds_remaining=min_secs,
                    max_seconds_remaining=max_secs,
                    fee_rate=fee_rate,
                    entry_slippage_usd=entry_slip,
                )
                logger.debug("tick: %s", stats)
            except Exception:
                logger.exception("tick failed; continuing")
            elapsed = time.monotonic() - tick_start
            sleep_s = max(0.0, args.loop - elapsed)
            try:
                await asyncio.wait_for(stop.wait(), timeout=sleep_s)
            except asyncio.TimeoutError:
                pass

    print_summary(conn)
    conn.close()
    return 0


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", default=DEFAULT_DB_PATH, help="SQLite db path")
    ap.add_argument(
        "--loop",
        type=float,
        default=5.0,
        help="Seconds between ticks; set 0 (or --once) to run a single scan",
    )
    ap.add_argument(
        "--once",
        action="store_true",
        help="Run one scan and exit (equivalent to --loop 0)",
    )
    ap.add_argument(
        "--max-position-usd", type=float, default=DEFAULT_MAX_POSITION_USD
    )
    ap.add_argument(
        "--max-ask-threshold", type=float, default=DEFAULT_MAX_ASK_THRESHOLD
    )
    ap.add_argument(
        "--min-seconds-remaining",
        type=int,
        default=DEFAULT_MIN_SECONDS_REMAINING,
    )
    ap.add_argument(
        "--max-seconds-remaining",
        type=int,
        default=DEFAULT_MAX_SECONDS_REMAINING,
    )
    ap.add_argument("--log-level", default="INFO")
    return ap


def main(argv: list[str]) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    return asyncio.run(run(args))


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
