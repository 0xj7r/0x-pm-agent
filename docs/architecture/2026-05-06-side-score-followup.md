# Side-Score Follow-up Audit — 2026-05-06

Re-review of `polymarket-exec` after the side-score signal engine merge (`74895d5`), with the original 2026-05-05 audit and all in-flight PRs in scope. Two parts:

1. Status of the in-flight PRs against current `main`.
2. Strategy and code-quality concerns introduced or amplified by the side-score work.

---

## Part 1 — In-flight PR status

PRs against `origin/main` (post-merge):

| PR | Branch | Status vs main | Notes |
|---|---|---|---|
| #65 | `docs/rescue-config-caveat` | **MERGED** | Already on main as `ffd04bb`. |
| #55 | `cleanup/cargo-fmt` | trivial conflict | `ladder_builder.rs` whitespace overlaps with new side-score lines. Resolves by re-running `cargo fmt`. |
| #56 | `cleanup/remove-gitnexus` | clean | |
| #57 | `cleanup/clippy-warnings` | substantive conflict | 3 files: `engine.rs`, `ladder_builder.rs`, `strategies/paired_mm.rs`. Needs careful 3-way. |
| #58 | `docs/rust-fundamentals` | clean | |
| #59 | `docs/architecture-audit` | clean | |
| #60 | `refactor/intent-kind-close-checks` (convex pair_id) | clean | **Still relevant** — the new `choose_convex_overlay` on main does not set `pair_id` on the package legs. The atomicity hole identified in Finding 1 is intact. |
| #61 | `fix/ladder-paired-equal-share-aggregation` | substantive conflict | `append_leg_ladder` on main now has the side-score `signal_clip_scale` integration. Needs to merge with my `last_emitted_price` collision break. |
| #62 | `refactor/convex-leg-struct` | substantive conflict | The new code added side-score lookups via `leg_from_tag(favorite_tag)` / `leg_from_tag(tail_tag)` — a string-to-enum round trip that the `ConvexLeg` struct refactor would eliminate. Refactor is **more valuable now**, not less. |
| #63 | `refactor/attribution-substring-purge` | trivial conflict | `ladder_builder.rs` whitespace only. |
| #64 | `fix/aws-config-deprecation` | trivial conflict | `ladder_builder.rs` whitespace only. |

**Recommended merge order to minimise rebase work**:
1. **#56** (remove gitnexus) and **#55** (cargo fmt) first — independent and unblock everyone else.
2. **#58**, **#59** (docs) and **#60** (convex pair_id) — clean and high-value.
3. **#57** (clippy autofix) — substantive but mechanical.
4. **#61** (ladder collide fix) — needs careful rebase on top of side-score's new ladder code.
5. **#62** (ConvexLeg struct) — needs to absorb the new side-score lookups.
6. **#63**, **#64** — once **#55** is in, these become whitespace-clean.

---

## Part 2 — Strategy and code-quality concerns

### Finding 11 — Convex package atomicity hole still open (carry-forward of audit Finding 1)

`paired_mm/engine.rs:603` and `paired_mm/engine.rs:639` still build `favorite_intent` and `tail_intent` without setting `pair_id`. The runtime atomicity guard (`runtime/mod.rs::filter_incomplete_paired_entry_intents`) cannot protect the package. This is exactly what PR #60 fixes; it rebases cleanly onto current main and should land.

### Finding 12 — `leg_from_tag(tag: &str) -> LadderLeg` is an anti-pattern

`paired_mm/engine.rs:729`:

```rust
fn leg_from_tag(tag: &str) -> LadderLeg {
    if tag == "yes" {
        LadderLeg::Yes
    } else {
        LadderLeg::No
    }
}
```

The `favorite_tag = "yes"` came from a branch that already knew `LadderLeg::Yes`. Round-tripping through a string and a fragile equality check (any tag that is not literally `"yes"` becomes `LadderLeg::No`, which masks future typos) is exactly the smell `IntentKind` and `MmQuoteKind` were promoted to type-state for.

**Recommendation:** PR #62's `ConvexLeg` struct should grow a `leg: LadderLeg` field. The `if/else` already routes `LadderLeg::{Yes, No}` to the constructors; nothing else needs to change. `leg_from_tag` can then be deleted.

