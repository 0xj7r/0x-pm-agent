"""CLI wrapper for autoresearch.audit.AuditLog.verify().

Usage:
    python3 scripts/audit_verify.py
    python3 scripts/audit_verify.py --path autoresearch/audit.jsonl

Exits 0 on success, 1 on chain violation. Used by:
- the pre-commit hook (.git/hooks/pre-commit) so audit log
  tampering is caught before it lands in git
- ad hoc by humans to confirm the log is intact
"""
from __future__ import annotations

import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from autoresearch.audit import AuditLog, DEFAULT_LOG_PATH


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--path", type=Path, default=DEFAULT_LOG_PATH)
    args = parser.parse_args()

    log = AuditLog(path=args.path)
    try:
        n = log.verify()
    except RuntimeError as exc:
        print(f"AUDIT LOG CHAIN VIOLATION: {exc}", file=sys.stderr)
        return 1
    print(f"audit log verified: {n} entries, chain intact")
    return 0


if __name__ == "__main__":
    sys.exit(main())
