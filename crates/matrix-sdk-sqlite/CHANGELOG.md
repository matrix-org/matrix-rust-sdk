# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Features

- Implement new method `CryptoStore::has_downloaded_all_room_keys`, and process
  `room_key_backups_fully_downloaded` field in `Changes`.
  ([#6017](https://github.com/matrix-org/matrix-rust-sdk/pull/6017))
  ([#6044](https://github.com/matrix-org/matrix-rust-sdk/pull/6044))

## [0.16.0] - 2025-12-04

### Features

- Implement new method `CryptoStore::get_withheld_sessions_by_room_id`.
  ([#5819](https://github.com/matrix-org/matrix-rust-sdk/pull/5819))
- [**breaking**] `SqliteCryptoStore::get_withheld_info` now returns `Result<Option<RoomKeyWithheldEntry>>`.
  ([#5737](https://github.com/matrix-org/matrix-rust-sdk/pull/5737))

- Implement a new constructor that allows to open `SqliteCryptoStore` with a cryptographic key
  ([#5472](https://github.com/matrix-org/matrix-rust-sdk/pull/5472))
- Implement `StateStore::upsert_thread_subscriptions()` method for bulk upserts.
  ([#5848](https://github.com/matrix-org/matrix-rust-sdk/pull/5848))

### Refactor
- [breaking] Change the logic for opening a store so as to use a `Secret` enum in the function `open_with_pool` instead of a `passphrase`
  ([#5472](https://github.com/matrix-org/matrix-rust-sdk/pull/5472))

## [0.14.0] - 2025-09-04

No notable changes in this release.

## [0.13.0] - 2025-07-10

### Security Fixes

- Fix SQL injection vulnerability in `find_event_relations()`.
  ([d0c0100](https://github.com/matrix-org/matrix-rust-sdk/commit/d0c01006e4808db5eb96ad5c496416f284d8bd3c), Moderate, [CVE-2025-53549](https://www.cve.org/CVERecord?id=CVE-2025-53549), [GHSA-275g-g844-73jh](https://github.com/matrix-org/matrix-rust-sdk/security/advisories/GHSA-275g-g844-73jh))

## [0.12.0] - 2025-06-10

### Bug Fixes

- Fix a `UNIQUE` constraint violation in the event cache store
  ([#5001](https://github.com/matrix-org/matrix-rust-sdk/pull/5001))

## [0.11.0] - 2025-04-11

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
- Add `SqliteStoreConfig::with_low_memory_config` constructor
  ([#4894](https://github.com/matrix-org/matrix-rust-sdk/pull/4894))

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
