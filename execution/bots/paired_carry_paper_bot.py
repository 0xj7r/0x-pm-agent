#!/usr/bin/env python3
"""Two-sided paired-share paper bot for short-dated Polymarket crypto windows.

Strategy thesis:
  - Reverse-engineered whale activity repeatedly buys both sides in BTC/ETH
    up/down markets, then recycles the matched inventory through merge or
    resolution/redeem.
  - The most portable v1 is the equal-share portion of that flow:
      buy X shares of Up and X shares of Down only when the combined ask is
      cheap enough, then carry the matched pair to resolution.
  - A matched pair always pays $1.00 per share-set at resolution regardless of
    winner, so when `up_ask + down_ask + entry_fees < 1.0` the edge is locked.

This script is intentionally narrow:
  - one entry per market
  - equal-share pairs only
  - no residual directional leg
  - no real orders, no wallet, no merge txs
  - resolution is used only to mark the trade complete and record the winner
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
from typing import Any

import httpx

ROOT = Path(__file__).resolve().parent.parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from shared.fees import taker_fee_usd


logger = logging.getLogger("paired_carry_paper")

GAMMA = "https://gamma-api.polymarket.com"
CLOB = "https://clob.polymarket.com"

DEFAULT_DB = "data/paired_carry_paper_trades.db"
DEFAULT_ASSETS = ("btc",)
DEFAULT_INTERVALS = ("5m",)
DEFAULT_MAX_PAIR_COST = 0.97
DEFAULT_MAX_POSITION_USD = 25.0
DEFAULT_MIN_SECONDS_FROM_START = 60
DEFAULT_MAX_SECONDS_FROM_START = 225
DEFAULT_RESOLUTION_GRACE_SECONDS = 30

INTERVAL_MINUTES = {
    "5m": 5,
    "15m": 15,
}


@dataclass(frozen=True)
class Window:
    asset: str
    interval_label: str
    interval_minutes: int
    market_id: str
    condition_id: str
    event_slug: str
    event_id: str
    question: str
    start_time: datetime
    end_time: datetime
    up_token_id: str
    down_token_id: str


@dataclass(frozen=True)
class PairDecision:
    up_ask: float
    down_ask: float
    up_ask_size: float
    down_ask_size: float
    matched_shares: float
    gross_cost_usd: float
    entry_fee_usd: float
    locked_pnl_usd: float
    locked_edge_pct: float


def _safe_float(value: Any, default: float = 0.0) -> float:
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


def _parse_iso(value: str | None) -> datetime | None:
    if not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except (TypeError, ValueError):
        return None


def init_db(db_path: str) -> sqlite3.Connection:
    Path(db_path).parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(db_path)
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS paired_carry_paper_trades (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            decided_ts TEXT NOT NULL,
            asset TEXT NOT NULL,
            interval_label TEXT NOT NULL,
            market_id TEXT NOT NULL UNIQUE,
            condition_id TEXT NOT NULL,
            event_slug TEXT NOT NULL,
            event_id TEXT NOT NULL,
            window_start_ts INTEGER NOT NULL,
            window_end_ts INTEGER NOT NULL,
            seconds_from_start INTEGER NOT NULL,
            seconds_to_end INTEGER NOT NULL,
            up_token_id TEXT NOT NULL,
            down_token_id TEXT NOT NULL,
            up_ask REAL NOT NULL,
            down_ask REAL NOT NULL,
            up_ask_size REAL NOT NULL,
            down_ask_size REAL NOT NULL,
            pair_cost REAL NOT NULL,
            matched_shares REAL NOT NULL,
            gross_cost_usd REAL NOT NULL,
            entry_fee_usd REAL NOT NULL,
            total_cost_usd REAL NOT NULL,
            locked_pnl_usd REAL NOT NULL,
            locked_edge_pct REAL NOT NULL,
            resolved INTEGER NOT NULL DEFAULT 0,
            winning_outcome TEXT,
            payout_usd REAL,
            realized_pnl_usd REAL
        )
        """
    )
    conn.commit()
    return conn


def already_recorded(conn: sqlite3.Connection, market_id: str) -> bool:
    row = conn.execute(
        "SELECT 1 FROM paired_carry_paper_trades WHERE market_id = ? LIMIT 1",
        (market_id,),
    ).fetchone()
    return row is not None