### Finding 13 — `quote_level_tag` uses Debug format for `MmQuoteKind`

Two sites in `paired_mm/engine.rs`:

```rust
favorite_intent.quote_level_tag = Some(format!(
    "mm-convex-accum:late-favorite:{favorite_tag}:{:?}",
    MmQuoteKind::ConvexAccumulation
));
```

`{:?}` is the `Debug` format. It happens to render `ConvexAccumulation` today, but `Debug` is not a stable contract — Rust's `derive(Debug)` output can change across compiler versions, and a future field addition to the enum changes nothing visible until someone refactors it. The string then ends up downstream in `MmQuoteKind::from_quote_level_tag` (which uses lowercase substring matching) and in attribution dashboards.

**Recommendation:** add a `MmQuoteKind::as_tag_str(self) -> &'static str` method (the bucket strings already exist in `attribution_bucket()` in PR #63), and use that here instead. Same pattern as `CloseMethod::as_str()` in `core/types.rs:140`.

### Finding 14 — Side-score and pressure-bias scales are multiplied without a final clamp

`paired_mm/engine.rs:528`:

```rust
pressure_bias.favorite_scale *= side_score.leg(favorite_leg).late_convex_scale;
pressure_bias.tail_scale *= side_score.leg(tail_leg).late_convex_scale;
```

`pressure_bias.favorite_scale` is clamped to `[0.5, 1.5]` in `convex_pressure_bias`. `late_convex_scale` is bounded by `SideScoreConfig::max_late_convex_tilt` (default `0.80`), which I read as keeping it in roughly `[0.2, 1.8]` (the bound is symmetric around 1.0 by the score sign). The product after multiplication can therefore land outside the original pressure-bias clamp range — for example `1.5 * 1.8 = 2.7` (over-amplifies the favourite when both signals support it) or `0.5 * 0.2 = 0.10` (suppresses the tail to ~10% when both signals dis-support it).

This is operationally important because the convex package then sizes the favourite and tail legs from these scales, and the result feeds into `choose_late_asymmetric_package`'s budget arithmetic. The package planner has its own `min_order_size` and `max_loss_usd` caps, but if either scale clamps right at zero the package can still emit a sub-minimum order that the cap layer then rejects, producing churn rather than a clean "no fire".

**Recommendation:** clamp the product back into a documented range immediately after the multiplications, e.g. `[0.10, 2.50]`. This makes the worst-case effect bounded and gives operators one knob to tune extremity instead of two compounding ones.

### Finding 15 — Side-score reversal-risk and momentum compound through two paths

`SideScoreSignal::compute` consumes `momentum` (directly via `momentum_component`) AND `reversal` (via `reversal_component`). `ReversalSignal` itself is built from momentum and orderflow. So a strong adverse momentum reading flows into the side score twice:

- once via `momentum_weight * momentum_component` (negative for the favourite),
- once via `reversal_risk_weight * reversal_component` (which boosts the underdog and penalises the favourite).

This is not strictly wrong — the configuration weights are designed to be tuned together — but it makes the side-score's response surface harder to reason about: changing `momentum.strength` moves both terms in the favourite's score with no single coefficient that controls overall sensitivity.

**Recommendation:** either (a) drop momentum from `SideScoreSignal::compute` directly and rely on reversal to carry the momentum signal, or (b) document the compounding clearly in the `SideScoreConfig` doc comment so the operator knows that `momentum_weight` and `reversal_risk_weight` interact. Today it is silent.

### Finding 16 — Terminal-timing and late-convex stack on the favourite

`SideScoreSignal::compute`'s `terminal_timing_component` increases for the favourite as `remaining_ms` shrinks toward zero. The late-convex overlay only fires when `remaining_ms <= late_threshold_ms`. So in the late window:

- `terminal_timing_component` is high → side_score on the favourite is high → `late_convex_scale` is > 1 → favourite leg sized larger.
- And the package has already passed the late-window gate.

That is "favourite gets MORE bias because we are LATE, in the late-convex code path that is only reached BECAUSE we are late." Both axes drive the same direction at the same time.

This is likely intentional (the convex package is supposed to load harder near bar-end), but the compounding is undocumented and the same effect is achievable through one knob (`SideScoreConfig::max_late_convex_tilt`). Worth either documenting or normalising so that one of the two layers is the source of truth.

### Finding 17 — Side-score on the paired ladder is partly erased by `normalize_paired_entry_quantities`

The strategy review explicitly flags this as a question to confirm. After analysing the call order (`append_leg_ladder` → `normalize_paired_entry_quantities` → `interleave` → `aggregate_collapsed_ladder_levels`):

- `append_leg_ladder` per leg: clip *= signal_clip_scale (which now includes side_score).
- `normalize_paired_entry_quantities`: `paired_quantity = yes.quantity.min(no.quantity)`.

The `min` step **collapses the larger leg back down to the smaller leg's size**. Net effect: side-score reduces the *smaller* side; the favourite never grows beyond what the underdog allows. The ladder becomes "shrink the underdog further when side-score disagrees with it", which is *not* what the design intends ("Use side score to tilt sizing, not to suppress one side entirely").

**Recommendation:** before normalising, decide whether the per-level pair is "tilt-mode" or "match-mode". Today it's always match-mode. Two viable approaches:

1. Allow per-level tilt by replacing `min` with a target-share allocation that uses both legs' clip targets; if the favourite is genuinely supposed to be larger, emit two intents at separate `pair_id`s but stop pretending they are an atomic level. (This conflicts with PR #60 / Finding 1's atomicity contract — needs design.)
2. Tilt only the *clip target before per-level matching*, e.g. compute a per-pair clip = `(yes_clip + no_clip) / 2 * side_score_pair_factor`, then build both legs from that single shared clip. Side-score steers the pair, not the legs.

