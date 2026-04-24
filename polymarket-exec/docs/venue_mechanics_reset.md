# Polymarket Venue Mechanics Reset

Status: audit checklist as of 2026-04-24.

This document is the implementation checklist for resetting `polymarket-exec` around Polymarket venue mechanics before further tiny-live runs. The immediate live bug class is local state diverging from venue state: the bot can believe an order is cancelled/unfilled while the account has acquired inventory. That invalidates paired-entry guarantees, PnL accounting, hedge rescue, and maker/rebate attribution.

## Sources

- Polymarket CLOB V2 migration: https://docs.polymarket.com/v2-migration
- Polymarket clients and SDKs: https://docs.polymarket.com/api-reference/clients-sdks
- Polymarket authentication: https://docs.polymarket.com/api-reference/authentication
- Polymarket order lifecycle and errors: https://docs.polymarket.com/trading/orders/overview
- Polymarket user websocket: https://docs.polymarket.com/market-data/websocket/user-channel
- Polymarket rate limits: https://docs.polymarket.com/api-reference/rate-limits
- Polymarket maker rebates endpoint: https://docs.polymarket.com/api-reference/rebates/get-current-rebated-fees-for-a-maker
- Polymarket MM overview: https://docs.polymarket.com/market-makers/overview
- Polymarket MM inventory operations: https://docs.polymarket.com/market-makers/inventory
- Polymarket liquidity rewards: https://docs.polymarket.com/market-makers/liquidity-rewards

## Current Repo Findings

- `polymarket-exec/Cargo.toml` pins `polymarket-client-sdk = "0.4.4"` with `clob` and `heartbeats`.
- `Cargo.lock` resolves `polymarket-client-sdk` to `0.4.4` from crates.io.
- The current live adapter default base URL is `https://clob.polymarket.com`.
- The adapter uses `authentication_builder`, `signature_type`, optional `funder`, signed limit orders, `post_order`, `cancel_order`, `orders`, `trades`, and `balance_allowance`.
- The adapter already supports `POLYMARKET_SIGNATURE_TYPE` values for EOA, proxy, and Gnosis Safe, and optional `POLYMARKET_FUNDER_ADDRESS`.
- `METAMASK_PRIVATE_KEY` is accepted as a local alias for `POLYMARKET_PRIVATE_KEY`; keep this documented but do not expand it as a generic secret name.
- `sync_balances_from_client` currently returns cash only and leaves `positions: Vec::new()`. That means local "flat" is not venue-authoritative.
- `sync_recent_fills_from_client` has been moving toward maker/taker address filtering, but this still needs a dedicated venue simulator and live reconcile test before restarting live trading.
- The strategy still has static defaults for `venue_min_order_quantity`, `maker_price_tick`, `taker_fee_coeff`, and related min-size/tick fields. These need to come from market metadata/CLOB market info rather than env defaults for live.
- User websocket is present, authenticated, and event-classified, but it must become the primary event source for order/trade lifecycle, with REST reconcile as a backstop.

## Hard Blockers Before Restarting Tiny-Live

1. **CLOB V2 readiness**
   - Polymarket states CLOB V2 goes live on 2026-04-28 around 11:00 UTC, with roughly one hour downtime and all open orders wiped.
   - V2 changes exchange contracts, collateral from USDC.e to pUSD, fee handling, order fields, and SDK package expectations.
   - Official docs list Rust package `polymarket-client-sdk` with repository `Polymarket/rs-clob-client-v2`, but the current repo is pinned to `0.4.4`. Verify whether `0.4.4` is the V2-compatible crate version before live. If not, upgrade or vendor/pin the correct V2 SDK.
   - Add a config switch for preprod testing against `https://clob-v2.polymarket.com` before cutover; after cutover, production remains `https://clob.polymarket.com`.

2. **Venue-authoritative positions**
   - Cash-only balance sync is insufficient.
   - Implement position sync using Polymarket Data API current positions, CLOB/user events, or SDK support if available.
   - Runtime must compare local inventory against venue inventory by market/asset every reconcile cycle.
   - If local and venue inventory disagree beyond dust, enter risk-off, cancel working orders, and block fresh entries until reconciled.

