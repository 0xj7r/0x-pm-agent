"""Order placement pipeline test.

Verifies connectivity, authentication, order book reads, and
order placement/cancellation against live Polymarket CLOB.

Usage:
    python tests/test_order_pipeline.py              # dry run (read-only)
    python tests/test_order_pipeline.py --live        # place + cancel test order
"""
from __future__ import annotations

import argparse
import asyncio
import logging
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from config import Config
from clients.polymarket import PolymarketClient
from clients.market_scanner import MarketWindowScanner

logger = logging.getLogger(__name__)


async def test_pipeline(live: bool = False):
    config = Config()
    results = {"passed": 0, "failed": 0, "skipped": 0}

    def check(name: str, passed: bool, detail: str = ""):
        status = "PASS" if passed else "FAIL"
        results["passed" if passed else "failed"] += 1
        print(f"  [{status}] {name}{': ' + detail if detail else ''}")

    def skip(name: str, reason: str):
        results["skipped"] += 1
        print(f"  [SKIP] {name}: {reason}")

    print("\n=== ORDER PIPELINE TEST ===\n")

    # 1. Config check
    print("1. Configuration")
    check("CLOB URL set", bool(config.CLOB_URL), config.CLOB_URL)
    check("Private key set", bool(config.PRIVATE_KEY), f"{config.PRIVATE_KEY[:8]}..." if config.PRIVATE_KEY else "MISSING")
    has_key = bool(config.PRIVATE_KEY)

    # 2. Client init
    print("\n2. Client initialization")
    try:
        client = PolymarketClient(config)
        check("PolymarketClient created", True)
    except Exception as e:
        check("PolymarketClient created", False, str(e))
        print(f"\nResults: {results}")
        return results

    # 3. Market discovery
    print("\n3. Market discovery (Gamma API)")
    try:
        scanner = MarketWindowScanner(coin="btc")
        windows = await scanner.find_active_windows()
        check("Gamma API reachable", True)
        check("BTC 5m markets found", len(windows) > 0, f"{len(windows)} markets")

        if windows:
            market = windows[0]
            print(f"    Sample: {market.question}")
            print(f"    Tokens: UP={market.up_token_id[:16]}... DOWN={market.down_token_id[:16]}...")
        else:
            skip("Order book test", "no markets found")
            print(f"\nResults: {results}")
            await scanner.close()
            return results
    except Exception as e:
        check("Gamma API reachable", False, str(e))
        print(f"\nResults: {results}")
        return results

    # 4. Order book
    print("\n4. Order book (CLOB)")
    try:
        market = windows[0]
        book = await client.get_order_book(market.up_token_id)
        check("Order book fetched", True)

        if book.bids:
            best_bid = book.bids[0]
            print(f"    Best bid: ${best_bid.price:.3f} x {best_bid.size:.1f}")
        if book.asks:
            best_ask = book.asks[0]
            print(f"    Best ask: ${best_ask.price:.3f} x {best_ask.size:.1f}")

        check("Book has bids", len(book.bids) > 0, f"{len(book.bids)} levels")
        check("Book has asks", len(book.asks) > 0, f"{len(book.asks)} levels")
    except Exception as e:
        check("Order book fetched", False, str(e))

    # 5. Balance
    print("\n5. Account balance")
    try:
        balance = await client.get_balance()
        check("Balance fetched", True, f"${balance:.2f}")
    except Exception as e:
        check("Balance fetched", False, str(e))

    # 6. Order placement (only if --live)
    print("\n6. Order placement")
    if not live:
        skip("Place test order", "dry run mode (use --live to test)")
        skip("Cancel test order", "dry run mode")
    elif not has_key:
        skip("Place test order", "no private key configured")
        skip("Cancel test order", "no private key configured")
    else:
        try:
            # Place a limit order far from market (won't fill)
            # Buy 1 share at $0.01 (will never fill)
            token_id = market.up_token_id
            test_price = 0.01
            test_size = 1.0

            print(f"    Placing: BUY 1 share @ $0.01 (will not fill)")
            result = await client.place_order(
                token_id=token_id,
                side="BUY",
                price=test_price,
                size=test_size,
            )
            order_id = result.get("orderID") or result.get("id")
            check("Order placed", bool(order_id), f"ID={order_id}")

            if order_id:
                print(f"    Cancelling order {order_id}...")
                cancel = await client.cancel_order(order_id)
                check("Order cancelled", True, str(cancel))
            else:
                skip("Cancel test order", "no order ID returned")
        except Exception as e:
            check("Place test order", False, str(e))

    # Summary
    total = results["passed"] + results["failed"] + results["skipped"]
    print(f"\n{'='*50}")
    print(f"RESULTS: {results['passed']}/{total} passed, "
          f"{results['failed']} failed, {results['skipped']} skipped")
    print(f"{'='*50}")

    return results


def main():
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--live", action="store_true",
                        help="Actually place and cancel a test order")
    args = parser.parse_args()
    asyncio.run(test_pipeline(args.live))


if __name__ == "__main__":
    main()
