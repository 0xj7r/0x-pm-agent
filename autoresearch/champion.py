"""Champion selection layer on top of deterministic autoresearch."""
from __future__ import annotations

from collections import Counter, defaultdict
from dataclasses import dataclass
from statistics import mean
from typing import Any


MIN_RECENT_TRADES = 8


def _bucket_price(price: float) -> str:
    if price < 0.10:
        return "<0.10"
    if price < 0.25:
        return "0.10-0.25"
    if price < 0.40:
        return "0.25-0.40"
    if price < 0.60:
        return "0.40-0.60"
    if price < 0.75:
        return "0.60-0.75"
    return ">=0.75"


def _bucket_move(move_pct: float) -> str:
    move = abs(move_pct)
    if move < 0.07:
        return "<0.07%"
    if move < 0.09:
        return "0.07-0.09%"
    if move < 0.12:
        return "0.09-0.12%"
    return ">=0.12%"


def _mean_or_zero(values: list[float]) -> float:
    return mean(values) if values else 0.0


def _price_bucket_penalty(bucket_stats: dict[str, dict[str, float]]) -> tuple[float, list[str]]:
    penalty = 0.0
    notes: list[str] = []
    cheap_bad = 0
    for bucket in ("<0.10", "0.10-0.25", "0.25-0.40"):
        stats = bucket_stats.get(bucket)
        if not stats or stats["trades"] == 0:
            continue
        if stats["pnl_per_trade"] <= 0:
            cheap_bad += stats["trades"]
    total_trades = sum(stats["trades"] for stats in bucket_stats.values())
    if total_trades and cheap_bad / total_trades >= 0.35:
        penalty += 0.25
        notes.append("heavy reliance on losing cheap-contract buckets")
    return penalty, notes


def _recent_manifest(manifest: list[Any], fraction: float = 0.2, min_markets: int = 40) -> list[Any]:
    if not manifest:
        return []
    recent_n = max(min_markets, int(len(manifest) * fraction))
    return manifest[-min(recent_n, len(manifest)) :]


def _extract_trade_rows(
    manifest: list[Any],
    features: dict[str, Any],
    strategy_name: str,
    params: dict[str, Any],
    stress: Any | None = None,
) -> list[dict[str, Any]]:
    from backtesting.eval.evaluator import find_first_trade
    from backtesting.eval.validate_strategy_readiness import StressScenario, _slice_market
    from strategies.registry import build_check_fn

    check_fn = build_check_fn(strategy_name, params)
    rows: list[dict[str, Any]] = []
    stress = stress or StressScenario("base")

    for meta in manifest:
        pm = _slice_market(meta, features)
        trade = find_first_trade(
            pm,
            check_fn,
            entry_delay=stress.entry_delay,
            entry_slippage=stress.entry_slippage,
            fee_multiplier=stress.fee_multiplier,
        )
        if trade is None:
            continue
        move_pct = float(pm.abs_move[trade.entry_index])
        rows.append(
            {
                "market_id": meta.market_id,
                "won": trade.won,
                "pnl": trade.pnl,
                "entry_price": trade.entry_price,
                "move_pct": move_pct,
                "price_bucket": _bucket_price(trade.entry_price),
                "move_bucket": _bucket_move(move_pct),
                "direction": trade.direction,
            }
        )
    return rows


def _summarize_trade_rows(rows: list[dict[str, Any]]) -> dict[str, Any]:
    trades = len(rows)
    wins = sum(1 for row in rows if row["won"])
    pnl = sum(row["pnl"] for row in rows)
    return {
        "trades": trades,
        "wins": wins,
        "losses": trades - wins,
        "win_rate": wins / trades if trades else 0.0,
        "pnl": pnl,
        "pnl_per_trade": pnl / trades if trades else 0.0,
        "avg_entry": _mean_or_zero([row["entry_price"] for row in rows]),
    }


