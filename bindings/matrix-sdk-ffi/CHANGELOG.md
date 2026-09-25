# Changelog

All notable changes to this project will be documented in this file.

<!-- changelog start -->

## [0.19.1](https://github.com/matrix-org/matrix-rust-sdk/tree/0.19.1) - 2026-09-18

No significant changes.

## [0.19.0](https://github.com/matrix-org/matrix-rust-sdk/tree/0.19.0) - 2026-09-16

### Removed

- Removed a special case for Android bindings which used `WebPkiServerVerifier`
  for compatibility.

  Note this means on Android, you:

  1. Won't need to use `ClientBuilder.addRootCertificates` since user
     certificates will be checked by default.
  2. Will need to add some CLRs to `network_security_config`, like:

  ```xml
  <domain-config cleartextTrafficPermitted="true">
      <!-- Let's Encrypt disabled OCSP for certificate revocation and switched to CRL, which needs clear-text access. -->
      <domain includeSubdomains="true">lencr.org</domain>
  </domain-config>
  ``` ([#6645](https://github.com/matrix-org/matrix-rust-sdk/pull/6645))
- [**breaking**] Removed the `TimelineEventFilter` wrapper, as well as
  `FilterTimelineEventType` and `FilterTimelineEventCondition`. Equivalent types
  are now exposed in the FFI layer from the `matrix_sdk_ui` crate:

  - `TimelineEventFilter` with `Include` and `Exclude` variants instead of
    `include` and `exclude` methods.
  - `TimelineEventType` instead of `FilterTimelineEventType`. This previously
    took either a `MessageLike` or a `State` variant with `MessageLikeEventType`
    and `StateEventType` respectively, now you just need to use
    `TimelineEventType`.
  - `TimelineEventCondition` instead of `FilterTimelineEventCondition`.

  ([#6985](https://github.com/matrix-org/matrix-rust-sdk/pull/6985))

### Added

- Expose the [MSC3814] dehydrated-device manager on `Encryption`:
  `is_dehydrated_device_supported`, `create_dehydrated_device`,
  `rehydrate_dehydrated_device`, `delete_dehydrated_device`,
  `start_dehydrated_devices`, `stop_dehydrated_devices`, and a
  `dehydrated_device_event_listener` callback for lifecycle observability.

  [MSC3814]: [https://github.com/matrix-org/matrix-spec-proposals/pull/3814][https-github-com-matrix-org-matrix-spec-proposals-pull-3814] ([#6606](https://github.com/matrix-org/matrix-rust-sdk/pull/6606))
- Expose [MSC4426] user-status profile fields through the FFI.
  ([#6616](https://github.com/matrix-org/matrix-rust-sdk/pull/6616))
- Added a new `edit_revisions` method on `Timeline` that returns the edit
  history of an event.
  ([#6630](https://github.com/matrix-org/matrix-rust-sdk/pull/6630))
- Expose client-level presence configuration with optional immediate updates.
  ([#6672](https://github.com/matrix-org/matrix-rust-sdk/pull/6672))
- Add `status` and `call` fields to the FFI `RoomMember` and `ProfileDetails`,
  exposing the user's
  [MSC4426](https://github.com/matrix-org/matrix-spec-proposals/pull/4426) user
  status and call indicator from their global profile.
  ([#6704](https://github.com/matrix-org/matrix-rust-sdk/pull/6704))
- Adds the `RawX509Signer` and `RawX509Verifier` foreign traits and the
  `ClientBuilder::with_raw_x509_signer` / `with_raw_x509_verifier` methods,
  allowing consumers to implement platform-specific signing and verification
  implementations.

  Gated behind the `experimental-x509-identity-verification` feature.
  ([#6727](https://github.com/matrix-org/matrix-rust-sdk/pull/6727))
- Add `thresholds()` accessor and `normal_score` field to
  `PasswordStrengthEstimator` and `PasswordStrengthEstimate`.
  ([#6728](https://github.com/matrix-org/matrix-rust-sdk/pull/6728))
- Add `Client::subscribe_to_own_profile`, which reports the current user's
  profile to a `ProfileListener` and notifies it of any subsequent changes.
  **Note:** Without the Profiles sliding sync extension enabled only an empty
  profile will be emitted and no updates will be published.
  ([#6760](https://github.com/matrix-org/matrix-rust-sdk/pull/6760))
- Add `Client::is_user_status_supported()`, which returns whether the homeserver
  supports user status. This is the case when the server supports MSC4262
  (Profiles Sliding Sync Extension) and allows setting the `m.status` extended
  profile field.
  ([#6778](https://github.com/matrix-org/matrix-rust-sdk/pull/6778))
- `Room` gained a `load_user_receipt()` method, exposing the existing SDK API of
  the same name: it returns a user's receipt of the given type in the room,
  optionally scoped to a thread (`ReceiptThread`), read from the local store.
  This allows e.g. computing per-thread read state from threaded receipts
  without instantiating a thread-focused timeline per thread.
  ([#6787](https://github.com/matrix-org/matrix-rust-sdk/pull/6787))
- `Room` gained a `send_single_receipt()` method, exposing the existing SDK API
  of the same name: it sends a receipt of the given type for the given event id,
  optionally scoped to a thread (`ReceiptThread`). This allows e.g. sending
  threaded read receipts without instantiating a thread-focused timeline per
  thread. ([#6810](https://github.com/matrix-org/matrix-rust-sdk/pull/6810))
- `UploadParameters` gained an optional `extra_content_json` field, and
  `Timeline` a `send_with_extra_content()` method (leaving `send()` unchanged),
  allowing additional top-level fields (a serialized JSON object) to be included
  in an event's content, e.g. vendor-prefixed keys matched by server-side push
  rules. Fields of the event itself take precedence.
  ([#6812](https://github.com/matrix-org/matrix-rust-sdk/pull/6812))
- Added `Client::enable_automatic_call_status(bool)` on the FFI. Opts in to
  auto-syncing this device's MatrixRTC participation into the [MSC4426] `m.call`
  profile field.
  ([#6825](https://github.com/matrix-org/matrix-rust-sdk/pull/6825))
- Add `Client::disable_well_known_lookup`, which disables every
  `/.well-known/matrix/client` request performed by the client, for users that
  must not emit any request to the well-known URI of their domain. When
  disabled, `Client::tile_server` returns `None`,
  `Client::well_known_rtc_transports` returns an empty list, and
  `Client::discover_rtc_transports` doesn't fall back to the well-known foci.

  Add `ClientBuilder::disable_well_known_lookup`, which controls the initial
  value of the `Client::disable_well_known_lookup` flag and additionally
  disables any homeserver discovery performed by `ClientBuilder::build`. If
  `true`, the homeserver must then be given with
  `ClientBuilder::homeserver_url`. `ClientBuilder::server_name` and
  `ClientBuilder::username` can only be resolved through the well-known, so
  `ClientBuilder::build` fails with the new
  `ClientBuildError::WellKnownLookupDisabled` variant if they are called.
  ([#6845](https://github.com/matrix-org/matrix-rust-sdk/pull/6845))
- `Timeline` gained a `toggle_reaction_with_extra_content()` method (leaving
  `toggle_reaction()` unchanged), allowing additional top-level fields (a
  serialized JSON object) to be included in a reaction's content, e.g.
  vendor-prefixed keys matched by server-side push rules. Fields of the event
  itself take precedence, and the extra fields only apply when a reaction is
  added, since removing one is a redaction.
  ([#6848](https://github.com/matrix-org/matrix-rust-sdk/pull/6848))
- Added `Client::is_profiles_sliding_sync_extension_supported()` on the FFI,
  exposing a standalone check for server support of the Profiles sliding sync
  extension ([MSC4262]), separately from `is_user_status_supported()`.
  ([#6863](https://github.com/matrix-org/matrix-rust-sdk/pull/6863))
- Add `Room::load_or_fetch_event_with_relations`, exposing the existing
  Rust-side API over FFI. Returns the event together with its related events
  (optionally filtered by `RelationType`).
  ([#6875](https://github.com/matrix-org/matrix-rust-sdk/pull/6875))
- Added a `server_name_from_user_id` function that returns the server name of a
  user ID, including the port when there is one. Clients that let people type a
  user ID where a server name is expected, such as when picking an account
  provider, no longer need to parse the ID themselves.
  ([#6922](https://github.com/matrix-org/matrix-rust-sdk/pull/6922))
- Add `Room::active_human_member_ids` and
  `Room::active_human_member_ids_no_sync`, which return the user IDs of the
  joined and invited room members, without the service members declared by the
  `io.element.functional_members` state event. This is a convenient way to find
  the other party of a direct message.
  ([#6930](https://github.com/matrix-org/matrix-rust-sdk/pull/6930))
- Expose `RoomListService::remove_room_subscriptions` and
  `RoomListService::reset_and_add_room_subscriptions`, so consumers can release
  room subscriptions they no longer need, or replace the whole subscription set.
  ([#6932](https://github.com/matrix-org/matrix-rust-sdk/pull/6932))
- Expose `Client::get_url_preview(url, ts)`, returning the homeserver-generated
  URL preview as OpenGraph JSON. The response is handed back as a raw JSON
  string because its field set is open-ended.
  ([#6949](https://github.com/matrix-org/matrix-rust-sdk/pull/6949))
- `SpaceService` has two new cheap accessor functions for asking about the
  ancestors of a given room or space:
  - `joined_parent_ids_of_child()`: returns the room IDs of a room or space's
  direct parent spaces that are known, without recomputing the space graph or
  building any expensive `SpaceRoom` instances.
  - `top_level_ancestors_of()`: returns the IDs of the top-level joined space(s)
  a room descends from.

  ([#6967](https://github.com/matrix-org/matrix-rust-sdk/pull/6967))
- Expose `Client::total_unread_notifications`, the sum of the client-side
  computed unread notification counts across all joined rooms, counting rooms
  marked as unread by hand as one each.
  ([#7002](https://github.com/matrix-org/matrix-rust-sdk/pull/7002))
- Add `Client::notification_client_with_timeouts`, which takes a
  `NotificationClientTimeouts` record, so that clients with a larger time budget
  can raise the timeouts applied while fetching notifications.
  `Client::notification_client` is unchanged and keeps using the defaults.
  ([#7023](https://github.com/matrix-org/matrix-rust-sdk/pull/7023))
- Add `Client::sendEncryptedToDeviceMessage` and `SendToDeviceOutcome` to
  Olm-encrypt a custom to-device message and send it to a set of recipient
  devices, reporting the devices that could not be reached.
  ([#6981](https://github.com/matrix-org/matrix-rust-sdk/pulls/6981))

### Changed

- [**breaking**] Enable `unstable-uniffi` feature in ruma, rename
  `TimelineEventType` to `FfiTimelineEventType` and replace `StateEventType`,
  `MessageLikeEventType` and `RoomAccountDataEventType` with the ruma types.
  ([#6161](https://github.com/matrix-org/matrix-rust-sdk/pull/6161))
- [**breaking**] Send redactions issued via `Timeline::redact_event` through the
  send queue.
  ([#6428](https://github.com/matrix-org/matrix-rust-sdk/pull/6428))
- [**breaking**] `Room::search_messages` and `Client::search_messages` no longer
  take a `num_results_per_batch` parameter. The returned
  `RoomSearchIterator`/`GlobalSearchIterator`'s `next_events()` now yields one
  page of results per call.
  ([#6645](https://github.com/matrix-org/matrix-rust-sdk/pull/6645))
- [**Breaking**]: instead of setting up a `ContentScanner` in `ClientBuilder`,
  using `ClientBuilder::set_content_scanner`, it can now be enabled and disabled
  at any time using `Client::set_content_scanner` and you can check if content
  scanning is enabled using `Client::content_scanner`.
  ([#6689](https://github.com/matrix-org/matrix-rust-sdk/pull/6689))
- [**breaking**] The message search FFI is now reactive.
  `Client::search_messages` (and its `GlobalSearchIterator`) and
  `Room::search_messages` (and its `RoomSearchIterator`) are removed, replaced
  by `Client::search_service(query, filter)` which returns a `SearchService`
  object. Call `SearchService::subscribe_to_results` with a
  `SearchServiceResultsListener` to receive `SearchServiceResultsUpdate`s
  (`VectorDiff`-style) over a single list of typed `SearchResult`s.
  ([#6695](https://github.com/matrix-org/matrix-rust-sdk/pull/6695))
- [**breaking**] `GrantQrLoginProgress::WaitingForAuth` and
  `GrantGeneratedQrLoginProgress::WaitingForAuth` now have a
  `continuation_sender: ContinuationMessageSender` field. Applications must call
  `continuation_sender.confirm()` once the verification URI has been opened in
  the browser and the application is ready to proceed, or
  `continuation_sender.cancel()` to abort; previously it proceeded automatically
  as soon as this state was reached. This lets applications that suspend or
  navigate away while the verification URI is open to resume the process
  explicitly.

  `HumanQrLoginError` has two new variants, `ContinuationAlreadySent` and
  `ContinuationCannotBeSent`, which `ContinuationMessageSender.confirm()` and
  `.cancel()` return instead of the check-code-specific `CheckCodeAlreadySent` /
  `CheckCodeCannotBeSent` variants.
  ([#6711](https://github.com/matrix-org/matrix-rust-sdk/pull/6711))
- [**breaking**] Adjust `OlmMachine::bootstrap_cross_signing` to return a new
  `BootstrapCrossSigningError` enum covering both crypto store and signing
  failures. This replaces the previous `CryptoStoreError` return type.
  ([#6715](https://github.com/matrix-org/matrix-rust-sdk/pull/6715))
- [**breaking**] The FFI `Room::heroes()` is now `async`, and the returned
  `RoomHero` now exposes the user's
  [MSC4426](https://github.com/matrix-org/matrix-spec-proposals/pull/4426)
  status and call fields (`status` and `call`), taken from their global profile.
  These fields are only populated when syncing via sliding sync with the
  profiles extension enabled.
  ([#6733](https://github.com/matrix-org/matrix-rust-sdk/pull/6733))
- [**breaking**] `ClientBuildError::WellKnownLookupFailed` inner type is now
  `Box`ed to reduce the error enum's size.
  ([#6763](https://github.com/matrix-org/matrix-rust-sdk/pull/6763))
- The dedicated `unstable-msc4426` Cargo feature has been removed. This feature
  was previously enabled by default and there is no change in functionality.
  ([#6778](https://github.com/matrix-org/matrix-rust-sdk/pull/6778))
- `Client::is_livekit_rtc_supported` that checks if the server supports
  livekit-based RTC calls is now **by default** only checking the new rtc
  discovery endpoint (MSC4143) and not falling back to the old well-known
  discovery method. If needed, for backwards compatibility, use the
  `fallback_to_well_known` parameter to also check the old method.
  ([#6791](https://github.com/matrix-org/matrix-rust-sdk/pull/6791))
- Added `matrix_sdk_search` log target and trace log pack to the available ones.
  ([#6823](https://github.com/matrix-org/matrix-rust-sdk/pull/6823))
- [**breaking**] `Client::enable_automatic_backpagination` has been removed.
  Automatic back-pagination is now enabled via
  `ClientBuilder::enable_automatic_back_pagination(bool)`, set before the client
  is built. ([#6838](https://github.com/matrix-org/matrix-rust-sdk/pull/6838))
- `Client::is_livekit_rtc_supported` does not accept a `fallback_to_well_known`
  parameter anymore. Its fallback behavior is now controlled by the new
  `Client::disable_well_known_lookup` flag instead.
  ([#6845](https://github.com/matrix-org/matrix-rust-sdk/pull/6845))
- [**breaking**] `Timeline::send_reply` now returns the `SendHandle` of the
  queued reply, instead of nothing. This matches `Timeline::send` and lets
  consumers abort or retry a reply that has not been sent yet.
  ([#6881](https://github.com/matrix-org/matrix-rust-sdk/pull/6881))
- `Timeline::send_location` is now a thin wrapper around the new
  `matrix_sdk_ui::Timeline::send_location`. Its signature is unchanged, but
  invalid input now returns an error instead of the old silent behavior: an
  out-of-range `zoom_level` (above 20) and `AssetType::Unknown` are rejected,
  where before the zoom level was silently dropped and the unknown asset type
  caused a panic.
  ([#6891](https://github.com/matrix-org/matrix-rust-sdk/pull/6891))
- [**breaking**] Converged on a single subscription behavior all through the FFI
  layer in which the initial value is published immediately and outside of the
  update task.
  ([#6895](https://github.com/matrix-org/matrix-rust-sdk/pull/6895))
- [**breaking**] `ClientBuilder::username` has been renamed to
  `ClientBuilder::server_name_from_user_id`. Make sure to only build a `Client`
  with one of `homeserver_url`, `server_name`, `server_name_or_homeserver_url`
  or `server_name_from_user_id`. There's no need to use this method when the
  built `Client` will be restored from a `Session` (and there never was).
  ([#6900](https://github.com/matrix-org/matrix-rust-sdk/pull/6900))
- `RoomListService::subscribe_to_rooms` is renamed to
  `RoomListService::set_room_subscriptions`, to match the room subscription
  methods of `SlidingSync`.
  ([#6927](https://github.com/matrix-org/matrix-rust-sdk/pull/6927))
- This patch changes the `RoomListEntriesDynamicFilterKind::Unread` filter to
  `ReadReceipts { expect: RoomListFilterReadReceipts }`. Before it was looking
  at the `ReadReceipts::num_notifications` field only, now it can look at the
  following field: `num_mentions`, `num_notifications` or `num_messages`.

  The condition where `Room::is_marked_unread` makes the room to be selected if
  there is no unread is kept because (i) it's a manual operation from the user,
  (ii) it signals the room is unread but for an unknown reason, it could be
  anything, so it's important and should be displayed regardless of the number
  of unread.

  Before:

  ```rust
  RoomListEntriesDynamicFilterKind::Unread
  ```

  After:

  ```rust
  RoomListEntriesDynamicFilterKind::ReadReceipts {
      expect: RoomListFilterReadReceipts::Notifications,
  }
  ``` ([#6928](https://github.com/matrix-org/matrix-rust-sdk/pull/6928))
- Change the return value of `RawX509Signer::validity_not_after` to be in
  milliseconds rather than seconds, to match other interfaces.
  ([#6933](https://github.com/matrix-org/matrix-rust-sdk/pull/6933))
- The `ThreadSummary::public_read_receipt_event_id` and
  `ThreadSummary::private_read_receipt_event_id` fields have been removed. They
  were a hack introduced in the past and no longer make sense.
  ([#6938](https://github.com/matrix-org/matrix-rust-sdk/pull/6938))
- `SendHandle::abort()` now takes an optional `reason` (defaulting to none),
  applied to the redaction that materializes the abort when the event had
  already been sent by the time the abort was processed.
  ([#6957](https://github.com/matrix-org/matrix-rust-sdk/pull/6957))
- `SessionVerificationController` no longer surfaces an incoming verification
  request this session can't complete. A verified session that is missing the
  private self-signing key can neither sign the other device nor be signed by
  it, so the request is dropped instead of being offered to the user only to
  fail after the emojis have been compared. Requests received while this session
  is still unverified are unaffected: the other side is then the one signing us.
  ([#6971](https://github.com/matrix-org/matrix-rust-sdk/pull/6971))
- [**breaking**] The Profiles sliding sync extension (MSC4262) is now always
  enabled like the other extensions.
  `SyncServiceBuilder::with_profiles_extension` has been removed.
  ([#6984](https://github.com/matrix-org/matrix-rust-sdk/pull/6984))
- [**breaking**] `WidgetCapabilitiesProvider::acquire_capabilities` is now an
  async callback (`suspend fun` in Kotlin, `async` in Swift). The SDK awaits it
  directly instead of running it on a tokio blocking thread.
  ([#7017](https://github.com/matrix-org/matrix-rust-sdk/pull/7017))

### Fixed

- Fixed attachment upload failing when blurhash isn't present.
  ([#6662](https://github.com/matrix-org/matrix-rust-sdk/pull/6662))
- Fixed a potential deadlock in `SessionVerificationController` where delegate
  callbacks were invoked while the delegate `RwLock` read guard was held, so a
  delegate that detached itself via `set_delegate` would deadlock.
  ([#6669](https://github.com/matrix-org/matrix-rust-sdk/pull/6669))
- Fixed upload failures for attachments when `height`, `width`, `size`,
  `duration`, or `blurhash` fields are not present.
  ([#6683](https://github.com/matrix-org/matrix-rust-sdk/pull/6683))

### Added

- Add `ClientBuilder::enable_content_scanner(String)` to be able to replace the
  default `MediaFetcher` with one backed by the content scanner server in the
  provided URL.
  ([#6625](https://github.com/matrix-org/matrix-rust-sdk/pull/6625))
- The `TimelineItemContent::RtcNotification` now contains additional fields for
  when the notification is related to and active call. The new fields are
  `active_members` (if not empty then the call is active),
  `call_start_ts_millis`, `is_joined`.
  ([#6668](https://github.com/matrix-org/matrix-rust-sdk/pull/6668))
- Add `PasswordStrengthEstimator` to the FFI layer, exposing password strength
  estimation via the zxcvbn algorithm with caller-configurable ranking
  thresholds.
  ([#6708](https://github.com/matrix-org/matrix-rust-sdk/pull/6708))
- Add `SyncServiceBuilder::with_profiles_extension` to enable the Profiles
  sliding sync extension, which syncs global profile fields such as `m.status`
  and `m.call`.
  ([#6726](https://github.com/matrix-org/matrix-rust-sdk/pull/6726))
- Add `SyncServiceBuilder::with_parent_span`.
  ([#6833](https://github.com/matrix-org/matrix-rust-sdk/pull/6833))
- Add `SqliteStoreBuilder::high_entropy_passphrase`, a faster alternative to
  `SqliteStoreBuilder::passphrase` for randomly generated, high-entropy
  passphrases. Using this setting once migrates the database from a
  passphrase-based setup to a key-based setup. After the migration,
  `SqliteStoreBuilder::key` can be used instead and is equivalent to
  `SqliteStoreBuilder::high_entropy_passphrase`.

  Do **NOT** use it with human-chosen passphrases, as migrating those to a
  key-based setup would remove their brute-force protection.
  ([#6878](https://github.com/matrix-org/matrix-rust-sdk/pull/6878))

### Changed

- Use a forked Ruma version with a workaround to avoid verifying the JNA
  checksums on the generated Kotlin bindings: these checksums consistently fail
  on 32bit devices and leave any implementing client unable to use any bindings.
  ([#6764](https://github.com/matrix-org/matrix-rust-sdk/pull/6764))

### Fixed

- `Client::clear_caches()` now deletes the state store's actual SQLite sidecar
  files: it looked for `.wal`/`.shm` suffixes while SQLite names them
  `-wal`/`-shm`, so the stale write-ahead log survived the cache clear. The
  rebuilt client then opened a fresh, empty state store next to the previous
  database's journal, which could fail with "disk I/O error" (reproducible when
  clearing caches while sync is active).
  ([#6811](https://github.com/matrix-org/matrix-rust-sdk/pull/6811))

## [0.18.0](https://github.com/matrix-org/matrix-rust-sdk/tree/0.18.0) - 2026-06-02

### Added

- Add `RoomInfo::fully_read_event_id` to expose the user's `m.fully_read` event
  ID. ([#6569](https://github.com/matrix-org/matrix-rust-sdk/pull/6569))
- Expose `Client::tile_server` on the FFI, returning a `TileServerInfo` record
  when the homeserver advertises a map tile server through its matrix client
  well-known
  ([MSC3488](https://github.com/matrix-org/matrix-spec-proposals/pull/3488)).
  The record carries a single `map_style_url` field pointing at a MapLibre
  `style.json`.
  ([#6610](https://github.com/matrix-org/matrix-rust-sdk/pull/6610))
- Expose `SqliteStoreBuilder::key` to allow clients to specify a key as a
  32-bytes array that will be used as is, skipping the key derivation process
  and speeding up opening DB connections. Note this should only be used for
  truly random values with high entropy, for user-provided passphrases the
  `SqliteStoreBuilder::passphrase` function should still be used. Some examples
  for the bindings are:

  Kotlin:

  ```kotlin
  val buffer = ByteArray(size = 32)
  SecureRandom().nextBytes(buffer)
  return buffer // this now contains the key
  ```

  Swift:

  ```swift
  var bytes = [UInt8](repeating: 0, count: 32)
  SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes)
  return Data(bytes: bytes) // this now contains the key
  ``` ([#6805](https://github.com/matrix-org/matrix-rust-sdk/pull/6805))

### Changed

- [**breaking**] `SpaceRoomList::rooms` and
  `SpaceRoomList::subscribe_to_room_updates` are now asynchronous.
  ([#6561](https://github.com/matrix-org/matrix-rust-sdk/pull/6561))
- [**breaking**] `Client::set_pusher` now takes an `append: bool` parameter,
  forwarded to the homeserver. Pass `true` to keep an existing pusher with the
  same `app_id` and `pushkey` registered for other users (e.g. multi-profile
  clients on a single device); pass `false` to preserve the previous default
  behaviour. ([#6600](https://github.com/matrix-org/matrix-rust-sdk/pull/6600))

## [0.17.0] - 2026-05-08

### Bug fixes

- Add `Client::set_avatar_url` to manually set the avatar URL of the user to a
  provided MXC one.
- Allow setting a custom Sliding Sync connection ID and timeline limit on
  `RoomListService`.
  ([#6289](https://github.com/matrix-org/matrix-rust-sdk/pull/6289))
- Fix devices on Android 11 crashing because the SDK could not be initialized
  using `libloading` to get a reference to the JVM. Replaced `libloading` with
  `jvm-getter`, which works like a compatibility layer.
  ([#6370](https://github.com/matrix-org/matrix-rust-sdk/pull/6370))
- Added `android_platform.rs` for fixing the `rustls` integration on Android,
  which was broken.
  ([#6306](https://github.com/matrix-org/matrix-rust-sdk/pull/6306))
- [**breaking**] `OtherState` properly supports redacted events that still have
  fields in the content. The following fields are no longer optional:
  - `federate` in `OtherState::RoomCreate`.
  - `history_visibility` in `OtherState::RoomHistoryVisibility`.
  - `thresholds` in `OtherState::RoomPowerLevels`.
- `omit_checksums` option is now enabled for the Kotlin bindings in all
  FFI-exporting crates. We enabled them because with JNA direct mapping enabled
  they result in invalid checks in ARM 32bit devices, preventing the SDK from
  working altogether (see
[this issue](https://github.com/mozilla/uniffi-rs/issues/2740)).
([#6069](https://github.com/matrix-org/matrix-rust-sdk/pull/6069),
[#6112](https://github.com/matrix-org/matrix-rust-sdk/pull/6112),
[#6115](https://github.com/matrix-org/matrix-rust-sdk/pull/6115),
[#6116](https://github.com/matrix-org/matrix-rust-sdk/pull/6116)).
- `Client::create_room` now uses `RoomPowerLevelsContentOverride` under the hood
  instead of `RoomPowerLevelsEventContent` to be able to explicitly set values
  which would previously be ignored if they matched the default power level
  values specified by the spec: these may not be the same in the homeserver and
  result in rooms with incorrect power levels being created.
  ([#6034](https://github.com/matrix-org/matrix-rust-sdk/pull/6034))
- Fix the `is_last_admin` check in `LeaveSpaceRoom` since it was not
  accounting for the membership state.
  [#6032](https://github.com/matrix-org/matrix-rust-sdk/pull/6032)
- [**breaking**] `LatestEventValue::Local { is_sending: bool }` is replaced
  by [`state: LatestEventValueLocalState`] to represent 3 states: `IsSending`,
  `HasBeenSent` and `CannotBeSent`.
  ([#5968](https://github.com/matrix-org/matrix-rust-sdk/pull/5968/))

### Features

- `RoomNotificationInfo`, `NotificationItem` and `SpaceRoom` now have `is_dm`
  fields. ([#6537](https://github.com/matrix-org/matrix-rust-sdk/pull/6537))
- Expose `RoomMember::is_service_member` field.
  ([#6536](https://github.com/matrix-org/matrix-rust-sdk/pull/6536))
- Expose `beacon` and `beacon_info` fields in `RoomPowerLevelsValues` and
  `RoomPowerLevelChanges`, allowing clients to read and update the power levels
  required to send beacon (live location) message events and beacon info state
  events respectively.
  ([#6540](https://github.com/matrix-org/matrix-rust-sdk/pull/6540))
- Expose `ClientBuilder::dm_room_definition` to customize the DM room definition
  used by the `Client`, added `RoomInfo::is_dm` field based on it.
  ([#6490](https://github.com/matrix-org/matrix-rust-sdk/pull/6490))
- Expose `HumanQrGrantLoginError::Unknown` reason in error message.
  ([#6514](https://github.com/matrix-org/matrix-rust-sdk/pull/6514))
- Add a list of `declined_by: Vec<String>` to the
  `TimelineItemContent::RtcNotification`, this will contain the list of users
  that have declined the call.
  ([#6494](https://github.com/matrix-org/matrix-rust-sdk/pull/6494))
- Add `RoomInfo::active_service_members_count` and
  `NotificationRoomInfo::active_service_members_count`, returning the amount of
  service members that are part of the room.
  ([#6483](https://github.com/matrix-org/matrix-rust-sdk/pull/6483))
- Add `Client::get_dm_rooms` function to get a list with the DMs for the
  provided user id.
  ([#6487](https://github.com/matrix-org/matrix-rust-sdk/pull/6487))
- Expose `ffi::NotificationRoomInfo::service_members` so clients can use the
  list of service members to calculate if a room is a DM from the notification
  info. ([#6474](https://github.com/matrix-org/matrix-rust-sdk/pull/6474))
- Enable `experimental-push-secrets` feature by default.
  ([#6473](https://github.com/matrix-org/matrix-rust-sdk/pull/6394))
- Add new high-level search helpers `RoomSearchIterator` and
  `GlobalSearchIterator` to perform searches for messages in a room or across
  all rooms. ([6394](https://github.com/matrix-org/matrix-rust-sdk/pull/6394))
- Added the `Client.request_openid_token()` method.
  ([#6458](https://github.com/matrix-org/matrix-rust-sdk/pull/6458))
- Added the `Client::import_secrets_bundle` method.
  ([#6212](https://github.com/matrix-org/matrix-rust-sdk/pull/6212))
- [**breaking**] Remove support for `native-tls` and remove all feature
  flags for selecting TLS backend, as `rustls` is the now the only supported
  TLS backend.
  ([#6409](https://github.com/matrix-org/matrix-rust-sdk/pull/6409))
- Expose `event_type_raw` and `latest_json()` on `EventTimelineItem`,
  allowing clients to access the raw event type string and full event JSON for
  custom event handling without pattern-matching through nested enums.
  ([#6387](https://github.com/matrix-org/matrix-rust-sdk/pull/6387))
  ([#6424](https://github.com/matrix-org/matrix-rust-sdk/pull/6424))
- Expose sync v2 API through FFI via `Client.sync_v2()` and
  `Client.sync_once_v2()`, enabling mobile clients to sync without
  requiring Sliding Sync support on the homeserver. `Client.sync_v2()`
  accepts a `SyncListenerV2` callback that receives a `SyncResponseV2`
  after each successful sync.
  ([#6359](https://github.com/matrix-org/matrix-rust-sdk/pull/6359))
- Added `HomeserverCapabilities` and `Client::homeserver_capabilities()` to get
  the capabilities of the homeserver.
  ([#6371](https://github.com/matrix-org/matrix-rust-sdk/pull/6371))
- Expose `Room.send_state_event_raw()` for sending arbitrary state events
  through the FFI layer.
  ([#6350](https://github.com/matrix-org/matrix-rust-sdk/pull/6350))
- Introduce a `ThreadListService` which offers reactive interfaces for rendering
  and managing the list of threads from a particular room.
  ([6311](https://github.com/matrix-org/matrix-rust-sdk/pull/6311))
- [**breaking**] Move `LiveLocation` out of `TimelineItemContent` and into
  `MsgLikeKind` so it has access to `MsgLikeContent` `reactions`.
  ([#6286](https://github.com/matrix-org/matrix-rust-sdk/pull/6286))
- Add `HumanQrLoginError::UnsupportedQrCodeType` for when a QR is parseable but
  cannot be used to complete a login.
  ([#6141](https://github.com/matrix-org/matrix-rust-sdk/pull/6285)
- Add `HumanQrGrantLoginError::UnsupportedQrCodeType` for when a QR is parseable
  but cannot be used to grant a login.
  ([#6141](https://github.com/matrix-org/matrix-rust-sdk/pull/6285)
- Add the `QrCodeData::base_url` and `QrCodeData::intent` methods.
  ([#6283](https://github.com/matrix-org/matrix-rust-sdk/pull/6283))
- Add `Encryption::recover_and_fix_backup` to automatically fix key storage
  backup if the private backup decryption key is missing, invalid or
  inconsistent with the public key.
  ([#6252](https://github.com/matrix-org/matrix-rust-sdk/pull/6252))
- Add support for
  [MSC3489](https://github.com/matrix-org/matrix-spec-proposals/pull/3489)  
  live location sharing through a new `TimelineItemContent::LiveLocation`
  variant. ([#6232](https://github.com/matrix-org/matrix-rust-sdk/pull/6232))
- Add `HumanQrGrantLoginError::ConnectionInsecure` for errors establishing the
  secure channel
  ([#6141](https://github.com/matrix-org/matrix-rust-sdk/pull/6141)
- Add `HumanQrGrantLoginError::Expired` for when a timeout is encountered during
  the grant ([#6141](https://github.com/matrix-org/matrix-rust-sdk/pull/6141)
- Add `HumanQrGrantLoginError::Cancelled` for when the grant is cancelled
  ([#6141](https://github.com/matrix-org/matrix-rust-sdk/pull/6141)
- Add `HumanQrGrantLoginError::OtherDeviceAlreadySignedIn` for when the other
  device is already signed in
  ([#6141](https://github.com/matrix-org/matrix-rust-sdk/pull/6141)
- Add `HumanQrGrantLoginError::DeviceNotFound` for when the requested device was
  not returned by the homeserver
  ([#6141](https://github.com/matrix-org/matrix-rust-sdk/pull/6141)
- Add `RoomInfo::is_low_priority` for getting the room's `m.lowpriority` tag
  state ([#6183](https://github.com/matrix-org/matrix-rust-sdk/pull/6183))
- Add `Client::subscribe_to_duplicate_key_upload_errors` for listening to
  duplicate key upload errors from `/keys/upload`.
  ([#6135](https://github.com/matrix-org/matrix-rust-sdk/pull/6135/))
- Add `NotificationItem::raw_event` to get the raw event content of the event
  that triggered the notification, which can be useful for debugging and to
  support clients that want to implement custom handling for certain
  notifications.
  ([#6122](https://github.com/matrix-org/matrix-rust-sdk/pull/6122))
- [**breaking**] Extend `TimelineFocus::Event` to allow marking the target
  event as the root of a thread.
  [#6050](https://github.com/matrix-org/matrix-rust-sdk/pull/6050)
- [**breaking**] Remove `TimelineFilter::EventTypeFilter` which has been
  replaced by the more generic `TimelineFilter::EventFilter`. Users of
  `TimelineEventTypeFilter::include` and `TimelineEventTypeFilter::exclude` can
  switch to `TimelineEventFilter::include_event_types` and
  `TimelineEventFilter::exclude_event_types`.
  ([#6070](https://github.com/matrix-org/matrix-rust-sdk/pull/6070/))
- Add `TimelineFilter::EventFilter` for filtering events based on their type or
  content. For content filtering, only membership and profile change filters
  are available as of now.
  ([#6048](https://github.com/matrix-org/matrix-rust-sdk/pull/6048/))
- Introduce `SpaceFilter`s as a mechanism for narrowing down what's displayed in
  the room list
  ([#6025](https://github.com/matrix-org/matrix-rust-sdk/pull/6025))
- Expose room power level thresholds in `OtherState::RoomPowerLevels` (ban,
  kick, invite, redact, state & events defaults, per-event overrides,
  notifications), so clients can compute the required power level for actions
  and compare with previous values.
  ([#5931](https://github.com/matrix-org/matrix-rust-sdk/pull/5931))
- Add `RoomCreationParameters::is_space` parameter to be able to create spaces.
  ([#6010](https://github.com/matrix-org/matrix-rust-sdk/pull/6010/))
- [**breaking**] `LazyTimelineItemProvider::get_shields` no longer returns an an
  `Option`: the `ShieldState` type contains a `None` variant, so the `Option`
  was redundant. The `message` field has also been removed: since there was no
  way to localise the returned string, applications should not be using it.
  ([#5959](https://github.com/matrix-org/matrix-rust-sdk/pull/5959))
- Add `Room::list_threads` to list all the threads in a room.
  ([#5953](https://github.com/matrix-org/matrix-rust-sdk/pull/5953))
- Add `SpaceService::get_space_room` to get a space given its id from the space
  graph if available.
[#5944](https://github.com/matrix-org/matrix-rust-sdk/pull/5944)
- Add `QrCodeData::to_bytes()` to allow generation of a QR code.
  ([#5939](https://github.com/matrix-org/matrix-rust-sdk/pull/5939))
- [**breaking**]: The new Latest Event API replaces the old API.
  `Room::new_latest_event` overwrites the `Room::latest_event` method. See the
  documentation of `matrix_sdk::latest_event` to learn about the new API.
  [#5624](https://github.com/matrix-org/matrix-rust-sdk/pull/5624/)
- Created `RoomPowerLevels::events` function which returns a
  `HashMap<TimelineEventType, i64>` with all the power levels per event type.
  ([#5937](https://github.com/matrix-org/matrix-rust-sdk/pull/5937))
- Expose `EventTimelineItem::forwarder` and `forwarder_profile`, which, if
  present, provide the ID and profile of the user who forwarded the keys used to
  decrypt the event as part of an
  [MSC4268](https://github.com/matrix-org/matrix-spec-proposals/pull/4268) key
  bundle. ([#6000](https://github.com/matrix-org/matrix-rust-sdk/pull/6000))
- Add `NonFavorite` filter to the Room List API.
  ([#5991](https://github.com/matrix-org/matrix-rust-sdk/pull/5991)
- Add `call_intent` (either `RtcCallIntent::Audio` or `RtcCallIntent::Video`)
  field to `RtcNotification` event content.
  ([#6207](https://github.com/matrix-org/matrix-rust-sdk/pull/6207))
- Add `RoomInfo::active_room_call_consensus_intent` method to get the call
  intent for the current call, based on what members are advertising.
  ([#6274](https://github.com/matrix-org/matrix-rust-sdk/pull/6274))

### Refactor

- [**breaking**] All OIDC related types and functions have been renamed from
  `Oidc`/`oidc` to `OAuth`/`oauth` to align with the finalised naming in the
  spec. ([#6500](https://github.com/matrix-org/matrix-rust-sdk/pull/6500))
- [**breaking**] `LiveLocationShares` has been renamed to
  `LiveLocationsObserver` and `Room::live_location_shares` to
  `Room::live_locations_observer`.
  ([#6446](https://github.com/matrix-org/matrix-rust-sdk/pull/6446))
- [**breaking**] `Room::observe_live_location_shares` has been replaced by
  `Room::live_locations_observer`. Call [`LiveLocationsObserver::subscribe`] on
  it to receive an initial snapshot and a stream of incremental updates.The
  stream is seeded from the event cache on creation and includes the own user's
  shares (previously excluded). `LiveLocationShare.is_live` has been removed;
  instead `ts` (start timestamp) and `timeout` (duration in milliseconds) are
  now exposed so clients can compute liveness themselves via
  `current_time < ts + timeout`. Non-live shares are automatically removed from
  the list. A new `LiveLocationShareListener` callback interface must be
  implemented and passed to the method.
  ([#6385](https://github.com/matrix-org/matrix-rust-sdk/pull/6385))
- [**breaking**] The `RoomAliases` variants of `StateEventContent`,
  `StateEventType` and `OtherState` was removed. This state event type was
  removed from the Matrix specification a while ago, and support for it has been
  removed in Ruma.
  ([#6414](https://github.com/matrix-org/matrix-rust-sdk/pull/6414))
- `Client::new` no longer unnecessarily instantiates an `OAuth` component if
  `CrossProcessLockConfig::SingleProcess` is used.
  ([#6293](https://github.com/matrix-org/matrix-rust-sdk/pull/6293))
- [**breaking**] `Room::report_content()` no longer takes a `score` argument,
  because it was removed from the Matrix specification.
  ([#6256](https://github.com/matrix-org/matrix-rust-sdk/pull/6256))
- [**breaking**] The `current_version` field of
  `ErrorKind::WrongRoomKeysVersion` is no longer optional.
  ([#6241](https://github.com/matrix-org/matrix-rust-sdk/pull/6241))
- [**breaking**] The following variants of `AccountManagementAction` were
  renamed to match their new names after being merge in the Matrix
  specification:
  - `SessionsList` is renamed to `DevicesList`
  - `SessionView` is renamed to `DeviceView`
  - `SessionEnd` is renamed to `DeviceDelete`
  ([#6217](https://github.com/matrix-org/matrix-rust-sdk/pull/6217))
- [**breaking**] `HumanQrGrantLoginError::UnableToCreateDevice` has been removed
  ([#6141](https://github.com/matrix-org/matrix-rust-sdk/pull/6141)
- [**breaking**] Removed `ClientBuilder::enable_oidc_refresh_lock` in favour of
  using `ClientBuilder::cross_process_lock_config` to configure that lock when a
  `MultiProcess` configuration is supplied.
  ([#6204](https://github.com/matrix-org/matrix-rust-sdk/pull/6204))
- `RoomPaginationStatus` is renamed to `PaginationStatus`.
  ([#6174](https://github.com/matrix-org/matrix-rust-sdk/pull/6174/))
- [**breaking**] Replaced `ClientBuilder::cross_process_store_locks_holder_name`
  with `ClientBuilder::cross_process_lock_config`, which accepts a
  `CrossProcessLockConfig` value to specify whether the resulting `Client` will
  be used in a single process or multiple processes.
  ([#6160](https://github.com/matrix-org/matrix-rust-sdk/pull/6160))
- [**breaking**] Refactored `is_last_admin` to `is_last_owner` the check will
  now account also for v12 rooms, where creators and users with PL 150 matter.
  ([#6036](https://github.com/matrix-org/matrix-rust-sdk/pull/6036))
- [**breaking**] The existing `TimelineEventType` was renamed to
  `TimelineEventContent`, because it contained the actual contents of the event.
  Then, we created a new `TimelineEventType` enum that actually contains _just_
  the event type.
  ([#5937](https://github.com/matrix-org/matrix-rust-sdk/pull/5937))
- [**breaking**] The function `TimelineEvent::event_type` is now
  `TimelineEvent::content`.
  ([#5937](https://github.com/matrix-org/matrix-rust-sdk/pull/5937))
- [**breaking**] The `SpaceService` will no longer auto-subscribe to required
  client events when invoking the `subscribe_to_joined_spaces` but instead do it
  through its, now async, constructor.
  ([#5972](https://github.com/matrix-org/matrix-rust-sdk/pull/5972))
- [**breaking**] The `SpaceService`'s `joined_spaces` method has been renamed
  `top_level_joined_spaces` and `subscribe_to_joined_spaces` to
  `space_service.subscribe_to_top_level_joined_spaces`
  ([#5972](https://github.com/matrix-org/matrix-rust-sdk/pull/5972))

## [0.16.1] - 2026-05-08

No notable changes in this release.

## [0.16.0] - 2025-12-04

### Breaking changes

- `TimelineConfiguration::track_read_receipts`'s type is now an enum to allow
  tracking to be enabled for all events (like before) or only for message-like
  events (which prevents read receipts from being placed on state events).
  ([#5900](https://github.com/matrix-org/matrix-rust-sdk/pull/5900))
- `Client::reset_server_info()` has been split into `reset_supported_versions()`
  and `reset_well_known()`.
  ([#5910](https://github.com/matrix-org/matrix-rust-sdk/pull/5910))
- Add `HumanQrLoginError::NotFound` for non-existing / expired rendezvous
  sessions ([#5898](https://github.com/matrix-org/matrix-rust-sdk/pull/5898))
- Add `HumanQrGrantLoginError::NotFound` for non-existing / expired rendezvous
  sessions ([#5898](https://github.com/matrix-org/matrix-rust-sdk/pull/5898))
- The `LatestEventValue::Local` type gains 2 new fields: `sender` and `profile`.
  ([#5885](https://github.com/matrix-org/matrix-rust-sdk/pull/5885))
- The `Encryption::user_identity()` method has received a new argument. The
  `fallback_to_server` argument controls if we should attempt to fetch the user
  identity from the homeserver if it wasn't found in the local storage.
  ([#5870](https://github.com/matrix-org/matrix-rust-sdk/pull/5870))
- Expose the power level required to modify `m.space.child` on
  `room::power_levels::RoomPowerLevelsValues`.
- Rename `Client::login_with_qr_code` to
  `Client::new_login_with_qr_code_handler`.
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

- UniFFI was upgraded to `v0.30.0`
  ([#5808](https://github.com/matrix-org/matrix-rust-sdk/pull/5808)).
- The `waveform` parameter in `Timeline::send_voice_message` format changed to a
  list of `f32` between 0 and 1.
  ([#5732](https://github.com/matrix-org/matrix-rust-sdk/pull/5732))
- The `normalized_power_level` field has been removed from the `RoomMember`
  struct.
  ([#5635](https://github.com/matrix-org/matrix-rust-sdk/pull/5635))
- Remove the deprecated `CallNotify` event (`org.matrix.msc4075.call.notify`) in
  favor of the new `RtcNotification` event
  (`org.matrix.msc4075.rtc.notification`).
  ([#5668](https://github.com/matrix-org/matrix-rust-sdk/pull/5668))
- Add `QrLoginProgress::SyncingSecrets` to indicate that secrets are being
  synced between the two devices.
  ([#5760](https://github.com/matrix-org/matrix-rust-sdk/pull/5760))
- Add `Room::subscribe_to_send_queue_updates` to observe room send queue
  updates. ([#5761](https://github.com/matrix-org/matrix-rust-sdk/pull/5761))
- `Client::login_with_qr_code` now returns a handler that allows performing the
  flow with either the current device scanning or generating the QR code.
  Additionally, new errors `HumanQrLoginError::CheckCodeAlreadySent` and
  `HumanQrLoginError::CheckCodeCannotBeSent` were added.
  ([#5786](https://github.com/matrix-org/matrix-rust-sdk/pull/5786))
- `ComposerDraft` now includes attachments alongside the text message.
  ([#5794](https://github.com/matrix-org/matrix-rust-sdk/pull/5794))
- Add `Client::subscribe_to_send_queue_updates` to observe global send queue
  updates. ([#5784](https://github.com/matrix-org/matrix-rust-sdk/pull/5784))

### Features

- Add `Client::get_store_sizes()` so to query the size of the existing stores,
  if available.
  ([#5911](https://github.com/matrix-org/matrix-rust-sdk/pull/5911))
- Expose `is_space` in `NotificationRoomInfo`, allowing clients to determine if
  the room that triggered the notification is a space.
- Add push actions to `NotificationItem` and replace `SyncNotification` with
  `NotificationItem`.
  ([#5835](https://github.com/matrix-org/matrix-rust-sdk/pull/5835))
- Add `Client::new_grant_login_with_qr_code_handler` for granting login to a new
  device by way of a QR code.
  ([#5836](https://github.com/matrix-org/matrix-rust-sdk/pull/5836))
- Add `Client::register_notification_handler` for observing notifications
  generated from sync responses.
  ([#5831](https://github.com/matrix-org/matrix-rust-sdk/pull/5831))
- Add `Room::mark_as_fully_read_unchecked` so clients can mark a room as read
  without needing a `Timeline` instance. Note this method is not recommended as
  it can potentially cause incorrect read receipts, but it can needed in certain
  cases.
- Add `Timeline::latest_event_id` to be able to fetch the event id of the latest
  event of the timeline.
- Add `Room::load_or_fetch_event` so we can get a `TimelineEvent` given its
  event id ([#5678](https://github.com/matrix-org/matrix-rust-sdk/pull/5678)).
- Add `TimelineEvent::thread_root_event_id` to expose the thread root event id
  for this type too
  ([#5678](https://github.com/matrix-org/matrix-rust-sdk/pull/5678)).
- Add `NotificationSettings::get_raw_push_rules` so clients can fetch the raw
  JSON content of the push rules of the current user and include it in bug
  reports ([#5706](https://github.com/matrix-org/matrix-rust-sdk/pull/5706)).
- Add new API to decline calls
  ([MSC4310](https://github.com/matrix-org/matrix-spec-proposals/pull/4310)):
  `Room::decline_call` and `Room::subscribe_to_call_decline_events`
  ([#5614](https://github.com/matrix-org/matrix-rust-sdk/pull/5614))
- Expose `m.federate` in `OtherState::RoomCreate` and `history_visibility` in
  `OtherState::RoomHistoryVisibility`, allowing clients to know whether a room
  federates and how its history is shared in the appropriate timeline events.
- Expose `join_rule` in `OtherState::RoomJoinRules`, allowing clients to know
  the join rules of a room from the appropriate timeline events.

### Changes

- `Timeline::latest_event_id` now uses its `ui::Timeline::latest_event_id`
  counterpart, instead of getting the latest event from the timeline and then
  its id.([#5864](https://github.com/matrix-org/matrix-rust-sdk/pull/5864))
- Build Android ARM64 bindings using better default RUSTFLAGS (the same used for
  iOS ARM64). This should improve performance.
  [(#5854)](https://github.com/matrix-org/matrix-rust-sdk/pull/5854)

## [0.14.0] - 2025-09-04

### Features

- Add `LowPriority` and `NonLowPriority` variants to
  `RoomListEntriesDynamicFilterKind` for filtering rooms based on their low
  priority status. These filters allow clients to show only low priority rooms
  or exclude low priority rooms from the room list.
  ([#5508](https://github.com/matrix-org/matrix-rust-sdk/pull/5508))
- Add `room_version` and `privileged_creators_role` to `RoomInfo`
  ([#5449](https://github.com/matrix-org/matrix-rust-sdk/pull/5449)).
- The [`unstable-hydra`] feature has been enabled, which enables room v12
  changes in the SDK.
  ([#5450](https://github.com/matrix-org/matrix-rust-sdk/pull/5450)).
- Add experimental support for
  [MSC4306](https://github.com/matrix-org/matrix-spec-proposals/pull/4306), with
  the `Room::fetch_thread_subscription()` and `Room::set_thread_subscription()`
  methods. ([#5442](https://github.com/matrix-org/matrix-rust-sdk/pull/5442))
- [**breaking**] [`GalleryUploadParameters::reply`] and
  [`UploadParameters::reply`] have been both replaced with a new optional
  `in_reply_to` field, that's a string which will be parsed into an
  `OwnedEventId` when sending the event. The thread relationship will be
  automatically filled in, based on the timeline focus.
  ([5427](https://github.com/matrix-org/matrix-rust-sdk/pull/5427))
- [**breaking**] [`Timeline::send_reply()`] now automatically fills in the
  thread relationship, based on the timeline focus. As a result, it only takes
  an `OwnedEventId` parameter, instead of the `Reply` type. The proper way to
  start a thread is now thus to create a threaded-focused timeline, and then use
  `Timeline::send()`.
  ([5427](https://github.com/matrix-org/matrix-rust-sdk/pull/5427))
- Add `HomeserverLoginDetails::supports_sso_login` for legacy SSO support
  information. This is primarily for Element X to give a dedicated error message
  in case it connects a homeserver with only this method available.
  ([#5222](https://github.com/matrix-org/matrix-rust-sdk/pull/5222))

### Breaking changes

- The timeline will now always use the send queue to upload medias, so the
  `UploadParameters::use_send_queue` bool has been removed. Make sure to listen
  to the send queue's error updates, and to handle send queue restarts.
  ([#5525](https://github.com/matrix-org/matrix-rust-sdk/pull/5525))
- Support for the legacy media upload progress has been disabled. Media upload
  progress is available through the send queue, and can be enabled thanks to
  `Client::enable_send_queue_upload_progress()`.
  ([#5525](https://github.com/matrix-org/matrix-rust-sdk/pull/5525))
- `TimelineDiff` is now exported as a true `uniffi::Enum` instead of the weird
  `uniffi::Object` hybrid. This matches both `RoomDirectorySearchEntryUpdate`
  and `RoomListEntriesUpdate` and can be used in the same way.
  ([#5474](https://github.com/matrix-org/matrix-rust-sdk/pull/5474))
- The `creator` field of `RoomInfo` has been renamed to `creators` and can now
  contain a list of user IDs, to reflect that a room can now have several
  creators, as introduced in room version 12.
  ([#5436](https://github.com/matrix-org/matrix-rust-sdk/pull/5436))
- The `PowerLevel` type was introduced to represent power levels instead of
  `i64` to differentiate the infinite power level of creators, as introduced in
  room version 12. It is used in `suggested_role_for_power_level`,
  `suggested_power_level_for_role` and `RoomMember`.
  ([#5436](https://github.com/matrix-org/matrix-rust-sdk/pull/5436))
- `Client::get_url` now returns a `Vec<u8>` instead of a `String`. It also
  throws an error when the response isn't status code 200 OK, instead of
  providing the error in the response body.
  ([#5438](https://github.com/matrix-org/matrix-rust-sdk/pull/5438))
- `RoomPreview::info()` doesn't return a result anymore. All unknown join rules
  are handled in the `JoinRule::Custom` variant.
  ([#5337](https://github.com/matrix-org/matrix-rust-sdk/pull/5337))
- The `reason` argument of `Room::report_room` is now required, do to a
  clarification in the spec.
  ([#5337](https://github.com/matrix-org/matrix-rust-sdk/pull/5337))
- `PublicRoomJoinRule` has more variants, supporting all the known values from
  the spec. ([#5337](https://github.com/matrix-org/matrix-rust-sdk/pull/5337))
- The fields of `MediaPreviewConfig` are both optional, allowing to use the type
  for room account data as well as global account data.
  ([#5337](https://github.com/matrix-org/matrix-rust-sdk/pull/5337))
- The `event_id` field of `PredecessorRoom` was removed, due to its removal in
  the Matrix specification with MSC4291.
  ([#5419](https://github.com/matrix-org/matrix-rust-sdk/pull/5419))
- `Client::url_for_oidc` now allows requesting additional scopes for the OAuth2
  authorization code grant.
  ([#5395](https://github.com/matrix-org/matrix-rust-sdk/pull/5395))
- `Client::url_for_oidc` now allows passing an optional existing device id from
  a previous login call.
  ([#5394](https://github.com/matrix-org/matrix-rust-sdk/pull/5394))
- `ClientBuilder::build_with_qr_code` has been removed. Instead, the Client
  should be built by passing `QrCodeData::server_name` to
  `ClientBuilder::server_name_or_homeserver_url`, after which QR login can be
  performed by calling `Client::login_with_qr_code`.
  ([#5388](https://github.com/matrix-org/matrix-rust-sdk/pull/5388))
- The MSRV has been bumped to Rust 1.88.
  ([#5431](https://github.com/matrix-org/matrix-rust-sdk/pull/5431))
- `Room::send_call_notification` and `Room::send_call_notification_if_needed`
  have been removed, since the event type they send is outdated, and `Client` is
  not actually supposed to be able to join MatrixRTC sessions (yet). In
  practice, users of these methods probably already rely on another MatrixRTC
  implementation to participate in sessions, and such an implementation should
  be capable of sending notifications itself.
- The `GalleryItemInfo` variants now take an `UploadSource` rather than a
  `String` path to enable uploading from bytes directly.
  ([#5529](https://github.com/matrix-org/matrix-rust-sdk/pull/5529))
- Media and gallery uploads now use `UploadSource` to specify the thumbnail.
  ([#5530](https://github.com/matrix-org/matrix-rust-sdk/pull/5530))

## [0.13.0] - 2025-07-10

### Features

- Add `NotificationRoomInfo::topic` to the `NotificationRoomInfo` struct, which
  contains the topic of the room. This is useful for displaying the room topic
  in notifications.
  ([#5300](https://github.com/matrix-org/matrix-rust-sdk/pull/5300))
- Add `EmbeddedEventDetails::timestamp` and
  `EmbeddedEventDetails::event_or_transaction_id` which are already available in
  regular timeline items.
  ([#5331](https://github.com/matrix-org/matrix-rust-sdk/pull/5331))
- `RoomListService::subscribe_to_rooms` becomes `async` and automatically calls
  `matrix_sdk::latest_events::LatestEvents::listen_to_room`
  ([#5369](https://github.com/matrix-org/matrix-rust-sdk/pull/5369))

### Refactor

- Adjust features in the `matrix-sdk-ffi` crate to expose more platform-specific
  knobs. Previously the `matrix-sdk-ffi` was configured primarily by target
  configs, choosing between the tls flavor (`rustls-tls` or `native-tls`) and
  features like `sentry` based purely on the target. As we work to add an
  additional Wasm target to this crate, the cross product of target specific
  features has become somewhat chaotic, and we have shifted to externalize these
  choices as feature flags.

  To maintain existing compatibility on the major platforms, these features
  should be used: Android: `"bundled-sqlite,unstable-msc4274,rustls-tls,sentry"`
  iOS: `"bundled-sqlite,unstable-msc4274,native-tls,sentry"` Javascript/Wasm:
  `"unstable-msc4274,native-tls"`

  In the future additional choices (such as session storage, `sqlite` and
  `indexeddb`) will likely be added as well.

Breaking changes:

- `Client::reset_server_capabilities` has been renamed to
  `Client::reset_server_info`.
  ([#5167](https://github.com/matrix-org/matrix-rust-sdk/pull/5167))
- `RoomPreview::join_rule`, `NotificationItem::join_rule`,
  `RoomInfo::is_public`, and `Room::is_public()` return values are now optional.
  They will be set to `None` if the join rule state event is missing for a given
  room. `NotificationRoomInfo::is_public` has been removed; callers can inspect
  the value of `NotificationItem::join_rule` to determine if the room is public
  (i.e. if the join rule is `Public`).
  ([#5278](https://github.com/matrix-org/matrix-rust-sdk/pull/5278))

## [0.12.0] - 2025-06-10

Breaking changes:

- `Client::send_call_notification_if_needed` now returns `Result<bool>` instead
  of `Result<()>` so we can check if the event was sent.
- `Client::upload_avatar` and `Timeline::send_attachment` now may fail if a file
  too large for the homeserver media config is uploaded.
- `UploadParameters` replaces field `filename: String` with
  `source: UploadSource`. `UploadSource` is an enum which may take a filename or
  a filename and bytes, which allows a foreign language to read file contents
  natively and then pass those contents to the foreign function when uploading a
  file through the `Timeline`.
  ([#4948](https://github.com/matrix-org/matrix-rust-sdk/pull/4948))
- `RoomInfo` replaces its field `is_tombstoned: bool` with
  `tombstone: Option<RoomTombstoneInfo>`, containing the data needed to
  implement the room migration UI, a message and the replacement room id.
  ([#5027](https://github.com/matrix-org/matrix-rust-sdk/pull/5027))

Additions:

- `Client::subscribe_to_room_info` allows clients to subscribe to room info
  updates in rooms which may not be known yet. This is useful when displaying a
  room preview for an unknown room, so when we receive any membership change for
  it, we can automatically update the UI.
- `Client::get_max_media_upload_size` to get the max size of a request sent to
  the homeserver so we can tweak our media uploads by compressing/transcoding
  the media.
- Add `ClientBuilder::enable_share_history_on_invite` to enable experimental
  support for sharing encrypted room history on invite, per
  [MSC4268](https://github.com/matrix-org/matrix-spec-proposals/pull/4268).
  ([#5141](https://github.com/matrix-org/matrix-rust-sdk/pull/5141))
- Support for adding a Sentry layer to the FFI bindings has been added. Only
  `tracing` statements with the field `sentry=true` will be forwarded to Sentry,
  in addition to default Sentry filters.
- Add room topic string to `StateEventContent`
- Add `UploadSource` for representing upload data - this is analogous to
  `matrix_sdk_ui::timeline::AttachmentSource`
- Add `Client::observe_account_data_event` and
  `Client::observe_room_account_data_event` to subscribe to global and room
  account data changes.
  ([#4994](https://github.com/matrix-org/matrix-rust-sdk/pull/4994))
- Add `Timeline::send_gallery` to send MSC4274-style galleries.
  ([#5163](https://github.com/matrix-org/matrix-rust-sdk/pull/5163))
- Add `reply_params` to `GalleryUploadParameters` to allow sending galleries as
  (threaded) replies.
  ([#5173](https://github.com/matrix-org/matrix-rust-sdk/pull/5173))

Breaking changes:

- `contacts` has been removed from `OidcConfiguration` (it was unused since the
  switch to OAuth).

## [0.11.0] - 2025-04-11

Breaking changes:

- `TracingConfiguration` now includes a new field `trace_log_packs`, which gives
  a convenient way to set the TRACE log level for multiple targets related to a
  given feature.
  ([#4824](https://github.com/matrix-org/matrix-rust-sdk/pull/4824))

- `setup_tracing` has been renamed `init_platform`; in addition to the
  `TracingConfiguration` parameter it also now takes a boolean indicating
  whether to spawn a minimal tokio runtime for the application; in general for
  main app processes this can be set to `false`, and memory-constrained programs
  can set it to `true`.

- Matrix client API errors coming from API responses will now be mapped to
  `ClientError::MatrixApi`, containing both the original message and the
  associated error code and kind.

- `EventSendState` now has two additional variants: `CrossSigningNotSetup` and
  `SendingFromUnverifiedDevice`. These indicate that your own device is not
  properly cross-signed, which is a requirement when using the identity-based
  strategy, and can only be returned when using the identity-based strategy.

  In addition, the `VerifiedUserHasUnsignedDevice` and
  `VerifiedUserChangedIdentity` variants can be returned when using the
  identity-based strategy, in addition to when using the device-based strategy
  with `error_on_verified_user_problem` is set.

- `EventSendState` now has two additional variants:
  `VerifiedUserHasUnsignedDevice` and `VerifiedUserChangedIdentity`. These
  reflect problems with verified users in the room and as such can only be
  returned when the room key recipient strategy has
  `error_on_verified_user_problem` set.

- The `AuthenticationService` has been removed:
  - Instead of calling `configure_homeserver`, build your own client with the
    `serverNameOrHomeserverUrl` builder method to keep the same behaviour.
    - The parts of `AuthenticationError` related to discovery will be
      represented in the `ClientBuildError` returned
      when calling `build()`.
  - The remaining methods can be found on the built `Client`.
    - There is a new `abortOidcLogin` method that should be called if the
      webview is dismissed without a callback (
      or fails to present).
    - The rest of `AuthenticationError` is now found in the OidcError type.
- `OidcAuthenticationData` is now called `OidcAuthorizationData`.
- The `get_element_call_required_permissions` function now requires the
  device_id.

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

- `RoomPreview::own_membership_details` is now
  `RoomPreview::member_with_sender_info`, takes any user id and returns an
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
- Add `ClientBuilder::session_pool_max_size`, `::session_cache_size` and
  `::session_journal_size_limit` to control the stores configuration, especially
  their memory consumption
  ([#4870](https://github.com/matrix-org/matrix-rust-sdk/pull/4870/))
- Add `ClientBuilder::system_is_memory_constrained` to indicate that the system
  has less memory available than the current standard
  ([#4894](https://github.com/matrix-org/matrix-rust-sdk/pull/4894))
- Add `Room::member_with_sender_info` to get both a room member's info and for
  the user who sent the `m.room.member` event the `RoomMember` is based on.

[https-github-com-matrix-org-matrix-spec-proposals-pull-3814]: https://github.com/matrix-org/matrix-spec-proposals/pull/3814
