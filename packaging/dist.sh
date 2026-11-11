#!/bin/sh
# Packs a built aarch64 binary into the release archive that install.sh expects.
#   packaging/dist.sh BINARY OUTDIR   ->   OUTDIR/framelight-aarch64-linux.tar.gz
set -eu
[ $# -eq 2 ] || {
	echo "usage: $0 BINARY OUTDIR" >&2
	exit 2
}
root=$(cd "$(dirname "$0")/.." && pwd)
bin=$1
out=$2

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/framelight" "$out"
install -m755 "$bin" "$stage/framelight/framelight"
install -m755 "$root/packaging/framelight-app.sh" "$stage/framelight/framelight-app.sh"
install -m644 "$root/assets/framelight.png" "$stage/framelight/framelight.png"
tar -C "$stage" --owner=0 --group=0 -czf "$out/framelight-aarch64-linux.tar.gz" framelight
echo "$out/framelight-aarch64-linux.tar.gz"
