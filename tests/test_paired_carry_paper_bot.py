from __future__ import annotations

import os
import tempfile
from datetime import datetime, timedelta, timezone

import pytest

from scripts.paired_carry_paper_bot import (
    PairDecision,
    Window,
    already_recorded,
    best_ask_from_book,
    compute_pair_decision,
    in_entry_zone,
    init_db,
    record_decision,
    record_resolution,
)


def make_window() -> Window:
    start = datetime(2026, 1, 1, 0, 0, tzinfo=timezone.utc)
    return Window(
        asset="BTC",
        interval_label="5m",
        interval_minutes=5,
        market_id="m1",
        condition_id="c1",
        event_slug="btc-updown-5m-1767225600",
        event_id="e1",
        question="Bitcoin Up or Down - 5 Minutes",
        start_time=start,
        end_time=start + timedelta(minutes=5),
        up_token_id="UP",
        down_token_id="DOWN",
    )


def test_best_ask_from_book_picks_lowest_price():
    price, size = best_ask_from_book(
        {"asks": [{"price": "0.40", "size": "10"}, {"price": "0.33", "size": "4"}]}
    )
    assert price == 0.33
    assert size == 4.0


def test_compute_pair_decision_requires_positive_locked_edge():
    decision = compute_pair_decision(
        up_ask=0.50,
        down_ask=0.50,
        up_ask_size=100.0,
        down_ask_size=100.0,
        max_pair_cost=1.00,
        max_position_usd=25.0,
    )
    assert decision is None


def test_compute_pair_decision_sizes_to_depth_and_cap():
    decision = compute_pair_decision(
        up_ask=0.22,
        down_ask=0.62,
        up_ask_size=100.0,
        down_ask_size=5.0,
        max_pair_cost=0.97,
        max_position_usd=25.0,
    )
    assert decision is not None
    assert decision.matched_shares == pytest.approx(5.0)
    assert decision.gross_cost_usd == pytest.approx(5.0 * 0.84)
    assert decision.locked_pnl_usd > 0
    assert decision.locked_edge_pct > 0


def test_in_entry_zone_uses_seconds_from_start_band():
    window = make_window()
    assert not in_entry_zone(
        window,
        window.start_time + timedelta(seconds=30),
        min_seconds_from_start=60,
        max_seconds_from_start=240,
    )
    assert in_entry_zone(
        window,
        window.start_time + timedelta(seconds=120),
        min_seconds_from_start=60,
        max_seconds_from_start=240,
    )
    assert not in_entry_zone(
        window,
        window.start_time + timedelta(seconds=280),
        min_seconds_from_start=60,
        max_seconds_from_start=240,
    )


def test_record_decision_is_idempotent_by_market_id():
    tmp = tempfile.NamedTemporaryFile(suffix=".db", delete=False)
    tmp.close()
    try:
        conn = init_db(tmp.name)
        window = make_window()
        decision = PairDecision(
            up_ask=0.25,
            down_ask=0.60,
            up_ask_size=50.0,
            down_ask_size=10.0,
            matched_shares=10.0,
            gross_cost_usd=8.5,
            entry_fee_usd=0.08,
            locked_pnl_usd=1.42,
            locked_edge_pct=16.7,
        )
        record_decision(conn, window, decision, seconds_from_start=120, seconds_to_end=180)
        record_decision(conn, window, decision, seconds_from_start=121, seconds_to_end=179)
        count = conn.execute(
            "SELECT COUNT(*) FROM paired_carry_paper_trades WHERE market_id='m1'"
        ).fetchone()[0]
        assert count == 1
        assert already_recorded(conn, "m1")
        conn.close()
    finally:
        os.unlink(tmp.name)


def test_record_resolution_marks_trade_resolved():
    tmp = tempfile.NamedTemporaryFile(suffix=".db", delete=False)
    tmp.close()
    try:
        conn = init_db(tmp.name)
        window = make_window()
        decision = PairDecision(
            up_ask=0.25,
            down_ask=0.60,
            up_ask_size=50.0,
            down_ask_size=10.0,
            matched_shares=10.0,
            gross_cost_usd=8.5,
            entry_fee_usd=0.08,
            locked_pnl_usd=1.42,
            locked_edge_pct=16.7,
        )
        record_decision(conn, window, decision, seconds_from_start=120, seconds_to_end=180)
        record_resolution(
            conn,
            "m1",
            winning_outcome="Down",
            payout_usd=10.0,
            realized_pnl_usd=1.42,
        )
        row = conn.execute(
            "SELECT resolved, winning_outcome, payout_usd, realized_pnl_usd "
            "FROM paired_carry_paper_trades WHERE market_id='m1'"
        ).fetchone()
        assert row == (1, "Down", pytest.approx(10.0), pytest.approx(1.42))
        conn.close()
    finally:
        os.unlink(tmp.name)
