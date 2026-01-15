# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Bug Fixes

- [**breaking**] New `LatestEventValue::LocalHasBeenSent` variant to represent
  a local event that has been sent successfully.
  ([#5968](https://github.com/matrix-org/matrix-rust-sdk/pull/5968))

### Features

- Add `StateStore::upsert_thread_subscriptions()` method for bulk upserts.
  ([#5848](https://github.com/matrix-org/matrix-rust-sdk/pull/5848))
- The `LatestEventValue::LocalHasBeenSent` variant gains a new `event_id:
  OwnedEventId` field.
  ([#5977](https://github.com/matrix-org/matrix-rust-sdk/pull/5977))

### Refactor

- [**breaking**] The `StateStore::upsert_thread_subscription` method has been removed in favor of a
  bulk method `StateStore::upsert_thread_subscriptions`.
- [**breaking**] The `message-ids` feature has been removed. It was already a no-op and has now
  been eliminated entirely.
  ([#5963](https://github.com/matrix-org/matrix-rust-sdk/pull/5963))

## [0.16.0] - 2025-12-04

### Security Fixes

- Skip the serialization of custom join rules in the `RoomInfo` which prevented
  the processing of sync responses containing events with custom join rules.
  ([#5924](https://github.com/matrix-org/matrix-rust-sdk/pull/5924)) (Low, [CVE-2025-66622](https://www.cve.org/CVERecord?id=CVE-2025-66622), [GHSA-jj6p-3m75-g2p3](https://github.com/matrix-org/matrix-rust-sdk/security/advisories/GHSA-jj6p-3m75-g2p3)).

### Refactor

- [**breaking**] `ServerInfo` has been renamed to `SupportedVersionsResponse`,
  and its `well_known` field has been removed. It is also wrapped in a
  `TtlStoreValue` that handles the expiration of the data, rather than calling
  `maybe_decode()`. Its constructor has been removed since all its fields are
  now public.
  ([#5910](https://github.com/matrix-org/matrix-rust-sdk/pull/5910))
  - `StateStoreData(Key/Value)::ServerInfo` has been split into the
    `SupportedVersions` and `WellKnown` variants.
- [**breaking**] Upgrade Ruma to version 0.14.0.
  ([#5882](https://github.com/matrix-org/matrix-rust-sdk/pull/5882))
- `Client::sync_lock` has been renamed `Client::state_store_lock`.
  ([#5707](https://github.com/matrix-org/matrix-rust-sdk/pull/5707))

### Features

- [**breaking**] The `EventCacheStore::get_room_events()` method has received
  two new arguments. This allows users to load only events of a certain event
  type and events that were encrypted using a certain room key identified by its
  session ID.
  ([#5817](https://github.com/matrix-org/matrix-rust-sdk/pull/5817))
- `ComposerDraft` can now store attachments alongside text messages.
  ([#5794](https://github.com/matrix-org/matrix-rust-sdk/pull/5794))

## [0.14.1] - 2025-09-10

### Security Fixes

- Fix a panic in the `RoomMember::normalized_power_level` method.
  ([#5635](https://github.com/matrix-org/matrix-rust-sdk/pull/5635)) (Low, [CVE-2025-59047](https://www.cve.org/CVERecord?id=CVE-2025-59047), [GHSA-qhj8-q5r6-8q6j](https://github.com/matrix-org/matrix-rust-sdk/security/advisories/GHSA-qhj8-q5r6-8q6j)).

## [0.14.0] - 2025-09-04

### Features
- Add `SyncResponse::RoomUpdates::is_empty` to check if there were any room updates.
  ([#5593](https://github.com/matrix-org/matrix-rust-sdk/pull/5593))
- Add `EncryptionState::StateEncrypted` to represent rooms supporting encrypted
  state events. Feature-gated behind `experimental-encrypted-state-events`.
  ([#5523](https://github.com/matrix-org/matrix-rust-sdk/pull/5523))
- [**breaking**] The `state` field of `JoinedRoomUpdate` and `LeftRoomUpdate`
  now uses the `State` enum, depending on whether the state changes were
  received in the `state` field or the `state_after` field.
  ([#5488](https://github.com/matrix-org/matrix-rust-sdk/pull/5488))
- [**breaking**] `RoomCreateWithCreatorEventContent` has a new field
  `additional_creators` that allows to specify additional room creators beside
  the user sending the `m.room.create` event, introduced with room version 12.
  ([#5436](https://github.com/matrix-org/matrix-rust-sdk/pull/5436))
- [**breaking**] The `RoomInfo` method now remembers the inviter at the time
  when the `BaseClient::room_joined()` method was called. The caller is
  responsible to remember the inviter before a server request to join the room
  is made. The  `RoomInfo::invite_accepted_at` method was removed, the
  `RoomInfo::invite_details` method returns both the timestamp and the
  inviter.
  ([#5390](https://github.com/matrix-org/matrix-rust-sdk/pull/5390))

### Refactor
- [**breaking**] The `Stripped` variants of `RawAnySyncOrStrippedTimelineEvent`,
  `RawAnySyncOrStrippedState` and `AnySyncOrStrippedState` use `StrippedState`
  instead of `AnyStrippedStateEvent`.
  ([#5473](https://github.com/matrix-org/matrix-rust-sdk/pull/5473))
- [**breaking**] The `stripped_state` field of `StateChanges` uses
  `StrippedState` instead of `AnyStrippedStateEvent`.
  ([#5473](https://github.com/matrix-org/matrix-rust-sdk/pull/5473))
- [**breaking**] `RelationalLinkedChunk::items` now takes a `RoomId` instead of an
  `&OwnedLinkedChunkId` parameter.
  ([#5445](https://github.com/matrix-org/matrix-rust-sdk/pull/5445))
- [**breaking**] Add an `IsPrefix = False` bound to the
  `get_state_event_static()`, `get_state_event_static_for_key()` and
  `get_state_events_static()`, `get_account_data_event_static()` and
  `get_room_account_data_event_static` methods of `StateStoreExt`. These methods
  only worked for events where the full event type is statically-known, and this
  is now enforced at compile-time. The matching non-`static` methods of
  `StateStore` can be used instead for event types with a variable suffix.
  ([#5444](https://github.com/matrix-org/matrix-rust-sdk/pull/5444))
- [**breaking**] `SyncOrStrippedState<RoomPowerLevelsEventContent>::power_levels()`
  takes `AuthorizationRules` and a list of creators, because creators can have
  infinite power levels, as introduced in room version 12.
  ([#5436](https://github.com/matrix-org/matrix-rust-sdk/pull/5436))
- [**breaking**] `RoomMember::power_level()` and
  `RoomMember::normalized_power_level()` now use `UserPowerLevel` to represent
  power levels instead of `i64` to differentiate the infinite power level of
  creators, as introduced in room version 12.
  ([#5436](https://github.com/matrix-org/matrix-rust-sdk/pull/5436))
- [**breaking**] The `creator()` methods of `Room` and `RoomInfo` have been
  renamed to `creators()` and can now return a list of user IDs, to reflect that
  a room can have several creators, as introduced in room version 12.
  ([#5436](https://github.com/matrix-org/matrix-rust-sdk/pull/5436))
- [**breaking**] `RoomInfo::room_version_or_default()` was replaced with
  `room_version_rules_or_default()`. The room version should only be used for
  display purposes. The rules contain flags for all the differences in behavior
  between all known room versions.
  ([#5337](https://github.com/matrix-org/matrix-rust-sdk/pull/5337))
- [**breaking**] `MinimalStateEvent::redact()` takes `RedactionRules` instead of
  a `RoomVersionId`.
  ([#5337](https://github.com/matrix-org/matrix-rust-sdk/pull/5337))
- [**breaking**] The `event_id` field of `PredecessorRoom` was removed, due to
  its removal in the Matrix specification with MSC4291.
  ([#5419](https://github.com/matrix-org/matrix-rust-sdk/pull/5419))

## [0.13.0] - 2025-07-10

### Features
- The `RoomInfo` now remembers when an invite was explicitly accepted when the
  `BaseClient::room_joined()` method was called. A new getter for this
  timestamp exists, the `RoomInfo::invite_accepted_at()` method returns this
  timestamp.
  ([#5333](https://github.com/matrix-org/matrix-rust-sdk/pull/5333))
- [**breaking**] The `BaseClient::new()` method now takes an additional `ThreadingSupport`
  parameter controlling whether the client is supposed to do extra processing for threads. Right
  now, it controls whether to exclude in-thread events from the room unread counts, but it may be
  expanded in the future to support more threading-related features.
  ([#5325](https://github.com/matrix-org/matrix-rust-sdk/pull/5325))

### Refactor

- The cached `ServerCapabilities` has been renamed to `ServerInfo` and
  additionally contains the well-known response alongside the existing server versions.
  Despite the old name, it does not contain the server capabilities.
  ([#5167](https://github.com/matrix-org/matrix-rust-sdk/pull/5167))
- `Room::join_rule` and `Room::is_public` now return an `Option` to reflect that the join rule
  state event might be missing, in which case they will return `None`.
  ([#5278](https://github.com/matrix-org/matrix-rust-sdk/pull/5278))

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
