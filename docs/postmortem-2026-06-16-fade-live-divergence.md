# Postmortem: fade_live lost ~$700 overnight while the validated engine made money

**Date:** 2026-06-16
**Severity:** High (real-money loss on a malfunctioning live trader)
**Status:** Live trading HALTED and safe; root-cause fix in progress (shared-engine rebuild)

---

## 1. Summary

The exo-fade strategy was running live (real money) on Polymarket BTC-5m via the
`fade_live` binary (polymarket-agent, Dublin box) at $20/clip. Overnight
2026-06-15 → 2026-06-16 it **lost ~$700**, while the validated `shadow` engine —
running the *exact* backtested decision logic on the *same* feeds and config on the
*same* box — **made +$211 (live-equivalent at $20 clips)** over the same window.

The loss was not market regime or variance: it was a **live-specific execution
failure**. `fade_live` is a separate reimplementation of the decision engine, and
that reimplementation silently diverged from the validated engine.

Live trading is now halted (process killed, kill-switch set, open position
redeemed). The fix is to retire the reimplementation and run live trading off the
*exact* shared engine.

---

## 2. Timeline (UTC)

- **2026-06-15 ~15:35** — fade_live places its first real-money order ($20 clips, caps $25/$50). Earlier the day it had a feed-teardown bug (book WS tore down at every 5-min rollover, ~35% fill rate) which was fixed (shadow-aligned `BookExit` reconnect), plus a perp klines-bootstrap added (cold perp had caused "no trades").
- **2026-06-15 evening** — surface metrics looked healthy: ~100% fill rate, config verified matching the frozen backtest config field-for-field, fill-price realization ~1.0. Live ~+$100 over a flat/choppy window; shadow ~flat over the same window. Concluded (wrongly) the system was behaving as designed.
- **2026-06-15 ~22:00 → 2026-06-16 ~06:00** — overnight. fade_live's book WS reset ~14× (vs shadow ~0). fade_live accumulated losses.
- **2026-06-16 ~06:00** — user flagged a ~$700 loss. Investigation: shadow made +$211 (live-equiv) over the same hours; fade_live lost ~$700.
- **2026-06-16 06:08** — kill switch set (`touch ~/fade.kill`): disarms entries AND halts the redeem sweep. fade_live process killed shortly after. Open 06:05 position redeemed manually via `redeem_once` (STATE_EXECUTED).

---

## 3. What went wrong (root cause)

`fade_live` shares the *pure decision kernel* with the backtest — `AlphaModel::evaluate`
(the belief) and `pm_alpha::decide_entry` (the entry gate) — BUT it has its **own
`FadeCore`** that ingests and maintains the **inputs** to that kernel: the spot
buffer, the perp buffer, the order-book ladders, and the strike. The validated
`shadow` engine (`pm-app::ShadowCore`) ingests those same inputs with *different*
code. The two input pipelines drifted.

Evidence (live vs shadow-final, identical config, same box, same feeds, 11h window):

| metric | result |
|---|---|
| markets where live & shadow chose the **same side** | 24 |
| markets where they chose the **opposite side** | **37 (61%)** |
| markets live entered that shadow **rejected** | **68 (massive over-entry)** |
| spot/perp belief-feed drops | **0** |
| config match (DecideConfig + AlphaModelConfig) | exact, field-for-field |

So: same config, healthy belief feeds, yet live faded the *wrong* side more often
than the right one and entered far more markets than the engine sanctioned. The
divergence is structural — in the reimplemented input pipeline, not config or feed
outages. Prime suspects: the perp-bootstrap-from-klines biasing the 0.75-weighted
`effective_spot`, and/or the book/strike handling. Contributing factor: ~14 book-WS
resets overnight created stale-book windows; same-side double-clips (rearm +
max_clips=2) amplified each wrong-side loss to −$40.

---

## 4. Why monitoring didn't catch it sooner (the real miss)

Pre-deploy and during the day, the validation checked: fill rate (100%), config
parity on paper (exact), fill-price realization (~1.0), feed health (clean), redeem
correctness (1:1). **All of these looked healthy while the engine was silently
choosing the wrong side.** The one check that would have caught it — **trade-by-trade
DECISION parity against the shadow twin (same markets, same sides)** — was not run
until the P&L blew up.

Lesson: fill-rate + config-on-paper are necessary but **not sufficient**. The thing
that must be proven before scaling a live trader is that its *decisions* match the
validated engine's decisions, market by market.

---

## 5. What was done

**Immediate (containment):**
- Kill switch set (disarm + halt redeem), fade_live process killed.
- Open position redeemed manually via `redeem_once --condition-id … --index-sets 1,2` (executed on-chain).
- Shadow streams left running as the correct, no-risk reference.

**Root fix (in progress) — run live off the EXACT shared engine:**
- **P1 (done):** Exposed `pm-app::shadow` as a library. `cargo check` clean, 28 shadow tests pass (engine behavior unchanged).
- **P2 (done):** Added `run_shadow_with_sink(args, intent_tx)` which emits an `ExecIntent` for each entry decision produced by the **unchanged** `decide()`. The decision path is identical to shadow/backtest; only the venue token is attached. 28 tests still pass.
- **P3 (next):** Extract a lightweight `pm-shadow` crate (pm-app pulls Nautilus + arrow/parquet — too heavy for the lean agent). Build a `shadow_live` consumer in polymarket-exec that drains `ExecIntent`s and submits via the **already-proven** execution adapter + arming + kill-switch + redemption (reused from fade_live/redeem_once, minus the broken engine). Default = shadow/paper (no real orders).
- **P4 (gate):** Run `shadow_live` in paper alongside `shadow` and prove trade-by-trade decision parity for a full session. **No real money until this passes.**
- **P5 (gated):** Re-arm real money only after P4 + explicit go. Caps + kill switch + $20 clips.

---

## 6. Lessons / action items

1. **Never run a parallel reimplementation of a validated engine for live trading.** Share the actual engine. (Live = shadow engine + order placement.)
2. **Gate live scaling on decision-parity**, proven trade-by-trade against the validated twin — not just fill-rate and config-on-paper.
3. The shared-engine architecture makes this class of divergence **structurally impossible**: the live trader cannot decide differently from the backtest because it *is* the same `decide()`.
4. Keep the divergence-detector permanently: a monitor that compares live entries to shadow entries market-by-market and alerts on any side disagreement.
