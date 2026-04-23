#!/usr/bin/env python3
"""Legacy entrypoint for whale-pair walk-forward evaluation.

Delegates to scripts/whale_pair/cmd/whale_pair_walkforward.py.
"""

from __future__ import annotations

from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parent.parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from scripts.whale_pair.cmd.whale_pair_walkforward import main


if __name__ == "__main__":
    main()

