#!/usr/bin/env python3
"""
P&L report for Polymarket wallets trading 5-min BTC up/down markets.

Correctness basis (per user request):
  - Cash: on-chain USDC.e balance (ERC-20 balanceOf) is ground truth.
  - Positions / mark-to-market: Polymarket Data API positions are ground truth.
  - SQLite event_log is used only as a cross-check (it may contain phantoms).
"""

from __future__ import annotations

import argparse
import json
import os
import sqlite3
import sys
from dataclasses import dataclass
from datetime import UTC, datetime
from decimal import Decimal, ROUND_HALF_UP
from typing import Any

import httpx


USDC_E_POLYGON = "0x2791Bca1F2de4661ED88A30C99A7a9449Aa84174"
DATA_API_BASE = "https://data-api.polymarket.com"
DEFAULT_RPC_URL = os.getenv("POLYGON_RPC_URL", "https://polygon-bor-rpc.publicnode.com")


def _d(x: Any) -> Decimal:
    if x is None:
        return Decimal("0")
    if isinstance(x, Decimal):
        return x
    # Convert via str to avoid float binary artifacts.
    return Decimal(str(x))


def _money(x: Decimal) -> str:
    q = x.quantize(Decimal("0.01"), rounding=ROUND_HALF_UP)
    sign = "+" if q > 0 else ""
    return f"{sign}${q:,.2f}"


def _money0(x: Decimal) -> str:
    q = x.quantize(Decimal("0.01"), rounding=ROUND_HALF_UP)
    return f"${q:,.2f}"


def _pct(x: Decimal) -> str:
    q = (x * Decimal("100")).quantize(Decimal("0.01"), rounding=ROUND_HALF_UP)
    sign = "+" if q > 0 else ""
    return f"{sign}{q:.2f}%"


def _short_addr(a: str) -> str:
    a = a or ""
    if len(a) <= 12:
        return a
    return f"{a[:8]}…{a[-4:]}"


def _parse_wallet(s: str) -> str:
    s = (s or "").strip()
    if not s:
        raise ValueError("wallet is required")
    if not s.startswith("0x"):
        s = "0x" + s
    return s


def _abi_selector(sig: str) -> str:
    # keccak256("balanceOf(address)")[:4] = 0x70a08231 (hard-coded below)
    if sig == "balanceOf(address)":
        return "70a08231"
    raise ValueError(f"unsupported selector: {sig}")


def _pad32(hex_no_0x: str) -> str:
    return hex_no_0x.rjust(64, "0")


async def _eth_call(
    rpc_url: str, to_addr: str, data_hex: str, timeout_s: float = 20.0
) -> str:
    payload = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_call",
        "params": [
            {"to": to_addr, "data": data_hex},
            "latest",
        ],
    }
    async with httpx.AsyncClient(timeout=timeout_s) as client:
        r = await client.post(rpc_url, json=payload)
        r.raise_for_status()
        out = r.json()
    if "error" in out:
        raise RuntimeError(f"RPC eth_call error: {out['error']}")
    result = out.get("result")
    if not isinstance(result, str) or not result.startswith("0x"):
        raise RuntimeError(f"RPC eth_call returned invalid result: {result!r}")
    return result


async def get_erc20_balance_usdc(
    rpc_url: str, token_address: str, wallet: str
) -> Decimal:
    # balanceOf(address)
    sel = _abi_selector("balanceOf(address)")
    addr_no_0x = wallet.lower().replace("0x", "")
    data = "0x" + sel + _pad32(addr_no_0x)
    raw = await _eth_call(rpc_url, token_address, data)
    bal_int = int(raw, 16)
    return _d(bal_int) / Decimal("1000000")


async def fetch_positions(wallet: str, timeout_s: float = 30.0) -> list[dict[str, Any]]:
    async with httpx.AsyncClient(timeout=timeout_s) as client:
        r = await client.get(f"{DATA_API_BASE}/positions", params={"user": wallet})
        r.raise_for_status()
        data = r.json()
    if not isinstance(data, list):
        raise RuntimeError(f"positions response not a list: {type(data)}")
    return [d for d in data if isinstance(d, dict)]


