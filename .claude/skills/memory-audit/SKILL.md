---
name: memory-audit
description: Retired. The daemon now runs a deterministic Rust-side audit before invoking the agent and attaches the report to the tick prompt.
---

# memory-audit (retired)

This skill used to ask the agent to inventory `$MEMORY_ROOT`, parse YAML
frontmatter, and emit a JSON audit. That work now happens in compiled
Rust (`src/audit.rs`) before each tick. The report is pre-attached to
the agent's prompt as JSON, so re-running the audit from inside the
agent is pure duplication — wasting turns and tokens.

If the daemon ever stops pre-attaching the audit (e.g. you're running
this skill from outside the daemon), fall back to:

1. Read `MEMORY.md`. Count its lines. Flag if > 200.
2. `Glob $MEMORY_ROOT/*.md`. For each file, check the frontmatter has
   `name`, `description`, and `metadata.type ∈ {user, feedback, project,
   reference}`.
3. Cross-reference `MEMORY.md` lines (`[title](filename.md)`) with the
   file list — flag dangling entries (line points at missing file) and
   missing entries (file exists but not in index).
4. For feedback/project entries, check the body has `**Why:**` and
   `**How to apply:**` lines.

That's it — keep the skill minimal. The daemon's Rust audit does
everything above plus duplicate-description detection and `[[wikilink]]`
integrity checking, none of which the agent needs to redo.
