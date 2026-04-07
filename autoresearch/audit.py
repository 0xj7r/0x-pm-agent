"""Append-only hash-chained audit log for the autoresearch framework.

The audit log is the single source of truth for "what actually
happened" in the research pipeline. Every dataset audit, every
search run, every candidate written, every holdout burned, every
promotion or kill is recorded here. The log is the substrate that
makes the rest of the discipline forensically verifiable: when a
paper-traded candidate fails, you can reconstruct exactly which
preregistration authorized it, which dataset it was trained on,
which validation report blessed it, and which holdout result was
used to promote it.

Why hash-chained: the threat model is not malicious tampering
(there is one user). The hash chain exists so an LLM agent cannot
quietly rewrite history in a later session without the violation
being obvious. If an entry is modified after the fact, every
subsequent entry's prev_hash check fails, and the verify() walker
points at the offending line.

Why append-only: the agent that writes the entries should not be
the agent that reads them, and neither should be able to delete
them. Append-only files mostly enforce this by convention. The
file lives in git so any direct modification is also visible in
diffs.

Schema (one JSON object per line):

    {
      "ts": "2026-04-08T00:00:00Z",
      "agent": "researcher",
      "action": "candidate_written",
      "subject": { "type": "candidate", "hash": "..." },
      "details": { ... },
      "git_head": "abc1234",
      "prev_hash": "sha256:...",
      "self_hash": "sha256:..."
    }

self_hash is computed over the entry with self_hash field removed,
serialized as canonical JSON. prev_hash is the self_hash of the
previous entry in the file. The first entry has prev_hash equal
to the empty-string SHA256.
"""
from __future__ import annotations

import hashlib
import json
import os
import subprocess
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterator


GENESIS_HASH = "sha256:" + hashlib.sha256(b"").hexdigest()
DEFAULT_LOG_PATH = Path(__file__).parent / "audit.jsonl"


def _utc_now_iso() -> str:
    """Current UTC time in ISO 8601 with second precision and 'Z' suffix.

    Sub-second precision is intentionally dropped so two entries
    written in the same second sort consistently across replays.
    """
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _git_head() -> str:
    """Return the current git HEAD short SHA, or 'unknown' on failure.

    The audit log records git HEAD on every entry so we can
    reconstruct what version of the code wrote which entry. This
    matters when an old candidate hash needs to be re-evaluated
    against newer code: the audit log tells us which code version
    produced it.
    """
    try:
        out = subprocess.check_output(
            ["git", "rev-parse", "--short", "HEAD"],
            cwd=Path(__file__).parent.parent,
            stderr=subprocess.DEVNULL,
        )
        return out.decode().strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return "unknown"


def _canonical_json(obj: Any) -> str:
    """Deterministic JSON serialization for hashing.

    Keys sorted, no whitespace, no NaN/Infinity. Two callers
    serializing the same dict get identical bytes, regardless of
    insertion order, which is the property the hash chain needs.
    """
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), allow_nan=False)


def _hash_entry_without_self(entry: dict[str, Any]) -> str:
    """Compute the self_hash field for an entry, excluding self_hash."""
    body = {k: v for k, v in entry.items() if k != "self_hash"}
    canonical = _canonical_json(body)
    return "sha256:" + hashlib.sha256(canonical.encode()).hexdigest()


