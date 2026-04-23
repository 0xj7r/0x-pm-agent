# whale-pair-exec

Rust execution runtime for Polymarket paper/live strategies.

Current strategy modes:

- `unlawful_shear`
- `goat_pair`
- `noop`

## Current recommended path

Use `unlawful_shear` in paper mode only.

This mode is grounded in the wallet research for:

- `0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82`
- alias: `unlawful-shear`

It currently implements:

- two-sided default participation
- expensive-core / cheap-hedge accumulation
- repeated clip-based rebalance
- late-window acceleration using sidecar market timing
- paired loser salvage with `reduce_only` sells

It does not yet implement:

- fully calibrated BTC-conditioned sizing and pacing
- full `price_to_beat` threshold decisioning
- merge-aware execution accounting
- queue-aware paper fills

## Runtime setup

1. export live market context and runtime env from Gamma

```bash
python3 scripts/export_btc_5m_runtime.py --source live \
  --include-prev 1 \
  --include-next 1 \
  --env-out data/research/wallet_research/unlawful-shear/rust_runtime.env \
  --context-out data/research/wallet_research/unlawful-shear/rust_market_context.json
```

2. copy `.env.example` to `.env` and fill anything runtime-specific you want to override

The generated runtime env file will populate:

- `WHALE_PAIR_ASSET_IDS`
- `WHALE_PAIR_INSTRUMENT_MARKETS`
- `WHALE_PAIR_USER_MARKETS`

The live exporter now prepares a rolling BTC 5m slate by exact slug:

- previous window for cleanup
- current active window
- next window for readiness

The runtime can optionally load a JSON strategy profile via `WHALE_PAIR_STRATEGY_PROFILE_PATH`, but that path is no longer required.
Current safe default is:

- strategy mode from `WHALE_PAIR_STRATEGY`
- built-in Rust defaults
- env overrides for sleeve-specific tuning

Optional auth is still only needed if user websocket is required.

3. run a named sleeve in paper mode once a Rust toolchain is installed

```bash
whale-pair-exec/scripts/run_sleeve.sh unlawful_baseline
```

Offline bootstrap from the local DB is also supported:

```bash
WHALE_PAIR_CONTEXT_SOURCE=db \
WHALE_PAIR_INCLUDE_PREV=1 \
WHALE_PAIR_INCLUDE_NEXT=1 \
WHALE_PAIR_SKIP_CARGO_RUN=true \
whale-pair-exec/scripts/run_sleeve.sh unlawful_baseline
```

## Parallel paper sleeves

Current checked-in sleeve presets:

- `unlawful_baseline`
- `unlawful_broad_hours`
- `unlawful_press`
- `goat_pair_baseline`

Run them in parallel with separate terminals or user services:

```bash
whale-pair-exec/scripts/run_sleeve.sh unlawful_baseline
whale-pair-exec/scripts/run_sleeve.sh unlawful_broad_hours
whale-pair-exec/scripts/run_sleeve.sh unlawful_press
whale-pair-exec/scripts/run_sleeve.sh goat_pair_baseline
```

Each sleeve env file owns its own:

- service name
- metrics port
- SQLite order store
- JSONL journal path

## Tiny-live envs

Paper mode does not require user-auth credentials.

Tiny-live requires:

- `POLYMARKET_API_KEY`
- `POLYMARKET_API_SECRET`
- `POLYMARKET_API_PASSPHRASE`
- `WHALE_PAIR_PAPER_MODE=false`

Optional dashboard/event envs:

- `WHALE_PAIR_DASHBOARD_WHALE_EVENTS_PATH`
- `WHALE_PAIR_DASHBOARD_REFRESH_MS`
- `WHALE_PAIR_DASHBOARD_EVENT_LIMIT`

## Validation bar before live money

Do not move this mode to live money until:

1. the crate builds and tests cleanly on a machine with Rust installed
2. paper journals look stable across multiple days
3. inventory stays bounded under rapid repricing
4. trim / salvage behavior is explainable on real BTC 5m windows
5. the paper strategy roughly matches the intended unlawful-shear geometry

Detailed operator steps live in:

- `docs/architecture/2026-04-23-btc-5m-mm-paper-and-tiny-live-runbook.md`
