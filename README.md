<p align="left">
  <img src="assets/logo.svg" alt="claude-memory-manager-daemon" width="120" height="120">
</p>

# claude-memory-manager-daemon

A Rust daemon that tends the Claude Code **file-based memory lane**
(`~/.claude/projects/-home-deadpool/memory/`) in a real-time tick loop,
while continuously surfacing **authmux** login state and the local Claude
account directory tree.

It does NOT manage system RAM. It is a **janitor process** for your
Claude memory + a **live view** of which Claude accounts are logged in.

## Why Rust

Compared to the original TypeScript draft:

- **~5 MB RSS** vs ~100 MB for Bun/Node. This thing runs 24/7.
- **Single static binary** (`cargo build --release`) — no `bun`/`node` on the host.
- **No SDK lock-in**: shells out to the official `claude` CLI per tick. When
  you upgrade `claude`, the daemon upgrades for free. Auth, agents, skills,
  and MCP wiring stay handled by the CLI.
- **Strong types** for the daemon state shared across the loop and the
  `mmctl` companion CLI.

## Two binaries

| Binary  | Path                | Purpose |
| ------- | ------------------- | ------- |
| `cmmd`  | `target/.../cmmd`   | the daemon itself; subcommands `run` (default) and `doctor` |
| `mmctl` | `target/.../mmctl`  | live status client over a Unix socket: `status`, `accounts`, `memory`, `last-tick`, `ping` |

## Architecture

```
                        ┌──────────────────────────────────────┐
                        │            cmmd (Rust)              │
                        │                                      │
                        │  ┌────────────────────────────────┐  │
                        │  │ main loop (src/main.rs)        │  │
                        │  │                                │  │
                        │  │  every tick:                   │  │
                        │  │    1. authmux::snapshot()      │──┼──▶ `authmux list/current/status`
                        │  │    2. memory::stat()           │  │
                        │  │    3. process::find_claude…()  │  │
                        │  │    4. spawn `claude -p …`      │──┼──▶ `claude` CLI (subprocess)
                        │  └────────────────────────────────┘  │
                        │                                      │
                        │  ┌────────────────────────────────┐  │
                        │  │ Unix socket: $STATUS_SOCK      │◀─┼── mmctl status
                        │  └────────────────────────────────┘  │
                        └──────────────────────────────────────┘
                                       │
                                       ▼
                        ┌──────────────────────────────────────┐
                        │ ~/.claude/projects/-home-deadpool/   │
                        │   memory/   (file-based lane)        │
                        └──────────────────────────────────────┘
```

## What "always shows users are logged in" means

Every tick, before anything else, the daemon:

1. Calls `authmux current` → active account (the row prefixed with `*`).
2. Calls `authmux list`    → all managed accounts with `5h=` / `weekly=` usage %.
3. Calls `authmux status`  → auto-switch on/off + service state.
4. Walks `~/.claude-accounts/` and counts which `account*/` dirs have a
   `.credentials.json` (so even if `authmux` is missing, you still see what
   Claude Code accounts are provisioned locally).

This snapshot is held in shared state. Run `mmctl accounts` at any moment
to see it:

```
$ mmctl accounts
{
  "binary": "authmux",
  "available": true,
  "current": "odin@kollarrobert.sk",
  "accounts": [
    { "email": "admin@kollarrobert.sk", "kind": "ChatGPT seat (Business)",
      "five_h_pct": 98, "weekly_pct": 100, "active": false },
    ...
  ],
  "auto_switch": "OFF",
  "service_state": "inactive"
}
```

The daemon also emits a one-line login summary into the log every tick:

```
2026-05-18T11:04:12Z INFO login snapshot authmux_available=true
  authmux_active=odin@kollarrobert.sk authmux_total=23
  claude_account_dirs_with_creds=2
```

## Files

