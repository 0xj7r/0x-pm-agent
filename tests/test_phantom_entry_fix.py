"""Regression test for the phantom-entry bug (2026-04 data integrity fix).

Before the fix, ``BTCTradingEngine._check_entry`` wrote the entry to
``event_log`` synchronously, BEFORE the async caller actually placed
the CLOB order. If the order never filled (posted to the book then
timed out and got cancelled at window close), we were still left with
a phantom entry in the database and no corresponding resolution.

The fix moves ``persistence.record_entry`` into the run-loop caller,
AFTER ``_submit_and_confirm_live_order`` confirms status=matched (or a
partial fill). These tests verify that:

1. ``_check_entry`` no longer calls ``persistence.record_entry`` itself.
2. A live order that times out + is cancelled produces ZERO entry rows
   in a real SQLite ``event_log`` (the core deliverable).
3. A live order that matches DOES produce exactly one entry row.
"""

from __future__ import annotations

import inspect
import tempfile
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from unittest.mock import AsyncMock, MagicMock

import pytest

from core.engine import BTCTradingEngine
from core.memory import MemoryStore
from core.trade_persistence import TradePersistence


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


def _trade_dict() -> dict:
    return {
        "id": "btc-threshold-m1-deadbeef",
        "market_id": "m1",
        "token_id": "tok-up",
        "token_price": 0.05,
        "raw_token_price": 0.04,
        "direction": "UP",
        "shares": 100.0,
        "size_usd": 5.0,
        "btc_price": 67000.0,
        "move_pct": 0.12,
        "strategy": "threshold",
        "timestamp": "2026-04-21T00:00:00+00:00",
    }


def _engine_with_real_persistence(
    polymarket,
    window_secs_remaining: float = 120.0,
    deadline_buffer: float = 30.0,
) -> tuple[BTCTradingEngine, MemoryStore, tempfile._TemporaryFileWrapper]:
    """Build an engine with a real MemoryStore (SQLite :memory: would work
    but we use a tempfile to exactly mirror production code paths)."""
    tf = tempfile.NamedTemporaryFile(suffix=".db", delete=False)
    tf.close()
    mem = MemoryStore(tf.name)
    persistence = TradePersistence("btc", mem, supabase=None)
    eng = BTCTradingEngine.__new__(BTCTradingEngine)
    eng.cfg = _RootCfg(paper=_PaperCfg(enabled=False))
    eng._polymarket = polymarket
    eng.memory = mem
    eng.persistence = persistence
    eng.risk = MagicMock()
    eng.poly_ws = MagicMock()
    eng.poly_ws.get_book.return_value = None
    eng._strategy = MagicMock()
    eng._strategy.name = "threshold"
    eng.current_window = _FakeWindow(window_secs_remaining)
    eng._already_traded_this_window = True
    eng._window_submitted = True
    eng._window_confirmed_fill = False
    eng._order_poll_interval_s = 0.01
    eng._order_fill_deadline_buffer_s = deadline_buffer
    eng._push_order_events = {}
    eng._push_order_snapshots = {}
    eng._user_ws = None
    eng._resolution_ws = None
    eng._push_resolved_condition_ids = set()
    return eng, mem, tf


def test_check_entry_no_longer_calls_record_entry():
    """Source-level assertion: _check_entry must not CALL record_entry.

    We permit the string to appear in a comment (to document the fix)
    but the function body must not invoke ``persistence.record_entry``.
    """
    src = inspect.getsource(BTCTradingEngine._check_entry)
    # Strip comments so a documenting NOTE about record_entry is ignored.
    code_lines = [
        ln for ln in src.splitlines() if not ln.lstrip().startswith("#")
    ]
    stripped = "\n".join(code_lines)
    assert "persistence.record_entry" not in stripped, (
        "_check_entry must NOT call persistence.record_entry "
        "(that is the phantom-entry bug). Record only after fill."
    )
    assert ".record_entry(" not in stripped, (
        "_check_entry must NOT call record_entry via any path."
    )


@pytest.mark.asyncio
async def test_live_order_timeout_produces_zero_entry_events():
    """Core deliverable: a live order that posts, never fills, and is
    cancelled at window close must leave ZERO entry events in event_log.
    """
    poly = MagicMock()
    poly.place_order = AsyncMock(
        return_value={"orderID": "ord-timeout-1", "status": "live"}
    )
    # Order stays LIVE forever; cancel at deadline succeeds.
    poly.get_order_status = AsyncMock(
        return_value={"id": "ord-timeout-1", "status": "LIVE", "size_matched": "0"}
    )
    poly.cancel_order = AsyncMock(return_value={"ok": True})

    eng, mem, _tf = _engine_with_real_persistence(
        poly, window_secs_remaining=10.0, deadline_buffer=30.0
    )

    # Simulate what run() does: _check_entry produced one candidate trade,
    # but _submit_and_confirm_live_order times out.
    t = _trade_dict()
    out = await eng._submit_and_confirm_live_order(t)

    assert out is None, "timeout + cancel must not return a filled trade"

    # The crucial assertion: event_log should have ZERO entries for this
    # window_id. Pre-fix, we'd have one phantom entry with no resolution.
    events = mem.get_events_for_window("m1")
    entry_events = [e for e in events if e["event_type"] == "entry"]
    assert entry_events == [], (
        f"Phantom-entry bug regression: expected 0 entry events after "
        f"order timeout+cancel; got {len(entry_events)}"
    )

    # Execution telemetry is allowed; filled-position entries are not.
    assert [e["event_type"] for e in events] == ["order_submit", "order_final"]
    mem.close()


@pytest.mark.asyncio
async def test_live_order_match_still_records_entry_when_caller_does_so():
    """Happy path: a matched live order, followed by the explicit
    record_entry call the engine's run-loop performs, produces exactly
    one entry event. This guards against the fix over-correcting."""
    poly = MagicMock()
    poly.place_order = AsyncMock(
        return_value={"orderID": "ord-match-1", "status": "matched"}
    )
    poly.get_order_status = AsyncMock()
    poly.cancel_order = AsyncMock()

    eng, mem, _tf = _engine_with_real_persistence(poly)
    t = _trade_dict()
    out = await eng._submit_and_confirm_live_order(t)
    assert out is t

    # Run-loop writes the entry AFTER fill confirmation.
    eng.persistence.record_entry(eng._strategy.name, out)

    events = mem.get_events_for_window("m1")
    entry_events = [e for e in events if e["event_type"] == "entry"]
    assert len(entry_events) == 1, (
        f"Expected exactly one entry event after fill; got {len(entry_events)}"
    )
    mem.close()


@pytest.mark.asyncio
async def test_live_order_rejection_produces_zero_entry_events():
    """Order rejected at submit (place_order raises) must also produce
    zero entry events."""
    poly = MagicMock()
    poly.place_order = AsyncMock(side_effect=RuntimeError("insufficient allowance"))
    poly.get_order_status = AsyncMock()
    poly.cancel_order = AsyncMock()

    eng, mem, _tf = _engine_with_real_persistence(poly)
    t = _trade_dict()
    out = await eng._submit_and_confirm_live_order(t)
    assert out is None

    events = mem.get_events_for_window("m1")
    entry_events = [e for e in events if e["event_type"] == "entry"]
    assert entry_events == [], (
        f"Rejection must not write an entry; got {len(entry_events)} entries"
    )
    assert [e["event_type"] for e in events] == ["order_submit"]
    mem.close()
