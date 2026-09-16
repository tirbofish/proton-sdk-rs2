# proton-sdk-rs2

`proton-sdk-rs2` is a community Rust port of the Proton Drive SDK. It is not
an official Proton product. The `pdcli` binary is a Linux FUSE client and
desktop utility built on top of the SDK.

This software can modify remote files and is still evolving. Keep an
independent backup of important data and review sync/conflict output before
using it on a primary copy.

## Build from source

The CLI currently targets Linux. Install the native build dependencies first.

Debian/Ubuntu:

```sh
sudo apt update
sudo apt install build-essential cargo rustc pkg-config protobuf-compiler \
  libdbus-1-dev libgtk-3-dev libxdo-dev libayatana-appindicator3-dev libfuse3-dev
```

Arch Linux:

```sh
sudo pacman -S --needed base-devel rust cargo protobuf pkgconf gtk3 libxdo \
  libayatana-appindicator libfuse3
```

Then build or install the CLI:

```sh
git clone https://github.com/tirbofish/proton-sdk-rs2
cd proton-sdk-rs2
cargo build --locked --release -p pdcli
cargo install --locked --path crates/pdcli
```

Package recipes for Debian, RPM-based systems, and Arch are in
[`packaging/README.md`](packaging/README.md).

## pdcli quick start

```sh
pdcli login                 # browser-based sign-in
pdcli mount                 # mounts ~/ProtonDrive
ls ~/ProtonDrive/MyFiles
pdcli status --json
```

`pdcli mount` also signs in when needed. On WSL, an invocation without a
subcommand defaults to mounting; use `pdcli gui` for the desktop window.

The main commands are documented by `pdcli --help` and include:

```text
pdcli status [--json]
pdcli retry <ID>
pdcli retry --all
pdcli pause
pdcli resume
pdcli sync
pdcli open
pdcli stop
pdcli logout
pdcli computers
pdcli computers register [--name NAME] [--bind DEVICE_ID]
pdcli computers sync PATH [--name NAME] [--dry-run]
pdcli computers restore COMPUTER FOLDER PATH
pdcli computers unsync JOB
pdcli share link NODE_UID [--role viewer|editor] [--password PASSWORD]
pdcli share status NODE_UID [--json]
pdcli share remove NODE_UID
pdcli share report NODE_UID --category CATEGORY --bona-fide
pdcli takeout DESTINATION
```

`pdcli status` reports login, daemon, mount, and failed/pending journal state.
Use `pdcli retry ID` or `pdcli retry --all` after inspecting a failed entry.

Computer sync validates paths, refuses to sync the Proton Drive mount itself,
does not delete files, and preserves the overwritten side when local and
remote versions differ as timestamped `pdcli conflict` copies. Run
`pdcli computers sync PATH --dry-run` first; it
does not create a device, folder, or job.

`pdcli takeout DESTINATION` exports **My Files** to a local directory and
resumes from `.pdcli-takeout-manifest.json`. It currently skips Photos and
degraded/unsupported nodes and does not export Computers backups.

## Start the daemon at login

For a source install, generate a systemd user unit for the current executable:

```sh
pdcli service enable
pdcli service status
```

`pdcli service enable` installs the generated unit, reloads systemd, and starts
it. `pdcli service install` only writes the unit. `pdcli service disable` stops
it, and `pdcli service uninstall` removes the generated unit. Package installs
provide `/usr/lib/systemd/user/pdcli.service`:

```sh
systemctl --user daemon-reload
systemctl --user enable --now pdcli.service
journalctl --user -u pdcli.service -e
```

User services normally start when the user manager starts at login. For a
headless machine, `loginctl enable-linger "$USER"` starts the user manager at
boot, but a lingering service may start before a desktop Secret Service/keyring
is available. Sign in once first and ensure the account's credentials are
available to that user; otherwise systemd may restart the daemon until login.

## Credentials, cache, and security

On Linux the default data directory is `$XDG_CONFIG_HOME/pdcli`, or
`$HOME/.config/pdcli` when `XDG_CONFIG_HOME` is unset. Other platforms follow
their native application-data directory. The directory contains the FUSE
database (encrypted `fuse.db`), SDK caches (`cache.db` and encrypted
`secret.db`), Computer sync state (`computers.json`), and the file cache
(`fuse_cache`).

Session tokens and the cache master key are stored in the OS keyring under the
`pdcli` service when available. If no keyring is available, pdcli falls back to
`cred.ron` and `cache.key` with restrictive permissions (`0600` on Unix).
Passwords are not persisted by pdcli, but session tokens can authorize the
account; protect the account keyring and configuration directory. `pdcli logout`
stops the daemon and removes the stored session credentials; it does not erase
cached files.

## SDK compatibility

For crates.io consumers:

```toml
proton-drive-sdk = "0.3"
```

For the repository version:

```toml
proton-drive-sdk = { git = "https://github.com/tirbofish/proton-sdk-rs2" }
```

`proton-drive-sdk` re-exports the lower-level `proton-sdk-rs2` crate, so most
applications do not need to add a second direct SDK dependency.

The current upstream compatibility reference is the fetched
`ProtonDriveApps/sdk` baseline `6cbf2f44` (15 September 2026). This Rust port
is not a drop-in guarantee for every upstream API change, so pin a crate
version or commit in applications that need reproducible builds. Upstream's
next cryptographic migration is expected around late 2026/early 2027; review
the migration notes and test existing stored credentials before upgrading
through that boundary.

## Troubleshooting

- `fusermount3` or `/dev/fuse` errors: install `fuse3` and confirm the user is
  allowed to access FUSE; then run `pdcli stop` before retrying.
- A daemon started by systemd but immediately restarts: inspect
  `journalctl --user -u pdcli.service -e`, check `pdcli status`, and verify the
  keyring/credential availability described above.
- A sync operation is not progressing: run `pdcli status --json`, inspect the
  failed journal entries, and retry a specific ID before using `--all`.
- Enable diagnostic logging for one invocation with
  `RUST_LOG=pdcli=debug pdcli status`.
- Tests that exercise the native keyring may require a running desktop Secret
  Service/DBus session; this is an environment requirement, not a second
  credential store.

## License

MIT. See [`LICENSE.md`](LICENSE.md).
