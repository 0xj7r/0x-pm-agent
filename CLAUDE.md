<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **polymarket-agent** (6909 symbols, 15026 relationships, 300 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

> If any GitNexus tool warns the index is stale, run `npx gitnexus analyze` in terminal first.

## Always Do

- **MUST run impact analysis before editing any symbol.** Before modifying a function, class, or method, run `gitnexus_impact({target: "symbolName", direction: "upstream"})` and report the blast radius (direct callers, affected processes, risk level) to the user.
- **MUST run `gitnexus_detect_changes()` before committing** to verify your changes only affect expected symbols and execution flows.
- **MUST warn the user** if impact analysis returns HIGH or CRITICAL risk before proceeding with edits.
- When exploring unfamiliar code, use `gitnexus_query({query: "concept"})` to find execution flows instead of grepping. It returns process-grouped results ranked by relevance.
- When you need full context on a specific symbol — callers, callees, which execution flows it participates in — use `gitnexus_context({name: "symbolName"})`.

## Never Do

- NEVER edit a function, class, or method without first running `gitnexus_impact` on it.
- NEVER ignore HIGH or CRITICAL risk warnings from impact analysis.
- NEVER rename symbols with find-and-replace — use `gitnexus_rename` which understands the call graph.
- NEVER commit changes without running `gitnexus_detect_changes()` to check affected scope.

## Resources

| Resource | Use for |
|----------|---------|
| `gitnexus://repo/polymarket-agent/context` | Codebase overview, check index freshness |
| `gitnexus://repo/polymarket-agent/clusters` | All functional areas |
| `gitnexus://repo/polymarket-agent/processes` | All execution flows |
| `gitnexus://repo/polymarket-agent/process/{name}` | Step-by-step execution trace |

## CLI

| Task | Read this skill file |
|------|---------------------|
| Understand architecture / "How does X work?" | `.claude/skills/gitnexus/gitnexus-exploring/SKILL.md` |
| Blast radius / "What breaks if I change X?" | `.claude/skills/gitnexus/gitnexus-impact-analysis/SKILL.md` |
| Trace bugs / "Why is X failing?" | `.claude/skills/gitnexus/gitnexus-debugging/SKILL.md` |
| Rename / extract / split / refactor | `.claude/skills/gitnexus/gitnexus-refactoring/SKILL.md` |
| Tools, resources, schema reference | `.claude/skills/gitnexus/gitnexus-guide/SKILL.md` |
| Index, status, clean, wiki CLI commands | `.claude/skills/gitnexus/gitnexus-cli/SKILL.md` |

<!-- gitnexus:end -->

# Engineering principles

## Calibration sources — always reference whale data first

Before tuning ANY strategy knob, check what we know about the operators
who already make this work. Don't tune in a vacuum and don't trust
intuition over observed whale behavior. Concrete sources:

| Source | Use for |
| [BANDAIDS.md](BANDAIDS.md) | Catalog of known anti-patterns to avoid |

## Gate calibration: prefer signals over hardcoded constants

V1 can use a constant for safety; V2 should be SIGNAL-DERIVED:
- Trend persistence → scaled by `btc_regime.realized_vol_5m_bps`
- Bar-relative timing → fraction of `bar_window_ms` (works 5m/15m/etc.)
- Bid count caps → `max_leg_cost / typical_clip` (capital-aware)
- Hit-rate self-feedback where possible

Constants bound the bot; signals optimize it. Plan the V2 in the same
PR's commit message even if you ship V1 first.


## No bandaids — fix the core engine

When a behavior is missing or broken, the fix goes IN THE RUST ENGINE
(`polymarket-exec/src/`), NOT in a bash supervisor, env wrapper, polling
script, cron, or any other shell-level workaround.

If the answer to "why doesn't X work" is "the engine doesn't have a code
path for X yet" — then add the code path to the engine. Do not paper
over it with shell.

**Examples of what NOT to do:**
- "Engine reads markets from env vars at startup only" → DO NOT wrap in a
  bash loop that restarts the bot when markets change. ADD periodic
  context refresh inside `runtime/runner.rs` that subscribes WS to new
  markets dynamically.
- "Engine doesn't know when to stand down in flat tape" → DO NOT add an
  env override or wrapper-side gate. ADD a regime gate in `strategy.rs`.
- "Engine doesn't track in-flight rescues so we over-rescue" → DO NOT
  add a sleep or per-tick throttle in shell. ADD per-market rescue
  tracking in `market_states`.

**Deploy scripts (`scripts/*.sh`) should ONLY:**
- Load env vars / secrets
- Launch the binary

They should NOT contain business logic, retry loops, polling, market
context refresh, or anything that needs to know about strategy/runtime
state. Process supervision via systemd is fine; bash supervision around
the bot's domain logic is not.

**Why this matters:** ad-hoc shell wrappers ship faster but they kill
in-flight engine state every restart, can't share state with the
engine's data structures, hide bugs from the type checker / tests, and
require operators to understand TWO systems instead of one. The Rust
engine is the source of truth.

## Distinguishing entry vs close intents

Trading intents fall into TWO categories with opposite risk profiles:

- **Entry** (paired bids, fresh quotes): ADD exposure. Subject to all
  entry-time caps (max_open_orders, max_position_qty, max_order_notional,
  max_gross_cost, submit rate cap). These exist to prevent accumulation
  runaway and quote-spam.
- **Close** (hedge rescue, reduce-only sells): REMOVE exposure. They
  manufacture or unwind paired inventory so it can be merged for $1
  collateral release. Entry-time caps must NOT apply — blocking a close
  leaves the bot stuck with naked directional exposure that the cap was
  trying to prevent in the first place.

If you find yourself building the third "bypass entry-time cap for
close intents" code path in a separate file, that's a smell. Consider
adding a typed `IntentKind::{Entry, Close}` enum on `OrderIntent` so
ALL downstream gates can branch cleanly on it, instead of every cap
layer doing its own `quote_level_tag.starts_with("mm-hedge-rescue")`
string check. Today's cap-bypass landed in strategy.rs, core/risk.rs,
and market_making/quote_reconciler.rs — three separate places, all
checking the same string. Promote to a type when the third or fourth
layer needs the same logic.
