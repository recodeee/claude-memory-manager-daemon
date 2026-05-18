#!/usr/bin/env bash
# Start the cmmd daemon detached. Idempotent.
set -euo pipefail
cd "$(dirname "$0")/.."

PID_FILE="${PID_FILE:-/tmp/claude-memory-manager.pid}"
LOCK_FILE="${LOCK_FILE:-/tmp/claude-memory-manager.lock}"
LOG_FILE="${LOG_FILE:-/tmp/claude-memory-manager.log}"

BIN="./target/release/cmmd"
if [[ ! -x "$BIN" ]]; then
  BIN="./target/debug/cmmd"
fi
if [[ ! -x "$BIN" ]]; then
  echo "no cmmd binary found — run: cargo build --release" >&2
  exit 1
fi

if [[ -f "$LOCK_FILE" ]]; then
  PID=$(cat "$LOCK_FILE")
  if kill -0 "$PID" 2>/dev/null; then
    echo "already running (pid=$PID, lock=$LOCK_FILE)"
    exit 1
  fi
  echo "stale lock at $LOCK_FILE, clearing"
  rm -f "$LOCK_FILE" "$PID_FILE"
fi

nohup "$BIN" run >>"$LOG_FILE" 2>&1 &
echo $! > "$PID_FILE"
echo "started pid=$(cat "$PID_FILE"), log=$LOG_FILE"
