#!/usr/bin/env bash
# uninstall.sh — remove everything installed by install.sh.
#
# Does NOT remove:
#   - The downloaded model file (~/.local/share/rewrite-it/models/)
#     Pass --purge to also delete the model (large file, may need re-download).
#   - The config file (~/.config/rewrite-it/config.toml)
#     Pass --purge to also delete config.
#
# Usage:
#   bash uninstall.sh [--purge]

set -euo pipefail

PURGE=false
for arg in "$@"; do
    case "$arg" in
        --purge) PURGE=true ;;
        --help|-h)
            echo "Usage: bash uninstall.sh [--purge]"
            echo "  --purge   Also delete the model file and config"
            exit 0 ;;
    esac
done

INSTALL_DIR="$HOME/.local/bin"
DBUS_SVC_DIR="$HOME/.local/share/dbus-1/services"
SYSTEMD_USER_DIR="$HOME/.config/systemd/user"
KDE_MENU_DIR="$HOME/.local/share/kio/servicemenus"
KGLOBALACCEL_DIR="$HOME/.local/share/kglobalaccel"
SHORTCUT_KEY="Meta+Shift+G"

echo "==> Stopping rewrite-it daemon…"
if command -v systemctl &>/dev/null; then
    systemctl --user stop    rewrite-it.service 2>/dev/null || true
    systemctl --user disable rewrite-it.service 2>/dev/null || true
fi
# Also stop any daemon started without systemd
if command -v rewrite-it &>/dev/null; then
    pkill -f "rewrite-it daemon" 2>/dev/null || true
fi

echo "==> Removing installed files…"

rm_if() {
    if [ -e "$1" ]; then
        rm -f "$1"
        echo "    removed $1"
    fi
}

rm_if "$INSTALL_DIR/rewrite-it"
rm_if "$INSTALL_DIR/rewrite-it-selection"
rm_if "$DBUS_SVC_DIR/org.rewriteit.Rewriter1.service"
rm_if "$SYSTEMD_USER_DIR/rewrite-it.service"
rm_if "$KDE_MENU_DIR/rewrite-it.desktop"
rm_if "$KGLOBALACCEL_DIR/rewrite-it.desktop"

# ── Remove KWin script (global shortcut) ─────────────────────────────────────
KWIN_PKG_TOOL="$(command -v kpackagetool6 2>/dev/null || command -v kpackagetool5 2>/dev/null || true)"
if [ -n "$KWIN_PKG_TOOL" ]; then
    if "$KWIN_PKG_TOOL" --type=KWin/Script --list 2>/dev/null | grep -q "rewrite-it-shortcut"; then
        # Disable the script first so KWin stops grabbing the shortcut.
        kwriteconfig6 --file kwinrc --group Plugins \
            --key "kwinscript_rewrite-it-shortcutEnabled" false 2>/dev/null || true
        "$KWIN_PKG_TOOL" --type=KWin/Script --remove rewrite-it-shortcut 2>/dev/null || true
        echo "    KWin script removed: rewrite-it-shortcut"
        # Tell the running KWin to reconfigure (unregisters the shortcut).
        _qdbus="$(command -v qdbus6 2>/dev/null || command -v qdbus-qt6 2>/dev/null || command -v qdbus 2>/dev/null || true)"
        [ -n "$_qdbus" ] && "$_qdbus" org.kde.KWin /KWin reconfigure 2>/dev/null || true
    fi
fi

# ── Remove GNOME keybinding ───────────────────────────────────────────────────
if command -v gsettings &>/dev/null && gsettings list-schemas 2>/dev/null | grep -q 'org.gnome.settings-daemon'; then
    BINDING_PATH='/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/rewrite-it/'
    CURRENT="$(gsettings get org.gnome.settings-daemon.plugins.media-keys custom-keybindings 2>/dev/null || echo '[]')"
    if echo "$CURRENT" | grep -qF "rewrite-it"; then
        gsettings set org.gnome.settings-daemon.plugins.media-keys custom-keybindings \
            "$(echo "$CURRENT" | sed "s|'${BINDING_PATH}'||g; s|, ,|,|g; s|\[, |[|g; s|, \]|]|g")"
        echo "    GNOME shortcut removed"
    fi
fi

# ── Reload systemd ────────────────────────────────────────────────────────────
if command -v systemctl &>/dev/null && systemctl --user is-system-running &>/dev/null 2>&1; then
    systemctl --user daemon-reload
    echo "    systemctl --user daemon-reload  ✓"
fi

# ── Optional: purge model and config ─────────────────────────────────────────
if [ "$PURGE" = true ]; then
    MODEL_DIR="$HOME/.local/share/rewrite-it"
    CONFIG_DIR="$HOME/.config/rewrite-it"

    if [ -d "$MODEL_DIR" ]; then
        rm -rf "$MODEL_DIR"
        echo "    purged model directory: $MODEL_DIR"
    fi
    if [ -d "$CONFIG_DIR" ]; then
        rm -rf "$CONFIG_DIR"
        echo "    purged config directory: $CONFIG_DIR"
    fi
else
    echo ""
    echo "    Model file and config were NOT removed."
    echo "    Run with --purge to also delete them."
fi

echo ""
echo "==> rewrite-it uninstalled."
