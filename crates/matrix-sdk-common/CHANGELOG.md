# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Features

- [**breaking**] Use `Raw<AnyTimelineEvent>` in place of `Raw<AnyMessageLikeEvent>`
  in `DecryptedRoomEvent::event`.
  ([#5512](https://github.com/matrix-org/matrix-rust-sdk/pull/5512/files)).
  Affects the following functions:
  - `OlmMachine::decrypt_room_event` - existing matches on the result's event field
     should be updated to `AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::...)`

## [0.13.0] - 2025-07-10

### Features

- Expose the `ROOM_VERSION_RULES_FALLBACK` that should be used when the rules of
  a room are unknown.
  ([#5337](https://github.com/matrix-org/matrix-rust-sdk/pull/5337))
- Expose the `ROOM_VERSION_FALLBACK` that should be used when the version of a
  room is unknown.
  ([#5306](https://github.com/matrix-org/matrix-rust-sdk/pull/5306))

### Refactor

- [**breaking**] `extract_bundled_thread_summary()` returns a
  `Raw<AnySyncMessageLikeEvent>` for the latest event instead of a
  `Raw<AnyMessageLikeEvent>`.
  ([#5337](https://github.com/matrix-org/matrix-rust-sdk/pull/5337))

## [0.12.0] - 2025-06-10

No notable changes in this release.

## [0.11.0] - 2025-04-11

### Features

- Add a simple TTL cache implementation. The `TtlCache` struct can be used as a
  key/value map that expires items after 15 minutes.
  ([#4663](https://github.com/matrix-org/matrix-rust-sdk/pull/4663))

## [0.10.0] - 2025-02-04

- [**breaking**]: `SyncTimelineEvent` and `TimelineEvent` have been
  fused into a single type `TimelineEvent`, and its field `push_actions`
  has been made `Option`al (it is set to `None` when we couldn't
  compute the push actions, because we lacked some information).
  ([#4568](https://github.com/matrix-org/matrix-rust-sdk/pull/4568))

## [0.9.0] - 2024-12-18

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
