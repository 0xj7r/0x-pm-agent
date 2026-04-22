from __future__ import annotations

import asyncio
import json
import tempfile
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock

import pytest

from clients.polymarket_ws import PolymarketWSClient, TokenBook
from core.whale_pair_ledger import (
    init_db,
    insert_fill,
    load_actions,
    load_fills,
    load_matches,
    load_open_lots,
    record_action,
)
from models.market import OrderBook, PricePoint
from scripts.whale_pair_live_bot import (
    BookSnapshot,
    OrderOutcome,
    WhalePairLiveBot,
    best_ask_from_orderbook,
)
from strategies.whale_pair import BookTop, FillDecision, WhalePairConfig


def test_scaled_fill_preserves_ratio():
    original = FillDecision(
        side="Up",
        reason="pair_accumulate",
        price=0.4,
        ask_size=100.0,
        shares=10.0,
        gross_cost_usd=4.0,
        fee_usd=0.02,
    )
    scaled = WhalePairLiveBot._scaled_fill(original, 4.0)
    assert scaled.shares == 4.0
    assert scaled.gross_cost_usd == 1.6
    assert round(scaled.fee_usd, 8) == 0.008


def test_record_action_persists_json_payload():
    with tempfile.NamedTemporaryFile(suffix=".db") as tmp:
        conn = init_db(tmp.name)
        action_id = record_action(conn, "m1", "decision", json.dumps({"foo": "bar"}))
        row = conn.execute(
            "SELECT market_id, action_type, payload_json FROM whale_pair_actions WHERE id = ?",
            (action_id,),
        ).fetchone()
        assert row[0] == "m1"
        assert row[1] == "decision"
        assert json.loads(row[2]) == {"foo": "bar"}
        conn.close()


def test_ws_client_freshness_helpers_track_book_age():
    ws = PolymarketWSClient()
    token = "tok"
    ws._books[token] = TokenBook(
        token_id=token,
        best_ask=0.5,
        best_ask_size=100.0,
        last_update=1000.0,
    )
    # 500 ms after the last update
    assert ws.book_age_ms(token, now=1000.5) == pytest.approx(500.0)
    assert ws.is_book_fresh(token, max_age_ms=1000.0, now=1000.5) is True
    assert ws.is_book_fresh(token, max_age_ms=100.0, now=1000.5) is False

    # Unknown token -> None
    assert ws.book_age_ms("missing") is None
    assert ws.is_book_fresh("missing", max_age_ms=1000.0) is False

    # Book exists but has no ask -> not "live"
    ws._books["empty"] = TokenBook(token_id="empty", last_update=1000.0)
    assert ws.is_book_fresh("empty", max_age_ms=1000.0, now=1000.1) is False


def test_best_ask_from_orderbook_returns_top_ask():
    book = OrderBook(
        bids=[PricePoint(price=0.40, size=100.0)],
        asks=[PricePoint(price=0.55, size=50.0), PricePoint(price=0.56, size=20.0)],
    )
    top = best_ask_from_orderbook(book)
    assert top is not None
    assert top.ask == 0.55
    assert top.ask_size == 50.0


def test_best_ask_from_orderbook_returns_none_without_asks():
    assert best_ask_from_orderbook(OrderBook()) is None


def _build_bot(
    *,
    db_path: str,
    ws_client: PolymarketWSClient | None,
    polymarket: object,
    use_ws: bool = True,
    ws_max_age_ms: float = 2_000.0,
    execute: bool = False,
) -> WhalePairLiveBot:
    scanner = MagicMock()
    scanner.close = AsyncMock()
    bot = WhalePairLiveBot(
        db_path=db_path,
        cfg=WhalePairConfig(),
        execute=execute,
        loop_seconds=0,
        use_ws=use_ws,
        ws_max_age_ms=ws_max_age_ms,
        polymarket=polymarket,
        scanner=scanner,
        ws_client=ws_client,
    )
    return bot


