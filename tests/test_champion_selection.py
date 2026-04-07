from __future__ import annotations

from types import SimpleNamespace

from autoresearch.champion import _price_bucket_penalty, _recent_manifest


def test_recent_manifest_uses_tail_slice():
    manifest = [SimpleNamespace(market_id=str(i), winner="Up", offset=i, length=100) for i in range(10)]
    recent = _recent_manifest(manifest, fraction=0.2, min_markets=3)
    assert [item.market_id for item in recent] == ["7", "8", "9"]


def test_price_bucket_penalty_flags_losing_cheap_concentration():
    buckets = {
        "0.10-0.25": {"trades": 4, "pnl_per_trade": -0.4},
        "0.25-0.40": {"trades": 4, "pnl_per_trade": -0.2},
        "0.40-0.60": {"trades": 2, "pnl_per_trade": 0.3},
    }
    penalty, notes = _price_bucket_penalty(buckets)
    assert penalty > 0
    assert notes


def test_price_bucket_penalty_ignores_healthy_distribution():
    buckets = {
        "0.10-0.25": {"trades": 2, "pnl_per_trade": 0.1},
        "0.40-0.60": {"trades": 8, "pnl_per_trade": 0.3},
    }
    penalty, notes = _price_bucket_penalty(buckets)
    assert penalty == 0
    assert notes == []
