# unreleased

Breaking changes:

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

Additions:

- Add `ClientBuilder::room_key_recipient_strategy`
