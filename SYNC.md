# Syncing instructions for agents

When the user asks an agent to sync, follow this workflow to bring the Rust
codebase up to date with the upstream Proton Drive SDK.

1. **Get the latest upstream commits.** Use
   `https://github.com/ProtonDriveApps/sdk.git` as the upstream repository and
   `ProtonDriveApps/sdk/` as its local checkout. If the checkout does not exist,
   create the parent directory and clone it:

   ```sh
   mkdir -p ProtonDriveApps
   git clone https://github.com/ProtonDriveApps/sdk.git ProtonDriveApps/sdk
   ```

   If it already exists, fetch the latest commits and tags from that repository.
   Determine the remote's default branch and inspect its latest fetched commit.
   Update the reference checkout to that commit without discarding local changes.

2. **Review and port the changes.** Identify the upstream commit used by the
   previous sync from the Rust repository's commit history. Review the upstream
   commits and diffs between that baseline and the latest fetched commit, then
   port the relevant behavior, API, cryptography, and bug fixes into the Rust
   crates. If no baseline is recorded, compare the current Rust implementation
   against upstream to identify missing changes. Use `ProtonDriveApps/sdk/` as
   a reference; do not edit upstream source files while implementing Rust changes.
   Add focused regression tests for the behavior being ported.

3. **Determine the upstream version.** Read the upstream SDK changelog at the
   fetched commit (`sdk/Changelog`, or its actual path in the checkout; the
   JavaScript SDK currently uses `ProtonDriveApps/sdk/client/js/CHANGELOG.md`).
   Record the latest released SDK version listed there and the exact upstream
   commit SHA being synced. If the fetched commit includes unreleased changes,
   make that explicit rather than assigning them an invented release version.

4. **Bump every crate version.** Every sync must increase the effective Cargo
   package version of each workspace crate: `proton-sdk-rs2`, `proton-drive-sdk`,
   and `pdcli`, including crates with no direct code changes. Choose an
   appropriate semantic-version increment, with at least a patch bump for each
   crate. These crates currently inherit `workspace.package.version`, so bumping
   that value in the root `Cargo.toml` updates all three. If crates use individual
   versions in the future, bump each crate's manifest separately. Update
   `Cargo.lock` and any internal dependency version requirements as needed, and
   verify that every crate's resolved package version increased. Rust crate
   versions remain independent of the upstream SDK version.

5. **Validate the Rust changes.** Run `cargo fmt --all -- --check` and
   `cargo test --workspace --all-features`. Resolve failures caused by the port
   and report any remaining validation blockers.

6. **Create a sync commit.** Commit the completed Rust port and all crate version
   bumps with a conventional description that includes the upstream SDK version
   from the changelog, for
   example `feat(drive-sdk): sync upstream SDK v<VERSION>`. Include the upstream
   commit SHA, a summary of the ported changes, and validation results in the
   commit body so the next sync has a clear baseline. Record the old and new
   version of each crate. Mention any unreleased changes or applicable changes
   that remain unported. Stage only files belonging
   to this sync; exclude unrelated user changes, credentials, generated databases,
   and files from `dist/`.
