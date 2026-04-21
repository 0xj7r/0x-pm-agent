"""Tests for ResolutionWatcher.

Exercises:
  - eth_subscribe payload shape (address + ConditionResolution topic)
  - decoding of payoutNumerators from an eth_subscription log
  - winner derivation for YES, NO, and multi-outcome
  - callback invocation with (condition_id, winner, numerators)
  - condition_id filter pass-through
"""
from __future__ import annotations

import asyncio
import json
import sys
import types
from typing import Any

import pytest

from clients.resolution_watcher import (
    CONDITION_RESOLUTION_TOPIC,
    POLYMARKET_CTF_ADDRESS,
    ResolutionWatcher,
    _decode_payout_numerators,
    derive_winner,
)


class _FakeWS:
    def __init__(self, script: list[str]) -> None:
        self._script = list(script)
        self.sent: list[str] = []
        self._recv_queue = list(script)

    async def send(self, msg: str) -> None:
        self.sent.append(msg)

    async def recv(self) -> str:
        if not self._recv_queue:
            await asyncio.sleep(3600)
            raise RuntimeError("no more frames")
        return self._recv_queue.pop(0)

    async def close(self) -> None:
        pass

    def __aiter__(self):
        return self

    async def __anext__(self):
        if not self._recv_queue:
            await asyncio.sleep(3600)
            raise StopAsyncIteration
        return self._recv_queue.pop(0)


class _FakeConnect:
    def __init__(self, ws: _FakeWS) -> None:
        self._ws = ws

    async def __aenter__(self) -> _FakeWS:
        return self._ws

    async def __aexit__(self, *_exc: Any) -> None:
        await self._ws.close()


def _install_fake_websockets(monkeypatch, ws: _FakeWS) -> None:
    fake = types.ModuleType("websockets")
    fake.connect = lambda url, **kw: _FakeConnect(ws)  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "websockets", fake)


def _encode_word(n: int) -> str:
    return f"{n:064x}"


def _make_condition_resolution_data(
    outcome_slots: int, payout_numerators: list[int]
) -> str:
    """Build the non-indexed ABI tail of ConditionResolution.

    Layout: outcomeSlotCount | offset=0x40 | len | items...
    """
    parts = [
        _encode_word(outcome_slots),
        _encode_word(0x40),
        _encode_word(len(payout_numerators)),
    ]
    parts.extend(_encode_word(n) for n in payout_numerators)
    return "0x" + "".join(parts)


def _make_log(
    condition_id: str, payout_numerators: list[int], outcome_slots: int = 2
) -> dict:
    # Pad condition_id to 32 bytes as bytes32 topic format.
    cid = condition_id.lower()
    if not cid.startswith("0x"):
        cid = "0x" + cid
    return {
        "address": POLYMARKET_CTF_ADDRESS.lower(),
        "topics": [
            CONDITION_RESOLUTION_TOPIC,
            cid,
            "0x" + "00" * 12 + "ab" * 20,  # oracle address padded
            "0x" + "11" * 32,               # questionId
        ],
        "data": _make_condition_resolution_data(outcome_slots, payout_numerators),
    }


def _make_sub_ack(req_id: int, sub_id: str = "0xabc") -> dict:
    return {"jsonrpc": "2.0", "id": req_id, "result": sub_id}


def _make_subscription_notification(sub_id: str, log: dict) -> dict:
    return {
        "jsonrpc": "2.0",
        "method": "eth_subscription",
        "params": {"subscription": sub_id, "result": log},
    }


def test_decode_payout_numerators_yes_wins():
    data = _make_condition_resolution_data(2, [1, 0])
    outcome_slots, nums = _decode_payout_numerators(data)
    assert outcome_slots == 2
    assert nums == [1, 0]


def test_decode_payout_numerators_no_wins():
    data = _make_condition_resolution_data(2, [0, 1])
    _os, nums = _decode_payout_numerators(data)
    assert nums == [0, 1]


def test_derive_winner():
    assert derive_winner([1, 0]) == "YES"
    assert derive_winner([0, 1]) == "NO"
    assert derive_winner([1, 1]) == "UNKNOWN"
    assert derive_winner([]) == "UNKNOWN"
    assert derive_winner([1, 0, 0]) == "YES"


