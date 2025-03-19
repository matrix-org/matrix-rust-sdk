# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Features

- [**breaking**]: The `RoomPagination::run_backwards` method has been removed, and replaced by two
simpler methods:
  - `RoomPagination::run_backwards_until()`, which will retrigger back-paginations until a certain
  number of events have been received (and retry if the timeline has been reset in the background).
  - `RoomPagination::run_backwards_once()`, which will run a single back-pagination (and retry if
  the timeline has been reset in the background).
  ([#4689](https://github.com/matrix-org/matrix-rust-sdk/pull/4689))
- [**breaking**]: The `Oidc::account_management_url` method now caches the
  result of a call, subsequent calls to the method will not contact the OIDC
  provider for a while, instead the cached URI will be returned. If caching of
  this URI is not desirable, the `Oidc::fetch_account_management_url` method
  can be used.
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

### Bug Fixes

- Ensure all known secrets are removed from secret storage when invoking the
  `Recovery::disable()` method. While the server is not guaranteed to delete
  these secrets, making an attempt to remove them is considered good practice.
  Note that all secrets are uploaded to the server in an encrypted form.
  ([#4629](https://github.com/matrix-org/matrix-rust-sdk/pull/4629))

### Refactor

- [**breaking**] We now require Rust 1.85 as the minimum supported Rust version to compile.
  Yay for async closures!
  ([#4745](https://github.com/matrix-org/matrix-rust-sdk/pull/4745)
- [**breaking**]: The `Oidc` API only supports public clients, i.e. clients
  without a secret.
  ([#4634](https://github.com/matrix-org/matrix-rust-sdk/pull/4634))
  - `Oidc::restore_registered_client()` takes a `ClientId` instead of
    `ClientCredentials`
  - `Oidc::restore_registered_client()` must NOT be called after
    `Oidc::register_client()` anymore.
- [**breaking**]: The `authentication::qrcode` module now reexports types from
  `oauth2` rather than `openidconnect`. Some type names might have changed.
  ([#4604](https://github.com/matrix-org/matrix-rust-sdk/pull/4604))
- [**breaking**] `Oidc::authorize_scope()` was removed because it has no use
  case anymore, according to the latest version of
  [MSC2967](https://github.com/matrix-org/matrix-spec-proposals/pull/2967).
  ([#4664](https://github.com/matrix-org/matrix-rust-sdk/pull/4664))
- The `UserSession` type cannot be deserialized from its old format anymore. The
  old format used an `issuer_info` field instead of an `issuer` field.
- [**breaking**]: The `Oidc` API uses the `GET /auth_metadata` endpoint from the
  latest version of [MSC2965](https://github.com/matrix-org/matrix-spec-proposals/pull/2965)
  by default. The previous `GET /auth_issuer` endpoint is still supported as a
  fallback for now.
  ([#4673](https://github.com/matrix-org/matrix-rust-sdk/pull/4673))
  - It is not possible to provide a custom issuer anymore:
    `Oidc::given_provider_metadata()` was removed, and the parameter was removed
    from `Oidc::register_client()`.
  - `Oidc::fetch_authentication_issuer()` was removed. To check if the
    homeserver supports OAuth 2.0, use `Oidc::provider_metadata()`. To get the
    issuer, use `VerifiedProviderMetadata::issuer()`.
  - `Oidc::provider_metadata()` returns an `OAuthDiscoveryError`. It has a
    `NotSupported` variant and an `is_not_supported()` method to check if the
    error is due to the server not supporting OAuth 2.0.
  - `OidcError::MissingAuthenticationIssuer` was removed.
- [**breaking**]: The `authentication::qrcode` module was moved inside
  `authentication::oidc`, because it is only available through the `Oidc` API.
- [**breaking**]: The behavior of `Oidc::logout()` is now aligned with
  [MSC4254](https://github.com/matrix-org/matrix-spec-proposals/pull/4254)
  ([#4674](https://github.com/matrix-org/matrix-rust-sdk/pull/4674))
  - Support for [RP-Initiated Logout](https://openid.net/specs/openid-connect-rpinitiated-1_0.html)
    was removed, so it doesn't return an `OidcEndSessionUrlBuilder` anymore.
  - Only one request is made to revoke the access token, since the server is
    supposed to revoke both the access token and the associated refresh token
    when the request is made.
- [**breaking**]: Remove most of the parameter methods of
  `OidcAuthCodeUrlBuilder`, since they were parameters defined in OpenID
  Connect. Only the `prompt` and `user_id_hint` parameters are still supported.
  ([#4699](https://github.com/matrix-org/matrix-rust-sdk/pull/4699))
- [**breaking**]: Remove support for ID tokens in the `Oidc` API.
  ([#4726](https://github.com/matrix-org/matrix-rust-sdk/pull/4726))
  - The `latest_id_token` field of `OidcSessionTokens` was removed. (De)
    serialization of the type should be backwards-compatible.
  - `Oidc::restore_registered_client()` doesn't take a `VerifiedClientMetadata`
    anymore.
  - `Oidc::latest_id_token()` and `Oidc::client_metadata()` were removed.
- [**breaking**]: The `Oidc` API makes use of the oauth2 and ruma crates rather
  than mas-oidc-client.
  ([#4761](https://github.com/matrix-org/matrix-rust-sdk/pull/4761))
  ([#4789](https://github.com/matrix-org/matrix-rust-sdk/pull/4789))
  - `ClientId` is a different type reexported from the oauth2 crate.
  - The error types that were in the `oidc` module have been moved to the
    `oidc::error` module.
  - The `prompt` parameter of `Oidc::url_for_oidc()` is now optional, and
    `Prompt` is a different type reexported from Ruma, that only supports the
    `create` value.
  - The `device_id` parameter of `Oidc::login` is now an `Option<OwnedDeviceId>`.
  - The `state` field of `OidcAuthorizationData` and `AuthorizationCode`, and
    the parameter of the same name in `Oidc::abort_authorization()` now use
    `CsrfToken`.
  - The `error` field of `AuthorizationError` uses an error type from the oauth2
    crate rather than one from mas-oidc-client.
  - The `types` and `requests` modules are gone and the necessary types are
    exported from the `oidc` module or available from `ruma`.
  - `AccountManagementUrlFull` now takes an `OwnedDeviceId` when a device ID is
    required.
  - `(Verified)ProviderMetadata` was replaced by `AuthorizationServerMetadata`.
  - The `issuer` is now a `Url`.
  - `Oidc::register()` doesn't accept a software statement anymore.
  - `(Verified)ClientMetadata` was replaced by the opinionated `ClientMetadata`.
- [**breaking**]: `OidcSessionTokens` and `MatrixSessionTokens` have been merged
  into `SessionTokens`. Methods to get and watch session tokens are now
  available directly on `Client`.
  `(MatrixAuth/Oidc)::session_tokens_stream()`, can be replaced by
  `Client::subscribe_to_session_changes()` and then calling
  `Client::session_tokens()` on a `SessionChange::TokenRefreshed`.
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
