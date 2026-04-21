"""Integration test: push vs poll for fill detection.

Asserts:
  - A user WS trade event sets the same engine state as a REST poll.
  - Push is faster than the default poll interval (we use a slow poll
    fixture so the push path demonstrably wins).
"""
from __future__ import annotations

import asyncio
import time
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from unittest.mock import AsyncMock, MagicMock

import pytest

from core.engine import BTCTradingEngine


@dataclass
class _PaperCfg:
    enabled: bool
    starting_balance: float = 100.0


@dataclass
class _RootCfg:
    paper: _PaperCfg


class _FakeWindow:
    def __init__(self, seconds_until_close: float) -> None:
        self.end_time = datetime.now(timezone.utc) + timedelta(
            seconds=seconds_until_close
        )


def _make_engine(polymarket: object, *, poll_interval: float = 2.0):
    eng = BTCTradingEngine.__new__(BTCTradingEngine)
    eng.cfg = _RootCfg(paper=_PaperCfg(enabled=False))
    eng._polymarket = polymarket
    eng.risk = MagicMock()
    eng.current_window = _FakeWindow(300.0)
    eng._already_traded_this_window = True
    eng._order_poll_interval_s = poll_interval
    eng._order_fill_deadline_buffer_s = 30.0
    eng._push_order_events = {}
    eng._push_order_snapshots = {}
    eng._user_ws = None
    eng._resolution_ws = None
    eng._push_resolved_condition_ids = set()
    return eng


@pytest.mark.asyncio
async def test_push_fill_matches_poll_state_and_is_faster():
    """Two engines submit the same order. One gets a push event after
    100ms; the other can only learn via the 2s REST poll. Both must
    end with identical order_status / shares / size_usd, but the push
    path must return meaningfully sooner."""

    # --- Push path ------------------------------------------------------
    poly_push = MagicMock()
    poly_push.place_order = AsyncMock(
        return_value={"orderID": "ord-push", "status": "live"}
    )
    # Poll would say LIVE; push fires MATCHED first.
    poly_push.get_order_status = AsyncMock(
        return_value={"id": "ord-push", "status": "LIVE", "size_matched": "0"}
    )
    poly_push.cancel_order = AsyncMock()

    eng_push = _make_engine(poly_push, poll_interval=2.0)

    async def fire_push_after(delay_s: float):
        await asyncio.sleep(delay_s)
        await eng_push._on_user_ws_event({
            "event_type": "trade",
            "type": "TRADE",
            "id": "trade-1",
            "taker_order_id": "ord-push",
            "market": "0xcond",
            "asset_id": "tok",
            "side": "BUY",
            "size": "100",
            "price": "0.05",
            "status": "MATCHED",
            "owner": "0xme",
            "timestamp": "1",
        })

    t_push = {
        "token_id": "tok", "token_price": 0.05, "direction": "UP",
        "shares": 100.0, "size_usd": 5.0,
    }
    push_task = asyncio.create_task(fire_push_after(0.1))
    start = time.monotonic()
    out_push = await eng_push._submit_and_confirm_live_order(t_push)
    push_elapsed = time.monotonic() - start
    await push_task

    # --- Poll path ------------------------------------------------------
    poly_poll = MagicMock()
    poly_poll.place_order = AsyncMock(
        return_value={"orderID": "ord-poll", "status": "live"}
    )
    # First poll (immediately) returns LIVE; second (after 2s sleep)
    # returns MATCHED.
    poll_responses = [
        {"id": "ord-poll", "status": "LIVE", "size_matched": "0"},
        {"id": "ord-poll", "status": "MATCHED", "size_matched": "100"},
    ]
    poly_poll.get_order_status = AsyncMock(side_effect=poll_responses)
    poly_poll.cancel_order = AsyncMock()

    eng_poll = _make_engine(poly_poll, poll_interval=2.0)
    t_poll = {
        "token_id": "tok", "token_price": 0.05, "direction": "UP",
        "shares": 100.0, "size_usd": 5.0,
    }
    start = time.monotonic()
    out_poll = await eng_poll._submit_and_confirm_live_order(t_poll)
    poll_elapsed = time.monotonic() - start

    # Both paths report the same terminal state.
    assert out_push is not None
    assert out_poll is not None
    assert out_push["order_status"] == out_poll["order_status"] == "matched"
    assert out_push["shares"] == out_poll["shares"] == pytest.approx(100.0)
    assert out_push["size_usd"] == out_poll["size_usd"] == pytest.approx(5.0)

    # Push is faster than one full poll interval.
    assert push_elapsed < 1.0, (
        f"push path took {push_elapsed:.2f}s, expected <1.0s"
    )
    assert poll_elapsed >= 1.5, (
        f"poll path took {poll_elapsed:.2f}s, expected >=1.5s "
        "(at least one full poll interval)"
    )
    assert push_elapsed < poll_elapsed


@pytest.mark.asyncio
async def test_push_cancellation_event_unblocks_poll_loop():
    """Order WS event with type=CANCELLATION should unblock the poll loop
    and return cancelled without waiting for the REST poll."""
    poly = MagicMock()
    poly.place_order = AsyncMock(
        return_value={"orderID": "ord-x", "status": "live"}
    )
    # REST always says LIVE; without push we'd time out.
    poly.get_order_status = AsyncMock(
        return_value={"id": "ord-x", "status": "LIVE", "size_matched": "0"}
    )
    poly.cancel_order = AsyncMock()

    eng = _make_engine(poly, poll_interval=5.0)

    async def fire_cancel():
        await asyncio.sleep(0.05)
        await eng._on_user_ws_event({
            "event_type": "order",
            "id": "ord-x",
            "owner": "0xme",
            "market": "0xcond",
            "asset_id": "tok",
            "side": "BUY",
            "original_size": "100",
            "size_matched": "0",
            "price": "0.05",
            "type": "CANCELLATION",
            "status": "CANCELED",
            "timestamp": "1",
        })

    t = {
        "token_id": "tok", "token_price": 0.05, "direction": "UP",
        "shares": 100.0, "size_usd": 5.0,
    }
    fire_task = asyncio.create_task(fire_cancel())
    start = time.monotonic()
    out = await eng._submit_and_confirm_live_order(t)
    elapsed = time.monotonic() - start
    await fire_task

    # Cancelled via push: no fill recorded.
    assert out is None
    # Without the push path we'd be waiting 5s for the next poll; the
    # push must wake us well under that.
    assert elapsed < 1.0


@pytest.mark.asyncio
async def test_resolution_push_annotates_open_trade():
    """A resolution push on our conditionId sets pending_push_resolution
    on the matching open trade. No premature resolution record -- the
    authoritative writer stays Gamma polling."""
    eng = BTCTradingEngine.__new__(BTCTradingEngine)
    eng._paper_trades = [
        {
            "market_id": "42",
            "condition_id": "0x" + "ab" * 32,
            "direction": "UP",
            "token_price": 0.05,
            "shares": 100.0,
            "size_usd": 5.0,
        }
    ]
    eng._push_resolved_condition_ids = set()

    await eng._on_resolution_push(
        "0x" + "AB" * 32, "YES", [1, 0]
    )

    trade = eng._paper_trades[0]
    assert trade.get("pending_push_resolution") is True
    assert trade.get("push_resolution_winner") == "YES"
    # Idempotent on second fire.
    await eng._on_resolution_push("0x" + "ab" * 32, "YES", [1, 0])
    assert len(eng._push_resolved_condition_ids) == 1