def test_topic_matches_keccak_signature():
    """Guard against silent drift of the event signature hash."""
    from eth_utils import keccak

    expected = "0x" + keccak(
        text="ConditionResolution(bytes32,address,bytes32,uint256,uint256[])"
    ).hex()
    assert CONDITION_RESOLUTION_TOPIC == expected


@pytest.mark.asyncio
async def test_subscribe_payload_shape(monkeypatch):
    """First RPC frame must be eth_subscribe on logs for CTF + topic."""
    ack = json.dumps(_make_sub_ack(req_id=1))
    ws = _FakeWS(script=[ack])
    _install_fake_websockets(monkeypatch, ws)

    watcher = ResolutionWatcher(ws_url="wss://fake")
    task = asyncio.create_task(watcher.connect())
    for _ in range(30):
        await asyncio.sleep(0.01)
        if ws.sent:
            break
    await watcher.close()
    task.cancel()
    try:
        await task
    except (asyncio.CancelledError, Exception):
        pass

    assert ws.sent, "expected subscribe RPC"
    req = json.loads(ws.sent[0])
    assert req["jsonrpc"] == "2.0"
    assert req["method"] == "eth_subscribe"
    assert req["params"][0] == "logs"
    assert req["params"][1]["address"] == POLYMARKET_CTF_ADDRESS.lower()
    assert req["params"][1]["topics"] == [CONDITION_RESOLUTION_TOPIC]


@pytest.mark.asyncio
async def test_resolution_event_invokes_callback(monkeypatch):
    cid = "0x" + "de" * 32
    ack = json.dumps(_make_sub_ack(req_id=1, sub_id="0xsub1"))
    notif = json.dumps(
        _make_subscription_notification("0xsub1", _make_log(cid, [1, 0]))
    )
    ws = _FakeWS(script=[ack, notif])
    _install_fake_websockets(monkeypatch, ws)

    received: list[tuple] = []

    async def on_res(condition_id: str, winner: str, nums: list[int]) -> None:
        received.append((condition_id, winner, nums))

    watcher = ResolutionWatcher(ws_url="wss://fake", on_resolution=on_res)
    task = asyncio.create_task(watcher.connect())
    for _ in range(50):
        await asyncio.sleep(0.01)
        if received:
            break

    await watcher.close()
    task.cancel()
    try:
        await task
    except (asyncio.CancelledError, Exception):
        pass

    assert len(received) == 1
    got_cid, winner, nums = received[0]
    assert got_cid.lower() == cid.lower()
    assert winner == "YES"
    assert nums == [1, 0]


@pytest.mark.asyncio
async def test_filter_excludes_non_matching_condition_ids(monkeypatch):
    cid_mine = "0x" + "aa" * 32
    cid_other = "0x" + "bb" * 32
    ack = json.dumps(_make_sub_ack(req_id=1, sub_id="0xsub1"))
    notif_other = json.dumps(
        _make_subscription_notification("0xsub1", _make_log(cid_other, [0, 1]))
    )
    notif_mine = json.dumps(
        _make_subscription_notification("0xsub1", _make_log(cid_mine, [1, 0]))
    )
    ws = _FakeWS(script=[ack, notif_other, notif_mine])
    _install_fake_websockets(monkeypatch, ws)

    received: list[tuple] = []
    watcher = ResolutionWatcher(
        ws_url="wss://fake",
        condition_ids=[cid_mine],
        on_resolution=lambda cid, w, n: received.append((cid, w, n)),
    )
    task = asyncio.create_task(watcher.connect())
    for _ in range(80):
        await asyncio.sleep(0.01)
        if received:
            break

    await watcher.close()
    task.cancel()
    try:
        await task
    except (asyncio.CancelledError, Exception):
        pass

    assert len(received) == 1
    assert received[0][0].lower() == cid_mine.lower()
    assert received[0][1] == "YES"


def test_rejects_empty_url():
    with pytest.raises(ValueError):
        ResolutionWatcher(ws_url="")
