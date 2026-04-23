#!/usr/bin/env python3
"""Dedicated live runner for the whale-family pair strategy.

This stays separate from the directional engine because the lifecycle is
different: paired legs, pair ledger, merge actions, and per-leg execution.

Market-data path:
  - primary source is a Polymarket CLOB WebSocket subscription per outcome
    token for the active BTC 5-minute window and its immediate successor
  - the bot tracks per-token book freshness (age in ms)
  - when the WS book is missing, stale, or the socket is disconnected, the
    bot falls back to the HTTP /book endpoint for that tick only
  - every decision and submit records book_ts, book_age_ms, decision_ts,
    submit_ts so we can audit end-to-end latency
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import time
from dataclasses import dataclass, replace
from datetime import datetime, timezone
from pathlib import Path
import sys
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from clients.ctf_merger import CTFMerger
from clients.market_scanner import MarketWindowScanner
from clients.polymarket import PolymarketClient
from clients.polymarket_ws import PolymarketWSClient
from config import Config
from core.whale_pair_ledger import (
    build_market_state,
    ensure_market_row,
    init_db,
    insert_fill,
    latest_action,
    materialize_merge,
    merge_ready_inventory,
    record_action,
)
from models.market import OrderBook
from shared.fees import taker_fee_usd
from strategies.whale_pair import (
    BookTop,
    FillDecision,
    WhalePairConfig,
    maybe_decide_fill,
    maybe_decide_pair_fill,
)

logger = logging.getLogger("whale_pair_live")

DEFAULT_WS_MAX_AGE_MS = 2_000.0


def compact(obj: Any) -> str:
    return json.dumps(obj, separators=(",", ":"), sort_keys=True, default=str)


def _now_epoch_ms() -> int:
    return int(time.time() * 1000)


def _iso_utc_ms(epoch_ms: int) -> str:
    return (
        datetime.fromtimestamp(epoch_ms / 1000.0, tz=timezone.utc)
        .isoformat(timespec="milliseconds")
        .replace("+00:00", "Z")
    )


def best_ask_from_orderbook(book: OrderBook) -> BookTop | None:
    """Extract BookTop (ask, ask_size) from an HTTP OrderBook."""
    if book.best_ask is None:
        return None
    ask_size = book.asks[0].size if book.asks else 0.0
    return BookTop(ask=float(book.best_ask), ask_size=float(ask_size))


@dataclass(frozen=True)
class BookSnapshot:
    """Top-of-book snapshot with provenance and freshness metadata."""

    top: BookTop
    source: str
    book_ts_ms: int
    age_ms: float
    detail: str | None = None

    def telemetry(self) -> dict[str, Any]:
        detail = self.detail
        if detail is None and self.source == "ws":
            detail = "fresh"
        payload = {
            "source": self.source,
            "book_ts_ms": self.book_ts_ms,
            "book_ts": _iso_utc_ms(self.book_ts_ms),
            "book_age_ms": round(self.age_ms, 1),
            "price": self.top.ask,
            "ask_size": self.top.ask_size,
        }
        if detail:
            payload["detail"] = detail
        return payload


@dataclass(frozen=True)
class OrderOutcome:
    fill: FillDecision | None
    status: str
    matched_shares: float
    remaining_shares: float


class WhalePairLiveBot:
    def __init__(
        self,
        *,
        db_path: str,
        cfg: WhalePairConfig,
        execute: bool,
        loop_seconds: int,
        use_ws: bool = True,
        ws_max_age_ms: float = DEFAULT_WS_MAX_AGE_MS,
        polymarket: PolymarketClient | None = None,
        scanner: MarketWindowScanner | None = None,
        ws_client: PolymarketWSClient | None = None,
    ) -> None:
        self.cfg = cfg
        self.execute = execute
        self.loop_seconds = loop_seconds
        self.use_ws = use_ws
        self.ws_max_age_ms = ws_max_age_ms
        self.config = Config()
        self.conn = init_db(db_path)
        self.scanner = scanner or MarketWindowScanner(coin="btc", market_type="5m")
        self.polymarket = polymarket or PolymarketClient(self.config)
        self.ws = ws_client if ws_client is not None else (PolymarketWSClient() if use_ws else None)
        self._ws_task: asyncio.Task[None] | None = None
        self._ws_target_tokens: set[str] = set()
        self.merger: CTFMerger | None = None
        if self.execute and self.config.PRIVATE_KEY:
            self.merger = CTFMerger(
                web3_provider_url=self.config.POLYGON_RPC_URL,
                private_key=self.config.PRIVATE_KEY,
                ctf_address=self.config.CTF_ADDRESS,
                collateral_token_address=self.config.COLLATERAL_TOKEN_ADDRESS,
                chain_id=self.config.CHAIN_ID,
                signature_type=self.config.POLYMARKET_SIGNATURE_TYPE,
                funder_address=self.config.POLYMARKET_FUNDER,
            )

    async def close(self) -> None:
        if self._ws_task is not None:
            self._ws_task.cancel()
            try:
                await self._ws_task
            except (asyncio.CancelledError, Exception):
                pass
            self._ws_task = None
        if self.ws is not None:
            await self.ws.close()
        await self.scanner.close()
        await self.polymarket.close()
        self.conn.close()

    async def _ensure_ws_running(self) -> None:
        if self.ws is None:
            return
        if self._ws_task is None or self._ws_task.done():
            self._ws_task = asyncio.create_task(self.ws.connect())

    @property
    def execution_mode(self) -> str:
        return "live" if self.execute else "shadow"

    async def _sync_window_subscriptions(self, windows: list[Any], now: datetime) -> None:
        if self.ws is None:
            return
        desired_tokens: list[str] = []
        active = [w for w in windows if w.is_active(now)]
        upcoming = sorted(
            [w for w in windows if w.start_time >= now and not w.is_active(now)],
            key=lambda w: w.start_time,
        )
        for window in active[:1] + upcoming[:1]:
            desired_tokens.extend([window.up_token_id, window.down_token_id])
        desired = {token for token in desired_tokens if token}
        if desired == self._ws_target_tokens:
            return
        await self.ws.sync_subscriptions(sorted(desired))
        self._ws_target_tokens = desired

    def _ws_telemetry(self) -> dict[str, Any]:
        if self.ws is None:
            return {
                "enabled": False,
                "connected": False,
                "subscribed_tokens": 0,
                "target_tokens": 0,
            }
        return {
            "enabled": self.use_ws,
            "connected": bool(self.ws.connected),
            "subscribed_tokens": self.ws.subscribed_count,
            "target_tokens": len(self._ws_target_tokens),
            "stats": self.ws.stats(),
        }

    async def get_book_snapshot(self, token_id: str) -> BookSnapshot | None:
        """Return top-of-book with provenance, trying WS first then HTTP.

        Returns None if neither source yields a usable ask.
        """
        now_epoch = time.time()
        now_ms = now_epoch * 1000.0
        fallback_detail = "ws_disabled"

        if self.ws is not None and self.use_ws:
            book = self.ws.get_book(token_id)
            if self.ws.is_book_fresh(token_id, max_age_ms=self.ws_max_age_ms, now=now_epoch):
                if book is not None and book.best_ask > 0:
                    age = self.ws.book_age_ms(token_id, now=now_epoch) or 0.0
                    return BookSnapshot(
                        top=BookTop(ask=float(book.best_ask), ask_size=float(book.best_ask_size)),
                        source="ws",
                        book_ts_ms=int(book.last_update * 1000),
                        age_ms=age,
                        detail="fresh",
                    )
            elif book is None:
                fallback_detail = "ws_missing"
            else:
                fallback_detail = "ws_stale"
                if not self.ws.connected:
                    fallback_detail = "ws_stale"
        elif self.ws is None:
            fallback_detail = "ws_unavailable"

        # HTTP fallback: either WS disabled, not connected, missing, or stale.
        http_book = await self.polymarket.get_order_book(token_id)
        top = best_ask_from_orderbook(http_book)
        if top is None:
            return None
        return BookSnapshot(
            top=top,
            source="http",
            book_ts_ms=int(now_ms),
            age_ms=0.0,
            detail=fallback_detail,
        )

    async def run(self) -> None:
        logger.warning(
            "[START] whale pair live bot execute=%s db=%s max_pair_cost=%.3f max_gross=$%.2f min_start=%ss ws=%s ws_max_age_ms=%s",
            self.execute,
            getattr(self.conn, "database", "sqlite"),
            self.cfg.max_pair_cost,
            self.cfg.max_gross_cost_usd,
            self.cfg.min_seconds_from_start,
            self.use_ws,
            self.ws_max_age_ms,
        )
        await self._ensure_ws_running()
        while True:
            await self.tick()
            if self.loop_seconds <= 0:
                break
            await asyncio.sleep(self.loop_seconds)

    async def tick(self) -> None:
        now = datetime.now(timezone.utc)
        await self._ensure_ws_running()
        windows = await self.scanner.find_active_windows()
        await self._sync_window_subscriptions(windows, now)

        for window in windows:
            if not window.is_active(now):
                continue
            seconds_from_start = int(window.elapsed_seconds(now))
            if not (self.cfg.min_seconds_from_start <= seconds_from_start <= self.cfg.max_seconds_from_start):
                continue

            ensure_market_row(
                self.conn,
                market_id=window.market_id,
                condition_id=window.condition_id or window.market_id,
                event_slug=window.slug or window.market_id,
                event_id=window.event_id or window.market_id,
                window_start_ts=int(window.start_time.timestamp()),
                window_end_ts=int(window.end_time.timestamp()),
            )

            up_snap = await self.get_book_snapshot(window.up_token_id)
            down_snap = await self.get_book_snapshot(window.down_token_id)
            self._record_book_tick(window, up_snap, down_snap)
            if up_snap is None or down_snap is None:
                continue

            allow_single_leg_accumulate = self.cfg.variant in (
                "skewed_pair_builder",
                "passive_ladder",
            )
            pair_applied = False
            state = build_market_state(self.conn, window.market_id)
            pair_fill = maybe_decide_pair_fill(
                up_top=up_snap.top,
                down_top=down_snap.top,
                state=state,
                cfg=self.cfg,
            )
            if pair_fill is not None:
                pair_applied = True
                await self._execute_fill(window, window.up_token_id, pair_fill.up_fill, up_snap)
                await self._execute_fill(window, window.down_token_id, pair_fill.down_fill, down_snap)

            if pair_applied:
                continue

            for side, token_id, snap in (
                ("Up", window.up_token_id, up_snap),
                ("Down", window.down_token_id, down_snap),
            ):
                state = build_market_state(self.conn, window.market_id)
                fill = maybe_decide_fill(
                    side=side,
                    top=snap.top,
                    state=state,
                    cfg=self.cfg,
                    allow_accumulate=allow_single_leg_accumulate,
                )
                if fill is not None:
                    await self._execute_fill(window, token_id, fill, snap)

            await self._merge_new_pairs(
                window.market_id,
                window.condition_id or window.market_id,
            )

    def _record_book_tick(
        self,
        window: Any,
        up: BookSnapshot | None,
        down: BookSnapshot | None,
    ) -> None:
        payload = {
            "slug": window.slug,
            "execution_mode": self.execution_mode,
            "seen_ts_ms": _now_epoch_ms(),
            "up": up.telemetry() if up else None,
            "down": down.telemetry() if down else None,
            "ws": self._ws_telemetry(),
        }
        record_action(self.conn, window.market_id, "book_tick", compact(payload))
        logger.info("[BOOK] %s", compact(payload))

    async def _execute_fill(
        self,
        window: Any,
        token_id: str,
        fill: FillDecision,
        snap: BookSnapshot,
        *,
        requotes_left: int = 1,
    ) -> None:
        decision_ts_ms = _now_epoch_ms()
        decision_payload = {
            "slug": window.slug,
            "execution_mode": self.execution_mode,
            "side": fill.side,
            "reason": fill.reason,
            "price": fill.price,
            "shares": fill.shares,
            "execute": self.execute,
            "book": snap.telemetry(),
            "decision_ts_ms": decision_ts_ms,
            "decision_ts": _iso_utc_ms(decision_ts_ms),
        }
        record_action(self.conn, window.market_id, "decision", compact(decision_payload))
        if not self.execute:
            logger.warning(
                "[SHADOW] %s side=%s reason=%s px=%.4f shares=%.2f src=%s detail=%s age=%.0fms",
                window.slug,
                fill.side,
                fill.reason,
                fill.price,
                fill.shares,
                snap.source,
                snap.detail or "na",
                snap.age_ms,
            )
            return

        submit_ts_ms = _now_epoch_ms()
        result = await self.polymarket.place_order(
            token_id=token_id,
            side="BUY",
            price=fill.price,
            size=fill.shares,
        )
        ack_ts_ms = _now_epoch_ms()
        order_id = result.get("orderID") or result.get("orderId") or result.get("id")
        submit_payload = {
            "slug": window.slug,
            "execution_mode": self.execution_mode,
            "side": fill.side,
            "token_id": token_id,
            "order_id": order_id,
            "result": result,
            "book": snap.telemetry(),
            "decision_ts_ms": decision_ts_ms,
            "submit_ts_ms": submit_ts_ms,
            "submit_ts": _iso_utc_ms(submit_ts_ms),
            "ack_ts_ms": ack_ts_ms,
            "book_to_decision_ms": max(0, decision_ts_ms - snap.book_ts_ms),
            "decision_to_submit_ms": max(0, submit_ts_ms - decision_ts_ms),
            "submit_to_ack_ms": max(0, ack_ts_ms - submit_ts_ms),
        }
        record_action(self.conn, window.market_id, "order_submit", compact(submit_payload))
        outcome = await self._confirm_fill(
            order_id=order_id,
            window_end=window.end_time,
            original_fill=fill,
        )
        record_action(
            self.conn,
            window.market_id,
            "order_final",
            compact(
                {
                    "slug": window.slug,
                    "side": fill.side,
                    "token_id": token_id,
                    "order_id": order_id,
                    "status": outcome.status,
                    "matched_shares": outcome.matched_shares,
                    "remaining_shares": outcome.remaining_shares,
                }
            ),
            action_ref=order_id,
        )
        if outcome.fill is None:
            return
        insert_fill(
            self.conn,
            window.market_id,
            outcome.fill,
            token_id=token_id,
            order_id=order_id,
        )
        record_action(
            self.conn,
            window.market_id,
            "fill_confirmed",
            compact(
                {
                    "slug": window.slug,
                    "side": outcome.fill.side,
                    "reason": outcome.fill.reason,
                    "price": outcome.fill.price,
                    "shares": outcome.fill.shares,
                    "gross_cost_usd": outcome.fill.gross_cost_usd,
                }
            ),
            action_ref=order_id,
        )
        if outcome.remaining_shares > 0 and requotes_left > 0:
            await self._requote_remaining(
                window,
                token_id,
                fill,
                outcome.remaining_shares,
                requotes_left=requotes_left - 1,
            )

    async def _confirm_fill(
        self,
        *,
        order_id: str | None,
        window_end: datetime,
        original_fill: FillDecision,
        timeout_seconds: int = 15,
    ) -> OrderOutcome:
        if not order_id:
            return OrderOutcome(
                fill=None,
                status="invalid_order",
                matched_shares=0.0,
                remaining_shares=original_fill.shares,
            )
        started = datetime.now(timezone.utc)
        while True:
            status = await self.polymarket.get_order_status(order_id)
            raw_status = str(status.get("status") or "").strip().lower()
            matched = float(status.get("size_matched", status.get("sizeMatched", 0)) or 0.0)
            if raw_status == "matched":
                if matched <= 0:
                    matched = original_fill.shares
                matched = min(matched, original_fill.shares)
                return OrderOutcome(
                    fill=self._scaled_fill(original_fill, matched),
                    status="matched",
                    matched_shares=matched,
                    remaining_shares=max(0.0, original_fill.shares - matched),
                )
            if raw_status in ("rejected", "failed", "cancelled", "canceled"):
                return OrderOutcome(
                    fill=self._scaled_fill(original_fill, matched) if matched > 0 else None,
                    status=raw_status,
                    matched_shares=matched,
                    remaining_shares=max(0.0, original_fill.shares - matched),
                )

            now = datetime.now(timezone.utc)
            if now >= window_end or (now - started).total_seconds() >= timeout_seconds:
                if matched > 0:
                    try:
                        await self.polymarket.cancel_order(order_id)
                    except Exception:
                        pass
                    matched = min(matched, original_fill.shares)
                    return OrderOutcome(
                        fill=self._scaled_fill(original_fill, matched),
                        status="partial_timeout",
                        matched_shares=matched,
                        remaining_shares=max(0.0, original_fill.shares - matched),
                    )
                try:
                    await self.polymarket.cancel_order(order_id)
                except Exception:
                    pass
                return OrderOutcome(
                    fill=None,
                    status="timeout",
                    matched_shares=0.0,
                    remaining_shares=original_fill.shares,
                )
            await asyncio.sleep(1.0)

    @staticmethod
    def _scaled_fill(fill: FillDecision, shares: float) -> FillDecision:
        if shares <= 0:
            return fill
        ratio = min(1.0, shares / fill.shares) if fill.shares > 0 else 0.0
        return replace(
            fill,
            shares=shares,
            gross_cost_usd=fill.gross_cost_usd * ratio,
            fee_usd=fill.fee_usd * ratio,
        )

    async def _requote_remaining(
        self,
        window: Any,
        token_id: str,
        original_fill: FillDecision,
        remaining_shares: float,
        *,
        requotes_left: int,
    ) -> None:
        now = datetime.now(timezone.utc)
        if remaining_shares <= 0 or now >= window.end_time:
            return
        snap = await self.get_book_snapshot(token_id)
        if snap is None:
            return
        if snap.top.ask <= 0 or snap.top.ask > original_fill.price:
            return
        shares = min(remaining_shares, snap.top.ask_size)
        if shares <= 0:
            return
        requote_fill = FillDecision(
            side=original_fill.side,
            reason=f"{original_fill.reason}_requote",
            price=snap.top.ask,
            ask_size=snap.top.ask_size,
            shares=shares,
            gross_cost_usd=shares * snap.top.ask,
            fee_usd=taker_fee_usd(snap.top.ask, shares * snap.top.ask),
        )
        record_action(
            self.conn,
            window.market_id,
            "order_requote",
            compact(
                {
                    "slug": window.slug,
                    "side": requote_fill.side,
                    "price": requote_fill.price,
                    "shares": requote_fill.shares,
                    "book": snap.telemetry(),
                    "requotes_left": requotes_left,
                }
            ),
        )
        await self._execute_fill(
            window,
            token_id,
            requote_fill,
            snap,
            requotes_left=requotes_left,
        )

    def _merge_submission_in_flight(self, market_id: str) -> bool:
        latest_submit = latest_action(
            self.conn,
            market_id=market_id,
            action_type="merge_submit",
        )
        if latest_submit is None:
            return False
        latest_materialized = latest_action(
            self.conn,
            market_id=market_id,
            action_type="merge_materialized",
        )
        if latest_materialized is not None and latest_materialized["id"] > latest_submit["id"]:
            return False
        payload = latest_submit.get("payload") or {}
        result = payload.get("result") if isinstance(payload, dict) else {}
        status = str((result or {}).get("status") or "").lower()
        return status in {"success", "timeout"}

    async def _merge_new_pairs(self, market_id: str, condition_id: str) -> None:
        merge_ready = merge_ready_inventory(self.conn, market_id)
        shares = float(merge_ready["pair_shares"])
        if shares <= 0:
            return
        if self._merge_submission_in_flight(market_id):
            record_action(
                self.conn,
                market_id,
                "merge_blocked",
                compact({"reason": "merge_submission_in_flight", "shares": shares}),
            )
            return
        record_action(
            self.conn,
            market_id,
            "merge_ready",
            compact({"shares": shares, "pair_pnl_usd": merge_ready["pair_pnl_usd"]}),
        )
        if not self.execute or self.merger is None:
            return
        amount_units = self.merger.shares_to_amount(shares)
        if amount_units <= 0:
            return
        result = await self.merger.merge(condition_id=condition_id, amount=amount_units)
        record_action(
            self.conn,
            market_id,
            "merge_submit",
            compact({"shares": shares, "amount_units": amount_units, "result": result}),
        )
        if result.get("status") != "success":
            return
        materialize_merge(
            self.conn,
            market_id,
            shares_to_match=shares,
            tx_hash=result.get("tx_hash"),
            tx_status=result.get("status"),
        )
        record_action(
            self.conn,
            market_id,
            "merge_materialized",
            compact(
                {
                    "shares": shares,
                    "tx_hash": result.get("tx_hash"),
                    "tx_status": result.get("status"),
                }
            ),
            action_ref=result.get("tx_hash"),
        )


def parse_args() -> argparse.Namespace:
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", default="data/whale_pair_live.db")
    ap.add_argument("--loop", type=int, default=15)
    ap.add_argument("--execute", action="store_true")
    ap.add_argument("--max-pair-cost", type=float, default=0.99)
    ap.add_argument(
        "--variant",
        choices=("pair_recycler", "skewed_pair_builder", "passive_ladder", "w1_mimic"),
        default="pair_recycler",
    )
    ap.add_argument("--base-clip-usd", type=float, default=50.0)
    ap.add_argument("--aggressive-clip-usd", type=float, default=250.0)
    ap.add_argument("--base-clip-shares", type=float, default=0.0)
    ap.add_argument("--aggressive-clip-shares", type=float, default=0.0)
    ap.add_argument("--max-gross-cost-usd", type=float, default=1000.0)
    ap.add_argument("--min-seconds-from-start", type=int, default=0)
    ap.add_argument("--max-seconds-from-start", type=int, default=298)
    ap.add_argument("--completion-min-pnl-per-share", type=float, default=0.002)
    ap.add_argument("--no-ws", action="store_true", help="Disable WebSocket book source (HTTP only)")
    ap.add_argument(
        "--ws-max-age-ms",
        type=float,
        default=DEFAULT_WS_MAX_AGE_MS,
        help="Max book age (ms) before falling back to HTTP polling",
    )
    return ap.parse_args()


async def _main() -> int:
    args = parse_args()
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
        stream=sys.stdout,
    )
    bot = WhalePairLiveBot(
        db_path=args.db,
        cfg=WhalePairConfig(
            variant=args.variant,
            max_pair_cost=args.max_pair_cost,
            base_clip_usd=args.base_clip_usd,
            aggressive_clip_usd=args.aggressive_clip_usd,
            base_clip_shares=(args.base_clip_shares or None),
            aggressive_clip_shares=(args.aggressive_clip_shares or None),
            max_gross_cost_usd=args.max_gross_cost_usd,
            min_seconds_from_start=args.min_seconds_from_start,
            max_seconds_from_start=args.max_seconds_from_start,
            completion_min_pnl_per_share=args.completion_min_pnl_per_share,
        ),
        execute=args.execute,
        loop_seconds=args.loop,
        use_ws=not args.no_ws,
        ws_max_age_ms=args.ws_max_age_ms,
    )
    try:
        await bot.run()
    finally:
        await bot.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(_main()))
