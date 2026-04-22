from __future__ import annotations

import pytest

from backtesting.whale_pair_backtest import (
    BacktestMarketResult,
    ExecutionModel,
    build_w1_comparison,
    simulate_market,
    summarize_whale_activity,
)
from strategies.whale_pair import WhalePairConfig


def test_simulate_market_matches_pairs_and_resolves_residuals():
    market = {
        "market_id": "m1",
        "slug": "btc-updown-5m-test",
        "start_time": "2026-01-01T00:00:00+00:00",
        "winner": "Up",
    }
    snapshots = [
        {
            "time": "2026-01-01T00:00:20+00:00",
            "best_ask_up": 0.40,
            "ask_size_up": 100.0,
            "best_ask_down": 0.45,
            "ask_size_down": 100.0,
        },
        {
            "time": "2026-01-01T00:00:40+00:00",
            "best_ask_up": 0.90,
            "ask_size_up": 100.0,
            "best_ask_down": 0.05,
            "ask_size_down": 100.0,
        },
    ]
    cfg = WhalePairConfig(
        base_clip_usd=10.0,
        aggressive_clip_usd=20.0,
        max_gross_cost_usd=100.0,
        min_seconds_from_start=10,
        max_seconds_from_start=20,
    )
    result = simulate_market(market, snapshots, cfg)
    assert result.fills >= 2
    assert result.fills % 2 == 0
    assert result.matches > 0
    assert result.gross_cost_usd > 0
    assert result.residual_pnl_usd == 0
    assert result.total_pnl_usd != 0


def test_summarize_whale_activity_groups_by_slug():
    rows = [
        {
            "slug": "btc-updown-5m-1000",
            "timestamp": 1015,
            "type": "TRADE",
            "side": "BUY",
            "outcome": "Up",
            "usdcSize": 5.0,
        },
        {
            "slug": "btc-updown-5m-1000",
            "timestamp": 1030,
            "type": "TRADE",
            "side": "BUY",
            "outcome": "Down",
            "usdcSize": 7.0,
        },
        {
            "slug": "btc-updown-5m-1000",
            "timestamp": 1040,
            "type": "MERGE",
            "side": "",
            "outcome": "",
            "usdcSize": 10.0,
        },
    ]
    summary = summarize_whale_activity(rows)
    rec = summary["btc-updown-5m-1000"]
    assert rec["whale_buy_rows"] == 2
    assert rec["whale_merge_rows"] == 1
    assert rec["whale_up_buy_usdc"] == 5.0
    assert rec["whale_down_buy_usdc"] == 7.0
    assert rec["whale_first_offset_sec"] == 15
    assert rec["whale_last_offset_sec"] == 30


def test_simulate_market_execution_latency_can_partial_and_add_slippage():
    market = {
        "market_id": "m2",
        "slug": "btc-updown-5m-latency",
        "start_time": "2026-01-01T00:00:00+00:00",
        "winner": "Up",
    }
    snapshots = [
        {
            "time": "2026-01-01T00:00:20+00:00",
            "best_ask_up": 0.40,
            "ask_size_up": 100.0,
            "best_ask_down": 0.45,
            "ask_size_down": 100.0,
        },
        {
            "time": "2026-01-01T00:00:21+00:00",
            "best_ask_up": 0.42,
            "ask_size_up": 5.0,
            "best_ask_down": 0.47,
            "ask_size_down": 5.0,
        },
    ]
    cfg = WhalePairConfig(
        base_clip_usd=10.0,
        aggressive_clip_usd=20.0,
        max_gross_cost_usd=100.0,
        min_seconds_from_start=10,
        max_seconds_from_start=20,
    )

    baseline = simulate_market(market, snapshots, cfg)
    degraded = simulate_market(
        market,
        snapshots,
        cfg,
        execution=ExecutionModel(latency_snapshots=1, fill_fraction=0.5),
    )

    assert baseline.fills == 2
    assert baseline.partial_orders == 0
    assert degraded.fills == 2
    assert degraded.partial_orders == 2
    assert degraded.gross_cost_usd < baseline.gross_cost_usd
    assert degraded.execution_slippage_usd > 0


