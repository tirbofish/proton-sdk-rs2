# Upstream SDK sync

Reviewed on 4 October 2026 against the default upstream branch `main` at
`28ac9cdc258737375692d1751dd9c7edcfb96708`.

The latest released version listed in `client/js/CHANGELOG.md` is **js/v0.22.1**
(2 October 2026). The fetched commit also carries tag `js/v0.22.2`, which has no
changelog entry. It includes the subsequent search import/package fixes; these
are recorded by commit rather than assigned an invented changelog version.

The previous Rust port, `a4973050b5ebfced5c1d0240026349df8b050d14`, added recently
accessed APIs and the registered-invitation fix without recording an exact
upstream SHA. The pre-fetch reference checkout was `f28a93ce`; its implemented
APIs and the current Rust code were compared with the fetched upstream. The
13 subsequent upstream commits were reviewed. This record and the accompanying
sync commit establish an exact baseline for the next run.

## Ported changes

- Integrity telemetry uses calendar-based `past_month`, `past_year`,
  `since_2024`, and `before_2024` recency, plus `1p`, `3p-sdk`, and `3p` creator
  context. Unknown provenance remains absent.
- Link and revision DTOs retain optional `ThirdParty` and `Sdk` flags. Active
  revision attribute failures use the revision's time and origin rather than
  the containing node's. Shares, membership inviters, and bookmarks report
  their available creation/invitation context.
- Node crypto failures reach the existing telemetry sink with upstream JSON
  field names. Decryption and verification reports are deduplicated separately
  per node across client clones. Author matching uses the My Files member;
  failed author lookup does not change the crypto result.
- Experimental search accepts an application-provided `SearchServiceProvider`.
  `ProtonDriveClient::init_search` forwards the Rust SDK version and the My Files
  member address. The returned interface exposes `enable`, matching upstream's
  current experimental surface. No provider is loaded by default.
- Protobuf integrity payloads reserve the retired volume/legacy-age field
  numbers and use upstream recency/creator enum values and tags. Recently
  accessed Drive and Photos request messages are added with upstream tags.
- The upstream JavaScript test catalog includes all 119 current files, including
  the four newly added files and nine older omitted CLI fixtures. Regression
  tests cover calendar boundaries, optional provenance,
  metric payloads, deduplication, crypto error preservation, revision context,
  search initialization, and protobuf wire compatibility.

## Coverage and compatibility

The recently accessed API reporting added to the C# SDK already exists in Rust,
including batches of 50 and separate Drive/Photos routes.

The incubating Rust search engine and its generated bindings are absent from
the public upstream mirror, as documented in upstream's
`client/js/src/search/vendorFallback.d.ts`. Extraction, indexing, a built-in
engine/storage implementation, and the browser SharedWorker transport remain
unported. Applications must provide their own experimental search service.
Browser/npm packaging fixes, translations, and C#/Kotlin/Swift binding glue do
not apply to the Rust runtime.

The generated protobuf integrity structs change their public fields to match
upstream's breaking telemetry update. Consumers of those structs must replace
`volume_type` and `from_before_2024` with `recency` and `created_by`. Retired wire
tags are reserved and ignored when decoding. The optional DTO fields also
affect consumers constructing DTOs with Rust struct literals. No stored cache
format or credential migration is introduced.

## Validation

- `cargo fmt --all -- --check`
- `cargo test --workspace --all-features`: intermittent pre-existing failure below.
- `cargo test --workspace --all-features -- --test-threads=1`: all tests pass.

Local HTTP server tests require execution outside the restricted sandbox.
The default parallel workspace run intermittently fails the pre-existing
`pdcli::db::tests::migrates_plaintext_fuse_db` fixture with "no such function:
sqlcipher_export"; the same failure was observed before this port. A full
parallel run also passed, but a subsequent run reproduced the failure. Serial
workspace validation passes, including that migration fixture. No cache-migration
code is changed by this sync.
