# polymarket-exec

Rust execution engine for Polymarket paper and tiny-live strategy operation.

This crate is the runtime/operator surface for:

- market + user websocket ingestion
- BTC spot telemetry ingestion
- strategy decisioning (`unlawful_shear`, `goat_pair`, `noop`)
- order lifecycle state machine and reconciliation
- risk limits, inventory tracking, and persistence
- ops API (`/healthz`, `/api/state`, `/metrics`)

## Repository layout

The crate now uses grouped modules:

- `src/core/`: shared domain primitives (`book`, `types`, `inventory`, `risk`, `market_context`)
- `src/market_making/`: quote construction/reconciliation and pair/merge mechanics
- `src/runtime/`: runtime orchestration, state machine, persistence, reconciliation
- `src/wire/`: external I/O adapters (Polymarket ws/api, spot ws, HTTP ops API)
- `src/signals/`: BTC regime + unlawful gate signal overlays
- `src/strategy.rs`: strategy decision logic and sizing/execution intent generation

Compatibility re-exports are preserved in `src/lib.rs` so older internal paths still compile.

## Quick start (paper)

1. Export rolling market context:

```bash
python3 scripts/export_btc_5m_runtime.py --source live \
  --include-prev 1 \
  --include-next 1 \
  --env-out data/research/wallet_research/unlawful-shear/rust_runtime.env \
  --context-out data/research/wallet_research/unlawful-shear/rust_market_context.json
```

2. (Optional) copy `polymarket-exec/.env.example` to `polymarket-exec/.env` and override defaults.

3. Run a sleeve:

```bash
polymarket-exec/scripts/run_sleeve.sh unlawful_broad_hours
```

Notes:

- Launcher refreshes prev/current/next BTC 5m slate every `45s` by default.
- Override with `WHALE_PAIR_CONTEXT_REFRESH_INTERVAL_SEC=<n>`.
- Set `WHALE_PAIR_CONTEXT_REFRESH_INTERVAL_SEC=0` to disable auto-refresh supervision.

## Tests and validation

Focused stack:

```bash
polymarket-exec/scripts/test_unlawful_stack.sh
```

Full crate:

```bash
cargo check -p polymarket-exec
cargo test -p polymarket-exec
```

## Tiny-Live Merge/Recycling

The strategy is not live-faithful unless paired inventory can be recycled. The
live adapter now carries `condition_id` from Polymarket Data API positions into
merge intents and submits proxy-wallet CTF `mergePositions` through the
Polymarket builder relayer when configured.

Required live env for proxy-wallet merge:

```bash
POLYMARKET_SIGNATURE_TYPE=proxy
POLYMARKET_PRIVATE_KEY=...
POLYMARKET_FUNDER_ADDRESS=...
RELAYER_API_KEY=...
RELAYER_API_KEY_ADDRESS=...
```

`RELAYER_API_KEY_ADDRESS` is the address that owns the relayer key. It does
not have to be the same as the bot signer. The bot still signs the relayed CTF
transaction with `POLYMARKET_PRIVATE_KEY`; the relayer key authenticates the
app/builder submission. The inventory-holding wallet must be correct:
`POLYMARKET_FUNDER_ADDRESS` or `POLYMARKET_PROXY_WALLET_ADDRESS` should point
at the Polymarket proxy wallet that owns the outcome tokens.
The legacy env alias `POLYMARKET_FUNDER` is also accepted.

Optional venue overrides:

```bash
POLYMARKET_RELAYER_URL=https://relayer-v2.polymarket.com
POLYMARKET_CTF_CONTRACT_ADDRESS=0x4D97DCd97eC945f40cF65F87097ACe5EA0476045
POLYMARKET_COLLATERAL_TOKEN_ADDRESS=0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174
POLYMARKET_COLLATERAL_DECIMALS=6
```

If relayer credentials or `condition_id` are missing, live merge fails closed
and the runtime degrades rather than continuing to add exposure.

## Systemd operation

Install templates and env scaffolding:

```bash
polymarket-exec/ops/systemd/install_user_paper_services.sh
```

Manage sleeves:

```bash
polymarket-exec/ops/systemd/manage_unlawful_paper_services.sh status
```

Primary unit names:

- `polymarket-exec@unlawful_baseline`
- `polymarket-exec@unlawful_broad_hours`
- `polymarket-exec@unlawful_press`
- `polymarket-exec-live-smoke.service`
- `polymarket-exec-archive.service`
- `polymarket-exec-archive.timer`

User config path:

- `~/.config/polymarket-exec/common.env`
- `~/.config/polymarket-exec/paper.d/*.env`
- `~/.config/polymarket-exec/live.env`
- `~/.config/polymarket-exec/live.kill`

AWS live host sync:

```bash
AWS_LIVE_HOST=<ec2-public-ip-or-dns> ops/deploy/deploy_live_aws_ec2.sh
```

The AWS helper installs the live-smoke one-shot service but does not start it.
Edit `~/.config/polymarket-exec/live.env` on the host first, then run:

```bash
systemctl --user start polymarket-exec-live-smoke.service
```

## Data paths

Runtime writes under crate-local data paths:

- `polymarket-exec/data/runtime/*/order-store.sqlite`
- `polymarket-exec/data/execution/paper/*/journal.jsonl`
- `polymarket-exec/data/execution/audit/*/audit.jsonl`

## Live safety posture

Before any live capital:

- run `WHALE_PAIR_EXEC_MODE=live_smoke` submit/open-sync/cancel/disappear check
- verify startup reconciliation against venue open orders
- ensure user websocket auth + event flow are healthy
- keep one sleeve and tiny notional until post-trade reconciliation is stable

Detailed operating runbooks:

- `docs/architecture/2026-04-23-btc-5m-mm-paper-and-tiny-live-runbook.md`
- `docs/architecture/2026-04-23-btc-5m-mm-infra-requirements.md`
- `docs/architecture/2026-04-24-paper-storage-archive-plan.md`
