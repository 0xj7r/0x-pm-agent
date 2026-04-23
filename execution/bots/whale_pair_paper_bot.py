#!/usr/bin/env python3
"""Whale-family two-sided pair/merge paper bot.

This is a closer paper replica of the `0xb27b...` / `xuanxuan008` family than
`paired_carry_paper_bot.py`:

  - repeated small buys across the same window
  - independent per-side inventory
  - automatic FIFO pairing as opposite inventory appears
  - immediate synthetic MERGE of matched pairs
  - residual unmatched legs held to resolution

It is still intentionally paper-only and still omits live execution details
like queue position, maker posting, and partial order acknowledgements.
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


logger = logging.getLogger("whale_pair_paper")

GAMMA = "https://gamma-api.polymarket.com"
CLOB = "https://clob.polymarket.com"

DEFAULT_DB = "data/whale_pair_paper_trades.db"
DEFAULT_ACCUMULATE_PRICE_MAX = 0.50
DEFAULT_AGGRESSIVE_PRICE_MAX = 0.10
DEFAULT_BASE_CLIP_USD = 10.0
DEFAULT_AGGRESSIVE_CLIP_USD = 25.0
DEFAULT_MAX_GROSS_COST_USD = 200.0
DEFAULT_MAX_SECONDS_FROM_START = 298
DEFAULT_MIN_SECONDS_FROM_START = 10
DEFAULT_COMPLETION_MIN_PNL_PER_SHARE = 0.002
DEFAULT_MAX_IMBALANCE_RATIO = 3.0
DEFAULT_RESOLUTION_GRACE_SECONDS = 30


@dataclass(frozen=True)
class Window:
    market_id: str
    condition_id: str
    event_slug: str
    event_id: str
    start_time: datetime
    end_time: datetime
    up_token_id: str
    down_token_id: str


@dataclass(frozen=True)
class OrderbookTop:
    ask: float
    ask_size: float


@dataclass(frozen=True)
class OpenInventory:
    shares: float
    all_in_cost_usd: float

    @property
    def all_in_cost_per_share(self) -> float | None:
        if self.shares <= 0:
            return None
        return self.all_in_cost_usd / self.shares


@dataclass(frozen=True)
class WhalePairPaperConfig:
    accumulate_price_max: float = DEFAULT_ACCUMULATE_PRICE_MAX
    aggressive_price_max: float = DEFAULT_AGGRESSIVE_PRICE_MAX
    base_clip_usd: float = DEFAULT_BASE_CLIP_USD
    aggressive_clip_usd: float = DEFAULT_AGGRESSIVE_CLIP_USD
    max_gross_cost_usd: float = DEFAULT_MAX_GROSS_COST_USD
    min_seconds_from_start: int = DEFAULT_MIN_SECONDS_FROM_START
    max_seconds_from_start: int = DEFAULT_MAX_SECONDS_FROM_START
    completion_min_pnl_per_share: float = DEFAULT_COMPLETION_MIN_PNL_PER_SHARE
    max_imbalance_ratio: float = DEFAULT_MAX_IMBALANCE_RATIO
    grace_seconds: int = DEFAULT_RESOLUTION_GRACE_SECONDS


class WhalePairPaperBot:
    def __init__(
        self,
        *,
        db_path: str,
        config: WhalePairPaperConfig,
        client: httpx.Client | None = None,
    ) -> None:
        self.db_path = db_path
        self.config = config
        self.conn = init_db(db_path)
        self.client = client or httpx.Client(timeout=15.0, headers={"User-Agent": "whale-pair-paper/1.0"})
        self._owns_client = client is None

    def close(self) -> None:
        if self._owns_client:
            self.client.close()
        self.conn.close()

    def step(self) -> None:
        tick(
            self.conn,
            self.client,
            accumulate_price_max=self.config.accumulate_price_max,
            aggressive_price_max=self.config.aggressive_price_max,
            base_clip_usd=self.config.base_clip_usd,
            aggressive_clip_usd=self.config.aggressive_clip_usd,
            max_gross_cost_usd=self.config.max_gross_cost_usd,
            min_seconds_from_start=self.config.min_seconds_from_start,
            max_seconds_from_start=self.config.max_seconds_from_start,
            completion_min_pnl_per_share=self.config.completion_min_pnl_per_share,
            max_imbalance_ratio=self.config.max_imbalance_ratio,
            grace_seconds=self.config.grace_seconds,
        )

    def run(self, loop_seconds: int) -> None:
        while True:
            self.step()
            print_summary(self.conn)
            if loop_seconds <= 0:
                break
            time.sleep(loop_seconds)


def build_config_from_args(args: argparse.Namespace) -> WhalePairPaperConfig:
    return WhalePairPaperConfig(
        accumulate_price_max=args.accumulate_price_max,
        aggressive_price_max=args.aggressive_price_max,
        base_clip_usd=args.base_clip_usd,
        aggressive_clip_usd=args.aggressive_clip_usd,
        max_gross_cost_usd=args.max_gross_cost_usd,
        min_seconds_from_start=args.min_seconds_from_start,
        max_seconds_from_start=args.max_seconds_from_start,
        completion_min_pnl_per_share=args.completion_min_pnl_per_share,
        max_imbalance_ratio=args.max_imbalance_ratio,
        grace_seconds=args.resolution_grace_seconds,
    )


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
        CREATE TABLE IF NOT EXISTS whale_pair_markets (
            market_id TEXT PRIMARY KEY,
            condition_id TEXT NOT NULL,
            event_slug TEXT NOT NULL,
            event_id TEXT NOT NULL,
            window_start_ts INTEGER NOT NULL,
            window_end_ts INTEGER NOT NULL,
            resolved INTEGER NOT NULL DEFAULT 0,
            winning_outcome TEXT,
            merged_pnl_usd REAL,
            residual_pnl_usd REAL
        )
        """
    )
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS whale_pair_fills (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            market_id TEXT NOT NULL,
            ts TEXT NOT NULL,
            side TEXT NOT NULL,
            reason TEXT NOT NULL,
            price REAL NOT NULL,
            ask_size REAL NOT NULL,
            shares REAL NOT NULL,
            gross_cost_usd REAL NOT NULL,
            fee_usd REAL NOT NULL
        )
        """
    )
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS whale_pair_open_lots (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            market_id TEXT NOT NULL,
            fill_id INTEGER NOT NULL,
            side TEXT NOT NULL,
            opened_ts TEXT NOT NULL,
            shares_remaining REAL NOT NULL,
            gross_cost_remaining_usd REAL NOT NULL,
            fee_remaining_usd REAL NOT NULL
        )
        """
    )
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS whale_pair_matches (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            market_id TEXT NOT NULL,
            matched_ts TEXT NOT NULL,
            up_fill_id INTEGER,
            down_fill_id INTEGER,
            shares REAL NOT NULL,
            up_cost_usd REAL NOT NULL,
            up_fee_usd REAL NOT NULL,
            down_cost_usd REAL NOT NULL,
            down_fee_usd REAL NOT NULL,
            payout_usd REAL NOT NULL,
            realized_pnl_usd REAL NOT NULL
        )
        """
    )
    conn.commit()
    return conn


def _parse_window(event: dict[str, Any]) -> Window | None:
    slug = str(event.get("slug") or "")
    if not slug.startswith("btc-updown-5m-"):
        return None
    try:
        start_ts = int(slug.rsplit("-", 1)[-1])
    except ValueError:
        return None
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
        raw = market.get("clobTokenIds", "[]")
        try:
            ids = json.loads(raw) if isinstance(raw, str) else raw
        except (TypeError, ValueError, json.JSONDecodeError):
            ids = []
        if len(ids) >= 2:
            up_token_id = up_token_id or str(ids[0])
            down_token_id = down_token_id or str(ids[1])
    if not up_token_id or not down_token_id:
        return None
    start_time = datetime.fromtimestamp(start_ts, tz=timezone.utc)
    return Window(
        market_id=str(market.get("id") or event.get("id") or ""),
        condition_id=str(market.get("conditionId") or ""),
        event_slug=slug,
        event_id=str(event.get("id") or ""),
        start_time=start_time,
        end_time=start_time + timedelta(minutes=5),
        up_token_id=up_token_id,
        down_token_id=down_token_id,
    )


def fetch_active_windows(client: httpx.Client) -> list[Window]:
    now = datetime.now(timezone.utc)
    base = now.replace(minute=(now.minute // 5) * 5, second=0, microsecond=0)
    slugs = [f"btc-updown-5m-{int((base + timedelta(minutes=5*i)).timestamp())}" for i in (-1, 0)]
    windows: list[Window] = []
    for slug in slugs:
        try:
            resp = client.get(f"{GAMMA}/events", params={"slug": slug}, timeout=10.0)
            if resp.status_code != 200:
                continue
            data = resp.json()
        except Exception as exc:
            logger.debug("gamma fetch failed for %s: %s", slug, exc)
            continue
        if not isinstance(data, list) or not data:
            continue
        window = _parse_window(data[0])
        if window is None:
            continue
        if window.start_time <= now < window.end_time:
            windows.append(window)
    return windows


def fetch_book_top(client: httpx.Client, token_id: str) -> OrderbookTop | None:
    try:
        resp = client.get(f"{CLOB}/book", params={"token_id": token_id}, timeout=10.0)
        if resp.status_code != 200:
            return None
        data = resp.json()
        asks = data.get("asks") or []
    except Exception as exc:
        logger.debug("book fetch failed for %s: %s", token_id[:10], exc)
        return None
    best_price = None
    best_size = None
    for level in asks:
        price = _safe_float(level.get("price"))
        size = _safe_float(level.get("size"))
        if price <= 0:
            continue
        if best_price is None or price < best_price:
            best_price = price
            best_size = size
    if best_price is None:
        return None
    return OrderbookTop(ask=best_price, ask_size=best_size or 0.0)


def ensure_market_row(conn: sqlite3.Connection, window: Window) -> None:
    conn.execute(
        """
        INSERT OR IGNORE INTO whale_pair_markets (
            market_id, condition_id, event_slug, event_id, window_start_ts, window_end_ts, resolved
        ) VALUES (?, ?, ?, ?, ?, ?, 0)
        """,
        (
            window.market_id,
            window.condition_id,
            window.event_slug,
            window.event_id,
            int(window.start_time.timestamp()),
            int(window.end_time.timestamp()),
        ),
    )
    conn.commit()


def market_gross_cost_usd(conn: sqlite3.Connection, market_id: str) -> float:
    row = conn.execute(
        "SELECT COALESCE(SUM(gross_cost_usd), 0.0) FROM whale_pair_fills WHERE market_id = ?",
        (market_id,),
    ).fetchone()
    return float(row[0] or 0.0)


def open_inventory(conn: sqlite3.Connection, market_id: str, side: str) -> OpenInventory:
    row = conn.execute(
        """
        SELECT
            COALESCE(SUM(shares_remaining), 0.0),
            COALESCE(SUM(gross_cost_remaining_usd + fee_remaining_usd), 0.0)
        FROM whale_pair_open_lots
        WHERE market_id = ? AND side = ? AND shares_remaining > 0
        """,
        (market_id, side),
    ).fetchone()
    return OpenInventory(shares=float(row[0] or 0.0), all_in_cost_usd=float(row[1] or 0.0))


def choose_accumulate_clip_usd(price: float, *, accumulate_price_max: float, aggressive_price_max: float, base_clip_usd: float, aggressive_clip_usd: float) -> float | None:
    if price <= 0:
        return None
    if price <= aggressive_price_max:
        return aggressive_clip_usd
    if price <= accumulate_price_max:
        return base_clip_usd
    return None


def completion_pnl_per_share(opposite_all_in_per_share: float | None, current_price: float) -> float | None:
    if opposite_all_in_per_share is None or current_price <= 0:
        return None
    current_fee_per_share = taker_fee_usd(current_price, current_price)
    return 1.0 - opposite_all_in_per_share - current_price - current_fee_per_share


def choose_fill_shares(
    *,
    price: float,
    ask_size: float,
    clip_usd: float,
    remaining_budget_usd: float,
) -> float:
    if price <= 0 or ask_size <= 0 or clip_usd <= 0 or remaining_budget_usd <= 0:
        return 0.0
    return min(ask_size, clip_usd / price, remaining_budget_usd / price)


def insert_fill(
    conn: sqlite3.Connection,
    market_id: str,
    side: str,
    reason: str,
    price: float,
    ask_size: float,
    shares: float,
) -> int:
    gross_cost = shares * price
    fee = taker_fee_usd(price, gross_cost)
    ts = datetime.now(timezone.utc).isoformat()
    cur = conn.execute(
        """
        INSERT INTO whale_pair_fills (
            market_id, ts, side, reason, price, ask_size, shares, gross_cost_usd, fee_usd
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        (market_id, ts, side, reason, price, ask_size, shares, gross_cost, fee),
    )
    fill_id = int(cur.lastrowid)
    conn.execute(
        """
        INSERT INTO whale_pair_open_lots (
            market_id, fill_id, side, opened_ts, shares_remaining,
            gross_cost_remaining_usd, fee_remaining_usd
        ) VALUES (?, ?, ?, ?, ?, ?, ?)
        """,
        (market_id, fill_id, side, ts, shares, gross_cost, fee),
    )
    conn.commit()
    return fill_id


