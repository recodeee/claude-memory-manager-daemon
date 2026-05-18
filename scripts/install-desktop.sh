#!/usr/bin/env bash
# Install a user-level .desktop entry + icon so GNOME System Monitor
# (and other XDG-aware tools) show the Claude sparkle for the daemon
# processes instead of the generic "settings" gear.
#
# Idempotent. Re-run after `cargo build --release` if you ever move the
# repo or rename the binary.

set -euo pipefail
cd "$(dirname "$0")/.."

REPO_DIR="$(pwd)"
BIN_PATH="$REPO_DIR/target/release/cmmd"
ICON_SRC="$REPO_DIR/assets/logo.svg"

ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"
APPS_DIR="$HOME/.local/share/applications"

mkdir -p "$ICON_DIR" "$APPS_DIR"

# --- Icon ---
# A SINGLE canonical name. Both .desktop files below reference it.
ICON_NAME="claude-memory-manager-daemon"
cp -f "$ICON_SRC" "$ICON_DIR/$ICON_NAME.svg"
echo "icon → $ICON_DIR/$ICON_NAME.svg"

write_desktop() {
  local file="$1"
  local exec_line="$2"
  local proc_name="$3"
  cat > "$file" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=Claude Memory Manager Daemon
GenericName=Claude memory daemon
Comment=Tends the Claude file-based memory lane and watches authmux logins.
Exec=$exec_line
TryExec=$BIN_PATH
Icon=$ICON_NAME
Terminal=false
NoDisplay=true
Categories=Utility;System;
StartupWMClass=$proc_name
StartupNotify=false
Keywords=claude;memory;daemon;authmux;
X-GNOME-UsesNotifications=false
EOF
  echo "desktop → $file"
}

# Two entries so System Monitor's name→.desktop match works whether the
# process shows up as `cmmd` (the new Rust binary) or `claude-daemon-memory`
# (a legacy or symlink name some screenshots showed).
write_desktop "$APPS_DIR/cmmd.desktop"                  "$BIN_PATH run"  "cmmd"
write_desktop "$APPS_DIR/claude-daemon-memory.desktop"  "$BIN_PATH run"  "claude-daemon-memory"

# Refresh caches so the change is visible immediately.
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
  echo "icon cache refreshed"
fi
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$APPS_DIR" 2>/dev/null || true
  echo "desktop database refreshed"
fi

echo
echo "Done. If GNOME System Monitor is open, close and re-open it"
echo "(or press Ctrl-Q then re-launch). The Claude sparkle should"
echo "replace the gear icon for cmmd / claude-daemon-memory rows."
