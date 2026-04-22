from __future__ import annotations

import os
import tempfile

import pytest

from scripts.whale_pair_paper_bot import (
    OpenInventory,
    choose_accumulate_clip_usd,
    choose_fill_shares,
    completion_pnl_per_share,
    init_db,
    insert_fill,
    match_and_merge,
    open_inventory,
)


def test_choose_accumulate_clip_usd_uses_aggressive_band():
    assert choose_accumulate_clip_usd(
        0.08,
        accumulate_price_max=0.50,
        aggressive_price_max=0.10,
        base_clip_usd=10.0,
        aggressive_clip_usd=25.0,
    ) == 25.0
    assert choose_accumulate_clip_usd(
        0.30,
        accumulate_price_max=0.50,
        aggressive_price_max=0.10,
        base_clip_usd=10.0,
        aggressive_clip_usd=25.0,
    ) == 10.0
    assert choose_accumulate_clip_usd(
        0.80,
        accumulate_price_max=0.50,
        aggressive_price_max=0.10,
        base_clip_usd=10.0,
        aggressive_clip_usd=25.0,
    ) is None


def test_completion_pnl_per_share_positive_for_cheap_opposite_leg():
    pnl = completion_pnl_per_share(0.20, 0.75)
    assert pnl is not None
    assert pnl > 0


def test_choose_fill_shares_respects_budget_clip_and_depth():
    shares = choose_fill_shares(price=0.5, ask_size=100.0, clip_usd=10.0, remaining_budget_usd=3.0)
    assert shares == pytest.approx(6.0)


def test_match_and_merge_reduces_open_inventory_and_realizes_pair():
    tmp = tempfile.NamedTemporaryFile(suffix=".db", delete=False)
    tmp.close()
    try:
        conn = init_db(tmp.name)
        insert_fill(conn, "m1", "Up", "accumulate", 0.25, 100.0, 10.0)
        insert_fill(conn, "m1", "Down", "accumulate", 0.60, 100.0, 4.0)
        matches = match_and_merge(conn, "m1")
        assert matches == 1
        up_open = open_inventory(conn, "m1", "Up")
        down_open = open_inventory(conn, "m1", "Down")
        assert up_open.shares == pytest.approx(6.0)
        assert down_open.shares == pytest.approx(0.0)
        pnl = conn.execute(
            "SELECT realized_pnl_usd FROM whale_pair_matches WHERE market_id = 'm1'"
        ).fetchone()[0]
        assert pnl > 0
        conn.close()
    finally:
        os.unlink(tmp.name)
