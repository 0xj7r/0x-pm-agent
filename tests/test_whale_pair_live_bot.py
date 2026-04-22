"""Tests for the whale-pair live execution bot.

Focus areas:
  - OrderLifecycle: submit -> poll -> partial -> cancel state machine.
  - execute_pair: two-leg orchestration + action-log structure.
  - DryRunClient: deterministic per-leg fills.
  - summarize_merge_ready: matchable-size math after asymmetric fills.

No network, no signing. The polymarket CLOB and CTF merger are replaced
with inline awaitables so we can assert the exact call sequence.
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any

import pytest

from scripts.whale_pair_live_bot import (
    ACTION_DECISION,
    ACTION_FILL_CONFIRMED,
    ACTION_MERGE_READY,
    ACTION_MERGE_SUBMIT,
    ACTION_ORDER_FINAL,
    ACTION_ORDER_SUBMIT,
    DryRunClient,
    EntryDecision,
    LegFillPlan,
    LiveOrderResult,
    OrderLifecycle,
    execute_pair,
    log_action,
    summarize_merge_ready,
)


@pytest.fixture
def fast_sleep():
    async def _sleep(_: float) -> None:
        return None
    return _sleep


@pytest.fixture
def fake_clock():
    state = {"t": 0.0}

    def _now() -> float:
        return state["t"]

    def _advance(dt: float) -> None:
        state["t"] += dt

    return _now, _advance


def _make_placer(result: dict[str, Any]):
    calls: list[dict[str, Any]] = []

    async def _place(**kwargs: Any) -> dict[str, Any]:
        calls.append(kwargs)
        return dict(result)

    return _place, calls


def _make_status_sequence(sequence: list[dict[str, Any]]):
    idx = {"i": 0}

    async def _status(order_id: str) -> dict[str, Any]:
        i = min(idx["i"], len(sequence) - 1)
        idx["i"] += 1
        out = dict(sequence[i])
        out["order_id"] = order_id
        return out

    return _status


def _make_canceler():
    calls: list[str] = []

    async def _cancel(order_id: str) -> dict[str, Any]:
        calls.append(order_id)
        return {"order_id": order_id, "status": "CANCELED"}

    return _cancel, calls


def test_log_action_returns_structured_record(caplog):
    caplog.set_level(logging.INFO, logger="whale_pair_live")
    rec = log_action(
        "decision", {"correlation_id": "abc", "allow": True, "reason": "ok"}
    )
    assert rec["action"] == "decision"
    assert rec["correlation_id"] == "abc"
    assert rec["allow"] is True
    assert "ts" in rec
    # INFO line emitted so operator tailing is useful.
    assert any(
        "DECISION" in m and "abc" in m for m in (r.getMessage() for r in caplog.records)
    )


def test_summarize_merge_ready_asymmetric_fills():
    up = LiveOrderResult(
        order_id="u", client_order_id="c:up", side="Up", token_id="ut",
        requested_shares=10.0, filled_shares=7.0, remainder_shares=3.0,
        avg_fill_price=0.42, status="partial",
    )
    down = LiveOrderResult(
        order_id="d", client_order_id="c:down", side="Down", token_id="dt",
        requested_shares=10.0, filled_shares=4.0, remainder_shares=6.0,
        avg_fill_price=0.50, status="partial",
    )
    out = summarize_merge_ready(up, down)
    assert out["matchable_shares"] == pytest.approx(4.0)
    assert out["up_leftover"] == pytest.approx(3.0)
    assert out["down_leftover"] == pytest.approx(0.0)
    # gross_cost covers everything filled, not just the matched portion;
    # downstream PnL accounts separately for the matched vs leftover sides.
    assert out["gross_cost_usd"] == pytest.approx(7 * 0.42 + 4 * 0.50)
    assert out["expected_payout_usd"] == pytest.approx(4.0)


def test_summarize_merge_ready_zero_fill_is_zero():
    zero = LiveOrderResult(
        order_id="", client_order_id="", side="Up", token_id="",
        requested_shares=5.0, filled_shares=0.0, remainder_shares=5.0,
        avg_fill_price=0.0, status="no_fill",
    )
    other = LiveOrderResult(
        order_id="d", client_order_id="", side="Down", token_id="dt",
        requested_shares=5.0, filled_shares=3.0, remainder_shares=2.0,
        avg_fill_price=0.5, status="partial",
    )
    out = summarize_merge_ready(zero, other)
    assert out["matchable_shares"] == 0.0


# OrderLifecycle


@pytest.mark.asyncio
async def test_lifecycle_full_fill_no_cancel(fast_sleep, fake_clock):
    now, _ = fake_clock
    place, place_calls = _make_placer({"order_id": "O1", "status": "live"})
    status = _make_status_sequence(
        [{"status": "FILLED", "size_matched": 5.0, "avg_fill_price": 0.4}]
    )
    cancel, cancel_calls = _make_canceler()

    lc = OrderLifecycle(
        place_order=place,
        get_order_status=status,
        cancel_order=cancel,
        poll_interval_sec=0.001,
        poll_timeout_sec=5.0,
        clock=now,
        sleep=fast_sleep,
    )
    plan = LegFillPlan(side="Up", token_id="T", price=0.4, size_shares=5.0)
    res = await lc.run(plan)

    assert res.status == "filled"
    assert res.filled_shares == 5.0
    assert res.remainder_shares == 0.0
    assert len(cancel_calls) == 0  # No cancel on full fill
    assert place_calls[0]["token_id"] == "T"
    assert place_calls[0]["side"] == "BUY"
    assert place_calls[0]["price"] == 0.4


@pytest.mark.asyncio
async def test_lifecycle_partial_fill_above_min_accepted_and_canceled(
    fast_sleep, fake_clock
):
    now, _ = fake_clock
    place, _ = _make_placer({"order_id": "O2", "status": "live"})
    # Order remains open (LIVE) long enough for timeout; we still report
    # partial fills accumulated so far. Then we cancel the remainder.
    status = _make_status_sequence(
        [
            {"status": "LIVE", "size_matched": 1.0, "avg_fill_price": 0.35},
            {"status": "LIVE", "size_matched": 3.0, "avg_fill_price": 0.36},
        ]
    )
    cancel, cancel_calls = _make_canceler()

    clock_now, advance = fake_clock

    # Advance clock past timeout after one iteration so the poll loop exits.
    async def slow_sleep(_: float) -> None:
        advance(10.0)

    lc = OrderLifecycle(
        place_order=place,
        get_order_status=status,
        cancel_order=cancel,
        poll_interval_sec=0.1,
        poll_timeout_sec=5.0,
        clock=clock_now,
        sleep=slow_sleep,
    )
    plan = LegFillPlan(
        side="Down", token_id="D", price=0.4, size_shares=5.0, min_fill_shares=2.0
    )
    res = await lc.run(plan)

    assert res.status == "partial"
    # The poll loop consumed one sleep; status was called at least once.
    assert res.filled_shares >= 1.0
    assert res.remainder_shares >= 1.0
    assert len(cancel_calls) == 1
    assert cancel_calls[0] == "O2"


@pytest.mark.asyncio
async def test_lifecycle_partial_below_min_is_no_fill_and_canceled(
    fast_sleep, fake_clock
):
    now, _ = fake_clock
    place, _ = _make_placer({"order_id": "O3", "status": "live"})
    status = _make_status_sequence(
        [{"status": "LIVE", "size_matched": 0.1, "avg_fill_price": 0.4}]
    )
    cancel, cancel_calls = _make_canceler()
    clock_now, advance = fake_clock

    async def slow_sleep(_: float) -> None:
        advance(10.0)

    lc = OrderLifecycle(
        place_order=place,
        get_order_status=status,
        cancel_order=cancel,
        poll_interval_sec=0.1,
        poll_timeout_sec=5.0,
        clock=clock_now,
        sleep=slow_sleep,
    )
    plan = LegFillPlan(
        side="Up", token_id="U", price=0.4, size_shares=5.0, min_fill_shares=1.0
    )
    res = await lc.run(plan)

    assert res.status == "no_fill"
    assert res.filled_shares == pytest.approx(0.1)
    assert len(cancel_calls) == 1


@pytest.mark.asyncio
async def test_lifecycle_submit_failure_is_no_fill_no_cancel(fast_sleep, fake_clock):
    now, _ = fake_clock
    # Immediate rejection: no order_id, status=rejected. We must not poll
    # nor cancel; simply classify as no_fill.
    place, _ = _make_placer({"status": "rejected", "reason": "insufficient"})
    status = _make_status_sequence([])
    cancel, cancel_calls = _make_canceler()

    lc = OrderLifecycle(
        place_order=place,
        get_order_status=status,
        cancel_order=cancel,
        poll_interval_sec=0.1,
        poll_timeout_sec=0.5,
        clock=now,
        sleep=fast_sleep,
    )
    plan = LegFillPlan(side="Up", token_id="X", price=0.4, size_shares=5.0)
    res = await lc.run(plan)

    assert res.status == "no_fill"
    assert res.filled_shares == 0.0
    assert len(cancel_calls) == 0


@pytest.mark.asyncio
async def test_lifecycle_submit_exception_is_error_no_cancel(fast_sleep, fake_clock):
    now, _ = fake_clock

    async def raises(**_: Any) -> dict[str, Any]:
        raise RuntimeError("clob down")

    status = _make_status_sequence([])
    cancel, cancel_calls = _make_canceler()

    lc = OrderLifecycle(
        place_order=raises,
        get_order_status=status,
        cancel_order=cancel,
        poll_interval_sec=0.1,
        poll_timeout_sec=0.5,
        clock=now,
        sleep=fast_sleep,
    )
    plan = LegFillPlan(side="Up", token_id="X", price=0.4, size_shares=5.0)
    res = await lc.run(plan)

    assert res.status == "error"
    assert "clob down" in res.reason
    assert len(cancel_calls) == 0


@pytest.mark.asyncio
async def test_lifecycle_canceled_terminal_no_extra_cancel(fast_sleep, fake_clock):
    """If the CLOB already reports CANCELED, we must not double-cancel."""
    now, _ = fake_clock
    place, _ = _make_placer({"order_id": "O4", "status": "live"})
    status = _make_status_sequence(
        [{"status": "CANCELED", "size_matched": 0.0}]
    )
    cancel, cancel_calls = _make_canceler()

    lc = OrderLifecycle(
        place_order=place,
        get_order_status=status,
        cancel_order=cancel,
        poll_interval_sec=0.001,
        poll_timeout_sec=5.0,
        clock=now,
        sleep=fast_sleep,
    )
    plan = LegFillPlan(side="Up", token_id="T", price=0.4, size_shares=5.0)
    res = await lc.run(plan)

    assert res.status == "no_fill"
    # Terminal CANCELED should short-circuit our own cancel attempt.
    assert len(cancel_calls) == 0


@pytest.mark.asyncio
async def test_lifecycle_poll_status_exception_does_not_abort(fast_sleep, fake_clock):
    """A transient RPC error from get_order_status should not propagate."""
    now, _ = fake_clock
    place, _ = _make_placer({"order_id": "O5", "status": "live"})
    seq = [
        RuntimeError("rpc transient"),
        {"status": "FILLED", "size_matched": 5.0, "avg_fill_price": 0.4},
    ]
    idx = {"i": 0}

    async def flaky_status(order_id: str) -> dict[str, Any]:
        item = seq[min(idx["i"], len(seq) - 1)]
        idx["i"] += 1
        if isinstance(item, Exception):
            raise item
        out = dict(item)
        out["order_id"] = order_id
        return out

    cancel, _ = _make_canceler()
    lc = OrderLifecycle(
        place_order=place,
        get_order_status=flaky_status,
        cancel_order=cancel,
        poll_interval_sec=0.001,
        poll_timeout_sec=5.0,
        clock=now,
        sleep=fast_sleep,
    )
    plan = LegFillPlan(side="Up", token_id="T", price=0.4, size_shares=5.0)
    res = await lc.run(plan)
    # Flaky status recovered on 2nd poll with FILLED.
    assert res.status == "filled"
    assert res.filled_shares == 5.0


# execute_pair orchestration


@pytest.mark.asyncio
async def test_execute_pair_denied_decision_short_circuits(caplog):
    caplog.set_level(logging.INFO, logger="whale_pair_live")
    placed: list[Any] = []

    async def place(**kwargs):
        placed.append(kwargs)
        return {"order_id": "x", "status": "live"}

    async def status(_):
        return {"status": "FILLED", "size_matched": 1.0}

    async def cancel(_):
        return {"status": "CANCELED"}

    lc = OrderLifecycle(
        place_order=place,
        get_order_status=status,
        cancel_order=cancel,
        poll_interval_sec=0.001,
        poll_timeout_sec=0.5,
    )
    decision = EntryDecision(allow=False, reason="sum_price >= 1.0")
    up = LegFillPlan(side="Up", token_id="U", price=0.55, size_shares=5.0)
    down = LegFillPlan(side="Down", token_id="D", price=0.55, size_shares=5.0)

    out = await execute_pair(
        lifecycle=lc, up_plan=up, down_plan=down, decision=decision
    )
    assert out["skipped"] is True
    assert placed == []
    assert any(
        r.getMessage().startswith("[DECISION]") for r in caplog.records
    )


@pytest.mark.asyncio
async def test_execute_pair_full_fill_logs_all_actions(caplog):
    caplog.set_level(logging.INFO, logger="whale_pair_live")
    placed: list[Any] = []

    async def place(**kwargs):
        placed.append(kwargs)
        return {"order_id": f"ord-{kwargs['token_id']}", "status": "live"}

    async def status(order_id):
        # Match the token-id-embedded id back to its price so the avg is
        # exactly the limit price. Token id "U" -> 0.4, "D" -> 0.5.
        if order_id.endswith("U"):
            return {"status": "FILLED", "size_matched": 10.0, "avg_fill_price": 0.4}
        return {"status": "FILLED", "size_matched": 10.0, "avg_fill_price": 0.5}

    async def cancel(_):
        return {"status": "CANCELED"}

    merge_calls: list[dict[str, Any]] = []

    async def merge_fn(*, condition_id, amount_shares):
        merge_calls.append({"cid": condition_id, "amount": amount_shares})
        return {"status": "success", "tx_hash": "0xabc", "gas_used": 120000}

    lc = OrderLifecycle(
        place_order=place,
        get_order_status=status,
        cancel_order=cancel,
        poll_interval_sec=0.001,
        poll_timeout_sec=5.0,
    )
    decision = EntryDecision(
        allow=True, reason="edge > 0", notes={"sum_price": 0.9}
    )
    up = LegFillPlan(side="Up", token_id="U", price=0.4, size_shares=10.0)
    down = LegFillPlan(side="Down", token_id="D", price=0.5, size_shares=10.0)

    out = await execute_pair(
        lifecycle=lc,
        up_plan=up,
        down_plan=down,
        decision=decision,
        condition_id="0x" + "aa" * 32,
        merge_fn=merge_fn,
    )

    assert len(placed) == 2
    assert out["merge_submit"]["status"] == "success"
    assert merge_calls == [{"cid": "0x" + "aa" * 32, "amount": 10.0}]

    messages = [r.getMessage() for r in caplog.records]
    assert any("[DECISION]" in m for m in messages)
    # Each leg emits ORDER_SUBMIT, ORDER_FINAL, FILL_CONFIRMED.
    assert sum("[ORDER_SUBMIT]" in m for m in messages) == 2
    assert sum("[ORDER_FINAL]" in m for m in messages) == 2
    assert sum("[FILL_CONFIRMED]" in m for m in messages) == 2
    assert sum("[MERGE_READY]" in m for m in messages) == 1
    assert sum("[MERGE_SUBMIT]" in m for m in messages) == 1


@pytest.mark.asyncio
async def test_execute_pair_zero_matchable_skips_merge(caplog):
    caplog.set_level(logging.INFO, logger="whale_pair_live")

    async def place(**kwargs):
        return {"order_id": f"o-{kwargs['token_id']}", "status": "live"}

    async def status(order_id):
        # Up fills, Down doesn't.
        if "U" in order_id:
            return {"status": "FILLED", "size_matched": 5.0, "avg_fill_price": 0.4}
        return {"status": "CANCELED", "size_matched": 0.0}

    async def cancel(_):
        return {"status": "CANCELED"}

    merge_called = []

    async def merge_fn(**kwargs):
        merge_called.append(kwargs)
        return {"status": "success", "tx_hash": "0x1"}

    lc = OrderLifecycle(
        place_order=place,
        get_order_status=status,
        cancel_order=cancel,
        poll_interval_sec=0.001,
        poll_timeout_sec=5.0,
    )
    decision = EntryDecision(allow=True, reason="edge > 0")
    up = LegFillPlan(side="Up", token_id="U", price=0.4, size_shares=5.0)
    down = LegFillPlan(side="Down", token_id="D", price=0.5, size_shares=5.0)

    out = await execute_pair(
        lifecycle=lc,
        up_plan=up,
        down_plan=down,
        decision=decision,
        condition_id="0x" + "bb" * 32,
        merge_fn=merge_fn,
    )
    assert out["merge"]["status"] == "skipped"
    assert "no matchable fill" in out["merge"]["reason"]
    assert merge_called == []


@pytest.mark.asyncio
async def test_execute_pair_dry_run_never_calls_merge(caplog):
    caplog.set_level(logging.INFO, logger="whale_pair_live")

    async def place(**kwargs):
        return {"order_id": f"o-{kwargs['token_id']}", "status": "live"}

    async def status(order_id):
        return {"status": "FILLED", "size_matched": 5.0, "avg_fill_price": 0.4}

    async def cancel(_):
        return {"status": "CANCELED"}

    merge_called = []

    async def merge_fn(**kwargs):
        merge_called.append(kwargs)
        return {"status": "success", "tx_hash": "0x1"}

    lc = OrderLifecycle(
        place_order=place,
        get_order_status=status,
        cancel_order=cancel,
        poll_interval_sec=0.001,
        poll_timeout_sec=5.0,
    )
    decision = EntryDecision(allow=True, reason="edge > 0")
    up = LegFillPlan(side="Up", token_id="U", price=0.4, size_shares=5.0)
    down = LegFillPlan(side="Down", token_id="D", price=0.4, size_shares=5.0)

    out = await execute_pair(
        lifecycle=lc,
        up_plan=up,
        down_plan=down,
        decision=decision,
        condition_id="0x" + "cc" * 32,
        merge_fn=merge_fn,
        dry_run=True,
    )
    assert merge_called == []
    assert out["merge_submit"]["status"] == "dry_run"
    assert out["merge_submit"]["tx_hash"] == ""


@pytest.mark.asyncio
async def test_execute_pair_merge_exception_logged_as_error():
    async def place(**kwargs):
        return {"order_id": f"o-{kwargs['token_id']}", "status": "live"}

    async def status(_):
        return {"status": "FILLED", "size_matched": 5.0, "avg_fill_price": 0.4}

    async def cancel(_):
        return {"status": "CANCELED"}

    async def merge_fn(**kwargs):
        raise RuntimeError("rpc flake")

    lc = OrderLifecycle(
        place_order=place,
        get_order_status=status,
        cancel_order=cancel,
        poll_interval_sec=0.001,
        poll_timeout_sec=5.0,
    )
    decision = EntryDecision(allow=True, reason="edge > 0")
    up = LegFillPlan(side="Up", token_id="U", price=0.4, size_shares=5.0)
    down = LegFillPlan(side="Down", token_id="D", price=0.4, size_shares=5.0)

    out = await execute_pair(
        lifecycle=lc,
        up_plan=up,
        down_plan=down,
        decision=decision,
        condition_id="0x" + "dd" * 32,
        merge_fn=merge_fn,
    )
    assert out["merge_submit"]["status"] == "error"
    assert "rpc flake" in out["merge_submit"]["reason"]


@pytest.mark.asyncio
async def test_execute_pair_missing_condition_id_errors_at_merge():
    async def place(**kwargs):
        return {"order_id": f"o-{kwargs['token_id']}", "status": "live"}

    async def status(_):
        return {"status": "FILLED", "size_matched": 5.0, "avg_fill_price": 0.4}

    async def cancel(_):
        return {"status": "CANCELED"}

    async def merge_fn(**kwargs):
        return {"status": "success", "tx_hash": "0x"}

    lc = OrderLifecycle(
        place_order=place,
        get_order_status=status,
        cancel_order=cancel,
        poll_interval_sec=0.001,
        poll_timeout_sec=5.0,
    )
    decision = EntryDecision(allow=True, reason="edge > 0")
    up = LegFillPlan(side="Up", token_id="U", price=0.4, size_shares=5.0)
    down = LegFillPlan(side="Down", token_id="D", price=0.4, size_shares=5.0)

    out = await execute_pair(
        lifecycle=lc,
        up_plan=up,
        down_plan=down,
        decision=decision,
        condition_id=None,
        merge_fn=merge_fn,
    )
    assert out["merge_submit"]["status"] == "error"
    assert "missing condition_id" in out["merge_submit"]["reason"]


# DryRunClient


@pytest.mark.asyncio
async def test_dry_run_client_full_fill():
    c = DryRunClient(fill_fraction=1.0)
    submit = await c.place_order(token_id="T", side="BUY", price=0.4, size=10.0)
    assert submit["status"] == "live"
    assert submit["order_id"].startswith("dry-")
    status = await c.get_order_status(submit["order_id"])
    assert status["status"] == "FILLED"
    assert status["size_matched"] == pytest.approx(10.0)
    assert status["avg_fill_price"] == pytest.approx(0.4)


@pytest.mark.asyncio
async def test_dry_run_client_partial_fill():
    c = DryRunClient(fill_fraction=0.3)
    s1 = await c.place_order(token_id="A", side="BUY", price=0.5, size=10.0)
    s2 = await c.place_order(token_id="B", side="BUY", price=0.4, size=20.0)
    # Per-order state separate, not leaked between legs.
    got1 = await c.get_order_status(s1["order_id"])
    got2 = await c.get_order_status(s2["order_id"])
    assert got1["size_matched"] == pytest.approx(3.0)
    assert got1["avg_fill_price"] == pytest.approx(0.5)
    assert got2["size_matched"] == pytest.approx(6.0)
    assert got2["avg_fill_price"] == pytest.approx(0.4)
    # Partial => MATCHED, not FILLED.
    assert got1["status"] == "MATCHED"


@pytest.mark.asyncio
async def test_dry_run_client_cancel_returns_shape():
    c = DryRunClient()
    res = await c.cancel_order("dry-000001")
    assert res["status"] == "CANCELED"
    assert res["dry_run"] is True


@pytest.mark.asyncio
async def test_lifecycle_integrates_with_dry_run_client():
    """Sanity: OrderLifecycle + DryRunClient yields a usable filled result."""
    c = DryRunClient(fill_fraction=1.0)
    lc = OrderLifecycle(
        place_order=c.place_order,
        get_order_status=c.get_order_status,
        cancel_order=c.cancel_order,
        poll_interval_sec=0.001,
        poll_timeout_sec=1.0,
    )
    res = await lc.run(
        LegFillPlan(side="Up", token_id="T", price=0.35, size_shares=8.0)
    )
    assert res.status == "filled"
    assert res.filled_shares == pytest.approx(8.0)
    assert res.avg_fill_price == pytest.approx(0.35)
