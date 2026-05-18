---
name: memory-audit
description: Inventory and validate the file-based memory lane. Use as the first step of every daemon tick.
---

# memory-audit

Read-only audit. Produces a structured report; never mutates files.

## Inputs

- `$MEMORY_ROOT` — directory containing `MEMORY.md` and the memory files.

## Procedure

1. `Glob $MEMORY_ROOT/*.md` — collect all memory files.
2. Read `MEMORY.md` (if present). Parse each non-blank, non-heading
   line. Capture:
   - linked filename
   - title text
   - hook text after the em-dash
3. For each memory file:
   - Parse YAML frontmatter. Capture `name`, `description`,
     `metadata.type`.
   - Scan body for `[[link]]` references.
4. Build the report.

## Report shape

Emit this JSON in a fenced block at the end of your response so the
caller can parse it:

```json
{
  "files_total": 0,
  "memory_md_lines": 0,
  "memory_md_oversize": false,
  "missing_frontmatter": ["filename.md"],
  "invalid_type": [{"file": "x.md", "type": "wrong"}],
  "dangling_index_entries": ["entry from MEMORY.md pointing to missing file"],
  "missing_from_index": ["file.md exists but no MEMORY.md entry"],
  "broken_wikilinks": [{"from": "a.md", "to_slug": "missing-name"}],
  "duplicate_candidates": [["a.md", "b.md", "reason"]],
  "missing_why_or_how": ["feedback/project file lacking the structured lines"]
}
```

## Limits

- Do not read files outside `$MEMORY_ROOT`.
- Do not propose specific edits in this skill — that's `memory-organize`
  and `memory-prune`. The audit's only job is the report.
- Soft cap: 100 files. Above that, sample and note that the audit was
  partial.
