# Changelog

All notable changes to this project will be documented in this file.

<!-- changelog start -->

## [0.19.1](https://github.com/matrix-org/matrix-rust-sdk/tree/0.19.1) - 2026-09-18

No significant changes.

## [0.19.0](https://github.com/matrix-org/matrix-rust-sdk/tree/0.19.0) - 2026-09-16

### Added

- Full-text search relevance scores are now included in the results returned by
  `RoomIndex::search`.
  ([#6645](https://github.com/matrix-org/matrix-rust-sdk/pulls/6645))

### Changed

- Performance optimisation: Cache the Tantivy `IndexReader` on `RoomIndex`
  because creating it is costly.
  ([#6707](https://github.com/matrix-org/matrix-rust-sdk/pulls/6707))
- Introduce an `IndexableEvent` type and decouple message parsing from search
  indexing. ([#6710](https://github.com/matrix-org/matrix-rust-sdk/pulls/6710))

### Fixed

- `RoomIndex::bulk_execute` now waits for the index writers merge threads before
  dropping it, avoiding spurious "couldn't find segment in SegmentManager"
  warnings and index fragmentation
  ([#6731](https://github.com/matrix-org/matrix-rust-sdk/pulls/6731))
- Fix a document being dropped from the index when it was added and then edited
  within the same batch of operations.
  ([#6774](https://github.com/matrix-org/matrix-rust-sdk/pulls/6774))
- Use `ReloadPolicy::Manual` for the Tantivy `IndexReader` of a `RoomIndex`.
  Tantivy's default policy spawns a meta file watcher thread per index, i.e. one
  per room, and panics if the thread cannot be spawned. Commits already reload
  the reader explicitly, so the watcher was pure overhead.
  ([#6798](https://github.com/matrix-org/matrix-rust-sdk/pulls/6798))
- Fix a possible overflow panic in Tantivy when a malformed timestamp is
  converted from milliseconds to nanoseconds. If the timestamp is too large, the
  conversion to nanoseconds was panicking. The timestamp now comes from
  `TimelineEvent::timestamp`, which already deals with malformed timestamp. In
  addition, the timestamp is capped in case the API is misused.
  ([#6813](https://github.com/matrix-org/matrix-rust-sdk/pulls/6813))

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
