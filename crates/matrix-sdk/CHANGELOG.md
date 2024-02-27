# unreleased

Breaking changes:

- Replace the `Notification` type from Ruma in `SyncResponse` and `Client::register_notification_handler`
  by a custom one
- `Room::can_user_redact` and `Member::can_redact` are split between `*_redact_own` and `*_redact_other`
- The ambiguity maps in `SyncResponse` are moved to `JoinedRoom` and `LeftRoom`
- `AmbiguityCache` contains the room member's user ID
- Replace `impl MediaEventContent` with `&impl MediaEventContent` in
  `Media::get_file`/`Media::remove_file`/`Media::get_thumbnail`/`Media::remove_thumbnail`
- A custom sliding sync proxy set with `ClientBuilder::sliding_sync_proxy` now takes precedence over a discovered proxy.

Additions:

- Add the `ClientBuilder::add_root_certificates()` method which re-exposes the
  `reqwest::ClientBuilder::add_root_certificate()` functionality.
- Add `Room::get_user_power_level(user_id)` and `Room::get_suggested_user_role(user_id)` to be able to fetch power level info about an user without loading the room member list.

Additions:

- Add new method `discard_room_key` on `Room` that allows to discard the current
  outbound session for that room. Can be used by clients as a dev tool like the `/discardsession` command.

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
