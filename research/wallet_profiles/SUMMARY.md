# Two-Sided Strategy Research — Wallet Profiles

Captured 2026-04-22. Raw data: `research/wallet_profiles/*.json` (positions + activity).

## Context

Live directional bot (`btc-live-t10`, profile `research_t10_live`) ran ~$120 equity → +$1.71 net P&L over 2h on 6 entries, all DOWN, 5/6 in an UP regime. Symptoms: (1) 0.04% move threshold is sub-noise; (2) no regime gate despite regime label being computed; (3) p_win bucket averages across regimes. Gave back +$13.26 → +$1.71 in 1h (classic "wins returned").

Scanned four well-performing Polymarket wallets on the same 5-min BTC product. **All four use two-sided or structural-arb strategies. None predicts BTC direction.** Our bot is the only one of five trying to predict.

## Wallet comparison

| Handle | Address | Equity | Class | Events | BUYs | SPLITs | MERGEs | REDEEMs | Span (h) |
|---|---|---|---|---|---|---|---|---|---|
| **weird-peak** | `0x9f5f…a008` | **$70,241** | merge-arb | 3,500 | 3,297 | 0 | **173** | 30 | 1.05 |
| **dyn3** | `0xd85e…6f11` | **$31,370** | paired-book-buy | 3,500 | 3,334 | 0 | 0 | 165 | 19.2 |
| **peter-dugo** | `0x3f2f…8150` | $10,393 | scaled-accumulation | 3,500 | 3,122 | 0 | 0 | 377 | 17.8 |
| **dcbf-split-arb** | `0xdcbf…1bf1` | $6,918 | split-arb | 3,500 | 0 | **1,222** | 0 | 1,219 | 104 |
| **us (btc-live-t10)** | `0x97fB…47c5` | $120 | threshold-directional | ~50/day | 6 | 0 | 0 | 2 | 2 |

## Strategy class definitions

### 1. Merge-arb (weird-peak)
**Edge:** $Y_{ask} + N_{ask} < \$1$
**Execution:** buy Y at ask, buy N at ask, MERGE pair → receive $1 via CTF `mergePositions()`
**Profit per pair:** $1 - (Y_{ask} + N_{ask})$
**Capital cycle:** seconds (atomic, no resolution wait)
**Risk:** partial fill on one leg leaves directional exposure

### 2. Paired-book-buy (dyn3)
**Edge:** $Y_{ask} + N_{ask} < \$1$ (same as merge-arb)
**Execution:** buy Y at ask, buy N at ask, hold to resolution, REDEEM winning side
**Profit per pair:** $1 - (Y_{ask} + N_{ask})$
**Capital cycle:** resolution wait (up to 5 min)
**Risk:** same as merge-arb plus resolution wait

### 3. Split-arb (dcbf)
**Edge:** $Y_{bid} + N_{bid} > \$1$
**Execution:** CTF `splitPosition()` at $1, sell losing side on book at its bid, REDEEM winning side for $1
**Profit per split:** $(Y_{bid} + N_{bid}) - \$1$ (approximate; depends which side loses)
**Capital cycle:** split → sell → resolve (seconds to minutes)
**Risk:** book moves during the sell leg; winning-side market-sell slippage

### 4. Scaled-accumulation (peter-dugo)
**Edge:** directional conviction + maker rebates + price averaging across the window
**Execution:** ~9 laddered BUYs per window, hold to resolution, REDEEM winners
**Profit per window:** directional p_win × ($1 - avg_cost)
**Risk:** directional. Still a bet on BTC direction, just better-executed than ours.

## Recommendation — what to build first

### Primary: **merge-arb + paired-book-buy combo** (weird-peak + dyn3)

These share the same edge signal ($Y_{ask} + N_{ask} < \$1$). The difference is only the exit mechanism. Build one engine that:

1. **Enters the same way both do:** watches the live book on both tokens of a market, fires paired IOC buys when the combined ask sums to <$1 (minus a safety margin for gas + fees).
2. **Exits via whichever is cheaper:**
   - If the full pair is held, call `mergePositions()` immediately (weird-peak mode).
   - If partial fill leaves a mismatched position, hold the mismatched side to resolution and REDEEM (dyn3 fallback).

This approach:
- Uses the existing CLOB client + existing redeem sweeper (no new primitives for the fallback path).
- Leans on `clients/ctf_merger.py` (untracked in the repo) for the fast-exit path.
- Is structurally impossible to lose in expectation if the pair-sum condition holds — the only real risks are execution quality (fill rate, gas) and mis-identified edge (stale book).

**Why not split-arb first?** Two reasons:
- Needs the SPLIT primitive wired up (probably exists alongside MERGE in the CTF contract but requires its own client work).
- Has an additional leg (sell the losing side) that carries book-movement timing risk during the 30-60s window near resolution. More moving parts, more edge cases.

**Why not scaled-accumulation?** It's directional. We already know we're bad at predicting BTC on 5-min windows. No reason to bet it'll be different with fancier execution.

### Sequencing

1. **Paper v0: paired-book-buy only** (no merge). Watch the live book, identify $Y_{ask} + N_{ask} < 0.98$ opportunities (2¢ safety margin), simulate IOC paired buys, compute realized fill prices, estimate daily opportunity count + avg edge.
2. **Paper v1: add merge exit.** Hook `clients/ctf_merger.py` (once validated) into v0. Measure: does MERGE reduce hold time enough to justify the gas cost?
3. **Live v0 behind small cap.** Once paper shows positive daily edge net of gas, live-trade at $50-$100 per opportunity, watch for paper/live divergence.
4. **Scale.** Only after 1 week of live v0 showing paper parity.

### Parameters to measure in paper first

- **Opportunity rate:** how many 5-min windows/day have $Y_{ask} + N_{ask} < 0.98$ at size ≥ $100?
- **Realized fill sum vs snapshot:** does the book move against you between seeing the opportunity and filling both legs?
- **Partial-fill rate:** what fraction of paired attempts get only one leg?
- **Merge gas cost:** Polygon gas × avg cycle count vs edge captured.
- **Resolution wait (fallback):** when holding a mismatched leg, what's the realized P&L distribution?

## Files

Per-wallet JSON (positions + full activity):
- `weird-peak_0x9f5ffe76.json`
- `dyn3_0xd85eba7b.json`
- `peter-dugo_0x3f2f5459.json`
- `dcbf-split-arb_0xdcbfd3f1.json`

Summary index:
- `index.json`
