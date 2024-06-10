# unreleased

Breaking changes:

- `Timeline::edit` now takes a `RoomMessageEventContentWithoutRelation`.
- `Timeline::send_attachment` now takes an `impl Into<PathBuf>` for the path of
  the file to send.

Bug fixes:

- `UtdHookManager` no longer re-reports UTD events as late decryptions.
  ([#3480](https://github.com/matrix-org/matrix-rust-sdk/pull/3480))

Other changes:

- `UtdHookManager` no longer reports UTD events that were already reported in a
  previous session.
  ([#3519](https://github.com/matrix-org/matrix-rust-sdk/pull/3519))


# 0.7.0

Initial release