def test_simulate_market_execution_latency_can_miss_when_no_future_snapshot():
    market = {
        "market_id": "m3",
        "slug": "btc-updown-5m-missed",
        "start_time": "2026-01-01T00:00:00+00:00",
        "winner": "Down",
    }
    snapshots = [
        {
            "time": "2026-01-01T00:00:20+00:00",
            "best_ask_up": 0.30,
            "ask_size_up": 100.0,
            "best_ask_down": 0.40,
            "ask_size_down": 100.0,
        },
    ]
    cfg = WhalePairConfig(
        base_clip_usd=10.0,
        aggressive_clip_usd=20.0,
        max_gross_cost_usd=100.0,
        min_seconds_from_start=10,
        max_seconds_from_start=298,
    )

    result = simulate_market(
        market,
        snapshots,
        cfg,
        execution=ExecutionModel(latency_snapshots=1, fill_fraction=1.0),
    )

    assert result.attempted_orders == 2
    assert result.missed_orders == 2
    assert result.fills == 0
    assert result.gross_cost_usd == 0


def _mk_result(
    *,
    slug: str,
    sim_up: float,
    sim_down: float,
    sim_first: int | None,
    sim_last: int | None,
    fills: int,
    whale_up: float,
    whale_down: float,
    whale_first: int | None,
    whale_last: int | None,
    whale_buy_rows: int,
) -> BacktestMarketResult:
    return BacktestMarketResult(
        market_id=slug,
        slug=slug,
        start_time="2026-01-01T00:00:00+00:00",
        winner="Up",
        fills=fills,
        matches=0,
        gross_cost_usd=sim_up + sim_down,
        merged_pnl_usd=0.0,
        residual_pnl_usd=0.0,
        total_pnl_usd=0.0,
        unresolved_up_shares=0.0,
        unresolved_down_shares=0.0,
        sim_up_buy_usdc=sim_up,
        sim_down_buy_usdc=sim_down,
        sim_first_offset_sec=sim_first,
        sim_last_offset_sec=sim_last,
        whale_buy_rows=whale_buy_rows,
        whale_up_buy_usdc=whale_up,
        whale_down_buy_usdc=whale_down,
        whale_first_offset_sec=whale_first,
        whale_last_offset_sec=whale_last,
    )


def test_build_w1_comparison_shapes_and_aggregates():
    results = [
        _mk_result(
            slug="btc-updown-5m-a",
            sim_up=10.0, sim_down=20.0,
            sim_first=15, sim_last=60, fills=4,
            whale_up=30.0, whale_down=10.0,
            whale_first=5, whale_last=80, whale_buy_rows=6,
        ),
        _mk_result(
            slug="btc-updown-5m-b",
            sim_up=0.0, sim_down=0.0,
            sim_first=None, sim_last=None, fills=0,
            whale_up=5.0, whale_down=5.0,
            whale_first=20, whale_last=30, whale_buy_rows=2,
        ),
    ]
    cmp_obj = build_w1_comparison(results)
    assert cmp_obj["overlap_markets"] == 2
    assert cmp_obj["overlap_markets_both_active"] == 1

    per_market = {row["slug"]: row for row in cmp_obj["per_market"]}
    a = per_market["btc-updown-5m-a"]
    assert a["sim"]["fill_count"] == 4
    assert a["sim"]["first_buy_offset_sec"] == 15
    assert a["sim"]["last_buy_offset_sec"] == 60
    assert a["sim"]["buy_usdc_total"] == 30.0
    assert a["sim"]["imbalance_ratio"] == 2.0
    assert a["whale"]["fill_count"] == 6
    assert a["whale"]["buy_usdc_total"] == 40.0
    assert a["whale"]["imbalance_ratio"] == 3.0

    b = per_market["btc-updown-5m-b"]
    # sim had no fills, so first/last and imbalance ratio are None.
    assert b["sim"]["first_buy_offset_sec"] is None
    assert b["sim"]["imbalance_ratio"] is None

    ag = cmp_obj["aggregates"]
    # Only market 'a' has both sides' offsets defined, so mean = (sim - whale) = 10.
    assert ag["first_buy_offset_sec_mean_diff"] == 10.0
    assert ag["last_buy_offset_sec_mean_diff"] == -20.0
    # fill_count diff: a: 4-6=-2, b: 0-2=-2 -> mean -2.
    assert ag["fill_count_mean_diff"] == -2.0
    # buy_usdc_total diff: a: 30-40=-10, b: 0-10=-10 -> mean -10.
    assert ag["buy_usdc_total_mean_diff"] == -10.0


