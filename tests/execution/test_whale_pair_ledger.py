from __future__ import annotations

import json
import sqlite3
import tempfile

from core.whale_pair_ledger import (
    ensure_market_row,
    init_db,
    init_schema,
    insert_fill,
    latest_action,
    load_actions,
    load_fills,
    load_market,
    load_matches,
    load_open_lots,
    load_reconciliation_candidates,
    match_and_merge,
    materialize_merge,
    record_action,
    summarize_open_lots,
)
from strategies.whale_pair import FillDecision


def _fill(
    *,
    side: str,
    shares: float,
    price: float,
    gross_cost_usd: float,
    fee_usd: float,
    reason: str = "accumulate",
    ask_size: float = 100.0,
) -> FillDecision:
    return FillDecision(
        side=side,
        reason=reason,
        price=price,
        ask_size=ask_size,
        shares=shares,
        gross_cost_usd=gross_cost_usd,
        fee_usd=fee_usd,
    )


def test_init_schema_migrates_live_ready_columns() -> None:
    with tempfile.NamedTemporaryFile(suffix=".db") as tmp:
        conn = sqlite3.connect(tmp.name)
        conn.execute(
            """
            CREATE TABLE whale_pair_fills (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                market_id TEXT NOT NULL,
                ts TEXT NOT NULL,
                side TEXT NOT NULL,
                reason TEXT NOT NULL,
                price REAL NOT NULL,
                ask_size REAL NOT NULL,
                shares REAL NOT NULL,
                gross_cost_usd REAL NOT NULL,
                fee_usd REAL NOT NULL
            )
            """
        )
        conn.execute(
            """
            CREATE TABLE whale_pair_actions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                market_id TEXT NOT NULL,
                ts TEXT NOT NULL,
                action_type TEXT NOT NULL,
                payload_json TEXT NOT NULL
            )
            """
        )
        conn.commit()

        init_schema(conn)

        fill_cols = {
            row[1] for row in conn.execute("PRAGMA table_info(whale_pair_fills)")
        }
        action_cols = {
            row[1] for row in conn.execute("PRAGMA table_info(whale_pair_actions)")
        }
        assert {"token_id", "order_id", "client_order_id"} <= fill_cols
        assert "action_ref" in action_cols
        assert conn.row_factory is sqlite3.Row
        conn.close()


def test_action_query_helpers_filter_decode_and_order() -> None:
    with tempfile.NamedTemporaryFile(suffix=".db") as tmp:
        conn = init_db(tmp.name)
        first_id = record_action(
            conn,
            "m1",
            "decision",
            json.dumps({"side": "Up"}),
            action_ref="decision-1",
            ts="2026-04-22T09:30:01+00:00",
        )
        second_id = record_action(
            conn,
            "m1",
            "order_submit",
            json.dumps({"order_id": "ord-1"}),
            action_ref="ord-1",
            ts="2026-04-22T09:30:02+00:00",
        )
        record_action(
            conn,
            "m2",
            "decision",
            json.dumps({"side": "Down"}),
            action_ref="decision-2",
            ts="2026-04-22T09:30:03+00:00",
        )

        m1_actions = load_actions(conn, market_id="m1")
        assert [row["id"] for row in m1_actions] == [first_id, second_id]
        assert m1_actions[0]["payload"] == {"side": "Up"}

        submit_actions = load_actions(conn, action_type="order_submit")
        assert len(submit_actions) == 1
        assert submit_actions[0]["action_ref"] == "ord-1"

        after_first = load_actions(conn, market_id="m1", after_id=first_id)
        assert [row["id"] for row in after_first] == [second_id]

        newest = load_actions(conn, market_id="m1", newest_first=True, limit=1)
        assert [row["id"] for row in newest] == [second_id]

        latest = latest_action(conn, market_id="m1")
        assert latest is not None
        assert latest["id"] == second_id
        assert latest["payload"] == {"order_id": "ord-1"}
        conn.close()


