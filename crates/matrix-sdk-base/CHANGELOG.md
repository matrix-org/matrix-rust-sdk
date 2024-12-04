# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Bug Fixes

- Fix an off-by-one error in the `ObservableMap` when the `remove()` method is
  called. Previously, items following the removed item were not shifted left by
  one position, leaving them at incorrect indices.
  ([#4346](https://github.com/matrix-org/matrix-rust-sdk/pull/4346))

## [0.8.0] - 2024-11-19

### Bug Fixes

- Add more invalid characters for room aliases.

- Use the `DisplayName` struct to protect against homoglyph attacks.


### Features
- Add `BaseClient::room_key_recipient_strategy` field

- `AmbiguityCache` contains the room member's user ID.

- [**breaking**] `Media::get_thumbnail` and `MediaFormat::Thumbnail` allow to
  request an animated thumbnail They both take a `MediaThumbnailSettings`
  instead of `MediaThumbnailSize`.

- Consider knocked members to be part of the room for display name
  disambiguation.

- `Client::cross_process_store_locks_holder_name` is used everywhere:
 - `StoreConfig::new()` now takes a
   `cross_process_store_locks_holder_name` argument.
 - `StoreConfig` no longer implements `Default`.
 - `BaseClient::new()` has been removed.
 - `BaseClient::clone_with_in_memory_state_store()` now takes a
   `cross_process_store_locks_holder_name` argument.
 - `BaseClient` no longer implements `Default`.
 - `EventCacheStoreLock::new()` no longer takes a `key` argument.
 - `BuilderStoreConfig` no longer has
   `cross_process_store_locks_holder_name` field for `Sqlite` and
   `IndexedDb`.

- Make `ObservableMap::stream` works on `wasm32-unknown-unknown`.

- Allow aborting media uploads.

- Replace the `Notification` type from Ruma in `SyncResponse` and `StateChanges`
  by a custom one.

- Introduce a `DisplayName` struct which normalizes and sanitizes
display names.


### Refactor

- [**breaking**] Rename `DisplayName` to `RoomDisplayName`.

- Rename `AmbiguityMap` to `DisplayNameUsers`.

- Move `event_cache_store/` to `event_cache/store/` in `matrix-sdk-base`.

- Move `linked_chunk` from `matrix-sdk` to `matrix-sdk-common`.

- Move `Event` and `Gap` into `matrix_sdk_base::event_cache`.

- The ambiguity maps in `SyncResponse` are moved to `JoinedRoom` and `LeftRoom`.

- `Store::get_rooms` and `Store::get_rooms_filtered` are way faster because they
  don't acquire the lock for every room they read.

- `Store::get_rooms`, `Store::get_rooms_filtered` and `Store::get_room` are
  renamed `Store::rooms`, `Store::rooms_filtered` and `Store::room`.

- [**breaking**] `Client::get_rooms` and `Client::get_rooms_filtered` are renamed
  `Client::rooms` and `Client::rooms_filtered`.

- [**breaking**] `Client::get_stripped_rooms` has finally been removed.

- [**breaking**] The `StateStore` methods to access data in the media cache
  where moved to a separate `EventCacheStore` trait.

- [**breaking**] The `instant` module was removed, use the `ruma::time` module instead.

# 0.7.0

- Rename `RoomType` to `RoomState`
- Add `RoomInfo::state` accessor
- Remove `members` and `stripped_members` fields in `StateChanges`. Room member events are now with
  other state events in `state` and `stripped_state`.
- `StateStore::get_user_ids` takes a `RoomMemberships` to be able to filter the results by any
  membership state.
  - `StateStore::get_joined_user_ids` and `StateStore::get_invited_user_ids` are deprecated.
- `Room::members` takes a `RoomMemberships` to be able to filter the results by any membership
  state.
  - `Room::active_members` and `Room::joined_members` are deprecated.
- `RoomMember` has new methods:
  - `can_ban`
  - `can_invite`
  - `can_kick`
  - `can_redact`
  - `can_send_message`
  - `can_send_state`
  - `can_trigger_room_notification`
- Move `StateStore::get_member_event` to `StateStoreExt`
- `StateStore::get_stripped_room_infos` is deprecated. All room infos should now be returned by
  `get_room_infos`.
- `BaseClient::get_stripped_rooms` is deprecated. Use `get_rooms_filtered` with
  `RoomStateFilter::INVITED` instead.
- Add methods to `StateStore` to be able to retrieve data in batch
  - `get_state_events_for_keys`
  - `get_profiles`
  - `get_presence_events`
  - `get_users_with_display_names`
- Move `Session`, `SessionTokens` and associated methods to the `matrix-sdk` crate.
- Add `Room::subscribe_info`

# 0.5.1

## Bug Fixes
- #664: Fix regression with push rules being applied to the own user_id only instead of all but the own user_id

# 0.5.0