(2) keeps pair atomicity intact and gives side-score real effect. Worth a brief design discussion before implementing.

### Finding 18 — `replay/journal.rs` (564 LOC) added but not reviewed

The merge introduced a new replay journaling module that I did not inspect in this pass. Worth a separate read; flag for the next review session.

### Finding 19 — Three signal-config blocks compound in YAML without a "global signal influence" knob

`signals.reversal`, `signals.book_sanity`, and `signals.side_score` each have their own weight surface. Side-score weighs reversal output. Book sanity feeds into side-score. There is no single dial for "make all signals more conservative" — operators have to dial each weight independently and reason about how they cascade.

This is a tuning ergonomics concern, not a bug. Worth considering a top-level `signal_aggressiveness` knob in `SideScoreConfig` that scales the score before computing `ladder_clip_scale` and `late_convex_scale`. One knob to dial during incidents.

---

## Part 3 — Updated PR sequence recommendation for tomorrow

Given the merge, I would re-prioritise:

1. **Land #55, #56, #58, #59** (no logical conflict, low review cost).
2. **Land #60** (convex pair_id atomicity) — still relevant, clean to rebase, directly addresses stranded-inventory bug surface.
3. **Land #57** (clippy autofix) — needs a 3-way rebase but no logical conflict.
4. **Land #61** (ladder collide fix) — needs rebase on top of side-score ladder changes; the test it fixes is still failing on main.
5. **Rework #62** (ConvexLeg struct) — should grow a `leg: LadderLeg` field and absorb the side-score lookup, replacing `leg_from_tag`. Net result is better than my original PR.
6. **Land #63, #64** — once #55 is in.
7. **Apply Findings 11–17** as follow-ups in priority order:
   - Finding 14 (clamp scale product) — small, defensive.
   - Finding 13 (`MmQuoteKind` as_tag_str) — small, removes Debug-format dependency.
   - Finding 17 (paired-ladder tilt erasure) — design conversation needed.
   - Finding 15, 16 (compounding signal paths) — documentation first, then refactor if measured to matter.

---

## Out of scope (still)

- Audit Finding 2 — `InventoryState` vs `AutoFillState` divergence. Bigger behaviour change.
- Audit Finding 3 — stranded-inventory operator alert. Needs state-machine design.
- Audit Finding 5 — single rescue trigger ownership. Deserves simulation regression coverage.
- Audit Finding 6 — `runtime/mod.rs` extraction. Should follow the above.
- The 9 pre-existing test failures listed in `2026-05-05-audit.md` and the `.claude/scratchpad.md` — none introduced by side-score; they are still on main.