async def fetch_activity(wallet: str, timeout_s: float = 30.0) -> list[dict[str, Any]]:
    async with httpx.AsyncClient(timeout=timeout_s) as client:
        r = await client.get(f"{DATA_API_BASE}/activity", params={"user": wallet})
        r.raise_for_status()
        data = r.json()
    if not isinstance(data, list):
        raise RuntimeError(f"activity response not a list: {type(data)}")
    return [d for d in data if isinstance(d, dict)]


def read_event_log_summary(
    db_path: str, day_utc: str | None
) -> dict[str, Any]:
    """Best-effort cross-check against event_log; never treated as truth."""
    try:
        conn = sqlite3.connect(db_path)
    except Exception as exc:
        return {"ok": False, "error": f"db open failed: {exc}"}

    conn.row_factory = sqlite3.Row
    try:
        rows = conn.execute(
            "SELECT id, timestamp, window_id, event_type, details FROM event_log ORDER BY id"
        ).fetchall()
    except Exception as exc:
        conn.close()
        return {"ok": False, "error": f"event_log query failed: {exc}"}
    finally:
        conn.close()

    def _matches_day(ts: str) -> bool:
        if not day_utc:
            return True
        # SQLite CURRENT_TIMESTAMP format: "YYYY-MM-DD HH:MM:SS"
        return isinstance(ts, str) and ts.startswith(day_utc)

    entries = 0
    entries_missing_order = 0
    resolutions = 0
    pnl_sum = Decimal("0")

    for r in rows:
        ts = r["timestamp"]
        if not _matches_day(ts):
            continue
        et = r["event_type"]
        details_raw = r["details"]
        try:
            details = json.loads(details_raw) if details_raw else None
        except Exception:
            details = None

        if et == "entry":
            entries += 1
            if isinstance(details, dict):
                if not details.get("order_id"):
                    entries_missing_order += 1
            else:
                entries_missing_order += 1
        elif et == "resolution":
            resolutions += 1
            if isinstance(details, dict):
                pnl = details.get("pnl_usd")
                if pnl is not None:
                    pnl_sum += _d(pnl)

    return {
        "ok": True,
        "entries": entries,
        "entries_missing_order_id": entries_missing_order,
        "resolutions": resolutions,
        "pnl_usd_sum": pnl_sum,
    }


@dataclass(frozen=True)
class PositionRow:
    condition_id: str
    title: str
    outcome: str
    shares: Decimal
    avg_price: Decimal
    cost_usdc: Decimal
    cur_price: Decimal
    value_usdc: Decimal
    pnl_usdc: Decimal
    redeemable: bool

    @property
    def status(self) -> str:
        if self.redeemable and self.value_usdc > 0:
            return "RESOLVED_WIN (unredeemed)"
        if self.redeemable and self.value_usdc == 0:
            return "RESOLVED_LOSS (0-value)"
        return "OPEN (unresolved)"


@dataclass(frozen=True)
class ClosedRow:
    condition_id: str
    title: str
    cost_usdc: Decimal
    payout_usdc: Decimal

    @property
    def pnl_usdc(self) -> Decimal:
        return self.payout_usdc - self.cost_usdc


def _format_table(headers: list[str], rows: list[list[str]]) -> str:
    widths = [len(h) for h in headers]
    for row in rows:
        for i, cell in enumerate(row):
            widths[i] = max(widths[i], len(cell))
    def fmt_row(r: list[str]) -> str:
        return "  ".join(c.ljust(widths[i]) for i, c in enumerate(r))
    out = [fmt_row(headers), fmt_row(["-" * w for w in widths])]
    out.extend(fmt_row(r) for r in rows)
    return "\n".join(out)


def _epoch_to_utc(ts: int | float | None) -> datetime | None:
    if ts is None:
        return None
    try:
        return datetime.fromtimestamp(float(ts), tz=UTC)
    except Exception:
        return None


def _filter_activity_day(activity: list[dict[str, Any]], day_utc: str | None) -> list[dict[str, Any]]:
    if not day_utc:
        return activity
    out: list[dict[str, Any]] = []
    for a in activity:
        dt = _epoch_to_utc(a.get("timestamp"))
        if dt is None:
            continue
        if dt.strftime("%Y-%m-%d") == day_utc:
            out.append(a)
    return out


