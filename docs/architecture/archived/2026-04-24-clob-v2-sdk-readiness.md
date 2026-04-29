# CLOB V2 SDK Readiness

Date: 2026-04-24

This is the readiness report for the Rust live executor before the Polymarket CLOB V2 cutover.

## Decision

Do not restart tiny-live or place live orders through the current Rust SDK's V1 order builder for CLOB V2.

The current dependency, `polymarket-client-sdk = 0.4.4`, is the latest version visible from local crate metadata/docs.rs, but its order-signing surface still matches CLOB V1. It still has `nonce`, `taker`, `expiration`, and `feeRateBps` in the signed order path and does not expose the V2 `timestamp`, `metadata`, or `builder` fields required by the migration guide.

The repo now has a narrow raw Rust V2 prototype behind `POLYMARKET_CLOB_VERSION=v2`. It builds the V2 EIP-712 order shape directly, signs against exchange domain version `"2"`, serializes the V2 `/order` payload without `nonce`, `feeRateBps`, or user-set `taker`, and posts with L2 HMAC headers. This path is intentionally opt-in and still requires preprod smoke testing before funded use.

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

- `POLYMARKET_CLOB_API_URL` is configurable, so the process can point at `https://clob-v2.polymarket.com` for preprod.
- `POLYMARKET_CLOB_VERSION=v2` selects the raw Rust V2 submitter instead of the SDK V1 order builder.
- `POLYMARKET_CLOB_V2_BUILDER_CODE`, `POLYMARKET_CLOB_V2_METADATA`, and `POLYMARKET_CLOB_V2_NEG_RISK` are explicit environment switches.
- Market and user websocket URLs are configurable.
- Live auth can derive/create API credentials and pass signature type/funder into the SDK.

Not ready:

- The Rust SDK dependency available to this repo still appears to sign CLOB V1 order structs, so V2 order submission must use the raw Rust V2 path or an official V2 SDK/sidecar.
- We do not have a verified Rust V2 SDK package/revision to upgrade to.
- We do not yet pull V2 CLOB market info into live sizing/risk. Static tick/min-size/fee defaults must not be treated as authoritative for V2 live trading.
- pUSD wrapping/allowance requirements are not automated or smoke-tested for API-only trading.
- Builder code support is wired as a bytes32 config value, but should remain zero unless we join the Builder Program.
- `POLYMARKET_CLOB_V2_NEG_RISK` is manual. The next step is to derive the correct V2 exchange address per token from CLOB market metadata.

## Cutover Runbook

Before 2026-04-28 11:00 UTC:

1. Keep tiny-live stopped.
2. Verify no open orders on the CLOB from the trading wallet.
3. Confirm whether Polymarket has published a Rust CLOB V2 SDK release or a V2 git revision for `polymarket-client-sdk`.
4. Do not use the current `0.4.4` SDK order builder for live CLOB V2 order submission.
5. If a V2-compatible Rust SDK is available, upgrade in `polymarket-exec/Cargo.toml`, compile, and compare its serialized order payload against `wire::clob_v2`.
6. If no V2 Rust SDK is available, test the raw Rust path against `https://clob-v2.polymarket.com` with `POLYMARKET_CLOB_VERSION=v2`, a burner/tiny funded account, and one far-from-fair post-only order.
7. Confirm pUSD balance/allowance/wrapping before any production order.
8. Refresh market metadata from V2 and verify minimum tick size, minimum order size, fee details, token IDs, and neg-risk exchange selection.

During cutover:

1. Keep the service stopped while Polymarket maintenance is active.
2. Treat all previous open-order state as invalid because Polymarket wipes open orders.
3. Do not replay old orders automatically.

After cutover:

1. Run no-trade reconciliation first.
2. Run live-smoke: submit one far-from-fair post-only order, confirm it is open, cancel it, confirm it is gone.
3. Only then restart tiny-live with very small caps.

## Minimal Next Implementation Step

The next code change should be:

- Add CLOB market-info ingestion for V2 tick size, min size, fee metadata, token IDs, and per-token neg-risk exchange selection.
- Add a preprod live-smoke command that runs `POLYMARKET_CLOB_VERSION=v2` in submit/cancel-only mode without enabling strategy trading.
- Replace the raw Rust path with the official Rust V2 SDK if Polymarket publishes one before cutover.

## Sources

- Polymarket CLOB V2 migration: https://docs.polymarket.com/v2-migration
- Polymarket Clients & SDKs: https://docs.polymarket.com/api-reference/clients-sdks
- Official TypeScript CLOB V2 client: https://github.com/Polymarket/clob-client-v2
- Official Python CLOB V2 client: https://github.com/Polymarket/py-clob-client-v2
- Rust crate docs: https://docs.rs/polymarket-client-sdk/0.4.4
- Published Rust SDK repo metadata: https://github.com/Polymarket/rs-clob-client
