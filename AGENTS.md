# Repository Guidelines

## Project Structure & Module Organization

This is a Rust 2024 Cargo workspace with three crates under `crates/`:

- `proton-sdk-rs2`: authentication, sessions, account APIs, and shared Proton primitives.
- `proton-drive-sdk`: Drive APIs, cryptography, caching, transfers, sharing, and photo operations.
- `pdcli`: the command-line, GUI, FUSE mount, sync, and takeout application.

Unit tests normally live beside their implementation in `#[cfg(test)]` modules. Cross-module upstream compatibility tests live in `crates/proton-drive-sdk/tests/`. `ProtonDriveApps/sdk/` is the checked-out upstream reference; do not edit it when implementing Rust changes. Packaging lives in `debian/`, `packaging/`, and `scripts/`.

## Build, Test, and Development Commands

- `cargo build --workspace`: build every crate in debug mode.
- `cargo build --locked --release -p pdcli`: produce the release CLI used by packaging.
- `cargo test --workspace --all-features`: run the same test scope used by CI.
- `cargo test -p proton-drive-sdk test_name`: run one matching SDK test.
- `cargo fmt --all -- --check`: verify formatting without modifying files.
- `cargo fmt --all`: apply standard Rust formatting.
- `cargo run -p pdcli -- --help`: run the CLI from source.

Linux builds require Protobuf, FUSE 3, GTK 3, DBus, and AppIndicator development packages. Keyring tests may require a desktop Secret Service session.

## Coding Style & Naming Conventions

Use `rustfmt` defaults and four-space indentation. Follow Rust conventions: `snake_case` for modules, functions, and tests; `CamelCase` for types and traits; `SCREAMING_SNAKE_CASE` for constants. Keep async I/O non-blocking, propagate recoverable errors with `Result`, and add dependencies only when the standard library or an existing workspace dependency cannot solve the problem.

## Testing Guidelines

Add the smallest test that demonstrates new behavior or prevents a regression. Name tests after observable behavior, such as `transfer_http_errors_classify_permanent_and_transient_statuses`. Avoid live Proton-account dependencies in the default suite. Before submitting, run formatting and the full workspace tests.

## Commit & Pull Request Guidelines

Use the repository's concise conventional style: `feat(pdcli): ...`, `feat(drive-sdk): ...`, `fix(auth): ...`, or `release: ...`. Keep commits focused. Pull requests should describe the trigger, resulting behavior, affected crates, and validation commands. Link relevant issues and call out compatibility, credential-storage, cache-migration, or packaging effects. Include screenshots only for GUI changes.

## Security & Configuration

Never commit Proton credentials, session tokens, cache keys, generated databases, or files from `dist/`. Preserve certificate validation and encrypted-cache behavior unless the change explicitly addresses them.
