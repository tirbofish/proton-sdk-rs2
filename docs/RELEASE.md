# pdcli v0.3

Linux only. Mount is `~/ProtonDrive` with `MyFiles/` and `Computers/`.

## Computers

Register this machine (or bind an existing Proton computer), then back up local folders and restore them on another machine. Sync is last-write-wins and does not delete.

```
pdcli computers register [--name NAME] [--bind DEVICE_ID]
pdcli computers sync ~/Documents
pdcli computers restore OtherPC Documents ~/Documents
```

Same flow lives in the GUI Computers page.

## Usage

```
chmod +x pdcli-x86_64-unknown-linux-gnu
./pdcli-x86_64-unknown-linux-gnu mount
```

Config: `$HOME/.config/pdcli`
