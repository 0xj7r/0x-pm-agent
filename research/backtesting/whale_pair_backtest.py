"""Historical backtest for the whale-family two-sided pair/merge strategy."""
from __future__ import annotations

import argparse
import json
from dataclasses import asdict, dataclass
from datetime import datetime
from pathlib import Path
import sys
from collections import defaultdict

ROOT = Path(__file__).resolve().parent.parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from shared.fees import taker_fee_usd
from shared.db import get_connection
from strategies.whale_pair import (
    BookTop,
    FillDecision,
    PairFillDecision,
    WhalePairConfig,
    WhalePairMarketState,
    apply_fill,
    match_pairs,
    maybe_decide_pair_fill,
    maybe_decide_fill,
    resolve_residual_pnl,
)


@dataclass(frozen=True)
class BacktestMarketResult:
    market_id: str
    slug: str
    start_time: str
    winner: str
    fills: int
    matches: int
    gross_cost_usd: float
    merged_pnl_usd: float
    residual_pnl_usd: float
    total_pnl_usd: float
    unresolved_up_shares: float
    unresolved_down_shares: float
    sim_up_buy_usdc: float = 0.0
    sim_down_buy_usdc: float = 0.0
    sim_first_offset_sec: int | None = None
    sim_last_offset_sec: int | None = None
    whale_buy_rows: int = 0
    whale_merge_rows: int = 0
    whale_redeem_rows: int = 0
    whale_up_buy_usdc: float = 0.0
    whale_down_buy_usdc: float = 0.0
    whale_first_offset_sec: int | None = None
    whale_last_offset_sec: int | None = None
    attempted_orders: int = 0
    missed_orders: int = 0
    partial_orders: int = 0
    execution_slippage_usd: float = 0.0


@dataclass(frozen=True)
class ExecutionModel:
    latency_snapshots: int = 0
    fill_fraction: float = 1.0


def _book_top(snapshot: dict, side: str) -> BookTop | None:
    suffix = "up" if side == "Up" else "down"
    ask = snapshot.get(f"best_ask_{suffix}")
    if ask is None:
        return None
    return BookTop(
        ask=float(ask),
        ask_size=float(snapshot.get(f"ask_size_{suffix}") or 0.0),
    )


def _execution_snapshot(
    snapshots: list[dict],
    snap_index: int,
    execution: ExecutionModel,
) -> dict | None:
    exec_index = snap_index + max(0, int(execution.latency_snapshots))
    if exec_index >= len(snapshots):
        return None
    return snapshots[exec_index]


def _execution_slippage(fill: FillDecision, executed: FillDecision) -> float:
    expected_gross = executed.shares * float(fill.price)
    expected_fee = taker_fee_usd(float(fill.price), expected_gross)
    return (executed.gross_cost_usd + executed.fee_usd) - (expected_gross + expected_fee)


def _realize_fill(
    fill: FillDecision,
    exec_top: BookTop | None,
    execution: ExecutionModel,
) -> FillDecision | None:
    if exec_top is None or exec_top.ask <= 0:
        return None
    available_shares = max(0.0, float(exec_top.ask_size) * float(execution.fill_fraction))
    realized_shares = min(float(fill.shares), available_shares)
    if realized_shares <= 0:
        return None
    gross_cost = realized_shares * float(exec_top.ask)
    return FillDecision(
        side=fill.side,
        reason=fill.reason,
        price=float(exec_top.ask),
        ask_size=float(exec_top.ask_size),
        shares=realized_shares,
        gross_cost_usd=gross_cost,
        fee_usd=taker_fee_usd(float(exec_top.ask), gross_cost),
    )


