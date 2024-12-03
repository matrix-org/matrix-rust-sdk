# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

## [0.8.0] - 2024-11-19

### Features

- Improve the efficiency of objects stored in the crypto store.
  ([#3645](https://github.com/matrix-org/matrix-rust-sdk/pull/3645), [#3651](https://github.com/matrix-org/matrix-rust-sdk/pull/3651))

- Add new method `IndexeddbCryptoStore::open_with_key`. ([#3423](https://github.com/matrix-org/matrix-rust-sdk/pull/3423))

- `save_change` performance improvement, all encryption and serialization
  is done now outside of the db transaction.
### Bug Fixes

- Use the `DisplayName` struct to protect against homoglyph attacks.
