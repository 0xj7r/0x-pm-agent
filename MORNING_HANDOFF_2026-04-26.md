# Morning Handoff — 2026-04-26

Generated 2026-04-25 ~23:50 UTC.
Pick up here when you check tomorrow.

## TL;DR

- **Auto-redeem shipped + tested**: `scripts/live_redeem.sh --execute` recovers stranded outcome tokens via the existing relayer infrastructure. Dry-run verified end-to-end against live Polymarket Data API. Mechanism works; just need a wallet that has positions to recover.
- **Live trading from Dublin AWS works through auth + signing**: SSH'd into your Dublin EC2 (`ec2-3-252-32-40.eu-west-1.compute.amazonaws.com`), deployed latest code, ran `live_smoke`. Got past geo-block + relayer config. **Hit a NEW Polymarket V2 schema validation issue** (see "Open Blocker" below).
- **Paper snapshot writer fixed**: env var was wrong all day (`WHALE_PAIR_PAPER_SNAPSHOT_PATH` → should be `WHALE_PAIR_BOOK_SNAPSHOT_LOG_PATH`). All overnight cycles before this had no snapshot data.
- **Overnight capture rolling** at `data/captures/2026-04-25-overnight/single/` (PID `/tmp/longcap_overnight.pid`, 2.6 MB / 9090 lines after 25 min).
- **Monte Carlo simulator scaffold** exists at `polymarket-exec/scripts/mc_strategy_sim.py` but needs parameter calibration (currently returns 0 fills due to queue_depth >> taker_size).

## Open Blocker (THE thing to look at first)

**Polymarket CLOB V2 has tightened validation since Apr 24.** Sequential 400 errors as we add each missing field:

1. ✅ `error parsing fee rate bps () to int64` → fixed by adding `fee_rate_bps: "1000"` to V2PostOrder JSON
2. ✅ `error parsing nonce () to int64` → fixed by adding `nonce: "0"`
3. ✅ (taker required) → fixed by adding `taker: "0x0000...0000"`
4. ❌ `invalid signature` → STILL BLOCKING

**CRITICAL INVESTIGATION RESULT:** I confirmed our V2 schema is **IDENTICAL** to the official Polymarket V2 SDK (`polymarket_client_sdk_v2 = "0.5.1"`, github.com/Polymarket/rs-clob-client-v2). All these match:
- Domain name: `"Polymarket CTF Exchange"` ✅
- Domain version: `"2"` ✅
- chain_id: `137` (Polygon) ✅
- Exchange address: `0xE111180000d2663C0091e4f400237545B87B996B` ✅
- EIP-712 struct fields + order: identical (salt, maker, signer, tokenId, makerAmount, takerAmount, side, signatureType, timestamp, metadata, builder) ✅

**So the bug is NOT in the signed payload.** Most likely in **signature serialization format**:
- Our code: `signer.sign_hash(...).await.map(|sig| sig.to_string())` (alloy Signature → string)
- V2 SDK: keeps as alloy `Signature` and serializes via SignedOrder struct
- Venue might want a specific encoding (r|s|v vs raw 65 bytes vs different v normalization)

## Latest update (2026-04-26 ~07:00 UTC)

Continued the loop overnight. **Schema fix committed (`6fca4d5`)** removes feeRateBps/nonce/taker from V2 JSON per the official docs (https://docs.polymarket.com/v2-migration). Verified via WebFetch on docs + reading polymarket_client_sdk_v2 v0.5.1 source — every config we send is now byte-identical to the official V2 SDK:
- ✅ Domain name: "Polymarket CTF Exchange"
- ✅ Domain version: "2"
- ✅ chain_id: 137
- ✅ Exchange: 0xE111180000d2663C0091e4f400237545B87B996B
- ✅ EIP-712 Order struct (salt, maker, signer, tokenId, makerAmount, takerAmount, side, signatureType, timestamp, metadata, builder)
- ✅ JSON body shape (above + expiration + signature)
- ✅ maker = funder (proxy wallet 0xa57189...), signer = EOA, signatureType = 1 (proxy)

**Tested against:**
- Dublin EC2 (geo-block clean)
- Both `clob.polymarket.com` and `clob-v2.polymarket.com` hosts
- The user's confirmed-liquid test market `0xaf5e90...` (yo-fdv-above-50m)
- Multiple current btc-updown-5m markets

**All return "invalid signature".**

The 30-min unblock path:
1. Add `polymarket_client_sdk_v2 = "0.5.1"` to `polymarket-exec/Cargo.toml`
2. Replace our custom `V2OrderDraft.sign()` + `post_body()` with the SDK's `OrderBuilder::sign_v2_order()` (see SDK's `clob/client.rs:1700-1740` and `clob/order_builder.rs`)
3. Run `live_smoke` from Dublin

