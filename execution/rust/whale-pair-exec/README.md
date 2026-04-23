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

- direct BTC spot-path awareness
- full `price_to_beat` threshold decisioning
- merge-aware execution accounting
- queue-aware paper fills

## Runtime setup

1. export live market context and runtime env from Gamma

```bash
python3 research/dataops/export_live_gamma_runtime.py \
  --env-out data/research/wallet_research/unlawful-shear/rust_runtime.env \
  --context-out data/research/wallet_research/unlawful-shear/rust_market_context.json
```

2. copy `.env.example` to `.env` and fill anything strategy-specific you want to override

The generated runtime env file will populate:

- `WHALE_PAIR_ASSET_IDS`
- `WHALE_PAIR_INSTRUMENT_MARKETS`
- `WHALE_PAIR_USER_MARKETS`

Optional auth is still only needed if user websocket is required.

3. run in paper mode once a Rust toolchain is installed

```bash
execution/rust/whale-pair-exec/scripts/run_unlawful_shear_paper.sh
```

## Validation bar before live money

Do not move this mode to live money until:

1. the crate builds and tests cleanly on a machine with Rust installed
2. paper journals look stable across multiple days
3. inventory stays bounded under rapid repricing
4. trim / salvage behavior is explainable on real BTC 5m windows
5. the paper strategy roughly matches the intended unlawful-shear geometry
