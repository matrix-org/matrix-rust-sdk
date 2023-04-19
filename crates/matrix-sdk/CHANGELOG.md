# unreleased

- `Common::members` and `Common::members_no_sync` take a `RoomMemberships` to be able to filter the
  results by any membership state.

# 0.6.2

- Fix the access token being printed in tracing span fields.

# 0.6.1

- Fixes a bug where the access token used for Matrix requests was added as a field to a tracing span.