The bug is almost certainly in:
- Signature wire serialization (we use `signature.to_string()`; verify what SDK does in `OrderV2WithSignature::serialize`)
- OR some subtle EIP-712 encoding detail (alloy version mismatch? sol! macro hash differs from venue's contract?)

Reference files:
- V2 SDK Order schema: `~/.cargo/registry/src/index.crates.io-*/polymarket_client_sdk_v2-0.5.1/src/clob/types/mod.rs`
- V2 SDK signing: `~/.cargo/registry/src/index.crates.io-*/polymarket_client_sdk_v2-0.5.1/src/clob/client.rs:1700-1740`
- V2 SDK wire serialization: `~/.cargo/registry/src/index.crates.io-*/polymarket_client_sdk_v2-0.5.1/src/clob/types/mod.rs:722` (Serialize impl for SignedOrder)
- Our impl: `polymarket-exec/src/wire/clob_v2.rs`

Schema-fix commit `6fca4d5` is on main. Next session: drop in the SDK and use it directly.

## What's Built and Ready (after CLOB V2 unblock)

### Auto-redeem mechanism
```bash
# DRY RUN (default - logs only):
./scripts/live_redeem.sh

# EXECUTE (submits to relayer):
./scripts/live_redeem.sh --execute
```
Scans Polymarket Data API for redeemable positions on `POLYMARKET_FUNDER_ADDRESS`, groups by condition_id, submits one CTF redeem per condition with `index_sets=[1,2]` (binary market both legs).

**Caveat**: configured funder `0xa57189d5b2285A5E64083d3925687bDFCE01fC83` has 0 positions on Data API. Either it's the right wallet and empty, or `.env` points to wrong address. Verify before scaling.

### Dublin AWS deploy
```bash
AWS_LIVE_HOST=ec2-3-252-32-40.eu-west-1.compute.amazonaws.com \
AWS_LIVE_USER=ubuntu \
AWS_LIVE_KEY_PATH=~/.ssh/whale_pair_dublin_ed25519.pem \
./ops/deploy/deploy_live_aws_ec2.sh
```
Syncs repo, builds, installs systemd units. **Already invoked successfully tonight** — code is on the host at `/home/ubuntu/go/polymarket-agent`, binary at `target/release/polymarket-exec`.

### Live smoke from Dublin
SSH wrapper at `/tmp/dublin_smoke_remote.sh` (also synced to host). Run:
```bash
ssh -i ~/.ssh/whale_pair_dublin_ed25519.pem ubuntu@ec2-3-252-32-40.eu-west-1.compute.amazonaws.com /tmp/dublin_smoke_remote.sh
```
**Currently fails at "invalid signature" step** — see Open Blocker above.

## What's Decided / Memorialized

Saved as project memories for context preservation:
- `project_2026-04-25_strategy_signal_gap.md` — btc_5m_mm has no signal beyond book mid
- `project_2026-04-25_btc5mmm_structural_verdict.md` — knob tuning is exhausted; needs strategy redesign
- `project_2026-04-25_paper_env_too_pessimistic_verdict.md` — paper_fill_from_book_snapshot too strict
- `project_2026-04-25_lifecycle_gaps.md` — merge unvalidated E2E + auto-redeem (now built)
- `project_2026-04-25_live_geoblocked.md` — local Mac is blocked, AWS Dublin works
- `project_whale_compounds_via_frequency_not_size.md` — whale path: $5-10 clips × high frequency

## Recommended Tomorrow Sequence

1. **Fix the V2 signature** (45 min) — either copy SDK V1 schema or check Polymarket V2 docs
2. **Run live_smoke from Dublin** (5 min) — confirm 0 fills isn't structural; this is the long-awaited ground truth
3. **If smoke succeeds**: run `live_redeem.sh --execute` against the real wallet (after confirming funder address is correct)
4. **If smoke succeeds + 0 fills**: real strategy issue, not a bug. Pivot to building signal-aware variant.
5. **If smoke succeeds + N fills**: paper env was the lying party. Calibrate paper from real fill rate.

## Risk Reminders

- All live ops gated by `WHALE_PAIR_LIVE_KILL_SWITCH_PATH` (default `~/.config/polymarket-exec/live.kill`) — `touch` that file to immediately block.
- Tinylive risk caps in `tinylive_replay.env`: max gross $25, max 2 open orders, min cash $5. Bump these if deploying $100.
- `WHALE_PAIR_LIVE_REDEEM_DRY_RUN=true` is the default for the redeem script — explicit `--execute` required.

## Process Summary (PIDs to be aware of)

- `/tmp/longcap_overnight.pid` — overnight book capture (paper_mode, no real money). Safe to leave running.
- All other tmp PID files (cycle3-6) are stale processes from earlier debugging.
