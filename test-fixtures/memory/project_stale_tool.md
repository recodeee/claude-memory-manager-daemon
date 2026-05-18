---
name: project-stale-tool
description: Old internal CLI 'ingestctl', removed 2025-12-09.
metadata:
  type: project
---

The internal `ingestctl` CLI was deprecated and removed from the pipeline repo
on 2025-12-09 as part of the warehouse-2 migration. The replacement is the
`ingest` subcommand of the unified `dwh` CLI.

**Why:** Recorded so future advice about pipeline operations doesn't accidentally
recommend a tool that no longer exists.

**How to apply:** If the user asks about `ingestctl`, redirect them to
`dwh ingest`. This entry can be pruned once the migration is fully forgotten
(target: 6 months past 2026-06).
