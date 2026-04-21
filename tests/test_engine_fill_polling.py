"""Tests for BTCTradingEngine live order-status polling.

The engine now polls py_clob_client's get_order endpoint after placing a
live order, rather than blindly recording the trade. These tests exercise
the four terminal paths: instant match, delayed match, timeout+cancel,
and rejection, using AsyncMock in place of PolymarketClient and
BTCTradingEngine.__new__ to skip heavy init.
"""

from __future__ import annotations

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
    """Minimal MarketWindow stand-in (only end_time is used)."""

    def __init__(self, seconds_until_close: float) -> None:
        self.end_time = datetime.now(timezone.utc) + timedelta(
            seconds=seconds_until_close
        )


def _make_engine(
    polymarket: object,
    *,
    window_secs_remaining: float | None = 120.0,
    poll_interval: float = 0.01,
    deadline_buffer: float = 30.0,
) -> BTCTradingEngine:
    """Stub just enough of BTCTradingEngine to drive the live-order path."""
    eng = BTCTradingEngine.__new__(BTCTradingEngine)
    eng.cfg = _RootCfg(paper=_PaperCfg(enabled=False))
    eng._polymarket = polymarket
    eng.risk = MagicMock()
    eng.current_window = (
        _FakeWindow(window_secs_remaining)
        if window_secs_remaining is not None
        else None
    )
    eng._already_traded_this_window = True
    eng._order_poll_interval_s = poll_interval
    eng._order_fill_deadline_buffer_s = deadline_buffer
    return eng


def _new_trade(shares: float = 100.0, size_usd: float = 5.0) -> dict:
    return {
        "token_id": "tok-abc",
        "token_price": 0.05,
        "direction": "UP",
        "shares": shares,
        "size_usd": size_usd,
    }


@pytest.mark.asyncio
async def test_instant_match_skips_polling():
    """status=matched on submit → record trade, no get_order call."""
    poly = MagicMock()
    poly.place_order = AsyncMock(
        return_value={"orderID": "ord-1", "status": "matched"}
    )
    poly.get_order_status = AsyncMock()
    poly.cancel_order = AsyncMock()

    eng = _make_engine(poly)
    t = _new_trade()
    out = await eng._submit_and_confirm_live_order(t)

    assert out is t
    assert out["order_id"] == "ord-1"
    assert out["order_status"] == "matched"
    assert out["shares"] == 100.0
    assert out["size_usd"] == pytest.approx(5.0)
    poly.get_order_status.assert_not_called()
    poly.cancel_order.assert_not_called()
    eng.risk.record_order_success.assert_called_once()
    eng.risk.record_order_rejection.assert_not_called()


@pytest.mark.asyncio
async def test_delayed_match_records_after_polling():
    """place_order returns live, then get_order_status flips to MATCHED."""
    poly = MagicMock()
    poly.place_order = AsyncMock(
        return_value={"orderID": "ord-2", "status": "live"}
    )
    # First two polls: still live. Third: matched with full size.
    poly.get_order_status = AsyncMock(
        side_effect=[
            {"id": "ord-2", "status": "LIVE", "size_matched": "0"},
            {"id": "ord-2", "status": "LIVE", "size_matched": "0"},
            {"id": "ord-2", "status": "MATCHED", "size_matched": "100"},
        ]
    )
    poly.cancel_order = AsyncMock()

    eng = _make_engine(poly)
    t = _new_trade()
    out = await eng._submit_and_confirm_live_order(t)

    assert out is t
    assert out["order_status"] == "matched"
    assert out["shares"] == 100.0
    assert out["size_usd"] == pytest.approx(5.0)
    assert poly.get_order_status.await_count == 3
    poly.cancel_order.assert_not_called()
    eng.risk.record_order_success.assert_called_once()
    eng.risk.record_order_rejection.assert_not_called()


@pytest.mark.asyncio
async def test_timeout_cancels_and_drops_trade():
    """Order stays LIVE; window closes before fill → cancel, no record."""
    poly = MagicMock()
    poly.place_order = AsyncMock(
        return_value={"orderID": "ord-3", "status": "live"}
    )
    poly.get_order_status = AsyncMock(
        return_value={"id": "ord-3", "status": "LIVE", "size_matched": "0"}
    )
    poly.cancel_order = AsyncMock(return_value={"ok": True})

    # Window is already inside the deadline buffer → next poll triggers cancel.
    eng = _make_engine(poly, window_secs_remaining=10.0, deadline_buffer=30.0)
    t = _new_trade()
    out = await eng._submit_and_confirm_live_order(t)

    assert out is None
    poly.cancel_order.assert_awaited_once_with("ord-3")
    eng.risk.record_order_rejection.assert_called_once()
    eng.risk.record_order_success.assert_not_called()
    # Window released for retry on the next snap.
    assert eng._already_traded_this_window is False


@pytest.mark.asyncio
async def test_rejection_on_submit_drops_trade():
    """place_order raises → no order id, risk.record_order_rejection, None."""
    poly = MagicMock()
    poly.place_order = AsyncMock(side_effect=RuntimeError("insufficient allowance"))
    poly.get_order_status = AsyncMock()
    poly.cancel_order = AsyncMock()

    eng = _make_engine(poly)
    t = _new_trade()
    out = await eng._submit_and_confirm_live_order(t)

    assert out is None
    poly.get_order_status.assert_not_called()
    poly.cancel_order.assert_not_called()
    eng.risk.record_order_rejection.assert_called_once()
    eng.risk.record_order_success.assert_not_called()
    assert eng._already_traded_this_window is False


