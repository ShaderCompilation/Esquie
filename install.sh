#!/usr/bin/env bash
# Install or update Esquie for the current user (no root needed).
#
#   ./install.sh              build + install (or update)
#   ./install.sh --uninstall  remove everything this script installed
#
# Installs:
#   ~/.local/bin/esquie
#   ~/.local/share/applications/esquie.desktop
#   ~/.local/share/icons/hicolor/scalable/apps/esquie.svg
set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")"

DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}"
BIN_DIR="$HOME/.local/bin"
BIN="$BIN_DIR/esquie"
DESKTOP="$DATA/applications/esquie.desktop"
ICON="$DATA/icons/hicolor/scalable/apps/esquie.svg"
AUTOSTART="$CONFIG/autostart/esquie.desktop"

refresh_caches() {
    command -v update-desktop-database >/dev/null && update-desktop-database -q "$DATA/applications" || true
    command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t "$DATA/icons/hicolor" || true
}

if [[ "${1:-}" == "--uninstall" ]]; then
    pkill -x esquie && echo "Stopped running Esquie." || true
    rm -f "$BIN" "$DESKTOP" "$ICON" "$AUTOSTART"
    refresh_caches
    echo "Esquie uninstalled."
    exit 0
fi

echo "Building release binary..."
cargo build --release --locked

# Was it running? Then restart it on the new version afterwards.
was_running=false
pgrep -x esquie >/dev/null && was_running=true

# `install` replaces the file rather than writing into it, so this is safe
# even while the old binary is running.
install -Dm755 target/release/esquie "$BIN"
install -Dm644 ui/icon.svg "$ICON"

mkdir -p "$(dirname "$DESKTOP")"
cat >"$DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Name=Esquie
Comment=Noise removal for your microphone
Exec=$BIN
Icon=esquie
Terminal=false
Categories=AudioVideo;Audio;Utility;
Keywords=noise;microphone;mic;suppression;denoise;
StartupNotify=false
EOF

# If "Start on login" was enabled from a dev build, point it at the installed binary.
if [[ -f "$AUTOSTART" ]]; then
    sed -i "s|^Exec=.*|Exec=\"$BIN\" --hidden|" "$AUTOSTART"
fi

refresh_caches

if $was_running; then
    echo "Restarting Esquie..."
    pkill -x esquie || true
    for _ in {1..20}; do pgrep -x esquie >/dev/null || break; sleep 0.1; done
    setsid -f "$BIN" --hidden >/dev/null 2>&1 </dev/null
fi

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
echo "Esquie $version installed to $BIN"
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "Note: $BIN_DIR is not on your PATH (launching from the app menu still works)." ;;
esac
