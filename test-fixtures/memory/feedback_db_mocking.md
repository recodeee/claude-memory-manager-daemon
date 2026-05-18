---
name: feedback-db-mocking
description: Integration tests must hit a real database, not mocks.
metadata:
  type: feedback
---

Integration tests must hit a real database, not mocks.

**Why:** A prior production migration broke because mocked tests passed while
the real schema-change path failed under concurrent writes. The team only
trusts integration coverage that exercises the actual database.

**How to apply:** When generating new integration tests, prefer testcontainers
or a real Postgres instance. Refuse to substitute a mock unless the user
explicitly asks for one and acknowledges the risk.
