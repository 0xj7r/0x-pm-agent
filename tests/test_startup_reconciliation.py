"""Tests for BTCTradingEngine._startup_reconciliation (2026-04 fix 2).

The engine catches up on resolutions it may have missed while offline
by walking ``event_log`` for entries without a matching resolution and
asking Gamma whether each market has closed. For any that has:

  - a ``resolution`` event is written via ``persistence.record_resolution``
  - the internal balance + risk state are updated
  - winners are queued for on-chain CTF redemption (returned to caller)

Active markets are left alone. Already-resolved markets (present in
``resolved_ids``) are not double-counted. The reconciler never mutates
existing event_log rows.
"""

from __future__ import annotations

import tempfile
from dataclasses import dataclass, field
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
class _RiskCfg:
    # Only the attrs read by the reconciliation path are present.
    placeholder: bool = True


@dataclass
class _RootCfg:
    paper: _PaperCfg
    risk: _RiskCfg = field(default_factory=_RiskCfg)


def _mk_trade_details(
    market_id: str,
    *,
    direction: str = "UP",
    token_price: float = 0.04,
    size_usd: float = 5.0,
    shares: float = 125.0,
    token_id: str | None = None,
) -> dict:
    return {
        "id": f"btc-threshold-{market_id}-a1b2c3",
        "market_id": market_id,
        "direction": direction,
        "token_id": token_id or f"tok-{market_id}",
        "token_price": token_price,
        "raw_token_price": token_price - 0.01,
        "size_usd": size_usd,
        "shares": shares,
        "btc_price": 67000.0,
        "move_pct": 0.12,
        "strategy": "threshold",
        "timestamp": "2026-04-21T00:00:00+00:00",
    }


def _make_engine(
    *,
    paper: bool = False,
    resolver: object | None = None,
    redeemer: object | None = None,
    balance: float = 100.0,
):
    tf = tempfile.NamedTemporaryFile(suffix=".db", delete=False)
    tf.close()
    mem = MemoryStore(tf.name)
    eng = BTCTradingEngine.__new__(BTCTradingEngine)
    eng.cfg = _RootCfg(paper=_PaperCfg(enabled=paper))
    eng.memory = mem
    eng.persistence = TradePersistence("btc", mem, supabase=None)
    eng.resolver = resolver if resolver is not None else MagicMock()
    if not hasattr(eng.resolver, "resolved_ids") or isinstance(eng.resolver, MagicMock):
        eng.resolver.resolved_ids = set()
    eng._redeemer = redeemer
    eng.risk = MagicMock()
    eng._strategy = MagicMock()
    eng._strategy.name = "threshold"
    eng.balance = balance
    return eng, mem, tf


@pytest.mark.asyncio
async def test_paper_mode_skips_reconciliation():
    eng, mem, _tf = _make_engine(paper=True)
    # Even with open entries in event_log, paper mode returns early.
    mem.save_event(
        window_id="42",
        event_type="entry",
        details=_mk_trade_details("42"),
    )
    out = await eng._startup_reconciliation()
    assert out == []
    # No resolution was written.
    events = mem.get_events_for_window("42")
    assert [e["event_type"] for e in events] == ["entry"]
    mem.close()


@pytest.mark.asyncio
async def test_no_open_entries_is_a_noop():
    resolver = MagicMock()
    resolver.check_resolution = AsyncMock()
    resolver.resolved_ids = set()
    eng, mem, _tf = _make_engine(resolver=resolver)
    out = await eng._startup_reconciliation()
    assert out == []
    resolver.check_resolution.assert_not_called()
    mem.close()


