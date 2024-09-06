# unreleased

Breaking changes:

- `Timeline::edit` now takes a `RoomMessageEventContentWithoutRelation`.
- `Timeline::send_attachment` now takes an `impl Into<PathBuf>` for the path of
  the file to send.
- `Timeline::item_by_transaction_id` has been renamed to `Timeline::local_item_by_transaction_id`
(always returns local echoes).

Bug fixes:

- `UtdHookManager` no longer re-reports UTD events as late decryptions.
  ([#3480](https://github.com/matrix-org/matrix-rust-sdk/pull/3480))
- Messages that we were unable to decrypt no longer display a red padlock.
  ([#3956](https://github.com/matrix-org/matrix-rust-sdk/issues/3956))

Other changes:

- `UtdHookManager` no longer reports UTD events that were already reported in a
  previous session.
  ([#3519](https://github.com/matrix-org/matrix-rust-sdk/pull/3519))


# 0.7.0

Initial release