def _bucket_summary(rows: list[dict[str, Any]], key: str) -> dict[str, dict[str, float]]:
    grouped: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        grouped[row[key]].append(row)
    summary: dict[str, dict[str, float]] = {}
    for bucket, items in sorted(grouped.items()):
        stats = _summarize_trade_rows(items)
        summary[bucket] = {
            "trades": stats["trades"],
            "win_rate": stats["win_rate"],
            "pnl": stats["pnl"],
            "pnl_per_trade": stats["pnl_per_trade"],
        }
    return summary


def _decision_score(candidate: dict[str, Any], validation: dict[str, Any], recent_stats: dict[str, Any], price_buckets: dict[str, dict[str, float]]) -> tuple[float, list[str]]:
    agg = validation["validation"]["aggregate_oos"]
    holdout = validation["validation"]["final_holdout"]
    robustness = validation["robustness"]
    stress = validation["stress"]["delay_and_slip"]

    notes: list[str] = []
    score = 0.0
    score += candidate.get("test_sharpe", 0.0) * 1.5
    score += agg["pnl_per_trade"] * 4.0
    score += holdout["pnl_per_trade"] * 5.0
    score += recent_stats["pnl_per_trade"] * 6.0
    score += recent_stats["win_rate"] * 1.5
    score += min(robustness["stable_neighbor_count"], 4) * 0.25
    score += stress["pnl_per_trade"] * 3.0

    if recent_stats["trades"] < MIN_RECENT_TRADES:
        score -= 0.5
        notes.append("limited recent trade count")
    if holdout["trades"] < 10:
        score -= 0.5
        notes.append("thin holdout sample")
    if stress["pnl_per_trade"] <= 0:
        score -= 1.0
        notes.append("fails delay+slippage stress")

    penalty, penalty_notes = _price_bucket_penalty(price_buckets)
    score -= penalty
    notes.extend(penalty_notes)

    return score, notes


@dataclass(frozen=True)
class ChampionDecision:
    coin: str
    chosen: dict[str, Any]
    rankings: list[dict[str, Any]]
    rejected: list[dict[str, Any]]

    def to_dict(self) -> dict[str, Any]:
        return {
            "coin": self.coin,
            "chosen": self.chosen,
            "rankings": self.rankings,
            "rejected": self.rejected,
        }


def rank_candidates(
    coin: str,
    shortlist: list[dict[str, Any]],
    manifest: list[Any],
    features: dict[str, Any],
    folds: int = 5,
    holdout_pct: float = 0.2,
) -> ChampionDecision:
    from backtesting.eval.validate_strategy_readiness import validate_coin

    recent_manifest = _recent_manifest(manifest)
    rankings: list[dict[str, Any]] = []
    rejected: list[dict[str, Any]] = []

    for candidate in shortlist:
        validation = validate_coin(
            coin,
            candidate["name"],
            candidate["params"],
            folds=folds,
            holdout_pct=holdout_pct,
        )
        recent_rows = _extract_trade_rows(
            recent_manifest,
            features,
            candidate["name"],
            candidate["params"],
        )
        recent_stats = _summarize_trade_rows(recent_rows)
        price_buckets = _bucket_summary(recent_rows, "price_bucket")
        move_buckets = _bucket_summary(recent_rows, "move_bucket")
        score, notes = _decision_score(candidate, validation, recent_stats, price_buckets)

        row = {
            "name": candidate["name"],
            "params": candidate["params"],
            "search_candidate": candidate,
            "validation": validation,
            "recent": {
                "window_markets": len(recent_manifest),
                "stats": recent_stats,
                "price_buckets": price_buckets,
                "move_buckets": move_buckets,
                "direction_counts": dict(Counter(item["direction"] for item in recent_rows)),
            },
            "decision_score": score,
            "notes": notes,
        }

        if validation["readiness"] == "reject_for_now":
            rejected.append(row)
        else:
            rankings.append(row)

    rankings.sort(key=lambda row: row["decision_score"], reverse=True)
    rejected.sort(key=lambda row: row["decision_score"], reverse=True)

    chosen = rankings[0] if rankings else {}
    return ChampionDecision(coin=coin, chosen=chosen, rankings=rankings, rejected=rejected)
