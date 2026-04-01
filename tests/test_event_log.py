"""Tests for the event log table in MemoryStore."""
from __future__ import annotations

import json
import tempfile

import pytest

from core.memory import MemoryStore


@pytest.fixture
def store():
    with tempfile.NamedTemporaryFile(suffix=".db", delete=False) as f:
        s = MemoryStore(f.name)
        yield s
        s.close()


def test_save_and_retrieve_event(store: MemoryStore):
    store.save_event(
        window_id="win_001",
        event_type="signal_update",
        log_odds=1.5,
        p_up=0.818,
        btc_price=84350.0,
        details={"reason": "strong buy flow"},
    )
    events = store.get_events_for_window("win_001")
    assert len(events) == 1
    assert events[0]["event_type"] == "signal_update"
    assert events[0]["log_odds"] == 1.5
    assert events[0]["p_up"] == 0.818
    assert events[0]["btc_price"] == 84350.0
    details = json.loads(events[0]["details"])
    assert details["reason"] == "strong buy flow"


def test_multiple_events_for_window(store: MemoryStore):
    for i in range(5):
        store.save_event(
            window_id="win_002",
            event_type="signal_update",
            log_odds=float(i),
            p_up=0.5,
            btc_price=84000.0 + i,
        )
    events = store.get_events_for_window("win_002")
    assert len(events) == 5


def test_events_isolated_by_window(store: MemoryStore):
    store.save_event(window_id="win_A", event_type="entry_attempt")
    store.save_event(window_id="win_B", event_type="fill")
    assert len(store.get_events_for_window("win_A")) == 1
    assert len(store.get_events_for_window("win_B")) == 1