def record_decision(
    conn: sqlite3.Connection,
    window: Window,
    decision: PairDecision,
    *,
    seconds_from_start: int,
    seconds_to_end: int,
) -> None:
    pair_cost = decision.up_ask + decision.down_ask
    conn.execute(
        """
        INSERT OR IGNORE INTO paired_carry_paper_trades (
            decided_ts, asset, interval_label, market_id, condition_id,
            event_slug, event_id, window_start_ts, window_end_ts,
            seconds_from_start, seconds_to_end, up_token_id, down_token_id,
            up_ask, down_ask, up_ask_size, down_ask_size, pair_cost,
            matched_shares, gross_cost_usd, entry_fee_usd, total_cost_usd,
            locked_pnl_usd, locked_edge_pct, resolved
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)
        """,
        (
            datetime.now(timezone.utc).isoformat(),
            window.asset.upper(),
            window.interval_label,
            window.market_id,
            window.condition_id,
            window.event_slug,
            window.event_id,
            int(window.start_time.timestamp()),
            int(window.end_time.timestamp()),
            seconds_from_start,
            seconds_to_end,
            window.up_token_id,
            window.down_token_id,
            decision.up_ask,
            decision.down_ask,
            decision.up_ask_size,
            decision.down_ask_size,
            pair_cost,
            decision.matched_shares,
            decision.gross_cost_usd,
            decision.entry_fee_usd,
            decision.gross_cost_usd + decision.entry_fee_usd,
            decision.locked_pnl_usd,
            decision.locked_edge_pct,
        ),
    )
    conn.commit()


def record_resolution(
    conn: sqlite3.Connection,
    market_id: str,
    *,
    winning_outcome: str,
    payout_usd: float,
    realized_pnl_usd: float,
) -> None:
    conn.execute(
        """
        UPDATE paired_carry_paper_trades
        SET resolved = 1,
            winning_outcome = ?,
            payout_usd = ?,
            realized_pnl_usd = ?
        WHERE market_id = ?
        """,
        (winning_outcome, payout_usd, realized_pnl_usd, market_id),
    )
    conn.commit()


