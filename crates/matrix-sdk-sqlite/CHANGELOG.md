# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Features

- Implement the new method of `EventCacheStoreMedia` for `SqliteEventCacheStore`.
  ([#4603](https://github.com/matrix-org/matrix-rust-sdk/pull/4603))
- Defragment an sqlite state store after removing a room.
  ([#4651](https://github.com/matrix-org/matrix-rust-sdk/pull/4651))
- Add `SqliteStoreConfig` and the `open_with_config` constructor on all the
  stores, it allows to control the maximum size of the pool of connections to
  SQLite for example.
  ([#4826](https://github.com/matrix-org/matrix-rust-sdk/pull/4826))
- Add `SqliteStoreConfig::path()` to override the path given to the constructor
  ([#4870](https://github.com/matrix-org/matrix-rust-sdk/pull/4870/))
- Implement `Clone` and `Debug` on `SqliteStoreConfig`
  ([#4870](https://github.com/matrix-org/matrix-rust-sdk/pull/4870/))

## [0.10.0] - 2025-02-04

### Features

- [**breaking**] `SqliteEventCacheStore` implements the new APIs of
  `EventCacheStore` for `MediaRetentionPolicy`. See the changelog of
  `matrix-sdk-base` for more details.
  ([#4571](https://github.com/matrix-org/matrix-rust-sdk/pull/4571))
- The SQLite databases are optimized during the construction of the stores. It
  should improve the performance of the queries.
  ([#4602](https://github.com/matrix-org/matrix-rust-sdk/pull/4602))
- The size of the WAL files is now limited to 10MB. This avoids cases where the
  WAL file takes as much space as the database.
  ([#4602](https://github.com/matrix-org/matrix-rust-sdk/pull/4602))

## [0.9.0] - 2024-12-18

### Features

- Add support for persisting LinkedChunks in the SQLite store. This is a step
  towards implementing event cache support, enabling a persisted cache of
  events.
  ([#4340](https://github.com/matrix-org/matrix-rust-sdk/pull/4340)) ([#4362](https://github.com/matrix-org/matrix-rust-sdk/pull/4362))

## [0.8.0] - 2024-11-19

### Bug Fixes

- Use the `DisplayName` struct to protect against homoglyph attacks.


### Refactor

- Move `event_cache_store/` to `event_cache/store/` in `matrix-sdk-base`.


