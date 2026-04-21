"""Tests for scripts/pnl_report.py aggregation logic.

Covers the two bugs tracked by issue #17:

1. Redeemed losers (REDEEM event with usdcSize=0) must appear in closed
   rows; the old filter ``payout > 0 AND cost > 0`` silently dropped
   them and produced reports like "3 wins / 0 losses".

2. Position cost basis must come from summed /activity BUY rows (real
   deployed capital) rather than /positions.initialValue (shares x
   avgPrice, rounded). UI and /activity agreed on the true number
   while /positions differed by a few cents per fill.
"""

from __future__ import annotations

from decimal import Decimal

from scripts.pnl_report import (
    aggregate_activity_by_condition,
    build_closed_rows_from_activity,
    build_position_rows,
)


COND_WIN = "0x" + "11" * 32
COND_LOSS = "0x" + "22" * 32
COND_OPEN = "0x" + "33" * 32


def _buy(cid: str, usdc: float, title: str = "") -> dict:
    return {
        "conditionId": cid,
        "type": "TRADE",
        "side": "BUY",
        "usdcSize": usdc,
        "title": title,
    }


def _redeem(cid: str, usdc: float, title: str = "") -> dict:
    return {
        "conditionId": cid,
        "type": "REDEEM",
        "usdcSize": usdc,
        "title": title,
    }


def test_redeemed_loser_included_despite_zero_payout():
    """Bug 1: REDEEM with usdcSize=0 still produces a closed row."""
    activity = [
        _buy(COND_LOSS, 4.24, "market A"),
        _redeem(COND_LOSS, 0, "market A"),
    ]
    closed = build_closed_rows_from_activity(activity)
    assert len(closed) == 1
    row = closed[0]
    assert row.condition_id == COND_LOSS
    assert row.cost_usdc == Decimal("4.24")
    assert row.payout_usdc == Decimal("0")
    assert row.pnl_usdc == Decimal("-4.24")


def test_redeemed_winner_included():
    activity = [
        _buy(COND_WIN, 5.68, "market B"),
        _redeem(COND_WIN, 14.28, "market B"),
    ]
    closed = build_closed_rows_from_activity(activity)
    assert len(closed) == 1
    row = closed[0]
    assert row.cost_usdc == Decimal("5.68")
    assert row.payout_usdc == Decimal("14.28")
    assert row.pnl_usdc == Decimal("8.60")


def test_open_position_without_redeem_is_excluded_from_closed():
    """BUY with no REDEEM event is an open position, not closed."""
    activity = [_buy(COND_OPEN, 3.00, "open")]
    closed = build_closed_rows_from_activity(activity)
    assert closed == []


def test_cost_basis_from_activity_preferred_over_positions_initialvalue():
    """Bug 2: /positions.initialValue is rounded; use /activity BUYs."""
    # /positions reports rounded initialValue; /activity reports the
    # real deployed amount. Use the latter when available.
    activity = [
        _buy(COND_OPEN, 5.94, "market"),
    ]
    positions = [
        {
            "conditionId": COND_OPEN,
            "title": "market",
            "outcome": "Up",
            "size": "10",
            "avgPrice": "0.571",
            "initialValue": "5.71",
            "curPrice": "0.55",
            "currentValue": "5.50",
            "cashPnl": "-0.21",
            "redeemable": False,
        }
    ]
    by_cid = aggregate_activity_by_condition(activity)
    rows = build_position_rows(positions, by_cid)
    assert len(rows) == 1
    r = rows[0]
    assert r.cost_usdc == Decimal("5.94")
    # P&L re-derived against the accurate cost for internal consistency
    # (value - cost, not the stale cashPnl from /positions).
    assert r.pnl_usdc == Decimal("5.50") - Decimal("5.94")


def test_cost_basis_falls_back_to_initialvalue_when_no_activity():
    """If there is no matching /activity row, keep /positions value."""
    positions = [
        {
            "conditionId": COND_OPEN,
            "title": "market",
            "outcome": "Up",
            "size": "10",
            "avgPrice": "0.50",
            "initialValue": "5.00",
            "curPrice": "0.50",
            "currentValue": "5.00",
            "cashPnl": "0",
            "redeemable": False,
        }
    ]
    rows = build_position_rows(positions, activity_by_cid=None)
    assert len(rows) == 1
    assert rows[0].cost_usdc == Decimal("5.00")


def test_multiple_fills_aggregate_into_single_cost():
    """Multiple partial fills on the same market sum to one cost."""
    activity = [
        _buy(COND_LOSS, 1.6965, "split fills"),
        _buy(COND_LOSS, 0.9831, "split fills"),
        _buy(COND_LOSS, 2.32, "split fills"),
        _redeem(COND_LOSS, 0, "split fills"),
    ]
    closed = build_closed_rows_from_activity(activity)
    assert len(closed) == 1
    # Sum is exact in Decimal arithmetic.
    assert closed[0].cost_usdc == Decimal("1.6965") + Decimal("0.9831") + Decimal("2.32")


def test_mixed_activity_produces_correct_win_loss_counts():
    """End-to-end: 2 wins, 2 losses from activity, one open position."""
    activity = [
        _buy(COND_WIN, 5.0, "win1"),
        _redeem(COND_WIN, 10.0, "win1"),
        _buy("0x" + "44" * 32, 3.0, "win2"),
        _redeem("0x" + "44" * 32, 6.0, "win2"),
        _buy(COND_LOSS, 4.0, "loss1"),
        _redeem(COND_LOSS, 0, "loss1"),
        _buy("0x" + "55" * 32, 2.0, "loss2"),
        _redeem("0x" + "55" * 32, 0, "loss2"),
        _buy(COND_OPEN, 1.5, "open"),
    ]
    closed = build_closed_rows_from_activity(activity)
    wins = [r for r in closed if r.payout_usdc > 0]
    losses = [r for r in closed if r.payout_usdc == 0 and r.cost_usdc > 0]
    assert len(wins) == 2
    assert len(losses) == 2
    # Open position (no REDEEM) is correctly excluded from closed rows.
    assert not any(r.condition_id == COND_OPEN for r in closed)


def test_aggregate_ignores_sell_trades_for_cost():
    """Secondary-market SELLs are not BUYs and must not reduce cost."""
    activity = [
        _buy(COND_OPEN, 4.0, "m"),
        {
            "conditionId": COND_OPEN,
            "type": "TRADE",
            "side": "SELL",
            "usdcSize": 3.5,
            "title": "m",
        },
    ]
    by_cid = aggregate_activity_by_condition(activity)
    assert by_cid[COND_OPEN]["cost"] == Decimal("4.0")
