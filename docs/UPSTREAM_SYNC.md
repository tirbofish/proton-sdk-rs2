# Upstream SDK sync

Reviewed on 22 October 2026 against the default upstream branch `main` at
`8a44654aece1609f0f7218be51c205103945abd8`.

The latest released version listed in `client/js/CHANGELOG.md` is **js/v0.22.2**
(2 October 2026). The fetched commit carries tag `js/v0.23.0`, which has no
changelog entry; its changes (album capabilities, the breaking available-name
signature, experimental search methods) are recorded by commit rather than
assigned an invented changelog version.

The previous sync, `7e3c95d62f9f9484576a513d406107a899f8b435`, recorded upstream
`28ac9cdc258737375692d1751dd9c7edcfb96708` as its baseline. The 20 upstream
commits between that baseline and the fetched commit were reviewed.

## Ported changes

- `ProtonDriveClient::get_available_name` takes a `NodeType`. Files and photos
  keep their extension (`file (2).txt`); folders and albums get the index
  appended (`folder.name (2)`). Candidates are checked in batches of 10 up to
  index 1000, and the name matching the first available hash is returned. The
  previous Rust implementation always returned the original name whenever any
  candidate was available; that bug is fixed.
- Deleting an album that holds photos not in the timeline maps API code 200302
  and its `Details.ChildLinkIDs` to `AlbumContainsPhotosNotInTimelineError`.
  `ProtonPhotosClient::delete_album` takes `save_to_timeline`; when set, those
  photos are saved to the timeline and the deletion is retried once. Album
  deletion now reports API errors instead of ignoring the response.
- The upstream JavaScript test catalog lists all 126 current files, including
  the renamed search tests.

## Coverage and compatibility

Album create, update, add, and remove operations added to the C# SDK already
exist in Rust.

Not ported:

- `fix(cli-js): keep crypto time in sync with the API`. Rust signs and generates
  keys with the device clock. Syncing to `x-pm-date`/`Date` needs a clock offset
  passed to every proton-rpgp/proton-crypto call site, so it is left for a
  dedicated change.
- Experimental search methods, the search SDK capabilities adapter, and the
  incubating search engine. The engine and its generated bindings are still not
  in the public mirror. The Rust `init_search` provider hook is unchanged.
- `fix(cli-js)` case-insensitive local download names: pdcli has no equivalent
  name-list collision logic.
- C# large-transfer local storage buffers, Kotlin transfer queue and Swift
  upload resume fixes, NDK/dependency/translation updates, and binding glue do
  not apply to the Rust runtime.

Source-breaking changes: `get_available_name` gains a `node_type` argument and
`delete_album` gains `save_to_timeline`. No stored cache format or credential
migration is introduced.

## Validation

- `cargo fmt --all -- --check`
- `cargo test --workspace --all-features`
