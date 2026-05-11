# Polymarket `btc_5m_mm` — implementation and debugging guide

Last updated: 2026-04-29

This doc describes how `polymarket-exec` SHOULD behave on `btc-updown-5m-*`
markets, the correct invariants for each subsystem, and the bug patterns
we have hit (and any future agent should look for first when something
seems wrong).

---

## 1. The strategy in one paragraph

We are a **paired-bid market maker** on Polymarket binary markets.
Every 5-minute BTC bar produces a market with two outcome tokens (the
YES instrument and NO instrument). We post **resting limit buy orders
on BOTH outcome tokens simultaneously**, at prices that sum to less
than $1.00. When a taker hits one of our bids we earn a maker rebate;
when the bar resolves we redeem the winning leg for $1 (or merge the
paired leg pair for $1 collateral release). Edge is the gap between
what we paid for the pair and the $1 we recover.

This is real market making *in the binary-market sense*: bidding NO
at $p$ is economically equivalent to asking YES at $1-p$, so a paired
bid is the canonical two-sided quote on a binary market. Polymarket
recognizes this and pays maker rebates for it.

We are **NOT** a directional trader. We do not predict the BTC move.
Our edge comes from spread capture and rebates over many fills.

---

## 1.5 The four entry/exit paths

The strategy has four code paths that emit OrderIntents. Each fires under
different conditions, has a different TIF, and has a different intent
kind. When debugging, identify which path is (or is not) firing first —
the answer to "why isn't the bot quoting" depends on which path you
expect.

### Path 1 — Paired bid (`mm-paired-bid`) — primary MM, ~95% of edge

**Function:** `build_paired_entry_ladder` (`strategy.rs:3433`).
**Tag:** `mm-paired-bid:lN` where N is the level index + 1.
**TIF:** GTD with `live_order_ttl_ms` TTL (default 20s), post_only=true.
**Intent kind:** Entry.
**Fires when:** market not in `ManagingInventory` (no stranded
inventory). Emits up to `entry_ladder_levels × 2` intents per
on_market_snapshot tick (8 levels × YES leg + NO leg = 16 intents).
**Edge source:** maker rebates + (1 - sum_of_paired_bid_prices) gap.

This is the rebate workhorse. If this isn't firing, the bot is dormant.

### Path 2 — Convex accumulation (`mm-convex-accum`) — cheap-leg side bet, ~3.5% of whale notional

**Function:** `build_convex_accumulation_intent` (`strategy.rs:3083`).
**Tag:** `mm-convex-accum:l1`.
**TIF:** GTD, post_only=true.
**Intent kind:** Entry.
**Fires when:** the cheap leg (price < `CONVEX_ACCUMULATION_MAX_BID = 0.45`)
shows trend persistence in the OPPOSITE direction (`CONVEX_TREND_PERSISTENCE_BPS = 50`)
within bar phase 60-240s. Emits ONE intent per tick, capped at
`CONVEX_MAX_BIDS_PER_BAR = 4` per market per bar.
**Edge source:** asymmetric payoff — pays $1 if the cheap leg wins
(low probability, high payoff). Bankroll-bounded by
`CONVEX_FRACTIONAL_KELLY = 0.25`.

Single-intent emission by design — this is a lottery ticket, not a
ladder. Whale data shows this is 3.5% of notional, mostly opportunistic.

### Path 3 — Late-bar core (`mm-late-bar-core`) — favored-leg late accumulation

**Function:** `build_late_bar_core_intent` (`strategy.rs:3007`).
**Tag:** `mm-late-bar-core:l1`.
**TIF:** GTD with `LATE_BAR_CORE_TTL_MS = 60s`, post_only=true.
**Intent kind:** Entry.
**Fires when:** all of:
- Time remaining in bar ∈ [30s, 120s]
- Expensive leg book ask ∈ [`LATE_BAR_CORE_PRICE_FLOOR = 0.85`,
  `LATE_BAR_CORE_PRICE_CEILING = 0.98`]
- `realized_vol_5m_bps >= LATE_BAR_CORE_MIN_VOL_BPS = 50`
- BTC momentum confirms direction
- < `LATE_BAR_CORE_MAX_BIDS_PER_BAR = 15` already filled this bar
- < `LATE_BAR_CORE_BUDGET_USD = $5` already spent

