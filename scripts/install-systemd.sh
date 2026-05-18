#!/usr/bin/env bash
# Install the cmmd systemd user unit so the daemon survives reboots.
# Idempotent. Stops any ad-hoc daemon first.

set -euo pipefail
cd "$(dirname "$0")/.."

REPO_DIR="$(pwd)"
UNIT_NAME="claude-memory-manager.service"
UNIT_SRC="$REPO_DIR/systemd/$UNIT_NAME"
UNIT_DST_DIR="$HOME/.config/systemd/user"

if [[ ! -x "$REPO_DIR/target/release/cmmd" ]]; then
  echo "error: $REPO_DIR/target/release/cmmd not built." >&2
  echo "Run: cargo build --release" >&2
  exit 1
fi

# If an ad-hoc daemon is running via start.sh, stop it so systemd can take over.
if [[ -f /tmp/claude-memory-manager.pid ]] && kill -0 "$(cat /tmp/claude-memory-manager.pid)" 2>/dev/null; then
  echo "stopping ad-hoc daemon (pid=$(cat /tmp/claude-memory-manager.pid))"
  "$REPO_DIR/scripts/stop.sh" || true
fi

mkdir -p "$UNIT_DST_DIR"
cp -f "$UNIT_SRC" "$UNIT_DST_DIR/$UNIT_NAME"
echo "unit → $UNIT_DST_DIR/$UNIT_NAME"

systemctl --user daemon-reload
systemctl --user enable --now "$UNIT_NAME"

echo
systemctl --user --no-pager status "$UNIT_NAME" | head -12
echo
echo "Follow logs:    journalctl --user -u $UNIT_NAME -f"
echo "Restart:        systemctl --user restart $UNIT_NAME"
echo "Disable:        systemctl --user disable --now $UNIT_NAME"
