# Prompt Anchoring Patterns

These are local prompting patterns to carry into paper-eval shards. They
reference `claude-code-ts` because that repo contains the agent/prompt
machinery we have been using as a source of truth for how autonomous
jobs should stay grounded.

Source references:

- `/Users/jackreid/claude-code-ts/src/components/agents/generateAgent.ts`
  - agent prompts should extract core intent, define success criteria,
    include quality controls, include self-verification steps, and align
    with project-specific instructions.
- `/Users/jackreid/claude-code-ts/src/services/toolUseSummary/toolUseSummaryGenerator.ts`
  - tool/eval summaries should be short, concrete, past-tense labels
    focused on the distinctive action, e.g. "Ran failing tests".
- `/Users/jackreid/claude-code-ts/src/components/tasks/RemoteSessionDetailDialog.tsx`
  - the remote task pipeline is explicitly staged as Find, Verify,
    Dedupe. Use that shape for eval shards: find evidence, verify the
    claim against artifacts, then dedupe false positives or repeated
    failures.

For paper-eval shards, this means:

1. Start from the exact job command and generated artifacts.
2. Treat `paper_report.json` and test output as primary evidence.
3. Use `diff.md` only to compare a variant against the chosen baseline.
4. Use `suggestions.json` as a heuristic, not as an automatic promotion.
5. Flag capture-quality problems separately from engine regressions.
6. Propose a fix only when the failure is reproducible and tied to a
   narrow code path.
7. Keep the proposed validation command attached to every fix.
