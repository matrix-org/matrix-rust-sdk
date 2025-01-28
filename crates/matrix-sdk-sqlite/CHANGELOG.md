# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Features

- [**breaking**] `SqliteEventCacheStore` implements the new APIs of
  `EventCacheStore` for `MediaRetentionPolicy`. See the changelog of
  `matrix-sdk-base` for more details.
  ([#4571](https://github.com/matrix-org/matrix-rust-sdk/pull/4571))

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


