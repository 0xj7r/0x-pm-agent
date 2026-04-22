from __future__ import annotations

import inspect

import scripts.whale_pair_walkforward as walkforward_module
from backtesting.whale_pair_backtest import ExecutionModel
from scripts.whale_pair_walkforward import (
    SENSITIVITY_GRID,
    build_folds,
    evaluate_markets,
    iter_configs,
    iter_configs_fast,
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


def test_iter_configs_fast_is_strict_subset_of_full():
    full = list(iter_configs())
    fast = list(iter_configs_fast())
    assert len(fast) >= 1
    assert len(fast) < len(full)
    full_set = {
        (
            c.max_pair_cost,
            c.base_clip_usd,
            c.aggressive_clip_usd,
            c.max_gross_cost_usd,
            c.min_seconds_from_start,
            c.completion_min_pnl_per_share,
        )
        for c in full
    }
    for c in fast:
        key = (
            c.max_pair_cost,
            c.base_clip_usd,
            c.aggressive_clip_usd,
            c.max_gross_cost_usd,
            c.min_seconds_from_start,
            c.completion_min_pnl_per_share,
        )
        assert key in full_set


def test_build_folds_is_strict_forward_only():
    markets = [{"market_id": f"m{i}", "slug": f"s{i}"} for i in range(10)]
    folds = build_folds(markets, min_train=4, fold_size=2)
    assert [len(f.train_markets) for f in folds] == [4, 6, 8]
    assert [len(f.test_markets) for f in folds] == [2, 2, 2]
    for fold in folds:
        train_ids = {m["market_id"] for m in fold.train_markets}
        test_ids = {m["market_id"] for m in fold.test_markets}
        assert train_ids.isdisjoint(test_ids)
        last_train = fold.train_markets[-1]["market_id"]
        first_test = fold.test_markets[0]["market_id"]
        assert int(last_train[1:]) < int(first_test[1:])


def test_walkforward_never_consumes_whale_activity():
    """Anti-bias: walkforward must not import or reference whale activity data.

    Primary truth is the historical BTC DB; whale overlap is a post-hoc
    shape benchmark in the backtest module only.
    """
    source = inspect.getsource(walkforward_module)
    for forbidden in (
        "whale_activity",
        "summarize_whale_activity",
        "whale_by_slug",
        "whale_summary",
    ):
        assert forbidden not in source, (
            f"walkforward should not reference `{forbidden}` — whale data "
            "must never feed config selection"
        )


def test_sensitivity_sweep_reacts_to_latency():
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
                "time": "2026-01-01T00:00:22+00:00",
                "best_ask_up": 0.46,
                "ask_size_up": 20.0,
                "best_ask_down": 0.51,
                "ask_size_down": 20.0,
            },
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
        "max_pair_cost": [0.99],
        "min_seconds_from_start": [10],
        "base_clip_usd": [10.0],
        "aggressive_clip_usd": [20.0],
        "max_gross_cost_usd": [100.0],
    }

    clean = sensitivity_sweep(
        markets,
        lambda mid: snapshots[mid],
        baseline,
        grid=grid,
        execution=ExecutionModel(latency_snapshots=0, fill_fraction=1.0),
    )
    degraded = sensitivity_sweep(
        markets,
        lambda mid: snapshots[mid],
        baseline,
        grid=grid,
        execution=ExecutionModel(latency_snapshots=1, fill_fraction=1.0),
    )

    clean_slippage = clean["_baseline"][0]["execution_slippage_usd"]
    degraded_slippage = degraded["_baseline"][0]["execution_slippage_usd"]
    assert clean_slippage == 0.0
    assert degraded_slippage > 0.0
