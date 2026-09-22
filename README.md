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

## License

MIT. See [`LICENSE.md`](LICENSE.md).
