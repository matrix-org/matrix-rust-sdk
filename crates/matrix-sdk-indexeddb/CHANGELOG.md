# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Features

- Add `IndexeddbEventCacheStore` for providing an IndexedDB implementation
  of the `EventCacheStore`. Expose the type through `IndexeddbEventCacheStoreBuilder`
  which ensures object stores in IndexedDB are properly initialized.

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
