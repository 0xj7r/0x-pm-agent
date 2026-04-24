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

By default the paper launcher now re-exports that slate every `45s` and
restarts the sleeve when the active asset set changes. Override with:

- `WHALE_PAIR_CONTEXT_REFRESH_INTERVAL_SEC=<n>`
- `WHALE_PAIR_CONTEXT_REFRESH_INTERVAL_SEC=0` to disable supervision

The runtime can optionally load a JSON strategy profile via `WHALE_PAIR_STRATEGY_PROFILE_PATH`, but that path is no longer required.
Current safe default is:

- strategy mode from `WHALE_PAIR_STRATEGY`
- built-in Rust defaults
- env overrides for sleeve-specific tuning

Active paper journals can now be bounded locally with:

- `WHALE_PAIR_EXEC_JOURNAL_ROTATE_BYTES`

When set, the runtime renames the current journal into timestamped segments like
`journal.<ts>.jsonl` and reopens a fresh `journal.jsonl` automatically.

Optional auth is still only needed if user websocket is required.

3. run a named sleeve in paper mode once a Rust toolchain is installed

```bash
polymarket-exec/scripts/run_sleeve.sh unlawful_baseline
```

Offline bootstrap from the local DB is also supported:

```bash
WHALE_PAIR_CONTEXT_SOURCE=db \
WHALE_PAIR_INCLUDE_PREV=1 \
WHALE_PAIR_INCLUDE_NEXT=1 \
WHALE_PAIR_SKIP_CARGO_RUN=true \
polymarket-exec/scripts/run_sleeve.sh unlawful_baseline
```

## Parallel paper sleeves

Current checked-in sleeve presets:

- `unlawful_baseline`
- `unlawful_broad_hours`
- `unlawful_press`
- `goat_pair_baseline`

For the MM paper pass, run the unlawful sleeves in parallel. Keep
`goat_pair_baseline` as an optional taker comparator.

Run them with separate terminals or user services:

```bash
polymarket-exec/scripts/run_sleeve.sh unlawful_baseline
polymarket-exec/scripts/run_sleeve.sh unlawful_broad_hours
polymarket-exec/scripts/run_sleeve.sh unlawful_press
polymarket-exec/scripts/run_sleeve.sh goat_pair_baseline
```

Each sleeve env file owns its own:

- service name
- metrics port
- SQLite order store
- JSONL journal path

The unlawful runtime now emits one compact `unlawful eval ...` strategy note per
paired-window evaluation, so missed or taken windows can be attributed from the
journal/event stream with the exact BTC, book, activity, mode, and gate-reason inputs.

For unattended host runs, use the checked-in user-service artifacts:

- install template + env scaffolding:
  - `polymarket-exec/ops/systemd/install_user_paper_services.sh`
- manage the three unlawful sleeves together:
  - `polymarket-exec/ops/systemd/manage_unlawful_paper_services.sh`
- service template:
  - `polymarket-exec/ops/systemd/whale-pair-exec@.service`
- host-level env template:
  - `polymarket-exec/ops/systemd/common.env.example`

The user service path is:

1. copy the systemd template into `~/.config/systemd/user`
2. copy `common.env.example` to `~/.config/polymarket-exec/common.env`
3. optionally add per-sleeve overrides under `~/.config/polymarket-exec/paper.d/*.env`
4. enable lingering with `sudo loginctl enable-linger "$USER"`
5. start:

```bash
systemctl --user enable --now whale-pair-exec@unlawful_baseline
systemctl --user enable --now whale-pair-exec@unlawful_broad_hours
systemctl --user enable --now whale-pair-exec@unlawful_press
```

Optional cheap archive path:

- archive script:
  - `polymarket-exec/scripts/archive_paper_artifacts.sh`
- systemd artifacts:
  - `polymarket-exec/ops/systemd/whale-pair-archive.service`
  - `polymarket-exec/ops/systemd/whale-pair-archive.timer`

The intended model is:

- local SQLite for hot runtime state and signal snapshots
- rotated journal segments for short-lived local logs
- S3 / Glacier for cold paper artifacts

The archive script now also covers explicit runtime backup directories like
`data/runtime/*.pre_fix_*` and warns if active `journal.jsonl` files are
growing without any rotated `journal.<ts>.jsonl` segments appearing.
For cheap local candidate inspection before touching AWS, use:

```bash
WHALE_PAIR_ARCHIVE_LIST_ONLY=true \
polymarket-exec/scripts/archive_paper_artifacts.sh
```

Detailed storage guidance lives in:

- `docs/architecture/2026-04-24-paper-storage-archive-plan.md`

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

## Unlawful test loop

Use the focused unlawful suite before paper changes and after every strategy or runtime bug fix:

```bash
polymarket-exec/scripts/test_unlawful_stack.sh
```

This is the intended order:

- unlawful gate behavior scenarios
- config and logging regressions
- runtime scenario harness
- calibration exporter syntax check

The full framework and promotion gates live in:

- `docs/architecture/2026-04-24-unlawful-bdd-tdd-framework.md`
