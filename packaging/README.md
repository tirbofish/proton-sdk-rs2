# Packaging

Each package installs `pdcli`, `pdcli-gui`, a desktop launcher and a systemd
user unit at `/usr/lib/systemd/user/pdcli.service`. Installing a package does
not enable that unit.

The commands below build from a tagged release. Set `VERSION` to the release
you are packaging:

```sh
VERSION=0.5.0
```

## Debian / Ubuntu

```sh
sudo apt install build-essential debhelper-compat cargo rustc pkg-config protobuf-compiler \
  libdbus-1-dev libgtk-3-dev libgtk-4-dev libadwaita-1-dev libxdo-dev \
  libayatana-appindicator3-dev libfuse3-dev
dpkg-buildpackage -us -uc -b
sudo apt install ../pdcli_${VERSION}-1_$(dpkg --print-architecture).deb
```

## Fedora / RHEL

```sh
sudo dnf install rpm-build cargo rust protobuf-compiler dbus-devel gtk3-devel \
  gtk4-devel libadwaita-devel libX11-xcb-devel libxdo-devel fuse3-devel systemd-rpm-macros
git archive --format=tar.gz --prefix=pdcli-${VERSION}/ v${VERSION} \
  > ~/rpmbuild/SOURCES/pdcli-${VERSION}.tar.gz
rpmbuild -ba packaging/rpm/pdcli.spec
sudo dnf install ~/rpmbuild/RPMS/$(rpm --eval '%{_arch}')/pdcli-${VERSION}-1*.rpm
```

## Arch Linux

The recipe fetches the matching GitHub tag.

```sh
(cd packaging/arch && makepkg -Cfs)
sudo pacman -U packaging/arch/pdcli-${VERSION}-1-$(uname -m).pkg.tar.zst
```

## Starting at login

Sign in once with `pdcli login`, then enable the user unit:

```sh
systemctl --user enable --now pdcli.service
```

To start before anyone logs in, run `loginctl enable-linger "$USER"`. The
daemon may then start before the desktop keyring is unlocked, and it will
restart until credentials are available.

## Troubleshooting

- **`fusermount3` or `/dev/fuse` errors.** `fuse3` is missing, or your user
  can't access FUSE. Run `pdcli stop` to clear an old mount.
- **Unit not found.** Run `systemctl --user daemon-reload` and check that a
  systemd user manager is running.
- **Daemon keeps restarting.** Read `journalctl --user -u pdcli.service -e` and
  check that the keyring is available.

## Releases

Maintainers release from `main` with **Actions → Release → Run workflow**:

- `patch`, `minor` or `major` bumps the version in Cargo, Debian, RPM and Arch
  metadata and `CHANGELOG.md`, then commits and tags `vX.Y.Z`.
- `none` tags the current version without changing files.
- **Dry run** builds everything without committing, tagging or publishing.
- **Publish crates** publishes `proton-sdk-rs2`, `proton-drive-sdk` and `pdcli`
  to crates.io. This needs a `CARGO_REGISTRY_TOKEN` repository secret.

The workflow builds amd64 and arm64 binaries, `.deb` packages and a source
tarball, and attaches them to a GitHub release. To preview the version bump
locally, run `python3 scripts/prepare-release.py --bump patch`.
