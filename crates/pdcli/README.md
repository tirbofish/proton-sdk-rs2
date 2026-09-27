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
exists. On Linux, `pdcli gui` (or the desktop launcher) opens the separate
GTK4/libadwaita window; install `pdcli-gui` beside `pdcli`. The daemon and
AppIndicator tray remain in a separate GTK3 process. On WSL, `pdcli` without
a subcommand mounts; use `pdcli gui` for the window. `--force-offline` uses
local state only and `--no-tray` suppresses the tray icon.

The window groups My Files, Photos, Computers, and Status in primary navigation.
Status shows active transfer progress, cancellation for downloads, sync controls,
and mount actions. Files uses the available pane for the folder list; right-click
an item for open, rename, sharing, web handoff, and confirmed trash actions.
Computers offers a folder chooser and a structured backup list. The profile
menu in the header is the entry point for Account, Settings, About, and Quit;
sign-out is on Account. Settings includes automatic mounting, default start
page, and global `.pdignore` patterns. The FUSE mount remains the way to
upload and download with a file manager.

Photos displays paginated timeline, album lists, and album contents without
loading previews. Import streams one local image into the timeline without
removing its source; export writes a resumable local copy of timeline Photos.
Full photo viewing hands off to the web app. Timeline photos can be marked as
favorites, and new albums can be created.
Favorite state is not shown for album photos because the album API does not
return photo tags.

## Commands

```text
pdcli status [--json]
pdcli cancel-transfer <ID>   # active downloads only
pdcli retry <ID>
pdcli retry --all
pdcli pause
pdcli resume
pdcli sync
pdcli open
pdcli stop                 # also: unmount
pdcli logout

pdcli computers --json
pdcli photos timeline [--cursor LINK_ID]
pdcli photos albums [--cursor LINK_ID]
pdcli photos album VOLUME~LINK [--cursor LINK_ID]
pdcli photos favorite VOLUME~LINK [--off]
pdcli photos create-album NAME
pdcli photos thumbnail VOLUME~LINK [--preview] > image
pdcli photos import PATH
pdcli photos export DEST

pdcli computers
pdcli computers register [--name NAME] [--bind DEVICE_ID]
pdcli computers sync PATH [--name NAME] [--dry-run]
pdcli computers restore COMPUTER FOLDER PATH
pdcli computers unsync JOB

pdcli share link NODE_UID [--role viewer|editor] [--expires RFC3339] [--password-stdin]
pdcli share status NODE_UID [--json]
pdcli share remove NODE_UID
pdcli share invite NODE_UID EMAIL [--role viewer|editor]
pdcli share revoke NODE_UID EMAIL
pdcli share report NODE_UID --category CATEGORY --bona-fide [--message TEXT] [--email EMAIL] [--revision UID] [--invitation UID]

pdcli takeout DESTINATION
```

`status --json` is intended for scripts and includes login, daemon, mount,
journal, and active transfer information. Failed journal entries remain available for inspection;
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
`photos export DEST` exports only timeline Photos into `DEST/Photos/YYYY/MM`,
using the same resumable manifest but writing directly to destination files
without plaintext temporary files. An interrupted file may remain incomplete
without a manifest entry; rerunning chooses a new name and skips completed
files. It does not export My Files, Computers, or album-only photos. `photos import
PATH` imports one supported image, preserving the local source and skipping
Photos timeline duplicates by name and SHA-1. Import and export have not been
verified against a live account.
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
**Downloaded FUSE file content is currently plaintext on disk**, even though the
FUSE database and SDK secret cache are encrypted. Do not treat a downloaded
file as protected against someone with filesystem access; place the
configuration directory on an encrypted filesystem if needed. There is not yet
a safe in-app way to evict offline copies. A thumbnail redirected from the CLI
is also a plaintext file.
Session tokens and the cache master key use the `pdcli` OS-keyring service when
available; otherwise
`cred.ron` and `cache.key` are created with `0600` permissions on Unix.

Passwords are not persisted, but session tokens are sensitive. Protect the
keyring and configuration directory. `pdcli logout` stops the daemon and
removes the session credential but intentionally leaves cached files in place.

This client is community software, not a Proton product. Keep independent
backups and verify Computer conflict copies before deleting anything yourself.
