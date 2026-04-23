#!/usr/bin/env python3
"""Legacy entrypoint for whale-pair walk-forward evaluation."""

from __future__ import annotations

if __name__ == "__main__":
    from scripts._legacy_bootstrap import run_module_entrypoint

    run_module_entrypoint("scripts.whale_pair.cmd.whale_pair_walkforward")
