"""Summarize overlap and clustering signals across saved whale wallets.

Usage:
  python3 scripts/cluster_whale_wallets.py
"""
from __future__ import annotations

import json
from collections import Counter
from dataclasses import dataclass
from itertools import combinations
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
WHALE_DIR = ROOT / "data" / "whale_analysis"
OUT = ROOT / "data" / "wallet_research"


@dataclass(frozen=True)
class WalletProfile:
    wallet: str
    suffix: str
    pseudonym: str
    activity_path: Path
    rows: list[dict[str, Any]]

    @property
    def trade_rows(self) -> list[dict[str, Any]]:
        return [row for row in self.rows if row.get("type") == "TRADE"]

    @property
    def slugs(self) -> set[str]:
        return {str(row.get("slug") or "") for row in self.trade_rows if row.get("slug")}

    @property
    def condition_ids(self) -> set[str]:
        return {str(row.get("conditionId") or "") for row in self.rows if row.get("conditionId")}

    @property
    def type_counts(self) -> dict[str, int]:
        return dict(Counter(str(row.get("type") or "") for row in self.rows))


def load_profiles() -> list[WalletProfile]:
    profiles: list[WalletProfile] = []
    for activity_path in sorted(WHALE_DIR.glob("activity_*.json")):
        rows = json.loads(activity_path.read_text())
        if not rows:
            continue
        first = rows[0]
        wallet = str(first.get("proxyWallet") or "").lower()
        if not wallet:
            continue
        suffix = activity_path.stem.replace("activity_", "")
        profiles.append(
            WalletProfile(
                wallet=wallet,
                suffix=suffix,
                pseudonym=str(first.get("pseudonym") or first.get("name") or suffix),
                activity_path=activity_path,
                rows=rows,
            )
        )
    return profiles


def _jaccard(left: set[str], right: set[str]) -> float:
    if not left and not right:
        return 0.0
    return len(left & right) / len(left | right)


def pair_overlap(left: WalletProfile, right: WalletProfile) -> dict[str, Any]:
    shared_slugs = left.slugs & right.slugs
    shared_conditions = left.condition_ids & right.condition_ids

    left_trades = {(str(row.get("slug") or ""), int(row.get("timestamp") or 0)) for row in left.trade_rows}
    right_trades = {(str(row.get("slug") or ""), int(row.get("timestamp") or 0)) for row in right.trade_rows}

    synced_2s = 0
    synced_10s = 0
    right_by_slug: dict[str, list[int]] = {}
    for slug, ts in right_trades:
        right_by_slug.setdefault(slug, []).append(ts)
    for times in right_by_slug.values():
        times.sort()
    for slug, ts in left_trades:
        other = right_by_slug.get(slug, [])
        if not other:
            continue
        if any(abs(ts - rhs) <= 2 for rhs in other):
            synced_2s += 1
        if any(abs(ts - rhs) <= 10 for rhs in other):
            synced_10s += 1

    return {
        "left_wallet": left.wallet,
        "right_wallet": right.wallet,
        "left_label": left.pseudonym,
        "right_label": right.pseudonym,
        "shared_slugs": len(shared_slugs),
        "shared_condition_ids": len(shared_conditions),
        "slug_jaccard": _jaccard(left.slugs, right.slugs),
        "condition_jaccard": _jaccard(left.condition_ids, right.condition_ids),
        "synced_trade_rows_2s": synced_2s,
        "synced_trade_rows_10s": synced_10s,
    }


def summarize_profiles(profiles: list[WalletProfile]) -> list[dict[str, Any]]:
    summary: list[dict[str, Any]] = []
    for profile in profiles:
        summary.append(
            {
                "wallet": profile.wallet,
                "suffix": profile.suffix,
                "pseudonym": profile.pseudonym,
                "rows": len(profile.rows),
                "trade_rows": len(profile.trade_rows),
                "type_counts": profile.type_counts,
                "market_count": len(profile.slugs),
                "condition_count": len(profile.condition_ids),
            }
        )
    return summary


def main() -> None:
    profiles = load_profiles()
    pairwise = [pair_overlap(left, right) for left, right in combinations(profiles, 2)]
    output = {
        "wallets": summarize_profiles(profiles),
        "pairwise_overlap": pairwise,
    }
    OUT.mkdir(parents=True, exist_ok=True)
    output_path = OUT / "whale_wallet_clusters.json"
    output_path.write_text(json.dumps(output, indent=2))
    print(json.dumps({"wallet_count": len(profiles), "pair_count": len(pairwise), "output": str(output_path)}, indent=2))


if __name__ == "__main__":
    main()
