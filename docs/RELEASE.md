# pdcli 0.3.2

This release is a Linux-only, community build of the Proton Drive client. It
mounts `~/ProtonDrive` through FUSE, with `MyFiles/` and `Computers/` roots.
It is not an official Proton product; keep an independent backup of important
data.

## Install and first run

For a release binary:

```sh
chmod +x pdcli-x86_64-unknown-linux-gnu
./pdcli-x86_64-unknown-linux-gnu login
./pdcli-x86_64-unknown-linux-gnu mount
```

Native build dependencies and package-manager instructions are in
[`packaging/README.md`](../packaging/README.md). The source quick start and
security/storage notes are in the [root README](../README.md).

## New CLI surfaces

```text
pdcli status --json
pdcli retry ID
pdcli retry --all
pdcli computers sync PATH --dry-run
pdcli share link NODE_UID [--role viewer|editor] [--password PASSWORD]
pdcli share status NODE_UID [--json]
pdcli share remove NODE_UID
pdcli takeout DESTINATION
pdcli service install
pdcli service enable
pdcli service status
pdcli service disable
pdcli service uninstall
```

Computer sync does not delete either side. When local and remote versions
differ, pdcli preserves the overwritten side as a timestamped conflict copy.
`takeout` is a
resumable export of My Files only; it skips Photos, degraded/unsupported
nodes, and Computers backups.

## Background service

`pdcli service install` writes a systemd **user** unit pointing at the current
executable. Package installs provide the equivalent unit at
`/usr/lib/systemd/user/pdcli.service`. Enable at login with:

```sh
systemctl --user daemon-reload
systemctl --user enable --now pdcli.service
```

For boot-before-login operation, an administrator can run
`loginctl enable-linger "$USER"`. A lingering service can start before a
desktop Secret Service/keyring is available, so sign in once first and verify
that the user can access the stored credentials. Use
`journalctl --user -u pdcli.service -e` for failures.

## Compatibility reference

The current upstream reference used for compatibility review is the fetched
`ProtonDriveApps/sdk` baseline `6cbf2f44` from 15 September 2026. The Rust SDK
port and pdcli are versioned independently from upstream; applications should
pin the crate version or commit when reproducibility matters.

Upstream's next cryptographic migration is expected around late 2026/early
2027. Treat that as a compatibility boundary: review migration notes and test
existing stored credentials before upgrading.

## Known limitations

- The takeout command exports My Files only and writes a manifest in the
  destination directory.
- Computer sync compares local and remote modification times, preserves
  conflict copies, and does not mirror deletes.
- `--password` values can appear in shell history.
- Native keyring tests need a running Secret Service/DBus environment.
