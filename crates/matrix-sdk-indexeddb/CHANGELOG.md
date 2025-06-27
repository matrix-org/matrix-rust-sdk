# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Features

- Add support for received room key bundle data, as required by encrypted history sharing ((MSC4268)[https://github.com/matrix-org/matrix-spec-proposals/pull/4268)). ([#5276](https://github.com/matrix-org/matrix-rust-sdk/pull/5276))

### Maintenance

- Update getrandom dependency from 0.2.15 to 0.3.3 and migrate from the
  deprecated 'js' feature to the new 'wasm_js' feature for WebAssembly
  compatibility. This ensures proper random number generation in WebAssembly
  environments with the latest getrandom API.
  ([#XXXX](https://github.com/matrix-org/matrix-rust-sdk/pull/XXXX))

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
