"""Verify that local and Docker runtime contract stay aligned."""
from __future__ import annotations

from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from strategies.strategy_config import list_strategy_profiles, load_strategy_config

CANONICAL_CONFIG = ROOT / "strategy_config.json"
COMPOSE_PATH = ROOT / "docker-compose.yml"


def main() -> int:
    profiles = list_strategy_profiles(str(CANONICAL_CONFIG))
    assert profiles, "No strategy profiles found"

    for profile in profiles:
        cfg = load_strategy_config(str(CANONICAL_CONFIG), profile=profile)
        assert cfg.profile == profile, f"Profile mismatch for {profile}"
        assert cfg.coins, f"No coins configured for profile {profile}"

    compose_text = COMPOSE_PATH.read_text()
    assert "strategy_config_relaxed.json" not in compose_text
    assert "strategy_config_low_thresh.json" not in compose_text
    assert "strategy_config_realmoney.json" not in compose_text
    assert "STRATEGY_CONFIG: /app/strategy_config.json" in compose_text
    assert "STRATEGY_PROFILE:" in compose_text
    print("runtime contract OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
