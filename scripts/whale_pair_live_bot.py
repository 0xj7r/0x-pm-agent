#!/usr/bin/env python3
"""Whale-pair live execution bot with per-leg order lifecycle management.

Strategy contract
-----------------
For every active 5-min btc-updown window we attempt to accumulate paired
Up+Down exposure at prices summing to < $1.00. Every filled Up+Down pair
is merged via the Gnosis CTF `mergePositions` call into $1 of USDC,
locking in a risk-free edge of (1 - up_cost - down_cost - fees).

What this module owns
---------------------
Live execution hardening for each leg of a whale-pair entry. The
decision surface (what price, what clip) is fed in by a caller that
already knows the book and the strategy parameters; we do not
re-implement the strategy logic from the paper bot here. Our job is to:

  1. decide whether to enter a leg (pre-flight checks + DECISION log),
  2. submit a GTC BUY limit order (ORDER_SUBMIT),
  3. poll order status until terminal or stall (ORDER_FINAL),
  4. scale partial fills down cleanly and record the real filled size
     (FILL_CONFIRMED) vs the stuck remainder which we cancel,
  5. once both legs have non-zero fills, merge the matchable amount
     atomically via CTFMerger (MERGE_READY -> MERGE_SUBMIT).

All terminal events are written to an action log (stdlib logger +
structured JSON dict) so operator dashboards can ingest them.

Dry-run mode
------------
`--dry-run` flips every external side effect (place_order, cancel_order,
merge_pair) into a simulation that returns deterministic fake receipts
so the whole flow can be exercised end-to-end on live market data
without risking capital.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import os
import sys
import time
import uuid
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Awaitable, Callable, Iterable

ROOT = Path(__file__).resolve().parent.parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

logger = logging.getLogger("whale_pair_live")


# Action constants: single source of truth for the log stream. Downstream
# dashboards key off these exact strings, so they deliberately stay
# outside Enum to keep JSON serialization trivial.
ACTION_DECISION = "decision"
ACTION_ORDER_SUBMIT = "order_submit"
ACTION_ORDER_FINAL = "order_final"
ACTION_FILL_CONFIRMED = "fill_confirmed"
ACTION_MERGE_READY = "merge_ready"
ACTION_MERGE_SUBMIT = "merge_submit"

DEFAULT_POLL_INTERVAL_SEC = 1.0
DEFAULT_POLL_TIMEOUT_SEC = 20.0
DEFAULT_MIN_FILL_SHARES = 0.5

_TERMINAL_STATUSES = {"FILLED", "MATCHED", "CANCELED", "CANCELLED", "EXPIRED", "REJECTED"}


def _now_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


def _safe_float(value: Any, default: float = 0.0) -> float:
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


def log_action(action: str, payload: dict[str, Any]) -> dict[str, Any]:
    """Emit a structured action log entry.

    Two-channel on purpose: a human-readable log line at INFO so tailing
    `docker logs` is useful, plus a JSON blob returned to the caller so
    tests and ledger code can assert on the exact shape without parsing
    the formatted log string.
    """
    record = {"action": action, "ts": _now_iso(), **payload}
    logger.info("[%s] %s", action.upper(), json.dumps(record, default=str))
    return record


@dataclass
class LiveOrderResult:
    """Terminal-state record for a single placed order.

    `filled_shares` is the portion we actually got; `remainder_shares` is
    what we asked for minus what filled (canceled or expired). Callers
    use `filled_shares` and never `requested_shares` when sizing merges.
    """
    order_id: str
    client_order_id: str
    side: str
    token_id: str
    requested_shares: float
    filled_shares: float
    remainder_shares: float
    avg_fill_price: float
    status: str
    reason: str = ""
    raw_final: dict[str, Any] = field(default_factory=dict)


@dataclass
class LegFillPlan:
    """Caller's execution intent for one side of the pair.

    `price` is the limit. `size_shares` is the target quantity; a
    partial fill smaller than `min_fill_shares` aborts the leg as
    "no_fill" rather than letting dust accumulate.
    """
    side: str  # "Up" or "Down"
    token_id: str
    price: float
    size_shares: float
    min_fill_shares: float = DEFAULT_MIN_FILL_SHARES


@dataclass
class EntryDecision:
    """Pre-flight decision record. `allow=False` leaves logs explaining why."""
    allow: bool
    reason: str
    notes: dict[str, Any] = field(default_factory=dict)


class OrderLifecycle:
    """Handles the submit -> poll -> partial -> cancel flow for one leg.

    Three terminal shapes emerge from `run(plan)`:

      1. "filled":        full fill by the polling deadline.
      2. "partial":       partial fill >= min, remainder canceled.
      3. "no_fill":       nothing filled (or < min); order canceled
                          and the leg is declared unusable.

    A "no_fill" result is not an error: the caller simply skips the
    merge for this tick. Errors surface as `status="error"` with a
    populated `reason`; the caller is responsible for backing off.
    """

    def __init__(
        self,
        *,
        place_order: Callable[..., Awaitable[dict[str, Any]]],
        get_order_status: Callable[..., Awaitable[dict[str, Any]]],
        cancel_order: Callable[..., Awaitable[dict[str, Any]]],
        poll_interval_sec: float = DEFAULT_POLL_INTERVAL_SEC,
        poll_timeout_sec: float = DEFAULT_POLL_TIMEOUT_SEC,
        clock: Callable[[], float] = time.monotonic,
        sleep: Callable[[float], Awaitable[None]] = asyncio.sleep,
    ) -> None:
        self._place_order = place_order
        self._get_order_status = get_order_status
        self._cancel_order = cancel_order
        self._poll_interval = max(0.05, float(poll_interval_sec))
        self._poll_timeout = max(self._poll_interval, float(poll_timeout_sec))
        self._clock = clock
        self._sleep = sleep

    async def run(
        self,
        plan: LegFillPlan,
        *,
        correlation_id: str | None = None,
    ) -> LiveOrderResult:
        cid = correlation_id or uuid.uuid4().hex[:12]
        # ORDER_SUBMIT
        try:
            submit_resp = await self._place_order(
                token_id=plan.token_id,
                side="BUY",
                price=plan.price,
                size=plan.size_shares,
            )
        except Exception as exc:
            log_action(
                ACTION_ORDER_SUBMIT,
                {
                    "correlation_id": cid,
                    "side": plan.side,
                    "status": "error",
                    "reason": f"place_order raised: {exc}",
                    "token_id": plan.token_id,
                    "price": plan.price,
                    "size": plan.size_shares,
                },
            )
            return LiveOrderResult(
                order_id="",
                client_order_id=cid,
                side=plan.side,
                token_id=plan.token_id,
                requested_shares=plan.size_shares,
                filled_shares=0.0,
                remainder_shares=plan.size_shares,
                avg_fill_price=0.0,
                status="error",
                reason=f"place_order raised: {exc}",
            )

        order_id = str(
            submit_resp.get("order_id")
            or submit_resp.get("orderId")
            or submit_resp.get("id")
            or ""
        )
        submit_status = str(submit_resp.get("status") or "").lower()
        log_action(
            ACTION_ORDER_SUBMIT,
            {
                "correlation_id": cid,
                "side": plan.side,
                "token_id": plan.token_id,
                "price": plan.price,
                "size": plan.size_shares,
                "order_id": order_id,
                "raw_status": submit_status,
            },
        )

        # CLOB returned an immediate failure (e.g. insufficient balance,
        # matching failure). Treat as no_fill without polling.
        if not order_id or submit_status in {"failed", "rejected"}:
            return LiveOrderResult(
                order_id=order_id,
                client_order_id=cid,
                side=plan.side,
                token_id=plan.token_id,
                requested_shares=plan.size_shares,
                filled_shares=0.0,
                remainder_shares=plan.size_shares,
                avg_fill_price=0.0,
                status="no_fill",
                reason=f"submit failed: {submit_status or 'no order_id'}",
                raw_final=dict(submit_resp),
            )

        # Poll for terminal state or timeout.
        deadline = self._clock() + self._poll_timeout
        latest: dict[str, Any] = dict(submit_resp)
        while True:
            try:
                latest = await self._get_order_status(order_id)
            except Exception as exc:
                logger.warning(
                    "get_order_status raised for %s: %s; continuing poll", order_id, exc
                )
            status = str(latest.get("status") or "").upper()
            if status in _TERMINAL_STATUSES:
                break
            if self._clock() >= deadline:
                break
            await self._sleep(self._poll_interval)

        filled = _safe_float(latest.get("size_matched") or latest.get("filled") or 0.0)
        remainder = max(plan.size_shares - filled, 0.0)
        avg_price = _safe_float(
            latest.get("avg_fill_price")
            or latest.get("price_avg")
            or latest.get("price")
            or plan.price
        )
        raw_status = str(latest.get("status") or "").upper()

        # Cancel only when the order is still open with a remainder. If
        # the CLOB already reports a terminal state (FILLED, CANCELED,
        # EXPIRED, REJECTED), issuing another cancel wastes an RPC call
        # and can confuse the server-side order book.
        cancel_needed = (
            raw_status not in _TERMINAL_STATUSES and remainder > 0
        )
        cancel_result: dict[str, Any] = {}
        if cancel_needed:
            try:
                cancel_result = await self._cancel_order(order_id)
            except Exception as exc:
                logger.warning("cancel_order raised for %s: %s", order_id, exc)
                cancel_result = {"error": str(exc)}

        # Classify outcome. Partial >= min: usable. Partial < min: no_fill.
        if filled >= plan.size_shares - 1e-9:
            outcome = "filled"
            reason = "full"
        elif filled >= plan.min_fill_shares:
            outcome = "partial"
            reason = "partial_accepted"
        else:
            outcome = "no_fill"
            reason = (
                f"filled {filled:.4f} < min {plan.min_fill_shares:.4f}"
                if filled > 0
                else "no fills within poll window"
            )

        result = LiveOrderResult(
            order_id=order_id,
            client_order_id=cid,
            side=plan.side,
            token_id=plan.token_id,
            requested_shares=plan.size_shares,
            filled_shares=filled,
            remainder_shares=remainder,
            avg_fill_price=avg_price,
            status=outcome,
            reason=reason,
            raw_final=dict(latest),
        )

        log_action(
            ACTION_ORDER_FINAL,
            {
                "correlation_id": cid,
                "side": plan.side,
                "order_id": order_id,
                "status": outcome,
                "requested": plan.size_shares,
                "filled": filled,
                "remainder": remainder,
                "avg_fill_price": avg_price,
                "clob_status": raw_status,
                "canceled": bool(cancel_result),
                "reason": reason,
            },
        )
        if outcome in {"filled", "partial"}:
            log_action(
                ACTION_FILL_CONFIRMED,
                {
                    "correlation_id": cid,
                    "side": plan.side,
                    "order_id": order_id,
                    "filled_shares": filled,
                    "avg_fill_price": avg_price,
                    "gross_cost_usd": filled * avg_price,
                },
            )
        return result


class DryRunClient:
    """Deterministic stand-in for the live CLOB client.

    Every call returns a synthetic-but-shape-correct response so the
    bot can run end-to-end on live market data without placing real
    orders. `fill_fraction` controls how much of a submitted order is
    reported as filled by the simulated polling path. State is kept
    per-order-id so two concurrent legs in the same tick each get the
    right filled-size and price reported back.

    Not a MagicMock: we want the attribute surface to be fixed so a
    drift in the real client signature is caught by import errors
    rather than silently fabricated mock methods.
    """

    def __init__(self, fill_fraction: float = 1.0) -> None:
        self.fill_fraction = float(fill_fraction)
        self._seq = 0
        self._orders: dict[str, dict[str, Any]] = {}

    def _next_id(self) -> str:
        self._seq += 1
        return f"dry-{self._seq:06d}"

    async def place_order(
        self,
        token_id: str,
        side: str,
        price: float,
        size: float,
        fee_rate_bps: int | None = None,
    ) -> dict[str, Any]:
        order_id = self._next_id()
        self._orders[order_id] = {
            "token_id": token_id,
            "side": side,
            "price": float(price),
            "size": float(size),
        }
        return {
            "order_id": order_id,
            "status": "live",
            "size_matched": 0.0,
            "price": price,
            "dry_run": True,
        }

    async def get_order_status(self, order_id: str) -> dict[str, Any]:
        state = self._orders.get(order_id, {})
        size = float(state.get("size", 0.0))
        price = float(state.get("price", 0.0))
        filled = size * self.fill_fraction
        return {
            "order_id": order_id,
            "status": "FILLED" if self.fill_fraction >= 1.0 else "MATCHED",
            "size_matched": filled,
            "avg_fill_price": price,
            "dry_run": True,
        }

    async def cancel_order(self, order_id: str) -> dict[str, Any]:
        return {"order_id": order_id, "status": "CANCELED", "dry_run": True}


def summarize_merge_ready(
    up_result: LiveOrderResult,
    down_result: LiveOrderResult,
) -> dict[str, Any]:
    """Compute the matchable merge size after both legs return.

    The merge can only burn as many pairs as the smaller leg filled.
    Leftover on the bigger side is recorded so the caller can either
    hold it to resolution or retry more inventory on the short leg.
    """
    matchable = max(0.0, min(up_result.filled_shares, down_result.filled_shares))
    up_leftover = max(0.0, up_result.filled_shares - matchable)
    down_leftover = max(0.0, down_result.filled_shares - matchable)
    total_cost = (
        up_result.filled_shares * up_result.avg_fill_price
        + down_result.filled_shares * down_result.avg_fill_price
    )
    return {
        "matchable_shares": matchable,
        "up_filled": up_result.filled_shares,
        "down_filled": down_result.filled_shares,
        "up_leftover": up_leftover,
        "down_leftover": down_leftover,
        "up_avg_price": up_result.avg_fill_price,
        "down_avg_price": down_result.avg_fill_price,
        "gross_cost_usd": total_cost,
        "expected_payout_usd": matchable,
        "expected_edge_usd": matchable - (
            matchable * (up_result.avg_fill_price + down_result.avg_fill_price)
        ),
    }


async def execute_pair(
    *,
    lifecycle: OrderLifecycle,
    up_plan: LegFillPlan,
    down_plan: LegFillPlan,
    decision: EntryDecision,
    correlation_id: str | None = None,
    merge_fn: Callable[..., Awaitable[dict[str, Any]]] | None = None,
    condition_id: str | None = None,
    dry_run: bool = False,
) -> dict[str, Any]:
    """End-to-end two-leg execution for a single market window.

    The steps, and the action log entries they emit:

      1. decision (allow/deny)               -> DECISION
      2. submit BOTH legs concurrently        -> ORDER_SUBMIT (x2)
      3. poll each until terminal / timeout  -> ORDER_FINAL (x2)
      4. for any accepted fills              -> FILL_CONFIRMED
      5. compute matchable pair size          -> MERGE_READY
      6. submit merge (or log dry-run)        -> MERGE_SUBMIT

    Returns a dict containing every action record + the two leg
    results + the merge result. The caller persists this as a single
    ledger row.
    """
    cid = correlation_id or uuid.uuid4().hex[:12]

    decision_record = log_action(
        ACTION_DECISION,
        {
            "correlation_id": cid,
            "allow": decision.allow,
            "reason": decision.reason,
            "up_side": {"price": up_plan.price, "size": up_plan.size_shares},
            "down_side": {"price": down_plan.price, "size": down_plan.size_shares},
            "dry_run": dry_run,
            **decision.notes,
        },
    )
    if not decision.allow:
        return {
            "correlation_id": cid,
            "decision": decision_record,
            "skipped": True,
        }

    up_task = asyncio.create_task(lifecycle.run(up_plan, correlation_id=f"{cid}:up"))
    down_task = asyncio.create_task(
        lifecycle.run(down_plan, correlation_id=f"{cid}:down")
    )
    up_result, down_result = await asyncio.gather(up_task, down_task)

    # Safer no-fill: if BOTH legs produced no usable fill, we bail out
    # without touching the CTF. Pair merging requires >0 on both sides.
    matchable = min(up_result.filled_shares, down_result.filled_shares)
    if matchable <= 0:
        return {
            "correlation_id": cid,
            "decision": decision_record,
            "up_result": up_result,
            "down_result": down_result,
            "merge": {"status": "skipped", "reason": "no matchable fill"},
        }

    ready = summarize_merge_ready(up_result, down_result)
    merge_ready_record = log_action(
        ACTION_MERGE_READY,
        {"correlation_id": cid, "condition_id": condition_id, **ready},
    )

    if dry_run or merge_fn is None:
        merge_submit_record = log_action(
            ACTION_MERGE_SUBMIT,
            {
                "correlation_id": cid,
                "condition_id": condition_id,
                "amount_shares": ready["matchable_shares"],
                "status": "dry_run",
                "tx_hash": "",
            },
        )
        return {
            "correlation_id": cid,
            "decision": decision_record,
            "up_result": up_result,
            "down_result": down_result,
            "merge_ready": merge_ready_record,
            "merge_submit": merge_submit_record,
        }

    if not condition_id:
        merge_submit_record = log_action(
            ACTION_MERGE_SUBMIT,
            {
                "correlation_id": cid,
                "condition_id": None,
                "amount_shares": ready["matchable_shares"],
                "status": "error",
                "reason": "missing condition_id",
                "tx_hash": "",
            },
        )
        return {
            "correlation_id": cid,
            "decision": decision_record,
            "up_result": up_result,
            "down_result": down_result,
            "merge_ready": merge_ready_record,
            "merge_submit": merge_submit_record,
        }

    try:
        merge_res = await merge_fn(
            condition_id=condition_id,
            amount_shares=ready["matchable_shares"],
        )
    except Exception as exc:
        merge_submit_record = log_action(
            ACTION_MERGE_SUBMIT,
            {
                "correlation_id": cid,
                "condition_id": condition_id,
                "amount_shares": ready["matchable_shares"],
                "status": "error",
                "reason": f"merge raised: {exc}",
                "tx_hash": "",
            },
        )
        return {
            "correlation_id": cid,
            "decision": decision_record,
            "up_result": up_result,
            "down_result": down_result,
            "merge_ready": merge_ready_record,
            "merge_submit": merge_submit_record,
        }

    merge_submit_record = log_action(
        ACTION_MERGE_SUBMIT,
        {
            "correlation_id": cid,
            "condition_id": condition_id,
            "amount_shares": ready["matchable_shares"],
            "status": merge_res.get("status", "unknown"),
            "tx_hash": merge_res.get("tx_hash", ""),
            "gas_used": merge_res.get("gas_used", 0),
        },
    )
    return {
        "correlation_id": cid,
        "decision": decision_record,
        "up_result": up_result,
        "down_result": down_result,
        "merge_ready": merge_ready_record,
        "merge_submit": merge_submit_record,
    }


def build_clob_callables(client: Any) -> dict[str, Callable[..., Awaitable[dict[str, Any]]]]:
    """Adapter: bind the three lifecycle hooks to an existing CLOB client.

    Kept tiny on purpose; if a caller wants to inject retries or circuit
    breakers they wrap the returned callables, not the client.
    """
    return {
        "place_order": client.place_order,
        "get_order_status": client.get_order_status,
        "cancel_order": client.cancel_order,
    }


def _configure_logging(verbose: bool) -> None:
    logging.basicConfig(
        level=logging.DEBUG if verbose else logging.INFO,
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
        stream=sys.stdout,
    )


def main(argv: Iterable[str] | None = None) -> int:
    """CLI entrypoint.

    The default mode is `--dry-run`: every order placement, cancel, and
    merge call is simulated so an operator can observe the lifecycle +
    action log shape against real market prices before flipping to
    live. To go live the operator must explicitly set `--live`.
    """
    ap = argparse.ArgumentParser(
        description="Whale-pair live execution bot (per-leg hardened)."
    )
    ap.add_argument("--up-token-id", required=False, default="")
    ap.add_argument("--down-token-id", required=False, default="")
    ap.add_argument("--up-price", type=float, default=0.4)
    ap.add_argument("--down-price", type=float, default=0.5)
    ap.add_argument("--size-shares", type=float, default=10.0)
    ap.add_argument(
        "--min-fill-shares", type=float, default=DEFAULT_MIN_FILL_SHARES
    )
    ap.add_argument("--condition-id", default="")
    ap.add_argument(
        "--dry-run",
        action="store_true",
        default=True,
        help="Simulate all external side effects (default: on).",
    )
    ap.add_argument(
        "--live",
        action="store_true",
        default=False,
        help="Disable dry-run; place real orders and submit real merge txs.",
    )
    ap.add_argument(
        "--dry-run-fill-fraction",
        type=float,
        default=1.0,
        help="Fraction of each leg to simulate as filled (0.0-1.0).",
    )
    ap.add_argument(
        "--poll-interval-sec", type=float, default=DEFAULT_POLL_INTERVAL_SEC
    )
    ap.add_argument(
        "--poll-timeout-sec", type=float, default=DEFAULT_POLL_TIMEOUT_SEC
    )
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args(list(argv) if argv is not None else None)

    _configure_logging(args.verbose)

    dry_run = args.dry_run and not args.live
    if not dry_run:
        logger.warning(
            "LIVE MODE ENABLED - real orders and real merge txs will be sent"
        )

    if not args.up_token_id or not args.down_token_id:
        logger.info(
            "[DEMO] No token ids provided; emitting a decision log record only."
        )
        log_action(
            ACTION_DECISION,
            {
                "allow": False,
                "reason": "missing token ids",
                "dry_run": dry_run,
            },
        )
        return 0

    up_plan = LegFillPlan(
        side="Up",
        token_id=args.up_token_id,
        price=args.up_price,
        size_shares=args.size_shares,
        min_fill_shares=args.min_fill_shares,
    )
    down_plan = LegFillPlan(
        side="Down",
        token_id=args.down_token_id,
        price=args.down_price,
        size_shares=args.size_shares,
        min_fill_shares=args.min_fill_shares,
    )
    sum_price = args.up_price + args.down_price
    decision = EntryDecision(
        allow=sum_price < 1.0,
        reason="edge > 0" if sum_price < 1.0 else "prices sum >= 1.0",
        notes={"sum_price": sum_price},
    )

    if dry_run:
        dry_client = DryRunClient(fill_fraction=args.dry_run_fill_fraction)
        lifecycle = OrderLifecycle(
            place_order=dry_client.place_order,
            get_order_status=dry_client.get_order_status,
            cancel_order=dry_client.cancel_order,
            poll_interval_sec=args.poll_interval_sec,
            poll_timeout_sec=args.poll_timeout_sec,
        )
        merge_fn = None
    else:
        # Late imports so `--help` works without the full trading stack.
        from clients.ctf_merger import CTFMerger
        from clients.polymarket import PolymarketClient
        from config import Config

        cfg = Config()
        client = PolymarketClient(cfg)
        calls = build_clob_callables(client)
        lifecycle = OrderLifecycle(
            place_order=calls["place_order"],
            get_order_status=calls["get_order_status"],
            cancel_order=calls["cancel_order"],
            poll_interval_sec=args.poll_interval_sec,
            poll_timeout_sec=args.poll_timeout_sec,
        )

        web3_url = os.getenv("POLYGON_RPC_URL") or os.getenv("WEB3_PROVIDER_URL")
        if not web3_url:
            logger.error(
                "POLYGON_RPC_URL (or WEB3_PROVIDER_URL) required for live merge"
            )
            return 2
        merger = CTFMerger(
            web3_provider_url=web3_url,
            private_key=cfg.PRIVATE_KEY,
            ctf_address="0x4D97DCd97eC945f40cF65F87097ACe5EA0476045",
            collateral_token_address="0x2791Bca1F2de4661ED88A30C99A7a9449Aa84174",
            chain_id=getattr(cfg, "CHAIN_ID", 137),
            signature_type=cfg.POLYMARKET_SIGNATURE_TYPE,
            funder_address=cfg.POLYMARKET_FUNDER,
        )
        merge_fn = merger.merge_pair

    async def _run() -> int:
        result = await execute_pair(
            lifecycle=lifecycle,
            up_plan=up_plan,
            down_plan=down_plan,
            decision=decision,
            condition_id=args.condition_id or None,
            merge_fn=merge_fn,
            dry_run=dry_run,
        )
        logger.info("[RESULT] correlation_id=%s", result.get("correlation_id"))
        return 0

    return asyncio.run(_run())


if __name__ == "__main__":
    sys.exit(main())