3. **User websocket as primary truth**
   - User WS emits authenticated order/trade updates filtered by API key.
   - Treat `MATCHED` trade events as immediate fill evidence; `CONFIRMED` is settlement finality, not the first time the execution engine should notice inventory risk.
   - Parse both maker and taker fills from user WS. For maker fills, use `maker_orders[].order_id`, `matched_amount`, `asset_id`, and `price`.
   - Parse order events for `PLACEMENT`, `UPDATE`, and `CANCELLATION`; never infer "unfilled" from a cancel ack alone if a fill event can race it.

4. **REST trade filtering**
   - Never use unfiltered CLOB trades as account fill truth.
   - Backfill/reconcile fills with maker-address and taker-address filters for the actual funder/trading address.
   - Deduplicate by venue order id, market id, asset id, side, price, quantity, and match timestamp where available.
   - If a REST trade references an order that local state already marked `Cancelled` or `Rejected`, apply the fill correction and reopen inventory accounting.

5. **Venue metadata-driven sizing**
   - Live min order size, minimum tick size, fee details, and token mapping must be pulled from CLOB market info/market metadata.
   - Remove live reliance on hardcoded defaults such as `5.0` shares, `0.01` tick, and static fee coefficients.
   - Strategy sizing should use:
     - venue minimum order size;
     - tick size;
     - available cash and reserved order exposure;
     - top-of-book/depth liquidity;
     - paired-leg cost and edge;
     - current inventory and mergeable paired inventory.
   - The `entry_min_size_multiplier` can remain as a strategy parameter, but it must multiply the venue minimum only when explicitly enabled and visible in decision logs.

6. **Inventory operations**
   - Polymarket MM docs treat split, merge, and redeem as core market-maker operations through CTF/Relayer.
   - Implement explicit inventory commands:
     - split pUSD into paired outcome inventory;
     - merge paired YES/NO inventory back into pUSD;
     - redeem winning tokens after resolution.
   - For our BTC 5m flow, merge is likely the key operation: if we buy complementary outcome inventory at total cost below payout, merge should free capital and prevent stale resolved-position clutter.
   - Do not use sell-based cleanup as the default if the intended strategy is buy/merge.

7. **Rebate and scoring attribution**
   - Local `FillLiquidity::Maker` is execution-quality telemetry, not proof of paid rebates.
   - Add maker rebate polling via `GET /rebates/current?date=YYYY-MM-DD&maker_address=...`.
   - Add order-scoring checks for active orders where the SDK supports it.
   - Track maker/taker status per fill, expected edge at quote time, realized edge after fill, and later rebated fees by condition/asset/maker.

8. **Rate-limit and delayed-response semantics**
   - Polymarket rate limits throttle/delay through Cloudflare rather than always rejecting immediately.
   - Trading endpoints have high burst limits, but reconciliation, balance allowance, and relayer calls have lower practical limits.
   - Treat delayed, uncertain, or timeout outcomes as `NeedsReconcile`, not as safe failure.
   - Reconcile queues should not block all trading forever, but live trading must not add exposure in a market whose own state is unresolved.

## CLOB V2 Implementation Checklist

- [ ] Confirm latest `polymarket-client-sdk` version and whether `0.4.4` is V2-compatible with `rs-clob-client-v2`.
- [ ] If SDK is stale, upgrade `polymarket-client-sdk` and adapt compile errors deliberately.
- [ ] Verify `Client::new`/`Config` supports V2 hot-swap or preprod `https://clob-v2.polymarket.com`.
- [ ] Verify order struct no longer exposes or requires removed V1 fields: `nonce`, `feeRateBps`, `taker`.
- [ ] Verify V2 order fields are handled by the SDK: `timestamp`, `metadata`, `builder`.
- [ ] Verify fee model: no local taker fee coefficient should be authoritative for live order construction.
- [ ] Add `POLYMARKET_CLOB_URL` or equivalent to avoid hardcoded production URL in live smoke/preprod tests.
- [ ] Add `POLYMARKET_BUILDER_CODE` only if we need builder attribution.
- [ ] Replace USDC.e-specific docs/config comments with pUSD-aware wording where live operations require it.
- [ ] Add cutover runbook: stop live, cancel/verify no open orders, wait for maintenance, refresh market metadata, replace orders only after V2 health is confirmed.

