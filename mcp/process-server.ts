#!/usr/bin/env bun
/**
 * Read-only MCP stdio server exposing local system process information.
 *
 * Tools:
 *   - list_processes(filter?)        : top processes by RSS, optional name filter
 *   - find_claude_sessions()         : claude / claude-cli / kiro-cli processes only
 *   - memory_dir_stat(path)          : file count + newest mtime for a dir
 *
 * The server is INTENTIONALLY READ-ONLY. It does not expose kill/signal/exec.
 */

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";

type Proc = {
  pid: number;
  user: string;
  rss_kb: number;
  pcpu: number;
  command: string;
};

function ps(): Proc[] {
  const r = spawnSync(
    "ps",
    ["-eo", "pid=,user=,rss=,pcpu=,args=", "--sort=-rss"],
    { encoding: "utf8" },
  );
  if (r.status !== 0) return [];
  const out: Proc[] = [];
  for (const line of r.stdout.split("\n")) {
    const m = line.match(/^\s*(\d+)\s+(\S+)\s+(\d+)\s+(\S+)\s+(.*)$/);
    if (!m) continue;
    out.push({
      pid: Number(m[1]),
      user: m[2]!,
      rss_kb: Number(m[3]),
      pcpu: Number(m[4]),
      command: m[5]!,
    });
  }
  return out;
}

const server = new Server(
  { name: "process-server", version: "0.1.0" },
  { capabilities: { tools: {} } },
);

server.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools: [
    {
      name: "list_processes",
      description: "List top processes by resident memory. Read-only.",
      inputSchema: {
        type: "object",
        properties: {
          filter: { type: "string", description: "case-insensitive substring of command" },
          limit: { type: "number", description: "max rows (default 30)" },
        },
      },
    },
    {
      name: "find_claude_sessions",
      description: "Return only Claude Code / CLI sessions so the daemon can avoid racing them.",
      inputSchema: { type: "object", properties: {} },
    },
    {
      name: "memory_dir_stat",
      description: "Quick stat of a memory directory: file count, newest mtime, total bytes.",
      inputSchema: {
        type: "object",
        properties: { path: { type: "string" } },
        required: ["path"],
      },
    },
  ],
}));

server.setRequestHandler(CallToolRequestSchema, async (req) => {
  const { name, arguments: args = {} } = req.params;

  if (name === "list_processes") {
    const filter = (args.filter as string | undefined)?.toLowerCase();
    const limit = (args.limit as number | undefined) ?? 30;
    let rows = ps();
    if (filter) rows = rows.filter(p => p.command.toLowerCase().includes(filter));
    rows = rows.slice(0, limit);
    return { content: [{ type: "text", text: JSON.stringify(rows, null, 2) }] };
  }

  if (name === "find_claude_sessions") {
    const pat = /\b(claude|claude-cli|kiro-cli|kiro-cli-chat)\b/i;
    const rows = ps().filter(p => pat.test(p.command));
    return { content: [{ type: "text", text: JSON.stringify(rows, null, 2) }] };
  }

  if (name === "memory_dir_stat") {
    const path = args.path as string;
    if (!path || !existsSync(path)) {
      return { content: [{ type: "text", text: JSON.stringify({ error: "path not found", path }) }] };
    }
    const find = spawnSync("find", [path, "-type", "f", "-printf", "%T@ %s %p\n"], { encoding: "utf8" });
    if (find.status !== 0) {
      return { content: [{ type: "text", text: JSON.stringify({ error: "find failed" }) }] };
    }
    let count = 0, bytes = 0, newest = 0;
    for (const line of find.stdout.split("\n")) {
      const parts = line.split(/\s+/, 3);
      if (parts.length < 3) continue;
      const mtime = Number(parts[0]);
      const size = Number(parts[1]);
      if (!Number.isFinite(mtime)) continue;
      count++;
      bytes += size;
      if (mtime > newest) newest = mtime;
    }
    return {
      content: [{
        type: "text",
        text: JSON.stringify({
          path,
          file_count: count,
          total_bytes: bytes,
          newest_mtime_iso: newest ? new Date(newest * 1000).toISOString() : null,
          idle_sec: newest ? Math.floor(Date.now() / 1000 - newest) : null,
        }, null, 2),
      }],
    };
  }

  return { content: [{ type: "text", text: `unknown tool: ${name}` }], isError: true };
});

const transport = new StdioServerTransport();
await server.connect(transport);
