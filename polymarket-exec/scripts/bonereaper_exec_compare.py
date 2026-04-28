"""Compare a wallet's actual fills against our shadow-live execution.

For each fill the wallet executed in the comparison window, the script joins:
  - the venue book snapshot we recorded for that token at fill time
  - any of our own intents/orders on that same token within +/-5s

and classifies each fill as we_didnt_quote / we_quoted_tighter / we_quoted_wider /
we_quoted_same / market_unknown. Emits a per-fill JSONL detail file plus a
human-readable rollup by market-family (e.g. eth-updown-5m, btc-updown-15m).

Inputs are JSONL streams the engine already produces in shadow-live mode:
  - book snapshots: {t (ms), asset, bids, asks, last_trade}
  - journal events: {seq, observed_at_ms, category, message, market_id,
                     instrument_id, client_order_id, metrics:{price, quantity, ...}}

The wallet's fills come from data-api.polymarket.com /activity (epoch seconds).
The asset/token_id in /activity matches the asset field in our book snapshots,
so the join doesn't need a conditionId-to-market_id lookup.
"""
from __future__ import annotations

import argparse
import bisect
import json
import sys
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable

import httpx

DATA_API = "https://data-api.polymarket.com"
TICK = 0.01
JOIN_WINDOW_MS = 5_000


@dataclass
class WalletFill:
    timestamp_ms: int
    asset: str
    side: str
    outcome: str
    price: float
    size: float
    usdc_size: float
    slug: str
    condition_id: str


@dataclass
class BookSnap:
    t_ms: int
    asset: str
    bids: list[tuple[float, float]]
    asks: list[tuple[float, float]]
    last_trade: float

    def best_bid(self) -> float | None:
        return self.bids[0][0] if self.bids else None

    def best_ask(self) -> float | None:
        return self.asks[0][0] if self.asks else None

    def mid(self) -> float | None:
        bb, ba = self.best_bid(), self.best_ask()
        if bb is not None and ba is not None:
            return (bb + ba) / 2.0
        return self.last_trade or None


@dataclass
class OurIntent:
    """One emitted intent from our journal stream."""

    observed_at_ms: int
    instrument_id: str
    market_id: str
    client_order_id: str
    price: float
    quantity: float


@dataclass
class FamilyStats:
    fills: int = 0
    by_class: Counter = field(default_factory=Counter)
    his_avg_price: float = 0.0
    our_avg_price: float = 0.0
    our_quoted_count: int = 0


def fetch_wallet_activity(
    wallet: str, from_ts: int, to_ts: int, page_limit: int = 500
) -> list[WalletFill]:
    fills: list[WalletFill] = []
    offset = 0
    with httpx.Client(timeout=30.0) as client:
        while True:
            resp = client.get(
                f"{DATA_API}/activity",
                params={
                    "user": wallet,
                    "limit": str(page_limit),
                    "offset": str(offset),
                    "sortBy": "TIMESTAMP",
                    "sortDirection": "DESC",
                },
            )
            resp.raise_for_status()
            payload = resp.json()
            if not isinstance(payload, list) or not payload:
                break
            stop = False
            for row in payload:
                if row.get("type") != "TRADE":
                    continue
                ts = int(row.get("timestamp", 0))
                if ts < from_ts:
                    stop = True
                    continue
                if ts > to_ts:
                    continue
                fills.append(
                    WalletFill(
                        timestamp_ms=ts * 1000,
                        asset=str(row.get("asset", "")),
                        side=str(row.get("side", "")),
                        outcome=str(row.get("outcome", "")),
                        price=float(row.get("price", 0.0)),
                        size=float(row.get("size", 0.0)),
                        usdc_size=float(row.get("usdcSize", 0.0)),
                        slug=str(row.get("slug", "")),
                        condition_id=str(row.get("conditionId", "")),
                    )
                )
            if stop or len(payload) < page_limit:
                break
            offset += page_limit
    fills.sort(key=lambda f: f.timestamp_ms)
    return fills


def load_book_snapshots(path: Path) -> dict[str, list[BookSnap]]:
    by_asset: dict[str, list[BookSnap]] = defaultdict(list)
    with path.open("r") as fp:
        for line in fp:
            line = line.strip()
            if not line:
                continue
            row = json.loads(line)
            snap = BookSnap(
                t_ms=int(row["t"]),
                asset=str(row["asset"]),
                bids=[(float(p), float(q)) for p, q in row.get("bids", [])],
                asks=[(float(p), float(q)) for p, q in row.get("asks", [])],
                last_trade=float(row.get("last_trade", 0.0) or 0.0),
            )
            by_asset[snap.asset].append(snap)
    for asset_snaps in by_asset.values():
        asset_snaps.sort(key=lambda s: s.t_ms)
    return by_asset


