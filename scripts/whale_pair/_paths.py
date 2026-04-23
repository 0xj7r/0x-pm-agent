"""Path helpers shared by whale-pair tooling."""

from __future__ import annotations

from pathlib import Path


def repo_root(anchor: Path | str | None = None) -> Path:
    """Resolve repository root from an anchor path.

    The function intentionally handles files nested in `scripts/whale_pair/...`.
    """

    path = Path(anchor).resolve() if anchor is not None else Path.cwd().resolve()
    for candidate in (path,) + tuple(path.parents):
        if (candidate / ".git").exists() and (candidate / "scripts").is_dir():
            return candidate
    # Fallback for environments where .git is unavailable.
    return Path(__file__).resolve().parents[3]

