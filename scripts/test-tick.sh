#!/usr/bin/env bash
# End-to-end tick smoke test against the synthetic memory fixture.
#
# Copies test-fixtures/memory/ → /tmp/cmmd-test-memory/, prints the before
# state, runs ONE tick with DRY_RUN=false (mutations allowed), then prints
# the after state + a diff. The new lsof-based session guard means the tick
# will actually run even with live claude sessions on the box, as long as
# /tmp/cmmd-test-memory has no open file descriptors pointing at it.
#
# Use:   ./scripts/test-tick.sh
# Reset: ./scripts/test-tick.sh --reset       (wipes /tmp/cmmd-test-memory only)

set -euo pipefail
cd "$(dirname "$0")/.."

SANDBOX=/tmp/cmmd-test-memory
FIXTURES="$(pwd)/test-fixtures/memory"

if [[ "${1:-}" == "--reset" ]]; then
  rm -rf "$SANDBOX"
  echo "wiped $SANDBOX"
  exit 0
fi

if [[ ! -d "$FIXTURES" ]]; then
  echo "missing $FIXTURES — are you in the repo root?" >&2
  exit 1
fi

# Fresh copy each run.
rm -rf "$SANDBOX"
cp -a "$FIXTURES" "$SANDBOX"
echo "=== sandbox prepared at $SANDBOX ==="
ls -la "$SANDBOX"
echo

echo "=== MEMORY.md (before) ==="
cat "$SANDBOX/MEMORY.md"
echo

# Build the binary if needed.
if [[ ! -x ./target/release/cmmd ]]; then
  cargo build --release --quiet --bin cmmd
fi

# Drive one tick.
# - MIN_IDLE_SEC=0     : memory was just copied; don't skip on idle.
# - DRY_RUN=false      : agent is allowed to Edit/Write.
# - MAX_TICK_SECONDS=180: cap so a runaway agent doesn't hang the script.
# - STATE_FILE override: don't pollute the real daemon's persisted state.
echo "=== running cmmd run --once ==="
env \
  MEMORY_ROOT="$SANDBOX" \
  DRY_RUN=false \
  MIN_IDLE_SEC=0 \
  TICK_INTERVAL_SEC=60 \
  MAX_TICK_SECONDS=180 \
  STATE_FILE=/tmp/cmmd-test-state.json \
  PID_FILE=/tmp/cmmd-test.pid \
  LOCK_FILE=/tmp/cmmd-test.lock \
  STATUS_SOCK=/tmp/cmmd-test.sock \
  LOG_FILE=/tmp/cmmd-test.log \
  CMMD_LOG=info \
  ./target/release/cmmd run --once 2>&1 | tail -40

echo
echo "=== MEMORY.md (after) ==="
cat "$SANDBOX/MEMORY.md"
echo
echo "=== diff vs fixture ==="
diff -r "$FIXTURES" "$SANDBOX" || true
echo
echo "=== state ==="
ls -la "$SANDBOX"
