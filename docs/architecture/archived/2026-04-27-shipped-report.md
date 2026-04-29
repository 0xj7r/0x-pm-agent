# Shipped Report — 2026-04-26 to 2026-04-27

Comprehensive record of every behavioral change shipped over the past
~36 hours, organized by category and impact.

## Executive summary

| Period | Net P&L | Drawdown peak-to-trough | Markets traded |
|---|---|---|---|
| 24h to 18:13 UTC 2026-04-27 | **-$47.77** (excl rebates) | **-$145.29** | 201 |

**40 commits shipped**. Most are correct bug fixes that improved the
engine's reliability. A subset of "tunings" that ran 2026-04-27 morning
amplified an underlying structural issue (asymmetric paired-bid fills in
trending markets) and were reverted in commit `8e26925`.

The bot is now running v34 conservative defaults. Day-analysis
(`2026-04-27-day-analysis.md`) shows the structural gap vs whale
`unlawful_shear`: 73× less capital-per-market, 45× fewer trades-per-market,
1.8× more lopsided fill ratio. Recovery roadmap is laid out in 3 tiers there.

---

## Category 1: Bug fixes (silent failures, real $$ impact)

### Pre-night (2026-04-26)

#### `2be1466` — Force-insert venue position when local is missing
- **Symptom**: After bot restart, `inventory.positions()` showed empty for
  positions that existed on venue. Blocked merge planner.
- **Fix**: In `inventory.rs reconcile_venue_positions`, when delta ≈ 0
  but local is None and venue has qty > 0, force-insert the position.
- **Status**: Live. Partially mitigated by other later fixes.

#### `7d662cc` — Bypass duplicate-buy filter for hedge-rescue intents
- **Symptom**: Rescue intent rejected as "duplicate buy" because a paired
  bid was still open on the same instrument.
- **Fix**: `accept_intent` adds rescue exemption to dup-buy check.
- **Status**: Now superseded by `b7aff28` IntentKind enum; same logic, cleaner.

#### `4f49f72` — Bypass submit rate cap for hedge-rescue
- **Symptom**: `quote_reconciler` rate cap (max_submit_per_window=6/20s)
  drowned rescue submissions in busy markets.
- **Fix**: Rescue intents bypass the rate cap.
- **Status**: Live, refactored into IntentKind check.

#### `5e9f13e` — Bypass entry-time risk caps for hedge-rescue
- **Symptom**: Risk engine (max_open_orders, max_position_qty,
  max_order_notional) rejected rescue intents.
- **Fix**: `core/risk.rs` adds rescue exemption to entry-time caps.
- **Status**: Live, refactored into IntentKind check.

#### `babd8f2` — Depth-sweep IOC + bypass entry caps
- **Symptom**: Rescue placed at top-of-book only; if depth was thin, FAK
  partial-filled and left residual stranded.
- **Fix**: `build_rescue_intent_for_quantity` now walks depth to find the
  price needed to sweep stranded_qty in one IOC.
- **Status**: Live.

#### `76a69bb` — Atomic on-fill rescue
- **Symptom**: Rescue waited for next snapshot tick (1-3s) after a fill
  created a stranded leg. Whale unlawful's atomic pair completion is
  sub-second.
