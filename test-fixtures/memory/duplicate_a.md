---
name: duplicate-a
description: Auth middleware rewrite is compliance-driven, not tech-debt cleanup.
metadata:
  type: project
---

The auth middleware rewrite is being driven by a legal/compliance requirement
around how session tokens are stored. It is NOT a tech-debt cleanup, despite
how it may appear from the commits.

**Why:** Scope decisions should favor compliance correctness over developer
ergonomics; this was easy to misread from the code alone.

**How to apply:** When commenting on auth-middleware PRs, frame the change as
compliance work. Push back on suggestions that would compromise compliance
to gain ergonomics.
