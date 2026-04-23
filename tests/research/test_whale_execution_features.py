from __future__ import annotations

from scripts.infer_whale_execution_features import (
    classify_execution,
    classify_fill_quality,
    enrich_row,
    summarize,
    window_bucket,
)


def _trade_row(**overrides):
    row = {
        "type": "TRADE",
        "side": "BUY",
        "outcome": "Up",
        "fill_price": 0.51,
        "chosen_best_ask": 0.51,
        "chosen_best_bid": 0.49,
        "ask_size_up": 20.0,
        "bid_size_up": 18.0,
        "seconds_from_start": 75,
        "ask_sum": 0.99,
        "slug": "btc-updown-5m-1",
        "size": 10.0,
    }
    row.update(overrides)
    return row


def test_classify_execution_buy_taker_at_ask():
    assert classify_execution(_trade_row()) == "likely_taker"


def test_classify_execution_buy_passive_inside_ask():
    assert classify_execution(_trade_row(fill_price=0.505)) == "likely_maker_or_passive"


def test_classify_fill_quality_sell_inside_bid():
    row = _trade_row(
        side="SELL",
        fill_price=0.50,
        chosen_best_bid=0.49,
        chosen_best_ask=0.51,
        bid_size_up=15.0,
    )
    assert classify_fill_quality(row) == "inside_or_better_than_bid"


def test_window_bucket_boundaries():
    assert window_bucket(None) == "unknown"
    assert window_bucket(-1) == "pre_window"
    assert window_bucket(59) == "0_60"
    assert window_bucket(60) == "60_120"
    assert window_bucket(301) == "late"


def test_enrich_row_adds_depth_ratio_and_negative_risk_flag():
    enriched = enrich_row(_trade_row())
    assert enriched["window_bucket"] == "60_120"
    assert enriched["execution_class"] == "likely_taker"
    assert enriched["touch_depth_ratio"] == 0.5
    assert enriched["likely_negative_risk_window"] is True


def test_summarize_counts_pair_windows_and_execution_mix():
    rows = [
        enrich_row(_trade_row(slug="m1", outcome="Up")),
        enrich_row(_trade_row(slug="m1", outcome="Down", ask_size_down=10.0)),
        enrich_row(_trade_row(slug="m2", fill_price=0.505)),
    ]
    summary = summarize(rows)
    assert summary["trade_rows"] == 3
    assert summary["pair_windows"] == 1
    assert summary["execution_class_counts"]["likely_taker"] == 2
    assert summary["execution_class_counts"]["likely_maker_or_passive"] == 1