def aggregate_activity_by_condition(
    activity: list[dict[str, Any]],
) -> dict[str, dict[str, Any]]:
    """Group /activity rows by conditionId, summing BUY cost and REDEEM payout.

    Tracks ``redeem_seen`` explicitly because redeemed losers emit a REDEEM
    event with ``usdcSize == 0``. Without this flag those markets would be
    indistinguishable from "never redeemed" markets in downstream logic.
    """
    by_cid: dict[str, dict[str, Any]] = {}
    for a in activity:
        cid = (a.get("conditionId") or "").lower()
        if not cid:
            continue
        rec = by_cid.setdefault(
            cid,
            {
                "title": a.get("title") or "",
                "cost": Decimal("0"),
                "payout": Decimal("0"),
                "redeem_seen": False,
            },
        )
        t = (a.get("type") or "").upper()
        if not rec["title"] and a.get("title"):
            rec["title"] = a.get("title")
        if t == "TRADE":
            # usdcSize is spend (positive number); side indicates BUY/SELL.
            side = (a.get("side") or "").upper()
            if side == "BUY":
                rec["cost"] += _d(a.get("usdcSize"))
        elif t == "REDEEM":
            rec["redeem_seen"] = True
            rec["payout"] += _d(a.get("usdcSize"))
    return by_cid


def build_closed_rows_from_activity(activity: list[dict[str, Any]]) -> list[ClosedRow]:
    """Return closed-trade rows for every market with a REDEEM event.

    A redeemed loser has ``cost > 0`` (we bought shares) and
    ``payout == 0`` (the worthless token was burned). The old filter
    ``payout > 0 and cost > 0`` silently dropped these, so the report
    under-counted losses once the redemption sweep ran. The correct
    condition is "we bought AND the market was redeemed in any form".
    """
    by_cid = aggregate_activity_by_condition(activity)
    out: list[ClosedRow] = []
    for cid, rec in by_cid.items():
        cost = rec["cost"]
        payout = rec["payout"]
        if cost > 0 and (payout > 0 or rec.get("redeem_seen")):
            out.append(
                ClosedRow(
                    condition_id=cid,
                    title=str(rec.get("title") or ""),
                    cost_usdc=cost,
                    payout_usdc=payout,
                )
            )
    # Most recent first for readability.
    return sorted(out, key=lambda r: r.title, reverse=True)


