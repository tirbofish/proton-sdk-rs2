# pdcli

`pdcli` is an unofficial Proton Drive client for Linux. It mounts the account
through FUSE at `~/ProtonDrive` and runs a background daemon for lazy reads,
uploads, event polling, and Computer sync.

## Quick start

```sh
pdcli login
pdcli mount
ls ~/ProtonDrive/MyFiles
pdcli status
```

The first login opens the browser. `pdcli mount` also logs in when no session
exists. On WSL, `pdcli` without a subcommand mounts; use `pdcli gui` for the
window. `--force-offline` uses local state only and `--no-tray` suppresses the
tray icon.

## Commands

```text
pdcli status [--json]
pdcli retry <ID>
pdcli retry --all
pdcli pause
pdcli resume
pdcli sync
pdcli open
pdcli stop                 # also: unmount
pdcli logout

pdcli computers
pdcli computers register [--name NAME] [--bind DEVICE_ID]
pdcli computers sync PATH [--name NAME] [--dry-run]
pdcli computers restore COMPUTER FOLDER PATH
pdcli computers unsync JOB

pdcli share link NODE_UID [--role viewer|editor] [--password PASSWORD]
pdcli share status NODE_UID [--json]
pdcli share remove NODE_UID
pdcli share report NODE_UID --category CATEGORY --bona-fide [--message TEXT] [--email EMAIL] [--revision UID] [--invitation UID]

pdcli takeout DESTINATION
```

`status --json` is intended for scripts and includes login, daemon, mount, and
journal information. Failed journal entries remain available for inspection;
retry one with `pdcli retry ID`, or retry all with `pdcli retry --all`.

Computer sync is additive: it does not delete local or remote files. When
local and remote versions differ, pdcli creates a timestamped `pdcli conflict`
copy and keeps the overwritten side. Use `--dry-run` to validate a local
directory and preview the job without registering a device, creating a remote
folder, or saving a job.
The Proton Drive mount itself cannot be used as a Computer source.

`takeout` exports My Files, Photos, and Computer backups to a directory and
records completed files in `.pdcli-takeout-manifest.json`, so rerunning resumes
safely. Degraded or unsupported nodes are recorded in the manifest's `issues`
list.
Public-link commands accept a node UID; `share remove` removes that node's
public link. Passwords passed with `--password` may be recorded by shell
history. `share report` submits an abuse report for a shared node. `--bona-fide`
is required. `--message` is required for `copyright` and `stolen-data`. Use
`--invitation` to report a pending invitation before accepting it.

## systemd user service

Source installs can generate and enable a unit pointing to the current
executable:

```sh
pdcli service enable
pdcli service status
pdcli service disable
pdcli service uninstall
```

Distribution packages install `/usr/lib/systemd/user/pdcli.service`; enable it
with `systemctl --user enable --now pdcli.service`. A user service normally
starts at login. `loginctl enable-linger "$USER"` enables boot-before-login
startup, but a lingering daemon may run before a desktop Secret Service/keyring
is available. Sign in once first and ensure credentials are available to the
same user, or inspect `journalctl --user -u pdcli.service -e` if it restarts.

## Local data and security

On Linux, pdcli uses `$XDG_CONFIG_HOME/pdcli`, falling back to
`$HOME/.config/pdcli`. It stores the encrypted FUSE database, SDK caches
(including an encrypted secret cache), Computer job state, and downloaded file
cache there.
Session tokens and the cache master key use the `pdcli` OS-keyring service when
available; otherwise
`cred.ron` and `cache.key` are created with `0600` permissions on Unix.

Passwords are not persisted, but session tokens are sensitive. Protect the
keyring and configuration directory. `pdcli logout` stops the daemon and
removes the session credential but intentionally leaves cached files in place.

This client is community software, not a Proton product. Keep independent
backups and verify Computer conflict copies before deleting anything yourself.
