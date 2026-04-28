#!/usr/bin/env python3
"""Wrap USDC.e -> pUSD via Polymarket's CollateralOnramp contract.

Polymarket V2 settles trading in pUSD (an ERC-20 wrapper around USDC.e).
V1 used USDC.e directly. To trade on V2 with an EOA wallet, USDC.e must
first be wrapped into pUSD via CollateralOnramp. This script does that.

Two-step flow per Polymarket docs:
  1. USDC.e.approve(ONRAMP, amount)
  2. ONRAMP.wrap(USDC.e, recipient, amount)

After running this you also need scripts/eoa_approvals.py to set the V2
exchange approvals on the wrapped pUSD.

Usage (dry-run first to verify):
  ./scripts/eoa_wrap_pusd.py --wallet 0x97fBC6Bc... --amount-usdce 5.0
  # then with --execute to actually send:
  ./scripts/eoa_wrap_pusd.py --wallet 0x97fBC6Bc... --amount-usdce 5.0 --execute
  # or wrap entire balance:
  ./scripts/eoa_wrap_pusd.py --wallet 0x97fBC6Bc... --all --execute

Requires POLYMARKET_PRIVATE_KEY in env (the EOA's private key).
"""

from __future__ import annotations

import argparse
import os
import sys
import time

from eth_account import Account
from web3 import Web3

POLYGON_RPC = os.getenv("POLYGON_RPC_URL", "https://polygon-bor-rpc.publicnode.com")

# From Polymarket docs (https://docs.polymarket.com -> pUSD)
ONRAMP = Web3.to_checksum_address("0x93070a847efEf7F70739046A929D47a521F5B8ee")
USDCE = Web3.to_checksum_address("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174")
PUSD = Web3.to_checksum_address("0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB")

