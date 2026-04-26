<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **polymarket-agent** (3668 symbols, 7760 relationships, 300 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

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