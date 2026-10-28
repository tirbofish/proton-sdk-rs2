# pdcli

`pdcli` is an unofficial Proton Drive client for Linux. It mounts the account
through FUSE at `~/ProtonDrive` and runs a background daemon for lazy reads,
uploads, event polling, and Computer sync.

## CLI and desktop app

The project installs two companion executables:

- `pdcli` is the command-line client and runs the filesystem daemon.
- `pdcli-gui` is **pdcli Desktop**, the graphical app shown in your application menu.
  Launch it with `pdcli gui`.

Install both executables together. They share your account and configuration;
pdcli Desktop calls `pdcli` for Drive operations. Commands in this README run in
a terminal using `pdcli`. The photo duplicate strategy options and duplicate report
are currently CLI controls; desktop photo imports use the default `skip` strategy.

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
local state only and `--no-tray` suppresses the tray icon. When the daemon
already runs without a tray (the systemd unit uses `--no-tray`), `pdcli mount`
and the window attach a tray to it after checking that the daemon belongs to
the same user and runs the same `pdcli` executable.

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
pdcli photos import PATH [--duplicate-strategy skip|error|keep-both] [--include-videos]
pdcli photos duplicates [--count-only]
pdcli photos dedupe [--dry-run]
pdcli photos trash VOLUME~LINK [VOLUME~LINK ...]
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
PATH` imports one supported image or recursively imports a directory, preserving
local sources and skipping
Photos timeline duplicates by declared plaintext SHA-1, even when renamed.
`--duplicate-strategy skip` (the default) returns the existing UID without uploading;
`error` exits unsuccessfully on a match; `keep-both` uploads another copy. Uploads
choose an available filename such as `photo (1).jpg` when the original is occupied.
Folder imports reuse one duplicate index and add completed uploads to it, so
local duplicate files are skipped too. Videos are excluded unless `--include-videos`
is supplied. Symlinks and sidecars are excluded from folder imports.
Every import scans the content fingerprints already included in timeline pages,
without decrypting every photo or downloading contents. Import fails before upload
if any timeline photo lacks a valid fingerprint, so it cannot silently miss duplicates.

`photos duplicates` groups timeline content fingerprints in one pass, then fetches
names and sizes only for matching groups, with at most four metadata batches in
flight. `--count-only` skips all name/size fetching; group UIDs and counts are still
returned, with names and sizes set to null. Both paths compare the same hashes.
The command shows hash scanning and a duplicate-details progress bar on
terminal stderr, keeping stdout as JSON for scripts. Progress also appears during
import duplicate checks and is hidden when stderr is redirected. It prints a JSON report with `duplicate_groups`, `extra_copies`
(the number beyond one copy per group), and groups containing names, sizes and UIDs.
`unchecked_photos` lists photos without a valid timeline fingerprint;
these are excluded from the duplicate count. The scan covers the entire timeline,
excluding trash and album-only photos. Hash matches identify identical file content,
not visually similar images; fingerprints are HMAC-SHA256 of uploader-declared
plaintext SHA-1 using the timeline root key, not freshly verified
by downloading. Review favorites and album membership, choose the copies to keep,
then use `photos trash UID ...` to move selected copies to Photos trash. This also
affects their album membership. Restore through Proton Drive if needed. The report
itself makes no changes and includes these available actions.

For automatic cleanup, run `pdcli photos dedupe --dry-run` to preview, then
`pdcli photos dedupe` to move extra copies to Photos trash. It keeps the lowest
UID in each exact-content group, matching the first entry in the duplicate report.
It leaves unique photos and photos with unknown fingerprints alone. Favorites
and album memberships are not merged into the retained copy; use selected-UID
trashing instead if those differ between copies. The JSON result lists kept,
extra, successfully trashed UIDs and any errors. Removal is batched; partial
failures return an unsuccessful exit status and uncertain writes are not retried.
Restore through Proton Drive if needed. This command does not permanently delete
photos or empty trash.

Import, export and duplicate management have not been verified against a live account.
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
