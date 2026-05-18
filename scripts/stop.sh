#!/usr/bin/env bash
set -euo pipefail

PID_FILE="${PID_FILE:-/tmp/claude-memory-manager.pid}"
LOCK_FILE="${LOCK_FILE:-/tmp/claude-memory-manager.lock}"

if [[ ! -f "$PID_FILE" ]]; then
  echo "no pid file at $PID_FILE — daemon not running?"
  exit 0
fi

PID=$(cat "$PID_FILE")
if kill -0 "$PID" 2>/dev/null; then
  kill -TERM "$PID"
  echo "sent SIGTERM to $PID, waiting..."
  for _ in $(seq 1 10); do
    if ! kill -0 "$PID" 2>/dev/null; then break; fi
    sleep 1
  done
  if kill -0 "$PID" 2>/dev/null; then
    echo "still alive after 10s, sending SIGKILL"
    kill -KILL "$PID" || true
  fi
fi

rm -f "$PID_FILE" "$LOCK_FILE"
echo "stopped"
