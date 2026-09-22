# Changelog

## 0.3.2 - 2026-09-22

- change readme
- fix(drive-sdk): recognize owned photos volume roles
- fix(pdcli): report unsupported takeout documents
- fix(pdcli): restore terminal input after resume
- feat(pdcli): guide users when storage is full
- feat(drive-sdk): report upload performance telemetry
- feat(drive-sdk): expose node membership metadata
- feat(drive-sdk): stream per-node move results
- feat(pdcli): export photos and computers in takeout
- feat(drive-sdk): save photos to timeline
- docs: add repository contributor guide

## 0.3.1 - 2026-09-16

- feat(drive-sdk): implement Easy Switch, diagnostic zip, and unauth routes
- feat: add pdcli packaging, sharing parity, and TypeScript test ports

## 0.3.0 - 2026-09-16

- Initial packaged pdcli release with FUSE mount, Computers sync, sharing, takeout, and a systemd user service.
- Debian, RPM, and Arch recipes install `/usr/bin/pdcli` and `/usr/lib/systemd/user/pdcli.service`.
