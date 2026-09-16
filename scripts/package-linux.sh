#!/usr/bin/env bash
set -euo pipefail

version="${1:?usage: package-linux.sh VERSION TARGET}"
target="${2:?usage: package-linux.sh VERSION TARGET}"
root="$(cd "$(dirname "$0")/.." && pwd)"
dist="$root/dist"
mkdir -p "$dist"

cp "$root/target/release/pdcli" "$dist/pdcli-${target}"
chmod +x "$dist/pdcli-${target}"
strip "$dist/pdcli-${target}" || true

arch="$(dpkg --print-architecture)"
pkg="$dist/deb-root"
rm -rf "$pkg"
mkdir -p "$pkg/usr/bin" "$pkg/usr/lib/systemd/user" "$pkg/DEBIAN"
install -Dm755 "$root/target/release/pdcli" "$pkg/usr/bin/pdcli"
install -Dm644 "$root/packaging/systemd/pdcli.service" "$pkg/usr/lib/systemd/user/pdcli.service"
cat > "$pkg/DEBIAN/control" <<EOF
Package: pdcli
Version: ${version}-1
Section: net
Priority: optional
Architecture: ${arch}
Maintainer: pdcli contributors <4tkbytes@pm.me>
Depends: fuse3
Homepage: https://github.com/tirbofish/proton-sdk-rs2
Description: Proton Drive command-line client and filesystem daemon
 pdcli mounts Proton Drive through FUSE and provides command-line
 controls for authentication, synchronization, and the background daemon.
EOF
dpkg-deb --root-owner-group --build "$pkg" "$dist/pdcli_${version}-1_${arch}.deb"
git -C "$root" archive --format=tar.gz --prefix="pdcli-${version}/" HEAD > "$dist/pdcli-${version}.tar.gz"