def aligned_window_starts(
    now: datetime,
    interval_minutes: int,
    *,
    lookback_windows: int = 1,
) -> list[datetime]:
    minute = (now.minute // interval_minutes) * interval_minutes
    current = now.replace(minute=minute, second=0, microsecond=0)
    return [
        current - timedelta(minutes=interval_minutes * offset)
        for offset in range(lookback_windows, -1, -1)
    ]


def build_candidate_slugs(
    assets: tuple[str, ...],
    intervals: tuple[str, ...],
    now: datetime,
) -> list[tuple[str, str, int]]:
    candidates: list[tuple[str, str, int]] = []
    for asset in assets:
        for interval_label in intervals:
            minutes = INTERVAL_MINUTES[interval_label]
            for start in aligned_window_starts(now, minutes):
                slug = f"{asset.lower()}-updown-{interval_label}-{int(start.timestamp())}"
                candidates.append((asset.lower(), slug, minutes))
    return candidates


def parse_window(asset: str, interval_label: str, event: dict[str, Any]) -> Window | None:
    slug = str(event.get("slug") or "")
    prefix = f"{asset.lower()}-updown-{interval_label}-"
    if not slug.startswith(prefix):
        return None
    try:
        start_ts = int(slug.replace(prefix, ""))
    except ValueError:
        return None

    start_time = datetime.fromtimestamp(start_ts, tz=timezone.utc)
    end_time = start_time + timedelta(minutes=INTERVAL_MINUTES[interval_label])
    markets = event.get("markets") or []
    if not markets:
        return None
    market = markets[0]
    if market.get("closed"):
        return None

    up_token_id = ""
    down_token_id = ""
    for tok in market.get("tokens") or []:
        outcome = str(tok.get("outcome") or "").lower()
        token_id = str(tok.get("token_id") or "")
        if outcome in ("up", "yes"):
            up_token_id = token_id
        elif outcome in ("down", "no"):
            down_token_id = token_id

    if not up_token_id or not down_token_id:
        raw_ids = market.get("clobTokenIds", "[]")
        try:
            token_ids = json.loads(raw_ids) if isinstance(raw_ids, str) else raw_ids
        except (TypeError, ValueError, json.JSONDecodeError):
            token_ids = []
        if len(token_ids) >= 2:
            up_token_id = up_token_id or str(token_ids[0])
            down_token_id = down_token_id or str(token_ids[1])

    if not up_token_id or not down_token_id:
        return None

    return Window(
        asset=asset.upper(),
        interval_label=interval_label,
        interval_minutes=INTERVAL_MINUTES[interval_label],
        market_id=str(market.get("id") or event.get("id") or ""),
        condition_id=str(market.get("conditionId") or ""),
        event_slug=slug,
        event_id=str(event.get("id") or ""),
        question=str(event.get("title") or market.get("question") or ""),
        start_time=start_time,
        end_time=end_time,
        up_token_id=up_token_id,
        down_token_id=down_token_id,
    )


def fetch_active_windows(
    client: httpx.Client,
    *,
    assets: tuple[str, ...],
    intervals: tuple[str, ...],
) -> list[Window]:
    now = datetime.now(timezone.utc)
    windows: list[Window] = []
    seen_market_ids: set[str] = set()
    for asset, slug, _minutes in build_candidate_slugs(assets, intervals, now):
        interval_label = slug.split("-")[2]
        try:
            response = client.get(f"{GAMMA}/events", params={"slug": slug}, timeout=10.0)
            if response.status_code != 200:
                continue
            data = response.json()
        except Exception as exc:
            logger.debug("gamma fetch failed for %s: %s", slug, exc)
            continue
        if not isinstance(data, list) or not data:
            continue
        window = parse_window(asset, interval_label, data[0])
        if window is None:
            continue
        if not (window.start_time <= now < window.end_time):
            continue
        if window.market_id in seen_market_ids:
            continue
        seen_market_ids.add(window.market_id)
        windows.append(window)
    return windows


def fetch_orderbook(client: httpx.Client, token_id: str) -> dict[str, Any] | None:
    try:
        response = client.get(f"{CLOB}/book", params={"token_id": token_id}, timeout=10.0)
        if response.status_code != 200:
            return None
        data = response.json()
        if not isinstance(data, dict):
            return None
        return data
    except Exception as exc:
        logger.debug("book fetch failed for %s: %s", token_id[:12], exc)
        return None


def best_ask_from_book(book: dict[str, Any]) -> tuple[float | None, float | None]:
    asks = book.get("asks") or []
    best_price: float | None = None
    best_size: float | None = None
    for level in asks:
        price = _safe_float(level.get("price"))
        size = _safe_float(level.get("size"))
        if price <= 0:
            continue
        if best_price is None or price < best_price:
            best_price = price
            best_size = size
    return best_price, best_size


def compute_pair_decision(
    *,
    up_ask: float,
    down_ask: float,
    up_ask_size: float,
    down_ask_size: float,
    max_pair_cost: float,
    max_position_usd: float,
) -> PairDecision | None:
    if up_ask <= 0 or down_ask <= 0:
        return None
    pair_cost = up_ask + down_ask
    if pair_cost <= 0 or pair_cost > max_pair_cost:
        return None

    max_shares_by_depth = min(max(up_ask_size, 0.0), max(down_ask_size, 0.0))
    max_shares_by_position = max_position_usd / pair_cost
    matched_shares = min(max_shares_by_depth, max_shares_by_position)
    if matched_shares <= 0:
        return None

    up_cost = matched_shares * up_ask
    down_cost = matched_shares * down_ask
    gross_cost = up_cost + down_cost
    entry_fee = taker_fee_usd(up_ask, up_cost) + taker_fee_usd(down_ask, down_cost)
    locked_pnl = matched_shares - gross_cost - entry_fee
    if locked_pnl <= 0:
        return None

    return PairDecision(
        up_ask=up_ask,
        down_ask=down_ask,
        up_ask_size=up_ask_size,
        down_ask_size=down_ask_size,
        matched_shares=matched_shares,
        gross_cost_usd=gross_cost,
        entry_fee_usd=entry_fee,
        locked_pnl_usd=locked_pnl,
        locked_edge_pct=(locked_pnl / gross_cost * 100.0) if gross_cost > 0 else 0.0,
    )


def in_entry_zone(
    window: Window,
    now: datetime,
    *,
    min_seconds_from_start: int,
    max_seconds_from_start: int,
) -> bool:
    if now < window.start_time or now >= window.end_time:
        return False
    seconds_from_start = int((now - window.start_time).total_seconds())
    return min_seconds_from_start <= seconds_from_start <= max_seconds_from_start


def fetch_winning_outcome(client: httpx.Client, slug: str) -> str | None:
    try:
        response = client.get(f"{GAMMA}/events", params={"slug": slug}, timeout=10.0)
        if response.status_code != 200:
            return None
        data = response.json()
    except Exception as exc:
        logger.debug("gamma resolution fetch failed for %s: %s", slug, exc)
        return None

    if not isinstance(data, list) or not data:
        return None
    event = data[0]
    if not event.get("closed"):
        return None
    for market in event.get("markets") or []:
        outcome_prices = market.get("outcomePrices")
        outcomes = market.get("outcomes")
        prices = json.loads(outcome_prices) if isinstance(outcome_prices, str) else outcome_prices
        outs = json.loads(outcomes) if isinstance(outcomes, str) else outcomes
        if not isinstance(prices, list) or not isinstance(outs, list):
            continue
        for price, outcome in zip(prices, outs):
            if _safe_float(price) >= 0.99:
                return str(outcome)
    return None


def maybe_enter(
    conn: sqlite3.Connection,
    client: httpx.Client,
    window: Window,
    *,
    max_pair_cost: float,
    max_position_usd: float,
    min_seconds_from_start: int,
    max_seconds_from_start: int,
) -> PairDecision | None:
    now = datetime.now(timezone.utc)
    if not in_entry_zone(
        window,
        now,
        min_seconds_from_start=min_seconds_from_start,
        max_seconds_from_start=max_seconds_from_start,
    ):
        return None
    if already_recorded(conn, window.market_id):
        return None

    up_book = fetch_orderbook(client, window.up_token_id)
    down_book = fetch_orderbook(client, window.down_token_id)
    if not up_book or not down_book:
        return None

    up_ask, up_ask_size = best_ask_from_book(up_book)
    down_ask, down_ask_size = best_ask_from_book(down_book)
    decision = compute_pair_decision(
        up_ask=up_ask or 0.0,
        down_ask=down_ask or 0.0,
        up_ask_size=up_ask_size or 0.0,
        down_ask_size=down_ask_size or 0.0,
        max_pair_cost=max_pair_cost,
        max_position_usd=max_position_usd,
    )
    if decision is None:
        logger.info(
            "[OBSERVE] %s pair_cost=%s up_ask=%s down_ask=%s",
            window.event_slug,
            f"{(up_ask + down_ask):.4f}" if up_ask and down_ask else "n/a",
            f"{up_ask:.4f}" if up_ask else "n/a",
            f"{down_ask:.4f}" if down_ask else "n/a",
        )
        return None

    seconds_from_start = int((now - window.start_time).total_seconds())
    seconds_to_end = int((window.end_time - now).total_seconds())
    record_decision(
        conn,
        window,
        decision,
        seconds_from_start=seconds_from_start,
        seconds_to_end=seconds_to_end,
    )
    return decision


def resolve_pending(
    conn: sqlite3.Connection,
    client: httpx.Client,
    *,
    grace_seconds: int,
) -> int:
    now_ts = int(time.time())
    rows = conn.execute(
        """
        SELECT market_id, event_slug, window_end_ts, matched_shares, gross_cost_usd, entry_fee_usd
        FROM paired_carry_paper_trades
        WHERE resolved = 0
        ORDER BY window_end_ts
        """
    ).fetchall()
    resolved = 0
    for market_id, slug, window_end_ts, matched_shares, gross_cost_usd, entry_fee_usd in rows:
        if now_ts < int(window_end_ts) + grace_seconds:
            continue
        winning_outcome = fetch_winning_outcome(client, slug)
        if winning_outcome is None:
            continue
        payout = float(matched_shares)
        realized_pnl = payout - float(gross_cost_usd) - float(entry_fee_usd)
        record_resolution(
            conn,
            str(market_id),
            winning_outcome=winning_outcome,
            payout_usd=payout,
            realized_pnl_usd=realized_pnl,
        )
        resolved += 1
    return resolved


def print_summary(conn: sqlite3.Connection) -> None:
    row = conn.execute(
        """
        SELECT
            COUNT(*) AS total,
            COALESCE(SUM(CASE WHEN resolved = 1 THEN 1 ELSE 0 END), 0) AS resolved,
            COALESCE(SUM(gross_cost_usd), 0.0) AS gross_cost,
            COALESCE(SUM(entry_fee_usd), 0.0) AS fees,
            COALESCE(SUM(locked_pnl_usd), 0.0) AS locked_pnl,
            COALESCE(SUM(realized_pnl_usd), 0.0) AS realized_pnl
        FROM paired_carry_paper_trades
        """
    ).fetchone()
    total, resolved, gross_cost, fees, locked_pnl, realized_pnl = row
    if total == 0:
        logger.info("[SUMMARY] no entries yet")
        return
    logger.info(
        "[SUMMARY] entries=%d resolved=%d gross_cost=$%.2f fees=$%.2f locked_pnl=$%+.2f realized_pnl=$%+.2f",
        total,
        resolved,
        gross_cost or 0.0,
        fees or 0.0,
        locked_pnl or 0.0,
        realized_pnl or 0.0,
    )


def tick(
    conn: sqlite3.Connection,
    client: httpx.Client,
    *,
    assets: tuple[str, ...],
    intervals: tuple[str, ...],
    max_pair_cost: float,
    max_position_usd: float,
    min_seconds_from_start: int,
    max_seconds_from_start: int,
    grace_seconds: int,
) -> None:
    windows = fetch_active_windows(client, assets=assets, intervals=intervals)
    for window in windows:
        decision = maybe_enter(
            conn,
            client,
            window,
            max_pair_cost=max_pair_cost,
            max_position_usd=max_position_usd,
            min_seconds_from_start=min_seconds_from_start,
            max_seconds_from_start=max_seconds_from_start,
        )
        if decision is not None:
            logger.warning(
                "[ENTER] %s pair_cost=%.4f matched_shares=%.2f gross=$%.2f fee=$%.3f locked_pnl=$%+.3f",
                window.event_slug,
                decision.up_ask + decision.down_ask,
                decision.matched_shares,
                decision.gross_cost_usd,
                decision.entry_fee_usd,
                decision.locked_pnl_usd,
            )
    resolved = resolve_pending(conn, client, grace_seconds=grace_seconds)
    if resolved:
        logger.info("[RESOLVE] resolved %d windows", resolved)


def parse_csv_arg(raw: str) -> tuple[str, ...]:
    return tuple(part.strip().lower() for part in raw.split(",") if part.strip())


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", default=DEFAULT_DB)
    ap.add_argument("--assets", default=",".join(DEFAULT_ASSETS))
    ap.add_argument("--intervals", default=",".join(DEFAULT_INTERVALS))
    ap.add_argument("--loop", type=int, default=0, help="sleep N seconds between ticks")
    ap.add_argument("--max-pair-cost", type=float, default=DEFAULT_MAX_PAIR_COST)
    ap.add_argument("--max-position-usd", type=float, default=DEFAULT_MAX_POSITION_USD)
    ap.add_argument("--min-seconds-from-start", type=int, default=DEFAULT_MIN_SECONDS_FROM_START)
    ap.add_argument("--max-seconds-from-start", type=int, default=DEFAULT_MAX_SECONDS_FROM_START)
    ap.add_argument("--resolution-grace-seconds", type=int, default=DEFAULT_RESOLUTION_GRACE_SECONDS)
    args = ap.parse_args()

    assets = parse_csv_arg(args.assets)
    intervals = parse_csv_arg(args.intervals)
    invalid_intervals = [label for label in intervals if label not in INTERVAL_MINUTES]
    if invalid_intervals:
        raise SystemExit(f"unsupported intervals: {', '.join(invalid_intervals)}")

    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
        stream=sys.stdout,
    )

    conn = init_db(args.db)
    client = httpx.Client(timeout=15.0, headers={"User-Agent": "paired-carry-paper/1.0"})
    logger.warning(
        "[START] paired carry paper bot assets=%s intervals=%s max_pair_cost=%.3f "
        "max_position=$%.2f band=%ds-%ds db=%s",
        ",".join(asset.upper() for asset in assets),
        ",".join(intervals),
        args.max_pair_cost,
        args.max_position_usd,
        args.min_seconds_from_start,
        args.max_seconds_from_start,
        args.db,
    )

    try:
        while True:
            tick(
                conn,
                client,
                assets=assets,
                intervals=intervals,
                max_pair_cost=args.max_pair_cost,
                max_position_usd=args.max_position_usd,
                min_seconds_from_start=args.min_seconds_from_start,
                max_seconds_from_start=args.max_seconds_from_start,
                grace_seconds=args.resolution_grace_seconds,
            )
            print_summary(conn)
            if args.loop <= 0:
                break
            time.sleep(args.loop)
    finally:
        client.close()
        conn.close()


if __name__ == "__main__":
    main()
