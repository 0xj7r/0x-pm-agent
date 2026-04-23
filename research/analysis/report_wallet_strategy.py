#!/usr/bin/env python3
"""Build a compact reverse-engineering report for a tracked Polymarket wallet.

This report is designed to answer:
  - what markets the wallet actually concentrates on
  - how it enters (timing, clip sizing, fragmentation, side balance)
  - what market conditions coincide with those entries
  - how it exits (merge/redeem/closed-position behavior)

Default focus is the dominant unlawful-shear regime: BTC 5m.

Usage:
  python3 scripts/report_wallet_strategy.py \
      --wallet 0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82
"""
from __future__ import annotations

import argparse
import json
from collections import Counter, defaultdict
from pathlib import Path
from statistics import mean, median
import sys
from typing import Any

ROOT = Path(__file__).resolve().parent.parent.parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from research.wallet_aliases import wallet_dir_name

WHALE_DIR = ROOT / "data" / "research" / "whale_analysis"
WALLET_DIR = ROOT / "data" / "research" / "wallet_research"
DEFAULT_WALLET = "0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82"
DEFAULT_FAMILY = "btc-updown-5m-"


def _load_json(path: Path) -> Any:
    return json.loads(path.read_text())


def _first_existing(paths: list[Path]) -> Path:
    for path in paths:
        if path.exists():
            return path
    raise FileNotFoundError(", ".join(str(path) for path in paths))


