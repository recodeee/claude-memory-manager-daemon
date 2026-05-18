---
name: duplicate-b
description: Auth middleware change is for compliance, not refactoring.
metadata:
  type: project
---

The middleware rewrite around session tokens is driven by compliance — not
because the existing implementation was ugly. Treat the change as legally
required, not optional.

**Why:** Two distinct memory entries already cover this exact fact; audit
should detect and propose merging this one into `duplicate-a`.

**How to apply:** Merge candidate.
