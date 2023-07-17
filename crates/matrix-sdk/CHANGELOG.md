# unreleased

Breaking changes:

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

Bug fixes:

- `Client::rooms` now returns all rooms, even invited, as advertised.

Additions:

- Add `VerificationRequest::state` and `VerificationRequest::changes` to check
  and listen to changes in the state of the `VerificationRequest`. This removes
  the need to listen to individual matrix events once the `VerificationRequest`
  object has been acquired.
- The `Room` methods to retrieve state events can now return a sync or stripped event,
  so they can be used for invited rooms too.
- Add `Client::subscribe_to_room_updates` and `room::Common::subscribe_to_updates`
- Add `Client::rooms_filtered`
- Add methods on `Client` that can handle several authentication APIs.

# 0.6.2

- Fix the access token being printed in tracing span fields.

# 0.6.1

- Fixes a bug where the access token used for Matrix requests was added as a field to a tracing span.