def _realize_pair_fill(
    pair_fill: PairFillDecision,
    exec_up: BookTop | None,
    exec_down: BookTop | None,
    execution: ExecutionModel,
) -> PairFillDecision | None:
    if exec_up is None or exec_down is None or exec_up.ask <= 0 or exec_down.ask <= 0:
        return None
    available_up = max(0.0, float(exec_up.ask_size) * float(execution.fill_fraction))
    available_down = max(0.0, float(exec_down.ask_size) * float(execution.fill_fraction))
    realized_shares = min(
        float(pair_fill.up_fill.shares),
        float(pair_fill.down_fill.shares),
        available_up,
        available_down,
    )
    if realized_shares <= 0:
        return None
    up_gross = realized_shares * float(exec_up.ask)
    down_gross = realized_shares * float(exec_down.ask)
    return PairFillDecision(
        up_fill=FillDecision(
            side="Up",
            reason=pair_fill.up_fill.reason,
            price=float(exec_up.ask),
            ask_size=float(exec_up.ask_size),
            shares=realized_shares,
            gross_cost_usd=up_gross,
            fee_usd=taker_fee_usd(float(exec_up.ask), up_gross),
        ),
        down_fill=FillDecision(
            side="Down",
            reason=pair_fill.down_fill.reason,
            price=float(exec_down.ask),
            ask_size=float(exec_down.ask_size),
            shares=realized_shares,
            gross_cost_usd=down_gross,
            fee_usd=taker_fee_usd(float(exec_down.ask), down_gross),
        ),
    )


def _parse_iso(value: str) -> datetime:
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def summarize_whale_activity(activity_rows: list[dict]) -> dict[str, dict]:
    by_slug: dict[str, dict] = defaultdict(
        lambda: {
            "whale_buy_rows": 0,
            "whale_merge_rows": 0,
            "whale_redeem_rows": 0,
            "whale_up_buy_usdc": 0.0,
            "whale_down_buy_usdc": 0.0,
            "_buy_timestamps": [],
        }
    )
    for row in activity_rows:
        slug = row.get("slug") or ""
        if not slug:
            continue
        rec = by_slug[slug]
        rtype = row.get("type")
        if rtype == "TRADE" and row.get("side") == "BUY":
            rec["whale_buy_rows"] += 1
            rec["_buy_timestamps"].append(int(row.get("timestamp") or 0))
            usdc = float(row.get("usdcSize") or 0.0)
            if row.get("outcome") == "Up":
                rec["whale_up_buy_usdc"] += usdc
            elif row.get("outcome") == "Down":
                rec["whale_down_buy_usdc"] += usdc
        elif rtype == "MERGE":
            rec["whale_merge_rows"] += 1
        elif rtype == "REDEEM":
            rec["whale_redeem_rows"] += 1

    for slug, rec in by_slug.items():
        try:
            start_ts = int(slug.rsplit("-", 1)[-1])
        except ValueError:
            start_ts = None
        timestamps = [ts for ts in rec.pop("_buy_timestamps", []) if ts]
        if timestamps and start_ts is not None:
            rec["whale_first_offset_sec"] = min(timestamps) - start_ts
            rec["whale_last_offset_sec"] = max(timestamps) - start_ts
        else:
            rec["whale_first_offset_sec"] = None
            rec["whale_last_offset_sec"] = None
    return dict(by_slug)


def _imbalance_ratio(up_usdc: float, down_usdc: float) -> float | None:
    up = max(0.0, float(up_usdc))
    down = max(0.0, float(down_usdc))
    smaller = min(up, down)
    larger = max(up, down)
    if smaller <= 0.0:
        return None
    return larger / smaller


<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
def _heavier_side(up_usdc: float, down_usdc: float) -> str | None:
    up = max(0.0, float(up_usdc))
    down = max(0.0, float(down_usdc))
    if up <= 0.0 and down <= 0.0:
        return None
    if up > down:
        return "Up"
    if down > up:
        return "Down"
    return "Even"


