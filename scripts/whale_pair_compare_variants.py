#!/usr/bin/env python3
"""Legacy entrypoint for whale-pair compare variants.

This file remains for backwards compatibility and delegates to the
new canonical location under scripts/whale_pair/cmd.
"""

from __future__ import annotations

from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parent.parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from scripts.whale_pair.cmd.whale_pair_compare_variants import main


if __name__ == "__main__":
    main()