ERC20_ABI = [
    {
        "name": "approve",
        "type": "function",
        "inputs": [
            {"name": "spender", "type": "address"},
            {"name": "amount", "type": "uint256"},
        ],
        "outputs": [{"type": "bool"}],
    },
    {
        "name": "allowance",
        "type": "function",
        "inputs": [
            {"name": "owner", "type": "address"},
            {"name": "spender", "type": "address"},
        ],
        "outputs": [{"type": "uint256"}],
        "stateMutability": "view",
    },
    {
        "name": "balanceOf",
        "type": "function",
        "inputs": [{"name": "account", "type": "address"}],
        "outputs": [{"type": "uint256"}],
        "stateMutability": "view",
    },
]
ONRAMP_ABI = [
    {
        "name": "wrap",
        "type": "function",
        "inputs": [
            {"name": "_asset", "type": "address"},
            {"name": "_to", "type": "address"},
            {"name": "_amount", "type": "uint256"},
        ],
        "outputs": [],
    }
]


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--wallet", required=True, help="EOA wallet 0x... that holds USDC.e and will receive pUSD")
    g = p.add_mutually_exclusive_group(required=True)
    g.add_argument("--amount-usdce", type=float, help="Amount of USDC.e to wrap (e.g. 5.0)")
    g.add_argument("--all", action="store_true", help="Wrap entire USDC.e balance")
    p.add_argument("--execute", action="store_true", help="Actually send tx (default is dry-run)")
    args = p.parse_args()

    pk = os.environ.get("POLYMARKET_PRIVATE_KEY")
    if not pk:
        print("FATAL: POLYMARKET_PRIVATE_KEY not set in env", file=sys.stderr)
        return 2
    if not pk.startswith("0x"):
        pk = "0x" + pk
    account = Account.from_key(pk)
    if account.address.lower() != args.wallet.lower():
        print(f"FATAL: --wallet {args.wallet} does not match key-derived address {account.address}", file=sys.stderr)
        return 2

    w3 = Web3(Web3.HTTPProvider(POLYGON_RPC))
    if not w3.is_connected():
        print(f"FATAL: cannot connect to {POLYGON_RPC}", file=sys.stderr)
        return 2

    wallet = Web3.to_checksum_address(args.wallet)
    usdce = w3.eth.contract(address=USDCE, abi=ERC20_ABI)
    pusd = w3.eth.contract(address=PUSD, abi=ERC20_ABI)
    onramp = w3.eth.contract(address=ONRAMP, abi=ONRAMP_ABI)

    bal_usdce = usdce.functions.balanceOf(wallet).call()
    bal_pusd = pusd.functions.balanceOf(wallet).call()
    cur_allow = usdce.functions.allowance(wallet, ONRAMP).call()

    print(f"Wallet: {wallet}")
    print(f"  USDC.e balance: ${bal_usdce / 1e6:,.6f}")
    print(f"  pUSD  balance: ${bal_pusd / 1e6:,.6f}")
    print(f"  USDC.e -> ONRAMP allowance: ${cur_allow / 1e6:,.6f}")

    if args.all:
        amount_units = bal_usdce
    else:
        amount_units = int(round(args.amount_usdce * 1e6))

    if amount_units <= 0:
        print("FATAL: nothing to wrap (amount=0 or balance=0)", file=sys.stderr)
        return 2
    if amount_units > bal_usdce:
        print(f"FATAL: amount {amount_units / 1e6:.6f} > balance {bal_usdce / 1e6:.6f}", file=sys.stderr)
        return 2

    print(f"\nPlan: wrap ${amount_units / 1e6:,.6f} USDC.e -> pUSD")
    print(f"  Step 1: USDC.e.approve(ONRAMP={ONRAMP}, amount={amount_units})")
    print(f"  Step 2: ONRAMP.wrap(USDC.e, {wallet}, {amount_units})")

    if not args.execute:
        print("\n(dry run -- pass --execute to actually send)")
        return 0

    # Tx 1: approve (only if current allowance insufficient)
    nonce = w3.eth.get_transaction_count(wallet)
    if cur_allow < amount_units:
        print(f"\nSending approve tx (nonce={nonce})...")
        approve_tx = usdce.functions.approve(ONRAMP, amount_units).build_transaction({
            "from": wallet,
            "nonce": nonce,
            "gas": 100_000,
            "maxFeePerGas": w3.to_wei("250", "gwei"),
            "maxPriorityFeePerGas": w3.to_wei("60", "gwei"),
            "chainId": 137,
        })
        signed = account.sign_transaction(approve_tx)
        approve_hash = w3.eth.send_raw_transaction(signed.raw_transaction)
        print(f"  approve tx: {approve_hash.hex()}")
        rcpt = w3.eth.wait_for_transaction_receipt(approve_hash, timeout=120)
        if rcpt.status != 1:
            print(f"  FAILED status={rcpt.status}", file=sys.stderr)
            return 3
        print(f"  approve confirmed in block {rcpt.blockNumber}")
        nonce += 1
    else:
        print(f"\nApprove not needed (current allowance ${cur_allow / 1e6:,.2f} >= ${amount_units / 1e6:,.2f})")

    # Tx 2: wrap
    print(f"\nSending wrap tx (nonce={nonce})...")
    wrap_tx = onramp.functions.wrap(USDCE, wallet, amount_units).build_transaction({
        "from": wallet,
        "nonce": nonce,
        "gas": 200_000,
        "maxFeePerGas": w3.to_wei("250", "gwei"),
        "maxPriorityFeePerGas": w3.to_wei("60", "gwei"),
        "chainId": 137,
    })
    signed = account.sign_transaction(wrap_tx)
    wrap_hash = w3.eth.send_raw_transaction(signed.raw_transaction)
    print(f"  wrap tx: {wrap_hash.hex()}")
    rcpt = w3.eth.wait_for_transaction_receipt(wrap_hash, timeout=120)
    if rcpt.status != 1:
        print(f"  FAILED status={rcpt.status}", file=sys.stderr)
        return 3
    print(f"  wrap confirmed in block {rcpt.blockNumber}")

    # Verify post-state
    new_usdce = usdce.functions.balanceOf(wallet).call()
    new_pusd = pusd.functions.balanceOf(wallet).call()
    print(f"\nPost-state:")
    print(f"  USDC.e: ${new_usdce / 1e6:,.6f} (Δ ${(new_usdce - bal_usdce) / 1e6:+,.6f})")
    print(f"  pUSD : ${new_pusd / 1e6:,.6f} (Δ ${(new_pusd - bal_pusd) / 1e6:+,.6f})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
