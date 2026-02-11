"""Post-trade analysis: performance metrics, strategy breakdown, and recommendations."""

from __future__ import annotations

import logging
import sqlite3
from collections import defaultdict
from dataclasses import dataclass, field
from datetime import datetime, timedelta

logger = logging.getLogger(__name__)


@dataclass
class StrategyStats:
    name: str
    total_trades: int = 0
    wins: int = 0
    losses: int = 0
    total_pnl: float = 0.0
    avg_edge: float = 0.0
    avg_confidence: float = 0.0
    best_pnl: float = 0.0
    worst_pnl: float = 0.0

    @property
    def win_rate(self) -> float:
        return self.wins / self.total_trades if self.total_trades > 0 else 0.0

    @property
    def avg_pnl(self) -> float:
        return self.total_pnl / self.total_trades if self.total_trades > 0 else 0.0


@dataclass
class TimeWindowStats:
    window: str  # e.g. "morning", "afternoon", "evening"
    trades: int = 0
    win_rate: float = 0.0
    pnl: float = 0.0


@dataclass
class AnalysisReport:
    """Full post-trade analysis report."""

    generated_at: datetime = field(default_factory=datetime.utcnow)
    total_trades: int = 0
    overall_win_rate: float = 0.0
    overall_pnl: float = 0.0
    overall_avg_edge: float = 0.0
    strategy_stats: list[StrategyStats] = field(default_factory=list)
    time_window_stats: list[TimeWindowStats] = field(default_factory=list)
    top_markets: list[dict] = field(default_factory=list)
    recommendations: list[str] = field(default_factory=list)


def _classify_time_window(hour: int) -> str:
    """Classify hour into trading windows."""
    if 6 <= hour < 12:
        return "morning"
    elif 12 <= hour < 18:
        return "afternoon"
    elif 18 <= hour < 23:
        return "evening"
    else:
        return "overnight"


def analyze_performance(db_path: str, days: int = 30) -> AnalysisReport:
    """Query resolved positions from SQLite and generate a full analysis report.

    Args:
        db_path: Path to the SQLite database
        days: Number of days to look back

    Returns:
        AnalysisReport with strategy breakdown, time analysis, and recommendations
    """
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row

    cutoff = (datetime.utcnow() - timedelta(days=days)).isoformat()

    # Fetch all resolved trades with their results
    rows = conn.execute(
        """
        SELECT t.id, t.market_id, t.market_question, t.source, t.edge,
               t.confidence, t.size_usd, t.price, t.created_at,
               r.won, r.pnl_usd
        FROM trades t
        JOIN results r ON r.trade_id = t.id
        WHERE r.resolved = 1 AND t.created_at >= ?
        ORDER BY t.created_at DESC
        """,
        (cutoff,),
    ).fetchall()

    conn.close()

    if not rows:
        return AnalysisReport(recommendations=["No resolved trades found. Keep trading!"])

    report = AnalysisReport()
    report.total_trades = len(rows)

    # Overall stats
    wins = sum(1 for r in rows if r["won"])
    report.overall_win_rate = wins / len(rows)
    report.overall_pnl = sum(r["pnl_usd"] for r in rows)
    report.overall_avg_edge = sum(r["edge"] for r in rows) / len(rows)

    # Per-strategy breakdown
    by_strategy: dict[str, list] = defaultdict(list)
    for row in rows:
        by_strategy[row["source"]].append(row)

    for strategy_name, trades in by_strategy.items():
        stats = StrategyStats(name=strategy_name)
        stats.total_trades = len(trades)
        stats.wins = sum(1 for t in trades if t["won"])
        stats.losses = stats.total_trades - stats.wins
        stats.total_pnl = sum(t["pnl_usd"] for t in trades)
        stats.avg_edge = sum(t["edge"] for t in trades) / len(trades)
        stats.avg_confidence = sum(t["confidence"] for t in trades) / len(trades)
        pnls = [t["pnl_usd"] for t in trades]
        stats.best_pnl = max(pnls) if pnls else 0
        stats.worst_pnl = min(pnls) if pnls else 0
        report.strategy_stats.append(stats)

    report.strategy_stats.sort(key=lambda s: s.total_pnl, reverse=True)

    # Time window analysis
    by_window: dict[str, list] = defaultdict(list)
    for row in rows:
        try:
            ts = datetime.fromisoformat(row["created_at"])
            window = _classify_time_window(ts.hour)
        except (TypeError, ValueError):
            window = "unknown"
        by_window[window].append(row)

    for window, trades in by_window.items():
        tw_wins = sum(1 for t in trades if t["won"])
        report.time_window_stats.append(
            TimeWindowStats(
                window=window,
                trades=len(trades),
                win_rate=tw_wins / len(trades) if trades else 0,
                pnl=sum(t["pnl_usd"] for t in trades),
            )
        )

    # Top markets by PnL
    by_market: dict[str, dict] = defaultdict(lambda: {"question": "", "pnl": 0, "trades": 0})
    for row in rows:
        m = by_market[row["market_id"]]
        m["question"] = row["market_question"]
        m["pnl"] += row["pnl_usd"]
        m["trades"] += 1

    sorted_markets = sorted(by_market.items(), key=lambda x: x[1]["pnl"], reverse=True)
    report.top_markets = [
        {"market_id": mid, **data} for mid, data in sorted_markets[:10]
    ]

    # Generate recommendations
    report.recommendations = _generate_recommendations(report)

    return report


