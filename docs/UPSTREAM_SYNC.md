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

### Gap-closing additions after the sync

A follow-up commit implements the upstream client features that Rust lacked:

- Crypto time follows the API. Drive HTTP clients read `x-pm-date`, falling back
  to `Date`, and every signing, encryption, and key-generation call is dated by
  the resulting server time. The offset is process-wide, like upstream's
  `updateServerTime`.
- `get_node_hierarchy` returns a node and its ancestors from the root, failing on
  parent loops.
- `copy_nodes` copies several nodes, each optionally renamed, with a result per node.
- `enumerate_shared_nodes` and `enumerate_shared_nodes_with_me` load the shared
  nodes themselves (deprecated upstream in favour of the UID variants).
- `get_event_scheduler` returns an `EventScheduler`: the own volume polls every
  30 s, shared scopes every 10 min, with up to 1 s of jitter. One shared scope at
  a time can be in the foreground; the next poll waits for the callback.
- `prepare_import_folder` generates import-folder crypto material in My Files
  without creating the folder, validating the name like upstream.
- The experimental search interface matches upstream: `run_indexing`,
  `run_extraction`, `query`, `nodes_deleted`, `get_status`, and
  `get_debug_info`. Providers receive `SearchSdkCapabilities` (node loading and
  authenticated Drive API calls). `init_search` starts once per client and
  retries a failed start.

Still not ported:

- The incubating search engine itself (extraction, indexing, built-in storage).
  It and its generated bindings are not in the public mirror; applications must
  provide a `SearchServiceProvider`.
- `fix(cli-js)` case-insensitive local download names: pdcli has no equivalent
  name-list collision logic.
- C# large-transfer local storage buffers, Kotlin transfer queue and Swift
  upload resume fixes, NDK/dependency/translation updates, and binding glue do
  not apply to the Rust runtime.

Source-breaking changes: `get_available_name` gains a `node_type` argument,
`delete_album` gains `save_to_timeline`, `ProtonDriveSearchClient` replaces
`enable` with upstream's methods, and `SearchServiceProvider::start` receives
`SearchSdkCapabilities`. No stored cache format or credential migration is
introduced.

## Validation

- `cargo fmt --all -- --check`
- `cargo test --workspace --all-features`
