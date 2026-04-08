#!/usr/bin/env bash
# install.sh — one-shot installer for rewrite-it.
#
# Safe to run multiple times: each run stops any running daemon, overwrites all
# installed files, and re-registers the DBus / systemd / shortcut entries.
#
# Builds the release binary, installs it together with the DBus activation
# service, the clipboard helper script, and registers keyboard shortcuts
# for rewriting with different styles (grammar, formal, concise, etc.).
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

# ── Stop any running daemon before overwriting the binary ─────────────────────
if command -v systemctl &>/dev/null && systemctl --user is-active --quiet rewrite-it.service 2>/dev/null; then
    echo "==> Stopping running rewrite-it daemon…"
    systemctl --user stop rewrite-it.service || true
fi

# ── Install binary ────────────────────────────────────────────────────────────
INSTALL_DIR="$HOME/.local/bin"
mkdir -p "$INSTALL_DIR"
cp -f "$BINARY" "$INSTALL_DIR/rewrite-it"
chmod +x "$INSTALL_DIR/rewrite-it"
echo "    binary  → $INSTALL_DIR/rewrite-it"

# ── Install clipboard helper ──────────────────────────────────────────────────
cp -f "$REPO_ROOT/assets/rewrite-it-selection" "$INSTALL_DIR/rewrite-it-selection"
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

# Reload dbus-broker so it picks up the new activation file immediately.
# Classic dbus-daemon re-scans on first activation attempt, but dbus-broker
# (used on Fedora, Arch, and other modern distros) requires an explicit reload.
if systemctl --user is-active --quiet dbus-broker.service 2>/dev/null; then
    systemctl --user reload dbus-broker.service
    echo "    systemctl --user reload dbus-broker.service  ✓"
fi

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
    # Re-start the service if it was previously enabled
    if systemctl --user is-enabled --quiet rewrite-it.service 2>/dev/null; then
        systemctl --user restart rewrite-it.service
        echo "    systemctl --user restart rewrite-it.service  ✓"
    else
        echo "    To enable auto-start on login: systemctl --user enable --now rewrite-it.service"
    fi
else
    echo "    (systemd user session not running; reload manually after login)"
fi

# ── Remove legacy KDE service menu (no longer shipped) ────────────────────────
KDE_MENU_FILE="$HOME/.local/share/kio/servicemenus/rewrite-it.desktop"
if [ -f "$KDE_MENU_FILE" ]; then
    rm -f "$KDE_MENU_FILE"
    echo "    removed legacy KDE service menu: $KDE_MENU_FILE"
fi

# ── Keyboard shortcut ─────────────────────────────────────────────────────────
echo ""
echo "==> Registering keyboard shortcut…"

SHORTCUT_CMD="$INSTALL_DIR/rewrite-it-selection grammar"
# Default shortcuts (all verified free on stock KDE Plasma 6):
#   Meta+Shift+G → grammar   Meta+Shift+F → formal   Meta+Shift+C → concise
# Additional styles (casual, elaborate, creative) are registered with no default
# key — users can bind them in System Settings → Shortcuts → KWin.
SHORTCUT_KEY="Meta+Shift+G"

