#!/bin/sh
# Installs framelight on a Steam Frame (stock SteamOS, aarch64), for the current user
# only: no sudo, nothing outside the home directory.
#
#   curl -fsSL https://github.com/VolanDeVovan/framelight/releases/latest/download/install.sh | sh
#
# Running it again updates. Also:
#   sh install.sh ARCHIVE       install from a local framelight-aarch64-linux.tar.gz
#   sh install.sh --uninstall   remove everything this script put in place
#   FRAMELIGHT_VERSION=v0.1.0   install that release instead of the latest one
set -eu

REPO=VolanDeVovan/framelight
ARCHIVE_NAME=framelight-aarch64-linux.tar.gz
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
APP="$DATA/framelight"
ENTRY="$DATA/applications/framelight.desktop"

say() { printf '%s\n' "$*"; }
die() {
	printf 'framelight: %s\n' "$*" >&2
	exit 1
}

if [ "${1:-}" = "--uninstall" ]; then
	pkill -x framelight 2>/dev/null || true
	rm -rf "$APP" "$ENTRY"
	say "framelight removed."
	exit 0
fi

# FRAMELIGHT_ANY_ARCH=1 lets the packaging be tested on another machine.
if [ "$(uname -m)" != aarch64 ] && [ -z "${FRAMELIGHT_ANY_ARCH:-}" ]; then
	die "this build is for the Steam Frame (aarch64), not $(uname -m)"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if [ -n "${1:-}" ]; then
	[ -f "$1" ] || die "no such archive: $1"
	cp "$1" "$tmp/$ARCHIVE_NAME"
else
	version="${FRAMELIGHT_VERSION:-latest}"
	if [ "$version" = latest ]; then
		url="https://github.com/$REPO/releases/latest/download/$ARCHIVE_NAME"
	else
		url="https://github.com/$REPO/releases/download/$version/$ARCHIVE_NAME"
	fi
	say "Downloading $url"
	curl -fL --progress-bar -o "$tmp/$ARCHIVE_NAME" "$url" || die "download failed"
fi

tar -xzf "$tmp/$ARCHIVE_NAME" -C "$tmp" || die "broken archive"
[ -x "$tmp/framelight/framelight" ] || die "the archive has no framelight binary"

# A running binary is never overwritten in place: the new one is moved over it, so a
# framelight window left open keeps working until it is closed.
mkdir -p "$APP" "$(dirname "$ENTRY")"
for f in framelight framelight-app.sh framelight.png; do
	cp "$tmp/framelight/$f" "$APP/.$f.new"
	mv -f "$APP/.$f.new" "$APP/$f"
done

# Steam lists every desktop entry in the user's application menu. Absolute paths,
# because desktop entries expand nothing.
cat >"$ENTRY.new" <<EOF
[Desktop Entry]
Type=Application
Name=Framelight
Comment=Stream the Headset View to browsers on the network
Exec=$APP/framelight-app.sh
Icon=$APP/framelight.png
Terminal=false
Categories=AudioVideo;Video;
StartupWMClass=framelight
EOF
mv -f "$ENTRY.new" "$ENTRY"
command -v update-desktop-database >/dev/null 2>&1 &&
	update-desktop-database -q "$(dirname "$ENTRY")" 2>/dev/null || true

installed=$("$APP/framelight" version 2>/dev/null || echo framelight)
say ""
say "Installed $installed into $APP"
say "Open \"Framelight\" from the app list, press Enable, then open"
say "  http://$(cat /proc/sys/kernel/hostname 2>/dev/null || echo steamos).local:8090"
say "in a browser on a TV or computer on the same network."