def _quantile(values: list[float], q: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    idx = max(0, min(len(ordered) - 1, round((len(ordered) - 1) * q)))
    return ordered[idx]


def _safe_mean(values: list[float]) -> float | None:
    return mean(values) if values else None


def _safe_median(values: list[float]) -> float | None:
    return median(values) if values else None


def _family_rows(rows: list[dict[str, Any]], family: str) -> list[dict[str, Any]]:
    return [row for row in rows if str(row.get("slug") or "").startswith(family)]


def _group_trade_rows(rows: list[dict[str, Any]]) -> dict[str, list[dict[str, Any]]]:
    grouped: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        if row.get("type") == "TRADE":
            grouped[str(row.get("slug") or "")].append(row)
    return dict(grouped)


def summarize_recent_activity(rows: list[dict[str, Any]]) -> dict[str, Any]:
    trade_rows = [row for row in rows if row.get("type") == "TRADE" and row.get("side") == "BUY"]
    grouped = _group_trade_rows(trade_rows)

    clip_usd = [float(row.get("usdcSize") or 0.0) for row in trade_rows]
    clip_shares = [float(row.get("size") or 0.0) for row in trade_rows]
    fills_per_market = [len(market_rows) for market_rows in grouped.values()]
    up_down_rows = []
    first_offsets = []
    last_offsets = []
    market_spans = []

    for slug, market_rows in grouped.items():
        try:
            start_ts = int(slug.rsplit("-", 1)[-1])
        except ValueError:
            start_ts = None
        by_outcome = Counter(str(row.get("outcome") or "") for row in market_rows)
        up_down_rows.append(
            {
                "slug": slug,
                "buy_rows": len(market_rows),
                "up_rows": by_outcome.get("Up", 0),
                "down_rows": by_outcome.get("Down", 0),
                "buy_usdc_total": sum(float(row.get("usdcSize") or 0.0) for row in market_rows),
            }
        )
        timestamps = sorted(int(row.get("timestamp") or 0) for row in market_rows if row.get("timestamp"))
        if timestamps:
            market_spans.append(float(timestamps[-1] - timestamps[0]))
            if start_ts is not None:
                first_offsets.append(float(timestamps[0] - start_ts))
                last_offsets.append(float(timestamps[-1] - start_ts))

    exact_100_share_clips = sum(1 for value in clip_shares if abs(value - 100.0) < 1e-6)
    return {
        "trade_buy_rows": len(trade_rows),
        "markets": len(grouped),
        "clip_usd": {
            "median": _safe_median(clip_usd),
            "p90": _quantile(clip_usd, 0.9),
            "mean": _safe_mean(clip_usd),
        },
        "clip_shares": {
            "median": _safe_median(clip_shares),
            "p90": _quantile(clip_shares, 0.9),
            "exact_100_share_clips": exact_100_share_clips,
        },
        "fragmentation": {
            "fills_per_market_median": _safe_median([float(v) for v in fills_per_market]),
            "fills_per_market_p90": _quantile([float(v) for v in fills_per_market], 0.9),
            "market_span_seconds_median": _safe_median(market_spans),
            "market_span_seconds_p90": _quantile(market_spans, 0.9),
        },
        "timing": {
            "first_buy_offset_sec_median": _safe_median(first_offsets),
            "first_buy_offset_sec_p90": _quantile(first_offsets, 0.9),
            "last_buy_offset_sec_median": _safe_median(last_offsets),
            "last_buy_offset_sec_p90": _quantile(last_offsets, 0.9),
        },
        "top_markets_by_rows": sorted(
            up_down_rows,
            key=lambda row: float(row["buy_rows"]),
            reverse=True,
        )[:10],
    }


def summarize_joined_market_conditions(rows: list[dict[str, Any]]) -> dict[str, Any]:
    trade_rows = [row for row in rows if row.get("type") == "TRADE" and row.get("side") == "BUY"]
    maker_rows = [row for row in trade_rows if row.get("execution_class") == "likely_maker_or_passive"]
    taker_rows = [row for row in trade_rows if row.get("execution_class") == "likely_taker"]

    ask_sums = [float(row["ask_sum"]) for row in trade_rows if row.get("ask_sum") is not None]
    fill_vs_ask_maker = [
        float(row["fill_vs_ask"])
        for row in maker_rows
        if row.get("fill_vs_ask") is not None
    ]
    fill_vs_ask_taker = [
        float(row["fill_vs_ask"])
        for row in taker_rows
        if row.get("fill_vs_ask") is not None
    ]
    touch_ratios = [
        float(row["touch_depth_ratio"])
        for row in trade_rows
        if row.get("touch_depth_ratio") is not None
    ]
    offsets = [
        float(row["seconds_from_start"])
        for row in trade_rows
        if row.get("seconds_from_start") is not None
    ]

    by_bucket = Counter(str(row.get("window_bucket") or "unknown") for row in trade_rows)
    by_execution = Counter(str(row.get("execution_class") or "unknown") for row in trade_rows)
    by_quality = Counter(str(row.get("fill_quality") or "unknown") for row in trade_rows)

    return {
        "trade_buy_rows": len(trade_rows),
        "execution_mix": dict(by_execution),
        "fill_quality": dict(by_quality),
        "timing": {
            "offset_sec_median": _safe_median(offsets),
            "offset_sec_p10": _quantile(offsets, 0.1),
            "offset_sec_p90": _quantile(offsets, 0.9),
            "bucket_counts": dict(by_bucket),
        },
        "market_conditions": {
            "ask_sum_mean": _safe_mean(ask_sums),
            "ask_sum_median": _safe_median(ask_sums),
            "ask_sum_p10": _quantile(ask_sums, 0.1),
            "ask_sum_lt_one_count": sum(1 for value in ask_sums if value < 1.0),
            "ask_sum_lt_one_share": (
                sum(1 for value in ask_sums if value < 1.0) / len(ask_sums)
                if ask_sums
                else None
            ),
        },
        "execution_edge": {
            "maker_fill_vs_ask_mean": _safe_mean(fill_vs_ask_maker),
            "maker_fill_vs_ask_median": _safe_median(fill_vs_ask_maker),
            "taker_fill_vs_ask_mean": _safe_mean(fill_vs_ask_taker),
            "taker_fill_vs_ask_median": _safe_median(fill_vs_ask_taker),
            "touch_depth_ratio_median": _safe_median(touch_ratios),
            "touch_depth_ratio_p90": _quantile(touch_ratios, 0.9),
        },
    }


def summarize_closed_positions(rows: list[dict[str, Any]]) -> dict[str, Any]:
    by_slug: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        by_slug[str(row.get("slug") or "")].append(row)

    per_market = []
    for slug, slug_rows in by_slug.items():
        realized = sum(float(row.get("realizedPnl") or 0.0) for row in slug_rows)
        bought = sum(float(row.get("totalBought") or 0.0) for row in slug_rows)
        outcomes = {str(row.get("outcome") or "") for row in slug_rows}
        per_market.append(
            {
                "slug": slug,
                "legs": len(slug_rows),
                "paired": {"Up", "Down"}.issubset(outcomes),
                "realized_pnl": realized,
                "total_bought": bought,
            }
        )

    realized_values = [row["realized_pnl"] for row in per_market]
    bought_values = [row["total_bought"] for row in per_market]
    profitable = [row for row in per_market if row["realized_pnl"] > 0]

    return {
        "markets": len(per_market),
        "paired_market_count": sum(1 for row in per_market if row["paired"]),
        "realized_pnl": {
            "total": sum(realized_values),
            "median": _safe_median(realized_values),
            "p10": _quantile(realized_values, 0.1),
            "p90": _quantile(realized_values, 0.9),
            "win_rate": len(profitable) / len(per_market) if per_market else None,
        },
        "capital_per_closed_market": {
            "median_total_bought": _safe_median(bought_values),
            "p90_total_bought": _quantile(bought_values, 0.9),
        },
        "top_winners": sorted(per_market, key=lambda row: row["realized_pnl"], reverse=True)[:5],
        "top_losers": sorted(per_market, key=lambda row: row["realized_pnl"])[:5],
    }


def build_takeaways(
    *,
    historical_summary: dict[str, Any],
    phase_summary: dict[str, Any],
    recent_activity: dict[str, Any],
    recent_joined: dict[str, Any],
    closed_positions: dict[str, Any],
) -> list[str]:
    periods = phase_summary.get("periods") or []
    latest_period = periods[-1] if periods else {}
    early_period = periods[0] if periods else {}
    takeaways = [
        (
            "The wallet is a high-frequency two-sided recycler, not a one-shot directional sniper: "
            f"{historical_summary['activity']['two_sided_market_count']} of "
            f"{historical_summary['activity']['market_count']} historical markets are two-sided, and "
            f"{historical_summary['activity']['merge_market_count']} show merge activity."
        ),
        (
            "The regime has clearly specialized into BTC 5m. Early history was broader "
            f"({early_period.get('top_assets')}) but the latest phase is fully BTC with "
            f"{latest_period.get('top_families')}."
        ),
        (
            "Entry timing is concentrated in the middle of the 5m window rather than purely at the open or final seconds: "
            f"median first buy offset is {recent_activity['timing']['first_buy_offset_sec_median']:.0f}s and "
            f"median last buy offset is {recent_activity['timing']['last_buy_offset_sec_median']:.0f}s."
        ),
        (
            "Execution is mixed. Recent joined rows are roughly "
            f"{recent_joined['execution_mix'].get('likely_maker_or_passive', 0)} passive vs "
            f"{recent_joined['execution_mix'].get('likely_taker', 0)} taker-classified buys, "
            "which implies active completion rather than passive-only posting."
        ),
        (
            "Visible negative-risk windows are rare, so the edge is not just naive ask-sum<1 scanning: "
            f"{recent_joined['market_conditions']['ask_sum_lt_one_count']} of "
            f"{recent_joined['trade_buy_rows']} recent buy rows had ask_sum<1."
        ),
        (
            "Sizing is fragmented but material. Recent BTC 5m child clips have median "
            f"${recent_activity['clip_usd']['median']:.2f} notional and p90 "
            f"${recent_activity['clip_usd']['p90']:.2f}, with "
            f"{recent_activity['clip_shares']['exact_100_share_clips']} exact-100-share clips."
        ),
        (
            "Closed-position behavior is consistent with paired inventory management: "
            f"{closed_positions['paired_market_count']} of {closed_positions['markets']} sampled closed markets "
            "contain both legs."
        ),
    ]
    return takeaways


def build_report(wallet: str, family: str) -> dict[str, Any]:
    wallet_key = wallet_dir_name(wallet)
    whale_dir = WHALE_DIR / wallet_key
    wallet_dir = WALLET_DIR / wallet_key

    history_root = _first_existing(
        [
            wallet_dir / "history" / "historical",
            wallet_dir / "history" / "current",
        ]
    )
    historical_summary_path = _first_existing([history_root / "summary.json"])
    phase_summary_path = _first_existing(
        [history_root / "phase_summary.json", wallet_dir / "history" / "historical" / "phase_summary.json"]
    )
    closed_positions_path = _first_existing([history_root / "closed_positions.json"])

    historical_summary = _load_json(historical_summary_path)
    phase_summary = _load_json(phase_summary_path)
    closed_positions_rows = _family_rows(
        _load_json(closed_positions_path),
        family,
    )
    recent_activity_rows = _family_rows(_load_json(wallet_dir / "recent_cap_activity.json"), family)
    recent_joined_rows = _family_rows(
        (_load_json(wallet_dir / "execution_features.json").get("rows") or []),
        family,
    )

    recent_activity = summarize_recent_activity(recent_activity_rows)
    recent_joined = summarize_joined_market_conditions(recent_joined_rows)
    closed_positions = summarize_closed_positions(closed_positions_rows)

    return {
        "wallet": wallet,
        "wallet_key": wallet_key,
        "focus_family": family,
        "paths": {
            "historical_summary": str(historical_summary_path),
            "phase_summary": str(phase_summary_path),
            "recent_activity": str(wallet_dir / "recent_cap_activity.json"),
            "recent_execution_features": str(wallet_dir / "execution_features.json"),
            "historical_closed_positions": str(closed_positions_path),
            "whale_activity": str(whale_dir / "activity.json"),
        },
        "historical_summary": historical_summary.get("activity"),
        "phase_summary": phase_summary,
        "recent_activity_summary": recent_activity,
        "recent_market_condition_summary": recent_joined,
        "closed_position_summary": closed_positions,
        "takeaways": build_takeaways(
            historical_summary=historical_summary,
            phase_summary=phase_summary,
            recent_activity=recent_activity,
            recent_joined=recent_joined,
            closed_positions=closed_positions,
        ),
    }


def build_markdown(report: dict[str, Any]) -> str:
    hist = report["historical_summary"]
    recent = report["recent_activity_summary"]
    joined = report["recent_market_condition_summary"]
    closed = report["closed_position_summary"]
    lines = [
        f"# Strategy Report: {report['wallet_key']}",
        "",
        f"Wallet: `{report['wallet']}`",
        f"Focus family: `{report['focus_family']}`",
        "",
        "## Key Takeaways",
    ]
    for point in report["takeaways"]:
        lines.append(f"- {point}")
    lines.extend(
        [
            "",
            "## Historical Shape",
            f"- Rows: {hist['rows']}",
            f"- Markets: {hist['market_count']}",
            f"- Two-sided markets: {hist['two_sided_market_count']}",
            f"- Merge markets: {hist['merge_market_count']}",
            f"- Asset mix: {hist['asset_counts']}",
            f"- Family mix: {hist['family_counts']}",
            "",
            "## Recent Entry Pattern",
            f"- Buy rows: {recent['trade_buy_rows']}",
            f"- Markets: {recent['markets']}",
            f"- Median clip USD: {recent['clip_usd']['median']}",
            f"- P90 clip USD: {recent['clip_usd']['p90']}",
            f"- Median first buy offset sec: {recent['timing']['first_buy_offset_sec_median']}",
            f"- Median last buy offset sec: {recent['timing']['last_buy_offset_sec_median']}",
            f"- Median fills per market: {recent['fragmentation']['fills_per_market_median']}",
            "",
            "## Recent Market Conditions",
            f"- Execution mix: {joined['execution_mix']}",
            f"- Fill quality: {joined['fill_quality']}",
            f"- Bucket counts: {joined['timing']['bucket_counts']}",
            f"- Mean ask sum: {joined['market_conditions']['ask_sum_mean']}",
            f"- Ask sum < 1 count: {joined['market_conditions']['ask_sum_lt_one_count']}",
            f"- Maker fill-vs-ask mean: {joined['execution_edge']['maker_fill_vs_ask_mean']}",
            f"- Taker fill-vs-ask mean: {joined['execution_edge']['taker_fill_vs_ask_mean']}",
            "",
            "## Closed Positions",
            f"- Closed markets sampled: {closed['markets']}",
            f"- Paired closed markets: {closed['paired_market_count']}",
            f"- Total realized PnL: {closed['realized_pnl']['total']}",
            f"- Win rate: {closed['realized_pnl']['win_rate']}",
            f"- Median total bought per closed market: {closed['capital_per_closed_market']['median_total_bought']}",
        ]
    )
    return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wallet", default=DEFAULT_WALLET)
    parser.add_argument("--family", default=DEFAULT_FAMILY)
    parser.add_argument("--output-json", default="")
    parser.add_argument("--output-md", default="")
    args = parser.parse_args()

    report = build_report(args.wallet.lower(), args.family)
    wallet_key = report["wallet_key"]
    wallet_dir = WALLET_DIR / wallet_key

    output_json = (
        Path(args.output_json)
        if args.output_json
        else wallet_dir / "strategy_report_btc_5m.json"
    )
    output_md = (
        Path(args.output_md)
        if args.output_md
        else wallet_dir / "strategy_report_btc_5m.md"
    )

    output_json.write_text(json.dumps(report, indent=2))
    output_md.write_text(build_markdown(report))
    print(
        json.dumps(
            {
                "wallet": report["wallet"],
                "output_json": str(output_json),
                "output_md": str(output_md),
                "takeaways": report["takeaways"],
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
