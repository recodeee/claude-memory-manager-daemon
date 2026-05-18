---
name: memory-organize
description: Rewrite MEMORY.md so it stays under 200 lines, is grouped by topic, and matches the files on disk.
---

# memory-organize

Reorganizes `MEMORY.md` only. Never touches individual memory file bodies.

## Inputs

- Audit report from `memory-audit`.

## Why this matters

`MEMORY.md` is loaded into the system prompt every session, but only
the first 200 lines survive truncation. Past 200, entries are
invisible. Keeping it tight is the single highest-leverage thing this
daemon does.

## Procedure

1. Read every memory file's frontmatter (the audit already has this
   loaded — reuse it if possible).
2. Group entries by `metadata.type`: user → feedback → project →
   reference.
3. Inside each group, sort by `name` ascending.
4. For each entry, write one line:
   ```
   - [<description first clause>](<filename>) — <description rest>
   ```
   Cap each line at ~150 characters.
5. Add a single-line section heading per group:
   ```
   ## User
   ...
   ## Feedback
   ...
   ## Project
   ...
   ## Reference
   ...
   ```
6. Confirm the result is ≤ 200 lines. If not, shorten hooks further or
   flag entries as candidates for `memory-prune` in the next tick.
7. Write the new `MEMORY.md`.

## Hard rules

- `MEMORY.md` has NO frontmatter. Do not add one.
- Forbidden when `DRY_RUN=true` — emit the proposed file content to the
  log instead of writing.
- Preserve any line that starts with `<!--` (HTML comment) as a user
  marker — leave it where it is.
- Atomic write: build the full new content in memory, write once.