@dataclass(frozen=True)
class AuditEntry:
    """One row in the audit log.

    Construct via AuditLog.append, not directly. The constructor
    here is for read-side use (verify, replay).
    """
    ts: str
    agent: str
    action: str
    subject: dict[str, Any]
    details: dict[str, Any]
    git_head: str
    prev_hash: str
    self_hash: str

    def to_dict(self) -> dict[str, Any]:
        return {
            "ts": self.ts,
            "agent": self.agent,
            "action": self.action,
            "subject": self.subject,
            "details": self.details,
            "git_head": self.git_head,
            "prev_hash": self.prev_hash,
            "self_hash": self.self_hash,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "AuditEntry":
        return cls(
            ts=d["ts"],
            agent=d["agent"],
            action=d["action"],
            subject=d.get("subject", {}),
            details=d.get("details", {}),
            git_head=d.get("git_head", "unknown"),
            prev_hash=d["prev_hash"],
            self_hash=d["self_hash"],
        )


class AuditLog:
    """Append-only hash-chained log writer + verifier.

    Usage:
        log = AuditLog()  # uses DEFAULT_LOG_PATH
        log.append("data_steward", "data_audit_run",
                   subject={"type": "dataset", "coin": "btc"},
                   details={"passed": True, "checks": 8})
        log.verify()  # raises if the chain is broken
    """

    def __init__(self, path: Path | None = None) -> None:
        self.path = Path(path) if path else DEFAULT_LOG_PATH
        self.path.parent.mkdir(parents=True, exist_ok=True)

    def _last_hash(self) -> str:
        """Return the self_hash of the last entry, or GENESIS_HASH if empty."""
        if not self.path.exists():
            return GENESIS_HASH
        last_line = None
        with self.path.open("r") as f:
            for line in f:
                line = line.strip()
                if line:
                    last_line = line
        if last_line is None:
            return GENESIS_HASH
        try:
            return json.loads(last_line)["self_hash"]
        except (json.JSONDecodeError, KeyError) as exc:
            raise RuntimeError(
                f"audit log corrupt: cannot read last entry self_hash: {exc}"
            )

    def append(
        self,
        agent: str,
        action: str,
        subject: dict[str, Any] | None = None,
        details: dict[str, Any] | None = None,
        ts: str | None = None,
        git_head: str | None = None,
    ) -> AuditEntry:
        """Append a new entry to the log and return it.

        Computes prev_hash from the file's last entry (or
        GENESIS_HASH if empty), then computes self_hash over
        the entry minus self_hash, then writes the line atomically.
        """
        entry_dict: dict[str, Any] = {
            "ts": ts or _utc_now_iso(),
            "agent": agent,
            "action": action,
            "subject": subject or {},
            "details": details or {},
            "git_head": git_head or _git_head(),
            "prev_hash": self._last_hash(),
        }
        entry_dict["self_hash"] = _hash_entry_without_self(entry_dict)

        line = json.dumps(entry_dict, separators=(",", ":")) + "\n"
        # Atomic append: open in append mode and write a single
        # newline-terminated line. Concurrent appenders may interleave
        # at line boundaries; this is fine because each line is its
        # own valid JSON object.
        with self.path.open("a") as f:
            f.write(line)
            f.flush()
            os.fsync(f.fileno())

        return AuditEntry.from_dict(entry_dict)

    def __iter__(self) -> Iterator[AuditEntry]:
        if not self.path.exists():
            return
        with self.path.open("r") as f:
            for line_no, raw in enumerate(f, start=1):
                raw = raw.strip()
                if not raw:
                    continue
                try:
                    d = json.loads(raw)
                except json.JSONDecodeError as exc:
                    raise RuntimeError(
                        f"audit log line {line_no}: invalid JSON ({exc})"
                    )
                yield AuditEntry.from_dict(d)

    def verify(self) -> int:
        """Walk the entire log and verify the hash chain.

        Returns the number of entries verified. Raises RuntimeError
        on the first violation, with the offending line number and
        the reason. Run by a pre-commit hook so any tampering is
        caught before it lands in git.
        """
        if not self.path.exists():
            return 0
        prev_hash = GENESIS_HASH
        count = 0
        with self.path.open("r") as f:
            for line_no, raw in enumerate(f, start=1):
                raw = raw.strip()
                if not raw:
                    continue
                try:
                    d = json.loads(raw)
                except json.JSONDecodeError as exc:
                    raise RuntimeError(
                        f"audit log line {line_no}: invalid JSON ({exc})"
                    )
                if d.get("prev_hash") != prev_hash:
                    raise RuntimeError(
                        f"audit log line {line_no}: prev_hash mismatch. "
                        f"expected {prev_hash}, got {d.get('prev_hash')}. "
                        f"chain broken at this entry."
                    )
                expected_self = _hash_entry_without_self(d)
                if d.get("self_hash") != expected_self:
                    raise RuntimeError(
                        f"audit log line {line_no}: self_hash mismatch. "
                        f"entry was modified after writing. "
                        f"expected {expected_self}, got {d.get('self_hash')}."
                    )
                prev_hash = d["self_hash"]
                count += 1
        return count

    def find(self, **filters: Any) -> list[AuditEntry]:
        """Return entries matching all filters (exact match on top-level fields).

        Useful for the Holdout Burner agent's "have we already burned
        the holdout for this candidate hash?" check:

            log.find(action="holdout_burned",
                     subject={"type": "candidate", "hash": "abc..."})
        """
        results = []
        for entry in self:
            ok = True
            for k, v in filters.items():
                if getattr(entry, k, None) != v:
                    ok = False
                    break
            if ok:
                results.append(entry)
        return results

    def count_since(
        self,
        action: str,
        since_iso: str,
        subject_type: str | None = None,
    ) -> int:
        """Count entries with a given action since a timestamp.

        Used by the Holdout Burner to enforce the weekly holdout
        budget per coin: count holdout_burned entries with this
        coin in the past 7 days.
        """
        count = 0
        for entry in self:
            if entry.action != action:
                continue
            if entry.ts < since_iso:
                continue
            if subject_type and entry.subject.get("type") != subject_type:
                continue
            count += 1
        return count
