from __future__ import annotations

from backtesting.whale_pair_backtest import ExecutionModel
from scripts.whale_pair_walkforward import (
    SENSITIVITY_GRID,
    evaluate_markets,
    sensitivity_sweep,
)
from strategies.whale_pair import WhalePairConfig


def test_evaluate_markets_aggregates_execution_metrics():
    markets = [
        {
            "market_id": "m1",
            "slug": "btc-updown-5m-1",
            "start_time": "2026-01-01T00:00:00+00:00",
            "winner": "Up",
        }
    ]
    snapshots = {
        "m1": [
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
    }
    cfg = WhalePairConfig(
        base_clip_usd=10.0,
        aggressive_clip_usd=20.0,
        max_gross_cost_usd=100.0,
        min_seconds_from_start=10,
        max_seconds_from_start=20,
    )

    result = evaluate_markets(
        markets,
        lambda market_id: snapshots[market_id],
        cfg,
        execution=ExecutionModel(latency_snapshots=1, fill_fraction=0.5),
    )

    assert result.traded == 1
    assert result.attempted_orders == 2
    assert result.partial_orders == 2
    assert result.missed_orders == 0
    assert result.execution_slippage_usd > 0


def test_sensitivity_sweep_covers_all_required_parameters():
    markets = [
        {
            "market_id": "m1",
            "slug": "btc-updown-5m-1",
            "start_time": "2026-01-01T00:00:00+00:00",
            "winner": "Up",
        }
    ]
    snapshots = {
        "m1": [
            {
                "time": "2026-01-01T00:00:20+00:00",
                "best_ask_up": 0.40,
                "ask_size_up": 100.0,
                "best_ask_down": 0.45,
                "ask_size_down": 100.0,
            },
            {
                "time": "2026-01-01T00:01:20+00:00",
                "best_ask_up": 0.42,
                "ask_size_up": 50.0,
                "best_ask_down": 0.47,
                "ask_size_down": 50.0,
            },
        ]
    }
    baseline = WhalePairConfig(
        base_clip_usd=10.0,
        aggressive_clip_usd=20.0,
        max_gross_cost_usd=100.0,
        min_seconds_from_start=10,
        max_seconds_from_start=200,
    )

    sweep = sensitivity_sweep(markets, lambda mid: snapshots[mid], baseline)

    required = {"max_pair_cost", "min_seconds_from_start", "base_clip_usd", "aggressive_clip_usd", "max_gross_cost_usd"}
    assert required.issubset(set(sweep.keys()))
    assert "_baseline" in sweep and len(sweep["_baseline"]) == 1
    for param in required:
        entries = sweep[param]
        assert len(entries) == len(SENSITIVITY_GRID[param])
        for entry in entries:
            assert "value" in entry
            assert "total_pnl_usd" in entry
            assert "delta_pnl_usd" in entry
            assert "pnl_bps_on_cost" in entry


def test_sensitivity_sweep_baseline_entry_has_zero_delta_when_value_matches():
    markets = [
        {
            "market_id": "m1",
            "slug": "btc-updown-5m-1",
            "start_time": "2026-01-01T00:00:00+00:00",
            "winner": "Up",
        }
    ]
    snapshots = {
        "m1": [
            {
                "time": "2026-01-01T00:00:20+00:00",
                "best_ask_up": 0.40,
                "ask_size_up": 100.0,
                "best_ask_down": 0.45,
                "ask_size_down": 100.0,
            }
        ]
    }
    baseline = WhalePairConfig(
        max_pair_cost=0.99,
        base_clip_usd=10.0,
        aggressive_clip_usd=20.0,
        max_gross_cost_usd=100.0,
        min_seconds_from_start=10,
        max_seconds_from_start=200,
    )

    grid = {
        "max_pair_cost": [0.97, 0.99],
        "min_seconds_from_start": [10],
        "base_clip_usd": [10.0],
        "aggressive_clip_usd": [20.0],
        "max_gross_cost_usd": [100.0],
    }
    sweep = sensitivity_sweep(markets, lambda mid: snapshots[mid], baseline, grid=grid)

    matching = next(e for e in sweep["max_pair_cost"] if e["value"] == 0.99)
    assert abs(matching["delta_pnl_usd"]) < 1e-9