## Auth, Funder, and Signature Checklist

- [ ] Keep L1 private-key auth only for deriving API credentials and signing local orders.
- [ ] Keep L2 API key/secret/passphrase for order management, cancellations, user orders, and user websocket.
- [ ] Require explicit `POLYMARKET_SIGNATURE_TYPE` in live config; do not silently default for production without logging.
- [ ] Require explicit `POLYMARKET_FUNDER_ADDRESS` for proxy/Safe trading and verify it matches the Polymarket profile wallet that holds funds.
- [ ] For EOA trading, document that funder is signer and allowances/gas requirements differ.
- [ ] At startup, log signer address, funder address, signature type, and CLOB URL without secrets.
- [ ] At startup, run a no-trade auth probe: derive credentials, connect user WS, fetch open orders, fetch balance/allowance, fetch positions.

## Order Lifecycle Checklist

- [ ] Use post-only GTD/GTC for maker quoting. Do not combine post-only with FOK/FAK.
- [ ] Respect GTD one-minute security threshold while maintaining internal quote TTL.
- [ ] Treat insert statuses explicitly:
  - `live`: resting and eligible to become maker;
  - `matched`: immediate execution, not a resting maker quote;
  - `delayed`/`unmatched`: uncertain until reconciled.
- [ ] On post-only cross rejection, reprice once only if the book still supports maker edge; otherwise suppress.
- [ ] On duplicate order rejection, query order by id/client context before retrying.
- [ ] On cancel ack, do not mark exposure flat until fill reconciliation and position reconciliation agree.
- [ ] On partial fill, immediately recompute paired exposure and keep/submit hedge or merge action.
- [ ] On market close/resolution, stop fresh quotes, cancel open orders, reconcile positions, then redeem/merge according to state.

## Simulator/BDD Harness Checklist

Build a deterministic local venue simulator before more live iterations. It should implement enough Polymarket mechanics to reproduce the exact bug classes we have seen.

- [ ] Simulated CLOB REST:
  - submit order;
  - post-only reject if crossing;
  - accepted `live` order;
  - delayed response;
  - cancel accepted;
  - cancel accepted after partial/full fill;
  - open orders;
  - filtered maker/taker trades;
  - balance allowance;
  - current positions;
  - rebates current.
- [ ] Simulated market websocket:
  - full book snapshot;
  - price changes;
  - tick size changes;
  - last trade price;
  - best bid/ask updates.
- [ ] Simulated user websocket:
  - placement;
  - update/partial fill;
  - matched trade;
  - mined/confirmed/failed status transitions;
  - cancellation;
  - out-of-order cancel then trade race.
- [ ] Simulated inventory:
  - split;
  - merge;
  - redeem;
  - dust thresholds;
  - stale unresolved market.
- [ ] BDD scenarios:
  - accepted paired quotes, no fills, cancel cleanly;
  - one leg fills before cancel ack, hedge/merge/risk-off fires;
  - both legs fill asymmetrically, paired payoff is recognized;
  - REST trades endpoint returns unrelated market trades unless filtered;
  - user WS disconnects while fill occurs, REST backfill catches it;
  - venue position exists while local store says flat, runtime blocks fresh entry;
  - V2 min order size/tick changes mid-market and strategy refreshes metadata;
  - post-only quote crosses after book update and is safely rejected/repriced;
  - merge frees paired inventory and cash accounting updates;
  - rebate endpoint attributes maker fees after date rollover.

## Development Plan

1. Keep tiny-live stopped until user-position reconciliation and fill filtering have green simulator coverage.
2. Build the simulator behind a test-only execution adapter or local mock CLOB server; prefer deterministic tests over flaky network tests.
3. Add BDD-style scenario tests that encode the live incidents as acceptance tests.
4. Upgrade/verify CLOB V2 SDK support before any further live restart.
5. Wire venue metadata into live strategy config and logs.
6. Implement position reconciliation and use it as the runtime source of truth.
7. Implement merge/redeem/split interfaces and a buy/merge state machine.
8. Add rebate polling and order-scoring telemetry.
9. Run live smoke against V2/preprod, then run a no-trade reconcile probe.
10. Only then restart tiny-live with strict caps and a kill switch.