Emits ONE intent per tick (single-shot, queued to repeat on subsequent
ticks until the budget or count is hit).
**Edge source:** high-probability, low-margin late-bar convergence —
buy at $0.93, win $1.00 = $0.07/share with ~93% confidence.

This is the "expensive leg accumulation" path you remember. Spec is at
`docs/strategy/asymmetric_core_hedge_spec.md`.

### Path 4 — Hedge rescue (`mm-hedge-rescue`) — close-side, IOC

**Function:** rescue branch at `strategy.rs:3360+`.
**Tag:** `mm-hedge-rescue`.
**TIF:** **IOC**, post_only=**false**.
**Intent kind:** **Close** — bypasses entry-time caps.
**Fires when:** stranded inventory detected on a market and
`decide_stranded_exposure` chooses RESCUE over HOLD. Lifts the
opposite leg's ask + `hedge_rescue_race_buffer_ticks` to manufacture
paired inventory for a merge.
**Edge source:** locks in $1 - avg_cost - rescue_cost - taker_fee - merge_gas
when better than holding to resolution.

This is a TAKER action — pays venue fee, doesn't earn rebate. Used
sparingly. Most stranded positions are HELD (positive-asymmetry hold
beats rescue when fair has moved in our favor).

### Decision tree per tick

```
on_market_snapshot:
  ├─ has_inventory?
  │    YES → ManagingInventory
  │      └─ stranded leg detected?
  │           ├─ rescue_ev > hold_ev → emit hedge_rescue (Path 4)
  │           └─ otherwise → hold, no intent emitted
  │    NO  → Ready
  │      ├─ Path 1: build_paired_entry_ladder (always tries)
  │      ├─ Path 2: build_convex_accumulation_intent (if cheap-leg gates pass)
  │      └─ Path 3: build_late_bar_core_intent (if late-bar gates pass)
```

