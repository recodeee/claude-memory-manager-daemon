<p align="left">
  <img src="assets/logo.svg" alt="claude-memory-manager-daemon" width="120" height="120">
</p>

# claude-memory-manager-daemon

A long-running daemon that tends the **Claude Code file-based memory lane**
(`~/.claude/projects/<slug>/memory/`) on a loop. It audits memory files,
prunes stale entries, reorganizes `MEMORY.md`, surfaces authmux login
state, optionally cleans up stale Claude / Codex / Kiro CLI sessions, and
keeps a per-tick audit + undo trail.

It is **not** a system-RAM tool. The only "memory" it manages is the
markdown-file memory store that Claude Code reads at session start.

## What it does, in one tick

```
  ┌─────────────────────────────────────────────────────────────────┐
  │ Every TICK_INTERVAL_SEC, for each MEMORY_ROOT:                  │
  │                                                                 │
  │   1. authmux snapshot      → who is logged in right now         │
  │   2. lsof MEMORY_ROOT      → if anyone holds files open, skip   │
  │   3. min-idle check        → was memory just touched? skip      │
  │   4. Rust audit (free)     → if 0 issues, skip; no claude call  │
  │   5. git commit snapshot   → captures pre-tick state for undo   │
  │   6. spawn `claude -p`     → agent applies up to 3 fixes        │
  │   7. write history record  → JSONL + Prometheus counters        │
  │                                                                 │
  └─────────────────────────────────────────────────────────────────┘
```

Most ticks short-circuit at step 4 (audit clean) or step 2 (someone's
working in memory) — so the loop is nearly free when nothing's wrong.
The agent only gets called when there's actual work to do.

## Two binaries

| Binary  | Path                  | Role |
| ------- | --------------------- | ---- |
| `cmmd`  | `target/release/cmmd` | the daemon + the audit/janitor/history/restore subcommands |
| `mmctl` | `target/release/mmctl`| companion CLI talking to the running daemon over a Unix socket |

## What's tended

`cmmd` checks each memory file for:

- valid YAML frontmatter (`name`, `description`, `metadata.type ∈
  {user, feedback, project, reference}`)
- presence of **Why:** / **How to apply:** lines on feedback and
  project entries
- intact `[[wikilinks]]` to known `name:` slugs
- non-dangling `MEMORY.md` index entries (each line points to a
  file that exists)
- coverage in `MEMORY.md` (each file appears in the index)
- `MEMORY.md` under 200 lines (Claude truncates beyond that)
- duplicate descriptions (likely the same fact recorded twice)

When the audit finds issues, the agent fixes the mechanical ones
(broken indexing, missing structure lines) and proposes merges for
the judgment calls (duplicates, prunes).

## Authmux integration

