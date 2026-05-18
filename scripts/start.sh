#!/usr/bin/env bash
# Start the daemon detached. Idempotent — refuses to start a second copy.
set -euo pipefail

cd "$(dirname "$0")/.."

PID_FILE="${PID_FILE:-/tmp/claude-memory-manager.pid}"
LOCK_FILE="${LOCK_FILE:-/tmp/claude-memory-manager.lock}"
LOG_FILE="${LOG_FILE:-/tmp/claude-memory-manager.log}"

if [[ -f "$LOCK_FILE" ]]; then
  PID=$(cat "$LOCK_FILE")
  if kill -0 "$PID" 2>/dev/null; then
    echo "already running (pid=$PID, lock=$LOCK_FILE)"
    exit 1
  fi
  echo "stale lock at $LOCK_FILE, clearing"
  rm -f "$LOCK_FILE" "$PID_FILE"
fi

if ! command -v bun >/dev/null 2>&1; then
  echo "bun not found in PATH" >&2
  exit 1
fi

nohup bun run src/daemon.ts >>"$LOG_FILE" 2>&1 &
echo $! > "$PID_FILE"
echo "started pid=$(cat "$PID_FILE"), log=$LOG_FILE"
