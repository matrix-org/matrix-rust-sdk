# unreleased

Breaking changes:

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

Additions:

- Add `Encryption::get_user_identity` which returns `UserIdentity`
- Add `ClientBuilder::room_key_recipient_strategy`
- Add `Room::send_raw`
- Expose `withdraw_verification` to `UserIdentity`
- Expose `report_room` to `Room`
- Add `RoomInfo::encryption_state`
  ([#4788](https://github.com/matrix-org/matrix-rust-sdk/pull/4788))
