---
name: process-janitor
description: Identify and (carefully) terminate stale Claude / Codex / Kiro CLI sessions that have been left running without an active TTY.
---

# process-janitor

Cleanup skill for orphaned `claude`, `claude-cli`, `kiro-cli`, `kiro-cli-chat`,
`codex`, and `codex-cli` processes.

This skill **NEVER calls `kill` directly**. All termination goes through the
compiled `cmmd janitor` subcommand, which enforces safety invariants in Rust
that the agent cannot override:

- Process must match the hardcoded name allowlist.
- Process must be owned by the daemon's uid.
- Daemon's own PID and the daemon's children are skipped automatically.
- PID 1 is skipped.
- Hard ceiling of 20 kills per invocation (you cannot raise it).
- Without `--no-dry-run`, `apply` prints what it would kill and exits.

## When to invoke

- A user explicitly asks "clean up stale claude/codex processes".
- During an idle tick, the daemon detects RSS pressure (free RAM low) AND
  there are processes older than 24 h with effectively zero CPU.
- Never during the user's working hours unless they asked.

## Procedure

1. Run `Bash cmmd janitor list --json --min-age-hours=24 --max-cpu-pct=0.5`.
   Read the JSON. This is your only source of truth — do not enumerate
   processes any other way.
2. If the list is empty, report `nothing to do` and exit.
3. If the list has entries, summarize them (pid, name, age, rss). Group
   by name so the user can see the shape (e.g. "8 stale claude, 3 codex").
4. If the user has authorized cleanup OR the daemon's auto-janitor flag
   is on, run:
   `Bash cmmd janitor apply --no-dry-run --max=5 --min-age-hours=24`
   Otherwise run with NO `--no-dry-run` flag — this is a preview that
   prints `would kill ...` lines and exits without sending signals.
5. Report the outcome counts: attempted / sigterm_ok / sigkill_ok /
   survived_grace.

## Hard rules

- Never raise `--max` above 5 in an automated invocation. If you have a
  bigger backlog, schedule it across multiple ticks.
- Never set `--min-age-hours` below 6. Anything younger is plausibly an
  active user session.
- If you can't tell whether a process is actively serving the user
  (e.g. you only see `claude` and no command-line context), be
  conservative: leave it alone.
- If a SIGKILL was required to remove a process, log loudly. That's a
  symptom of a stuck session and the user may want to investigate.
- This skill never touches `node`, `bun`, `python`, IDE servers, or
  anything outside the allowlist. If the user asks you to kill those,
  refuse and tell them to do it manually — those are out of scope.
