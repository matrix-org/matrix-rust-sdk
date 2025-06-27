# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

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

### Bug Fixes

- Remove the usage of an unwrap in the `StoreCipher::import_with_key` method.
  This could have lead to panics if the second argument was an invalid
  `StoreCipher` export.
  ([#4506](https://github.com/matrix-org/matrix-rust-sdk/pull/4506))

## [0.9.0] - 2024-12-18

No notable changes in this release.

## [0.8.0] - 2024-11-19

No notable changes in this release.
