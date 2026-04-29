# Polymarket venue reset

This is the execution-quality reset plan before any further meaningful live
capital. The goal is to validate our runtime against venue mechanics, not just
against strategy unit tests.

## Current stance

- Official Polymarket endpoints remain the only allowed endpoints for funded
  live wallets.
- Third-party CLOB-compatible simulators may be useful, but only with no wallet
  connection or with a burner wallet that has no funds, approvals, or account
  linkage to production trading.
- If a simulator requires connecting a real wallet before API testing, treat it
  as unsuitable for safety-critical validation.

## Configurable venue endpoints

The runtime now supports an explicit CLOB REST base URL:

```env
POLYMARKET_CLOB_API_URL=https://clob.polymarket.com
POLYMARKET_DATA_API_URL=https://data-api.polymarket.com
```

Existing websocket endpoints remain separately configurable:

```env
POLYMARKET_MARKET_WS_URL=wss://ws-subscriptions-clob.polymarket.com/ws/market
POLYMARKET_USER_WS_URL=wss://ws-subscriptions-clob.polymarket.com/ws/user
```

For a sandbox smoke, use a dedicated env file and a burner wallet:

```env
POLYMARKET_CLOB_API_URL=https://clob.loremipsumtrade.com
POLYMARKET_DATA_API_URL=<sandbox data api if provided>
POLYMARKET_MARKET_WS_URL=<sandbox market ws if provided>
POLYMARKET_USER_WS_URL=<sandbox user ws if provided>
POLYMARKET_PRIVATE_KEY=<burner only>
POLYMARKET_SIGNATURE_TYPE=eoa
POLYMARKET_FUNDER_ADDRESS=
```

Do not reuse production API keys, production private keys, funded proxy wallets,
or the tiny-live wallet against third-party endpoints.

## Required venue-parity checks

Before resuming tiny-live, validate the following against either official
Polymarket tiny-live or a safe sandbox:

- Submit post-only limit order, verify open-order sync returns it.
- Cancel order, verify open-order sync removes it.
- Submit both legs, verify both are working or neither remains active.
- Force or observe one-leg fill, verify local inventory is not flat.
- Verify late fills after cancel are applied to inventory.
- Verify trades sync is filtered to our maker/taker address only.
- Verify user websocket trade/order events and REST reconciliation agree.
- Verify venue position/account state agrees with local inventory.
- Verify merge/redeem events are treated as inventory operations, not normal
  trade fills.

## CLOB V2 migration risk

Polymarket has announced a CLOB V2 cutover on 2026-04-28 with new exchange
contracts, pUSD collateral, order payload changes, and SDK migration work. The
Rust dependency must be checked for V2 support before any live run after that
cutover. If the Rust SDK is not V2-ready, freeze live trading until the adapter
is upgraded or replaced.