@pytest.mark.asyncio
async def test_reconciles_three_entries_marks_two_resolved_one_won():
    """Core deliverable from the Fix-2 spec:

    event_log has 3 open entries. Gamma says 2 are resolved (1 won,
    1 lost); the third is still active. Assert exactly 2 new resolution
    events are written and the winner is returned as a redemption
    candidate for Fix 3.
    """
    resolver = MagicMock()

    async def _check(market_id: str):
        # m-won: direction=UP, resolved Yes → UP wins. Our trade was UP → WIN.
        # m-lost: direction=UP, resolved No → DOWN wins. Our trade was UP → LOSS.
        # m-active: not resolved yet.
        if market_id == "m-won":
            return {
                "resolved": True,
                "winning_outcome": "Yes",
                "condition_id": "0x" + "aa" * 32,
            }
        if market_id == "m-lost":
            return {
                "resolved": True,
                "winning_outcome": "No",
                "condition_id": "0x" + "bb" * 32,
            }
        return None  # m-active

    resolver.check_resolution = AsyncMock(side_effect=_check)
    resolver.resolved_ids = set()

    eng, mem, _tf = _make_engine(
        resolver=resolver, balance=100.0
    )

    mem.save_event(
        window_id="m-won",
        event_type="entry",
        details=_mk_trade_details("m-won", direction="UP", token_price=0.04,
                                   size_usd=5.0, shares=125.0),
    )
    mem.save_event(
        window_id="m-lost",
        event_type="entry",
        details=_mk_trade_details("m-lost", direction="UP", token_price=0.06,
                                   size_usd=5.0, shares=83.33),
    )
    mem.save_event(
        window_id="m-active",
        event_type="entry",
        details=_mk_trade_details("m-active"),
    )

    won_trades = await eng._startup_reconciliation()

    # One winner returned, with condition_id stamped on for the sweep.
    assert len(won_trades) == 1
    assert won_trades[0]["market_id"] == "m-won"
    assert won_trades[0]["condition_id"] == "0x" + "aa" * 32

    # Exactly two resolution rows written. Active market still only has
    # its entry.
    resolutions = mem.conn.execute(
        "SELECT window_id FROM event_log WHERE event_type = 'resolution' ORDER BY id"
    ).fetchall()
    resolved_windows = sorted(r[0] for r in resolutions)
    assert resolved_windows == ["m-lost", "m-won"]

    active_events = mem.get_events_for_window("m-active")
    assert [e["event_type"] for e in active_events] == ["entry"]

    # resolved_ids is updated so the normal poll path won't re-process.
    assert "m-won" in resolver.resolved_ids
    assert "m-lost" in resolver.resolved_ids

    # Risk gets notified of PnL events.
    assert eng.risk.record_trade_result.call_count == 2
    assert eng.risk.update_peak_balance.call_count == 2

    # Gamma was asked about all three markets.
    assert resolver.check_resolution.await_count == 3

    mem.close()


@pytest.mark.asyncio
async def test_never_mutates_existing_resolution_rows():
    """Reconciler only appends; it never overwrites an existing resolution."""
    resolver = MagicMock()
    resolver.check_resolution = AsyncMock()
    resolver.resolved_ids = set()
    eng, mem, _tf = _make_engine(resolver=resolver)

    # Entry + a pre-existing resolution for same market → no open entry,
    # reconciler should skip it entirely.
    mem.save_event(
        window_id="m1",
        event_type="entry",
        details=_mk_trade_details("m1"),
    )
    mem.save_event(
        window_id="m1",
        event_type="resolution",
        details={"won": True, "pnl_usd": 100.0,
                 "resolved_direction": "UP",
                 "trade": _mk_trade_details("m1")},
    )

    before = mem.conn.execute(
        "SELECT COUNT(*) FROM event_log"
    ).fetchone()[0]
    out = await eng._startup_reconciliation()
    after = mem.conn.execute(
        "SELECT COUNT(*) FROM event_log"
    ).fetchone()[0]

    assert out == []
    assert before == after, "reconciler must not append when entry is already resolved"
    resolver.check_resolution.assert_not_called()
    mem.close()


@pytest.mark.asyncio
async def test_gamma_failure_per_market_does_not_crash_reconciler():
    """If Gamma raises on one market, the other markets are still checked."""
    call_log = []

    async def _check(market_id: str):
        call_log.append(market_id)
        if market_id == "m-bad":
            raise RuntimeError("gamma 500")
        return None  # treat remaining as unresolved

    resolver = MagicMock()
    resolver.check_resolution = AsyncMock(side_effect=_check)
    resolver.resolved_ids = set()
    eng, mem, _tf = _make_engine(resolver=resolver)

    mem.save_event(
        window_id="m-ok-1", event_type="entry",
        details=_mk_trade_details("m-ok-1"),
    )
    mem.save_event(
        window_id="m-bad", event_type="entry",
        details=_mk_trade_details("m-bad"),
    )
    mem.save_event(
        window_id="m-ok-2", event_type="entry",
        details=_mk_trade_details("m-ok-2"),
    )

    out = await eng._startup_reconciliation()
    assert out == []
    assert sorted(call_log) == ["m-bad", "m-ok-1", "m-ok-2"]
    mem.close()
