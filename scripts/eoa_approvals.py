#!/usr/bin/env python3
"""Set Polymarket V2 trading approvals for an EOA wallet.

Sets the two approvals needed to trade non-neg-risk markets on V2:
  1. USDC.e.approve(CTF_EXCHANGE_V2, MAX_UINT256)
  2. CTF.setApprovalForAll(CTF_EXCHANGE_V2, true)

If you also want to trade neg-risk markets (different exchange contract),
add the --include-neg-risk flag — sets 4 more approvals (USDC + CTF for
both NegRisk CTF Exchange and NegRisk Adapter).

Usage:
  ./scripts/eoa_approvals.py --wallet 0x97fBC6Bc... --execute
"""

from __future__ import annotations

import argparse
import os
import sys
import time

from web3 import Web3
from eth_account import Account

POLYGON_RPC = os.getenv(
    "POLYGON_RPC_URL", "https://polygon-bor-rpc.publicnode.com"
)

USDCE = Web3.to_checksum_address("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174")
CTF = Web3.to_checksum_address("0x4D97DCd97eC945f40cF65F87097ACe5EA0476045")

# V2 exchange addresses (per official polymarket_client_sdk_v2 lib.rs)
CTF_EXCHANGE_V2 = Web3.to_checksum_address(
    "0xE111180000d2663C0091e4f400237545B87B996B"
)
NEG_RISK_CTF_EXCHANGE_V2 = Web3.to_checksum_address(
    "0xe2222d279d744050d28e00520010520000310F59"
)
# Neg-risk adapter (handles token minting/splitting for neg-risk markets)
NEG_RISK_ADAPTER = Web3.to_checksum_address(
    "0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296"
)

MAX_UINT256 = (1 << 256) - 1

ERC20_ABI = [
    {
        "name": "approve",
        "type": "function",
        "stateMutability": "nonpayable",
        "inputs": [
            {"name": "spender", "type": "address"},
            {"name": "value", "type": "uint256"},
        ],
        "outputs": [{"type": "bool"}],
    },
    {
        "name": "allowance",
        "type": "function",
        "stateMutability": "view",
        "inputs": [
            {"name": "owner", "type": "address"},
            {"name": "spender", "type": "address"},
        ],
        "outputs": [{"type": "uint256"}],
    },
]

ERC1155_ABI = [
    {
        "name": "setApprovalForAll",
        "type": "function",
        "stateMutability": "nonpayable",
        "inputs": [
            {"name": "operator", "type": "address"},
            {"name": "approved", "type": "bool"},
        ],
        "outputs": [],
    },
    {
        "name": "isApprovedForAll",
        "type": "function",
        "stateMutability": "view",
        "inputs": [
            {"name": "owner", "type": "address"},
            {"name": "operator", "type": "address"},
        ],
        "outputs": [{"type": "bool"}],
    },
]


def submit_tx(w3, account, tx, label):
    signed = account.sign_transaction(tx)
    raw = signed.raw_transaction if hasattr(signed, "raw_transaction") else signed.rawTransaction
    h = w3.eth.send_raw_transaction(raw)
    print(f"  tx_hash: {h.hex()}  waiting...")
    receipt = w3.eth.wait_for_transaction_receipt(h, timeout=60)
    print(f"  {label}: {'OK' if receipt.status == 1 else 'REVERTED'} block={receipt.blockNumber} gas={receipt.gasUsed}")
    return receipt.status == 1


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--wallet", required=True)
    ap.add_argument("--execute", action="store_true", help="Submit (default: dry-run)")
    ap.add_argument(
        "--include-neg-risk",
        action="store_true",
        help="Also approve neg-risk markets (4 more txs)",
    )
    args = ap.parse_args()

    pk = os.environ.get("POLYMARKET_PRIVATE_KEY") or os.environ.get(
        "METAMASK_PRIVATE_KEY"
    )
    if not pk:
        print("ERROR: POLYMARKET_PRIVATE_KEY or METAMASK_PRIVATE_KEY required",
              file=sys.stderr)
        sys.exit(1)
    if not pk.startswith("0x"):
        pk = "0x" + pk

    account = Account.from_key(pk)
    if account.address.lower() != args.wallet.lower():
        print(f"ERROR: key derives {account.address}, not --wallet", file=sys.stderr)
        sys.exit(1)

    w3 = Web3(Web3.HTTPProvider(POLYGON_RPC))
    print(f"wallet: {account.address}")
    print(f"MATIC: {w3.eth.get_balance(account.address)/1e18:.6f}")

    usdce = w3.eth.contract(address=USDCE, abi=ERC20_ABI)
    ctf = w3.eth.contract(address=CTF, abi=ERC1155_ABI)

    targets = [(CTF_EXCHANGE_V2, "CTF Exchange V2 (standard markets)")]
    if args.include_neg_risk:
        targets.extend([
            (NEG_RISK_CTF_EXCHANGE_V2, "Neg Risk CTF Exchange V2"),
            (NEG_RISK_ADAPTER, "Neg Risk Adapter"),
        ])

    # Audit current state
    print("\n=== current allowances ===")
    plan = []
    for spender, label in targets:
        cur_usdc = usdce.functions.allowance(account.address, spender).call()
        cur_ctf = ctf.functions.isApprovedForAll(account.address, spender).call()
        usdc_ok = cur_usdc >= MAX_UINT256 // 2
        print(f"  {label}")
        print(f"    USDC.e allowance: {'MAX' if usdc_ok else f'{cur_usdc/1e6:.2f}'} {'✓' if usdc_ok else '→ approve needed'}")
        print(f"    CTF approved: {cur_ctf} {'✓' if cur_ctf else '→ setApprovalForAll needed'}")
        if not usdc_ok:
            plan.append(("usdc", spender, label))
        if not cur_ctf:
            plan.append(("ctf", spender, label))

    if not plan:
        print("\nAll approvals already set. Nothing to do.")
        return

    print(f"\n=== {len(plan)} approval txs needed ===")
    for kind, spender, label in plan:
        print(f"  {kind.upper()} → {label}")

    if not args.execute:
        print("\nDRY RUN — pass --execute to submit")
        return

    nonce = w3.eth.get_transaction_count(account.address)
    gas_price = w3.eth.gas_price
    submitted = 0
    for kind, spender, label in plan:
        print(f"\n>>> {kind.upper()} approval for {label}")
        if kind == "usdc":
            tx = usdce.functions.approve(spender, MAX_UINT256).build_transaction({
                "from": account.address, "nonce": nonce, "gas": 100_000,
                "gasPrice": gas_price, "chainId": 137,
            })
        else:
            tx = ctf.functions.setApprovalForAll(spender, True).build_transaction({
                "from": account.address, "nonce": nonce, "gas": 100_000,
                "gasPrice": gas_price, "chainId": 137,
            })
        if submit_tx(w3, account, tx, label):
            submitted += 1
        nonce += 1
        time.sleep(2)

    print(f"\n=== done: {submitted}/{len(plan)} succeeded ===")


if __name__ == "__main__":
    main()
