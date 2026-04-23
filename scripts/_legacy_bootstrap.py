"""Shared helpers for legacy top-level script entrypoints.

These wrappers keep compatibility with current invocations while centralizing
path bootstrapping and entrypoint dispatch logic.
"""

from __future__ import annotations

from importlib import import_module
from pathlib import Path
import sys


def _ensure_repo_root() -> None:
    root = Path(__file__).resolve().parent.parent
    root_str = str(root)
    if root_str not in sys.path:
        sys.path.insert(0, root_str)


def run_module_entrypoint(module_name: str, fn_name: str = "main") -> None:
    """Import *module_name* and invoke *fn_name*."""
    _ensure_repo_root()
    module = import_module(module_name)
    fn = getattr(module, fn_name)
    fn()