def test_build_w1_comparison_empty():
    cmp_obj = build_w1_comparison([])
    assert cmp_obj["overlap_markets"] == 0
    assert cmp_obj["overlap_markets_both_active"] == 0
    # side_tilt_markets_considered is a counter, stays 0 on empty input.
    integer_counters = {"side_tilt_markets_considered"}
    for key, value in cmp_obj["aggregates"].items():
        if key in integer_counters:
            assert value == 0, f"{key} should be 0 on empty input"
        else:
            assert value is None, f"{key} should be None on empty input"


def test_build_w1_comparison_zero_side_imbalance_is_none():
    results = [
        _mk_result(
            slug="btc-updown-5m-oneside",
            sim_up=25.0, sim_down=0.0,
            sim_first=10, sim_last=10, fills=1,
            whale_up=15.0, whale_down=0.0,
            whale_first=5, whale_last=5, whale_buy_rows=1,
        ),
    ]
    cmp_obj = build_w1_comparison(results)
    row = cmp_obj["per_market"][0]
    assert row["sim"]["imbalance_ratio"] is None
    assert row["whale"]["imbalance_ratio"] is None


def test_build_w1_comparison_reports_heavier_side_and_tilt_agreement():
    results = [
        # Both lean Up.
        _mk_result(
            slug="btc-updown-5m-agree-up",
            sim_up=30.0, sim_down=10.0,
            sim_first=15, sim_last=60, fills=4,
            whale_up=40.0, whale_down=10.0,
            whale_first=5, whale_last=80, whale_buy_rows=6,
        ),
        # Disagree: sim leans Up, whale leans Down.
        _mk_result(
            slug="btc-updown-5m-disagree",
            sim_up=30.0, sim_down=10.0,
            sim_first=15, sim_last=60, fills=4,
            whale_up=10.0, whale_down=40.0,
            whale_first=5, whale_last=80, whale_buy_rows=6,
        ),
        # Whale zero -> excluded from tilt-agreement denominator.
        _mk_result(
            slug="btc-updown-5m-whale-zero",
            sim_up=5.0, sim_down=0.0,
            sim_first=20, sim_last=20, fills=1,
            whale_up=0.0, whale_down=0.0,
            whale_first=None, whale_last=None, whale_buy_rows=0,
        ),
    ]
    cmp_obj = build_w1_comparison(results)
    per = {row["slug"]: row for row in cmp_obj["per_market"]}
    assert per["btc-updown-5m-agree-up"]["sim"]["heavier_side"] == "Up"
    assert per["btc-updown-5m-agree-up"]["whale"]["heavier_side"] == "Up"
    assert per["btc-updown-5m-disagree"]["sim"]["heavier_side"] == "Up"
    assert per["btc-updown-5m-disagree"]["whale"]["heavier_side"] == "Down"
    assert per["btc-updown-5m-whale-zero"]["whale"]["heavier_side"] is None

    ag = cmp_obj["aggregates"]
    # Two markets with both sides' tilts defined: one agrees, one disagrees.
    assert ag["side_tilt_markets_considered"] == 2
    assert ag["side_tilt_agreement_rate"] == 0.5
    # Overlap rate: 1/3 of markets have both sim and whale active with fills.
    # sim fills > 0 for all 3, whale fill_count > 0 for first two -> 2/3.
    assert cmp_obj["both_active_rate"] == pytest.approx(2 / 3)


def test_build_w1_comparison_both_active_rate_none_when_empty():
    cmp_obj = build_w1_comparison([])
    assert cmp_obj["both_active_rate"] is None
    assert cmp_obj["aggregates"]["side_tilt_agreement_rate"] is None
    assert cmp_obj["aggregates"]["side_tilt_markets_considered"] == 0