def test_get_book_snapshot_prefers_fresh_ws(monkeypatch):
    monkeypatch.setattr("scripts.whale_pair_live_bot.time.time", lambda: 10_000.0)
    ws = PolymarketWSClient()
    token = "alpha"
    ws._books[token] = TokenBook(
        token_id=token,
        best_ask=0.42,
        best_ask_size=125.0,
        last_update=9_999.5,
    )
    polymarket = SimpleNamespace(
        get_order_book=AsyncMock(side_effect=AssertionError("HTTP should not run")),
        close=AsyncMock(),
    )
    with tempfile.NamedTemporaryFile(suffix=".db") as tmp:
        bot = _build_bot(db_path=tmp.name, ws_client=ws, polymarket=polymarket)
        try:
            snap = asyncio.run(bot.get_book_snapshot(token))
        finally:
            bot.conn.close()
    assert snap is not None
    assert snap.source == "ws"
    assert snap.top.ask == pytest.approx(0.42)
    assert snap.top.ask_size == pytest.approx(125.0)
    assert snap.age_ms == pytest.approx(500.0)
    assert snap.book_ts_ms == int(9_999.5 * 1000)


def test_get_book_snapshot_falls_back_to_http_when_ws_stale(monkeypatch):
    monkeypatch.setattr("scripts.whale_pair_live_bot.time.time", lambda: 10_000.0)
    ws = PolymarketWSClient()
    token = "beta"
    # Last update was 5 seconds ago -> stale vs 2s max age
    ws._books[token] = TokenBook(
        token_id=token,
        best_ask=0.30,
        best_ask_size=50.0,
        last_update=9_995.0,
    )
    http_book = OrderBook(
        asks=[PricePoint(price=0.33, size=77.0)],
    )
    polymarket = SimpleNamespace(
        get_order_book=AsyncMock(return_value=http_book),
        close=AsyncMock(),
    )
    with tempfile.NamedTemporaryFile(suffix=".db") as tmp:
        bot = _build_bot(
            db_path=tmp.name,
            ws_client=ws,
            polymarket=polymarket,
            ws_max_age_ms=2_000.0,
        )
        try:
            snap = asyncio.run(bot.get_book_snapshot(token))
        finally:
            bot.conn.close()
    polymarket.get_order_book.assert_awaited_once_with(token)
    assert snap is not None
    assert snap.source == "http"
    assert snap.top.ask == pytest.approx(0.33)
    assert snap.top.ask_size == pytest.approx(77.0)
    assert snap.age_ms == 0.0


def test_get_book_snapshot_http_when_ws_disabled():
    http_book = OrderBook(asks=[PricePoint(price=0.5, size=10.0)])
    polymarket = SimpleNamespace(
        get_order_book=AsyncMock(return_value=http_book),
        close=AsyncMock(),
    )
    with tempfile.NamedTemporaryFile(suffix=".db") as tmp:
        bot = _build_bot(
            db_path=tmp.name,
            ws_client=None,
            polymarket=polymarket,
            use_ws=False,
        )
        try:
            snap = asyncio.run(bot.get_book_snapshot("gamma"))
        finally:
            bot.conn.close()
    assert snap is not None
    assert snap.source == "http"
    polymarket.get_order_book.assert_awaited_once_with("gamma")


def test_get_book_snapshot_returns_none_when_http_empty():
    polymarket = SimpleNamespace(
        get_order_book=AsyncMock(return_value=OrderBook()),
        close=AsyncMock(),
    )
    with tempfile.NamedTemporaryFile(suffix=".db") as tmp:
        bot = _build_bot(db_path=tmp.name, ws_client=None, polymarket=polymarket, use_ws=False)
        try:
            snap = asyncio.run(bot.get_book_snapshot("delta"))
        finally:
            bot.conn.close()
    assert snap is None


@dataclass
class _WindowStub:
    market_id: str
    slug: str
    end_time: datetime = datetime.now(timezone.utc) + timedelta(minutes=5)


