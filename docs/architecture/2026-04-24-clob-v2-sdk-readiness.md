# CLOB V2 SDK Readiness

Date: 2026-04-24

This is the readiness report for the Rust live executor before the Polymarket CLOB V2 cutover.

## Decision

Do not restart tiny-live or place live orders through the current Rust SDK for CLOB V2 until Polymarket publishes or confirms a Rust SDK build that signs V2 orders.

The current dependency, `polymarket-client-sdk = 0.4.4`, is the latest version visible from local crate metadata/docs.rs, but its order-signing surface still matches CLOB V1. It still has `nonce`, `taker`, `expiration`, and `feeRateBps` in the signed order path and does not expose the V2 `timestamp`, `metadata`, or `builder` fields required by the migration guide.

## Evidence Checked

- Repo dependency: `polymarket-exec/Cargo.toml` pins `polymarket-client-sdk = { version = "0.4.4", features = ["clob", "heartbeats"] }`.
- Lockfile resolves `polymarket-client-sdk` to `0.4.4`.
- `cargo info polymarket-client-sdk` reports `0.4.4`, MSRV `1.88.0`, repository `https://github.com/polymarket/rs-clob-client`.
- docs.rs shows `polymarket_client_sdk 0.4.4` as the current rendered Rust crate.
- Official Clients & SDKs docs list Rust package `polymarket-client-sdk` but point the source repository at `github.com/Polymarket/rs-clob-client-v2`.
- The published crate source still contains V1 order fields in `src/clob/order_builder.rs` and `src/clob/types/mod.rs`.

## Official V2 Requirements

Polymarket's migration guide says CLOB V2 goes live on 2026-04-28 around 11:00 UTC, with roughly one hour downtime and all open orders wiped.

Required changes called out by the official docs:

- Test against `https://clob-v2.polymarket.com` before go-live.
- After cutover, production remains `https://clob.polymarket.com`.
- Order uniqueness moves from `nonce` to millisecond `timestamp`.
- Signed order fields add `timestamp`, `metadata`, and `builder`.
- `nonce`, `feeRateBps`, and user-set `taker` are removed from order creation.
- Exchange EIP-712 domain version changes from `"1"` to `"2"`.
- Collateral changes from USDC.e to pUSD.
- Fees are handled at match time; makers are not charged fees, takers pay dynamic market fees.
- Market parameters should be queried through `getClobMarketInfo()`: minimum tick size, minimum order size, fee details, tokens, and RFQ status.

## Current Repo Readiness

Ready:

- `POLYMARKET_CLOB_API_URL` is configurable, so the process can point at `https://clob-v2.polymarket.com` for preprod once the SDK is compatible.
- Market and user websocket URLs are configurable.
- Live auth can derive/create API credentials and pass signature type/funder into the SDK.

Not ready:

- The Rust SDK dependency available to this repo still appears to sign CLOB V1 order structs.
- We do not have a verified Rust V2 SDK package/revision to upgrade to.
- We do not yet pull V2 CLOB market info into live sizing/risk. Static tick/min-size/fee defaults must not be treated as authoritative for V2 live trading.
- pUSD wrapping/allowance requirements are not automated or smoke-tested for API-only trading.
- Builder code support is not wired, which is fine unless we join the Builder Program, but it should not be confused with old builder HMAC headers.

## Cutover Runbook

Before 2026-04-28 11:00 UTC:

1. Keep tiny-live stopped.
2. Verify no open orders on the CLOB from the trading wallet.
3. Confirm whether Polymarket has published a Rust CLOB V2 SDK release or a V2 git revision for `polymarket-client-sdk`.
4. Do not use the current `0.4.4` crate for live CLOB V2 order submission unless Polymarket explicitly confirms it hot-swaps order signing despite the V1 public structs.
5. If a V2-compatible Rust SDK is available, upgrade in `polymarket-exec/Cargo.toml`, compile, and run a live-smoke test against `https://clob-v2.polymarket.com` with a burner or tiny funded account.
6. Confirm pUSD balance/allowance/wrapping before any production order.
7. Refresh market metadata from V2 and verify minimum tick size, minimum order size, fee details, and token IDs.

During cutover:

1. Keep the service stopped while Polymarket maintenance is active.
2. Treat all previous open-order state as invalid because Polymarket wipes open orders.
3. Do not replay old orders automatically.

After cutover:

1. Run no-trade reconciliation first.
2. Run live-smoke: submit one far-from-fair post-only order, confirm it is open, cancel it, confirm it is gone.
3. Only then restart tiny-live with very small caps.

## Minimal Next Implementation Step

The next code change should be one of:

- Upgrade to the official Rust V2 SDK once published/confirmed, then adapt compile errors deliberately.
- If no Rust V2 SDK is published, implement a narrow internal V2 order signer only after reviewing the final official V2 contract addresses and wire schemas. This is higher risk and should not be mixed into strategy changes.

## Sources

- Polymarket CLOB V2 migration: https://docs.polymarket.com/v2-migration
- Polymarket Clients & SDKs: https://docs.polymarket.com/api-reference/clients-sdks
- Rust crate docs: https://docs.rs/polymarket-client-sdk/0.4.4
- Published Rust SDK repo metadata: https://github.com/Polymarket/rs-clob-client
