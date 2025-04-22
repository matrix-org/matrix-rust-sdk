# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

Additions:

- Add room topic string to `StateEventContent`

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

- `RoomPreview::own_membership_details` is now `RoomPreview::member_with_sender_info`, takes any user id and returns an `Option<RoomMemberWithSenderInfo>`.

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
- Add `ClientBuilder::session_pool_max_size`, `::session_cache_size` and `::session_journal_size_limit` to control the stores configuration, especially their memory consumption
  ([#4870](https://github.com/matrix-org/matrix-rust-sdk/pull/4870/))
- Add `ClientBuilder::system_is_memory_constrained` to indicate that the system
  has less memory available than the current standard
  ([#4894](https://github.com/matrix-org/matrix-rust-sdk/pull/4894))
- Add `Room::member_with_sender_info` to get both a room member's info and for the user who sent the `m.room.member` event the `RoomMember` is based on.
