#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

PID_FILE="${PID_FILE:-/tmp/claude-memory-manager.pid}"
LOG_FILE="${LOG_FILE:-/tmp/claude-memory-manager.log}"
SOCK="${STATUS_SOCK:-/tmp/claude-memory-manager.sock}"

if [[ -f "$PID_FILE" ]] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
  PID=$(cat "$PID_FILE")
  echo "status: running"
  echo "pid:    $PID"
  ps -p "$PID" -o pid=,etime=,rss=,pcpu=,cmd= | sed 's/^/proc:   /'
else
  echo "status: not running"
fi

MMCTL="./target/release/mmctl"
[[ -x "$MMCTL" ]] || MMCTL="./target/debug/mmctl"
if [[ -x "$MMCTL" && -S "$SOCK" ]]; then
  echo
  echo "--- live snapshot via mmctl ---"
  "$MMCTL" --sock "$SOCK" status || true
fi

if [[ -f "$LOG_FILE" ]]; then
  echo
  echo "--- last 10 log lines ($LOG_FILE) ---"
  tail -n 10 "$LOG_FILE"
fi
