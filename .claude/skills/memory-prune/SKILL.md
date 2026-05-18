---
name: memory-prune
description: Empty (NOT delete) memory entries that the audit flagged as stale or duplicated.
---

# memory-prune

Conservative pruning. Pruning here means **emptying the file body to a
single `[pruned: reason]` line** and removing its `MEMORY.md` index
entry. The file itself stays so the user can see what changed and
restore it if needed.

## Inputs

- Audit report from `memory-audit`.
- Concrete list of slugs / files the caller wants pruned (the agent
  decides — this skill does not auto-select).

## Procedure

For each file to prune:

1. Read it. If it has secrets or anything that looks load-bearing
   (referenced by [[wikilink]] from another live memory), STOP and
   report. Do not prune.
2. Replace the body (everything after the closing `---` of the
   frontmatter) with:
   ```
   [pruned by claude-memory-manager-daemon on YYYY-MM-DD: <reason>]
   ```
3. Update the frontmatter `description:` to `"[pruned] <original>"`.
4. Remove the corresponding line from `MEMORY.md`.

## Hard rules

- Never `rm` a file from this skill. The user runs deletions manually.
- Never prune more than 3 files per tick.
- Never prune a file modified in the last 7 days unless the audit
  flagged it as a strict duplicate of another file.
- Pruning is forbidden when `DRY_RUN=true` — report what you WOULD
  prune instead.

## Output

A list of `(file, action, reason)` tuples — one per attempted prune.
