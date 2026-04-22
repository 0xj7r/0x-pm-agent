from __future__ import annotations

import json
from pathlib import Path

import pytest

from scripts import whale_pair_w1_calibrate, whale_pair_w1_report
from strategies.whale_pair import WhalePairConfig


def _fake_run_backtest_factory(record: dict):
    def fake(db_path, cfg, *, whale_activity_path=None, execution=None):
        record["calls"] = record.get("calls", 0) + 1
        record["last_cfg"] = cfg
        record["last_execution"] = execution
        return {
            "db_path": str(db_path),
            "execution": {
                "latency_snapshots": execution.latency_snapshots if execution else 0,
                "fill_fraction": execution.fill_fraction if execution else 1.0,
            },
            "markets_considered": 3,
            "markets_traded": 2,
            "wins": 1,
            "losses": 1,
            "total_pnl_usd": 0.12,
            "results": [],
            "w1_comparison": {
                "overlap_markets": 2,
                "overlap_markets_both_active": 1,
                "aggregates": {
                    "first_buy_offset_sec_mean_diff": 10.0,
                    "last_buy_offset_sec_mean_diff": -5.0,
                    "fill_count_mean_diff": -1.0,
                    "buy_usdc_total_mean_diff": -2.0,
                    "imbalance_ratio_mean_diff": 0.5,
                },
                "per_market": [],
            },
        }

    return fake


def test_build_report_emits_compact_artifact(tmp_path, monkeypatch):
    record: dict = {}
    monkeypatch.setattr(
        whale_pair_w1_report,
        "run_backtest",
        _fake_run_backtest_factory(record),
    )
    activity = tmp_path / "activity.json"
    activity.write_text(
        json.dumps(
            [
                {
                    "slug": "btc-updown-5m-1000",
                    "timestamp": 1010,
                    "type": "TRADE",
                    "side": "BUY",
                    "outcome": "Up",
                    "usdcSize": 5.0,
                }
            ]
        )
    )
    pnl = tmp_path / "pnl.json"
    pnl.write_text(json.dumps([{"t": 900, "p": 100.0}, {"t": 1100, "p": 150.0}]))
    cfg = WhalePairConfig()
    artifact = whale_pair_w1_report.build_report(
        tmp_path / "fake.db",
        activity,
        cfg,
        pnl_path=pnl,
    )
    assert record["calls"] == 1
    assert "disclaimer" in artifact
    assert "shape benchmark only" in artifact["disclaimer"].lower()
    assert artifact["config"]["base_clip_usd"] == cfg.base_clip_usd
    assert artifact["w1_comparison"]["overlap_markets"] == 2
    assert artifact["pnl_bracket_info"]["pnl_delta"] == 50.0
    assert "Informational" in artifact["pnl_bracket_info"]["note"]


def test_build_report_survives_missing_pnl(tmp_path, monkeypatch):
    record: dict = {}
    monkeypatch.setattr(
        whale_pair_w1_report,
        "run_backtest",
        _fake_run_backtest_factory(record),
    )
    activity = tmp_path / "activity.json"
    activity.write_text(
        json.dumps(
            [
                {
                    "slug": "btc-updown-5m-1000",
                    "timestamp": 1010,
                    "type": "TRADE",
                    "side": "BUY",
                    "outcome": "Up",
                    "usdcSize": 5.0,
                }
            ]
        )
    )
    artifact = whale_pair_w1_report.build_report(
        tmp_path / "fake.db",
        activity,
        WhalePairConfig(),
        pnl_path=None,
    )
    assert "pnl_bracket_info" not in artifact


def test_calibrate_sweep_spans_grid(tmp_path, monkeypatch):
    record: dict = {}
    monkeypatch.setattr(
        whale_pair_w1_calibrate,
        "run_backtest",
        _fake_run_backtest_factory(record),
    )
    activity = tmp_path / "activity.json"
    activity.write_text("[]")
    rows = whale_pair_w1_calibrate.sweep(
        tmp_path / "fake.db",
        activity,
        base=WhalePairConfig(),
        base_clip_usds=[5.0, 10.0],
        aggressive_clip_usds=[20.0],
        max_seconds_from_start_values=[240, 298],
        max_imbalance_ratios=[3.0],
        execution=whale_pair_w1_calibrate.ExecutionModel(),
    )
    assert record["calls"] == 4
    assert len(rows) == 4
    configs = [(r["config"]["base_clip_usd"], r["config"]["max_seconds_from_start"]) for r in rows]
    assert configs == [(5.0, 240), (5.0, 298), (10.0, 240), (10.0, 298)]
    for row in rows:
        assert row["overlap_markets"] == 2
        assert "first_buy_offset_sec_mean_diff" in row["aggregates"]


def test_calibrate_sweep_empty_lists_use_base(tmp_path, monkeypatch):
    record: dict = {}
    monkeypatch.setattr(
        whale_pair_w1_calibrate,
        "run_backtest",
        _fake_run_backtest_factory(record),
    )
    activity = tmp_path / "activity.json"
    activity.write_text("[]")
    rows = whale_pair_w1_calibrate.sweep(
        tmp_path / "fake.db",
        activity,
        base=WhalePairConfig(base_clip_usd=7.5),
        base_clip_usds=[],
        aggressive_clip_usds=[],
        max_seconds_from_start_values=[],
        max_imbalance_ratios=[],
        execution=whale_pair_w1_calibrate.ExecutionModel(),
    )
    assert record["calls"] == 1
    assert len(rows) == 1
    assert rows[0]["config"]["base_clip_usd"] == 7.5
