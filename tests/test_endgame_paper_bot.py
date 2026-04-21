"""Smoke tests for the endgame yield paper bot decision + P&L logic."""
from __future__ import annotations

import os
import sqlite3
import tempfile
from datetime import datetime, timezone

import pytest

from scripts.endgame_paper_bot import (
    Decision,
    Window,
    best_ask_from_book,
    choose_decision,
    compute_paper_pnl,
    init_db,
    record_decision,
    record_resolution,
    already_recorded,
)


def test_best_ask_from_book_picks_lowest_price():
    book = {
        "asks": [
            {"price": "0.99", "size": "100"},
            {"price": "0.97", "size": "5"},
            {"price": "0.98", "size": "20"},
        ]
    }
    price, size = best_ask_from_book(book)
    assert price == 0.97
    assert size == 5.0


def test_best_ask_from_book_empty():
    assert best_ask_from_book({"asks": []}) == (None, None)
    assert best_ask_from_book({}) == (None, None)


def test_choose_decision_no_edge_when_asks_too_high():
    up_book = {"asks": [{"price": "0.98", "size": "50"}]}
    down_book = {"asks": [{"price": "0.99", "size": "50"}]}
    d = choose_decision(
        up_book=up_book,
        down_book=down_book,
        max_ask_threshold=0.97,
        max_position_usd=5.0,
        up_token_id="UP",
        down_token_id="DOWN",
    )
    assert d is None


def test_choose_decision_picks_lower_ask_side():
    up_book = {"asks": [{"price": "0.95", "size": "10"}]}
    down_book = {"asks": [{"price": "0.97", "size": "20"}]}
    d = choose_decision(
        up_book=up_book,
        down_book=down_book,
        max_ask_threshold=0.97,
        max_position_usd=5.0,
        up_token_id="UP",
        down_token_id="DOWN",
    )
    assert d is not None
    assert d.side == "UP"
    assert d.token_id == "UP"
    assert d.best_ask == 0.95
    assert d.size_usd == pytest.approx(5.0)
    assert d.intended_shares == pytest.approx(5.0 / 0.95)


def test_choose_decision_depth_caps_size():
    up_book = {"asks": [{"price": "0.96", "size": "3"}]}
    down_book = {"asks": [{"price": "0.99", "size": "100"}]}
    d = choose_decision(
        up_book=up_book,
        down_book=down_book,
        max_ask_threshold=0.97,
        max_position_usd=5.0,
        up_token_id="UP",
        down_token_id="DOWN",
    )
    assert d is not None
    assert d.side == "UP"
    assert d.size_usd == pytest.approx(0.96 * 3)
    assert d.intended_shares == pytest.approx(3.0)


def test_compute_paper_pnl_win_includes_both_fees():
    # Gross edge vs fees: at ask 0.96 + 0.005 slip, effective entry = 0.965,
    # gross win margin = (1 - 0.965) = 0.035 (~3.5%). With 2% entry + 2% exit
    # taker fees on a win, fees consume ~4% of cost, which can push P&L
    # negative at this price level. The formula still returns the expected
    # analytic value; the sign depends on whether gross margin > fee load.
    shares = 5.0 / 0.96
    cost = shares * (0.96 + 0.005)
    pnl_win = compute_paper_pnl(cost, size_usd=5.0, shares=shares, won=True, fee_rate=0.02)
    expected_payout = shares * 1.0
    expected = expected_payout - cost - cost * 0.02 - expected_payout * 0.02
    assert pnl_win == pytest.approx(expected)


def test_compute_paper_pnl_win_profitable_at_aggressive_entry():
    # Lower ask (0.92) gives enough gross margin to clear 2% fees both ways.
    shares = 5.0 / 0.92
    cost = shares * (0.92 + 0.005)
    pnl_win = compute_paper_pnl(cost, size_usd=5.0, shares=shares, won=True, fee_rate=0.02)
    assert pnl_win > 0


def test_compute_paper_pnl_loss_is_total_cost_plus_entry_fee():
    shares = 5.0
    cost = 5.0 * 0.965
    pnl_loss = compute_paper_pnl(cost, size_usd=5.0, shares=shares, won=False, fee_rate=0.02)
    assert pnl_loss == pytest.approx(-(cost + cost * 0.02))
    assert pnl_loss < 0


def test_record_decision_is_idempotent_on_market_id():
    tmp = tempfile.NamedTemporaryFile(suffix=".db", delete=False)
    tmp.close()
    try:
        conn = init_db(tmp.name)
        w = Window(
            market_id="m1",
            condition_id="c1",
            event_slug="btc-updown-5m-1700000000",
            event_id="e1",
            question="Bitcoin Up or Down - Test",
            start_time=datetime(2026, 1, 1, 0, 0, tzinfo=timezone.utc),
            end_time=datetime(2026, 1, 1, 0, 5, tzinfo=timezone.utc),
            up_token_id="UP",
            down_token_id="DOWN",
        )
        d = Decision(
            token_id="UP",
            side="UP",
            best_ask=0.96,
            best_ask_size=20.0,
            intended_shares=5.0,
            size_usd=4.8,
        )
        record_decision(conn, w, d, seconds_remaining=30.0)
        assert already_recorded(conn, "m1")
        # Second call should not insert a duplicate row.
        record_decision(conn, w, d, seconds_remaining=25.0)
        count = conn.execute(
            "SELECT COUNT(*) FROM endgame_paper_trades WHERE market_id='m1'"
        ).fetchone()[0]
        assert count == 1
        conn.close()
    finally:
        os.unlink(tmp.name)


def test_record_resolution_updates_row():
    tmp = tempfile.NamedTemporaryFile(suffix=".db", delete=False)
    tmp.close()
    try:
        conn = init_db(tmp.name)
        w = Window(
            market_id="m2",
            condition_id="c2",
            event_slug="btc-updown-5m-1700000300",
            event_id="e2",
            question="q",
            start_time=datetime(2026, 1, 1, 0, 5, tzinfo=timezone.utc),
            end_time=datetime(2026, 1, 1, 0, 10, tzinfo=timezone.utc),
            up_token_id="UP",
            down_token_id="DOWN",
        )
        d = Decision(
            token_id="UP",
            side="UP",
            best_ask=0.96,
            best_ask_size=20.0,
            intended_shares=5.0,
            size_usd=4.8,
        )
        record_decision(conn, w, d, seconds_remaining=30.0)
        record_resolution(conn, "m2", won=True, observed_direction="UP", pnl_usd=0.15)
        row = conn.execute(
            "SELECT resolved, won, observed_direction, pnl_usd "
            "FROM endgame_paper_trades WHERE market_id='m2'"
        ).fetchone()
        assert row == (1, 1, "UP", pytest.approx(0.15))
        conn.close()
    finally:
        os.unlink(tmp.name)