def _generate_recommendations(report: AnalysisReport) -> list[str]:
    """Generate actionable recommendations based on the analysis."""
    recs = []

    # Overall performance
    if report.overall_win_rate < 0.5:
        recs.append(
            f"Win rate is {report.overall_win_rate:.0%}. "
            "Consider raising MIN_EDGE_THRESHOLD to be more selective."
        )
    elif report.overall_win_rate > 0.7:
        recs.append(
            f"Win rate is {report.overall_win_rate:.0%}. "
            "Could lower MIN_EDGE_THRESHOLD slightly to capture more opportunities."
        )

    # Strategy-specific
    for stats in report.strategy_stats:
        if stats.total_trades >= 5:
            if stats.win_rate < 0.4:
                recs.append(
                    f"Strategy '{stats.name}' underperforming ({stats.win_rate:.0%} win rate). "
                    "Consider disabling or increasing edge threshold."
                )
            elif stats.win_rate > 0.7 and stats.total_pnl > 0:
                recs.append(
                    f"Strategy '{stats.name}' performing well ({stats.win_rate:.0%}, "
                    f"${stats.total_pnl:+.2f}). Consider increasing position sizes."
                )

    # Time windows
    best_window = max(report.time_window_stats, key=lambda tw: tw.pnl) if report.time_window_stats else None
    worst_window = min(report.time_window_stats, key=lambda tw: tw.pnl) if report.time_window_stats else None

    if best_window and best_window.pnl > 0:
        recs.append(
            f"Best trading window: {best_window.window} "
            f"(${best_window.pnl:+.2f}, {best_window.win_rate:.0%} win rate). "
            "Consider concentrating activity here."
        )

    if worst_window and worst_window.pnl < 0:
        recs.append(
            f"Worst trading window: {worst_window.window} "
            f"(${worst_window.pnl:+.2f}). Consider reducing activity."
        )

    # Edge calibration
    if report.overall_avg_edge > 0.20:
        recs.append(
            f"Average edge is high ({report.overall_avg_edge:.2%}). "
            "Signals may be overfitting. Monitor for regression."
        )

    if not recs:
        recs.append("Performance looks solid. Keep current parameters and monitor.")

    return recs


def print_report(report: AnalysisReport):
    """Pretty-print the analysis report to the logger."""
    logger.info(f"\n{'='*60}")
    logger.info("POST-TRADE ANALYSIS REPORT")
    logger.info(f"Generated: {report.generated_at.isoformat()}")
    logger.info(f"{'='*60}")

    logger.info(f"\nOverall: {report.total_trades} trades | "
                f"{report.overall_win_rate:.0%} win rate | "
                f"${report.overall_pnl:+.2f} PnL | "
                f"Avg edge: {report.overall_avg_edge:.2%}")

    logger.info("\n--- Strategy Breakdown ---")
    for s in report.strategy_stats:
        logger.info(
            f"  {s.name}: {s.total_trades} trades | "
            f"{s.win_rate:.0%} WR | "
            f"${s.total_pnl:+.2f} PnL | "
            f"Avg edge: {s.avg_edge:.2%} | "
            f"Best: ${s.best_pnl:+.2f} / Worst: ${s.worst_pnl:+.2f}"
        )

    logger.info("\n--- Time Windows ---")
    for tw in sorted(report.time_window_stats, key=lambda x: x.pnl, reverse=True):
        logger.info(
            f"  {tw.window}: {tw.trades} trades | "
            f"{tw.win_rate:.0%} WR | ${tw.pnl:+.2f} PnL"
        )

    if report.top_markets:
        logger.info("\n--- Top Markets ---")
        for m in report.top_markets[:5]:
            logger.info(
                f"  ${m['pnl']:+.2f} ({m['trades']} trades) | "
                f"{m['question'][:60]}"
            )

    logger.info("\n--- Recommendations ---")
    for i, rec in enumerate(report.recommendations, 1):
        logger.info(f"  {i}. {rec}")

    logger.info(f"\n{'='*60}")
