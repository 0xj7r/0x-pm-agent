"""Promote a reviewed candidate artifact into a named strategy profile."""
from __future__ import annotations

import argparse
import json
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("candidate", help="Path to candidate JSON artifact")
    parser.add_argument("--config", default="strategy_config.json")
    parser.add_argument("--profile", default="default")
    args = parser.parse_args()

    candidate = json.loads(Path(args.candidate).read_text())
    config_path = Path(args.config)
    config = json.loads(config_path.read_text())

    profiles = config.setdefault("profiles", {})
    if args.profile not in profiles:
        raise ValueError(f"Unknown profile: {args.profile}")

    patch = candidate["profile_patch"]
    profile = profiles[args.profile]
    profile.setdefault("coins", {}).update(patch.get("coins", {}))
    config_path.write_text(json.dumps(config, indent=2) + "\n")
    print(f"Promoted {args.candidate} into profile {args.profile}")


if __name__ == "__main__":
    main()
