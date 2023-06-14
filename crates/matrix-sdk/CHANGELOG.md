# unreleased

- Add `VerificationRequest::state` and `VerificationRequest::changes` to check
  and listen to changes in the state of the `VerificationRequest`. This removes
  the need to listen to individual matrix events once the `VerificationRequest`
  object has been acquired.

- `Common::members` and `Common::members_no_sync` take a `RoomMemberships` to be able to filter the
  results by any membership state.
  - `Common::active_members(_no_sync)` and `Common::joined_members(_no_sync)` are deprecated.
- `matrix-sdk-sqlite` is the new default store implementation outside of WASM, behind the `sqlite` feature.
  - The `sled` feature was removed. The `matrix-sdk-sled` crate is deprecated and no longer maintained.
- The `Common` methods to retrieve state events can now return a sync or stripped event, so it can be used
  for invited rooms too.
- Add `Client::subscribe_to_room_updates` and `room::Common::subscribe_to_updates`
- `Client::rooms` now returns all rooms, even invited, as advertised.
- Add `Client::rooms_filtered`

# 0.6.2

- Fix the access token being printed in tracing span fields.

# 0.6.1

- Fixes a bug where the access token used for Matrix requests was added as a field to a tracing span.
