# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Features

- Implement new method `CryptoStore::get_withheld_sessions_by_room_id`.
  ([#5819](https://github.com/matrix-org/matrix-rust-sdk/pull/5819))
- [**breaking**] `IndexeddbCryptoStore::get_withheld_info` now returns `Result<Option<RoomKeyWithheldEntry>, ...>`.
  ([#5737](https://github.com/matrix-org/matrix-rust-sdk/pull/5737))

### Performance

- Improve performance of certain media queries in `MediaStore` implementation by storing media content and media metadata
  in separate object stores in IndexedDB (see [#5795](https://github.com/matrix-org/matrix-rust-sdk/pull/5795)).

## [0.14.0] - 2025-09-04

No notable changes in this release.

## [0.13.0] - 2025-07-10

### Features

- Add support for received room key bundle data, as required by encrypted history sharing ((MSC4268)[https://github.com/matrix-org/matrix-spec-proposals/pull/4268)). ([#5276](https://github.com/matrix-org/matrix-rust-sdk/pull/5276))

## [0.12.0] - 2025-06-10

No notable changes in this release.

## [0.11.0] - 2025-04-11

No notable changes in this release.

## [0.10.0] - 2025-02-04

No notable changes in this release.

## [0.9.0] - 2024-12-18

No notable changes in this release.

## [0.8.0] - 2024-11-19

### Features

- Improve the efficiency of objects stored in the crypto store.
  ([#3645](https://github.com/matrix-org/matrix-rust-sdk/pull/3645), [#3651](https://github.com/matrix-org/matrix-rust-sdk/pull/3651))

- Add new method `IndexeddbCryptoStore::open_with_key`. ([#3423](https://github.com/matrix-org/matrix-rust-sdk/pull/3423))

- `save_change` performance improvement, all encryption and serialization
  is done now outside of the db transaction.

### Bug Fixes

- Use the `DisplayName` struct to protect against homoglyph attacks.
