"""Reconcile a Polymarket wallet's funding path against saved P&L artifacts.

Requires:
  ETHERSCAN_API_KEY=<key>

Example:
  ETHERSCAN_API_KEY=... python3 scripts/reconcile_whale_funding.py \
    --wallet 0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82 \
    --current-cash-usdc 177837.973835
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time
from collections import defaultdict
from dataclasses import asdict, dataclass
from datetime import UTC, datetime
from decimal import Decimal, getcontext
from pathlib import Path
from typing import Any

import httpx

from research.wallet_aliases import wallet_dir_name

getcontext().prec = 28

ETHERSCAN_V2 = "https://api.etherscan.io/v2/api"
POLYGON_CHAIN_ID = "137"
USDC_E_CONTRACT = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174"
DEFAULT_WALLET = "0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82"
ROOT = Path(__file__).resolve().parent.parent
WHALE_DIR = ROOT / "data" / "research" / "whale_analysis"
KNOWN_PROTOCOL_ADDRESSES = {
    "0x4bfb41d5b3570defd03c39a9a4d8de6bd8b8982e": "ctf_exchange",
    "0x4d97dcd97ec945f40cf65f87097ace5ea0476045": "ctf_contract",
}
KNOWN_EXTERNAL_SERVICE_ADDRESSES = {
    "0xf70da97812cb96acdf810712aa562db8dfa3dbef": "relay_solver",
}
PROTOCOL_FUNCTION_MARKERS = (
    "matchorders",
    "mergepositions",
    "splitposition",
    "redeempositions",
)
REQUEST_DELAY_SECONDS = 0.4
MAX_REQUEST_RETRIES = 5


def _d(value: Any) -> Decimal:
    if isinstance(value, Decimal):
        return value
    return Decimal(str(value))


def _iso_utc(ts: int | str) -> str:
    return datetime.fromtimestamp(int(ts), tz=UTC).isoformat()


@dataclass(frozen=True)
class Transfer:
    timestamp: int
    tx_hash: str
    block_number: int
    from_address: str
    to_address: str
    amount: Decimal
    token_symbol: str
    direction: str
    method: str
    function_name: str
    counterparty: str
    flow_class: str
    flow_reason: str
    activity_types: tuple[str, ...]
    activity_usdc: Decimal | None

    @property
    def date_utc(self) -> str:
        return datetime.fromtimestamp(self.timestamp, tz=UTC).strftime("%Y-%m-%d")


def _row_key(row: dict[str, Any]) -> tuple[str, ...]:
    return (
        str(row.get("blockNumber") or ""),
        str(row.get("hash") or ""),
        str(row.get("transactionIndex") or ""),
        str(row.get("from") or "").lower(),
        str(row.get("to") or "").lower(),
        str(row.get("value") or ""),
        str(row.get("tokenDecimal") or ""),
        str(row.get("input") or ""),
    )


def checkpoint_paths(wallet: str) -> tuple[Path, Path]:
    checkpoint_dir = WHALE_DIR / wallet_dir_name(wallet) / "etherscan_checkpoint"
    return checkpoint_dir / "rows.jsonl", checkpoint_dir / "state.json"


def load_checkpoint(wallet: str) -> tuple[list[dict[str, Any]], set[tuple[str, ...]], int, int]:
    rows_path, state_path = checkpoint_paths(wallet)
    if not rows_path.exists() or not state_path.exists():
        return [], set(), 0, 1

    rows: list[dict[str, Any]] = []
    seen_row_keys: set[tuple[str, ...]] = set()
    with rows_path.open() as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            row = json.loads(line)
            rows.append(row)
            seen_row_keys.add(_row_key(row))

    state = load_json(state_path)
    return rows, seen_row_keys, int(state.get("start_block") or 0), int(state.get("page") or 1)


def save_checkpoint(
    wallet: str,
    rows_to_append: list[dict[str, Any]],
    *,
    start_block: int,
    page: int,
    total_rows: int,
) -> None:
    rows_path, state_path = checkpoint_paths(wallet)
    rows_path.parent.mkdir(parents=True, exist_ok=True)
    if rows_to_append:
        with rows_path.open("a") as handle:
            for row in rows_to_append:
                handle.write(json.dumps(row))
                handle.write("\n")
    state_path.write_text(
        json.dumps(
            {
                "wallet": wallet,
                "start_block": start_block,
                "page": page,
                "total_rows": total_rows,
                "updated_at": datetime.now(tz=UTC).isoformat(),
            },
            indent=2,
        )
    )


def fetch_token_transfers(
    wallet: str,
    contract: str,
    api_key: str,
    page_size: int = 1000,
) -> list[dict[str, Any]]:
    rows, seen_row_keys, start_block, page = load_checkpoint(wallet)
    timeout = httpx.Timeout(120.0, connect=30.0)
    with httpx.Client(timeout=timeout) as client:
        while True:
            for attempt in range(1, MAX_REQUEST_RETRIES + 1):
                try:
                    resp = client.get(
                        ETHERSCAN_V2,
                        params={
                            "chainid": POLYGON_CHAIN_ID,
                            "module": "account",
                            "action": "tokentx",
                            "contractaddress": contract,
                            "address": wallet,
                            "startblock": start_block,
                            "endblock": 99_999_999,
                            "page": page,
                            "offset": page_size,
                            "sort": "asc",
                            "apikey": api_key,
                        },
                    )
                    resp.raise_for_status()
                    break
                except (httpx.ReadTimeout, httpx.ConnectTimeout, httpx.ConnectError):
                    if attempt == MAX_REQUEST_RETRIES:
                        raise
                    time.sleep(min(2.0 * attempt, 8.0))
            payload = resp.json()
            status = payload.get("status")
            result = payload.get("result")
            if status != "1":
                message = payload.get("message")
                # Etherscan caps page*offset at 10k. Roll the window forward by block.
                if "Result window is too large" in str(message):
                    if not rows:
                        raise RuntimeError("Etherscan window limit hit before any rows were fetched")
                    start_block = int(rows[-1]["blockNumber"])
                    page = 1
                    save_checkpoint(
                        wallet,
                        [],
                        start_block=start_block,
                        page=page,
                        total_rows=len(rows),
                    )
                    time.sleep(REQUEST_DELAY_SECONDS)
                    continue
                if "rate limit" in str(result).lower() or "rate limit" in str(message).lower():
                    time.sleep(1.0)
                    continue
                raise RuntimeError(f"Etherscan error on page {page}: {message} {result}")
            if not isinstance(result, list) or not result:
                break
            new_rows: list[dict[str, Any]] = []
            for row in result:
                row_key = _row_key(row)
                if row_key in seen_row_keys:
                    continue
                seen_row_keys.add(row_key)
                rows.append(row)
                new_rows.append(row)
            next_page = page + 1
            save_checkpoint(
                wallet,
                new_rows,
                start_block=start_block,
                page=next_page,
                total_rows=len(rows),
            )
            if len(result) < page_size:
                break
            page = next_page
            time.sleep(REQUEST_DELAY_SECONDS)
    return rows


def load_json(path: Path) -> Any:
    return json.loads(path.read_text())


def load_activity(wallet: str) -> list[dict[str, Any]]:
    candidates = [WHALE_DIR / wallet_dir_name(wallet) / "activity.json"]
    for path in candidates:
        if path.exists():
            return load_json(path)
    return []


def build_activity_index(rows: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    by_tx: dict[str, dict[str, Any]] = {}
    for row in rows:
        tx_hash = str(row.get("transactionHash") or "").lower()
        if not tx_hash:
            continue
        entry = by_tx.setdefault(
            tx_hash,
            {
                "types": set(),
                "expected_directions": set(),
                "usdc_total": Decimal("0"),
            },
        )
        activity_type = str(row.get("type") or "").upper()
        side = str(row.get("side") or "").upper()
        entry["types"].add(activity_type)
        entry["usdc_total"] += _d(row.get("usdcSize") or "0")
        if activity_type == "TRADE":
            if side == "BUY":
                entry["expected_directions"].add("out")
            elif side == "SELL":
                entry["expected_directions"].add("in")
        elif activity_type == "SPLIT":
            entry["expected_directions"].add("out")
        elif activity_type in {"MERGE", "REDEEM"}:
            entry["expected_directions"].add("in")
    return by_tx


def classify_transfer(
    transfer: Transfer,
    activity_by_tx: dict[str, dict[str, Any]],
) -> tuple[str, str, tuple[str, ...], Decimal | None]:
    if transfer.direction == "self":
        return "self", "self_transfer", (), None

    function_name = transfer.function_name.lower()
    if any(marker in function_name for marker in PROTOCOL_FUNCTION_MARKERS):
        return "protocol", "protocol_function", (), None

    activity = activity_by_tx.get(transfer.tx_hash)
    if activity:
        activity_types = tuple(sorted(activity["types"]))
        activity_usdc = activity["usdc_total"]
        if transfer.direction in activity["expected_directions"]:
            return "protocol", "activity_tx_match", activity_types, activity_usdc
        return "unknown", "activity_tx_sign_mismatch", activity_types, activity_usdc

    if transfer.counterparty in KNOWN_PROTOCOL_ADDRESSES:
        return "protocol", KNOWN_PROTOCOL_ADDRESSES[transfer.counterparty], (), None

    if transfer.counterparty in KNOWN_EXTERNAL_SERVICE_ADDRESSES:
        return "likely_external", KNOWN_EXTERNAL_SERVICE_ADDRESSES[transfer.counterparty], (), None

    return "likely_external", "unmatched_counterparty", (), None


def normalize_transfers(
    wallet: str,
    rows: list[dict[str, Any]],
    activity_by_tx: dict[str, dict[str, Any]],
) -> list[Transfer]:
    wallet_l = wallet.lower()
    transfers: list[Transfer] = []
    for row in rows:
        decimals = int(row.get("tokenDecimal") or "0")
        amount = _d(row.get("value") or "0") / (Decimal(10) ** decimals)
        from_addr = str(row.get("from") or "").lower()
        to_addr = str(row.get("to") or "").lower()
        if to_addr == wallet_l and from_addr != wallet_l:
            direction = "in"
            counterparty = from_addr
        elif from_addr == wallet_l and to_addr != wallet_l:
            direction = "out"
            counterparty = to_addr
        else:
            direction = "self"
            counterparty = to_addr or from_addr
        transfer = Transfer(
            timestamp=int(row["timeStamp"]),
            tx_hash=str(row["hash"]).lower(),
            block_number=int(row["blockNumber"]),
            from_address=from_addr,
            to_address=to_addr,
            amount=amount,
            token_symbol=str(row.get("tokenSymbol") or ""),
            direction=direction,
            method=str(row.get("methodId") or ""),
            function_name=str(row.get("functionName") or ""),
            counterparty=counterparty,
            flow_class="unknown",
            flow_reason="",
            activity_types=(),
            activity_usdc=None,
        )
        flow_class, flow_reason, activity_types, activity_usdc = classify_transfer(
            transfer=transfer,
            activity_by_tx=activity_by_tx,
        )
        payload = asdict(transfer)
        payload["flow_class"] = flow_class
        payload["flow_reason"] = flow_reason
        payload["activity_types"] = activity_types
        payload["activity_usdc"] = activity_usdc
        transfers.append(
            Transfer(**payload)
        )
    return transfers


def summarize(
    wallet: str,
    current_cash_usdc: Decimal | None,
    transfers: list[Transfer],
) -> dict[str, Any]:
    external_in = [t for t in transfers if t.flow_class == "likely_external" and t.direction == "in"]
    external_out = [t for t in transfers if t.flow_class == "likely_external" and t.direction == "out"]
    protocol = [t for t in transfers if t.flow_class == "protocol"]
    unknown = [t for t in transfers if t.flow_class == "unknown"]
    self_moves = [t for t in transfers if t.flow_class == "self"]
    total_in = sum((t.amount for t in external_in), Decimal("0"))
    total_out = sum((t.amount for t in external_out), Decimal("0"))
    net_external = total_in - total_out

    running = Decimal("0")
    by_day: dict[str, Decimal] = {}
    for t in transfers:
        if t.flow_class != "likely_external":
            continue
        if t.direction == "in":
            running += t.amount
        elif t.direction == "out":
            running -= t.amount
        by_day[t.date_utc] = running

    wallet_dir = WHALE_DIR / wallet_dir_name(wallet)
    pnl_path = wallet_dir / "pnl_timeseries.json"
    daily_pnl_path = wallet_dir / "daily_pnl.json"
    pnl_series = load_json(pnl_path) if pnl_path.exists() else []
    daily_pnl = load_json(daily_pnl_path) if daily_pnl_path.exists() else {}
    current_pnl = _d(pnl_series[-1]["p"]) if pnl_series else None

    implied_net_withdrawals = None
    if current_cash_usdc is not None and current_pnl is not None:
        # If current equity is approximated by visible USDC balance, then:
        # withdrawals - deposits = starting + pnl - ending_equity
        implied_net_withdrawals = total_out - total_in

    summary = {
        "wallet": wallet,
        "usdc_contract": USDC_E_CONTRACT,
        "transfer_count": len(transfers),
        "likely_external_deposit_count": len(external_in),
        "likely_external_withdrawal_count": len(external_out),
        "protocol_transfer_count": len(protocol),
        "unknown_transfer_count": len(unknown),
        "self_move_count": len(self_moves),
        "first_transfer_at": _iso_utc(transfers[0].timestamp) if transfers else None,
        "last_transfer_at": _iso_utc(transfers[-1].timestamp) if transfers else None,
        "likely_external_deposits_usdc": float(total_in),
        "likely_external_withdrawals_usdc": float(total_out),
        "net_likely_external_capital_usdc": float(net_external),
        "protocol_inflow_usdc": float(
            sum((t.amount for t in protocol if t.direction == "in"), Decimal("0"))
        ),
        "protocol_outflow_usdc": float(
            sum((t.amount for t in protocol if t.direction == "out"), Decimal("0"))
        ),
        "unknown_inflow_usdc": float(
            sum((t.amount for t in unknown if t.direction == "in"), Decimal("0"))
        ),
        "unknown_outflow_usdc": float(
            sum((t.amount for t in unknown if t.direction == "out"), Decimal("0"))
        ),
        "current_cash_usdc": float(current_cash_usdc) if current_cash_usdc is not None else None,
        "current_pnl_usdc": float(current_pnl) if current_pnl is not None else None,
        "daily_net_likely_external_capital_usdc": {k: float(v) for k, v in sorted(by_day.items())},
        "largest_likely_external_deposits": [
            {
                "timestamp": _iso_utc(t.timestamp),
                "amount_usdc": float(t.amount),
                "tx_hash": t.tx_hash,
                "from": t.from_address,
                "reason": t.flow_reason,
            }
            for t in sorted(external_in, key=lambda x: x.amount, reverse=True)[:10]
        ],
        "largest_likely_external_withdrawals": [
            {
                "timestamp": _iso_utc(t.timestamp),
                "amount_usdc": float(t.amount),
                "tx_hash": t.tx_hash,
                "to": t.to_address,
                "reason": t.flow_reason,
            }
            for t in sorted(external_out, key=lambda x: x.amount, reverse=True)[:10]
        ],
        "first_likely_external_funding_txs": [
            {
                "timestamp": _iso_utc(t.timestamp),
                "amount_usdc": float(t.amount),
                "tx_hash": t.tx_hash,
                "from": t.from_address,
                "reason": t.flow_reason,
            }
            for t in external_in[:10]
        ],
        "top_likely_external_counterparties": [
            {"counterparty": counterparty, "net_usdc": float(amount)}
            for counterparty, amount in sorted(
                _net_by_counterparty([t for t in transfers if t.flow_class == "likely_external"]).items(),
                key=lambda item: abs(item[1]),
                reverse=True,
            )[:10]
        ],
        "top_protocol_counterparties": [
            {"counterparty": counterparty, "net_usdc": float(amount)}
            for counterparty, amount in sorted(
                _net_by_counterparty(protocol).items(),
                key=lambda item: abs(item[1]),
                reverse=True,
            )[:10]
        ],
        "saved_daily_pnl_usdc": daily_pnl,
        "implied_net_withdrawals_minus_deposits_usdc": float(implied_net_withdrawals)
        if implied_net_withdrawals is not None
        else None,
    }
    return summary


def render_report(summary: dict[str, Any]) -> str:
    lines: list[str] = []
    lines.append(f"Wallet: {summary['wallet']}")
    lines.append(f"Transfers: {summary['transfer_count']}")
    lines.append(
        "Likely external deposits / withdrawals / protocol / unknown / self: "
        f"{summary['likely_external_deposit_count']} / "
        f"{summary['likely_external_withdrawal_count']} / "
        f"{summary['protocol_transfer_count']} / "
        f"{summary['unknown_transfer_count']} / "
        f"{summary['self_move_count']}"
    )
    lines.append(
        "Likely external capital (USDC.e): "
        f"in={summary['likely_external_deposits_usdc']:.2f} "
        f"out={summary['likely_external_withdrawals_usdc']:.2f} "
        f"net={summary['net_likely_external_capital_usdc']:.2f}"
    )
    lines.append(
        "Protocol settlement (USDC.e): "
        f"in={summary['protocol_inflow_usdc']:.2f} "
        f"out={summary['protocol_outflow_usdc']:.2f}"
    )
    if summary["current_cash_usdc"] is not None:
        lines.append(f"Current visible cash (USDC.e): {summary['current_cash_usdc']:.6f}")
    if summary["current_pnl_usdc"] is not None:
        lines.append(f"Current Polymarket P&L: {summary['current_pnl_usdc']:.2f}")
    if summary["first_transfer_at"]:
        lines.append(f"First transfer: {summary['first_transfer_at']}")
        lines.append(f"Last transfer:  {summary['last_transfer_at']}")

    lines.append("")
    lines.append("Largest likely external deposits:")
    for row in summary["largest_likely_external_deposits"][:5]:
        lines.append(
            f"  {row['timestamp']}  +${row['amount_usdc']:.2f}  "
            f"{row['from']}  {row['tx_hash']}"
        )

    lines.append("")
    lines.append("Largest likely external withdrawals:")
    for row in summary["largest_likely_external_withdrawals"][:5]:
        lines.append(
            f"  {row['timestamp']}  -${row['amount_usdc']:.2f}  "
            f"{row['to']}  {row['tx_hash']}"
        )
    return "\n".join(lines)


def _net_by_counterparty(transfers: list[Transfer]) -> dict[str, Decimal]:
    by_counterparty: dict[str, Decimal] = defaultdict(lambda: Decimal("0"))
    for t in transfers:
        if t.direction == "in":
            by_counterparty[t.counterparty] += t.amount
        elif t.direction == "out":
            by_counterparty[t.counterparty] -= t.amount
    return by_counterparty


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--wallet", default=DEFAULT_WALLET)
    ap.add_argument("--contract", default=USDC_E_CONTRACT)
    ap.add_argument("--current-cash-usdc", type=Decimal, default=None)
    args = ap.parse_args()

    api_key = os.environ.get("ETHERSCAN_API_KEY", "").strip()
    if not api_key:
        raise SystemExit("ETHERSCAN_API_KEY is required")

    wallet = args.wallet.lower()
    activity_by_tx = build_activity_index(load_activity(wallet))
    rows = fetch_token_transfers(wallet=wallet, contract=args.contract, api_key=api_key)
    transfers = normalize_transfers(wallet=wallet, rows=rows, activity_by_tx=activity_by_tx)
    summary = summarize(wallet=wallet, current_cash_usdc=args.current_cash_usdc, transfers=transfers)

    dir_name = wallet_dir_name(wallet)
    out_dir = WHALE_DIR / dir_name
    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / f"usdc_transfers_{dir_name}.json").write_text(
        json.dumps([asdict(t) | {"amount": str(t.amount)} for t in transfers], indent=2)
    )
    (out_dir / f"funding_summary_{dir_name}.json").write_text(json.dumps(summary, indent=2))
    print(render_report(summary))
    print("")
    print(f"Saved: {out_dir / f'usdc_transfers_{dir_name}.json'}")
    print(f"Saved: {out_dir / f'funding_summary_{dir_name}.json'}")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
