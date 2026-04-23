# Whale Research Index

Status: active  
Last updated: 2026-04-23

This index points to the surviving local data and the rebuilt research threads for the main wallets we care about.

## Current State

The old ad hoc local notes and JSON sidecars are mostly gone. The durable source of truth that survived is the SQLite research DBs under `data/research/wallet_research/`.

That means:

- `unlawful-shear` can be reconstructed from deep local microstructure data
- `xuanxuan008`, `penny-tail`, and `split-sell` still have strong raw activity history locally, but much less market microstructure
- `Bonereaper` does not currently have a local research DB in the repo, so its thread is based on live API / chain evidence from this session

## Wallets

### `unlawful-shear`

- address: `0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82`
- local DB: [wallet_research.db](/Users/jackreid/go/polymarket-agent/data/research/wallet_research/unlawful-shear/wallet_research.db)
- thread: [unlawful-shear-reconstruction-thread.md](/Users/jackreid/go/polymarket-agent/docs/research/unlawful-shear-reconstruction-thread.md)
- signal spec: [unlawful-shear-signal-pack-spec.md](/Users/jackreid/go/polymarket-agent/docs/research/unlawful-shear-signal-pack-spec.md)
- machine-readable pack: [unlawful_signal_pack.json](/Users/jackreid/go/polymarket-agent/data/research/wallet_research/unlawful-shear/unlawful_signal_pack.json)
- key call:
  - wallet-level economics are maker/MM-active
  - intrawindow behavior still looks like active core/hedge shaping, repeated clip-based accumulation, merge recycling, and late-window salvage

### `Bonereaper`

- address: `0xeebde7a0e019a63e6b476eb425505b7b3e6eba30`
- public profile: https://polymarket.com/@bonereaper
- thread: [bonereaper-research-thread.md](/Users/jackreid/go/polymarket-agent/docs/research/bonereaper-research-thread.md)
- key call:
  - strong short-duration crypto MM-style wallet
  - confirmed maker-active via live rebate evidence
  - active in ETH 5m, ETH 15m, BTC 5m, and BTC 15m

### `xuanxuan008`

- address: `0xcfb103c37c0234f524c632d964ed31f117b5f694`
- local DB: [wallet_research.db](/Users/jackreid/go/polymarket-agent/data/research/wallet_research/xuanxuan008/wallet_research.db)
- current local coverage:
  - raw activity
  - closed positions
  - minimal accounting snapshots
- key call:
  - still the best non-MM two-sided replication target

### `penny-tail`

- address: `0x7da07b2a8b009a406198677debda46ad651b6be2`
- local DB: [wallet_research.db](/Users/jackreid/go/polymarket-agent/data/research/wallet_research/penny-tail/wallet_research.db)
- key call:
  - directional / tail-style taker, not MM

### `split-sell`

- address: `0xe51b3d64da5b0b8a07a55f8bb3c3170237f73cad`
- local DB: [wallet_research.db](/Users/jackreid/go/polymarket-agent/data/research/wallet_research/split-sell/wallet_research.db)
- key call:
  - split / sell arb sleeve, not MM

## Surviving Local DB Coverage

### `unlawful-shear`

- `wallet_activity_raw`: `241,243` rows
- `wallet_market_stream_events`: `265,556` rows
- `wallet_orderbook_snapshots`: `646` rows
- `wallet_window_reconstructions`: `531` rows
- `wallet_closed_positions_raw`: `137` rows
- `wallet_accounting_snapshots`: `20` rows

### `xuanxuan008`

- `wallet_activity_raw`: `59,721` rows
- `wallet_closed_positions_raw`: `50` rows
- `wallet_accounting_snapshots`: `2` rows

### `penny-tail`

- `wallet_activity_raw`: `219,761` rows
- `wallet_closed_positions_raw`: `50` rows

### `split-sell`

- `wallet_activity_raw`: `23,314` rows
- `wallet_closed_positions_raw`: `67` rows
- `wallet_accounting_snapshots`: `2` rows

## Recommended Next Steps

1. Treat [unlawful-shear-reconstruction-thread.md](/Users/jackreid/go/polymarket-agent/docs/research/unlawful-shear-reconstruction-thread.md) as the canonical reverse-engineering thread for the current maker-first engine rebuild.
2. Treat [bonereaper-research-thread.md](/Users/jackreid/go/polymarket-agent/docs/research/bonereaper-research-thread.md) as the canonical second benchmark for cross-market crypto MM behavior.
3. Use [unlawful_signal_pack.json](/Users/jackreid/go/polymarket-agent/data/research/wallet_research/unlawful-shear/unlawful_signal_pack.json) and [unlawful-shear-signal-pack-spec.md](/Users/jackreid/go/polymarket-agent/docs/research/unlawful-shear-signal-pack-spec.md) as the research-to-implementation handoff for the Rust strategy build.
4. Pull a new local DB for `Bonereaper`, then run the same signal-pack export so both benchmark wallets are on the same format.

## Shared Funding Note

`unlawful-shear` and `Bonereaper` were both visibly funded by:

- `0xf70da97812cb96acdf810712aa562db8dfa3dbef`

Current best read:

- shared funder: yes
- Safe / contract wallet on Polygon: no
- likely role: treasury / allocator wallet for a broader operator cluster