@pytest.mark.asyncio
async def test_status_rejected_on_poll_drops_trade():
    """Polled status=FAILED → no position recorded, rejection counted."""
    poly = MagicMock()
    poly.place_order = AsyncMock(
        return_value={"orderID": "ord-4", "status": "live"}
    )
    poly.get_order_status = AsyncMock(
        return_value={"id": "ord-4", "status": "FAILED", "size_matched": "0"}
    )
    poly.cancel_order = AsyncMock()

    eng = _make_engine(poly)
    t = _new_trade()
    out = await eng._submit_and_confirm_live_order(t)

    assert out is None
    eng.risk.record_order_rejection.assert_called_once()
    eng.risk.record_order_success.assert_not_called()


@pytest.mark.asyncio
async def test_partial_fill_cancels_remainder_and_scales_trade():
    """Polled MATCHED with size_matched < shares → record the filled portion."""
    poly = MagicMock()
    poly.place_order = AsyncMock(
        return_value={"orderID": "ord-5", "status": "live"}
    )
    # Polled response: matched status but only 40 of 100 shares filled.
    poly.get_order_status = AsyncMock(
        return_value={
            "id": "ord-5",
            "status": "MATCHED",
            "size_matched": "40",
        }
    )
    poly.cancel_order = AsyncMock(return_value={"ok": True})

    eng = _make_engine(poly)
    t = _new_trade(shares=100.0, size_usd=5.0)
    out = await eng._submit_and_confirm_live_order(t)

    assert out is t
    assert out["order_status"] == "partial"
    assert out["shares"] == pytest.approx(40.0)
    assert out["size_usd"] == pytest.approx(2.0)  # 5.0 * 40/100
    poly.cancel_order.assert_awaited_once_with("ord-5")
    eng.risk.record_order_success.assert_called_once()
    eng.risk.record_order_rejection.assert_not_called()


@pytest.mark.asyncio
async def test_timeout_with_partial_fill_records_filled_portion():
    """Window closes with status=LIVE and size_matched>0 → record partial."""
    poly = MagicMock()
    poly.place_order = AsyncMock(
        return_value={"orderID": "ord-6", "status": "live"}
    )
    # Poll returns a live order that has partially filled.
    poly.get_order_status = AsyncMock(
        return_value={
            "id": "ord-6",
            "status": "LIVE",
            "size_matched": "25",
        }
    )
    poly.cancel_order = AsyncMock(return_value={"ok": True})

    # Deadline already reached: first iteration cancels immediately.
    # Use a tiny window so the _await_order_fill loop's second iteration
    # (after initial GET) hits the timeout.
    eng = _make_engine(
        poly,
        window_secs_remaining=5.0,
        deadline_buffer=30.0,
        poll_interval=0.01,
    )
    t = _new_trade(shares=100.0, size_usd=5.0)
    out = await eng._submit_and_confirm_live_order(t)

    assert out is t
    assert out["order_status"] == "partial"
    assert out["shares"] == pytest.approx(25.0)
    assert out["size_usd"] == pytest.approx(1.25)  # 5.0 * 25/100
    poly.cancel_order.assert_awaited_once_with("ord-6")
    eng.risk.record_order_success.assert_called_once()
    eng.risk.record_order_rejection.assert_not_called()


@pytest.mark.asyncio
async def test_place_order_submits_effective_price_with_cap():
    """place_order must receive the trade's effective token_price (already
    slippage-adjusted by _check_entry), clamped at MAX_BUY_PRICE."""
    from core.engine import MAX_BUY_PRICE

    poly = MagicMock()
    poly.place_order = AsyncMock(
        return_value={"orderID": "ord-slip", "status": "matched"}
    )
    poly.get_order_status = AsyncMock()
    poly.cancel_order = AsyncMock()

    eng = _make_engine(poly)
    t = _new_trade()
    # Simulate what _check_entry produces: raw 0.38 + 0.01 slippage = 0.39.
    t["token_price"] = 0.39
    t["raw_token_price"] = 0.38
    await eng._submit_and_confirm_live_order(t)

    poly.place_order.assert_awaited_once()
    kwargs = poly.place_order.await_args.kwargs
    assert kwargs["side"] == "BUY"
    assert kwargs["price"] == pytest.approx(0.39)

    # Price above the cap must be clamped.
    poly.place_order.reset_mock()
    poly.place_order.return_value = {"orderID": "ord-cap", "status": "matched"}
    t2 = _new_trade()
    t2["token_price"] = 1.05  # pathological pre-clamp value
    await eng._submit_and_confirm_live_order(t2)
    kwargs2 = poly.place_order.await_args.kwargs
    assert kwargs2["price"] == pytest.approx(MAX_BUY_PRICE)


@pytest.mark.asyncio
async def test_place_order_missing_id_is_treated_as_rejection():
    """Response with no orderID / id → record_order_rejection, None."""
    poly = MagicMock()
    poly.place_order = AsyncMock(return_value={"status": "live"})
    poly.get_order_status = AsyncMock()
    poly.cancel_order = AsyncMock()

    eng = _make_engine(poly)
    t = _new_trade()
    out = await eng._submit_and_confirm_live_order(t)

    assert out is None
    poly.get_order_status.assert_not_called()
    eng.risk.record_order_rejection.assert_called_once()
