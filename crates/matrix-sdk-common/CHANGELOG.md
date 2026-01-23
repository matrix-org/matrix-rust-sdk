# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Features

- [**breaking**] `ShieldStateCode` no longer includes
  `SentInClear`. `VerificationState::to_shield_state_{lax,strict}` never
  returned that code, and so having it in the enum was somewhat misleading.
  ([#5959](https://github.com/matrix-org/matrix-rust-sdk/pull/5959))
- Add field `forwarder` of type `ForwarderInfo` to `EncryptionInfo`, which
  exposes information about the forwarder of the keys with which an event was
  encrypted if they were shared as part of an [MSC4268](https://github.com/matrix-org/matrix-spec-proposals/pull/4268)
  room key bundle.
  ([#5945](https://github.com/matrix-org/matrix-rust-sdk/pull/5945)).

### Bug Fixes

- Fix an off-by-one check for `Error:InvalidItemIndex` in `LinkedChunk::remove_item_at`.
  ([#6057](https://github.com/matrix-org/matrix-rust-sdk/pull/6057))
- Fix `TimelineEvent::from_bundled_latest_event` sometimes removing the `session_id` of UTDs. This broken event could later be saved to the event cache and become an unresolvable UTD. ([#5970](https://github.com/matrix-org/matrix-rust-sdk/pull/5970)).

## [0.16.0] - 2025-12-04

### Features

- [**breaking**] Cross-process lock can be dirty. The `CrossProcess::try_lock_once` now returns a new type `CrossProcessResult`, which is an enum with `Clean`, `Dirty` or `Unobtained` variants. When the lock is dirty it means it's been acquired once, then acquired another time from another holder, so the current holder may want to refresh its internal state.
  ([#5672](https://github.com/matrix-org/matrix-rust-sdk/pull/5672)).

## [0.14.0] - 2025-09-04

### Features

- Tracing subscribers created via [`matrix_sdk_common::js_tracing::MakeJsLogWriter`] or [`make_tracing_subscriber`] will now drop log events at the `TRACE` level. Previously `TRACE` logs were treated the same as `DEBUG` logs. ([#5590](https://github.com/matrix-org/matrix-rust-sdk/pull/5590)).

- [**breaking**] Use `Raw<AnyTimelineEvent>` in place of `Raw<AnyMessageLikeEvent>`
  in `DecryptedRoomEvent::event`.
  ([#5512](https://github.com/matrix-org/matrix-rust-sdk/pull/5512)).
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