=======
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py
def _market_comparison_row(r: "BacktestMarketResult") -> dict:
    sim_total = float(r.sim_up_buy_usdc) + float(r.sim_down_buy_usdc)
    whale_total = float(r.whale_up_buy_usdc) + float(r.whale_down_buy_usdc)
    return {
        "slug": r.slug,
        "market_id": r.market_id,
        "start_time": r.start_time,
        "winner": r.winner,
        "sim": {
            "first_buy_offset_sec": r.sim_first_offset_sec,
            "last_buy_offset_sec": r.sim_last_offset_sec,
            "fill_count": int(r.fills),
            "buy_usdc_up": float(r.sim_up_buy_usdc),
            "buy_usdc_down": float(r.sim_down_buy_usdc),
            "buy_usdc_total": sim_total,
            "imbalance_ratio": _imbalance_ratio(r.sim_up_buy_usdc, r.sim_down_buy_usdc),
<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
            "heavier_side": _heavier_side(r.sim_up_buy_usdc, r.sim_down_buy_usdc),
=======
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py
        },
        "whale": {
            "first_buy_offset_sec": r.whale_first_offset_sec,
            "last_buy_offset_sec": r.whale_last_offset_sec,
            "fill_count": int(r.whale_buy_rows),
            "buy_usdc_up": float(r.whale_up_buy_usdc),
            "buy_usdc_down": float(r.whale_down_buy_usdc),
            "buy_usdc_total": whale_total,
            "imbalance_ratio": _imbalance_ratio(r.whale_up_buy_usdc, r.whale_down_buy_usdc),
<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
            "heavier_side": _heavier_side(r.whale_up_buy_usdc, r.whale_down_buy_usdc),
=======
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py
        },
    }


def _mean_or_none(values: list[float]) -> float | None:
    values = [v for v in values if v is not None]
    if not values:
        return None
    return sum(values) / len(values)


def _median_or_none(values: list[float]) -> float | None:
    values = sorted(v for v in values if v is not None)
    if not values:
        return None
    mid = len(values) // 2
    if len(values) % 2 == 1:
        return float(values[mid])
    return (values[mid - 1] + values[mid]) / 2.0


def build_w1_comparison(overlap_results: list["BacktestMarketResult"]) -> dict:
    """Per-market and aggregate shape comparison between sim and w1.

    w1 is a shape benchmark only: this produces diffs (sim minus whale) on
    first/last buy offsets, fill count, notional by side, and imbalance
    ratio. Nothing here is suitable for picking configs.
    """

    rows = [_market_comparison_row(r) for r in overlap_results]

    def diffs(attr: str) -> list[float]:
        out: list[float] = []
        for row in rows:
            s = row["sim"].get(attr)
            w = row["whale"].get(attr)
            if s is None or w is None:
                continue
            out.append(float(s) - float(w))
        return out

    first_offset_diffs = diffs("first_buy_offset_sec")
    last_offset_diffs = diffs("last_buy_offset_sec")
    fill_count_diffs = diffs("fill_count")
    total_usdc_diffs = diffs("buy_usdc_total")
    up_usdc_diffs = diffs("buy_usdc_up")
    down_usdc_diffs = diffs("buy_usdc_down")
    imbalance_diffs = diffs("imbalance_ratio")

    overlap_markets = [r for r in rows if r["sim"]["fill_count"] > 0 and r["whale"]["fill_count"] > 0]