def load_our_intents(path: Path, strategy_tag: str) -> dict[str, list[OurIntent]]:
    """Index our submitted orders by instrument_id from journal events.

    The runtime emits two relevant event lines per order:
      - on submit: client_order_id, instrument_id, metrics.price/quantity set
      - on transition: same client_order_id (price/qty already known)

    We only count one entry per (client_order_id, instrument_id, price, qty)
    so reposts at new prices are recorded as separate intents.
    """
    seen: set[tuple[str, str, float, float]] = set()
    by_instrument: dict[str, list[OurIntent]] = defaultdict(list)
    with path.open("r") as fp:
        for line in fp:
            line = line.strip()
            if not line:
                continue
            try:
                envelope = json.loads(line)
            except json.JSONDecodeError:
                continue
            if envelope.get("kind") != "runtime_event":
                continue
            record = envelope.get("record")
            if not record:
                continue
            client_order_id = record.get("client_order_id") or ""
            if not client_order_id.startswith(f"{strategy_tag}:"):
                continue
            instrument_id = record.get("instrument_id") or ""
            if not instrument_id:
                continue
            metrics = record.get("metrics") or {}
            price = metrics.get("price")
            quantity = metrics.get("quantity")
            if price is None or quantity is None:
                continue
            key = (client_order_id, instrument_id, float(price), float(quantity))
            if key in seen:
                continue
            seen.add(key)
            by_instrument[instrument_id].append(
                OurIntent(
                    observed_at_ms=int(record.get("observed_at_ms", 0)),
                    instrument_id=instrument_id,
                    market_id=record.get("market_id") or "",
                    client_order_id=client_order_id,
                    price=float(price),
                    quantity=float(quantity),
                )
            )
    for intents in by_instrument.values():
        intents.sort(key=lambda i: i.observed_at_ms)
    return by_instrument


def closest_at_or_before(items_ms: list[int], target_ms: int) -> int | None:
    if not items_ms:
        return None
    idx = bisect.bisect_right(items_ms, target_ms) - 1
    if idx < 0:
        return None
    return idx


def intents_in_window(
    intents: list[OurIntent], target_ms: int, half_window_ms: int
) -> list[OurIntent]:
    if not intents:
        return []
    lo = bisect.bisect_left(
        [i.observed_at_ms for i in intents], target_ms - half_window_ms
    )
    hi = bisect.bisect_right(
        [i.observed_at_ms for i in intents], target_ms + half_window_ms
    )
    return intents[lo:hi]


def family_of(slug: str) -> str:
    parts = slug.split("-")
    if len(parts) >= 3:
        return "-".join(parts[:3])
    return slug or "unknown"


def classify(fill: WalletFill, our_match: OurIntent | None) -> str:
    if our_match is None:
        return "we_didnt_quote"
    delta = our_match.price - fill.price
    if abs(delta) <= TICK / 2.0:
        return "we_quoted_same"
    return "we_quoted_tighter" if delta > 0 else "we_quoted_wider"


def compare(
    fills: list[WalletFill],
    snapshots: dict[str, list[BookSnap]],
    intents: dict[str, list[OurIntent]],
) -> tuple[list[dict], dict[str, FamilyStats]]:
    detail: list[dict] = []
    family_stats: dict[str, FamilyStats] = defaultdict(FamilyStats)

    for fill in fills:
        family = family_of(fill.slug)
        stats = family_stats[family]
        stats.fills += 1
        stats.his_avg_price += fill.price

        snap_list = snapshots.get(fill.asset, [])
        snap_ts = [s.t_ms for s in snap_list]
        snap_idx = closest_at_or_before(snap_ts, fill.timestamp_ms)
        snap = snap_list[snap_idx] if snap_idx is not None else None

        candidate_intents = intents_in_window(
            intents.get(fill.asset, []), fill.timestamp_ms, JOIN_WINDOW_MS
        )
        # Pick the closest in time on the same instrument.
        our_match: OurIntent | None = None
        if candidate_intents:
            our_match = min(
                candidate_intents,
                key=lambda i: abs(i.observed_at_ms - fill.timestamp_ms),
            )

        # If we never even saw this market in our discovery, that's a
        # different signal than "saw the market but didn't quote here".
        market_known = snap is not None or fill.asset in intents
        cls = classify(fill, our_match) if market_known else "market_unknown"

        if our_match is not None:
            stats.our_avg_price += our_match.price
            stats.our_quoted_count += 1
        stats.by_class[cls] += 1

        detail.append(
            {
                "ts_ms": fill.timestamp_ms,
                "slug": fill.slug,
                "family": family,
                "asset": fill.asset,
                "outcome": fill.outcome,
                "his_price": fill.price,
                "his_size": fill.size,
                "his_usdc": fill.usdc_size,
                "book_mid_at_fill": snap.mid() if snap else None,
                "book_best_bid": snap.best_bid() if snap else None,
                "book_best_ask": snap.best_ask() if snap else None,
                "our_price": our_match.price if our_match else None,
                "our_qty": our_match.quantity if our_match else None,
                "our_lag_ms": (
                    our_match.observed_at_ms - fill.timestamp_ms if our_match else None
                ),
                "classification": cls,
            }
        )

    for stats in family_stats.values():
        if stats.fills:
            stats.his_avg_price /= stats.fills
        if stats.our_quoted_count:
            stats.our_avg_price /= stats.our_quoted_count

    return detail, family_stats


