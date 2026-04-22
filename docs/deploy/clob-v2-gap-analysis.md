# CLOB V2 Gap Analysis

Status: exact migration gap from current repo reality

## External reference points

Primary sources checked:

- Polymarket migration guide: `https://docs.polymarket.com/v2-migration`
- Polymarket changelog: `https://docs.polymarket.com/changelog`
- pUSD docs: `https://docs.polymarket.com/concepts/pusd`
- `py-clob-client-v2` repo: `https://github.com/Polymarket/py-clob-client-v2`

Repo reality checked against:

- `clients/polymarket.py`
- `clients/polymarket_ws.py`
- `clients/polymarket_user_ws.py`
- `config.py`
- `core/engine.py`
- `scripts/whale_pair_live_bot.py`
- `clients/ctf_redeemer.py`
- `clients/ctf_merger.py`
- `requirements.txt`
- `tests/test_polymarket_client.py`

## Executive summary

The repo is not V2-ready today.

The break is not limited to swapping a package name. The current live path assumes:

- V1 Python SDK package and symbols
- V1 order-construction flow
- fee-rate-in-signed-order behavior
- USDC.e as live collateral
- current on-chain settlement helpers without a pUSD wrapper stage

The market and user websocket patterns may survive with limited changes, but the authenticated trading and collateral surfaces do not.

## Gap matrix

### 1. SDK package and import surface

Current repo:

- `requirements.txt` pins `py-clob-client`
- `clients/polymarket.py` imports from `py_clob_client.*`
- tests patch `py_clob_client.client.ClobClient`

V2 requirement:

- migrate to `py-clob-client-v2`

Impact:

- `clients/polymarket.py`
- `requirements.txt`
- `tests/test_polymarket_client.py`
- any scripts or tools importing V1 types

Blocker class: hard blocker

### 2. Client construction and credential bootstrap

Current repo:

- V1-style `ClobClient(config.CLOB_URL, key=..., chain_id=..., signature_type=..., funder=...)`
- startup uses `derive_api_key()` and fallback `create_api_key()`

V2 surface:

- V2 SDK exposes `create_or_derive_api_key()`
- constructor and method surface differ from V1

Repo implication:

- `PolymarketClient.__init__` and `_resolve_api_creds()` must be rewritten
- the rest of the repo should stop reaching into SDK-specific credential behavior directly

Blocker class: hard blocker

### 3. Order creation API

Current repo:

- calls `get_fee_rate_bps(token_id)`
- builds `OrderArgs(... fee_rate_bps=...)`
- `create_order(...)`
- `post_order(...)`

V2 requirement:

- signed orders no longer carry `feeRateBps`
- official V2 client examples use `create_and_post_order(...)`
- market info / fee info moves to venue-side metadata

Repo implication:

- `clients/polymarket.py::place_order` is incompatible as written
- any downstream code that expects `fee_rate_bps` in the response is now coupling itself to a V1 detail

Impacted downstream references already present:

- `core/engine.py`
- tests that assert `fee_rate_bps` propagation

Blocker class: hard blocker

### 4. Fee accounting and sizing

Current repo:

- runtime order placement queries `get_fee_rate_bps`
- research/risk code uses `shared/fees.py` with the current taker-fee formula
- live analytics attach `fee_rate_bps` to order results

V2 reality:

- fee handling is no longer embedded in the signed order
- market fee parameters are queried via V2 market-info APIs

Repo implication:

- split fee logic into:
  - venue placement semantics
  - research/economic modeling semantics
- do not let strategy code depend on signed-order fee fields

Blocker class: hard blocker for live; medium blocker for research consistency

### 5. Collateral: USDC.e to pUSD

Current repo:

- `config.py` defaults collateral token to USDC.e
- reporting and funding docs are written around USDC.e balances
- CTF merger/redeemer take the configured collateral token directly

V2 requirement:

- pUSD is the trading collateral
- API-only users wrap USDC.e into pUSD through the onramp flow

Repo implication:

- introduce an explicit collateral service/workflow:
  - balance check for pUSD
  - wrap USDC.e to pUSD
  - approve the correct contracts
- update all live-balance reporting and operator runbooks
- do not assume current USDC.e wallet balance equals live deployable quote balance

Files affected conceptually:

- `config.py`
- `clients/ctf_redeemer.py`
- `clients/ctf_merger.py`
- `scripts/pnl_report.py`
- deploy docs and funding runbooks

Blocker class: hard blocker

### 6. Exchange contracts and raw signing assumptions

Current repo:

- raw signing details are mostly hidden behind the V1 SDK
- on-chain helpers are hardcoded to current addresses and current collateral assumptions

V2 requirement:

- new exchange contracts
- EIP-712 exchange domain version bump
- different order struct

Repo implication:

- even if the SDK handles exchange signing, any direct contract assumptions in live settlement flows must be revalidated
- current merger/redeemer helpers cannot be assumed correct for V2 without contract-by-contract verification

Blocker class: hard blocker for funded live

### 7. User websocket

Current repo:

- `clients/polymarket_user_ws.py` uses API creds and a JSON auth subscribe message

V2 docs:

- L1/L2 API auth remains the same

Repo implication:

- user WS is lower risk than order placement
- still needs end-to-end validation against V2 backend behavior and event payloads

Blocker class: validation blocker, not the first blocker

### 8. Market websocket and `/book` fallback

Current repo:

- market WS in `clients/polymarket_ws.py`
- HTTP fallback through `get_order_book`
- whale-pair runner depends on both

V2 docs:

- production base URL hot-swaps after cutover
- backend is rewritten

Repo implication:

- book/subscription payloads must be verified against V2 test endpoint
- fallback path remains useful but cannot be assumed wire-compatible without shadow validation

Blocker class: validation blocker

### 9. Recovery after cutover/order wipe

Current repo:

- startup recovery is local-process oriented
- there is no explicit cutover-day boot mode

V2 cutover:

- open orders are wiped

Repo implication:

- startup reconciliation must accept:
  - zero open orders
  - live balances differing from local pre-cutover assumptions
  - pUSD inventory replacing USDC.e-centric reasoning

Blocker class: hard blocker for live launch around cutover

## Concrete migration workstreams

### Workstream A: compatibility shim in Python

Goal:

- get current Python shadow stack running on CLOB V2 before Rust exists

Deliverables:

- new client adapter around `py-clob-client-v2`
- no V1 imports left in live/runtime paths
- shadow validation against `https://clob-v2.polymarket.com`

### Workstream B: collateral and settlement rewrite

Goal:

- make collateral state explicit and pUSD-aware

Deliverables:

- wrap/approve workflow
- pUSD balance checks
- updated funding/reporting docs
- revalidated merge/redeem implementation plan

### Workstream C: execution-plane separation

Goal:

- prevent the repo from staying permanently coupled to a Python SDK surface

Deliverables:

- Rust execution service boundary
- Python reduced to research/control/support roles

## Ordered blockers

1. V1 SDK dependency in `clients/polymarket.py` and `requirements.txt`
2. V1 order-placement contract in `place_order()`
3. USDC.e collateral assumption across config/reporting/settlement helpers
4. No explicit pUSD wrap/approval workflow
5. No V2-validated startup reconciliation for post-wipe recovery
6. No execution/control-plane split for standby-safe live trading