| Path | Purpose |
| --- | --- |
| `src/main.rs`        | daemon entry, tick loop, signal handling |
| `src/lib.rs`         | re-exports modules to both binaries |
| `src/config.rs`      | env → typed `Config`, supports `.env` |
| `src/authmux.rs`     | `authmux list/current/status` parser + `~/.claude-accounts` scanner |
| `src/process.rs`     | `sysinfo` snapshot, `find_claude_sessions` |
| `src/memory.rs`      | file count / total bytes / newest mtime for `MEMORY_ROOT` |
| `src/tick.rs`        | spawns `claude -p ...`, streams `stream-json` lines into the log |
| `src/ipc.rs`         | PID lock + Unix-socket status server / client |
| `src/bin/mmctl.rs`   | companion CLI |
| `.claude/agents/memory-manager.md` | per-tick subagent prompt |
| `src/janitor.rs`     | stale-process janitor: allowlisted-name scan + SIGTERM/SIGKILL with hard ceilings |
| `.claude/skills/{memory-audit,memory-prune,memory-organize,process-janitor}` | four skills |
| `scripts/{start,stop,status}.sh` | local lifecycle |
| `systemd/claude-memory-manager.service` | optional user-level systemd unit |

## Build

```
cargo build --release
# → target/release/cmmd
# → target/release/mmctl
```

## Run

Foreground, one-shot, dry-run:

```
./target/release/cmmd run --once
```

Foreground, looping, dry-run (Ctrl-C to stop):

```
./target/release/cmmd run
```

Detached:

```
./scripts/start.sh
./scripts/status.sh        # uses mmctl for live state
./scripts/stop.sh
```

Resolved config + one-shot snapshot, no daemon:

```
./target/release/cmmd doctor
```

Janitor — find or terminate stale Claude / Codex / Kiro CLI sessions
(allowlist: `claude`, `claude-cli`, `kiro-cli`, `kiro-cli-chat`, `codex`,
`codex-cli`). Safe by default — `apply` is a preview unless `--no-dry-run`
is passed:

```
./target/release/cmmd janitor list                          # default: age ≥ 24h, cpu ≤ 0.5%
./target/release/cmmd janitor list --min-age-hours=1 --json
./target/release/cmmd janitor apply --max=5                 # preview (dry run)
./target/release/cmmd janitor apply --no-dry-run --max=5    # actually SIGTERM, then SIGKILL after 10s
```

Safety baked into the compiled binary (cannot be overridden by env or flags):

- Name allowlist as above.
- Process owner must equal the daemon's uid.
- Daemon's own pid + the daemon's direct children are skipped.
- PID 1 and kernel threads are skipped.
- Hard ceiling of 20 kills per invocation, regardless of `--max`.

Live queries against the running daemon:

```
./target/release/mmctl status      # full JSON snapshot
./target/release/mmctl accounts    # authmux block only
./target/release/mmctl memory      # MEMORY_ROOT stat only
./target/release/mmctl last-tick   # last tick record
./target/release/mmctl ping        # liveness
```

## Safety defaults

- **`DRY_RUN=true`** is shipped in `.env.example` and the systemd unit. The
  agent reports proposed changes to the log; no writes until you flip to
  `false`.
- Tick **aborts** if any non-daemon `claude` / `claude-cli` / `kiro-cli`
  process is running. Never races a live session.
- Tick **aborts** if `MEMORY_ROOT` was touched < `MIN_IDLE_SEC` (300 s
  default) ago.
- MCP / tool surface is **read-only** when `DRY_RUN=true` (allowedTools =
  `Read,Glob,Grep,Bash(find:*),Bash(ps:*)` — no Write/Edit).
- Lockfile prevents two daemons from running at once.
- Per `~/.claude/CLAUDE.md`, the daemon **never** touches the `claude-mem`
  database or Colony hivemind. Those lanes own themselves.

## Status

Pre-alpha. The Rust scaffold compiles in theory — verify with
`cargo check` before relying on it. The skills under `.claude/skills/`
are markdown specs only; the agent inside `claude` interprets them. The
auditing logic itself runs inside the spawned `claude` process, so the
daemon is intentionally thin.
