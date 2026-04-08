"""End-to-end integration test for feature_store regime layer.

Builds a synthetic sqlite db with the framework's standard schema,
populates it with deterministic markets and snapshots, runs
build_feature_store, then asserts that the regime feature arrays
have the correct values market-by-market — including the look-ahead
safety property that market i's regime features depend only on
markets [0, i-1].
"""
from __future__ import annotations

import json
import math
import sqlite3
from datetime import datetime, timedelta, timezone
from pathlib import Path

import numpy as np
import pytest

from backtesting.eval.feature_store import (
    REGIME_FEATURE_NAMES,
    build_feature_store,
    load_feature_store,
)
from shared.db import init_coin_db


@pytest.fixture
def synthetic_db(tmp_path: Path) -> Path:
    """Create a coin db with 50 5-min markets, alternating Up/Down,
    each with 60 snapshots and orderbook columns populated."""
    db_path = tmp_path / "btc.db"
    conn = init_coin_db(db_path)
    base = datetime(2026, 4, 1, 0, 0, 0, tzinfo=timezone.utc)

    for i in range(50):
        start = base + timedelta(minutes=5 * i)
        end = start + timedelta(minutes=5)
        winner = "Up" if i % 2 == 0 else "Down"
        # Synthetic underlying: 100.0 + i * 0.1 with a small per-market move
        price_start = 100.0 + i * 0.1
        price_end = price_start * (1.001 if winner == "Up" else 0.999)
        market_id = f"m{i:04d}"
        conn.execute(
            "INSERT INTO markets "
            "(market_id, slug, market_type, start_time, end_time, "
            "price_start, price_end, winner, final_volume, final_liquidity) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (market_id, f"slug-{i}", "5m",
             start.isoformat(), end.isoformat(),
             price_start, price_end, winner, 1000.0, 500.0),
        )
        # 60 snapshots per market
        for j in range(60):
            ts = start + timedelta(seconds=j * 5)
            mid_up = 0.5 + (0.01 if winner == "Up" else -0.01)
            mid_down = 1.0 - mid_up
            conn.execute(
                "INSERT INTO snapshots VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (market_id, ts.isoformat(),
                 price_start + (price_end - price_start) * (j / 60),  # underlying
                 mid_up, mid_down,
                 mid_up - 0.005, mid_up + 0.005, 100.0, 100.0,  # up book
                 mid_down - 0.005, mid_down + 0.005, 100.0, 100.0,  # down book
                 None, None),  # json columns
            )
    conn.commit()
    conn.close()
    return db_path


def test_regime_features_built_correctly(synthetic_db: Path, tmp_path: Path, monkeypatch):
    # Build feature store into a temp dir to avoid polluting real
    # backtesting/eval/features/
    from backtesting.eval import feature_store as fs
    monkeypatch.setattr(fs, "FEATURES_DIR", tmp_path / "features")

    n_markets = build_feature_store(synthetic_db, "btc", lag_n=10)
    assert n_markets == 50

    manifest, features = load_feature_store("btc")

    # All 8 regime features should exist as arrays
    for name in REGIME_FEATURE_NAMES:
        assert name in features, f"missing regime feature {name}"
        assert features[name].dtype == np.float64

    # Look-ahead safety: market 0 should have NaN for all regime
    # features that depend on the lag window (n_past_markets=0)
    m0 = manifest[0]
    s0 = features["n_past_markets"][m0.offset]
    assert s0 == 0
    assert math.isnan(features["lag_n_up_frac"][m0.offset])
    assert math.isnan(features["lag_n_mean_abs_move"][m0.offset])

    # Market 1 should have window of [m0] only (1 past market)
    m1 = manifest[1]
    assert features["n_past_markets"][m1.offset] == 1
    # m0 was Up (i=0 even), so lag_n_up_frac for m1 = 1/1 = 1.0
    assert features["lag_n_up_frac"][m1.offset] == pytest.approx(1.0)

    # Market 2 should have window of [m0, m1]: m0 Up, m1 Down -> 0.5
    m2 = manifest[2]
    assert features["n_past_markets"][m2.offset] == 2
    assert features["lag_n_up_frac"][m2.offset] == pytest.approx(0.5)

    # Market 11 should have a full lag_n=10 window
    m11 = manifest[11]
    assert features["n_past_markets"][m11.offset] == 10
    # Markets 1-10: indices 1,2,3,...,10 -> winners alternate Down/Up/Down/Up/...
    # i=1 Down, i=2 Up, i=3 Down, i=4 Up, ..., i=10 Up
    # Total Up in window: 5, Down: 5 -> up_frac = 0.5
    assert features["lag_n_up_frac"][m11.offset] == pytest.approx(0.5)

    # Hour of day should be the actual hour from start_time
    # Market 0 starts at 00:00 -> hour 0
    assert features["hour_of_day"][m0.offset] == 0
    # Market 12 starts at 01:00 (12 * 5 min after 00:00) -> hour 1
    m12 = manifest[12]
    assert features["hour_of_day"][m12.offset] == 1


def test_regime_features_constant_within_market(synthetic_db: Path, tmp_path: Path, monkeypatch):
    """Within a market, every snapshot should see the same regime
    feature values (broadcast to per-snapshot array)."""
    from backtesting.eval import feature_store as fs
    monkeypatch.setattr(fs, "FEATURES_DIR", tmp_path / "features")
    build_feature_store(synthetic_db, "btc", lag_n=10)
    manifest, features = load_feature_store("btc")

    for m in manifest[5:15]:  # check several markets
        slice_ = slice(m.offset, m.offset + m.length)
        for name in REGIME_FEATURE_NAMES:
            arr = features[name][slice_]
            unique_vals = np.unique(arr[~np.isnan(arr)]) if not np.all(np.isnan(arr)) else [np.nan]
            # Should be either all NaN (degenerate window) or one unique value
            if not np.all(np.isnan(arr)):
                assert len(unique_vals) == 1, (
                    f"market {m.market_id} feature {name} has multiple values: "
                    f"{unique_vals}"
                )


def test_orderbook_and_computed_features_still_work(synthetic_db: Path, tmp_path: Path, monkeypatch):
    """The regime layer addition should not break existing features."""
    from backtesting.eval import feature_store as fs
    monkeypatch.setattr(fs, "FEATURES_DIR", tmp_path / "features")
    build_feature_store(synthetic_db, "btc", lag_n=10)
    manifest, features = load_feature_store("btc")

    # Sanity check on a couple of existing features
    assert "move_pct" in features
    assert "best_ask_up" in features
    assert features["move_pct"].dtype == np.float64
    assert len(features["move_pct"]) == sum(m.length for m in manifest)
    # best_ask_up should be populated (we set it in the synthetic data)
    assert not np.any(np.isnan(features["best_ask_up"][:60]))