`cmmd` shells out to the local [`authmux`](https://www.npmjs.com/package/authmux)
CLI every tick. The current account, every managed account, and per-row
5h / weekly usage % are recorded in the daemon state. You can read them
back any time:

```
mmctl accounts
# → JSON with current=..., accounts=[{email, kind, five_h_pct, weekly_pct, active}, ...]
```

Switch accounts without leaving the CLI:

```
mmctl accounts --switch odin@mite.hu
```

The daemon also walks `~/.claude-accounts/account*/` and reports which
directories have a `.credentials.json`.

## Process janitor

A separate subcommand that lists or terminates stale Claude / Codex /
Kiro CLI sessions. Allowlist is **hardcoded in Rust**:

```
claude  claude-cli  kiro-cli  kiro-cli-chat  codex  codex-cli
```

Safety invariants (cannot be overridden by env or flags):

- process owner must equal the daemon's uid
- daemon's own pid and direct children are skipped
- pid ≤ 1 is skipped
- hard ceiling of 20 kills per invocation
- `--require-no-tty` (default true): only kill orphans
- SIGTERM, wait 10 s, then SIGKILL

```
cmmd janitor list                          # default: age ≥ 6h, cpu ≤ 0.5%
cmmd janitor list --min-age-hours=2 --json
cmmd janitor apply --max=5                 # preview only (dry run by default)
cmmd janitor apply --no-dry-run --max=5    # actually kill
```

## Undo

Before each mutating tick, `cmmd` makes sure `MEMORY_ROOT` is a git
repo, then commits its current state with a `[cmmd] pre-tick snapshot at
unix=<ts>` message. That makes every agent edit reversible.

```
mmctl git-log -n 10           # what cmmd has snapshotted
mmctl diff <sha>              # what would change if I restored to this sha
mmctl restore <sha>           # actually do the restore (destructive)
```

Commits use a synthetic identity (`cmmd@local /
claude-memory-manager-daemon`) so they don't pollute your normal
author history.

## History

Every tick — skipped or run — appends a row to a JSONL log at
`$HISTORY_FILE` (default `/tmp/cmmd-history.jsonl`).

```
mmctl history -n 20
mmctl history --json | jq '. | length'
```

A record looks like:

```json
{
  "tick_id": "6a0af6fa28a000ea",
  "started_at_unix": 1779103449,
  "finished_at_unix": 1779103482,
  "dry_run": false,
  "memory_root": "/tmp/cmmd-test-memory",
  "ran": true,
  "reason_skipped": null,
  "exit_code": 0,
  "audit_total_issues": 2,
  "pre_tick_sha": "ee948351107b89a860b2b8e2781ad778e3565f20"
}
```

When a tick spawns claude, the full streamed agent output is also
saved to `/tmp/cmmd-tick-<tick_id>.log` so any decision the agent
made can be audited after the fact:

```
mmctl tick-log <tick_id>
```

## Skills + plugins

`.claude/skills/` is the agent's playbook. Bundled skills:

- `memory-prune` — empties (never deletes) stale or duplicate entries
- `memory-organize` — keeps `MEMORY.md` grouped and under 200 lines
- `process-janitor` — wraps the `cmmd janitor` invocations safely

Drop a folder containing a `SKILL.md` into `.claude/skills/` and the
agent picks it up on the next tick.

`mmctl plugins` manages those folders:

```
mmctl plugins list
mmctl plugins install /path/to/local-skill
mmctl plugins install https://github.com/foo/some-skill
mmctl plugins disable <name>      # moves into .claude/skills-disabled/
mmctl plugins enable  <name>      # moves back
mmctl plugins remove  <name>
```

## Multi-MEMORY_ROOT

If you have memory dirs under more than one Claude account, point
`cmmd` at all of them:

```
MEMORY_ROOT=/home/you/.claude/projects/A/memory \
MEMORY_ROOTS=/home/you/.claude/projects/B/memory:/home/you/.claude/projects/C/memory \
cmmd run
```

Each tick rotates through every root in order. The first (`MEMORY_ROOT`)
is the "primary" — its stats appear in `mmctl status` / `mmctl memory`
for backward compat. Other roots show up in `mmctl history`.

## Metrics

A minimal Prometheus exporter listens on `$METRICS_BIND` (default
`127.0.0.1:9601`, set to empty string to disable):

```
$ curl -s 127.0.0.1:9601/metrics | head -10
# HELP cmmd_ticks_total Total tick attempts (ran + skipped).
# TYPE cmmd_ticks_total counter
cmmd_ticks_total 0
...
```

Exposed counters and gauges:

```
cmmd_ticks_total
cmmd_ticks_ran_total
cmmd_ticks_skipped_total
cmmd_tick_duration_sum_seconds
cmmd_tick_failures_total
cmmd_audit_issues_last
cmmd_history_appends_total
cmmd_last_tick_unix
cmmd_tick_staleness_seconds
```

`cmmd_tick_staleness_seconds` is the alert-friendly one: if it climbs
past `2 × TICK_INTERVAL_SEC`, the daemon is stuck.

## Files

| Path | Purpose |
| --- | --- |
| `src/main.rs`        | daemon entry, signal handling, subcommand dispatch |
| `src/lib.rs`         | shared modules used by both binaries |
| `src/config.rs`      | env → typed `Config`, `.env` loading, multi-root parsing |
| `src/audit.rs`       | deterministic memory audit (the cheap-tick optimization) |
| `src/authmux.rs`     | `authmux list/current/status` parser + `~/.claude-accounts` scanner |
| `src/process.rs`     | sysinfo snapshot + lsof-based `memory_holders` guard |
| `src/memory.rs`      | file count / total bytes / newest mtime / MEMORY.md line count |
| `src/janitor.rs`     | stale-process janitor (allowlist + TTY check + SIGTERM→SIGKILL) |
| `src/history.rs`     | JSONL append/tail + `git -C MEMORY_ROOT` wrappers |
| `src/state.rs`       | persisted runtime overrides (dry-run survives restart) |
| `src/metrics.rs`     | hand-rolled Prometheus HTTP exposition |
| `src/tick.rs`        | the full tick: lsof → audit → snapshot → spawn → timeout |
| `src/ipc.rs`         | Unix socket protocol (status / ping / tick / dry-run-on|off) |
| `src/bin/mmctl.rs`   | the companion CLI |
| `.claude/agents/memory-manager.md` | per-tick subagent prompt |
| `.claude/skills/*`   | skill specs the spawned agent reads |
| `scripts/start.sh`   | detached daemon lifecycle (pid + lock + log) |
| `scripts/stop.sh`    | SIGTERM, wait 10s, SIGKILL |
| `scripts/status.sh`  | proc info + mmctl status + log tail |
| `scripts/test-tick.sh` | end-to-end smoke against `test-fixtures/memory/` |
| `scripts/install-systemd.sh` | install + enable the user-level systemd unit |
| `scripts/install-desktop.sh` | XDG desktop file + icon → GNOME System Monitor |
| `systemd/claude-memory-manager.service` | the user-level unit |
| `test-fixtures/memory/` | synthetic memory dir exercising every audit case |
| `.github/workflows/ci.yml` | fmt + check + clippy + test on every push/PR |
| `.github/workflows/sandbox-tick.yml` | gated end-to-end tick (opt-in via `[run-tick]`) |

## Build

```
cargo build --release
# → target/release/cmmd
# → target/release/mmctl
```

## Run

```
# foreground, one-shot, dry-run by default
./target/release/cmmd run --once

# loop
./target/release/cmmd run

# detached
./scripts/start.sh
./scripts/status.sh
./scripts/stop.sh

# survive reboots
./scripts/install-systemd.sh
journalctl --user -u claude-memory-manager -f
```

## Full subcommand reference

### `cmmd`

```
cmmd run [--once]              # the daemon (default subcommand)
cmmd doctor                    # one-shot config + authmux + memory snapshot, no claude call
cmmd audit [--memory-root P] [--json]
                               # deterministic Rust audit, no token cost
cmmd janitor list   [--min-age-hours N] [--max-cpu-pct N] [--require-no-tty] [--json]
cmmd janitor apply  [--min-age-hours N] [--max N] [--no-dry-run] [--json]
cmmd history [-n N] [--json]
cmmd git-log [-n N]
cmmd restore <sha>
```

### `mmctl`

```
# read-only daemon queries (talks to the Unix socket)
mmctl status                   # full state as JSON
mmctl accounts                 # authmux block only
mmctl accounts --switch <email>
mmctl memory                   # MEMORY_ROOT stat only
mmctl last-tick
mmctl ping
mmctl logs -n 50 [--follow]

# act on the daemon
mmctl tick                     # poke immediate tick (non-blocking)
mmctl tick --wait              # poke + block until the tick lands
mmctl dry-run on | off         # toggle runtime dry-run, persisted across restarts

# proxies to cmmd (binary located as sibling first, $PATH second)
mmctl audit [--memory-root P] [--json]
mmctl history [-n N] [--json]
mmctl git-log [-n N]
mmctl diff <sha>               # what would `restore <sha>` change?
mmctl restore <sha>
mmctl tick-log <tick_id>       # full agent transcript from /tmp/cmmd-tick-<id>.log
mmctl janitor list  [--min-age-hours N] [--json]
mmctl janitor apply [--min-age-hours N] [--max N] [--no-dry-run] [--json]

# skill plugin management
mmctl plugins list
mmctl plugins install <path-or-git-url>
mmctl plugins disable <name>
mmctl plugins enable  <name>
mmctl plugins remove  <name>
```

## Configuration

All settings are environment variables; a `.env` in the working
directory is auto-loaded. Defaults aim at "safe on first run".

| Var | Default | Notes |
| --- | --- | --- |
| `MEMORY_ROOT` | `~/.claude/projects/-home-deadpool/memory` | the primary directory the daemon tends |
| `MEMORY_ROOTS` | _empty_ | colon-separated extra roots, rotated each tick |
| `TICK_INTERVAL_SEC` | `900` | sleep between ticks (15 min) |
| `MIN_IDLE_SEC` | `300` | refuse to tick if memory was just touched |
| `MAX_TICK_SECONDS` | `600` | hard cap on a single `claude -p` invocation |
| `DRY_RUN` | `true` | shipped safe; flip to `false` only after you trust it |
| `MODEL` | `claude-haiku-4-5-20251001` | model used per tick |
| `MAX_TURNS` | `12` | per-tick agent turn budget |
| `CLAUDE_BIN` | `claude` | absolute path also accepted |
| `AUTHMUX_BIN` | `authmux` | |
| `CLAUDE_CONFIG_DIR` | _unset_ | per-account override (used by authmux) |
| `CLAUDE_ACCOUNTS_DIR` | `~/.claude-accounts` | where the daemon scans for `account*/.credentials.json` |
| `GIT_TRACK_MEMORY` | `true` | auto-init git in `MEMORY_ROOT` for undo |
| `METRICS_BIND` | `127.0.0.1:9601` | empty to disable the Prometheus endpoint |
| `LOG_FILE` | `/tmp/claude-memory-manager.log` | |
| `PID_FILE` | `/tmp/claude-memory-manager.pid` | |
| `LOCK_FILE` | `/tmp/claude-memory-manager.lock` | |
| `STATUS_SOCK` | `/tmp/claude-memory-manager.sock` | mmctl ↔ daemon |
| `STATE_FILE` | `/tmp/cmmd-state.json` | persisted runtime overrides |
| `HISTORY_FILE` | `/tmp/cmmd-history.jsonl` | append-only tick log |
| `CMMD_LOG` | `info` | tracing-subscriber EnvFilter syntax |

## Safety defaults

- `DRY_RUN=true` ships in `.env.example` and the systemd unit. The agent
  reports proposed changes only until you flip to `false`.
- `mmctl dry-run off` is persisted to `$STATE_FILE` and survives a
  daemon restart.
- Ticks abort if anything other than the daemon has a file open under
  `MEMORY_ROOT` (lsof guard).
- Ticks abort if the audit finds zero issues (no token spend).
- Ticks abort if `MEMORY_ROOT` was modified less than `MIN_IDLE_SEC`
  ago.
- Allowed tools when `DRY_RUN=true` are `Read,Glob,Grep,Bash(find:*),Bash(ps:*)`
  only — no `Write`/`Edit`.
- `cmmd` never touches the `claude-mem` database or Colony hivemind.
  Per `~/.claude/CLAUDE.md`, those lanes own themselves.
- The janitor allowlist is in compiled Rust; no skill prompt can broaden
  it.

## Status

Daemon + audit + history + undo + janitor + metrics + multi-root all in
place. End-to-end tick verified against the sandbox fixtures (the agent
correctly prunes duplicates, adds missing **Why:** / **How to apply:**
lines, removes dangling MEMORY.md entries). CI is green on every push.
