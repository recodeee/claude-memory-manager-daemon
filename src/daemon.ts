#!/usr/bin/env bun
/**
 * claude-memory-manager-daemon
 *
 * Long-running loop. Each tick spawns a short Claude Agent SDK query against
 * the memory-manager subagent. The agent inspects MEMORY_ROOT, audits
 * MEMORY.md and individual memory files, and (when DRY_RUN=false) prunes /
 * reorganizes entries. The agent has access to:
 *
 *   - the process-server MCP (read-only system process listing)
 *   - the three skills under .claude/skills/
 *   - the standard Read / Write / Edit tools, scoped to MEMORY_ROOT
 */

import { query, type SDKMessage } from "@anthropic-ai/claude-agent-sdk";
import { existsSync, mkdirSync, readFileSync, writeFileSync, appendFileSync, unlinkSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { spawnSync } from "node:child_process";

type Config = {
  memoryRoot: string;
  tickIntervalSec: number;
  minIdleSec: number;
  dryRun: boolean;
  model: string;
  maxTurns: number;
  logFile: string;
  pidFile: string;
  lockFile: string;
};

function loadConfig(): Config {
  // Load .env if present (no external dep — single-pass parser)
  const envPath = resolve(process.cwd(), ".env");
  if (existsSync(envPath)) {
    for (const line of readFileSync(envPath, "utf8").split("\n")) {
      const m = line.match(/^([A-Z0-9_]+)=(.*)$/);
      if (m && !process.env[m[1]!]) process.env[m[1]!] = m[2]!.replace(/^['"]|['"]$/g, "");
    }
  }

  return {
    memoryRoot: process.env.MEMORY_ROOT ?? `${process.env.HOME}/.claude/projects/-home-deadpool/memory`,
    tickIntervalSec: Number(process.env.TICK_INTERVAL_SEC ?? 900),
    minIdleSec: Number(process.env.MIN_IDLE_SEC ?? 300),
    dryRun: (process.env.DRY_RUN ?? "true").toLowerCase() !== "false",
    model: process.env.MODEL ?? "claude-haiku-4-5-20251001",
    maxTurns: Number(process.env.MAX_TURNS ?? 12),
    logFile: process.env.LOG_FILE ?? "/tmp/claude-memory-manager.log",
    pidFile: process.env.PID_FILE ?? "/tmp/claude-memory-manager.pid",
    lockFile: process.env.LOCK_FILE ?? "/tmp/claude-memory-manager.lock",
  };
}

function log(cfg: Config, msg: string): void {
  const line = `[${new Date().toISOString()}] ${msg}\n`;
  mkdirSync(dirname(cfg.logFile), { recursive: true });
  appendFileSync(cfg.logFile, line);
  if (process.stdout.isTTY) process.stdout.write(line);
}

function acquireLock(cfg: Config): boolean {
  if (existsSync(cfg.lockFile)) {
    const pid = Number(readFileSync(cfg.lockFile, "utf8").trim());
    // Is the holder still alive?
    try {
      process.kill(pid, 0);
      return false;
    } catch {
      // Stale lock — overwrite
    }
  }
  writeFileSync(cfg.lockFile, String(process.pid));
  writeFileSync(cfg.pidFile, String(process.pid));
  return true;
}

function releaseLock(cfg: Config): void {
  for (const f of [cfg.lockFile, cfg.pidFile]) {
    try { unlinkSync(f); } catch { /* ignore */ }
  }
}

/** Newest mtime among files under MEMORY_ROOT, in seconds-since-epoch. */
function newestMemoryMtime(memoryRoot: string): number {
  const r = spawnSync("find", [memoryRoot, "-type", "f", "-printf", "%T@\n"], { encoding: "utf8" });
  if (r.status !== 0) return 0;
  return r.stdout.split("\n").map(Number).filter(Boolean).reduce((a, b) => Math.max(a, b), 0);
}

async function runTick(cfg: Config): Promise<void> {
  const idleSec = Math.floor(Date.now() / 1000 - newestMemoryMtime(cfg.memoryRoot));
  if (idleSec < cfg.minIdleSec) {
    log(cfg, `tick skipped: memory mutated ${idleSec}s ago (< MIN_IDLE_SEC=${cfg.minIdleSec})`);
    return;
  }

  log(cfg, `tick start (dry_run=${cfg.dryRun}, model=${cfg.model})`);

  const prompt = [
    `You are the memory-manager daemon tick agent.`,
    ``,
    `MEMORY_ROOT: ${cfg.memoryRoot}`,
    `DRY_RUN: ${cfg.dryRun}`,
    ``,
    `Your job for THIS tick:`,
    `1. Use the memory-audit skill to inventory memory files.`,
    `2. Use the process-server MCP to confirm no OTHER live Claude Code session is`,
    `   currently writing to MEMORY_ROOT. If find_claude_sessions returns more than`,
    `   one active claude process, abort the tick and report.`,
    `3. If audit finds issues (stale entries, duplicates, MEMORY.md over 200 lines,`,
    `   broken [[links]]), invoke memory-organize / memory-prune skills.`,
    `4. If DRY_RUN is true, REPORT proposed changes only. Do not Edit or Write.`,
    `5. End with a one-paragraph summary: counts of files, issues found, actions taken.`,
  ].join("\n");

  const messages: SDKMessage[] = [];

  try {
    const response = query({
      prompt,
      options: {
        model: cfg.model,
        maxTurns: cfg.maxTurns,
        agents: {
          "memory-manager": {
            description: "Audits and tends the file-based memory lane.",
            prompt: readFileSync(resolve(process.cwd(), ".claude/agents/memory-manager.md"), "utf8"),
            tools: ["Read", "Write", "Edit", "Glob", "Grep", "Bash"],
            model: cfg.model,
          },
        },
        mcpServers: {
          "process-server": {
            type: "stdio",
            command: "bun",
            args: [resolve(process.cwd(), "mcp/process-server.ts")],
          },
        },
        allowedTools: ["Read", "Glob", "Grep", "Task", "mcp__process-server__*"]
          .concat(cfg.dryRun ? [] : ["Write", "Edit"]),
        cwd: process.cwd(),
      },
    });

    for await (const m of response) {
      messages.push(m);
      if (m.type === "assistant") {
        for (const block of m.message.content) {
          if (block.type === "text") log(cfg, `agent: ${block.text.slice(0, 500)}`);
        }
      }
    }
    log(cfg, `tick ok (${messages.length} messages)`);
  } catch (err) {
    log(cfg, `tick FAILED: ${err instanceof Error ? err.message : String(err)}`);
  }
}

async function main(): Promise<void> {
  const cfg = loadConfig();
  const once = process.argv.includes("--once");

  if (!acquireLock(cfg)) {
    console.error(`another daemon already holds ${cfg.lockFile}`);
    process.exit(2);
  }

  const cleanup = (): void => { releaseLock(cfg); process.exit(0); };
  process.on("SIGINT", cleanup);
  process.on("SIGTERM", cleanup);

  log(cfg, `daemon up (pid=${process.pid}, memory=${cfg.memoryRoot})`);

  if (once) {
    await runTick(cfg);
    releaseLock(cfg);
    return;
  }

  // Real-time loop. Each tick is guarded by MIN_IDLE_SEC so we never race a live session.
  while (true) {
    await runTick(cfg);
    await new Promise(r => setTimeout(r, cfg.tickIntervalSec * 1000));
  }
}

main().catch(err => { console.error(err); process.exit(1); });