def _oldest_open_lot(conn: sqlite3.Connection, market_id: str, side: str) -> tuple | None:
    return conn.execute(
        """
        SELECT id, fill_id, shares_remaining, gross_cost_remaining_usd, fee_remaining_usd
        FROM whale_pair_open_lots
        WHERE market_id = ? AND side = ? AND shares_remaining > 0
        ORDER BY id
        LIMIT 1
        """,
        (market_id, side),
    ).fetchone()


def match_and_merge(conn: sqlite3.Connection, market_id: str) -> int:
    matches = 0
    while True:
        up = _oldest_open_lot(conn, market_id, "Up")
        down = _oldest_open_lot(conn, market_id, "Down")
        if up is None or down is None:
            break
        up_lot_id, up_fill_id, up_shares, up_cost, up_fee = up
        dn_lot_id, dn_fill_id, dn_shares, dn_cost, dn_fee = down
        shares = min(float(up_shares), float(dn_shares))
        up_ratio = shares / float(up_shares)
        dn_ratio = shares / float(dn_shares)
        alloc_up_cost = float(up_cost) * up_ratio
        alloc_up_fee = float(up_fee) * up_ratio
        alloc_dn_cost = float(dn_cost) * dn_ratio
        alloc_dn_fee = float(dn_fee) * dn_ratio
        payout = shares
        pnl = payout - alloc_up_cost - alloc_up_fee - alloc_dn_cost - alloc_dn_fee
        conn.execute(
            """
            INSERT INTO whale_pair_matches (
                market_id, matched_ts, up_fill_id, down_fill_id, shares,
                up_cost_usd, up_fee_usd, down_cost_usd, down_fee_usd,
                payout_usd, realized_pnl_usd
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                market_id,
                datetime.now(timezone.utc).isoformat(),
                up_fill_id,
                dn_fill_id,
                shares,
                alloc_up_cost,
                alloc_up_fee,
                alloc_dn_cost,
                alloc_dn_fee,
                payout,
                pnl,
            ),
        )
        conn.execute(
            """
            UPDATE whale_pair_open_lots
            SET shares_remaining = shares_remaining - ?,
                gross_cost_remaining_usd = gross_cost_remaining_usd - ?,
                fee_remaining_usd = fee_remaining_usd - ?
            WHERE id = ?
            """,
            (shares, alloc_up_cost, alloc_up_fee, up_lot_id),
        )
        conn.execute(
            """
            UPDATE whale_pair_open_lots
            SET shares_remaining = shares_remaining - ?,
                gross_cost_remaining_usd = gross_cost_remaining_usd - ?,
                fee_remaining_usd = fee_remaining_usd - ?
            WHERE id = ?
            """,
            (shares, alloc_dn_cost, alloc_dn_fee, dn_lot_id),
        )
        conn.commit()
        matches += 1
    return matches


def can_add_to_side(
    *,
    side: str,
    price: float,
    opposite_inventory: OpenInventory,
    same_inventory: OpenInventory,
    completion_min_pnl_per_share: float,
    max_imbalance_ratio: float,
) -> tuple[bool, str]:
    clip_usd = choose_accumulate_clip_usd(
        price,
        accumulate_price_max=DEFAULT_ACCUMULATE_PRICE_MAX,
        aggressive_price_max=DEFAULT_AGGRESSIVE_PRICE_MAX,
        base_clip_usd=DEFAULT_BASE_CLIP_USD,
        aggressive_clip_usd=DEFAULT_AGGRESSIVE_CLIP_USD,
    )
    if clip_usd is not None:
        projected_same = same_inventory.shares + (clip_usd / price)
        projected_opp = max(opposite_inventory.shares, 1e-9)
        if opposite_inventory.shares > 0 and projected_same / projected_opp > max_imbalance_ratio:
            return False, "imbalance_cap"
        return True, "accumulate"

    pnl_per_share = completion_pnl_per_share(opposite_inventory.all_in_cost_per_share, price)
    if opposite_inventory.shares > same_inventory.shares and pnl_per_share is not None and pnl_per_share >= completion_min_pnl_per_share:
        return True, "complete"
    return False, "no_edge"


def maybe_fill_side(
    conn: sqlite3.Connection,
    market_id: str,
    side: str,
    top: OrderbookTop,
    *,
    remaining_budget_usd: float,
    opposite_inventory: OpenInventory,
    same_inventory: OpenInventory,
    accumulate_price_max: float,
    aggressive_price_max: float,
    base_clip_usd: float,
    aggressive_clip_usd: float,
    completion_min_pnl_per_share: float,
    max_imbalance_ratio: float,
) -> int | None:
    price = top.ask
    ask_size = top.ask_size
    clip_usd = choose_accumulate_clip_usd(
        price,
        accumulate_price_max=accumulate_price_max,
        aggressive_price_max=aggressive_price_max,
        base_clip_usd=base_clip_usd,
        aggressive_clip_usd=aggressive_clip_usd,
    )
    reason = None
    if clip_usd is not None:
        projected_same = same_inventory.shares + (clip_usd / max(price, 1e-9))
        projected_opp = max(opposite_inventory.shares, 1e-9)
        if opposite_inventory.shares > 0 and projected_same / projected_opp > max_imbalance_ratio:
            return None
        reason = "accumulate"
    else:
        pnl_per_share = completion_pnl_per_share(opposite_inventory.all_in_cost_per_share, price)
        if opposite_inventory.shares > same_inventory.shares and pnl_per_share is not None and pnl_per_share >= completion_min_pnl_per_share:
            clip_usd = base_clip_usd
            reason = "complete"
        else:
            return None

    shares = choose_fill_shares(
        price=price,
        ask_size=ask_size,
        clip_usd=clip_usd or 0.0,
        remaining_budget_usd=remaining_budget_usd,
    )
    if shares <= 0:
        return None
    return insert_fill(conn, market_id, side, reason, price, ask_size, shares)


def in_entry_zone(window: Window, now: datetime, *, min_seconds_from_start: int, max_seconds_from_start: int) -> bool:
    if not (window.start_time <= now < window.end_time):
        return False
    seconds_from_start = int((now - window.start_time).total_seconds())
    return min_seconds_from_start <= seconds_from_start <= max_seconds_from_start


def resolve_market(conn: sqlite3.Connection, client: httpx.Client, market_id: str, event_slug: str) -> bool:
    try:
        resp = client.get(f"{GAMMA}/events", params={"slug": event_slug}, timeout=10.0)
        if resp.status_code != 200:
            return False
        data = resp.json()
    except Exception:
        return False
    if not isinstance(data, list) or not data:
        return False
    event = data[0]
    if not event.get("closed"):
        return False
    winner = None
    for market in event.get("markets") or []:
        outcome_prices = market.get("outcomePrices")
        outcomes = market.get("outcomes")
        prices = json.loads(outcome_prices) if isinstance(outcome_prices, str) else outcome_prices
        outs = json.loads(outcomes) if isinstance(outcomes, str) else outcomes
        if not isinstance(prices, list) or not isinstance(outs, list):
            continue
        for price, outcome in zip(prices, outs):
            if _safe_float(price) >= 0.99:
                winner = str(outcome)
                break
    if winner is None:
        return False

    residual_rows = conn.execute(
        """
        SELECT side, COALESCE(SUM(shares_remaining),0), COALESCE(SUM(gross_cost_remaining_usd),0), COALESCE(SUM(fee_remaining_usd),0)
        FROM whale_pair_open_lots
        WHERE market_id = ? AND shares_remaining > 0
        GROUP BY side
        """,
        (market_id,),
    ).fetchall()
    residual_pnl = 0.0
    for side, shares, gross_cost, fee in residual_rows:
        payout = float(shares) if side == winner else 0.0
        residual_pnl += payout - float(gross_cost) - float(fee)

    merged_row = conn.execute(
        "SELECT COALESCE(SUM(realized_pnl_usd),0.0) FROM whale_pair_matches WHERE market_id = ?",
        (market_id,),
    ).fetchone()
    conn.execute(
        """
        UPDATE whale_pair_markets
        SET resolved = 1, winning_outcome = ?, merged_pnl_usd = ?, residual_pnl_usd = ?
        WHERE market_id = ?
        """,
        (winner, float(merged_row[0] or 0.0), residual_pnl, market_id),
    )
    conn.commit()
    return True


def resolve_pending(conn: sqlite3.Connection, client: httpx.Client, *, grace_seconds: int) -> int:
    now_ts = int(time.time())
    rows = conn.execute(
        """
        SELECT market_id, event_slug, window_end_ts
        FROM whale_pair_markets
        WHERE resolved = 0
        ORDER BY window_end_ts
        """
    ).fetchall()
    resolved = 0
    for market_id, event_slug, window_end_ts in rows:
        if now_ts < int(window_end_ts) + grace_seconds:
            continue
        if resolve_market(conn, client, str(market_id), str(event_slug)):
            resolved += 1
    return resolved


def print_summary(conn: sqlite3.Connection) -> None:
    row = conn.execute(
        """
        SELECT
            COUNT(*) AS markets,
            COALESCE(SUM(CASE WHEN resolved = 1 THEN 1 ELSE 0 END),0) AS resolved,
            COALESCE((SELECT SUM(gross_cost_usd) FROM whale_pair_fills),0.0) AS gross_cost,
            COALESCE((SELECT SUM(realized_pnl_usd) FROM whale_pair_matches),0.0) AS merged_pnl,
            COALESCE(SUM(residual_pnl_usd),0.0) AS residual_pnl
        FROM whale_pair_markets
        """
    ).fetchone()
    markets, resolved, gross_cost, merged_pnl, residual_pnl = row
    if markets == 0:
        logger.info("[SUMMARY] no markets yet")
        return
    logger.info(
        "[SUMMARY] markets=%d resolved=%d gross_cost=$%.2f merged_pnl=$%+.2f residual_pnl=$%+.2f total=$%+.2f",
        markets,
        resolved,
        gross_cost or 0.0,
        merged_pnl or 0.0,
        residual_pnl or 0.0,
        (merged_pnl or 0.0) + (residual_pnl or 0.0),
    )


def tick(
    conn: sqlite3.Connection,
    client: httpx.Client,
    *,
    accumulate_price_max: float,
    aggressive_price_max: float,
    base_clip_usd: float,
    aggressive_clip_usd: float,
    max_gross_cost_usd: float,
    min_seconds_from_start: int,
    max_seconds_from_start: int,
    completion_min_pnl_per_share: float,
    max_imbalance_ratio: float,
    grace_seconds: int,
) -> None:
    now = datetime.now(timezone.utc)
    for window in fetch_active_windows(client):
        ensure_market_row(conn, window)
        if not in_entry_zone(window, now, min_seconds_from_start=min_seconds_from_start, max_seconds_from_start=max_seconds_from_start):
            continue
        gross_used = market_gross_cost_usd(conn, window.market_id)
        remaining_budget = max_gross_cost_usd - gross_used
        if remaining_budget <= 0:
            continue

        up_top = fetch_book_top(client, window.up_token_id)
        down_top = fetch_book_top(client, window.down_token_id)
        if up_top is None or down_top is None:
            continue

        # cheaper side first
        sides = [("Up", up_top), ("Down", down_top)]
        sides.sort(key=lambda item: item[1].ask)
        for side, top in sides:
            if remaining_budget <= 0:
                break
            same_inv = open_inventory(conn, window.market_id, side)
            opp_inv = open_inventory(conn, window.market_id, "Down" if side == "Up" else "Up")
            fill_id = maybe_fill_side(
                conn,
                window.market_id,
                side,
                top,
                remaining_budget_usd=remaining_budget,
                opposite_inventory=opp_inv,
                same_inventory=same_inv,
                accumulate_price_max=accumulate_price_max,
                aggressive_price_max=aggressive_price_max,
                base_clip_usd=base_clip_usd,
                aggressive_clip_usd=aggressive_clip_usd,
                completion_min_pnl_per_share=completion_min_pnl_per_share,
                max_imbalance_ratio=max_imbalance_ratio,
            )
            if fill_id is not None:
                remaining_budget = max_gross_cost_usd - market_gross_cost_usd(conn, window.market_id)
                logger.warning(
                    "[FILL] %s side=%s reason=%s price=%.4f ask_size=%.2f budget_left=$%.2f",
                    window.event_slug,
                    side,
                    conn.execute("SELECT reason FROM whale_pair_fills WHERE id = ?", (fill_id,)).fetchone()[0],
                    top.ask,
                    top.ask_size,
                    remaining_budget,
                )
        matches = match_and_merge(conn, window.market_id)
        if matches:
            logger.info("[MERGE] %s matched %d lot(s)", window.event_slug, matches)
    resolved = resolve_pending(conn, client, grace_seconds=grace_seconds)
    if resolved:
        logger.info("[RESOLVE] resolved %d market(s)", resolved)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", default=DEFAULT_DB)
    ap.add_argument("--loop", type=int, default=0)
    ap.add_argument("--accumulate-price-max", type=float, default=DEFAULT_ACCUMULATE_PRICE_MAX)
    ap.add_argument("--aggressive-price-max", type=float, default=DEFAULT_AGGRESSIVE_PRICE_MAX)
    ap.add_argument("--base-clip-usd", type=float, default=DEFAULT_BASE_CLIP_USD)
    ap.add_argument("--aggressive-clip-usd", type=float, default=DEFAULT_AGGRESSIVE_CLIP_USD)
    ap.add_argument("--max-gross-cost-usd", type=float, default=DEFAULT_MAX_GROSS_COST_USD)
    ap.add_argument("--min-seconds-from-start", type=int, default=DEFAULT_MIN_SECONDS_FROM_START)
    ap.add_argument("--max-seconds-from-start", type=int, default=DEFAULT_MAX_SECONDS_FROM_START)
    ap.add_argument("--completion-min-pnl-per-share", type=float, default=DEFAULT_COMPLETION_MIN_PNL_PER_SHARE)
    ap.add_argument("--max-imbalance-ratio", type=float, default=DEFAULT_MAX_IMBALANCE_RATIO)
    ap.add_argument("--resolution-grace-seconds", type=int, default=DEFAULT_RESOLUTION_GRACE_SECONDS)
    args = ap.parse_args()
    config = build_config_from_args(args)

    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
        stream=sys.stdout,
    )
    logger.warning(
        "[START] whale pair paper bot db=%s clip=$%.2f/$%.2f accumulate<=%.2f aggressive<=%.2f max_gross=$%.2f band=%ds-%ds",
        args.db,
        config.base_clip_usd,
        config.aggressive_clip_usd,
        config.accumulate_price_max,
        config.aggressive_price_max,
        config.max_gross_cost_usd,
        config.min_seconds_from_start,
        config.max_seconds_from_start,
    )
    bot = WhalePairPaperBot(
        db_path=args.db,
        config=config,
    )
    try:
        bot.run(loop_seconds=args.loop)
    finally:
        bot.close()


if __name__ == "__main__":
    main()