Path 1 and Paths 2/3 can fire on the SAME tick (they're not exclusive).
Path 4 only fires when there's existing stranded inventory.

---

## 2. Lifecycle of a single paired bid

```
strategy.on_market_snapshot()
   └─ build_paired_entry_ladder(8 levels, 2 legs each → up to 16 OrderIntent)
        ↓
runtime.accept_strategy_decision(decision.intents)
   └─ DesiredQuoteSet::from_intents (passes pre-laddered intents through)
   └─ QuoteReconciler::plan vs open_orders → Submit / Replace / Keep / Cancel
        ↓
runtime.accept_intent(intent)
   ├─ has_active_btc_mm_buy_for_instrument check (matches market+inst+level_tag)
   ├─ duplicate client_order_id check
   ├─ drift block (skipped for Close intents)
   └─ RiskEngine::evaluate (max_open_orders, max_position_qty, max_leg_cost)
        ↓
execution_adapter.submit_order
   └─ Polymarket V2 SDK → POST /order with TIF=GTD, post_only=true
        ↓
order acks Working → sits on the book at our limit price
        ↓
either:
  (a) taker hits us → fill received as Maker → maker rebate
  (b) book moves, our quote stale → reconciler Replace next tick
  (c) TTL expires (20s) → venue auto-cancels → strategy reposts next tick
        ↓
both legs of pair fill → paired_inventory detected → MERGE planned
   └─ CTF.merge tx → $1 collateral released (USDC.e) → auto-wrap to pUSD
```

Each numbered step has a known-correct contract. If you suspect a bug,
work through this list top-to-bottom and check the contract at each
boundary.

---

## 3. Required invariants by subsystem

### Strategy (`polymarket-exec/src/strategy.rs`)

- `build_paired_entry_ladder` returns up to `entry_ladder_levels × 2`
  intents per call. Each level has a unique `quote_level_tag` of the form
  `mm-paired-bid:lN`. Prices are tick-aligned (multiples of $0.01) by
  `floor_to_tick`.
- The only HARD bid cap is `entry_premium_bid_cap` (default $0.97,
  env-tunable). Any other suppression must be signal-derived per the
  V2 signals spec, not a constant.
- `compute_market_mode` returns either `ManagingInventory` (if we hold
  inventory) or `Ready`. There are NO regime/cooling/cooldown gates —
  these were removed in the 2026-04-29 cleanup because whale data showed
  none of them.
- Hedge rescue (`mm-hedge-rescue` tag) emits Close intents (`IntentKind::Close`)
  with `TIF=IOC, post_only=false`. Everything else is Entry.

### Quote engine (`polymarket-exec/src/market_making/quote_engine.rs`)

- `DesiredQuoteSet::from_intents` MUST detect pre-laddered intents and
  pass them through verbatim — no `take(N)` truncation, no `skew_for_side`
  price mutation. The detection criterion is `bucket.len() > 1 ||
  level_tag.contains(":l")`.
- `max_levels_per_side` clamp must be ≥ `entry_ladder_levels`. Default 16.
- For legacy single-intent emissions (no per-level tag), the legacy fan-out
  with skew is preserved.

### Reconciler (`polymarket-exec/src/market_making/quote_reconciler.rs`)

- `QuoteMatchKey` includes `level_tag`. Distinct ladder levels are
  distinct keys.
- Submit/replace/cancel rate caps come from env (`PM_BTC_5M_QUOTE_*`).
  Not in code defaults.
- Close intents (hedge rescue) bypass the submit rate cap.

### Runtime (`polymarket-exec/src/runtime/mod.rs`)

- `has_active_btc_mm_buy_for_instrument` matches by
  `(market, instrument, level_tag)`. Without `level_tag` the filter
  collapses ladders to the first level — see Bug #6 below.
- All entry-time caps (max_open_orders, max_leg_cost, drift block)
  must check `intent.kind` and skip Close intents.
- `submit_rejection_counts_against_live_budget` excludes benign reasons
  ("post-only", "crosses book", "would cross", "would take liquidity")
  in BOTH the Ok-ack and Err paths. Without this, a single transient
  book cross trips the live kill switch.

### Execution adapter (`polymarket-exec/src/wire/execution_adapter.rs`)

- Round `quantity` to 2 decimal places at the wire boundary (V2 SDK
  rejects 15-decimal precision).
- Treat `400 "invalid post-only order: order crosses book"` as a benign
  rejection.

### Hedge rescue and merge

- Stranded inventory is detected from venue position reconciliation
  (`venue reconciliation found stranded inventory`).
- `decide_stranded_exposure` chooses HOLD vs RESCUE by EV. Hold when
  `hold_ev > rescue_ev + HOLD_EV_MARGIN`.
- Rescue is IOC FAK at the opposite leg's ask + race buffer ticks.
- After both legs paired, merge plan fires with `min_notional ≥ $2.00`
  to amortize gas.

---

## 4. Configuration sources of truth

| Concept | Where it's defined | Override mechanism |
|---|---|---|
| Ladder depth | `polymarket-exec/config/strategies/btc_5m_paired_mm.live.yaml` | YAML profile |
| Ladder spacing | `polymarket-exec/config/strategies/btc_5m_paired_mm.live.yaml` | YAML profile |
| Per-leg bid cap | `polymarket-exec/config/strategies/btc_5m_paired_mm.live.yaml` | YAML profile |
| Capital caps | Strategy YAML + runtime risk caps | YAML / `PM_BTC_5M_EXEC_*` |
| Risk caps | `PM_BTC_5M_EXEC_MAX_OPEN_ORDERS_*` env | env |
| TTL | `PM_BTC_5M_LIVE_ORDER_TTL_MS` env | env, default 20s |
| Reconciler rate caps | `PM_BTC_5M_QUOTE_MAX_*` env | env |
| Maker rebate behavior | venue-side; we just maintain `post_only=true` | n/a |
| Suppression / cooling gates | **REMOVED** (2026-04-29) | n/a — should be signal-derived |

Live env file lives at `~/.config/polymarket-exec/btc_5m_mm_tinylive.env`
on the AWS host. Repo `polymarket-exec/env/btc_5m_mm_tinylive.env` is a
template; the deployed version may diverge.

**Single rule:** if a tunable doesn't appear in the env file, the code's
default applies. There should be NO third source (no JSON profile gets
wired for `btc_5m_mm` — see Bug #2 below).

---

## 5. Verification checklist (before claiming the bot works)

Run these in order. If any fails, do not claim "deployed and working":

1. **Service alive:** `systemctl --user status polymarket-exec@btc_5m_mm_tinylive`
   should show `Active: active (running)`.

2. **No suppression gates firing:**
   `journalctl ... | grep -E 'cooling|suppressed' | tail -20` should be
   empty (post-2026-04-29 cleanup). If you see "cooling" entries, a gate
   has been re-introduced — find and remove.

3. **Ladder reaches the venue at all 8 levels:**
   `journalctl ... | grep -oE 'mm-paired-bid:l[0-9]+' | sort | uniq -c`
   should show non-zero counts for `l1` through `l8` (assuming env
   `LEVELS=8`). If only `l1` appears, suspect:
   - `from_intents` `clamp(1, 3)` — bug #4
   - `has_active_btc_mm_buy_for_instrument` not matching by level_tag — bug #6
   - `skew_for_side` pushing l2+ off-tick — bug #5

4. **Maker fill ratio:** `python3 scripts/order_audit.py` should show
   the vast majority of paired-bid fills tagged Maker. Taker fills on
   `mm-paired-bid` indicate the post_only flag is broken or being
   stripped at the SDK boundary.

5. **Rebate inflow:** check `data-api.polymarket.com/rebates/current`
   for the wallet — it should show non-zero earned rebates if we are
   maker-quoting actively. Yesterday's measurement: ~$5.90/day on $50
   starting capital.

6. **Stranded inventory rescue path is hot but not fired needlessly:**
   `journalctl ... | grep 'hedge rescue branch entered'` should show
   logs only when `paired_quantity=0 stranded_legs > 0`. If it's
   firing while `paired_quantity > 0`, the inventory detection is
   broken.

7. **Merges happen on paired inventory:** look for
   `merge intent planned` events. If you see paired inventory sitting
   for > 30s without a merge, the merge gate is broken or gas-cost
   penalty has gone wrong.

---

## 6. Known bug patterns and how to recognize them

### Bug 1 — Live kill switch trips on benign post-only rejection

**Symptom:** logs show `live execution error budget exhausted
submit_errors=1` after a single submit failure, then the bot starts
cancelling orders and stops submitting. Often after an entry like
`"invalid post-only order: order crosses book"`.

**Root cause:** `submit_rejection_counts_against_live_budget` in
`runner.rs` was only consulted on the Ok-ack path, not the Err path.
A 400 from the SDK comes back as `Err(ExecutionError)` and was
incrementing the counter unconditionally.

**Fix:** classify the Err message through the same exclusion list before
incrementing. Patched on 2026-04-29.

### Historical Bug 2 — Profile JSON had no effect on old `btc_5m_mm`

**Symptom:** operator sets `quote.levels_per_side: 8` in profile JSON,
ladder still emits 3 levels.

**Root cause:** the old `btc_5m_mm` selector ignored the profile and called
env-only config. The current active selectors are `paired_mm`, `pair_cost_arb`,
and `hybrid`, all driven by YAML strategy profiles.

**Detection:** `grep -n 'StrategyMode::try_from_name\|btc_5m_mm.*profile'` —
if no profile field flows into `Btc5mMmConfig`, none will take effect.

**Fix path:** PR1 in the audit (`docs/strategy/audit_2026-04-29.md`) —
add `Btc5mMmProfile` sub-struct and wire it.

### Bug 3 — Quote engine clamps ladder to 3 levels (clamp(1, 3))

**Symptom:** strategy emits 8 sized levels (verified in `strategy.sizing`
logs), only top-3 priced ones reach the reconciler.

**Root cause:** `DesiredQuoteSet::from_intents` had a hardcoded
`max_levels.clamp(1, 3)`.

**Fix:** clamped to 1..32 + pre-laddered detection, 2026-04-29.

### Bug 4 — `skew_for_side` pushes ladder prices off-tick

**Symptom:** strategy emits prices 0.43, 0.42, 0.41 (tick-aligned);
venue rejects 67% of l2/l3 orders silently.

**Root cause:** `from_intents` runs `skew_for_side(price, level)` which
multiplies by `(1 - level × skew_bps/10000)` → 0.42 becomes 0.4196850
which is not on the 1c tick grid. Polymarket V2 silently drops
off-tick prices.

**Fix:** skip skew for pre-laddered intents (detected by `bucket.len() > 1`
or `level_tag.contains(":l")`). 2026-04-29.

### Bug 5 — Duplicate-buy filter blocks ladder beyond level 0

**Symptom:** ladder reaches venue but only `:l1:` ever sees `Working`.
`l2`-`l8` rejected as "duplicate active btc buy intent". This was the
2026-04-29 mid-day surprise — the fix to bug 4 made `intents_in=16` but
plan size stayed at 1 because of THIS filter.

**Root cause:** `has_active_btc_mm_buy_for_instrument` matched by
`(market, instrument, side)`. Once `l1` was active, every later level
on the same instrument was rejected.

**Fix:** include `quote_level_tag` in the match. 2026-04-29.

### Bug 6 — Cooling/suppression gates compound

**Symptom:** bot looks dormant on volatile bars; `journalctl` is full
of `state="cooling" reason="market mid moved 0.10"` etc. for every
tick.

**Root cause:** five separate gates stack:
- post-fill cooldown
- mid-trend movement
- premium fair cap
- asymmetric-fill cooldown
- regime-trending suppression

ALL were hardcoded constants. Any one firing → no paired entry.

**Fix:** removed all of them in the 2026-04-29 cleanup. The signals
spec describes the V2 signal-derived replacements (order flow imbalance,
bar-phase pacing, vol-scaled thresholds).

### Bug 7 — Order TTL too short or too long

**Symptom:** if too short, you see lots of "order expired" + reposts
(reconciler churn). If too long, stale quotes get filled at
disadvantageous prices when the book moves.

**Tunable:** `PM_BTC_5M_LIVE_ORDER_TTL_MS`. Current default 20s.
Whale's empirical bid persistence appears similar — leave at 20s
unless you see a specific symptom.

### Bug 8 — Stranded inventory naked because rescue dropped

**Symptom:** `hedge rescue branch entered ... intent_built=false` for
many ticks while a stranded leg sits exposed.

**Root cause history:** four separate places stripped Close-kind
intents when they shouldn't have:
1. Drift block in `accept_intent`
2. Risk engine `max_open_orders`
3. `enforce_unlawful_mode` cancel filter
4. Submit rate cap in reconciler

**Fix:** all four now check `intent.kind == IntentKind::Close` and
bypass. If you add a fifth gate that touches order intents, do this too.

---

## 7. Useful one-liners

```bash
# How many ladder levels are actually firing?
journalctl --user -u polymarket-exec@btc_5m_mm_tinylive.service --since '15 minutes ago' --no-pager \
  | grep -oE 'mm-paired-bid:l[0-9]+' | sort | uniq -c

# Audit submit→fill→reject lifecycle
python3 scripts/order_audit.py --since 3600

# What's the bot decision per tick?
journalctl ... | grep 'ladder pipeline counts' | tail

# Why was paired entry suppressed (should be empty post-2026-04-29)?
journalctl ... | grep -E 'suppressed by market state|cooling|asymmetric'

# Did our submit rejection trip the kill switch?
journalctl ... | grep 'live execution error budget exhausted'

# Maker vs taker fill mix
journalctl ... | grep 'liquidity=' | grep -oE 'liquidity=\w+' | sort | uniq -c

# Wallet rebate balance
curl -s "https://data-api.polymarket.com/rebates/current?user=$WALLET" | jq

# Live env file (source of truth for the deployment)
ssh ubuntu@$AWS_LIVE_HOST 'cat ~/.config/polymarket-exec/btc_5m_mm_tinylive.env'
```

---

## 8. When to escalate vs when to fix yourself

**Fix yourself:**
- A constant in `strategy.rs` is gating behavior whale exhibits — promote
  to env or remove (follow `docs/strategy/audit_2026-04-29.md` PR list).
- Logs show a known bug pattern from §6 — apply the listed fix.
- Reconciler churn is tight — bump `PM_BTC_5M_QUOTE_MAX_SUBMIT_PER_WINDOW`.
- TTL too aggressive/loose — bump `PM_BTC_5M_LIVE_ORDER_TTL_MS`.

**Discuss with operator first:**
- Touching `inventory.rs` (paired ledger) — a bug here causes
  incorrect P&L attribution.
- Touching `core/risk.rs` — gates can fire on legitimate intents.
- Adding a new suppression gate — these have a strong pattern of
  causing the bot to look dormant for hours; require signal grounding
  per CLAUDE.md "Gate calibration".
- Changing `IntentKind::{Entry, Close}` semantics — this is the typed
  invariant that prevents the rescue-trapping bugs.

---

## 9. Quick mental model: what does "winning" look like?

A normal day in the life:
- Bot quotes both legs of every active 5m bar at $0.45 / $0.55 (or
  wherever the book sits)
- A taker hits one leg → we have one-sided inventory for ~10 seconds
- The other leg fills via natural taker flow → paired inventory
- Auto-merge releases $1 collateral → +$0.0X realized + maker rebate
- 50-100x per day → compounds

If the bot sits idle with 0 fills for 30+ minutes during active hours,
something is gating entry. Start with §5 verification checklist.
