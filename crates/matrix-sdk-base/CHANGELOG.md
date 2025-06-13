# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

## [0.12.0] - 2025-06-10

No notable changes in this release.

## [0.11.0] - 2025-04-11

### Features

- [**breaking**] The `Client::subscribe_to_ignore_user_list_changes()`
  method will now only trigger whenever the ignored user list has
  changed from what was previously known, instead of triggering
  every time an ignore-user-list event has been received from sync.
  ([#4779](https://github.com/matrix-org/matrix-rust-sdk/pull/4779))
- [**breaking**] The `MediaRetentionPolicy` can now trigger regular cleanups
  with its new `cleanup_frequency` setting.
  ([#4603](https://github.com/matrix-org/matrix-rust-sdk/pull/4603))
  - `Clone` is a supertrait of `EventCacheStoreMedia`.
  - `EventCacheStoreMedia` has a new method `last_media_cleanup_time_inner`
  - There are new `'static` bounds in `MediaService` for the media cache stores
- `event_cache::store::MemoryStore` implements `Clone`.
- `BaseClient` now has a `handle_verification_events` field which is `true` by 
  default and can be negated so the `NotificationClient` won't handle received 
  verification events too, causing errors in the `VerificationMachine`.
- [**breaking**] `Room::is_encryption_state_synced` has been removed
  ([#4777](https://github.com/matrix-org/matrix-rust-sdk/pull/4777))
- [**breaking**] `Room::is_encrypted` is replaced by `Room::encryption_state`
  which returns a value of the new `EncryptionState` enum
  ([#4777](https://github.com/matrix-org/matrix-rust-sdk/pull/4777))

### Refactor

- [**breaking**] `BaseClient::store` is renamed `state_store`
  ([#4851](https://github.com/matrix-org/matrix-rust-sdk/pull/4851))
- [**breaking**] `BaseClient::with_store_config` is renamed `new`
  ([#4847](https://github.com/matrix-org/matrix-rust-sdk/pull/4847))
- [**breaking**] `BaseClient::set_session_metadata` is renamed
  `activate`, and `BaseClient::logged_in` is renamed `is_activated`
  ([#4850](https://github.com/matrix-org/matrix-rust-sdk/pull/4850))
- [**breaking] `DependentQueuedRequestKind::UploadFileWithThumbnail`
  was renamed to `DependentQueuedRequestKind::UploadFileOrThumbnail`.
  Under the `unstable-msc4274` feature, `DependentQueuedRequestKind::UploadFileOrThumbnail`
  and `SentMediaInfo` were generalized to allow chaining multiple dependent
  file / thumbnail uploads.
  ([#4897](https://github.com/matrix-org/matrix-rust-sdk/pull/4897))
- [**breaking**] `RoomInfo::prev_state` has been removed due to being useless.
  ([#5054](https://github.com/matrix-org/matrix-rust-sdk/pull/5054))

## [0.10.0] - 2025-02-04

### Features

- [**breaking**] `EventCacheStore` allows to control which media content is
  allowed in the media cache, and how long it should be kept, with a
  `MediaRetentionPolicy`:
  - `EventCacheStore::add_media_content()` has an extra argument,
    `ignore_policy`, which decides whether a media content should ignore the
    `MediaRetentionPolicy`. It should be stored alongside the media content.
  - `EventCacheStore` has four new methods: `media_retention_policy()`,
    `set_media_retention_policy()`, `set_ignore_media_retention_policy()` and
    `clean_up_media_cache()`.
  - `EventCacheStore` implementations should delegate media cache methods to the
    methods of the same name of `MediaService` to use the `MediaRetentionPolicy`.
    They need to implement the `EventCacheStoreMedia` trait that can be tested
    with the `event_cache_store_media_integration_tests!` macro.
    ([#4571](https://github.com/matrix-org/matrix-rust-sdk/pull/4571))

### Refactor

- [**breaking**] Replaced `Room::compute_display_name` with the reintroduced
  `Room::display_name()`. The new method computes a display name, or return a
  cached value from the previous successful computation. If you need a sync
  variant, consider using `Room::cached_display_name()`.
  ([#4470](https://github.com/matrix-org/matrix-rust-sdk/pull/4470))
- [**breaking**]: The reexported types `SyncTimelineEvent` and `TimelineEvent`
  have been fused into a single type `TimelineEvent`, and its field
  `push_actions` has been made `Option`al (it is set to `None` when we couldn't
  compute the push actions, because we lacked some information).
  ([#4568](https://github.com/matrix-org/matrix-rust-sdk/pull/4568))

## [0.9.0] - 2024-12-18

### Features

- Introduced support for
  [MSC4171](https://github.com/matrix-org/matrix-rust-sdk/pull/4335), enabling
  the designation of certain users as service members. These flagged users are
  excluded from the room display name calculation.
  ([#4335](https://github.com/matrix-org/matrix-rust-sdk/pull/4335))

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
