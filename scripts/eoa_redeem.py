#!/usr/bin/env python3
"""EOA-mode CTF redeem for Polymarket bot wallets.

Why this exists: our Rust auto-redeem only supports proxy/relayer
(SIGNATURE_TYPE=1) wallets which gives us gasless redemption. EOA-funded
bot wallets must call CTF.redeemPositions directly via Polygon RPC,
paying gas in MATIC. This script handles that path.

Pulls redeemable positions from Polymarket Data API for the wallet,
then for each unique condition_id submits a redeemPositions transaction
calling the CTF contract. Idempotent — already-redeemed conditions
revert with no side effects (we'll catch + skip).

Usage:
  ./scripts/eoa_redeem.py --wallet 0x97fBC6Bc... [--execute]
    --wallet:   EOA address (private key read from POLYMARKET_PRIVATE_KEY env)
    --execute:  actually submit (default: dry-run)

Requires: POLYMARKET_PRIVATE_KEY env (the EOA's private key, hex with 0x).
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.parse
import urllib.request
from typing import Iterable

from web3 import Web3
from eth_account import Account


POLYGON_RPC = os.getenv(
    "POLYGON_RPC_URL", "https://polygon-bor-rpc.publicnode.com"
)
DATA_API = "https://data-api.polymarket.com"

CTF_ADDRESS = Web3.to_checksum_address("0x4D97DCd97eC945f40cF65F87097ACe5EA0476045")
USDCE_ADDRESS = Web3.to_checksum_address(
    "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174"
)

# function redeemPositions(IERC20 collateralToken, bytes32 parentCollectionId,
#   bytes32 conditionId, uint256[] indexSets) external
CTF_ABI = [
    {
        "name": "redeemPositions",
        "type": "function",
        "stateMutability": "nonpayable",
        "inputs": [
            {"name": "collateralToken", "type": "address"},
            {"name": "parentCollectionId", "type": "bytes32"},
            {"name": "conditionId", "type": "bytes32"},
            {"name": "indexSets", "type": "uint256[]"},
        ],
        "outputs": [],
    }
]


def fetch_redeemable_positions(wallet: str) -> list[dict]:
    params = {"user": wallet, "sizeThreshold": "0", "limit": 500}
    url = f"{DATA_API}/positions?{urllib.parse.urlencode(params)}"
    req = urllib.request.Request(url, headers={"User-Agent": "eoa-redeem/1.0"})
    with urllib.request.urlopen(req, timeout=15) as resp:
        positions = json.loads(resp.read())
    return [p for p in positions if p.get("redeemable")]


def group_by_condition(positions: Iterable[dict]) -> dict[str, list[dict]]:
    by_cid: dict[str, list[dict]] = {}
    for p in positions:
        cid = p.get("conditionId")
        if not cid:
            continue
        by_cid.setdefault(cid, []).append(p)
    return by_cid


def submit_redeem(
    w3: Web3,
    account: Account,
    ctf,
    condition_id: str,
    nonce: int,
    gas_price_wei: int,
) -> str:
    """Build, sign, and submit a redeemPositions tx. Returns tx hash."""
    tx = ctf.functions.redeemPositions(
        USDCE_ADDRESS,
        b"\x00" * 32,  # parentCollectionId
        Web3.to_bytes(hexstr=condition_id),
        [1, 2],  # binary outcomes — claim both legs atomically
    ).build_transaction(
        {
            "from": account.address,
            "nonce": nonce,
            "gas": 200_000,
            "gasPrice": gas_price_wei,
            "chainId": 137,
        }
    )
    signed = account.sign_transaction(tx)
    raw = signed.raw_transaction if hasattr(signed, "raw_transaction") else signed.rawTransaction
    tx_hash = w3.eth.send_raw_transaction(raw)
    return tx_hash.hex()


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--wallet", required=True, help="EOA address (0x...)")
    ap.add_argument("--execute", action="store_true",
                    help="Actually submit (default: dry-run)")
    ap.add_argument("--rpc", default=POLYGON_RPC, help="Polygon RPC URL")
    args = ap.parse_args()

    private_key = os.environ.get("POLYMARKET_PRIVATE_KEY") or os.environ.get(
        "METAMASK_PRIVATE_KEY"
    )
    if not private_key:
        print(
            "ERROR: neither POLYMARKET_PRIVATE_KEY nor METAMASK_PRIVATE_KEY set",
            file=sys.stderr,
        )
        sys.exit(1)
    # Allow with or without 0x prefix
    if not private_key.startswith("0x"):
        private_key = "0x" + private_key

    account = Account.from_key(private_key)
    if account.address.lower() != args.wallet.lower():
        print(f"ERROR: private key does not derive --wallet address",
              file=sys.stderr)
        print(f"  derived: {account.address}", file=sys.stderr)
        print(f"  --wallet: {args.wallet}", file=sys.stderr)
        sys.exit(1)

    w3 = Web3(Web3.HTTPProvider(args.rpc))
    if not w3.is_connected():
        print(f"ERROR: cannot connect to RPC {args.rpc}", file=sys.stderr)
        sys.exit(1)

    matic_wei = w3.eth.get_balance(account.address)
    matic = matic_wei / 1e18
    print(f">>> wallet: {account.address}")
    print(f">>> MATIC balance: {matic:.6f}")

    positions = fetch_redeemable_positions(args.wallet)
    by_cid = group_by_condition(positions)
    print(f">>> redeemable positions: {len(positions)} across {len(by_cid)} conditions")
    total_value = sum(float(p.get("currentValue") or 0) for p in positions)
    print(f">>> total value: ${total_value:,.2f}")
    print()

    if not by_cid:
        print("nothing to redeem")
        return

    if not args.execute:
        print(">>> DRY RUN — pass --execute to actually submit. Plan:")
        for cid, ps in sorted(by_cid.items()):
            val = sum(float(p.get("currentValue") or 0) for p in ps)
            slug = ps[0].get("slug", "?")
            print(f"  {cid}  ${val:>7.2f}  {slug}")
        return

    ctf = w3.eth.contract(address=CTF_ADDRESS, abi=CTF_ABI)
    nonce = w3.eth.get_transaction_count(account.address)
    gas_price = w3.eth.gas_price

    submitted = 0
    failed = 0
    for cid, ps in sorted(by_cid.items()):
        val = sum(float(p.get("currentValue") or 0) for p in ps)
        slug = ps[0].get("slug", "?")
        print(f">>> redeem {cid}  ${val:.2f}  ({slug})")
        try:
            tx_hash = submit_redeem(w3, account, ctf, cid, nonce, gas_price)
            print(f"    tx_hash: {tx_hash}  waiting for receipt...")
            # Wait for receipt before submitting next — Polygon public RPC
            # rate-limits per-account in-flight tx (~3 pending max). Waiting
            # also surfaces any revert immediately.
            try:
                receipt = w3.eth.wait_for_transaction_receipt(
                    tx_hash, timeout=60
                )
                if receipt.status == 1:
                    print(f"    confirmed block={receipt.blockNumber} gas={receipt.gasUsed}")
                    submitted += 1
                else:
                    print(f"    REVERTED (status=0) — likely already-redeemed condition")
                    failed += 1
            except Exception as wait_err:
                print(f"    timeout waiting for receipt: {wait_err}")
                failed += 1
            nonce += 1
        except Exception as e:
            print(f"    FAILED: {e}")
            failed += 1

    print(f"\n>>> submitted: {submitted}  failed: {failed}  total: {len(by_cid)}")
    print(f">>> waiting 30s for confirmations...")
    time.sleep(30)
    new_balance = w3.eth.get_balance(account.address) / 1e18
    print(f">>> MATIC after: {new_balance:.6f} (spent {matic - new_balance:.6f})")


if __name__ == "__main__":
    main()
