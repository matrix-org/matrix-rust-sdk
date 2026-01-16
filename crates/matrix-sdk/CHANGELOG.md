# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Features

- Add `Room::set_own_member_display_name` to set the current user's display name
  within only the one single room (can be used for /myroomnick functionality).
  [#5981](https://github.com/matrix-org/matrix-rust-sdk/pull/5981)
- Sending `MessageLike` and `RawMessageLike` events through a `Room` now returns
  the used `EncryptionInfo`, if any.
  ([#5936](https://github.com/matrix-org/matrix-rust-sdk/pull/5936))
- [**breaking**]: The new Latest Event API replaces the old API. All the
  `new_` prefixes have been removed, thus `Room::new_latest_event` becomes
  and overwrites the `Room::latest_event` value. The new Latest Event values
  stored in `RoomInfo` are also erased once during the first update of the
  SDK. The new values will be re-calculated. The following types or functions
  are removed: `PossibleLatestEvent`, `is_suitable_for_latest_event`, and
  `LatestEvent` (replaced by `LatestEventValue`). See the documentation of
  `matrix_sdk::latest_event` to learn about the new API.
  ([#5624](https://github.com/matrix-org/matrix-rust-sdk/pull/5624/))
- Expose a new method `RoomEventCache::find_event_relations` for loading
  events relating to a specific event ID from the cache.
  ([#5930](https://github.com/matrix-org/matrix-rust-sdk/pull/5930/))
- Replace in-memory stores with IndexedDB implementations when initializing
  `Client` with `BuilderStoreConfig::IndexedDb`.
  ([#5946](https://github.com/matrix-org/matrix-rust-sdk/pull/5946))
- Call: Add support for the new Intents for voice only calls `Intent.StartCallDmVoice`
  and `Intent.JoinExistingDmVoice`.
  ([#6003](https://github.com/matrix-org/matrix-rust-sdk/pull/6003))
- Add `SlidingSync::unsubscribe_to_rooms` and
  `SlidingSync::clear_and_subscribe_to_rooms`.
  ([#6012](https://github.com/matrix-org/matrix-rust-sdk/pull/6012))
- [**breaking**] Sliding Sync has a new `PollTimeout` type, used by
  `SlidingSyncBuilder::requires_timeout`.
  ([#6005](https://github.com/matrix-org/matrix-rust-sdk/pull/6005))
- Inviting a user to a room with `Client::enable_share_history_on_invite` set
  to true will now trigger a download of all historical keys for the room in
  question from the client's key backup.
  ([#6017](https://github.com/matrix-org/matrix-rust-sdk/pull/6017))

### Bugfix

- Add manual WAL checkpoints when opening Sqlite DBs and when vacuuming them, since the WAL files aren't automatically shrinking. ([#6004](https://github.com/matrix-org/matrix-rust-sdk/pull/6004))
- Use the server name extracted from the user id in `Client::fetch_client_well_known` as a fallback value. Otherwise, sometimes the server name is not available and we can't reload the well-known contents. ([#5996](https://github.com/matrix-org/matrix-rust-sdk/pull/5996))
- Latest Event is lazier: a `RoomLatestEvents` can be registered even if its
  associated `RoomEventCache` isn't created yet.
  ([#5947](https://github.com/matrix-org/matrix-rust-sdk/pull/5947))
- Allow granting of QR login to a new client whose device ID is not a base64
  encoded Curve25519 public key.
  ([#5940](https://github.com/matrix-org/matrix-rust-sdk/pull/5940))

## [0.16.0] - 2025-12-04

### Features

- Add `Client::get_store_sizes()` so to query the size of the existing stores, if available. ([#5911](https://github.com/matrix-org/matrix-rust-sdk/pull/5911))
- Add `QRCodeLoginError::NotFound` for non-existing / expired rendezvous sessions
  ([#5898](https://github.com/matrix-org/matrix-rust-sdk/pull/5898))
- Add `QRCodeGrantLoginError::NotFound` for non-existing / expired rendezvous sessions
  ([#5898](https://github.com/matrix-org/matrix-rust-sdk/pull/5898))
- Improve logging around key history bundles when joining a room.
  ([#5866](https://github.com/matrix-org/matrix-rust-sdk/pull/5866))
- Expose the power level required to modify `m.space.child` on
  `room::power_levels::RoomPowerLevelChanges`.
  ([#5857](https://github.com/matrix-org/matrix-rust-sdk/pull/5857))
- Add the `Client::server_versions_cached()` method.
  ([#5853](https://github.com/matrix-org/matrix-rust-sdk/pull/5853))
- Extend `authentication::oauth::OAuth::grant_login_with_qr_code` to support granting
  login by scanning a QR code on the existing device.
  ([#5818](https://github.com/matrix-org/matrix-rust-sdk/pull/5818))
- Add a new `RequestConfig::skip_auth()` option. This is useful to ensure that
  certain request won't ever include an authorization header.
  ([#5822](https://github.com/matrix-org/matrix-rust-sdk/pull/5822))
- Add support for extended profile fields with `Account::fetch_profile_field_of()`,
  `Account::fetch_profile_field_of_static()`, `Account::set_profile_field()` and
  `Account::delete_profile_field()`.
  ([#5771](https://github.com/matrix-org/matrix-rust-sdk/pull/5771))
- [**breaking**] Remove the `matrix-sdk-crypto` re-export.
  ([#5769](https://github.com/matrix-org/matrix-rust-sdk/pull/5769))
- Allow `Client::get_dm_room()` to be called without the `e2e-encryption` crate feature.
  ([#5787](https://github.com/matrix-org/matrix-rust-sdk/pull/5787))
- [**breaking**] Add `encryption::secret_storage::SecretStorageError::ImportError` to indicate
  an error that occurred when importing a secret from secret storage.
  ([#5647](https://github.com/matrix-org/matrix-rust-sdk/pull/5647))
- [**breaking**] Add `authentication::oauth::qrcode::login::LoginProgress::SyncingSecrets` to
  indicate that secrets are being synced between the two devices.
  ([#5760](https://github.com/matrix-org/matrix-rust-sdk/pull/5760))
- Add `authentication::oauth::OAuth::grant_login_with_qr_code` to reciprocate a login by
  generating a QR code on the existing device.
  ([#5801](https://github.com/matrix-org/matrix-rust-sdk/pull/5801))
- [**breaking**] `OAuth::login_with_qr_code` now returns a builder that allows performing the flow with either the
  current device scanning or generating the QR code. Additionally, new errors `SecureChannelError::CannotReceiveCheckCode`
  and `QRCodeLoginError::ServerReset` were added.
  ([#5711](https://github.com/matrix-org/matrix-rust-sdk/pull/5711))
- [**breaking**] `ThreadedEventsLoader::new` now takes optional `tokens` parameter to customise where the pagination
  begins ([#5678](https://github.com/matrix-org/matrix-rust-sdk/pull/5678).
- Make `PaginationTokens` `pub`, as well as its `previous` and `next` tokens so they can be assigned from other files
  ([#5678](https://github.com/matrix-org/matrix-rust-sdk/pull/5678).
- Add new API to decline calls ([MSC4310](https://github.com/matrix-org/matrix-spec-proposals/pull/4310)): `Room::make_decline_call_event` and `Room::subscribe_to_call_decline_events`
  ([#5614](https://github.com/matrix-org/matrix-rust-sdk/pull/5614))
- Use `StateStore::upsert_thread_subscriptions()` to bulk process thread subscription updates received
  via the sync response or from the MSC4308 companion endpoint.
  ([#5848](https://github.com/matrix-org/matrix-rust-sdk/pull/5848))

### Refactor

- [**breaking**]: `Client::server_vendor_info()` requires to enable the
  `federation-api` feature.
  ([#5912](https://github.com/matrix-org/matrix-rust-sdk/pull/5912))
- [**breaking**]: `Client::reset_server_info()` has been split into
  `reset_supported_versions()` and `reset_well_known()`.
  ([#5910](https://github.com/matrix-org/matrix-rust-sdk/pull/5910))
- [**breaking**]: `Client::send()` has extra bounds where
  `Request::Authentication: AuthScheme<Input<'a> = SendAccessToken<'a>>` and
  `Request::PathBuilder: SupportedPathBuilder`. This method should still work for any request to the
  Client-Server API. This allows to drop the `HttpError::NotClientRequest` error in favor of a
  compile-time error.
  ([#5781](https://github.com/matrix-org/matrix-rust-sdk/pull/5781),
   [#5789](https://github.com/matrix-org/matrix-rust-sdk/pull/5789),
   [#5815](https://github.com/matrix-org/matrix-rust-sdk/pull/5815))
- [**breaking**]: The `waveform` field was moved from `AttachmentInfo::Voice` to `BaseAudioInfo`,
  allowing to set it for any audio message. Its format also changed, and it is now a list of `f32`
  between 0 and 1.
  ([#5732](https://github.com/matrix-org/matrix-rust-sdk/pull/5732))
- [**breaking**] The `caption` and `formatted_caption` fields and methods of `AttachmentConfig`,
  `GalleryConfig` and `GalleryItemInfo` have been merged into a single field that uses
  `TextMessageEventContent`.
  ([#5733](https://github.com/matrix-org/matrix-rust-sdk/pull/5733))
- The Matrix SDK crate now uses the 2024 edition of Rust.
  ([#5677](https://github.com/matrix-org/matrix-rust-sdk/pull/5677))
- [**breaking**] Make `LoginProgress::EstablishingSecureChannel` generic in order to reuse it
  for the currently missing QR login flow.
  ([#5750](https://github.com/matrix-org/matrix-rust-sdk/pull/5750))
- [**breaking**] The `new_virtual_element_call_widget` now uses a `props` and a `config` parameter instead of only `props`.
  This splits the configuration of the widget into required properties ("widget_id", "parent_url"...) so the widget can work
  and optional config parameters ("skip_lobby", "header", "...").
  The config option should in most cases only provide the `"intent"` property.
  All other config options will then be chosen by EC based on platform + `intent`.

  Before:

  ```rust
  new_virtual_element_call_widget(
    VirtualElementCallWidgetProperties {
      widget_id: "my_widget_id", // required property
      skip_lobby: Some(true), // optional configuration
      preload: Some(true), // optional configuration
      // ...
    }
  )
  ```

  Now:

  ```rust
  new_virtual_element_call_widget(
    VirtualElementCallWidgetProperties {
      widget_id: "my_widget_id", // required property
      // ... only required properties
    },
    VirtualElementCallWidgetConfig {
      intend: Intend.StartCallDM, // defines the default values for all other configuration
      skip_lobby: Some(false), // overwrite a specific default value
      ..VirtualElementCallWidgetConfig::default() // set all other config options to `None`. Use defaults from intent.
    }
  )
  ```
  ([#5560](https://github.com/matrix-org/matrix-rust-sdk/pull/5560))

### Bugfix

- A new local `LatestEventValue` was always created as `LocalIsSending`. It
  must be created as `LocalCannotBeSent` if a previous local `LatestEventValue`
  exists and is `LocalCannotBeSent`.
  ([#5908](https://github.com/matrix-org/matrix-rust-sdk/pull/5908))
- Switch QR login implementation from `std::time::Instant` to `ruma::time::Instant` which
  is compatible with Wasm.
  ([#5889](https://github.com/matrix-org/matrix-rust-sdk/pull/5889))

## [0.14.0] - 2025-09-04

### Features

- `Client::fetch_thread_subscriptions` implements support for the companion endpoint of the
  experimental MSC4308, allowing to fetch thread subscriptions for a given range, as specified by
  the MSC.
  ([#5590](https://github.com/matrix-org/matrix-rust-sdk/pull/5590))
- Add a `Client::joined_space_rooms` method that allows retrieving the list of joined spaces.
  ([#5592](https://github.com/matrix-org/matrix-rust-sdk/pull/5592))
- `Room::enable_encryption` and `Room::enable_encryption_with_state_event_encryption` will poll
  the encryption state for up to 3 seconds, rather than checking once after a single sync has
  completed.
  ([#5559](https://github.com/matrix-org/matrix-rust-sdk/pull/5559))
- Add `Room::enable_encryption_with_state` to enable E2E encryption with encrypted state event
  support, gated behind the `experimental-encrypted-state-events` feature.
  ([#5557](https://github.com/matrix-org/matrix-rust-sdk/pull/5557))
- Add `ignore_timeout_on_first_sync` to the `SyncSettings`, which should allow to have a quicker
  first response when using one of the `sync`, `sync_with_callback`, `sync_with_result_callback`
  or `sync_stream` methods on `Client`, if the response is empty.
  ([#5481](https://github.com/matrix-org/matrix-rust-sdk/pull/5481))
- The methods to use the `/v3/sync` endpoint set the `use_state_after` field,
  which means that, if the server supports it, the response will contain the
  state changes between the last sync and the end of the timeline.
  ([#5488](https://github.com/matrix-org/matrix-rust-sdk/pull/5488))
- Add experimental support for
  [MSC4306](https://github.com/matrix-org/matrix-spec-proposals/pull/4306), with the
  `Room::fetch_thread_subscription()`, `Room::subscribe_thread()` and `Room::unsubscribe_thread()`
  methods.
  ([#5439](https://github.com/matrix-org/matrix-rust-sdk/pull/5439))
- [**breaking**] `RoomMemberRole` has a new `Creator` variant, that
  differentiates room creators with infinite power levels, as introduced in room
  version 12.
  ([#5436](https://github.com/matrix-org/matrix-rust-sdk/pull/5436))
- Add `Account::fetch_account_data_static` to fetch account data from the server
  with a statically-known type, with a signature similar to
  `Account::account_data`.
  ([#5424](https://github.com/matrix-org/matrix-rust-sdk/pull/5424))
- Add support to accept historic room key bundles that arrive out of order, i.e.
  the bundle arrives after the invite has already been accepted.
  ([#5322](https://github.com/matrix-org/matrix-rust-sdk/pull/5322))
- [**breaking**] `OAuth::login` now allows requesting additional scopes for the authorization code grant.
  ([#5395](https://github.com/matrix-org/matrix-rust-sdk/pull/5395))

### Refactor

- [**breaking**] Upgrade ruma to 0.13.0
  ([#5623](https://github.com/matrix-org/matrix-rust-sdk/pull/5623))
- [**breaking**] `SyncSettings` token is now `SyncToken` enum type which has default behaviour of `SyncToken::ReusePrevious` token. This breaks `Client::sync_once`.
  For old behaviour, set the token to `SyncToken::NoToken` with the usual `SyncSettings::token` setter.
  ([#5522](https://github.com/matrix-org/matrix-rust-sdk/pull/5522))
- [**breaking**] Change the upload_encrypted_file and make it clone the client instead of owning it. The lifetime of the `UploadEncryptedFile` request returned by `Client::upload_encrypted_file()` only depends on the request lifetime now.
  ([#5470](https://github.com/matrix-org/matrix-rust-sdk/pull/5470))
- [**breaking**] Add an `IsPrefix = False` bound to the `account_data()` and
  `fetch_account_data_static()` methods of `Account`. These methods only worked
  for events where the full event type is statically-known, and this is now
  enforced at compile-time. `account_data_raw()` and `fetch_account_data()`
  respectively can be used instead for event types with a variable suffix.
  ([#5444](https://github.com/matrix-org/matrix-rust-sdk/pull/5444))
- [**breaking**] `RoomMemberRole::suggested_role_for_power_level()` and
  `RoomMemberRole::suggested_power_level()` now use `UserPowerLevel` to represent
  power levels instead of `i64` to differentiate the infinite power level of
  creators, as introduced in room version 12.
  ([#5436](https://github.com/matrix-org/matrix-rust-sdk/pull/5436))
- [**breaking**] The `reason` argument of `Room::report_room()` is now required,
  due to a clarification in the spec.
  ([#5337](https://github.com/matrix-org/matrix-rust-sdk/pull/5337))
- [**breaking**] The `join_rule` field of `RoomPreview` is now a
  `JoinRuleSummary`. It has the same variants as `SpaceRoomJoinRule` but
  contains as summary of the allow rules for the restricted variants.
  ([#5337](https://github.com/matrix-org/matrix-rust-sdk/pull/5337))
- [**breaking**] The MSRV has been bumped to Rust 1.88.
  ([#5431](https://github.com/matrix-org/matrix-rust-sdk/pull/5431))
- [**breaking**] `Room::send_call_notification` and `Room::send_call_notification_if_needed` have been removed, since the event type they send is outdated, and `Client` is not actually supposed to be able to join MatrixRTC sessions (yet). In practice, users of these methods probably already rely on another MatrixRTC implementation to participate in sessions, and such an implementation should be capable of sending notifications itself.
  ([#5452](https://github.com/matrix-org/matrix-rust-sdk/pull/5452))

### Bugfix

- The event handlers APIs now properly support events whose type is not fully
  statically-known. Before, those events would never trigger an event handler.
  ([#5444](https://github.com/matrix-org/matrix-rust-sdk/pull/5444))
- All HTTP requests now have a default `read_timeout` of 60s, which means they'll disconnect if the connection stalls.
 `RequestConfig::timeout` is now optional and can be disabled on a per-request basis. This will be done for
 the requests used to download media, so they don't get cancelled after the default 30s timeout for no good reason.
 ([#5437](https://github.com/matrix-org/matrix-rust-sdk/pull/5437))

## [0.13.0] - 2025-07-10

### Security Fixes

- Fix SQL injection vulnerability in `EventCache`
  ([d0c0100](https://github.com/matrix-org/matrix-rust-sdk/commit/d0c01006e4808db5eb96ad5c496416f284d8bd3c), Moderate, [CVE-2025-53549](https://www.cve.org/CVERecord?id=CVE-2025-53549), [GHSA-275g-g844-73jh](https://github.com/matrix-org/matrix-rust-sdk/security/advisories/GHSA-275g-g844-73jh))

### Bug fixes

- `Room.leave()` will now attempt to leave all reachable predecessors too.
  ([#5381](https://github.com/matrix-org/matrix-rust-sdk/pull/5381))
- When joining a room via `Client::join_room_by_id()`, if the client has `enable_share_history_on_invite` enabled,
  we will correctly check for received room key bundles. Previously this was only done when calling `Room::join`.
  ([#5043](https://github.com/matrix-org/matrix-rust-sdk/pull/5043))

### Features

- Add `Client::supported_versions()`, which returns the results of both `Client::server_versions()` and
  `Client::unstable_features()` with a single call.
  ([#5357](https://github.com/matrix-org/matrix-rust-sdk/pull/5357))
- `WidgetDriver::send_to_device` Now supports sending encrypted to-device messages.
  ([#5252](https://github.com/matrix-org/matrix-rust-sdk/pull/5252))
- `Client::add_event_handler`: Set `Option<EncryptionInfo>` in `EventHandlerData` for to-device messages.
  If the to-device message was encrypted, the `EncryptionInfo` will be set. If it is `None` the message was sent in clear.
  ([#5099](https://github.com/matrix-org/matrix-rust-sdk/pull/5099))
- `EventCache::subscribe_to_room_generic_updates` is added to subscribe to _all_
  room updates without having to subscribe to all rooms individually
  ([#5247](https://github.com/matrix-org/matrix-rust-sdk/pull/5247))
- [**breaking**] The element call widget URL configuration struct uses the new `header` url parameter
  instead of the now deprecated `hideHeader` parameter. This is only compatible with EC v0.13.0 or newer.
- [**breaking**] `RoomEventCacheGenericUpdate` gains a new `Clear` variant, and sees
  its `TimelineUpdated` variant being renamed to `UpdateTimeline`.
  ([#5363](https://github.com/matrix-org/matrix-rust-sdk/pull/5363/))
- [**breaking**]: The element call widget URL configuration struct uses the new `header` url parameter
  instead of the now deprecated `hideHeader` parameter. This is only compatible
  with EC v0.13.0 or newer.
- [**breaking**]: The experimental `Encryption::encrypt_and_send_raw_to_device`
  function now takes a `share_strategy` parameter, and will not send to devices
  that do not satisfy the given share strategy.
  ([#5457](https://github.com/matrix-org/matrix-rust-sdk/pull/5457/))

### Refactor

- [**breaking**]: `Client::unstable_features()` returns a `BTreeSet<FeatureFlag>`, containing only
  the features whose value was set to true in the response to the `/versions` endpoint.
  ([#5357](https://github.com/matrix-org/matrix-rust-sdk/pull/5357))
- [**breaking**]: The family of `Room::can_user_*` methods has been removed. The
  same functionality can be accessed using the `RoomPowerLevels::user_can_*`
  family of methods. The `RoomPowerLevels` object can be accessed using the
  `Room::power_levels()` method.
  ([#5250](https://github.com/matrix-org/matrix-rust-sdk/pull/5250/))
- `ClientServerCapabilities` has been renamed to `ClientServerInfo`. Alongside this,
  `Client::reset_server_info` is now `Client::reset_server_info` and `Client::fetch_server_capabilities`
  is now `Client::fetch_server_versions`, returning the server versions response directly.
  ([#5167](https://github.com/matrix-org/matrix-rust-sdk/pull/5167))
- `RoomEventCacheListener` is renamed `RoomEventCacheSubscriber`
  ([#5269](https://github.com/matrix-org/matrix-rust-sdk/pull/5269))
- `RoomPreview::join_rule` is now optional, and will be set to `None` if the join rule state event
  is missing for a given room.
  ([#5278](https://github.com/matrix-org/matrix-rust-sdk/pull/5278))

### Bug fixes

- `m.room.avatar` has been added as required state for sliding sync until [the existing backend issue](https://github.com/element-hq/synapse/issues/18598)
causing deleted room avatars to not be flagged is fixed. ([#5293](https://github.com/matrix-org/matrix-rust-sdk/pull/5293))

## [0.12.0] - 2025-06-10

### Features

- `Client::send_call_notification_if_needed` now returns `Result<bool>` instead of `Result<()>` so we can check if
  the event was sent.
  ([#5171](https://github.com/matrix-org/matrix-rust-sdk/pull/5171))
- Added `SendMediaUploadRequest` wrapper for `SendRequest`, which checks the size of the request to
  upload making sure it doesn't exceed the `m.upload.size` value that can be fetched through
  `Client::load_or_fetch_max_upload_size`.
  ([#5119](https://github.com/matrix-org/matrix-rust-sdk/pull/5119))
- Add `ClientBuilder::with_enable_share_history_on_invite` to enable experimental support for sharing encrypted room history on invite, per [MSC4268](https://github.com/matrix-org/matrix-spec-proposals/pull/4268).
  ([#5141](https://github.com/matrix-org/matrix-rust-sdk/pull/5141))
- `Room::list_threads()` is a new method to list all the threads in a room.
  ([#4973](https://github.com/matrix-org/matrix-rust-sdk/pull/4973))
- `Room::relations()` is a new method to list all the events related to another event
  ("relations"), with additional filters for relation type or relation type + event type.
  ([#4973](https://github.com/matrix-org/matrix-rust-sdk/pull/4973))
- The `EventCache`'s persistent storage has been enabled by default. This means that all the events
  received by sync or back-paginations will be stored, in memory or on disk, by default, as soon as
  `EventCache::subscribe()` has been called (which happens automatically if you're using the
  `matrix_sdk_ui::Timeline`). This offers offline access and super quick back-paginations (when the
  cache has been filled) whenever the event cache is enabled. It's also not possible to disable the
  persistent storage anymore. Note that by default, the event cache store uses an in-memory store,
  so the events will be lost when the process exits. To store the events on disk, you need to use
  the sqlite event cache store.
  ([#4308](https://github.com/matrix-org/matrix-rust-sdk/pull/4308))
- `Room::set_unread_flag()` now sets the stable `m.marked_unread` room account data, which was
  stabilized in Matrix 1.12. `Room::is_marked_unread()` also ignores the unstable
  `com.famedly.marked_unread` room account data if the stable variant is present.
  ([#5034](https://github.com/matrix-org/matrix-rust-sdk/pull/5034))
- `Encryption::encrypt_and_send_raw_to_device`: Introduced as an experimental method for
  sending custom encrypted to-device events. This feature is gated behind the
  `experimental-send-custom-to-device` flag, as it remains under active development and may undergo changes.
  ([4998](https://github.com/matrix-org/matrix-rust-sdk/pull/4998))
- `Room::send_single_receipt()` and `Room::send_multiple_receipts()` now also unset the unread
  flag of the room if an unthreaded read receipt is sent.
  ([#5055](https://github.com/matrix-org/matrix-rust-sdk/pull/5055))
- `Client::is_user_ignored(&UserId)` can be used to check if a user is currently ignored.
  ([#5081](https://github.com/matrix-org/matrix-rust-sdk/pull/5081))
- `RoomSendQueue::send_gallery` has been added to allow sending MSC4274-style media galleries
  via the send queue under the `unstable-msc4274` feature.
  ([#4977](https://github.com/matrix-org/matrix-rust-sdk/pull/4977))

### Bug fixes

- A invited DM room joined with `Client::join_room_by_id()` or `Client::join_room_by_id_or_alias()`
  will now be correctly marked as a DM.
  ([#5043](https://github.com/matrix-org/matrix-rust-sdk/pull/5043))
- API responses with an HTTP status code `520` won't be retried anymore, as this is used by some proxies
  (including Cloudflare) to warn that an unknown error has happened in the actual server.
  ([#5105](https://github.com/matrix-org/matrix-rust-sdk/pull/5105))

### Refactor

- Support for the deprecated `GET /auth_issuer` endpoint was removed in the `OAuth` API. Only the
  `GET /auth_metadata` endpoint is used now.
  ([#5302](https://github.com/matrix-org/matrix-rust-sdk/pull/5302))
- `Room::push_context()` has been renamed into `Room::push_condition_room_ctx()`. The newer
  `Room::push_context` now returns a `matrix_sdk::Room::PushContext`, which can be used to compute
  the push actions for any event.
  ([#4962](https://github.com/matrix-org/matrix-rust-sdk/pull/4962))
- `Room::decrypt_event()` now requires an extra `matrix_sdk::Room::PushContext` parameter to
  compute the push notifications for the decrypted event.
  ([#4962](https://github.com/matrix-org/matrix-rust-sdk/pull/4962))
- `SlidingSyncRoom` has been removed. With it, the `SlidingSync::get_room`,
  `get_all_rooms`, `get_rooms`, `get_number_of_rooms`, and
  `FrozenSlidingSync` methods and type have been removed.
  ([#5047](https://github.com/matrix-org/matrix-rust-sdk/pull/5047))
- `Room::set_unread_flag()` is now a no-op if the unread flag already has the wanted value.
  ([#5055](https://github.com/matrix-org/matrix-rust-sdk/pull/5055))

## [0.11.0] - 2025-04-11

### Features

- `Room::load_or_fetch_event()` is a new method that will find an event in the event cache (if
  enabled), or using network like `Room::event()` does.
  ([#4837](https://github.com/matrix-org/matrix-rust-sdk/pull/4837))
- [**breaking**]: The element call widget URL configuration struct
  (`VirtualElementCallWidgetOptions`) and URL generation have changed.
  - It supports the new fields: `hide_screensharing`, `posthog_api_host`, `posthog_api_key`,
  `rageshake_submit_url`, `sentry_dsn`, `sentry_environment`.
  - The widget URL will no longer automatically add `/room` to the base domain. For backward compatibility
  the app itself would need to add `/room` to the `element_call_url`.
  - And replaced:
    - `analytics_id` -> `posthog_user_id` (The widget URL query parameters will
      include `analytics_id` & `posthog_user_id` for backward compatibility)
    - `skip_lobby` -> `intent` (`Intent.StartCall`, `Intent.JoinExisting`.
      The widget URL query parameters will include `skip_lobby` if `intent` is
      `Intent.StartCall` for backward compatibility)
  - `VirtualElementCallWidgetOptions` now implements `Default`.
  ([#4822](https://github.com/matrix-org/matrix-rust-sdk/pull/4822))
- [**breaking**]: The `RoomPagination::run_backwards` method has been removed and replaced by two
simpler methods:
  - `RoomPagination::run_backwards_until()`, which will retrigger back-paginations until a certain
  number of events have been received (and retry if the timeline has been reset in the background).
  - `RoomPagination::run_backwards_once()`, which will run a single back-pagination (and retry if
  the timeline has been reset in the background).
  ([#4689](https://github.com/matrix-org/matrix-rust-sdk/pull/4689))
- [**breaking**]: The `OAuth::account_management_url` method now caches the
  result of a call, subsequent calls to the method will not contact the server
  for a while, instead the cached URI will be returned. If caching of this URI
  is not desirable, the `OAuth::fetch_account_management_url` method can be used.
  ([#4663](https://github.com/matrix-org/matrix-rust-sdk/pull/4663))
- The `MediaRetentionPolicy` can now trigger regular cleanups with its new
  `cleanup_frequency` setting.
  ([#4603](https://github.com/matrix-org/matrix-rust-sdk/pull/4603))
- [**breaking**] The HTTP client only allows TLS 1.2 or newer, as recommended by
  [BCP 195](https://datatracker.ietf.org/doc/bcp195/).
  ([#4647](https://github.com/matrix-org/matrix-rust-sdk/pull/4647))
- Add `Room::report_room` api. ([#4713](https://github.com/matrix-org/matrix-rust-sdk/pull/4713))
- `Client::notification_client` will create a copy of the existing `Client`,
  but now it'll make sure  it doesn't handle any verification events to
  avoid an issue with these events being received and  processed twice if
  `NotificationProcessSetup` was `SingleSetup`.
- [**breaking**] `Room::is_encrypted` is replaced by
  `Room::latest_encryption_state` which returns a value of the new
  `EncryptionState` enum; another `Room::encryption_state` non-async and
  infallible method is added to get the `EncryptionState` without calling
  `Room::request_encryption_state`. This latter method is also now public.
  ([#4777](https://github.com/matrix-org/matrix-rust-sdk/pull/4777)). One can
  safely replace:

  ```rust
  room.is_encrypted().await?
  ```

  by

  ```rust
  room.latest_encryption_state().await?.is_encrypted()
  ```
- `LocalServerBuilder`, behind the `local-server` feature, can be used to spawn
  a server when the end-user needs to be redirected to an address on localhost.
  It was used for `SsoLoginBuilder` and can now be used in other cases, like for
  login with the OAuth 2.0 API.
  ([#4804](https://github.com/matrix-org/matrix-rust-sdk/pull/4804)
- The `OAuth` api is no longer gated behind the `experimental-oidc` cargo
  feature.
  ([#4830](https://github.com/matrix-org/matrix-rust-sdk/pull/4830))
- Re-export `SqliteStoreConfig` and add
  `ClientBuilder::sqlite_store_with_config_and_cache_path` to configure the
  SQLite store with the new `SqliteStoreConfig` structure
  ([#4870](https://github.com/matrix-org/matrix-rust-sdk/pull/4870))
- Add `Client::logout()` that allows to log out regardless of the `AuthApi` that
  is used for the session.
  ([#4886](https://github.com/matrix-org/matrix-rust-sdk/pull/4886))

### Bug Fixes

- Ensure all known secrets are removed from secret storage when invoking the
  `Recovery::disable()` method. While the server is not guaranteed to delete
  these secrets, making an attempt to remove them is considered good practice.
  Note that all secrets are uploaded to the server in an encrypted form.
  ([#4629](https://github.com/matrix-org/matrix-rust-sdk/pull/4629))
- Most of the features in the `OAuth` API should now work under WASM
  ([#4830](https://github.com/matrix-org/matrix-rust-sdk/pull/4830))

### Refactor


- [**breaking**] Switched from the unmaintained backoff crate to the [backon](https://docs.rs/backon/1.5.0/backon/)
  crate. As part of this change, the `RequestConfig::retry_limit` method was
  renamed to `RequestConfig::max_retry_time` and the parameter for the method was
  updated from a `u64` to a `usize`.
  ([#4916](https://github.com/matrix-org/matrix-rust-sdk/pull/4916))
- [**breaking**] We now require Rust 1.85 as the minimum supported Rust version to compile.
  Yay for async closures!
  ([#4745](https://github.com/matrix-org/matrix-rust-sdk/pull/4745))
- [**breaking**] The `server_url` and `server_response` methods of
  `SsoLoginBuilder` are replaced by `server_builder()`, which allows more
  fine-grained settings for the server.
  ([#4804](https://github.com/matrix-org/matrix-rust-sdk/pull/4804)
- [**breaking**]: `OidcSessionTokens` and `MatrixSessionTokens` have been merged
  into `SessionTokens`. Methods to get and watch session tokens are now
  available directly on `Client`.
  `(MatrixAuth/Oidc)::session_tokens_stream()`, can be replaced by
  `Client::subscribe_to_session_changes()` and then calling
  `Client::session_tokens()` on a `SessionChange::TokenRefreshed`.
  ([#4772](https://github.com/matrix-org/matrix-rust-sdk/pull/4772))
- [**breaking**] `Oidc::url_for_oidc()` doesn't take the `VerifiedClientMetadata`
  to register as an argument, the one in `OidcRegistrations` is used instead.
  However it now takes the redirect URI to use, instead of always using the
  first one in the client metadata.
  ([#4771](https://github.com/matrix-org/matrix-rust-sdk/pull/4771))
- [**breaking**] The `server_url` and `server_response` methods of
  `SsoLoginBuilder` are replaced by `server_builder()`, which allows more
  fine-grained settings for the server.
- [**breaking**]: Rename the `Oidc` API to `OAuth`, since it's using almost
  exclusively OAuth 2.0 rather than OpenID Connect.
  ([#4805](https://github.com/matrix-org/matrix-rust-sdk/pull/4805))
  - The `oidc` module was renamed to `oauth`.
  - `Client::oidc()` was renamed to `Client::oauth()` and the `AuthApi::Oidc`
    variant was renamed to `AuthApi::OAuth`.
  - `OidcSession` was renamed to `OAuthSession` and the `AuthSession::Oidc`
    variant was renamed to `AuthSession::OAuth`.
  - `OidcAuthCodeUrlBuilder` and `OidcAuthorizationData` were renamed to
    `OAuthAuthCodeUrlBuilder` and `OAuthAuthorizationData`.
  - `OidcError` was renamed to `OAuthError` and the `RefreshTokenError::Oidc`
    variant was renamed to `RefreshTokenError::OAuth`.
  - `Oidc::provider_metadata()` was renamed to `OAuth::server_metadata()`.
- [**breaking**]: `OAuth::finish_login()` must always be called, instead of `OAuth::finish_authorization()`
  ([#4817](https://github.com/matrix-org/matrix-rust-sdk/pull/4817))
  - `OAuth::abort_authorization()` was renamed to `OAuth::abort_login()`.
  - `OAuth::finish_login()` can be called several times for the same session,
    but it will return an error if it is called with a new session.
  - `OAuthError::MissingDeviceId` was removed, it cannot occur anymore.
- [**breaking**] `OidcRegistrations` was renamed to `OAuthRegistrationStore`.
  ([#4814](https://github.com/matrix-org/matrix-rust-sdk/pull/4814))
  - `OidcRegistrationsError` was renamed to `OAuthRegistrationStoreError`.
  - The `registrations` module was renamed and is now private.
    `OAuthRegistrationStore` and `ClientId` are exported from `oauth`, and
    `OAuthRegistrationStoreError` is exported from `oauth::error`.
  - All the methods of `OAuthRegistrationStore` are now `async` and return a
    `Result`: errors when reading the file are no longer ignored, and blocking
    I/O is performed in a separate thread.
  - `OAuthRegistrationStore::new()` takes a `PathBuf` instead of a `Path`.
  - `OAuthRegistrationStore::new()` no longer takes a `static_registrations`
    parameter. It should be provided if needed with
    `OAuthRegistrationStore::with_static_registrations()`.
- [**breaking**] Allow to use any registration method with `OAuth::login()` and
  `OAuth::login_with_qr_code()`.
  ([#4827](https://github.com/matrix-org/matrix-rust-sdk/pull/4827))
  - `OAuth::login` takes an optional `ClientRegistrationData` to be able to
    register and login with a single function call.
  - `OAuth::url_for_oidc()` was removed, it can be replaced by a call to
    `OAuth::login()`.
  - `OAuth::login_with_qr_code()` takes an optional `ClientRegistrationData`
    instead of the client metadata.
  - `OAuth::finish_login` takes a `UrlOrQuery` instead of an
    `AuthorizationCode`. The deserialization of the query string will occur
    inside the method and eventual errors will be handled.
  - `OAuth::login_with_oidc_callback()` was removed, it can be replaced by a
    call to `OAuth::finish_login()`.
  - `AuthorizationResponse`, `AuthorizationCode` and `AuthorizationError` are
    now private.
- [**breaking**] - `OAuth::account_management_url()` and
  `OAuth::fetch_account_management_url()` don't take an action anymore but
  return an `AccountManagementUrlBuilder`. The final URL can be obtained with
  `AccountManagementUrlBuilder::build()`.
  ([#4831](https://github.com/matrix-org/matrix-rust-sdk/pull/4831))
- [**breaking**] `Client::store` is renamed `state_store`
  ([#4851](https://github.com/matrix-org/matrix-rust-sdk/pull/4851))
- [**breaking**] The parameters `event_id` and `enforce_thread` on [`Room::make_reply_event()`]
  have been wrapped in a `reply` struct parameter.
  ([#4880](https://github.com/matrix-org/matrix-rust-sdk/pull/4880/))
- [**breaking**]: The `Oidc` API was updated to match the latest version of the
  next-gen auth MSCs. The most notable change is that these MSCs are now based
  on OAuth 2.0 rather then OpenID Connect. To reflect that, most types have been
  renamed, with the `Oidc` prefix changed to `OAuth`. The API has also been
  cleaned up, it is now simpler and has fewer methods while keeping most of the
  available features. Here is a detailed list of changes:
  - Rename the `Oidc` API to `OAuth`, since it's using almost exclusively OAuth
    2.0 rather than OpenID Connect.
    ([#4805](https://github.com/matrix-org/matrix-rust-sdk/pull/4805))
    - The `oidc` module was renamed to `oauth`.
    - `Client::oidc()` was renamed to `Client::oauth()` and the `AuthApi::Oidc`
      variant was renamed to `AuthApi::OAuth`.
    - `OidcSession` was renamed to `OAuthSession` and the `AuthSession::Oidc`
      variant was renamed to `AuthSession::OAuth`.
    - `OidcAuthCodeUrlBuilder` and `OidcAuthorizationData` were renamed to
      `OAuthAuthCodeUrlBuilder` and `OAuthAuthorizationData`.
    - `OidcError` was renamed to `OAuthError` and the `RefreshTokenError::Oidc`
      variant was renamed to `RefreshTokenError::OAuth`.
    - `Oidc::provider_metadata()` was renamed to `OAuth::server_metadata()`.
  - The `authentication::qrcode` module was moved inside `authentication::oauth`,
    because it is only available through the `OAuth` API.
    ([#4687](https://github.com/matrix-org/matrix-rust-sdk/pull/4687/))
  - The `OAuth` API only supports public clients, i.e. clients
    without a secret.
    ([#4634](https://github.com/matrix-org/matrix-rust-sdk/pull/4634))
    - `OAuth::restore_registered_client()` takes a `ClientId` instead of
      `ClientCredentials`
    - `OAuth::restore_registered_client()` must NOT be called after
      `OAuth::register_client()` anymore.
  - `Oidc::authorize_scope()` was removed because it has no use
    case anymore, according to the latest version of
    [MSC2967](https://github.com/matrix-org/matrix-spec-proposals/pull/2967).
    ([#4664](https://github.com/matrix-org/matrix-rust-sdk/pull/4664))
  - The `OAuth` API uses the `GET /auth_metadata` endpoint from the
    latest version of [MSC2965](https://github.com/matrix-org/matrix-spec-proposals/pull/2965)
    by default. The previous `GET /auth_issuer` endpoint is still supported as a
    fallback for now.
    ([#4673](https://github.com/matrix-org/matrix-rust-sdk/pull/4673))
    - It is not possible to provide a custom issuer anymore:
      `Oidc::given_provider_metadata()` was removed, and the parameter was
      removed from `OAuth::register_client()`.
    - `Oidc::fetch_authentication_issuer()` was removed. To check if the
      homeserver supports OAuth 2.0, use `OAuth::server_metadata()`.
    - `OAuth::server_metadata()` returns an `OAuthDiscoveryError`. It has a
      `NotSupported` variant and an `is_not_supported()` method to check if the
      error is due to the server not supporting OAuth 2.0.
    - `OAuthError::MissingAuthenticationIssuer` was removed.
  - The behavior of `OAuth::logout()` is now aligned with
    [MSC4254](https://github.com/matrix-org/matrix-spec-proposals/pull/4254)
    ([#4674](https://github.com/matrix-org/matrix-rust-sdk/pull/4674))
    - Support for [RP-Initiated Logout](https://openid.net/specs/openid-connect-rpinitiated-1_0.html)
      was removed, so it doesn't return an `OidcEndSessionUrlBuilder` anymore.
    - Only one request is made to revoke the access token, since the server is
      supposed to revoke both the access token and the associated refresh token
      when the request is made.
  - Remove most of the parameter methods of `OAuthAuthCodeUrlBuilder`, since
    they were parameters defined in OpenID Connect. Only the `prompt` and
    `user_id_hint` parameters are still supported.
    ([#4699](https://github.com/matrix-org/matrix-rust-sdk/pull/4699))
  - Remove support for ID tokens in the `OAuth` API.
    ([#4726](https://github.com/matrix-org/matrix-rust-sdk/pull/4726))
    - `OAuth::restore_registered_client()` doesn't take a
      `VerifiedClientMetadata` anymore.
    - `Oidc::latest_id_token()` and `Oidc::client_metadata()` were removed.
  - The `OAuth` API makes use of the oauth2 and ruma crates rather than
    mas-oidc-client.
    ([#4761](https://github.com/matrix-org/matrix-rust-sdk/pull/4761))
    ([#4789](https://github.com/matrix-org/matrix-rust-sdk/pull/4789))
    - `ClientId` is a different type reexported from the oauth2 crate.
    - The error types that were in the `oauth` module have been moved to the
      `oauth::error` module.
    - The `device_id` parameter of `OAuth::login` is now an
      `Option<OwnedDeviceId>`.
    - The `state` field of `OAuthAuthorizationData` and the parameter of the
      same name in `OAuth::abort_login()` now use `CsrfToken`.
    - The `types` and `requests` modules are gone and the necessary types are
      exported from the `oauth` module or available from `ruma`.
    - `AccountManagementUrlFull` now takes an `OwnedDeviceId` when a device ID
      is required.
    - `(Verified)ProviderMetadata` was replaced by `AuthorizationServerMetadata`.
    - `OAuth::register_client()` doesn't accept a software statement anymore.
    - `(Verified)ClientMetadata` was replaced by `Raw<ClientMetadata>`.
      `ClientMetadata` is an opinionated type that only supports the fields
      required for the `OAuth` API, however any type can be used to construct
      the metadata by serializing it to JSON and converting it.
  - `OAuth::finish_login()` must always be called, instead of
    `OAuth::finish_authorization()`
    ([#4817](https://github.com/matrix-org/matrix-rust-sdk/pull/4817))
    - `OAuth::abort_authorization()` was renamed to `OAuth::abort_login()`.
    - `OAuth::finish_login()` can be called several times for the same session,
      but it will return an error if it is called with a new session.
    - `OAuthError::MissingDeviceId` was removed, it cannot occur anymore.
  - Allow to use any registration method with `OAuth::login()` and
    `OAuth::login_with_qr_code()`.
    ([#4827](https://github.com/matrix-org/matrix-rust-sdk/pull/4827))
    - `OAuth::login` takes an optional `ClientRegistrationData` to be able to
      register and login with a single function call.
    - `OAuth::url_for_oidc()` was removed, it can be replaced by a call to
      `OAuth::login()`.
    - `OAuth::login_with_qr_code()` takes an optional `ClientRegistrationData`
      instead of the client metadata.
    - `OAuth::finish_login` takes a `UrlOrQuery` instead of an
      `AuthorizationCode`. The deserialization of the query string will occur
      inside the method and eventual errors will be handled.
    - `OAuth::login_with_oidc_callback()` was removed, it can be replaced by a
      call to `OAuth::finish_login()`.
    - `AuthorizationResponse`, `AuthorizationCode` and `AuthorizationError` are
      now private.
  - `OAuth::account_management_url()` and
    `OAuth::fetch_account_management_url()` don't take an action anymore but
    return an `AccountManagementUrlBuilder`. The final URL can be obtained with
    `AccountManagementUrlBuilder::build()`.
    ([#4831](https://github.com/matrix-org/matrix-rust-sdk/pull/4831))
  - `OidcRegistrations` was removed. Clients are supposed to re-register with
    the homeserver for every login.
    ([#4879](https://github.com/matrix-org/matrix-rust-sdk/pull/4879))
  - `OAuth::restore_registered_client()` doesn't take an `issuer` anymore.
    ([#4879](https://github.com/matrix-org/matrix-rust-sdk/pull/4879))
    - `Oidc::issuer()` was removed.
    - The `issuer` field of `UserSession` was removed.
- `SendHandle::media_handles` was generalized into a vector
  ([#4898](https://github.com/matrix-org/matrix-rust-sdk/pull/4898))

## [0.10.0] - 2025-02-04

### Features

- Allow to set and check whether an image is animated via its `ImageInfo`.
  ([#4503](https://github.com/matrix-org/matrix-rust-sdk/pull/4503))

- Implement `Default` for `BaseImageInfo`, `BaseVideoInfo`, `BaseAudioInfo` and
  `BaseFileInfo`.
  ([#4503](https://github.com/matrix-org/matrix-rust-sdk/pull/4503))

- Expose `Client::server_versions()` publicly to allow users of the library to
  get the versions of Matrix supported by the homeserver.
  ([#4519](https://github.com/matrix-org/matrix-rust-sdk/pull/4519))

- Create `RoomPrivacySettings` helper to group room settings functionality
  related to room access and visibility.
  ([#4401](https://github.com/matrix-org/matrix-rust-sdk/pull/4401))

- Enable HTTP/2 support in the HTTP client.
  ([#4566](https://github.com/matrix-org/matrix-rust-sdk/pull/4566))

- Add support for creating custom conditional push rules in `NotificationSettings::create_custom_conditional_push_rule`.
  ([#4587](https://github.com/matrix-org/matrix-rust-sdk/pull/4587))

- The media contents stored in the media cache can now be controlled with a
  `MediaRetentionPolicy` and the new `Media` methods `media_retention_policy()`,
  `set_media_retention_policy()`, `clean_up_media_cache()`.
  ([#4571](https://github.com/matrix-org/matrix-rust-sdk/pull/4571))

- Add support for creating custom conditional push rules in `NotificationSettings::create_custom_conditional_push_rule`.
  ([#4587](https://github.com/matrix-org/matrix-rust-sdk/pull/4587))

### Refactor

- [**breaking**]: The `RoomEventCacheUpdate::Clear` variant has been removed, as
  it is redundant with the `RoomEventCacheUpdate::UpdateTimelineEvents { diffs:
  Vec<VectorDiff<_>>, .. }` where `VectorDiff` has its own `Clear` variant.
  ([#4627](https://github.com/matrix-org/matrix-rust-sdk/pull/4627))

- Improve the performance of `EventCache` (approximately 4.5 times faster).
  ([#4616](https://github.com/matrix-org/matrix-rust-sdk/pull/4616))

- [**breaking**]: The reexported types `SyncTimelineEvent` and `TimelineEvent` have been fused into a single type
  `TimelineEvent`, and its field `push_actions` has been made `Option`al (it is set to `None` when
  we couldn't compute the push actions, because we lacked some information).
  ([#4568](https://github.com/matrix-org/matrix-rust-sdk/pull/4568))

- [**breaking**] Move the optional `RequestConfig` argument of the
  `Client::send()` method to the `with_request_config()` builder method. You
  should call `Client::send(request).with_request_config(request_config).await`
  now instead.
  ([#4443](https://github.com/matrix-org/matrix-rust-sdk/pull/4443))

- [**breaking**] Remove the `AttachmentConfig::with_thumbnail()` constructor and
  replace it with the `AttachmentConfig::thumbnail()` builder method. You should
  call `AttachmentConfig::new().thumbnail(thumbnail)` now instead.
  ([#4452](https://github.com/matrix-org/matrix-rust-sdk/pull/4452))

- [**breaking**] `Room::send_attachment()` and `RoomSendQueue::send_attachment()`
  now take any type that implements `Into<String>` for the filename.
  ([#4451](https://github.com/matrix-org/matrix-rust-sdk/pull/4451))

- [**breaking**] `Recovery::are_we_the_last_man_standing()` has been renamed to `is_last_device()`.
  ([#4522](https://github.com/matrix-org/matrix-rust-sdk/pull/4522))

- [**breaking**] The `matrix_auth` module is now at `authentication::matrix`.
  ([#4575](https://github.com/matrix-org/matrix-rust-sdk/pull/4575))

- [**breaking**] The `oidc` module is now at `authentication::oidc`.
  ([#4575](https://github.com/matrix-org/matrix-rust-sdk/pull/4575))

## [0.9.0] - 2024-12-18

### Bug Fixes

- Use the inviter's server name and the server name from the room alias as
  fallback values for the via parameter when requesting the room summary from
  the homeserver. This ensures requests succeed even when the room being
  previewed is hosted on a federated server.
  ([#4357](https://github.com/matrix-org/matrix-rust-sdk/pull/4357))

- Do not use the encrypted original file's content type as the encrypted
  thumbnail's content type.
  ([#ecf4434](https://github.com/matrix-org/matrix-rust-sdk/commit/ecf44348cf6a872b843fb7d7af1a88f724c58c3e))

### Features

- Enable persistent storage for the `EventCache`. This allows events received
  through the `/sync` endpoint or backpagination to be stored persistently,
  enabling client applications to restore a room's view, including events,
  without requiring server communication.
  ([#4347](https://github.com/matrix-org/matrix-rust-sdk/pull/4347))

- [**breaking**] Make all fields of Thumbnail required
  ([#4324](https://github.com/matrix-org/matrix-rust-sdk/pull/4324))

- `Backups::exists_on_server`, which always fetches up-to-date information from the
  server about whether a key storage backup exists, was renamed to
  `fetch_exists_on_the_server`, and a new implementation of `exists_on_server`
  which caches the most recent answer is now provided.

## [0.8.0] - 2024-11-19

### Bug Fixes

- Add more invalid characters for room aliases.

- Match the right status code in `Client::is_room_alias_available`.

- Fix a bug where room keys were considered to be downloaded before backups were
  enabled. This bug only affects the
  `BackupDownloadStrategy::AfterDecryptionFailure`, where no attempt would be
  made to download a room key, if a decryption failure with a given room key
  would have been encountered before the backups were enabled.

### Documentation

- Improve documentation of `Client::observe_events`.


### Features


- Add `create_room_alias` function.

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

- `EncryptionSyncService` and `Notification` are using `Client::cross_process_store_locks_holder_name`.

- Allow passing a custom `RequestConfig` to an upload request.

- Retry uploads if they've failed with transient errors.

- Implement `EventHandlerContext` for tuples.

- Introduce a mechanism similar to `Client::add_event_handler` and
  `Client::add_room_event_handler` but with a reactive programming pattern. Add
  `Client::observe_events` and `Client::observe_room_events`.

 ```rust
 // Get an observer.
 let observer =
     client.observe_events::<SyncRoomMessageEvent, (Room, Vec<Action>)>();

 // Subscribe to the observer.
 let mut subscriber = observer.subscribe();

 // Use the subscriber as a `Stream`.
 let (message_event, (room, push_actions)) = subscriber.next().await.unwrap();
 ```

 When calling `observe_events`, one has to specify the type of event (in the
 example, `SyncRoomMessageEvent`) and a context (in the example, `(Room,
 Vec<Action>)`, respectively for the room and the push actions).

- Implement unwedging for media uploads.

- Send state from state sync and not from timeline to widget ([#4254](https://github.com/matrix-org/matrix-rust-sdk/pull/4254))

- Allow aborting media uploads.

- Add `RoomPreviewInfo::num_active_members`.

- Use room directory search as another data source.

- Check if the user is allowed to do a room mention before trying to send a call
  notify event.
  ([#4271](https://github.com/matrix-org/matrix-rust-sdk/pull/4271))

- Add `Client::cross_process_store_locks_holder_name()`.

- Add a `PreviouslyVerified` variant to `VerificationLevel` indicating that the
  identity is unverified and previously it was verified.

- New `UserIdentity::pin` method.

- New `ClientBuilder::with_decryption_trust_requirement` method.

- New `ClientBuilder::with_room_key_recipient_strategy` method

- New `Room.set_account_data` and `Room.set_account_data_raw` RoomAccountData
  setters, analogous to the GlobalAccountData

- New `RequestConfig.max_concurrent_requests` which allows to limit the maximum
  number of concurrent requests the internal HTTP client issues (all others have
  to wait until the number drops below that threshold again)

- Implement proper redact handling in the widget driver. This allows the Rust
  SDK widget driver to support widgets that rely on redacting.


### Refactor
- [**breaking**] Rename `DisplayName` to `RoomDisplayName`.

- Improve `is_room_alias_format_valid` so it's more strict.

- Remove duplicated fields in media event contents.

- Use `SendHandle` for media uploads too.

- Move `event_cache_store/` to `event_cache/store/` in `matrix-sdk-base`.

- Move `linked_chunk` from `matrix-sdk` to `matrix-sdk-common`.

- Move `Event` and `Gap` into `matrix_sdk_base::event_cache`.

- Move `formatted_caption_from` to the SDK, rename it.

- Tidy up and start commenting the widget code.

- Get rid of `ProcessingContext` and inline it in its callers.

- Get rid of unused `limits` parameter when constructing a `WidgetMachine`.

- Use a specialized mutex for locking access to the state store and
  `being_sent`.

- Renamed `VerificationLevel::PreviouslyVerified` to
  `VerificationLevel::VerificationViolation`.

- [**breaking**] Replace the `Notification` type from Ruma in `SyncResponse` and
  `Client::register_notification_handler` by a custom one.

- [**breaking**] The ambiguity maps in `SyncResponse` are moved to `JoinedRoom`
  and `LeftRoom`.

- [**breaking**] `Room::can_user_redact` and `Member::can_redact` are split
  between `*_redact_own` and `*_redact_other`.

- [**breaking**] `AmbiguityCache` contains the room member's user ID.

- [**breaking**] Replace `impl MediaEventContent` with `&impl MediaEventContent` in
  `Media::get_file`/`Media::remove_file`/`Media::get_thumbnail`/`Media::remove_thumbnail`

- [**breaking**] A custom sliding sync proxy set with
  `ClientBuilder::sliding_sync_proxy` now takes precedence over a discovered
  proxy.

- [**breaking**] `Client::get_profile` was moved to `Account` and renamed to
  `Account::fetch_user_profile_of`. `Account::get_profile` was renamed to
  `Account::fetch_user_profile`.

- [**breaking**] The `HttpError::UnableToCloneRequest` error variant has been
  removed because it was never used or generated by the SDK.

- [**breaking**] The `Error::InconsistentState` error variant has been removed
  because it was never used or generated by the SDK.

- [**breaking**] The widget capabilities in the FFI now need two additional
  flags: `update_delayed_event`, `send_delayed_event`.

- [**breaking**] `Room::event` now takes an optional `RequestConfig` to allow
  for tweaking the network behavior.

- [**breaking**] The `instant` module was removed, use the `ruma::time` module
  instead.

- [**breaking**] Add `ClientBuilder::sqlite_store_with_cache_path` to build a
  client that stores caches in a different directory to state/crypto.

- [**breaking**] The `body` parameter in `get_media_file` has been replaced with
  a `filename` parameter now that Ruma has a `filename()` method.

# 0.7.0

Breaking changes:

- The `Client::sync_token` accessor function is no longer public. If you were
  using this for `Client::sync_once()`, you can get the token from the result of
  the `Client::sync_once()` method instead ([#1216](https://github.com/matrix-org/matrix-rust-sdk/pull/1216)).
- `Common::members` and `Common::members_no_sync` take a `RoomMemberships` to be able to filter the
  results by any membership state.
  - `Common::active_members(_no_sync)` and `Common::joined_members(_no_sync)` are deprecated.
- `matrix-sdk-sqlite` is the new default store implementation outside of WASM, behind the `sqlite` feature.
  - The `sled` feature was removed. The `matrix-sdk-sled` crate is deprecated and no longer maintained.
- Replace `Client::authentication_issuer` with `Client::authentication_server_info` that contains
  all the fields discovered from the homeserver for authenticating with OIDC
- Remove `HttpSend` trait in favor of allowing a custom `reqwest::Client` instance to be supplied
- Move all the types and methods using the native Matrix login and registration APIs from `Client`
  to the new `matrix_auth::MatrixAuth` API that is accessible via `Client::matrix_auth()`.
- Move `Session` and `SessionTokens` to the `matrix_auth` module.
  - Move the session methods on `Client` to the `MatrixAuth` API.
  - Split `Session`'s content into several types. Its (de)serialization is still backwards
    compatible.
- The room API has been simplified
  - Removed the previous `Room`, `Joined`, `Invited` and `Left` types
  - Merged all of the functionality from `Joined`, `Invited` and `Left` into `room::Common`
  - Renamed `room::Common` to just `Room` and made it accessible as `matrix_sdk::Room`
- Event handler closures now need to implement `FnOnce` + `Clone` instead of `Fn`
  - As a consequence, you no longer need to explicitly need to `clone` variables they capture
    before constructing an `async move {}` block inside
- `Room::sync_members` doesn't return the underlying Ruma response anymore. If you need to get the
  room members, you can use `Room::members` or `Room::get_member` which will make sure that the
  members are up to date.
- The `transaction_id` parameter of `Room::{send, send_raw}` was removed
  - Instead, both methods now return types that implement `IntoFuture` (so can be awaited like
    before) and have a `with_transaction_id` builder-style method
- The parameter order of `Room::{send_raw, send_state_event_raw}` has changed, `content` is now last
  - The parameter type of `content` has also changed to a generic; `serde_json::Value` arguments
    are still allowed, but so are other types like `Box<serde_json::value::RawValue>`
- All "named futures" (structs implementing `IntoFuture`) are now exported from modules named
  `futures` instead of directly in the respective parent module
- `Verification` is non-exhaustive, to make the `qrcode` cargo feature additive

Bug fixes:

- `Client::rooms` now returns all rooms, even invited, as advertised.

Additions:

- Add secret storage support, the secret store can be opened using the
  `Client::encryption()::open_secret_store()` method, which allows you to import
  or export secrets from the account-data backed secret-store.

- Add `VerificationRequest::state` and `VerificationRequest::changes` to check
  and listen to changes in the state of the `VerificationRequest`. This removes
  the need to listen to individual matrix events once the `VerificationRequest`
  object has been acquired.
- The `Room` methods to retrieve state events can now return a sync or stripped event,
  so they can be used for invited rooms too.
- Add `Client::subscribe_to_room_updates` and `room::Common::subscribe_to_updates`
- Add `Client::rooms_filtered`
- Add methods on `Client` that can handle several authentication APIs.
- Add new method `force_discard_session` on `Room` that allows to discard the current
  outbound session (room key) for that room. Can be used by clients for the `/discardsession` command.

# 0.6.2

- Fix the access token being printed in tracing span fields.

# 0.6.1

- Fixes a bug where the access token used for Matrix requests was added as a field to a tracing span.
