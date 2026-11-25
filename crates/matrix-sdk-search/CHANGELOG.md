# Changelog

All notable changes to this project will be documented in this file.

<!-- changelog start -->

## [0.20.0](https://github.com/matrix-org/matrix-rust-sdk/tree/0.20.0) - 2026-11-25

### Added

- Add `RoomIndex::warm_up` to open all the segments of an index ahead of its
  first search.
  ([#7304](https://github.com/matrix-org/matrix-rust-sdk/pull/7304))
- [[**breaking**] The RoomIndex::search function now returns a SearchResult
  type, containing the total search result count in addition to the list of
  score, event tuples.
  ([#7059](https://github.com/matrix-org/matrix-rust-sdk/pull/7059))

### Changed

- [**breaking**] `RoomIndex::search` now sorts results by timestamp, newest
  first, instead of by relevance score. `SearchResult::events` now holds
  `(MilliSecondsSinceUnixEpoch, OwnedEventId)` pairs.
  ([#7260](https://github.com/matrix-org/matrix-rust-sdk/pull/7260))
- Encrypted room indexes no longer stretch their key with PBKDF2, which made
  opening each index slow: the key, formerly called a passphrase, must now be a
  high-entropy key. Existing encrypted indexes, and any index that can't be
  opened (e.g. after a key change), are deleted and recreated.
  ([#7268](https://github.com/matrix-org/matrix-rust-sdk/pull/7268))
- Merge the segments of a room index as soon as two of them are in the same
  layer, so a room index usually has a single segment. Opening an index opens
  every file of every segment, so this makes opening many room indexes much
  faster. ([#7304](https://github.com/matrix-org/matrix-rust-sdk/pull/7304))
- Bump tantivy to 0.27.0 (which includes bumping zstd to 0.14.3).
  ([#7350](https://github.com/matrix-org/matrix-rust-sdk/pull/7350))

### Fixed

- `AesWriter::terminate_ref` only appended the MAC and never terminated the
  inner writer, so encrypted index files were never fsynced to disk and could be
  lost on an OS crash or power loss.
  ([#7351](https://github.com/matrix-org/matrix-rust-sdk/pull/7351))

## [0.19.1](https://github.com/matrix-org/matrix-rust-sdk/tree/0.19.1) - 2026-09-18

No significant changes.

## [0.19.0](https://github.com/matrix-org/matrix-rust-sdk/tree/0.19.0) - 2026-09-16

### Added

- Full-text search relevance scores are now included in the results returned by
  `RoomIndex::search`.
  ([#6645](https://github.com/matrix-org/matrix-rust-sdk/pull/6645))

### Changed

- Performance optimisation: Cache the Tantivy `IndexReader` on `RoomIndex`
  because creating it is costly.
  ([#6707](https://github.com/matrix-org/matrix-rust-sdk/pull/6707))
- Introduce an `IndexableEvent` type and decouple message parsing from search
  indexing. ([#6710](https://github.com/matrix-org/matrix-rust-sdk/pull/6710))

### Fixed

- `RoomIndex::bulk_execute` now waits for the index writers merge threads before
  dropping it, avoiding spurious "couldn't find segment in SegmentManager"
  warnings and index fragmentation
  ([#6731](https://github.com/matrix-org/matrix-rust-sdk/pull/6731))
- Fix a document being dropped from the index when it was added and then edited
  within the same batch of operations.
  ([#6774](https://github.com/matrix-org/matrix-rust-sdk/pull/6774))
- Use `ReloadPolicy::Manual` for the Tantivy `IndexReader` of a `RoomIndex`.
  Tantivy's default policy spawns a meta file watcher thread per index, i.e. one
  per room, and panics if the thread cannot be spawned. Commits already reload
  the reader explicitly, so the watcher was pure overhead.
  ([#6798](https://github.com/matrix-org/matrix-rust-sdk/pull/6798))
- Fix a possible overflow panic in Tantivy when a malformed timestamp is
  converted from milliseconds to nanoseconds. If the timestamp is too large, the
  conversion to nanoseconds was panicking. The timestamp now comes from
  `TimelineEvent::timestamp`, which already deals with malformed timestamp. In
  addition, the timestamp is capped in case the API is misused.
  ([#6813](https://github.com/matrix-org/matrix-rust-sdk/pull/6813))

## [0.18.0](https://github.com/matrix-org/matrix-rust-sdk/tree/0.18.0) - 2026-06-02

No significant changes.

## [0.17.0] - 2026-05-08

No notable changes in this release.

## [0.16.1] - 2026-05-08

No notable changes in this release.

## [0.16.0] - 2025-12-04

No notable changes in this release.

## [0.14.0] - 2025-09-04

Initial release of the matrix-sdk-search crate