<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
    side_tilt_considered = 0
    side_tilt_agree = 0
    for row in rows:
        sim_side = row["sim"].get("heavier_side")
        whale_side = row["whale"].get("heavier_side")
        if sim_side in (None, "Even") or whale_side in (None, "Even"):
            continue
        side_tilt_considered += 1
        if sim_side == whale_side:
            side_tilt_agree += 1
    side_tilt_agreement_rate = (
        (side_tilt_agree / side_tilt_considered) if side_tilt_considered else None
    )

    both_active_rate = (
        (len(overlap_markets) / len(rows)) if rows else None
    )

    return {
        "overlap_markets": len(rows),
        "overlap_markets_both_active": len(overlap_markets),
        "both_active_rate": both_active_rate,
=======
    return {
        "overlap_markets": len(rows),
        "overlap_markets_both_active": len(overlap_markets),
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py
        "aggregates": {
            "first_buy_offset_sec_mean_diff": _mean_or_none(first_offset_diffs),
            "first_buy_offset_sec_median_diff": _median_or_none(first_offset_diffs),
            "last_buy_offset_sec_mean_diff": _mean_or_none(last_offset_diffs),
            "last_buy_offset_sec_median_diff": _median_or_none(last_offset_diffs),
            "fill_count_mean_diff": _mean_or_none(fill_count_diffs),
            "fill_count_median_diff": _median_or_none(fill_count_diffs),
            "buy_usdc_total_mean_diff": _mean_or_none(total_usdc_diffs),
            "buy_usdc_total_median_diff": _median_or_none(total_usdc_diffs),
            "buy_usdc_up_mean_diff": _mean_or_none(up_usdc_diffs),
            "buy_usdc_down_mean_diff": _mean_or_none(down_usdc_diffs),
            "imbalance_ratio_mean_diff": _mean_or_none(imbalance_diffs),
            "imbalance_ratio_median_diff": _median_or_none(imbalance_diffs),
<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
            "side_tilt_agreement_rate": side_tilt_agreement_rate,
            "side_tilt_markets_considered": side_tilt_considered,
=======
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py
        },
        "per_market": rows,
    }


def simulate_market(
    market: dict,
    snapshots: list[dict],
    cfg: WhalePairConfig,
    whale_summary: dict | None = None,
    execution: ExecutionModel | None = None,
) -> BacktestMarketResult:
    execution = execution or ExecutionModel()
    state = WhalePairMarketState()
    start_time = _parse_iso(market["start_time"])
    sim_offsets: list[int] = []
    sim_up_buy_usdc = 0.0
    sim_down_buy_usdc = 0.0
    attempted_orders = 0
    missed_orders = 0
    partial_orders = 0
    execution_slippage_usd = 0.0
<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
    allow_single_leg_accumulate = cfg.variant in (
        "skewed_pair_builder",
        "passive_ladder",
        "w1_mimic",
    )
=======
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py
    for snap_index, snap in enumerate(snapshots):
        best_ask_up = snap.get("best_ask_up")
        best_ask_down = snap.get("best_ask_down")
        if best_ask_up is None or best_ask_down is None:
            continue
        snap_time = _parse_iso(snap["time"])
        seconds_from_start = int((snap_time - start_time).total_seconds())
        if not (cfg.min_seconds_from_start <= seconds_from_start <= cfg.max_seconds_from_start):
            continue

        sides = [
            ("Up", BookTop(ask=float(best_ask_up), ask_size=float(snap.get("ask_size_up") or 0.0))),
            ("Down", BookTop(ask=float(best_ask_down), ask_size=float(snap.get("ask_size_down") or 0.0))),
        ]
        up_top = sides[0][1]
        down_top = sides[1][1]

        pair_fill = maybe_decide_pair_fill(
            up_top=up_top,
            down_top=down_top,
            state=state,
            cfg=cfg,
        )
<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
        pair_applied = False
=======
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py
        if pair_fill is not None:
            attempted_orders += 2
            exec_snap = _execution_snapshot(snapshots, snap_index, execution)
            realized_pair = _realize_pair_fill(
                pair_fill,
                _book_top(exec_snap, "Up") if exec_snap else None,
                _book_top(exec_snap, "Down") if exec_snap else None,
                execution,
            )
            if realized_pair is None:
                missed_orders += 2
            else:
                if realized_pair.up_fill.shares < pair_fill.up_fill.shares:
                    partial_orders += 2
                execution_slippage_usd += _execution_slippage(
                    pair_fill.up_fill,
                    realized_pair.up_fill,
                )
                execution_slippage_usd += _execution_slippage(
                    pair_fill.down_fill,
                    realized_pair.down_fill,
                )
                for fill in (realized_pair.up_fill, realized_pair.down_fill):
                    apply_fill(state, fill)
                    sim_offsets.append(seconds_from_start)
                    if fill.side == "Up":
                        sim_up_buy_usdc += fill.gross_cost_usd
                    else:
                        sim_down_buy_usdc += fill.gross_cost_usd
                match_pairs(state)
<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
                pair_applied = True

        if pair_applied:
            continue
=======
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py

        sides.sort(key=lambda item: item[1].ask)
        exec_snap = _execution_snapshot(snapshots, snap_index, execution)
        for side, top in sides:
            fill = maybe_decide_fill(
                side=side,
                top=top,
                state=state,
                cfg=cfg,
<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
                allow_accumulate=allow_single_leg_accumulate,
=======
                allow_accumulate=False,
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py
            )
            if fill is not None:
                attempted_orders += 1
                realized_fill = _realize_fill(
                    fill,
                    _book_top(exec_snap, side) if exec_snap else None,
                    execution,
                )
                if realized_fill is None:
                    missed_orders += 1
                    continue
                if realized_fill.shares < fill.shares:
                    partial_orders += 1
                execution_slippage_usd += _execution_slippage(fill, realized_fill)
                apply_fill(state, realized_fill)
                sim_offsets.append(seconds_from_start)
                if realized_fill.side == "Up":
                    sim_up_buy_usdc += realized_fill.gross_cost_usd
                else:
                    sim_down_buy_usdc += realized_fill.gross_cost_usd
                match_pairs(state)

    merged_pnl = sum(m.realized_pnl_usd for m in state.matches)
    residual_pnl = resolve_residual_pnl(state, market["winner"])
    up_residual = state.open_inventory("Up").shares_remaining
    down_residual = state.open_inventory("Down").shares_remaining
    whale = whale_summary or {}
    return BacktestMarketResult(
        market_id=market["market_id"],
        slug=market["slug"],
        start_time=market["start_time"],
        winner=market["winner"],
        fills=len(state.fills),
        matches=len(state.matches),
        gross_cost_usd=state.gross_cost_usd,
        merged_pnl_usd=merged_pnl,
        residual_pnl_usd=residual_pnl,
        total_pnl_usd=merged_pnl + residual_pnl,
        unresolved_up_shares=up_residual,
        unresolved_down_shares=down_residual,
        sim_up_buy_usdc=sim_up_buy_usdc,
        sim_down_buy_usdc=sim_down_buy_usdc,
        sim_first_offset_sec=min(sim_offsets) if sim_offsets else None,
        sim_last_offset_sec=max(sim_offsets) if sim_offsets else None,
        whale_buy_rows=int(whale.get("whale_buy_rows", 0)),
        whale_merge_rows=int(whale.get("whale_merge_rows", 0)),
        whale_redeem_rows=int(whale.get("whale_redeem_rows", 0)),
        whale_up_buy_usdc=float(whale.get("whale_up_buy_usdc", 0.0)),
        whale_down_buy_usdc=float(whale.get("whale_down_buy_usdc", 0.0)),
        whale_first_offset_sec=whale.get("whale_first_offset_sec"),
        whale_last_offset_sec=whale.get("whale_last_offset_sec"),
        attempted_orders=attempted_orders,
        missed_orders=missed_orders,
        partial_orders=partial_orders,
        execution_slippage_usd=execution_slippage_usd,
    )


def load_markets(
    conn,
    *,
    limit: int | None = None,
    market_type: str = "5m",
    include_slugs: set[str] | None = None,
) -> list[dict]:
    if limit is not None and limit > 0 and not include_slugs:
        rows = conn.execute(
            """
            SELECT market_id, slug, start_time, end_time, price_start, price_end, winner
            FROM markets
            WHERE winner IS NOT NULL AND market_type = ?
            ORDER BY start_time DESC
            LIMIT ?
            """,
            (market_type, limit),
        ).fetchall()
        rows = list(reversed(rows))
        return [dict(row) for row in rows]

    sql = (
        "SELECT market_id, slug, start_time, end_time, price_start, price_end, winner "
        "FROM markets "
        "WHERE winner IS NOT NULL AND market_type = ? "
        "AND EXISTS (SELECT 1 FROM snapshots s WHERE s.market_id = markets.market_id LIMIT 1) "
    )
    params: list = [market_type]
    if include_slugs:
        placeholders = ",".join("?" for _ in include_slugs)
        sql += f"AND slug IN ({placeholders}) "
        params.extend(sorted(include_slugs))
    if limit is not None and limit > 0:
        sql += "ORDER BY start_time DESC LIMIT ?"
        params.append(limit)
        rows = conn.execute(sql, tuple(params)).fetchall()
        rows = list(reversed(rows))
        return [dict(row) for row in rows]
    sql += "ORDER BY start_time"
    rows = conn.execute(sql, tuple(params)).fetchall()
    return [dict(row) for row in rows]


def load_snapshots(conn, market_id: str) -> list[dict]:
    rows = conn.execute(
        """
        SELECT time, price, price_up, price_down,
               best_bid_up, best_ask_up, bid_size_up, ask_size_up,
               best_bid_down, best_ask_down, bid_size_down, ask_size_down
        FROM snapshots
        WHERE market_id = ?
        ORDER BY time
        """,
        (market_id,),
    ).fetchall()
    return [dict(row) for row in rows]


def run_backtest(
    db_path: Path,
    cfg: WhalePairConfig,
    *,
<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
    market_type: str = "5m",
=======
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py
    limit: int | None = None,
    whale_activity_path: Path | None = None,
    execution: ExecutionModel | None = None,
) -> dict:
    conn = get_connection(db_path)
    whale_by_slug: dict[str, dict] = {}
    if whale_activity_path is not None:
        whale_rows = json.loads(whale_activity_path.read_text())
        whale_by_slug = summarize_whale_activity(whale_rows)
    try:
        markets = load_markets(
            conn,
            limit=limit,
<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
            market_type=market_type,
=======
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py
            include_slugs=set(whale_by_slug.keys()) if whale_by_slug else None,
        )
        results: list[BacktestMarketResult] = []
        for market in markets:
            snapshots = load_snapshots(conn, market["market_id"])
            if not snapshots:
                continue
            results.append(
                simulate_market(
                    market,
                    snapshots,
                    cfg,
                    whale_summary=whale_by_slug.get(market["slug"]),
                    execution=execution,
                )
            )
    finally:
        conn.close()

    total_pnl = sum(r.total_pnl_usd for r in results)
    merged_pnl = sum(r.merged_pnl_usd for r in results)
    residual_pnl = sum(r.residual_pnl_usd for r in results)
    gross_cost = sum(r.gross_cost_usd for r in results)
    active = [r for r in results if r.fills > 0]
    winners = [r for r in active if r.total_pnl_usd > 0]
    losers = [r for r in active if r.total_pnl_usd < 0]
    report = {
        "db_path": str(db_path),
<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
        "market_type": market_type,
        "config": asdict(cfg),
=======
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py
        "execution": asdict(execution or ExecutionModel()),
        "markets_considered": len(results),
        "markets_traded": len(active),
        "wins": len(winners),
        "losses": len(losers),
        "win_rate": (len(winners) / len(active)) if active else 0.0,
        "gross_cost_usd": gross_cost,
        "merged_pnl_usd": merged_pnl,
        "residual_pnl_usd": residual_pnl,
        "total_pnl_usd": total_pnl,
        "avg_pnl_per_traded_market": (total_pnl / len(active)) if active else 0.0,
        "attempted_orders": sum(r.attempted_orders for r in results),
        "missed_orders": sum(r.missed_orders for r in results),
        "partial_orders": sum(r.partial_orders for r in results),
        "execution_slippage_usd": sum(r.execution_slippage_usd for r in results),
        "results": [asdict(r) for r in results],
    }
    if whale_by_slug:
        whale_overlap = [r for r in results if r.whale_buy_rows or r.whale_merge_rows or r.whale_redeem_rows]
        report["w1_comparison"] = build_w1_comparison(whale_overlap)
        report["whale_alignment"] = {
            "activity_file": str(whale_activity_path),
            "whale_markets_in_activity": len(whale_by_slug),
            "matched_markets_in_backtest_db": len(whale_overlap),
            "sim_markets_with_fills": sum(1 for r in whale_overlap if r.fills > 0),
            "avg_whale_buy_rows": (
                sum(r.whale_buy_rows for r in whale_overlap) / len(whale_overlap)
                if whale_overlap else 0.0
            ),
            "avg_sim_buy_usdc": (
                sum(r.sim_up_buy_usdc + r.sim_down_buy_usdc for r in whale_overlap) / len(whale_overlap)
                if whale_overlap else 0.0
            ),
            "avg_whale_buy_usdc": (
                sum(r.whale_up_buy_usdc + r.whale_down_buy_usdc for r in whale_overlap) / len(whale_overlap)
                if whale_overlap else 0.0
            ),
            "avg_sim_fills": (
                sum(r.fills for r in whale_overlap) / len(whale_overlap)
                if whale_overlap else 0.0
            ),
            "avg_whale_merge_rows": (
                sum(r.whale_merge_rows for r in whale_overlap) / len(whale_overlap)
                if whale_overlap else 0.0
            ),
            "avg_sim_matches": (
                sum(r.matches for r in whale_overlap) / len(whale_overlap)
                if whale_overlap else 0.0
            ),
            "avg_sim_first_offset_sec": (
                sum(r.sim_first_offset_sec for r in whale_overlap if r.sim_first_offset_sec is not None)
                / max(1, sum(1 for r in whale_overlap if r.sim_first_offset_sec is not None))
            ),
            "avg_whale_first_offset_sec": (
                sum(r.whale_first_offset_sec for r in whale_overlap if r.whale_first_offset_sec is not None)
                / max(1, sum(1 for r in whale_overlap if r.whale_first_offset_sec is not None))
            ),
            "avg_sim_last_offset_sec": (
                sum(r.sim_last_offset_sec for r in whale_overlap if r.sim_last_offset_sec is not None)
                / max(1, sum(1 for r in whale_overlap if r.sim_last_offset_sec is not None))
            ),
            "avg_whale_last_offset_sec": (
                sum(r.whale_last_offset_sec for r in whale_overlap if r.whale_last_offset_sec is not None)
                / max(1, sum(1 for r in whale_overlap if r.whale_last_offset_sec is not None))
            ),
        }
    return report


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", default="backtesting/btc.db")
    ap.add_argument("--limit", type=int, default=0)
<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
    ap.add_argument(
        "--variant",
        choices=("pair_recycler", "skewed_pair_builder", "passive_ladder", "w1_mimic"),
        default="pair_recycler",
    )
=======
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py
    ap.add_argument("--accumulate-price-max", type=float, default=0.50)
    ap.add_argument("--aggressive-price-max", type=float, default=0.10)
    ap.add_argument("--max-pair-cost", type=float, default=0.99)
    ap.add_argument("--base-clip-usd", type=float, default=10.0)
    ap.add_argument("--aggressive-clip-usd", type=float, default=25.0)
<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
    ap.add_argument("--base-clip-shares", type=float, default=0.0)
    ap.add_argument("--aggressive-clip-shares", type=float, default=0.0)
=======
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py
    ap.add_argument("--max-gross-cost-usd", type=float, default=200.0)
    ap.add_argument("--min-seconds-from-start", type=int, default=10)
    ap.add_argument("--max-seconds-from-start", type=int, default=298)
    ap.add_argument("--completion-min-pnl-per-share", type=float, default=0.002)
    ap.add_argument("--max-imbalance-ratio", type=float, default=3.0)
    ap.add_argument("--latency-snapshots", type=int, default=0)
    ap.add_argument("--fill-fraction", type=float, default=1.0)
    ap.add_argument("--whale-activity", default="")
    ap.add_argument("--output", default="")
    args = ap.parse_args()

    cfg = WhalePairConfig(
<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
        variant=args.variant,
=======
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py
        accumulate_price_max=args.accumulate_price_max,
        aggressive_price_max=args.aggressive_price_max,
        max_pair_cost=args.max_pair_cost,
        base_clip_usd=args.base_clip_usd,
        aggressive_clip_usd=args.aggressive_clip_usd,
<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
        base_clip_shares=(args.base_clip_shares or None),
        aggressive_clip_shares=(args.aggressive_clip_shares or None),
=======
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py
        max_gross_cost_usd=args.max_gross_cost_usd,
        min_seconds_from_start=args.min_seconds_from_start,
        max_seconds_from_start=args.max_seconds_from_start,
        completion_min_pnl_per_share=args.completion_min_pnl_per_share,
        max_imbalance_ratio=args.max_imbalance_ratio,
    )
    report = run_backtest(
        Path(args.db),
        cfg,
        limit=(args.limit or None),
        whale_activity_path=Path(args.whale_activity) if args.whale_activity else None,
        execution=ExecutionModel(
            latency_snapshots=max(0, int(args.latency_snapshots)),
            fill_fraction=max(0.0, min(1.0, float(args.fill_fraction))),
        ),
    )
<<<<<<< HEAD:research/backtesting/whale_pair_backtest.py
    report["config"] = asdict(cfg)
=======
>>>>>>> feat/whale-pair-eval:backtesting/whale_pair_backtest.py
    text = json.dumps(report, indent=2)
    if args.output:
        Path(args.output).write_text(text)
    print(text)


if __name__ == "__main__":
    main()
