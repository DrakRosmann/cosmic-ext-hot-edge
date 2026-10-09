#!/bin/sh
# Build and install Hot Edge, for the current user (default) or for everyone.
#   ./install.sh              # in ~/.local; starts with the session from ~/.config/autostart
#   sudo ./install.sh /usr    # for everyone; starts from /etc/xdg/autostart
# From a release tarball (bin/ beside this script) nothing is built.
# Packagers: DESTDIR="$pkgdir" ./install.sh /usr --no-build
set -e
cd "$(dirname "$0")"
prefix="${1:-$HOME/.local}"
id=io.github.DrakRosmann.CosmicHotEdge
dest="${DESTDIR:-}"

if [ -x bin/cosmic-ext-hot-edge ]; then
    bin=bin
else
    bin=target/release
    if [ "$2" != "--no-build" ]; then
        ./deps.sh 1.93
        cargo build --release --locked
    fi
fi

# A user's own install starts from their autostart, with the full path: the
# session may not have ~/.local/bin on its PATH.
case "$prefix" in
    "$HOME"/*) autostart="$HOME/.config/autostart"; exec_dir="$prefix/bin/" ;;
    *) autostart="/etc/xdg/autostart"; exec_dir= ;;
esac

install -Dm0755 "$bin/cosmic-ext-hot-edge" "$dest$prefix/bin/cosmic-ext-hot-edge"
install -Dm0755 "$bin/cosmic-ext-hot-edge-daemon" "$dest$prefix/bin/cosmic-ext-hot-edge-daemon"
mkdir -p "$dest$prefix/share/applications" "$dest$autostart"
sed "s|^Exec=|Exec=$exec_dir|" "data/$id.desktop" > "$dest$prefix/share/applications/$id.desktop"
sed "s|^Exec=|Exec=$exec_dir|" "data/$id.Daemon.desktop" > "$dest$autostart/$id.Daemon.desktop"
install -Dm0644 "data/icons/scalable/apps/$id.svg" "$dest$prefix/share/icons/hicolor/scalable/apps/$id.svg"
install -Dm0644 "data/$id.metainfo.xml" "$dest$prefix/share/metainfo/$id.metainfo.xml"

if [ -z "$dest" ]; then
    # On now, without logging in again (the settings app also does this).
    if [ "$XDG_CURRENT_DESKTOP" = COSMIC ] && [ "$(id -u)" != 0 ]; then
        nohup "$prefix/bin/cosmic-ext-hot-edge-daemon" >/dev/null 2>&1 &
    fi
    echo "Installed. Open \"Hot Edge\" from the app library to set it up."
fi
