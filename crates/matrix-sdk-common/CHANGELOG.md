# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Bug Fixes

- Change the behavior of `LinkedChunk::new_with_update_history()` to emit an
  `Update::NewItemsChunk` when a new, initial empty, chunk is created.
  ([#4327](https://github.com/matrix-org/matrix-rust-sdk/pull/4321))

- [**breaking**] Make `Room::history_visibility()` return an Option, and
  introduce `Room::history_visibility_or_default()` to return a better
  sensible default, according to the spec.
  ([#4325](https://github.com/matrix-org/matrix-rust-sdk/pull/4325))

- Clear the internal state of the `AsVector` struct if an `Update::Clear`
  state has been received.
  ([#4321](https://github.com/matrix-org/matrix-rust-sdk/pull/4321))

### Documentation

- Document that a decrypted raw event always has a room id.
  ([#728e1fd](https://github.com/matrix-org/matrix-rust-sdk/commit/728e1fda2ae9f1bfa87df162aa553040be705223))

## [0.8.0] - 2024-11-19

### Refactor

- Move `linked_chunk` from `matrix-sdk` to `matrix-sdk-common`.