- **Fix**: `on_fill` immediately fires rescue intent (mirrors
  unlawful's pattern).
- **Status**: Live.

### Night (2026-04-27 ~00:00–11:30 UTC)

#### `c751838` — Surface venue rejection reason on submit ack
- **Symptom**: Venue rejections only showed "order rejected by venue"
  with no reason, masking root causes.
- **Fix**: `runner.rs` promotes ack-rejected branch from `debug` to `warn`
  with full venue text.
- **Why this enabled subsequent fixes**: First time we saw the actual
  venue errors like "no orders found to match with FAK" and "invalid
  post-only order: order crosses book".
- **Status**: Live.

#### `61c9903` — Upsize rescue qty to venue minimum
- **Symptom**: Stranded qty < venue_min_order_quantity (typically 5
  shares) blocked rescue forever. Stranded 4.99 shares accumulated for
  hours with no rescue attempted.
- **Fix**: `build_rescue_intent_for_quantity` upsizes sweep_qty to
  `max(stranded_qty, venue_min)`. The merge engine pairs MIN(left, right)
  so the residual becomes a tiny new (bounded) stranded position.
- **Status**: Live. Bug class — same as the cap-bypass family.

#### `4727951` — Race buffer for FAK rescue limit price
- **Symptom**: Polymarket returning "no orders found to match with FAK"
  hundreds of times per hour. depth-walk computed sweep_price from local
  book; by the time the FAK reached venue (50-200ms later), best_ask had
  ticked up.
- **Fix**: Pad rescue limit by `hedge_rescue_race_buffer_ticks=3`. Capped
  at $0.99 to maintain net-positive economics (merge releases $1).
- **Status**: Live. v23 had ~hundreds of FAK kills/h; v32+ shows 0.

#### `35be520` — Thread venue MarketMetadata into StrategyContext
- **Symptom**: `venue_min_order_quantity` and `maker_price_tick` were
  config knobs duplicating venue facts. Operators kept env in sync with
  Polymarket changes.
- **Fix**: `Runtime::set_venue_market_rules()` cached per-market.
  StrategyContext.venue_rules read by strategy instead of config.
- **Status**: Live. Architectural improvement — engine now self-syncs.

#### `a0aa4e4` — Use venue tick + bump safety_ticks (later reverted to 2)
- **Symptom**: With safety_ticks=1, paired-bid post-only orders crossed
  book on race condition. ~31 rejections per 6h.
- **Fix initial**: safety_ticks=3 + use venue tick (from cached metadata).
- **Status**: Reverted in `8e26925` to safety_ticks=2 (3 cratered fill rate).

#### `e337be4` — Drift block must bypass rescue intents
- **Symptom**: 998 rescue intents BUILT per hour, ZERO ever appeared at
  venue. Markets accumulated stranded inventory permanently.
- **Root cause**: `accept_intent` drift block rejected all `!reduce_only`
  intents on drifted markets. Rescue is `reduce_only=false` (BUY of
  opposite leg) so it got trapped — the exact exposure the cap meant to
  prevent.
- **Fix**: Drift block exempts rescue intents.
- **Status**: Live. **HIGHEST-IMPACT bug fix of the session** — unblocked
  the entire rescue path.

#### `b7aff28` — Promote IntentKind { Entry, Close } enum
- **Symptom**: 5 separate `quote_level_tag.starts_with("mm-hedge-rescue")`
  string-prefix checks across runtime/risk/reconciler/runner/strategy.
  Each new gate someone added risked forgetting the bypass.
- **Fix**: Added `kind: IntentKind` field on `OrderIntent`. All 5 string
  checks replaced with `intent.kind == IntentKind::Close`.
- **Status**: Live. Architectural cleanup — one source of truth.

#### `ecb327c` — Clear pending_merge_by_market on submit failure
- **Symptom**: When merge submission failed (transient RPC error), the
  runner logged "will retry on next reconcile sweep" but never cleared
  the dedup key. Future merges on that market silently skipped.
- **Fix**: Added `Runtime::clear_pending_merge()` called from all 3
  failure paths (Ok rejected, Err retryable, Err non-retryable).
- **Status**: Live.

#### `1ccef91` — Asymmetric (true,true) inventory rescue
- **Symptom**: Strategy no-op'd on (true, true) inventory state. When 47
  Up + 5 Down stranded, merge consumed only 5 paired and the 42 excess
  Up bled at resolution.
- **Fix**: New branch — if `abs(left - right) >= venue_min`, fire rescue
  for the under-stocked leg.
- **Status**: Live.

#### `b59972d` — Post-fill per-market entry cooldown
- **Symptom**: After successful merge, strategy immediately re-entered same
  market in trending tape → re-stranded.
- **Fix**: Per-market `last_fill_ms` tracking. (false, false) entry branch
  waits `30 × cooldown_ms` (~15s) before re-entering same market.
- **Status**: Live.

---

## Category 2: Strategy improvements (real edge, kept)

#### `139eda6` — Hedge rescue is now IOC ask-lift
- **Why**: Mirrors verified unlawful_shear pattern. Atomic pair completion
  via FAK lifting opposite leg's ask.
- **Status**: Live, foundational for entire rescue path.

#### `1da094e` — Regime gate (skip flat/trending tape)
- **Why**: Skip entries when realized_vol_5m < threshold (no taker flow)
  or |return_60s| > threshold (book moves between bid + fill).
- **Status**: Live. Threshold reverted from 0.1 → 1.0 in conservative
  rollback (`8e26925`).

#### `bb6f418` — Spot momentum tilt on fair_value
- **Why**: fair_value was pure book mid → no directional adjustment as
  BTC moved → adverse-selected on every directional tick. Small bias
  on UP leg's fair when BTC is rising.
- **Status**: Live. Effect is mild by design.

---

## Category 3: Architectural (no behavior change but cleaner)

#### `d1a36cc` — Codify entry-vs-close intent distinction in CLAUDE.md
- **Why**: Cap-bypass was being implemented as ad-hoc string checks in 4
  separate files. Documented as engineering principle so future changes
  recognize the pattern + promote to typed enum at threshold.
- **Status**: Live. Led to `b7aff28` IntentKind refactor.

---

## Category 4: Tunings — REVERTED on 2026-04-27 (caused bleed)

These changes individually had defensible reasoning but stacked together
they reduced defensive surface against trending-market adverse selection.
Cash dropped $98 → $42 (peak-to-cash-trough $56) over ~3h as they
compounded. Reverted in `8e26925`.

#### `df92f76` — 3 quick wins (REVERTED)
- safety_ticks 3 → 2 (kept lower fill cap defense)
- regime threshold 0.3 → 0.1 (let too many entries through in mildly
  trending tape; reverted to 1.0)
- include-prev/next 0 → 1 (3 markets concurrent; reverted to single)

#### `ad05ee2` — Fill-rate-aware clip sizing (REVERTED)
- Scaled clip 1.0 → 2.0× with recent fills.
- **Why bad**: scaled UP both good fill streaks AND adverse fill streaks.
- Reverted to flat 1.0×.

#### `bc6e6e1` — Per-leg adaptive safety_ticks (REVERTED)
- Cheap leg got safety=1 (at-the-bid for queue position 0).
- **Why bad**: combined with extreme-book entries, kept filling us on
  whichever leg was rallying.
- Reverted to uniform safety_ticks.

#### `0c5fbad` — Price extremity gate (REFINED)
- Initial threshold 0.85 was too lax — markets at 0.65-0.85 still mean-revert.
- Tightened to 0.65 in `8e26925`.

---

## Category 5: Documentation

- `2026-04-27-fix-log.md` — Append-only log of fixes implemented
- `2026-04-27-depth-ladder-spec.md` — Spec for #51 (deferred)
- `2026-04-27-day-analysis.md` — Data-driven comparison vs unlawful_shear
- `2026-04-27-shipped-report.md` (this doc)

---

## Category 6: Discoveries (no code, but key insights)

#### Maker rebates ARE active on 5min crypto markets
- Initially thought `clobRewards: []` meant no rebate program. Wrong —
  that's the OLD Q-score program.
- The NEW dynamic-taker-fee redistribution program IS active.
  20% of taker fees airdropped daily to maker_address.
- Endpoint: `clob.polymarket.com/rebates/current?maker_address=...&date=...`
- Our wallet earned **$5.90 across 87 markets on 2026-04-26**.
- Doc: `project_2026-04-27_maker_rebates_real.md`

#### Fill-symmetry is THE structural gap vs unlawful
- We: 0.33 (~3× lopsided fills)
- Unlawful: 0.58 (~2× lopsided)
- Cause: we trade thin clips × many markets × always-rescue
- Reference: `2026-04-27-day-analysis.md`

---

## Open work (prioritized)

### Tier 1 (high impact, do next)
- **#45 Dynamic in-engine market discovery** — eliminates supervisor
  restart cycle. Each restart re-engages drift block + loses in-flight
  state. Foundational for staying on markets longer (the whale pattern).
- **Bigger clips ($5 base instead of $1.10)** — matches whale capital-per-
  market concentration, allows more attempts before risk caps.
- **Asymmetric hold signal** — don't rescue when trend favors stranded
  leg. Current strategy always rescues, capping upside at break-even.

### Tier 2 (structural, after Tier 1 stable)
- **#51 Depth-ladder quoting** — 3-5 price levels per leg. Spec written.
- **#53 Per-market trend gate** — pause entry when this market's mid
  moves >5¢ in 30s. The actual missing defense.

### Tier 3 (defer until data justifies)
- **#52 Bound HashMap growth** — slow leak (days/weeks before issue)
- **#47 Inventory drift edge cases** — mostly mitigated by other fixes
- Re-enable multi-market + adaptive safety after #51 + #53 prove out

---

## Lessons learned

1. **Don't stack changes without per-change observation**. Each tuning
   I shipped on 2026-04-27 morning had defensible reasoning; together
   they were a disaster. Should have shipped one, observed 30+ min,
   then the next.

2. **Distinguish bugs from tunings clearly**. Bug fixes are usually safe
   (they just close a silent failure). Tunings change risk profile and
   need observation. I conflated them.

3. **Use evidence, not theory, for tunings**. The "depth ladder = 3-5×
   fills" claim is theoretical; the per-market trend gate need is
   data-evident. Prioritize data-evident.

4. **Conservative defaults > aggressive defaults**. v34's rollback
   trades fill volume for predictability. Better to crawl forward with
   evidence than to bleed while looking competitive.
