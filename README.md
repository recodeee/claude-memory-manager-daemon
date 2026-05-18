# claude-memory-manager-daemon

A long-running Claude Code agent that tends the **file-based memory lane**
(`~/.claude/projects/-home-deadpool/memory/`) in a real-time loop while
watching the local system process table.

Think of it as a janitor process for your Claude memory:

- Keeps `MEMORY.md` tidy and under the 200-line truncation threshold
- Prunes stale entries that no longer match what's on disk
- Detects duplicates and merges them
- Re-organizes entries by topic (user / feedback / project / reference)
- Surfaces what Claude Code sessions are currently running so it never
  rewrites a file another live session has touched

It is **NOT** a system-memory (RAM) optimizer. The process viewer is only
used to coordinate with other Claude sessions — never to kill processes.

## Architecture

```
                   ┌──────────────────────────────────┐
                   │ claude-memory-manager-daemon     │
                   │                                  │
                   │  ┌────────────────────────────┐  │
                   │  │ daemon loop (src/daemon.ts)│  │
                   │  └─────────────┬──────────────┘  │
                   │                │ spawns          │
                   │  ┌─────────────▼──────────────┐  │
                   │  │ Claude Agent SDK query()   │  │
                   │  │  - memory-manager subagent │  │
                   │  │  - mcp: process-server     │  │
                   │  │  - skills/*                │  │
                   │  └────────────────────────────┘  │
                   └──────────────────────────────────┘
                                 │
                                 ▼
              ┌──────────────────────────────────────┐
              │ ~/.claude/projects/-home-deadpool/   │
              │   memory/   (file-based lane)        │
              │     ├── MEMORY.md      (index)       │
              │     └── *.md           (entries)     │
              └──────────────────────────────────────┘
```

## Components

| Path | Purpose |
| --- | --- |
| `src/daemon.ts` | Long-running loop. Calls the Claude Agent SDK once per tick. |
| `src/process-watcher.ts` | Snapshot of `claude` / `node` / `bun` processes so the agent can coordinate. |
| `mcp/process-server.ts` | MCP stdio server. Exposes `list_processes`, `find_claude_sessions`, `memory_dir_stat`. |
| `.claude/agents/memory-manager.md` | Subagent prompt the daemon loop calls. |
| `.claude/skills/memory-audit/` | Skill: read every memory file, flag staleness / dup. |
| `.claude/skills/memory-prune/` | Skill: remove entries the user agreed are stale. |
| `.claude/skills/memory-organize/` | Skill: rewrite `MEMORY.md` so it stays under 200 lines. |
| `scripts/start.sh` | Start the daemon detached, write pid to `/tmp/claude-memory-manager.pid`. |
| `scripts/stop.sh` | Stop via pid file. |
| `scripts/status.sh` | Show pid, uptime, last tick. |
| `systemd/claude-memory-manager.service` | Optional user-level systemd unit. |

## Configuration

Set in `.env` (gitignored):

```
MEMORY_ROOT=/home/deadpool/.claude/projects/-home-deadpool/memory
TICK_INTERVAL_SEC=900           # 15 minutes between audits
ANTHROPIC_API_KEY=...           # if not using subscription auth
DRY_RUN=true                    # safety default — agent reports, does not write
MODEL=claude-haiku-4-5-20251001 # cheap default for the loop
LOG_FILE=/tmp/claude-memory-manager.log
```

## Install

```
cd ~/Documents/claude-memory-manager-daemon
bun install
```

## Run (foreground, dry-run)

```
DRY_RUN=true bun run src/daemon.ts
```

## Run (detached)

```
./scripts/start.sh
./scripts/status.sh
./scripts/stop.sh
```

## Safety defaults

- **DRY_RUN=true** is the shipped default. The agent reports proposed
  changes to the log file; it does not mutate memory until you set
  `DRY_RUN=false`.
- The MCP server is **read-only**. It cannot kill, signal, or modify
  any process — only enumerate them.
- The daemon never touches Colony hivemind state or the `claude-mem`
  database. Per `~/.claude/CLAUDE.md`, those lanes are owned by their
  respective systems.
- A lockfile at `/tmp/claude-memory-manager.lock` prevents two daemons
  from running at once.

## Status

Pre-alpha scaffold. Run with `DRY_RUN=true` and read the log before
trusting it with writes.