def build_position_rows(
    positions: list[dict[str, Any]],
    activity_by_cid: dict[str, dict[str, Any]] | None = None,
) -> list[PositionRow]:
    """Build position rows; prefer activity BUY totals for cost basis.

    ``/positions.initialValue`` is ``shares * avgPrice`` rounded to the
    API's precision and can drift from true deployed capital by a cent
    or two per fill. When activity data is available, sum the BUY
    ``usdcSize`` rows (real on-chain dollars) for an exact match.
    """
    out: list[PositionRow] = []
    for p in positions:
        cid = (p.get("conditionId") or "").lower()
        if not cid:
            continue
        cost = _d(p.get("initialValue"))
        value = _d(p.get("currentValue"))
        pnl = _d(p.get("cashPnl"))
        if activity_by_cid is not None:
            rec = activity_by_cid.get(cid)
            if rec is not None and rec.get("cost", Decimal("0")) > 0:
                cost = rec["cost"]
                # Re-derive pnl against the activity-sourced cost so the
                # cost basis and P&L shown in the same row agree.
                pnl = value - cost
        out.append(
            PositionRow(
                condition_id=cid,
                title=str(p.get("title") or ""),
                outcome=str(p.get("outcome") or ""),
                shares=_d(p.get("size")),
                avg_price=_d(p.get("avgPrice")),
                cost_usdc=cost,
                cur_price=_d(p.get("curPrice")),
                value_usdc=value,
                pnl_usdc=pnl,
                redeemable=bool(p.get("redeemable")),
            )
        )
    # Keep "interesting" (nonzero value / open) at top.
    def _sort_key(r: PositionRow):
        return (r.value_usdc > 0, not r.redeemable, r.value_usdc)
    return sorted(out, key=_sort_key, reverse=True)


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description="Polymarket P&L report (cash + positions ground truth).")
    ap.add_argument("--wallet", required=True, help="Wallet/proxy address (0x...).")
    ap.add_argument("--starting-usdc", required=True, type=float, help="Starting funded USDC.e balance for net P&L.")
    ap.add_argument("--day-utc", default=datetime.now(UTC).strftime("%Y-%m-%d"), help="UTC day filter (YYYY-MM-DD) for activity/db cross-check.")
    ap.add_argument("--rpc-url", default=DEFAULT_RPC_URL, help="Polygon JSON-RPC URL.")
    ap.add_argument("--token", default=USDC_E_POLYGON, help="USDC.e token contract on Polygon.")
    ap.add_argument("--db-path", default="/data/btc_live_t10_trades.db", help="SQLite DB path (event_log cross-check).")
    args = ap.parse_args(argv)

    wallet = _parse_wallet(args.wallet)
    starting_usdc = _d(args.starting_usdc)
    day_utc = (args.day_utc or "").strip() or None

    async def _run() -> dict[str, Any]:
        usdc = await get_erc20_balance_usdc(args.rpc_url, args.token, wallet)
        positions = await fetch_positions(wallet)
        activity = await fetch_activity(wallet)
        activity_day = _filter_activity_day(activity, day_utc)
        return {"usdc": usdc, "positions": positions, "activity": activity_day}

    try:
        import asyncio
        data = asyncio.run(_run())
    except Exception as exc:
        print(f"ERROR: failed to fetch ground-truth data: {exc}", file=sys.stderr)
        return 2

    onchain_usdc: Decimal = data["usdc"]
    activity_by_cid = aggregate_activity_by_condition(data["activity"])
    position_rows = build_position_rows(data["positions"], activity_by_cid)
    closed_rows = build_closed_rows_from_activity(data["activity"])

    # Equity
    positions_value = sum((r.value_usdc for r in position_rows), Decimal("0"))
    total_equity = onchain_usdc + positions_value
    net_pnl = total_equity - starting_usdc
    net_pnl_pct = (net_pnl / starting_usdc) if starting_usdc > 0 else Decimal("0")

    # Gross deployed = BUY cost on every conditionId we bought. closed_rows
    # already covers anything with a REDEEM event (losers + redeemed winners).
    # Open positions (no REDEEM yet) contribute from position_rows. A market
    # can theoretically appear in both if a redeemed position still lingers
    # in /positions — guard against double-counting by conditionId.
    closed_cids = {r.condition_id for r in closed_rows}
    gross_deployed = sum(
        (r.cost_usdc for r in position_rows if r.condition_id not in closed_cids),
        Decimal("0"),
    ) + sum((r.cost_usdc for r in closed_rows), Decimal("0"))

    # Split open positions P&L buckets.
    realized_losses_unredeemed = sum(
        (r.pnl_usdc for r in position_rows if r.redeemable and r.value_usdc == 0),
        Decimal("0"),
    )
    unrealized_wins_unredeemed = sum(
        (r.pnl_usdc for r in position_rows if r.redeemable and r.value_usdc > 0),
        Decimal("0"),
    )
    unrealized_open = sum(
        (r.pnl_usdc for r in position_rows if not r.redeemable),
        Decimal("0"),
    )

    # Closed (redeemed) realized P&L from activity (payout - cost).
    realized_closed = sum((r.pnl_usdc for r in closed_rows), Decimal("0"))
    realized_total = realized_closed + realized_losses_unredeemed
    unrealized_total = unrealized_open + unrealized_wins_unredeemed

    # Resolved hit-rate (ground truth: /activity REDEEM rows, NOT /positions).
    # /positions drops resolved losers once their tokens are burned on
    # redemption, so counting losses from /positions alone misses them.
    # - wins: redeemed with payout > 0, plus unredeemed winners still in /positions
    # - losses: redeemed with payout == 0, plus unredeemed losses still in /positions
    wins = sum(1 for r in closed_rows if r.payout_usdc > 0)
    wins += sum(1 for r in position_rows if r.redeemable and r.value_usdc > 0)
    losses = sum(1 for r in closed_rows if r.payout_usdc == 0 and r.cost_usdc > 0)
    losses += sum(1 for r in position_rows if r.redeemable and r.value_usdc == 0)
    resolved_total = wins + losses
    hit_rate = (Decimal(wins) / Decimal(resolved_total)) if resolved_total else Decimal("0")

    # DB cross-check (best-effort; can be missing in local dev).
    db_summary = read_event_log_summary(args.db_path, day_utc)

    now_utc = datetime.now(UTC).strftime("%Y-%m-%d %H:%M UTC")

    # Tables
    pos_headers = ["Status", "Market", "Side", "Shares", "AvgPx", "Cost", "CurPx", "Value", "P&L"]
    pos_rows: list[list[str]] = []
    for r in position_rows:
        title = r.title if len(r.title) <= 48 else (r.title[:45] + "…")
        pos_rows.append(
            [
                r.status,
                title,
                r.outcome,
                f"{r.shares:.4f}".rstrip("0").rstrip("."),
                f"{r.avg_price:.3f}",
                _money0(r.cost_usdc),
                f"{r.cur_price:.3f}",
                _money0(r.value_usdc),
                _money(r.pnl_usdc),
            ]
        )

    closed_headers = ["Closed", "Market", "Cost", "Payout", "P&L"]
    closed_rows_out: list[list[str]] = []
    for r in closed_rows:
        title = r.title if len(r.title) <= 54 else (r.title[:51] + "…")
        closed_rows_out.append(
            [
                "REDEEMED",
                title,
                _money0(r.cost_usdc),
                _money0(r.payout_usdc),
                _money(r.pnl_usdc),
            ]
        )

    lines: list[str] = []
    lines.append(f"=== P&L Report ({now_utc}) ===")
    lines.append(f"Wallet: {wallet} ({_short_addr(wallet)})")
    lines.append(f"Starting USDC.e (input): {_money0(starting_usdc)}")
    lines.append("")
    lines.append("MATH (ground truth):")
    lines.append(f"  Total equity = On-chain USDC.e + Σ(position currentValue)")
    lines.append(f"              = {_money0(onchain_usdc)} + {_money0(positions_value)}")
    lines.append(f"              = {_money0(total_equity)}")
    lines.append(f"  Net P&L      = Total equity - Starting")
    lines.append(f"              = {_money0(total_equity)} - {_money0(starting_usdc)}")
    lines.append(f"              = {_money(net_pnl)} ({_pct(net_pnl_pct)})")
    lines.append("")
    lines.append("DEPLOYED:")
    lines.append(f"  Gross capital deployed (buys) = Σ(cost basis open positions) + Σ(cost redeemed)")
    lines.append(f"                              = {_money0(gross_deployed)}")
    lines.append("")
    lines.append("RESOLVED (from Polymarket Data API + activity):")
    lines.append(f"  {wins} wins, {losses} losses ({_pct(hit_rate)} hit rate)")
    lines.append(f"  Realized (redeemed) P&L:      {_money(realized_closed)}")
    lines.append(f"  Realized losses (0-value):    {_money(realized_losses_unredeemed)}")
    lines.append(f"  Unrealized wins (unredeemed): {_money(unrealized_wins_unredeemed)}")
    lines.append(f"  Unrealized (open/unresolved): {_money(unrealized_open)}")
    lines.append(f"  Check: realized+unrealized =  {_money(realized_total + unrealized_total)} (should track net P&L)")
    lines.append("")
    lines.append("EQUITY:")
    lines.append(f"  On-chain USDC.e: {_money0(onchain_usdc)}")
    lines.append(f"  Positions value: {_money0(positions_value)}")
    lines.append(f"  Total equity:    {_money0(total_equity)}")
    lines.append("")
    lines.append("POSITIONS (ground truth: /positions):")
    lines.append(_format_table(pos_headers, pos_rows) if pos_rows else "(none)")
    lines.append("")
    lines.append("CLOSED (ground truth: /activity REDEEM vs BUY cost):")
    lines.append(_format_table(closed_headers, closed_rows_out) if closed_rows_out else "(none)")

    lines.append("")
    lines.append("EVENT_LOG CROSS-CHECK (non-authoritative):")
    if not db_summary.get("ok"):
        lines.append(f"  db_path={args.db_path} -> {db_summary.get('error')}")
    else:
        lines.append(f"  db_path={args.db_path}")
        lines.append(f"  entries: {db_summary['entries']} (missing order_id: {db_summary['entries_missing_order_id']})")
        lines.append(f"  resolutions: {db_summary['resolutions']} (pnl_usd sum: {_money(_d(db_summary['pnl_usd_sum']))})")
        if db_summary["entries_missing_order_id"] > 0:
            lines.append("  NOTE: entry rows missing order_id are likely phantom (pre-fill logging).")

    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

