# Project notes

This file records deliberately deferred ideas; it is not a list of supported
commands.

## Deferred CLI surface

The FUSE mount already provides normal file-manager operations. A separate
headless command set (`ls`, `get`, `put`, `mv`, `mkdir`, `stat`, and similar)
would duplicate that surface and is not currently implemented. Add it only if
NAS/headless users need operations without mounting the filesystem.

## Current safe defaults

- Computer sync is additive and does not delete either side.
- Local and remote versions that differ create timestamped conflict copies.
- `pdcli computers sync PATH --dry-run` validates a path without creating
  remote state.
- `pdcli takeout DESTINATION` is a resumable My Files export; Photos,
  degraded/unsupported nodes, and Computer backups remain outside its scope.

## Upstream tracking

The current compatibility reference is the fetched
`ProtonDriveApps/sdk` baseline `6cbf2f44` (15 September 2026). The next
upstream cryptographic migration is expected around late 2026/early 2027;
review and test stored credentials at that boundary before changing SDK
versions.