def test_record_book_tick_writes_action_with_latency(monkeypatch):
    monkeypatch.setattr(
        "scripts.whale_pair_live_bot._now_epoch_ms",
        lambda: 12_345_000,
    )
    ws = PolymarketWSClient()
    polymarket = SimpleNamespace(close=AsyncMock())
    with tempfile.NamedTemporaryFile(suffix=".db") as tmp:
        bot = _build_bot(db_path=tmp.name, ws_client=ws, polymarket=polymarket)
        try:
            from scripts.whale_pair_live_bot import BookSnapshot

            up = BookSnapshot(
                top=BookTop(ask=0.4, ask_size=10.0),
                source="ws",
                book_ts_ms=12_344_800,
                age_ms=200.0,
            )
            bot._record_book_tick(_WindowStub(market_id="m1", slug="slug-1"), up, None)
            actions = load_actions(bot.conn, market_id="m1", action_type="book_tick")
        finally:
            bot.conn.close()
    assert len(actions) == 1
    payload = json.loads(actions[0]["payload_json"])
    assert payload["slug"] == "slug-1"
    assert payload["seen_ts_ms"] == 12_345_000
    assert payload["up"]["source"] == "ws"
    assert payload["up"]["book_age_ms"] == 200.0
    assert payload["up"]["book_ts_ms"] == 12_344_800
    assert payload["down"] is None


def test_execute_fill_dry_run_logs_decision_with_timestamps(monkeypatch):
    times = iter([55_000, 55_010, 55_020, 55_030])
    monkeypatch.setattr(
        "scripts.whale_pair_live_bot._now_epoch_ms",
        lambda: next(times),
    )
    polymarket = SimpleNamespace(close=AsyncMock())
    with tempfile.NamedTemporaryFile(suffix=".db") as tmp:
        bot = _build_bot(db_path=tmp.name, ws_client=None, polymarket=polymarket, use_ws=False)
        try:
            from scripts.whale_pair_live_bot import BookSnapshot

            fill = FillDecision(
                side="Up",
                reason="pair_accumulate",
                price=0.4,
                ask_size=10.0,
                shares=5.0,
                gross_cost_usd=2.0,
                fee_usd=0.01,
            )
            snap = BookSnapshot(
                top=BookTop(ask=0.4, ask_size=10.0),
                source="http",
                book_ts_ms=54_900,
                age_ms=100.0,
            )
            asyncio.run(
                bot._execute_fill(
                    _WindowStub(market_id="m2", slug="slug-2"),
                    "tok-1",
                    fill,
                    snap,
                )
            )
            actions = load_actions(bot.conn, market_id="m2")
        finally:
            bot.conn.close()

    decisions = [a for a in actions if a["action_type"] == "decision"]
    submits = [a for a in actions if a["action_type"] == "order_submit"]
    assert len(decisions) == 1
    assert submits == []  # dry run -> no submit
    payload = json.loads(decisions[0]["payload_json"])
    assert payload["slug"] == "slug-2"
    assert payload["side"] == "Up"
    assert payload["reason"] == "pair_accumulate"
    assert payload["execute"] is False
    assert payload["book"]["source"] == "http"
    assert payload["book"]["book_age_ms"] == 100.0
    assert payload["decision_ts_ms"] == 55_000


def test_ws_client_connected_property():
    ws = PolymarketWSClient()
    assert ws.connected is False
    ws._ws = object()
    assert ws.connected is True


def test_execute_fill_persists_token_and_order_identifiers():
    polymarket = SimpleNamespace(
        place_order=AsyncMock(return_value={"orderID": "ord-123"}),
        close=AsyncMock(),
    )
    with tempfile.NamedTemporaryFile(suffix=".db") as tmp:
        bot = _build_bot(
            db_path=tmp.name,
            ws_client=None,
            polymarket=polymarket,
            use_ws=False,
            execute=True,
        )
        bot._confirm_fill = AsyncMock(
            return_value=OrderOutcome(
                fill=FillDecision(
                    side="Up",
                    reason="pair_accumulate",
                    price=0.4,
                    ask_size=10.0,
                    shares=5.0,
                    gross_cost_usd=2.0,
                    fee_usd=0.01,
                ),
                status="matched",
                matched_shares=5.0,
                remaining_shares=0.0,
            )
        )
        try:
            from scripts.whale_pair_live_bot import BookSnapshot

            snap = BookSnapshot(
                top=BookTop(ask=0.4, ask_size=10.0),
                source="http",
                book_ts_ms=1_000,
                age_ms=0.0,
            )
            asyncio.run(
                bot._execute_fill(
                    _WindowStub(market_id="m3", slug="slug-3"),
                    "tok-3",
                    FillDecision(
                        side="Up",
                        reason="pair_accumulate",
                        price=0.4,
                        ask_size=10.0,
                        shares=5.0,
                        gross_cost_usd=2.0,
                        fee_usd=0.01,
                    ),
                    snap,
                )
            )
            fills = load_fills(bot.conn, "m3")
        finally:
            bot.conn.close()
    assert len(fills) == 1
    assert fills[0]["token_id"] == "tok-3"
    assert fills[0]["order_id"] == "ord-123"


