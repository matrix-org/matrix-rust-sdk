# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Bug Fixes

- `Client::create_room` now uses `RoomPowerLevelsContentOverride` under the hood instead of 
  `RoomPowerLevelsEventContent` to be able to explicitly set values which would previously be 
  ignored if they matched the default power level values specified by the spec: these may not be 
  the same in the homeserver and result in rooms with incorrect power levels being created.
  ([#6034](https://github.com/matrix-org/matrix-rust-sdk/pull/6034))
- Fix the `is_last_admin` check in `LeaveSpaceRoom` since it was not
  accounting for the membership state.
  [#6032](https://github.com/matrix-org/matrix-rust-sdk/pull/6032)
- [**breaking**] `LatestEventValue::Local { is_sending: bool }` is replaced
  by [`state: LatestEventValueLocalState`] to represent 3 states: `IsSending`,
  `HasBeenSent` and `CannotBeSent`.
  ([#5968](https://github.com/matrix-org/matrix-rust-sdk/pull/5968/))

### Features

- [**breaking**] Extend `TimelineFocus::Event` to allow marking the target
  event as the root of a thread.
  [#6050](https://github.com/matrix-org/matrix-rust-sdk/pull/6050)
- [**breaking**] Remove `TimelineFilter::EventTypeFilter` which has been replaced by
  the more generic `TimelineFilter::EventFilter`. Users of `TimelineEventTypeFilter::include`
  and `TimelineEventTypeFilter::exclude` can switch to `TimelineEventFilter::include_event_types`
  and `TimelineEventFilter::exclude_event_types`.
  ([#6070](https://github.com/matrix-org/matrix-rust-sdk/pull/6070/))
- Add `TimelineFilter::EventFilter` for filtering events based on their type or
  content. For content filtering, only membership and profile change filters
  are available as of now.
  ([#6048](https://github.com/matrix-org/matrix-rust-sdk/pull/6048/))
- Introduce `SpaceFilter`s as a mechanism for narrowing down what's displayed in
  the room list ([#6025](https://github.com/matrix-org/matrix-rust-sdk/pull/6025))
- Expose room power level thresholds in `OtherState::RoomPowerLevels` (ban, kick, invite, redact, state &
  events defaults, per-event overrides, notifications), so clients can compute the required power level
  for actions and compare with previous values. ([#5931](https://github.com/matrix-org/matrix-rust-sdk/pull/5931))
- Add `RoomCreationParameters::is_space` parameter to be able to create spaces. ([#6010](https://github.com/matrix-org/matrix-rust-sdk/pull/6010/))
- [**breaking**] `LazyTimelineItemProvider::get_shields` no longer returns an
  an `Option`: the `ShieldState` type contains a `None` variant, so the
  `Option` was redundant. The `message` field has also been removed: since there
  was no way to localise the returned string, applications should not be using it.
  ([#5959](https://github.com/matrix-org/matrix-rust-sdk/pull/5959))
- Add `Room::list_threads` to list all the threads in a room.
  ([#5953](https://github.com/matrix-org/matrix-rust-sdk/pull/5953))
- Add `SpaceService::get_space_room` to get a space given its id from the space graph if available.
[#5944](https://github.com/matrix-org/matrix-rust-sdk/pull/5944)
- Add `QrCodeData::to_bytes()` to allow generation of a QR code.
  ([#5939](https://github.com/matrix-org/matrix-rust-sdk/pull/5939))
- [**breaking**]: The new Latest Event API replaces the old API.
  `Room::new_latest_event` overwrites the `Room::latest_event` method. See the
  documentation of `matrix_sdk::latest_event` to learn about the new API.
  [#5624](https://github.com/matrix-org/matrix-rust-sdk/pull/5624/)
- Created `RoomPowerLevels::events` function which returns a `HashMap<TimelineEventType, i64>` with all the power 
  levels per event type. ([#5937](https://github.com/matrix-org/matrix-rust-sdk/pull/5937))
- Expose `EventTimelineItem::forwarder` and `forwarder_profile`, which, if present, provide the ID and profile of
  the user who forwarded the keys used to decrypt the event as part of an [MSC4268](https://github.com/matrix-org/matrix-spec-proposals/pull/4268)
  key bundle.
  ([#6000](https://github.com/matrix-org/matrix-rust-sdk/pull/6000))
- Add `NonFavorite` filter to the Room List API. ([#5991](https://github.com/matrix-org/matrix-rust-sdk/pull/5991))
  
### Refactor

- [**breaking**] Refactored `is_last_admin` to `is_last_owner` the check will now
  account also for v12 rooms, where creators and users with PL 150 matter.
  ([#6036](https://github.com/matrix-org/matrix-rust-sdk/pull/6036))
- [**breaking**] The existing `TimelineEventType` was renamed to `TimelineEventContent`, because it contained the 
  actual contents of the event. Then, we created a new `TimelineEventType` enum that actually contains *just* the 
  event type. ([#5937](https://github.com/matrix-org/matrix-rust-sdk/pull/5937))
- [**breaking**] The function `TimelineEvent::event_type` is now `TimelineEvent::content`. 
  ([#5937](https://github.com/matrix-org/matrix-rust-sdk/pull/5937))
- [**breaking**] The `SpaceService` will no longer auto-subscribe to required
  client events when invoking the `subscribe_to_joined_spaces` but instead do it
  through its, now async, constructor.
  ([#5972](https://github.com/matrix-org/matrix-rust-sdk/pull/5972))
- [**breaking**] The `SpaceService`'s `joined_spaces` method has been renamed
  `top_level_joined_spaces` and `subscribe_to_joined_spaces` to `space_service.subscribe_to_top_level_joined_spaces`
  ([#5972](https://github.com/matrix-org/matrix-rust-sdk/pull/5972))

## [0.16.0] - 2025-12-04

### Breaking changes

- `TimelineConfiguration::track_read_receipts`'s type is now an enum to allow tracking to be enabled for all events
  (like before) or only for message-like events (which prevents read receipts from being placed on state events).
  ([#5900](https://github.com/matrix-org/matrix-rust-sdk/pull/5900))
- `Client::reset_server_info()` has been split into `reset_supported_versions()`
  and `reset_well_known()`.
  ([#5910](https://github.com/matrix-org/matrix-rust-sdk/pull/5910))
- Add `HumanQrLoginError::NotFound` for non-existing / expired rendezvous sessions
  ([#5898](https://github.com/matrix-org/matrix-rust-sdk/pull/5898))
- Add `HumanQrGrantLoginError::NotFound` for non-existing / expired rendezvous sessions
  ([#5898](https://github.com/matrix-org/matrix-rust-sdk/pull/5898))
- The `LatestEventValue::Local` type gains 2 new fields: `sender` and `profile`.
  ([#5885](https://github.com/matrix-org/matrix-rust-sdk/pull/5885))
- The `Encryption::user_identity()` method has received a new argument. The
  `fallback_to_server` argument controls if we should attempt to fetch the user
  identity from the homeserver if it wasn't found in the local storage.
  ([#5870](https://github.com/matrix-org/matrix-rust-sdk/pull/5870))
- Expose the power level required to modify `m.space.child` on
  `room::power_levels::RoomPowerLevelsValues`.
- Rename `Client::login_with_qr_code` to `Client::new_login_with_qr_code_handler`.
  ([#5836](https://github.com/matrix-org/matrix-rust-sdk/pull/5836))
- Add the `sqlite` feature, along with the `indexeddb` feature, to enable either
  the SQLite or IndexedDB store. The `session_paths`, `session_passphrase`,
  `session_pool_max_size`, `session_cache_size` and `session_journal_size_limit`
  methods on `ClientBuilder` have been removed. New methods are added:
  `ClientBuilder::in_memory_store` if one wants non-persistent stores,
  `ClientBuilder::sqlite_store` to configure and to use SQLite stores (if
  the `sqlite` feature is enabled), and `ClientBuilder::indexeddb_store` to
  configure and to use IndexedDB stores (if the `indexeddb` feature is enabled).
  ([#5811](https://github.com/matrix-org/matrix-rust-sdk/pull/5811))

  The code:

  ```rust
  client_builder
      .session_paths("data_path", "cache_path")
      .passphrase("foobar")
  ```

  now becomes:

  ```rust
  client_builder
      .sqlite_store(
          SqliteSessionStoreBuilder::new("data_path", "cache_path")
              .passphrase("foobar")
      )
  ```

- UniFFI was upgraded to `v0.30.0` ([#5808](https://github.com/matrix-org/matrix-rust-sdk/pull/5808)).
- The `waveform` parameter in `Timeline::send_voice_message` format changed to a list of `f32`
  between 0 and 1.
  ([#5732](https://github.com/matrix-org/matrix-rust-sdk/pull/5732))
- The `normalized_power_level` field has been removed from the `RoomMember`
  struct.
  ([#5635](https://github.com/matrix-org/matrix-rust-sdk/pull/5635))
- Remove the deprecated `CallNotify` event (`org.matrix.msc4075.call.notify`) in favor of the new
  `RtcNotification` event (`org.matrix.msc4075.rtc.notification`).
  ([#5668](https://github.com/matrix-org/matrix-rust-sdk/pull/5668))
- Add `QrLoginProgress::SyncingSecrets` to indicate that secrets are being synced between the two
  devices.
  ([#5760](https://github.com/matrix-org/matrix-rust-sdk/pull/5760))
- Add `Room::subscribe_to_send_queue_updates` to observe room send queue updates.
  ([#5761](https://github.com/matrix-org/matrix-rust-sdk/pull/5761))
- `Client::login_with_qr_code` now returns a handler that allows performing the flow with either the
  current device scanning or generating the QR code. Additionally, new errors `HumanQrLoginError::CheckCodeAlreadySent`
  and `HumanQrLoginError::CheckCodeCannotBeSent` were added.
  ([#5786](https://github.com/matrix-org/matrix-rust-sdk/pull/5786))
- `ComposerDraft` now includes attachments alongside the text message.
  ([#5794](https://github.com/matrix-org/matrix-rust-sdk/pull/5794))
- Add `Client::subscribe_to_send_queue_updates` to observe global send queue updates.
  ([#5784](https://github.com/matrix-org/matrix-rust-sdk/pull/5784))

### Features

- Add `Client::get_store_sizes()` so to query the size of the existing stores, if available. ([#5911](https://github.com/matrix-org/matrix-rust-sdk/pull/5911))
- Expose `is_space` in `NotificationRoomInfo`, allowing clients to determine if the room that triggered the notification is a space.
- Add push actions to `NotificationItem` and replace `SyncNotification` with `NotificationItem`.
  ([#5835](https://github.com/matrix-org/matrix-rust-sdk/pull/5835))
- Add `Client::new_grant_login_with_qr_code_handler` for granting login to a new device by way of
  a QR code.
  ([#5836](https://github.com/matrix-org/matrix-rust-sdk/pull/5836))
- Add `Client::register_notification_handler` for observing notifications generated from sync responses.
  ([#5831](https://github.com/matrix-org/matrix-rust-sdk/pull/5831))
- Add `Room::mark_as_fully_read_unchecked` so clients can mark a room as read without needing a `Timeline` instance. Note this method is not recommended as it can potentially cause incorrect read receipts, but it can needed in certain cases.
- Add `Timeline::latest_event_id` to be able to fetch the event id of the latest event of the timeline.
- Add `Room::load_or_fetch_event` so we can get a `TimelineEvent` given its event id ([#5678](https://github.com/matrix-org/matrix-rust-sdk/pull/5678)).
- Add `TimelineEvent::thread_root_event_id` to expose the thread root event id for this type too ([#5678](https://github.com/matrix-org/matrix-rust-sdk/pull/5678)).
- Add `NotificationSettings::get_raw_push_rules` so clients can fetch the raw JSON content of the push rules of the current user and include it in bug reports ([#5706](https://github.com/matrix-org/matrix-rust-sdk/pull/5706)).
- Add new API to decline calls ([MSC4310](https://github.com/matrix-org/matrix-spec-proposals/pull/4310)): `Room::decline_call` and `Room::subscribe_to_call_decline_events`
  ([#5614](https://github.com/matrix-org/matrix-rust-sdk/pull/5614))
- Expose `m.federate` in `OtherState::RoomCreate` and `history_visibility` in `OtherState::RoomHistoryVisibility`, allowing clients to know whether a room federates and how its history is shared in the appropriate timeline events.
- Expose `join_rule` in `OtherState::RoomJoinRules`, allowing clients to know the join rules of a room from the appropriate timeline events.

### Changes

- `Timeline::latest_event_id` now uses its `ui::Timeline::latest_event_id` counterpart, instead of getting the latest event from the timeline and then its id.([#5864](https://github.com/matrix-org/matrix-rust-sdk/pull/5864))
- Build Android ARM64 bindings using better default RUSTFLAGS (the same used for iOS ARM64). This should improve performance. [(#5854)](https://github.com/matrix-org/matrix-rust-sdk/pull/5854)

## [0.14.0] - 2025-09-04

### Features:

- Add `LowPriority` and `NonLowPriority` variants to `RoomListEntriesDynamicFilterKind` for filtering
  rooms based on their low priority status. These filters allow clients to show only low priority rooms
  or exclude low priority rooms from the room list.
  ([#5508](https://github.com/matrix-org/matrix-rust-sdk/pull/5508))
- Add `room_version` and `privileged_creators_role` to `RoomInfo` ([#5449](https://github.com/matrix-org/matrix-rust-sdk/pull/5449)).
- The [`unstable-hydra`] feature has been enabled, which enables room v12 changes in the SDK.
  ([#5450](https://github.com/matrix-org/matrix-rust-sdk/pull/5450)).
- Add experimental support for
  [MSC4306](https://github.com/matrix-org/matrix-spec-proposals/pull/4306), with the
  `Room::fetch_thread_subscription()` and `Room::set_thread_subscription()` methods.
  ([#5442](https://github.com/matrix-org/matrix-rust-sdk/pull/5442))
- [**breaking**] [`GalleryUploadParameters::reply`] and [`UploadParameters::reply`] have been both
  replaced with a new optional `in_reply_to` field, that's a string which will be parsed into an
  `OwnedEventId` when sending the event. The thread relationship will be automatically filled in,
  based on the timeline focus.
  ([5427](https://github.com/matrix-org/matrix-rust-sdk/pull/5427))
- [**breaking**] [`Timeline::send_reply()`] now automatically fills in the thread relationship,
  based on the timeline focus. As a result, it only takes an `OwnedEventId` parameter, instead of
  the `Reply` type. The proper way to start a thread is now thus to create a threaded-focused
  timeline, and then use `Timeline::send()`.
  ([5427](https://github.com/matrix-org/matrix-rust-sdk/pull/5427))
- Add `HomeserverLoginDetails::supports_sso_login` for legacy SSO support information.
  This is primarily for Element X to give a dedicated error message in case
  it connects a homeserver with only this method available.
  ([#5222](https://github.com/matrix-org/matrix-rust-sdk/pull/5222))

### Breaking changes:

- The timeline will now always use the send queue to upload medias, so the
  `UploadParameters::use_send_queue` bool has been removed. Make sure to listen to the send queue's
  error updates, and to handle send queue restarts.
  ([#5525](https://github.com/matrix-org/matrix-rust-sdk/pull/5525))
- Support for the legacy media upload progress has been disabled. Media upload progress is
  available through the send queue, and can be enabled thanks to
  `Client::enable_send_queue_upload_progress()`.
  ([#5525](https://github.com/matrix-org/matrix-rust-sdk/pull/5525))
- `TimelineDiff` is now exported as a true `uniffi::Enum` instead of the weird `uniffi::Object` hybrid. This matches
  both `RoomDirectorySearchEntryUpdate` and `RoomListEntriesUpdate` and can be used in the same way.
  ([#5474](https://github.com/matrix-org/matrix-rust-sdk/pull/5474))
- The `creator` field of `RoomInfo` has been renamed to `creators` and can now contain a list of
  user IDs, to reflect that a room can now have several creators, as introduced in room version 12.
  ([#5436](https://github.com/matrix-org/matrix-rust-sdk/pull/5436))
- The `PowerLevel` type was introduced to represent power levels instead of `i64` to differentiate
  the infinite power level of creators, as introduced in room version 12. It is used in
  `suggested_role_for_power_level`, `suggested_power_level_for_role` and `RoomMember`.
  ([#5436](https://github.com/matrix-org/matrix-rust-sdk/pull/5436))
- `Client::get_url` now returns a `Vec<u8>` instead of a `String`. It also throws an error when the
  response isn't status code 200 OK, instead of providing the error in the response body.
  ([#5438](https://github.com/matrix-org/matrix-rust-sdk/pull/5438))
- `RoomPreview::info()` doesn't return a result anymore. All unknown join rules are handled in the
  `JoinRule::Custom` variant.
  ([#5337](https://github.com/matrix-org/matrix-rust-sdk/pull/5337))
- The `reason` argument of `Room::report_room` is now required, do to a clarification in the spec.
  ([#5337](https://github.com/matrix-org/matrix-rust-sdk/pull/5337))
- `PublicRoomJoinRule` has more variants, supporting all the known values from the spec.
  ([#5337](https://github.com/matrix-org/matrix-rust-sdk/pull/5337))
- The fields of `MediaPreviewConfig` are both optional, allowing to use the type for room account
  data as well as global account data.
  ([#5337](https://github.com/matrix-org/matrix-rust-sdk/pull/5337))
- The `event_id` field of `PredecessorRoom` was removed, due to its removal in the Matrix
  specification with MSC4291.
  ([#5419](https://github.com/matrix-org/matrix-rust-sdk/pull/5419))
- `Client::url_for_oidc` now allows requesting additional scopes for the OAuth2 authorization code grant.
  ([#5395](https://github.com/matrix-org/matrix-rust-sdk/pull/5395))
- `Client::url_for_oidc` now allows passing an optional existing device id from a previous login call.
  ([#5394](https://github.com/matrix-org/matrix-rust-sdk/pull/5394))
- `ClientBuilder::build_with_qr_code` has been removed. Instead, the Client should be built by passing
  `QrCodeData::server_name` to `ClientBuilder::server_name_or_homeserver_url`, after which QR login can be performed by
  calling `Client::login_with_qr_code`. ([#5388](https://github.com/matrix-org/matrix-rust-sdk/pull/5388))
- The MSRV has been bumped to Rust 1.88.
  ([#5431](https://github.com/matrix-org/matrix-rust-sdk/pull/5431))
- `Room::send_call_notification` and `Room::send_call_notification_if_needed` have been removed, since the event type they send is outdated, and `Client` is not actually supposed to be able to join MatrixRTC sessions (yet). In practice, users of these methods probably already rely on another MatrixRTC implementation to participate in sessions, and such an implementation should be capable of sending notifications itself.
- The `GalleryItemInfo` variants now take an `UploadSource` rather than a `String` path to enable uploading
  from bytes directly.
  ([#5529](https://github.com/matrix-org/matrix-rust-sdk/pull/5529))
- Media and gallery uploads now use `UploadSource` to specify the thumbnail.
  ([#5530](https://github.com/matrix-org/matrix-rust-sdk/pull/5530))

## [0.13.0] - 2025-07-10

### Features

- Add `NotificationRoomInfo::topic` to the `NotificationRoomInfo` struct, which
  contains the topic of the room. This is useful for displaying the room topic
  in notifications. ([#5300](https://github.com/matrix-org/matrix-rust-sdk/pull/5300))
- Add `EmbeddedEventDetails::timestamp` and `EmbeddedEventDetails::event_or_transaction_id`
  which are already available in regular timeline items.
  ([#5331](https://github.com/matrix-org/matrix-rust-sdk/pull/5331))
- `RoomListService::subscribe_to_rooms` becomes `async` and automatically calls
  `matrix_sdk::latest_events::LatestEvents::listen_to_room`
  ([#5369](https://github.com/matrix-org/matrix-rust-sdk/pull/5369))

### Refactor

- Adjust features in the `matrix-sdk-ffi` crate to expose more platform-specific knobs.
  Previously the `matrix-sdk-ffi` was configured primarily by target configs, choosing
  between the tls flavor (`rustls-tls` or `native-tls`) and features like `sentry` based
  purely on the target. As we work to add an additional Wasm target to this crate,
  the cross product of target specific features has become somewhat chaotic, and we
  have shifted to externalize these choices as feature flags.

  To maintain existing compatibility on the major platforms, these features should be used:
  Android: `"bundled-sqlite,unstable-msc4274,rustls-tls,sentry"`
  iOS: `"bundled-sqlite,unstable-msc4274,native-tls,sentry"`
  Javascript/Wasm: `"unstable-msc4274,native-tls"`

  In the future additional choices (such as session storage, `sqlite` and `indexeddb`)
  will likely be added as well.

Breaking changes:

- `Client::reset_server_capabilities` has been renamed to `Client::reset_server_info`.
  ([#5167](https://github.com/matrix-org/matrix-rust-sdk/pull/5167))
- `RoomPreview::join_rule`, `NotificationItem::join_rule`, `RoomInfo::is_public`, and
  `Room::is_public()` return values are now optional. They will be set to `None` if the join rule
  state event is missing for a given room. `NotificationRoomInfo::is_public` has been removed;
  callers can inspect the value of `NotificationItem::join_rule` to determine if the room is public
  (i.e. if the join rule is `Public`).
  ([#5278](https://github.com/matrix-org/matrix-rust-sdk/pull/5278))

## [0.12.0] - 2025-06-10

Breaking changes:

- `Client::send_call_notification_if_needed` now returns `Result<bool>` instead of `Result<()>` so we can check if
  the event was sent.
- `Client::upload_avatar` and `Timeline::send_attachment` now may fail if a file too large for the homeserver media
  config is uploaded.
- `UploadParameters` replaces field `filename: String` with `source: UploadSource`.
  `UploadSource` is an enum which may take a filename or a filename and bytes, which
  allows a foreign language to read file contents natively and then pass those contents to
  the foreign function when uploading a file through the `Timeline`.
  ([#4948](https://github.com/matrix-org/matrix-rust-sdk/pull/4948))
- `RoomInfo` replaces its field `is_tombstoned: bool` with `tombstone: Option<RoomTombstoneInfo>`,
  containing the data needed to implement the room migration UI, a message and the replacement room id.
  ([#5027](https://github.com/matrix-org/matrix-rust-sdk/pull/5027))

Additions:

- `Client::subscribe_to_room_info` allows clients to subscribe to room info updates in rooms which may not be known yet.
  This is useful when displaying a room preview for an unknown room, so when we receive any membership change for it,
  we can automatically update the UI.
- `Client::get_max_media_upload_size` to get the max size of a request sent to the homeserver so we can tweak our media
  uploads by compressing/transcoding the media.
- Add `ClientBuilder::enable_share_history_on_invite` to enable experimental support for sharing encrypted room history
  on invite, per [MSC4268](https://github.com/matrix-org/matrix-spec-proposals/pull/4268).
  ([#5141](https://github.com/matrix-org/matrix-rust-sdk/pull/5141))
- Support for adding a Sentry layer to the FFI bindings has been added. Only `tracing` statements with
  the field `sentry=true` will be forwarded to Sentry, in addition to default Sentry filters.
- Add room topic string to `StateEventContent`
- Add `UploadSource` for representing upload data - this is analogous to `matrix_sdk_ui::timeline::AttachmentSource`
- Add `Client::observe_account_data_event` and `Client::observe_room_account_data_event` to
  subscribe to global and room account data changes.
  ([#4994](https://github.com/matrix-org/matrix-rust-sdk/pull/4994))
- Add `Timeline::send_gallery` to send MSC4274-style galleries.
  ([#5163](https://github.com/matrix-org/matrix-rust-sdk/pull/5163))
- Add `reply_params` to `GalleryUploadParameters` to allow sending galleries as (threaded) replies.
  ([#5173](https://github.com/matrix-org/matrix-rust-sdk/pull/5173))

Breaking changes:

- `contacts` has been removed from `OidcConfiguration` (it was unused since the switch to OAuth).

## [0.11.0] - 2025-04-11

Breaking changes:

- `TracingConfiguration` now includes a new field `trace_log_packs`, which gives a convenient way
  to set the TRACE log level for multiple targets related to a given feature.
  ([#4824](https://github.com/matrix-org/matrix-rust-sdk/pull/4824))

- `setup_tracing` has been renamed `init_platform`; in addition to the `TracingConfiguration`
  parameter it also now takes a boolean indicating whether to spawn a minimal tokio runtime for the
  application; in general for main app processes this can be set to `false`, and memory-constrained
  programs can set it to `true`.

- Matrix client API errors coming from API responses will now be mapped to `ClientError::MatrixApi`, containing both the
  original message and the associated error code and kind.

- `EventSendState` now has two additional variants: `CrossSigningNotSetup` and
  `SendingFromUnverifiedDevice`. These indicate that your own device is not
  properly cross-signed, which is a requirement when using the identity-based
  strategy, and can only be returned when using the identity-based strategy.

  In addition, the `VerifiedUserHasUnsignedDevice` and
  `VerifiedUserChangedIdentity` variants can be returned when using the
  identity-based strategy, in addition to when using the device-based strategy
  with `error_on_verified_user_problem` is set.

- `EventSendState` now has two additional variants: `VerifiedUserHasUnsignedDevice` and
  `VerifiedUserChangedIdentity`. These reflect problems with verified users in the room
  and as such can only be returned when the room key recipient strategy has
  `error_on_verified_user_problem` set.

- The `AuthenticationService` has been removed:
    - Instead of calling `configure_homeserver`, build your own client with the `serverNameOrHomeserverUrl` builder
      method to keep the same behaviour.
        - The parts of `AuthenticationError` related to discovery will be represented in the `ClientBuildError` returned
          when calling `build()`.
    - The remaining methods can be found on the built `Client`.
        - There is a new `abortOidcLogin` method that should be called if the webview is dismissed without a callback (
          or fails to present).
        - The rest of `AuthenticationError` is now found in the OidcError type.

- `OidcAuthenticationData` is now called `OidcAuthorizationData`.

- The `get_element_call_required_permissions` function now requires the device_id.

- Some `OidcPrompt` cases have been removed (`None`, `SelectAccount`).

- `Room::is_encrypted` is replaced by `Room::latest_encryption_state`
  which returns a value of the new `EncryptionState` enum; another
  `Room::encryption_state` non-async and infallible method is added to get the
  `EncryptionState` without running a network request.
  ([#4777](https://github.com/matrix-org/matrix-rust-sdk/pull/4777)). One can
  safely replace:

  ```rust
  room.is_encrypted().await?
  ```

  by

  ```rust
  room.latest_encryption_state().await?.is_encrypted()
  ```

- `ClientBuilder::passphrase` is renamed `session_passphrase`
  ([#4870](https://github.com/matrix-org/matrix-rust-sdk/pull/4870/))

- Merge `Timeline::send_thread_reply` into `Timeline::send_reply`. This
  changes the parameters of `send_reply` which now requires passing the
  event ID (and thread reply behaviour) inside a `ReplyParameters` struct.
  ([#4880](https://github.com/matrix-org/matrix-rust-sdk/pull/4880/))

- The `dynamic_registrations_file` field of `OidcConfiguration` was removed.
  Clients are supposed to re-register with the homeserver for every login.

- `RoomPreview::own_membership_details` is now `RoomPreview::member_with_sender_info`, takes any user id and returns an
  `Option<RoomMemberWithSenderInfo>`.

Additions:

- Add `Encryption::get_user_identity` which returns `UserIdentity`
- Add `ClientBuilder::room_key_recipient_strategy`
- Add `Room::send_raw`
- Add `NotificationSettings::set_custom_push_rule`
- Expose `withdraw_verification` to `UserIdentity`
- Expose `report_room` to `Room`
- Add `RoomInfo::encryption_state`
  ([#4788](https://github.com/matrix-org/matrix-rust-sdk/pull/4788))
- Add `Timeline::send_thread_reply` for clients that need to start threads
  themselves.
  ([4819](https://github.com/matrix-org/matrix-rust-sdk/pull/4819))
- Add `ClientBuilder::session_pool_max_size`, `::session_cache_size` and `::session_journal_size_limit` to control the
  stores configuration, especially their memory consumption
  ([#4870](https://github.com/matrix-org/matrix-rust-sdk/pull/4870/))
- Add `ClientBuilder::system_is_memory_constrained` to indicate that the system
  has less memory available than the current standard
  ([#4894](https://github.com/matrix-org/matrix-rust-sdk/pull/4894))
- Add `Room::member_with_sender_info` to get both a room member's info and for the user who sent the `m.room.member`
  event the `RoomMember` is based on.
