"""Tests for autoresearch.audit.

Verifies the hash chain works, tampering is detected, and the
read-side helpers (find, count_since) work for the use cases the
Holdout Burner agent will need them for.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from autoresearch.audit import (
    GENESIS_HASH,
    AuditEntry,
    AuditLog,
    _hash_entry_without_self,
)


@pytest.fixture
def tmp_log(tmp_path: Path) -> AuditLog:
    return AuditLog(path=tmp_path / "audit.jsonl")


class TestAppendAndVerify:
    def test_empty_log_verifies_zero(self, tmp_log: AuditLog):
        assert tmp_log.verify() == 0

    def test_single_append_verifies(self, tmp_log: AuditLog):
        entry = tmp_log.append(
            "test_agent",
            "test_action",
            subject={"type": "test", "id": "abc"},
            details={"foo": "bar"},
        )
        assert isinstance(entry, AuditEntry)
        assert entry.prev_hash == GENESIS_HASH
        assert entry.self_hash.startswith("sha256:")
        assert tmp_log.verify() == 1

    def test_chain_links_correctly(self, tmp_log: AuditLog):
        e1 = tmp_log.append("agent_a", "action_1")
        e2 = tmp_log.append("agent_b", "action_2")
        e3 = tmp_log.append("agent_c", "action_3")
        assert e2.prev_hash == e1.self_hash
        assert e3.prev_hash == e2.self_hash
        assert tmp_log.verify() == 3

    def test_tampering_with_action_detected(self, tmp_log: AuditLog):
        tmp_log.append("agent_a", "action_1")
        tmp_log.append("agent_b", "action_2")
        # Tamper with the second line: change "action_2" to "action_X"
        lines = tmp_log.path.read_text().splitlines()
        d = json.loads(lines[1])
        d["action"] = "action_X"
        # Don't recompute self_hash — leave the tampered field
        lines[1] = json.dumps(d, separators=(",", ":"))
        tmp_log.path.write_text("\n".join(lines) + "\n")

        with pytest.raises(RuntimeError, match="self_hash mismatch"):
            tmp_log.verify()

    def test_tampering_with_self_hash_recomputed_still_breaks_chain(
        self, tmp_log: AuditLog
    ):
        # Even if the attacker recomputes self_hash for the tampered
        # entry, the next entry's prev_hash check will fail.
        tmp_log.append("agent_a", "action_1")
        tmp_log.append("agent_b", "action_2")
        tmp_log.append("agent_c", "action_3")
        lines = tmp_log.path.read_text().splitlines()
        d = json.loads(lines[1])
        d["action"] = "action_X"
        d["self_hash"] = _hash_entry_without_self(d)
        lines[1] = json.dumps(d, separators=(",", ":"))
        tmp_log.path.write_text("\n".join(lines) + "\n")

        with pytest.raises(RuntimeError, match="prev_hash mismatch"):
            tmp_log.verify()

    def test_blank_lines_ignored(self, tmp_log: AuditLog):
        tmp_log.append("agent", "action")
        with tmp_log.path.open("a") as f:
            f.write("\n\n")
        tmp_log.append("agent", "action")
        assert tmp_log.verify() == 2


class TestFindAndCount:
    def test_find_by_action(self, tmp_log: AuditLog):
        tmp_log.append("a", "candidate_written")
        tmp_log.append("b", "holdout_burned")
        tmp_log.append("c", "candidate_written")
        results = tmp_log.find(action="candidate_written")
        assert len(results) == 2

    def test_find_by_agent(self, tmp_log: AuditLog):
        tmp_log.append("data_steward", "data_audit_run")
        tmp_log.append("researcher", "candidate_written")
        results = tmp_log.find(agent="data_steward")
        assert len(results) == 1
        assert results[0].agent == "data_steward"

    def test_count_since_filters_by_time(self, tmp_log: AuditLog):
        tmp_log.append("burner", "holdout_burned", ts="2026-04-01T00:00:00Z")
        tmp_log.append("burner", "holdout_burned", ts="2026-04-05T00:00:00Z")
        tmp_log.append("burner", "holdout_burned", ts="2026-04-08T00:00:00Z")
        # Count since April 4
        n = tmp_log.count_since(action="holdout_burned", since_iso="2026-04-04T00:00:00Z")
        assert n == 2

    def test_count_since_filters_by_subject_type(self, tmp_log: AuditLog):
        tmp_log.append(
            "burner", "holdout_burned",
            subject={"type": "candidate", "hash": "x"},
            ts="2026-04-08T00:00:00Z",
        )
        tmp_log.append(
            "burner", "holdout_burned",
            subject={"type": "other", "id": "y"},
            ts="2026-04-08T00:00:01Z",
        )
        n = tmp_log.count_since(
            action="holdout_burned",
            since_iso="2026-04-01T00:00:00Z",
            subject_type="candidate",
        )
        assert n == 1


class TestIteration:
    def test_iter_yields_entries(self, tmp_log: AuditLog):
        tmp_log.append("a", "x")
        tmp_log.append("b", "y")
        entries = list(tmp_log)
        assert len(entries) == 2
        assert entries[0].agent == "a"
        assert entries[1].agent == "b"

    def test_iter_on_missing_file(self, tmp_path: Path):
        log = AuditLog(path=tmp_path / "doesnotexist.jsonl")
        assert list(log) == []
