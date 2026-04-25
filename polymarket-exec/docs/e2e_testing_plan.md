# E2E Testing Plan Without Live Funds

The live strategy still needs a tiny-capital venue smoke before scaling, but
most failure modes should be covered without touching production funds.

## Test Layers

1. Pure strategy tests
   - Input: synthetic BTC/book/market snapshots.
   - Assert: expected order intents, suppressions, clip sizes, and hedge/merge
     postures.
   - Purpose: prevent parameter or signal regressions.

2. Runtime scenario tests
   - Input: deterministic event timelines with book updates, fills, cancels,
     reconnects, venue snapshots, and stale state.
   - Assert: order lifecycle, inventory, risk-off, merge intent creation, and
     reconciliation behavior.
   - Purpose: prove the state machine before any venue call.

3. Venue-contract adapter tests
   - Use a mock HTTP server for CLOB/Data/Relayer endpoints.
   - Exercise signed submit/cancel shape, Data API position mapping,
     relayer nonce fetch, relayer submit payload, and transaction polling.
   - Assert: payloads include the correct signer, proxy wallet, condition id,
     collateral, partition, quantity scaling, auth headers, and fail-closed
     behavior when required fields are missing.

4. Replay tests from live captures
   - Record anonymized orderbook/user/position/whale events.
   - Re-run them against the runtime with fixed seeds.
   - Assert: no one-sided exposure expansion after reconcile mismatch; paired
     inventory plans merge; stale orders are not replayed.

5. Tiny-live smoke
   - Final venue check only.
   - Place/cancel one maker order, then run one intentionally tiny paired/merge
     cycle when the relayer wallet mapping is verified.

## Why This Is Enough Before Live

The CLOB and relayer are deterministic HTTP contracts once the payload is
correct. Mock tests can validate almost everything except whether Polymarket
accepts the exact current production account setup. That last part requires
tiny-live because it depends on live wallet deployment, allowances, collateral,
and relayer account permissions.

## Current Gaps

- Mock HTTP server coverage for the full CLOB/Data/Relayer lifecycle.
- Relayer transaction polling after `/submit` returns `STATE_NEW`.
- Redeem flow for resolved markets.
- Captured live replay fixture for our April 24 one-sided failure cases.
