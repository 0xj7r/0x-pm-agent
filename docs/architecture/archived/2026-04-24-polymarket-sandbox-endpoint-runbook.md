# Polymarket-compatible sandbox endpoint runbook

Status: safety runbook
Date: 2026-04-24
Primary runtime: `polymarket-exec`

## 1. Decision

Treat third-party Polymarket-compatible sandboxes as untrusted unless they can be
used through API endpoints without connecting a production wallet.

Do not connect the user's real wallet, tiny-live wallet, production API keys, or
funded proxy/Safe wallet to `loremipsumtrade`, `polysandbox`, `clob.tools`, or any
other third-party simulator.

If a third-party sandbox requires signing before API access, use only a fresh
burner wallet with:

- no funds
- no token approvals
- no production Polymarket account linkage
- no reuse as a future live trading wallet

## 2. Current runtime support

The runtime has separate endpoint surfaces:

- `POLYMARKET_CLOB_API_URL`: REST CLOB host for submit/cancel/open-order/fill
  sync when wired through live auth.
- `POLYMARKET_MARKET_WS_URL`: public market websocket URL.
- `POLYMARKET_USER_WS_URL`: authenticated user websocket URL.

This separation matters because some simulators advertise a REST-compatible CLOB
but do not clearly publish matching market/user websocket endpoints. In that
case, only REST smoke validation is in scope; websocket reconciliation cannot be
claimed.

The safe endpoint template is:

- `polymarket-exec/env/sandbox_endpoint.example.env`

## 3. Endpoint-only evaluation

Before any signed/burner test, verify the provider can be used as plain API:

1. Read provider docs and confirm whether API access requires wallet login.
2. Confirm the REST base URL.
3. Confirm whether Polymarket-compatible market/user websocket URLs exist.
4. Confirm whether market IDs and token IDs are synthetic or mirror production.
5. Confirm whether orders are virtual and cannot affect production Polymarket.

If any answer is unclear, stop and use an internal deterministic simulator
instead.

## 4. loremipsumtrade posture

The public loremipsumtrade page advertises:

- REST host: `https://clob.loremipsumtrade.com`
- Polymarket SDK-style compatibility
- websocket support
- virtual balances

It also describes a wallet-connect/SIWE onboarding step. That makes it unsuitable
for the user's real wallet. Use it only if API access works without wallet
connection, or with a burner wallet as defined above.

## 5. Burner-only smoke plan

Only after a burner wallet exists:

```bash
cp polymarket-exec/env/sandbox_endpoint.example.env /tmp/polymarket-sandbox.env
```

Edit `/tmp/polymarket-sandbox.env` with burner credentials and sandbox-specific
market/token IDs, then run:

```bash
set -a
source /tmp/polymarket-sandbox.env
set +a
cargo run -p polymarket-exec
```

Required pass criteria:

- REST auth succeeds against the sandbox endpoint.
- One tiny post-only GTD order is accepted.
- Open-order sync returns that order.
- Cancel succeeds.
- Open-order sync no longer returns that order.
- No local order remains `Open`, `CancelRequested`, or `NeedsReconcile`.

Additional websocket pass criteria, only if sandbox websocket URLs are confirmed:

- Market websocket receives book updates for the sandbox market.
- User websocket receives order-open/cancel/fill events for the burner account.
- REST reconciliation and user websocket events agree.

## 6. When to prefer internal deterministic simulation

Build/use an internal deterministic simulator instead of an external sandbox if:

- wallet connection is required before API access
- websocket endpoints are absent or undocumented
- market/token identity mapping is unclear
- submit/cancel/fill semantics differ from Polymarket CLOB
- the sandbox cannot reproduce delayed cancel acks, late fills, partial fills, or
  user-event ordering

The internal simulator remains the right tool for CI and BDD invariants. External
sandboxes are useful only for adapter-compatibility smoke tests.
