# pdcli packages

The package recipes expect a tagged source tree and build the `pdcli` binary
with Cargo. They install `/usr/bin/pdcli` and a systemd **user** unit at
`/usr/lib/systemd/user/pdcli.service`; package installation does not enable or
start that unit automatically.

## Debian/Ubuntu

Install the build dependencies listed in `debian/control`, then build a local
package from the repository root:

```sh
sudo apt install build-essential cargo rustc debhelper-compat pkg-config protobuf-compiler \
  libdbus-1-dev libgtk-3-dev libxdo-dev libayatana-appindicator3-dev libfuse3-dev
dpkg-buildpackage -us -uc -b
sudo apt install ../pdcli_0.3.0-1_$(dpkg --print-architecture).deb
```

## Fedora/RHEL-like systems

Create a source archive from the tagged checkout and build the RPM:

```sh
sudo dnf install rpm-build cargo rust protobuf-compiler \
  dbus-devel gtk3-devel libX11-xcb-devel libxdo-devel fuse3-devel \
  systemd-rpm-macros
git archive --format=tar.gz --prefix=pdcli-0.3.0/ v0.3.0 > ~/rpmbuild/SOURCES/pdcli-0.3.0.tar.gz
rpmbuild -ba packaging/rpm/pdcli.spec
sudo dnf install ~/rpmbuild/RPMS/$(rpm --eval '%{_arch}')/pdcli-0.3.0-1*.rpm
```

## Arch Linux

Run `makepkg` in `packaging/arch` (the recipe fetches the matching GitHub
tag), then install the resulting package:

```sh
(cd packaging/arch && makepkg -Cfs)
sudo pacman -U packaging/arch/pdcli-0.3.0-1-$(uname -m).pkg.tar.zst
```

## Starting at login

Package installation does not enable background mounts automatically. After
signing in once with `pdcli login`, enable the user unit:

```sh
systemctl --user daemon-reload
systemctl --user enable --now pdcli.service
systemctl --user status pdcli.service
```

For a headless machine that must start before an interactive login, enable a
user manager for the account (this requires administrator permission):

```sh
loginctl enable-linger "$USER"
```

Linger can start the daemon before a desktop Secret Service/keyring exists.
Sign in once first and ensure the user's credentials are available to that
same user; otherwise the service may restart until login. Inspect failures with

```sh
journalctl --user -u pdcli.service -e
pdcli status --json
```

For source installs, `pdcli service install` writes a unit pointing at the
currently running executable. `pdcli service enable` installs, reloads, and
starts it; the separate reload form is:

```sh
pdcli service install
pdcli service reload
pdcli service enable
```

## Runtime data and recovery

On Linux, pdcli stores configuration, session fallback files, SDK caches
(including an encrypted secret cache), the encrypted FUSE database, Computer
jobs, and the file cache under
`$XDG_CONFIG_HOME/pdcli` or `$HOME/.config/pdcli`. Session tokens and the cache
master key use the OS keyring when available; fallback files are locked down
to `0600` on Unix. Protect this directory and the keyring because session
tokens are sensitive.

Computer sync is additive and creates timestamped conflict copies when local
and remote versions differ rather than deleting either version. Check a job with
`pdcli computers sync PATH --dry-run`
before creating it. Failed daemon journal entries remain visible in
`pdcli status --json`; retry a specific entry with `pdcli retry ID` or all
failed entries with `pdcli retry --all`.

The takeout command is a resumable My Files export only:

```sh
pdcli takeout ~/ProtonDrive-takeout
```

It writes `.pdcli-takeout-manifest.json` in the destination and currently skips
Photos, degraded/unsupported nodes, and Computers backups.

## Troubleshooting packages

- `fusermount3` or `/dev/fuse` errors usually mean `fuse3` is missing or the
  user lacks FUSE access. Stop an old mount with `pdcli stop`.
- If the package unit is not found, run `systemctl --user daemon-reload`; if it
  still fails, check that a systemd user manager is running.
- If the daemon restarts, use `journalctl --user -u pdcli.service -e` and check
  keyring availability, especially with linger.
- Passwords are not stored by pdcli, but `--password` public-link arguments may
  remain in shell history.