def test_execute_fill_requotes_remaining_partial_at_same_or_better_price():
    polymarket = SimpleNamespace(
        place_order=AsyncMock(side_effect=[{"orderID": "ord-1"}, {"orderID": "ord-2"}]),
        close=AsyncMock(),
    )
    with tempfile.NamedTemporaryFile(suffix=".db") as tmp:
        bot = _build_bot(
            db_path=tmp.name,
            ws_client=None,
            polymarket=polymarket,
            use_ws=False,
            execute=True,
        )
        outcomes = iter(
            [
                OrderOutcome(
                    fill=FillDecision(
                        side="Up",
                        reason="pair_accumulate",
                        price=0.4,
                        ask_size=10.0,
                        shares=3.0,
                        gross_cost_usd=1.2,
                        fee_usd=0.006,
                    ),
                    status="partial_timeout",
                    matched_shares=3.0,
                    remaining_shares=2.0,
                ),
                OrderOutcome(
                    fill=FillDecision(
                        side="Up",
                        reason="pair_accumulate_requote",
                        price=0.39,
                        ask_size=10.0,
                        shares=2.0,
                        gross_cost_usd=0.78,
                        fee_usd=0.004,
                    ),
                    status="matched",
                    matched_shares=2.0,
                    remaining_shares=0.0,
                ),
            ]
        )
        bot._confirm_fill = AsyncMock(side_effect=lambda **_: next(outcomes))
        bot.get_book_snapshot = AsyncMock(
            return_value=BookSnapshot(
                top=BookTop(ask=0.39, ask_size=10.0),
                source="http",
                book_ts_ms=1_100,
                age_ms=0.0,
            )
        )
        try:
            snap = BookSnapshot(
                top=BookTop(ask=0.4, ask_size=10.0),
                source="http",
                book_ts_ms=1_000,
                age_ms=0.0,
            )
            asyncio.run(
                bot._execute_fill(
                    _WindowStub(market_id="m3b", slug="slug-3b"),
                    "tok-3b",
                    FillDecision(
                        side="Up",
                        reason="pair_accumulate",
                        price=0.4,
                        ask_size=10.0,
                        shares=5.0,
                        gross_cost_usd=2.0,
                        fee_usd=0.01,
                    ),
                    snap,
                )
            )
            fills = load_fills(bot.conn, "m3b")
            actions = load_actions(bot.conn, market_id="m3b")
        finally:
            bot.conn.close()
    assert len(fills) == 2
    assert [row["order_id"] for row in fills] == ["ord-1", "ord-2"]
    assert any(a["action_type"] == "order_requote" for a in actions)


