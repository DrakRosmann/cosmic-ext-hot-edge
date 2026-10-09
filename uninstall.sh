#!/bin/sh
# Remove what install.sh installed, with the same prefix.
#   ./uninstall.sh            # from ~/.local
#   sudo ./uninstall.sh /usr
set -e
prefix="${1:-$HOME/.local}"
id=io.github.DrakRosmann.CosmicHotEdge
case "$prefix" in
    "$HOME"/*) autostart="$HOME/.config/autostart" ;;
    *) autostart="/etc/xdg/autostart" ;;
esac
pkill -f "(^|/)cosmic-ext-hot-edge-daemon$" 2>/dev/null || true
rm -f "$prefix/bin/cosmic-ext-hot-edge" "$prefix/bin/cosmic-ext-hot-edge-daemon" \
    "$prefix/share/applications/$id.desktop" "$autostart/$id.Daemon.desktop" \
    "$prefix/share/icons/hicolor/scalable/apps/$id.svg" "$prefix/share/metainfo/$id.metainfo.xml"
echo "Removed. Your settings stay in ~/.config/cosmic/$id."
