#!/usr/bin/env bash
# install.sh — one-shot installer for rewrite-it.
#
# Builds the release binary, installs it together with the DBus activation
# service, the clipboard helper script, the KDE service menu, and registers an
# optional keyboard shortcut for "Help me rewrite (fix grammar)".
#
# Tested on KDE Plasma 6+ (Wayland + X11) and GNOME 45+.
#
# Usage:
#   bash install.sh [--cuda | --vulkan]   # optional GPU back-end

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FEATURES=""

# ── Parse args ────────────────────────────────────────────────────────────────
for arg in "$@"; do
    case "$arg" in
        --cuda)   FEATURES="--features cuda"  ;;
        --vulkan) FEATURES="--features vulkan" ;;
        --help|-h)
            echo "Usage: bash install.sh [--cuda | --vulkan]"
            exit 0 ;;
    esac
done

# ── Dependency checks ─────────────────────────────────────────────────────────
echo "==> Checking build dependencies…"

check_cmd() {
    command -v "$1" &>/dev/null || { echo "Error: '$1' not found. Install it and try again."; exit 1; }
}

check_cmd cargo
check_cmd cmake   # required by llama-cpp-2 build script
check_cmd cc      # C/C++ compiler needed to build llama.cpp

# ── Build ─────────────────────────────────────────────────────────────────────
echo "==> Building rewrite-it (release)…"
cd "$REPO_ROOT"
# shellcheck disable=SC2086
cargo build --release $FEATURES

BINARY="$REPO_ROOT/target/release/rewrite-it"

# ── Install binary ────────────────────────────────────────────────────────────
INSTALL_DIR="$HOME/.local/bin"
mkdir -p "$INSTALL_DIR"
cp "$BINARY" "$INSTALL_DIR/rewrite-it"
chmod +x "$INSTALL_DIR/rewrite-it"
echo "    binary  → $INSTALL_DIR/rewrite-it"

# ── Install clipboard helper ──────────────────────────────────────────────────
cp "$REPO_ROOT/assets/rewrite-it-selection" "$INSTALL_DIR/rewrite-it-selection"
chmod +x "$INSTALL_DIR/rewrite-it-selection"
echo "    helper  → $INSTALL_DIR/rewrite-it-selection"

# Warn if ~/.local/bin is not in PATH
if ! echo "$PATH" | tr ':' '\n' | grep -qxF "$INSTALL_DIR"; then
    echo ""
    echo "  ⚠  $INSTALL_DIR is not in your PATH."
    echo "  Add this to your ~/.bashrc or ~/.profile:"
    echo "      export PATH=\"\$HOME/.local/bin:\$PATH\""
fi

# ── DBus session activation ───────────────────────────────────────────────────
DBUS_SVC_DIR="$HOME/.local/share/dbus-1/services"
mkdir -p "$DBUS_SVC_DIR"

# Patch the Exec path in the service file to the actual binary location.
sed "s|Exec=.*|Exec=$INSTALL_DIR/rewrite-it daemon|" \
    "$REPO_ROOT/assets/org.rewriteit.Rewriter1.service" \
    > "$DBUS_SVC_DIR/org.rewriteit.Rewriter1.service"

echo "    dbus    → $DBUS_SVC_DIR/org.rewriteit.Rewriter1.service"

# ── Systemd user service (optional, provides watchdog + auto-restart) ─────────
SYSTEMD_USER_DIR="$HOME/.config/systemd/user"
mkdir -p "$SYSTEMD_USER_DIR"

# Patch the ExecStart path to the actual binary location.
sed "s|ExecStart=.*|ExecStart=$INSTALL_DIR/rewrite-it daemon|" \
    "$REPO_ROOT/assets/rewrite-it.service" \
    > "$SYSTEMD_USER_DIR/rewrite-it.service"

echo "    systemd → $SYSTEMD_USER_DIR/rewrite-it.service"

if command -v systemctl &>/dev/null && systemctl --user is-system-running &>/dev/null 2>&1; then
    systemctl --user daemon-reload
    echo "    systemctl --user daemon-reload  ✓"
    echo "    To enable auto-start on login: systemctl --user enable --now rewrite-it.service"
else
    echo "    (systemd user session not running; reload manually after login)"
fi

# ── KDE service menu ──────────────────────────────────────────────────────────
if [ -d "$HOME/.local/share/kio" ] || command -v plasmashell &>/dev/null; then
    KDE_MENU_DIR="$HOME/.local/share/kio/servicemenus"
    mkdir -p "$KDE_MENU_DIR"
    cp "$REPO_ROOT/assets/rewrite-it-kde.desktop" "$KDE_MENU_DIR/rewrite-it.desktop"
    echo "    kde     → $KDE_MENU_DIR/rewrite-it.desktop"
fi

# ── Keyboard shortcut ─────────────────────────────────────────────────────────
echo ""
echo "==> Registering keyboard shortcut…"

SHORTCUT_CMD="$INSTALL_DIR/rewrite-it-selection grammar"
SHORTCUT_KEY="Meta+Shift+R"

if command -v kwriteconfig6 &>/dev/null || command -v kwriteconfig5 &>/dev/null; then
    # KDE Plasma custom shortcut via kwriteconfig
    KC="$(command -v kwriteconfig6 2>/dev/null || command -v kwriteconfig5)"
    "$KC" --file kglobalshortcutsrc \
          --group "rewrite-it" \
          --key "rewrite-grammar" \
          "$SHORTCUT_CMD,none,Help me rewrite (grammar)"
    # Reload shortcuts daemon
    command -v kquitapp6 &>/dev/null && kquitapp6 kglobalaccel 2>/dev/null || true
    command -v kglobalaccel6 &>/dev/null && kglobalaccel6 &>/dev/null || true
    echo "    KDE shortcut registered: $SHORTCUT_KEY → $SHORTCUT_CMD"
    echo "    (You may also set it manually in System Settings → Keyboard → Shortcuts)"
elif command -v gsettings &>/dev/null && gsettings list-schemas 2>/dev/null | grep -q 'org.gnome.settings-daemon'; then
    # GNOME: add a custom keybinding
    BINDING_PATH='/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/rewrite-it/'
    gsettings set org.gnome.settings-daemon.plugins.media-keys custom-keybindings \
        "['${BINDING_PATH}']"
    gsettings set "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:${BINDING_PATH}" name    'Help me rewrite'
    gsettings set "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:${BINDING_PATH}" command "$SHORTCUT_CMD"
    gsettings set "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:${BINDING_PATH}" binding '<Super><Shift>r'
    echo "    GNOME shortcut registered: Super+Shift+R → $SHORTCUT_CMD"
else
    echo "    Could not auto-register shortcut."
    echo "    Set it manually: $SHORTCUT_CMD"
fi

echo ""
echo "==> Installation complete!"
echo "    Start the daemon     : rewrite-it"
echo "    Rewrite from terminal: echo 'Hello world.' | rewrite-it rewrite"
echo "    Check model status   : rewrite-it status"
echo "    Pre-download model   : rewrite-it setup"
echo "    Keyboard shortcut    : $SHORTCUT_KEY  (select text first, then press)"
