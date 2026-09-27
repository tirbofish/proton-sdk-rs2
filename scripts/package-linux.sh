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
cp "$root/target/release/pdcli-gui" "$dist/pdcli-gui-${target}"
chmod +x "$dist/pdcli-gui-${target}"
strip "$dist/pdcli-gui-${target}" || true

arch="$(dpkg --print-architecture)"
pkg="$dist/deb-root"
rm -rf "$pkg"
mkdir -p "$pkg/usr/bin" "$pkg/usr/lib/systemd/user" "$pkg/DEBIAN"
install -Dm755 "$root/target/release/pdcli" "$pkg/usr/bin/pdcli"
install -Dm755 "$root/target/release/pdcli-gui" "$pkg/usr/bin/pdcli-gui"
install -Dm644 "$root/packaging/io.github.tirbofish.pdcli.desktop" "$pkg/usr/share/applications/io.github.tirbofish.pdcli.desktop"
install -Dm644 "$root/packaging/systemd/pdcli.service" "$pkg/usr/lib/systemd/user/pdcli.service"
shlib_deps="$(dpkg-shlibdeps -O -e"$pkg/usr/bin/pdcli" -e"$pkg/usr/bin/pdcli-gui" | sed -n 's/^shlibs:Depends=//p')"
cat > "$pkg/DEBIAN/control" <<EOF
Package: pdcli
Version: ${version}-1
Section: net
Priority: optional
Architecture: ${arch}
Maintainer: pdcli contributors <4tkbytes@pm.me>
Depends: fuse3, ${shlib_deps}
Homepage: https://github.com/tirbofish/proton-sdk-rs2
Description: Unofficial Proton Drive desktop and command-line client
 pdcli mounts Proton Drive through FUSE and provides a GNOME desktop
 window, command-line controls, and a background synchronization daemon.
EOF
dpkg-deb --root-owner-group --build "$pkg" "$dist/pdcli_${version}-1_${arch}.deb"
git -C "$root" archive --format=tar.gz --prefix="pdcli-${version}/" HEAD > "$dist/pdcli-${version}.tar.gz"