def render_summary(family_stats: dict[str, FamilyStats]) -> str:
    lines: list[str] = []
    lines.append("=" * 80)
    lines.append("Bonereaper exec comparison summary")
    lines.append("=" * 80)
    classes = [
        "we_quoted_same",
        "we_quoted_tighter",
        "we_quoted_wider",
        "we_didnt_quote",
        "market_unknown",
    ]
    header = f"{'family':<30} {'fills':>6} " + " ".join(
        f"{c[:14]:>14}" for c in classes
    ) + f"  {'his_avg':>8}  {'our_avg':>8}"
    lines.append(header)
    lines.append("-" * len(header))
    total_fills = 0
    total_by_class: Counter = Counter()
    for family, stats in sorted(family_stats.items()):
        cells = [f"{stats.by_class.get(c, 0):>14}" for c in classes]
        lines.append(
            f"{family:<30} {stats.fills:>6} {' '.join(cells)}"
            f"  {stats.his_avg_price:>8.4f}  {stats.our_avg_price:>8.4f}"
        )
        total_fills += stats.fills
        for c in classes:
            total_by_class[c] += stats.by_class.get(c, 0)
    lines.append("-" * len(header))
    cells = [f"{total_by_class.get(c, 0):>14}" for c in classes]
    lines.append(f"{'TOTAL':<30} {total_fills:>6} {' '.join(cells)}")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--wallet",
        default="0xeebde7a0e019a63e6b476eb425505b7b3e6eba30",
        help="wallet to compare (default: bonereaper)",
    )
    parser.add_argument(
        "--strategy-tag",
        default="bonereaper",
        help="strategy tag prefix in our client_order_ids",
    )
    parser.add_argument(
        "--book-snapshot-log",
        type=Path,
        required=True,
        help="path to book_snapshots.jsonl produced by shadow-live",
    )
    parser.add_argument(
        "--journal-log",
        type=Path,
        required=True,
        help="path to journal.jsonl produced by shadow-live",
    )
    parser.add_argument(
        "--from-ts",
        type=int,
        required=True,
        help="comparison window start (epoch seconds)",
    )
    parser.add_argument(
        "--to-ts",
        type=int,
        required=True,
        help="comparison window end (epoch seconds)",
    )
    parser.add_argument(
        "--detail-out",
        type=Path,
        default=None,
        help="optional JSONL output path for per-fill detail",
    )
    args = parser.parse_args()

    if not args.book_snapshot_log.exists():
        print(f"missing book snapshot log: {args.book_snapshot_log}", file=sys.stderr)
        return 2
    if not args.journal_log.exists():
        print(f"missing journal log: {args.journal_log}", file=sys.stderr)
        return 2

    print(
        f"fetching {args.wallet} fills in [{args.from_ts}, {args.to_ts}]...",
        file=sys.stderr,
    )
    fills = fetch_wallet_activity(args.wallet, args.from_ts, args.to_ts)
    print(f"  got {len(fills)} TRADE rows", file=sys.stderr)

    print("loading book snapshots...", file=sys.stderr)
    snapshots = load_book_snapshots(args.book_snapshot_log)
    print(
        f"  {sum(len(v) for v in snapshots.values())} rows across {len(snapshots)} assets",
        file=sys.stderr,
    )

    print("loading our intents from journal...", file=sys.stderr)
    intents = load_our_intents(args.journal_log, args.strategy_tag)
    print(
        f"  {sum(len(v) for v in intents.values())} intents across {len(intents)} assets",
        file=sys.stderr,
    )

    detail, family_stats = compare(fills, snapshots, intents)

    if args.detail_out is not None:
        args.detail_out.parent.mkdir(parents=True, exist_ok=True)
        with args.detail_out.open("w") as fp:
            for row in detail:
                fp.write(json.dumps(row) + "\n")
        print(f"wrote per-fill detail to {args.detail_out}", file=sys.stderr)

    print(render_summary(family_stats))
    return 0


if __name__ == "__main__":
    sys.exit(main())