def test_merge_new_pairs_only_consumes_inventory_after_success():
    polymarket = SimpleNamespace(close=AsyncMock())
    fake_merger = SimpleNamespace(
        shares_to_amount=lambda shares: 4_000_000,
        merge=AsyncMock(return_value={"status": "success", "tx_hash": "0xabc"}),
    )
    with tempfile.NamedTemporaryFile(suffix=".db") as tmp:
        bot = _build_bot(
            db_path=tmp.name,
            ws_client=None,
            polymarket=polymarket,
            use_ws=False,
            execute=True,
        )
        bot.merger = fake_merger
        try:
            insert_fill(
                bot.conn,
                "m4",
                FillDecision(
                    side="Up",
                    reason="pair_accumulate",
                    price=0.4,
                    ask_size=10.0,
                    shares=4.0,
                    gross_cost_usd=1.6,
                    fee_usd=0.01,
                ),
            )
            insert_fill(
                bot.conn,
                "m4",
                FillDecision(
                    side="Down",
                    reason="pair_accumulate",
                    price=0.5,
                    ask_size=10.0,
                    shares=4.0,
                    gross_cost_usd=2.0,
                    fee_usd=0.01,
                ),
            )
            asyncio.run(bot._merge_new_pairs("m4", "0x" + "ab" * 32))
            matches = load_matches(bot.conn, "m4")
            open_lots = load_open_lots(bot.conn, "m4")
        finally:
            bot.conn.close()
    assert len(matches) == 1
    assert matches[0]["shares"] == 4.0
    assert matches[0]["tx_hash"] == "0xabc"
    assert matches[0]["tx_status"] == "success"
    assert open_lots == []


def test_merge_new_pairs_keeps_inventory_on_failed_chain_merge():
    polymarket = SimpleNamespace(close=AsyncMock())
    fake_merger = SimpleNamespace(
        shares_to_amount=lambda shares: 4_000_000,
        merge=AsyncMock(return_value={"status": "failed", "tx_hash": "0xdef"}),
    )
    with tempfile.NamedTemporaryFile(suffix=".db") as tmp:
        bot = _build_bot(
            db_path=tmp.name,
            ws_client=None,
            polymarket=polymarket,
            use_ws=False,
            execute=True,
        )
        bot.merger = fake_merger
        try:
            insert_fill(
                bot.conn,
                "m5",
                FillDecision(
                    side="Up",
                    reason="pair_accumulate",
                    price=0.4,
                    ask_size=10.0,
                    shares=4.0,
                    gross_cost_usd=1.6,
                    fee_usd=0.01,
                ),
            )
            insert_fill(
                bot.conn,
                "m5",
                FillDecision(
                    side="Down",
                    reason="pair_accumulate",
                    price=0.5,
                    ask_size=10.0,
                    shares=4.0,
                    gross_cost_usd=2.0,
                    fee_usd=0.01,
                ),
            )
            asyncio.run(bot._merge_new_pairs("m5", "0x" + "cd" * 32))
            matches = load_matches(bot.conn, "m5")
            open_lots = load_open_lots(bot.conn, "m5")
        finally:
            bot.conn.close()
    assert matches == []
    assert len(open_lots) == 2


def test_merge_new_pairs_blocks_when_previous_submit_unmaterialized():
    polymarket = SimpleNamespace(close=AsyncMock())
    fake_merger = SimpleNamespace(
        shares_to_amount=lambda shares: 4_000_000,
        merge=AsyncMock(return_value={"status": "success", "tx_hash": "0xabc"}),
    )
    with tempfile.NamedTemporaryFile(suffix=".db") as tmp:
        bot = _build_bot(
            db_path=tmp.name,
            ws_client=None,
            polymarket=polymarket,
            use_ws=False,
            execute=True,
        )
        bot.merger = fake_merger
        try:
            insert_fill(
                bot.conn,
                "m6",
                FillDecision(
                    side="Up",
                    reason="pair_accumulate",
                    price=0.4,
                    ask_size=10.0,
                    shares=4.0,
                    gross_cost_usd=1.6,
                    fee_usd=0.01,
                ),
            )
            insert_fill(
                bot.conn,
                "m6",
                FillDecision(
                    side="Down",
                    reason="pair_accumulate",
                    price=0.5,
                    ask_size=10.0,
                    shares=4.0,
                    gross_cost_usd=2.0,
                    fee_usd=0.01,
                ),
            )
            record_action(
                bot.conn,
                "m6",
                "merge_submit",
                json.dumps({"result": {"status": "timeout", "tx_hash": "0xtimeout"}}),
                action_ref="0xtimeout",
            )
            asyncio.run(bot._merge_new_pairs("m6", "0x" + "ef" * 32))
            actions = load_actions(bot.conn, market_id="m6")
        finally:
            bot.conn.close()
    fake_merger.merge.assert_not_awaited()
    assert any(a["action_type"] == "merge_blocked" for a in actions)
