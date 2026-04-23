"""Remove likely duplicate same-strategy trade rows from Supabase.

Only collapses rows that match on the same strategy execution signature within
the same second. Cross-strategy rows are preserved.
"""
from __future__ import annotations

import argparse
import json
from collections import defaultdict
from datetime import datetime, timezone
from uuid import uuid5, NAMESPACE_URL

from shared.supabase_client import SupabaseClient


def parse_dt(value: str | None) -> datetime | None:
    if not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def second_bucket(value: str) -> str:
    dt = parse_dt(value)
    if dt is None:
        return value
    return dt.astimezone(timezone.utc).replace(microsecond=0).isoformat()


def rounded(value: float | int | None, digits: int = 6) -> float | None:
    if value is None:
        return None
    return round(float(value), digits)


def trade_key(row: dict) -> tuple:
    return (
        row.get("coin"),
        row.get("strategy"),
        row.get("market_id"),
        row.get("direction"),
        second_bucket(row.get("created_at")),
        rounded(row.get("token_price")),
        rounded(row.get("size_usd")),
        rounded(row.get("shares")),
    )


def choose_canonical(rows: list[dict]) -> dict:
    def sort_key(row: dict) -> tuple:
        created_at = row.get("created_at") or ""
        return (
            1 if row.get("resolved_at") else 0,
            len(created_at),
            1 if "." in created_at else 0,
            row.get("id") or "",
        )

    return max(rows, key=sort_key)


def is_legacy_timestamp_id(row: dict) -> bool:
    trade_id = str(row.get("id") or "")
    market_id = str(row.get("market_id") or "")
    marker = f"-{market_id}-"
    if marker not in trade_id:
        return False
    suffix = trade_id.rsplit(marker, 1)[-1]
    return parse_dt(suffix) is not None


def migrated_id(row: dict) -> str:
    legacy_id = row["id"]
    deterministic_uuid = uuid5(NAMESPACE_URL, legacy_id).hex
    return f"{row['coin']}-{row['strategy']}-{row['market_id']}-{deterministic_uuid}"


def format_group(rows: list[dict]) -> dict:
    canonical = choose_canonical(rows)
    duplicates = [row for row in rows if row["id"] != canonical["id"]]
    return {
        "canonical_id": canonical["id"],
        "canonical_migrated_id": migrated_id(canonical) if is_legacy_timestamp_id(canonical) else canonical["id"],
        "duplicate_ids": [row["id"] for row in duplicates],
        "coin": canonical.get("coin"),
        "strategy": canonical.get("strategy"),
        "market_id": canonical.get("market_id"),
        "direction": canonical.get("direction"),
        "created_at": canonical.get("created_at"),
        "token_price": canonical.get("token_price"),
        "size_usd": canonical.get("size_usd"),
        "shares": canonical.get("shares"),
        "rows": len(rows),
    }


def delete_rows(client: SupabaseClient, ids: list[str]) -> None:
    if not ids:
        return
    chunk_size = 100
    for start in range(0, len(ids), chunk_size):
        chunk = ids[start:start + chunk_size]
        quoted = ",".join(json.dumps(str(value)) for value in chunk)
        resp = client._http.delete("/trades", params={"id": f"in.({quoted})"})
        resp.raise_for_status()


def upsert_rows(client: SupabaseClient, rows: list[dict]) -> None:
    if not rows:
        return
    client._upsert("trades", rows, "id")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--apply", action="store_true", help="Delete duplicate rows instead of printing them")
    args = parser.parse_args()

    client = SupabaseClient()
    try:
        trades = client.load_trades()
        grouped: dict[tuple, list[dict]] = defaultdict(list)
        for row in trades:
            grouped[trade_key(row)].append(row)

        dup_groups = [format_group(rows) for rows in grouped.values() if len(rows) > 1]
        dup_groups.sort(key=lambda row: (row["coin"], row["strategy"], row["created_at"], row["market_id"]))

        duplicate_ids = [dup_id for group in dup_groups for dup_id in group["duplicate_ids"]]

        canonical_by_key = {key: choose_canonical(rows) for key, rows in grouped.items()}
        retained_rows = list(canonical_by_key.values())
        rows_to_migrate = [row for row in retained_rows if is_legacy_timestamp_id(row)]
        migrated_rows = [
            {**row, "id": migrated_id(row)}
            for row in rows_to_migrate
        ]
        migrated_old_ids = [row["id"] for row in rows_to_migrate]
        rows_to_delete = sorted(set(duplicate_ids + migrated_old_ids))

        summary = {
            "total_rows": len(trades),
            "duplicate_groups": len(dup_groups),
            "duplicate_rows_to_remove": len(duplicate_ids),
            "legacy_rows_to_migrate": len(rows_to_migrate),
            "total_rows_to_delete": len(rows_to_delete),
            "groups": dup_groups,
        }
        print(json.dumps(summary, indent=2))

        if args.apply:
            upsert_rows(client, migrated_rows)
            delete_rows(client, rows_to_delete)
            print(json.dumps({
                "migrated_rows": len(migrated_rows),
                "deleted_rows": len(rows_to_delete),
            }, indent=2))
    finally:
        client.close()


if __name__ == "__main__":
    main()