if command -v kwriteconfig6 &>/dev/null || command -v kwriteconfig5 &>/dev/null; then
    # KDE Plasma 6 global shortcut via KWin script.
    #
    # kglobalshortcutsrc + khotkeysrc do NOT execute shell commands in Plasma 6
    # (KHotKeys was removed).  kglobalshortcutsrc entries are only active when a
    # running application has registered the component with kglobalaccel.
    #
    # A KWin script registers shortcuts at the compositor level — always
    # grabbed, works on X11 and Wayland — and calls our DBus service directly.
    # The daemon copies the rewritten text to the clipboard for manual paste.
    KWIN_PKG_TOOL="$(command -v kpackagetool6 2>/dev/null || command -v kpackagetool5 2>/dev/null || true)"
    if [ -n "$KWIN_PKG_TOOL" ]; then
        # Clean up stale shortcut entries from previous installs.
        > ~/.config/khotkeysrc 2>/dev/null || true
        sed -i '/^\[rewrite-it\]/,/^$/d' ~/.config/kglobalshortcutsrc 2>/dev/null || true
        # Remove stale kglobalaccel components that may block key re-registration.
        if command -v gdbus &>/dev/null; then
            for _comp in rewrite_it rewrite_it_desktop; do
                gdbus call --session --dest org.kde.kglobalaccel \
                    --object-path "/component/$_comp" \
                    --method org.kde.kglobalaccel.Component.cleanUp 2>/dev/null || true
            done
        fi

        # Upgrade if already installed; install fresh otherwise.
        # Always remove first (handles corrupt/outdated installs) then reinstall.
        rm -rf "$HOME/.local/share/kwin/scripts/rewrite-it-shortcut"
        "$KWIN_PKG_TOOL" --type=KWin/Script --install \
            "$REPO_ROOT/assets/rewrite-it-kwin" 2>/dev/null \
            && echo "    KWin script installed: rewrite-it-shortcut"

        # Enable the script in kwinrc (without this KWin never loads it).
        kwriteconfig6 --file kwinrc --group Plugins \
            --key "kwinscript_rewrite-it-shortcutEnabled" true 2>/dev/null || true

        # Tell the running KWin to unload, reload and start the script so all
        # shortcuts (grammar, formal, concise, casual, elaborate, creative) are
        # registered immediately without needing a logout.
        _qdbus="$(command -v qdbus6 2>/dev/null || command -v qdbus-qt6 2>/dev/null || command -v qdbus 2>/dev/null || true)"
        _script_js="$HOME/.local/share/kwin/scripts/rewrite-it-shortcut/contents/code/main.js"
        if [ -n "$_qdbus" ] && [ -f "$_script_js" ]; then
            "$_qdbus" org.kde.KWin /Scripting org.kde.kwin.Scripting.unloadScript \
                "rewrite-it-shortcut" 2>/dev/null || true
            "$_qdbus" org.kde.KWin /KWin reconfigure 2>/dev/null || true
            "$_qdbus" org.kde.KWin /Scripting org.kde.kwin.Scripting.loadScript \
                "$_script_js" "rewrite-it-shortcut" 2>/dev/null >/dev/null || true
            "$_qdbus" org.kde.KWin /Scripting org.kde.kwin.Scripting.start 2>/dev/null || true
        fi
    else
        echo "    ⚠  kpackagetool6 not found; KWin script not installed."
        echo "       Install it manually:"
        echo "         kpackagetool6 --type=KWin/Script --install $REPO_ROOT/assets/rewrite-it-kwin"
    fi

    echo "    KDE shortcuts registered:"
    echo "      Meta+Shift+G → grammar   Meta+Shift+F → formal   Meta+Shift+C → concise"
    echo "    (Rebind or add casual/elaborate/creative in System Settings → Shortcuts → KWin)"
elif command -v gsettings &>/dev/null && gsettings list-schemas 2>/dev/null | grep -q 'org.gnome.settings-daemon'; then
    # GNOME: add a custom keybinding
    BINDING_PATH='/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/rewrite-it/'
    gsettings set org.gnome.settings-daemon.plugins.media-keys custom-keybindings \
        "['${BINDING_PATH}']"
    gsettings set "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:${BINDING_PATH}" name    'Help me rewrite'
    gsettings set "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:${BINDING_PATH}" command "$SHORTCUT_CMD"
    gsettings set "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:${BINDING_PATH}" binding '<Super><Shift>g'
    echo "    GNOME shortcut registered: Super+Shift+G → $SHORTCUT_CMD"
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
echo "    Keyboard shortcuts   : Meta+Shift+G (grammar), +F (formal), +C (concise)"
echo ""
echo "    To uninstall: bash $REPO_ROOT/uninstall.sh"