def test_open_lot_and_reconciliation_helpers_surface_restart_state() -> None:
    with tempfile.NamedTemporaryFile(suffix=".db") as tmp:
        conn = init_db(tmp.name)

        ensure_market_row(
            conn,
            market_id="m-open",
            condition_id="c-open",
            event_slug="btc-updown-5m-open",
            event_id="e-open",
            window_start_ts=100,
            window_end_ts=200,
        )
        ensure_market_row(
            conn,
            market_id="m-unresolved",
            condition_id="c-unresolved",
            event_slug="btc-updown-5m-unresolved",
            event_id="e-unresolved",
            window_start_ts=110,
            window_end_ts=150,
        )
        ensure_market_row(
            conn,
            market_id="m-closed",
            condition_id="c-closed",
            event_slug="btc-updown-5m-closed",
            event_id="e-closed",
            window_start_ts=120,
            window_end_ts=140,
        )

        up_fill_id = insert_fill(
            conn,
            "m-open",
            _fill(side="Up", shares=10.0, price=0.25, gross_cost_usd=2.5, fee_usd=0.01),
            token_id="tok-up",
            order_id="ord-up",
            client_order_id="client-up",
            ts="2026-04-22T09:30:10+00:00",
        )
        insert_fill(
            conn,
            "m-open",
            _fill(side="Down", shares=4.0, price=0.60, gross_cost_usd=2.4, fee_usd=0.02),
            token_id="tok-down",
            order_id="ord-down",
            client_order_id="client-down",
            ts="2026-04-22T09:30:11+00:00",
        )
        matches = match_and_merge(conn, "m-open")
        assert matches == 1

        insert_fill(
            conn,
            "m-closed",
            _fill(side="Up", shares=3.0, price=0.20, gross_cost_usd=0.6, fee_usd=0.01),
            ts="2026-04-22T09:30:12+00:00",
        )
        insert_fill(
            conn,
            "m-closed",
            _fill(side="Down", shares=3.0, price=0.70, gross_cost_usd=2.1, fee_usd=0.02),
            ts="2026-04-22T09:30:13+00:00",
        )
        assert match_and_merge(conn, "m-closed") == 1
        conn.execute(
            "UPDATE whale_pair_markets SET resolved = 1, winning_outcome = 'Down' WHERE market_id = 'm-closed'"
        )
        conn.execute(
            "UPDATE whale_pair_markets SET resolved = 1 WHERE market_id = 'm-open'"
        )
        conn.commit()

        record_action(
            conn,
            "m-open",
            "order_submit",
            json.dumps({"order_id": "ord-up"}),
            action_ref="ord-up",
            ts="2026-04-22T09:30:09+00:00",
        )
        record_action(
            conn,
            "m-unresolved",
            "decision",
            json.dumps({"note": "pending restart"}),
            ts="2026-04-22T09:30:08+00:00",
        )

        market = load_market(conn, "m-open")
        assert market is not None
        assert market["condition_id"] == "c-open"

        fills = load_fills(conn, "m-open", order_id="ord-up")
        assert len(fills) == 1
        assert fills[0]["id"] == up_fill_id
        assert fills[0]["token_id"] == "tok-up"

        open_lots = load_open_lots(conn, "m-open")
        assert len(open_lots) == 1
        assert open_lots[0]["side"] == "Up"
        assert open_lots[0]["shares_remaining"] == 6.0

        summary = summarize_open_lots(conn, "m-open")
        assert summary["lot_count"] == 1
        assert summary["total_shares_remaining"] == 6.0
        assert summary["sides"]["Up"]["shares_remaining"] == 6.0
        assert summary["sides"]["Down"]["shares_remaining"] == 0.0
        assert summary["net_share_imbalance"] == 6.0
        assert summary["is_flat"] is False

        match_rows = load_matches(conn, "m-open")
        assert len(match_rows) == 1
        assert match_rows[0]["shares"] == 4.0

        candidates = load_reconciliation_candidates(conn)
        by_market = {row["market_id"]: row for row in candidates}
        assert set(by_market) == {"m-open", "m-unresolved"}

        open_candidate = by_market["m-open"]
        assert open_candidate["resolved"] is True
        assert open_candidate["open_lot_count"] == 1
        assert open_candidate["total_shares_remaining"] == 6.0
        assert open_candidate["matched_shares"] == 4.0
        assert open_candidate["latest_action_type"] == "order_submit"
        assert open_candidate["latest_action_ref"] == "ord-up"
        assert open_candidate["latest_action_payload"] == {"order_id": "ord-up"}

        unresolved_candidate = by_market["m-unresolved"]
        assert unresolved_candidate["resolved"] is False
        assert unresolved_candidate["open_lot_count"] == 0
        assert unresolved_candidate["total_shares_remaining"] == 0.0
        assert unresolved_candidate["latest_action_type"] == "decision"
        conn.close()


def test_materialize_merge_respects_requested_shares_and_tx_metadata() -> None:
    with tempfile.NamedTemporaryFile(suffix=".db") as tmp:
        conn = init_db(tmp.name)
        ensure_market_row(
            conn,
            market_id="m-partial",
            condition_id="c-partial",
            event_slug="btc-updown-5m-partial",
            event_id="e-partial",
            window_start_ts=100,
            window_end_ts=200,
        )
        insert_fill(
            conn,
            "m-partial",
            _fill(side="Up", shares=5.0, price=0.30, gross_cost_usd=1.5, fee_usd=0.01),
        )
        insert_fill(
            conn,
            "m-partial",
            _fill(side="Down", shares=5.0, price=0.40, gross_cost_usd=2.0, fee_usd=0.01),
        )

        matches = materialize_merge(
            conn,
            "m-partial",
            shares_to_match=2.0,
            tx_hash="0xmerge",
            tx_status="success",
        )
        assert matches == 1

        rows = load_matches(conn, "m-partial")
        assert len(rows) == 1
        assert rows[0]["shares"] == 2.0
        assert rows[0]["tx_hash"] == "0xmerge"
        assert rows[0]["tx_status"] == "success"

        open_lots = load_open_lots(conn, "m-partial")
        assert len(open_lots) == 2
        assert {row["side"]: row["shares_remaining"] for row in open_lots} == {
            "Up": 3.0,
            "Down": 3.0,
        }
        conn.close()
