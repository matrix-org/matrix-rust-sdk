# unreleased

Breaking changes:

- `Timeline::edit` now takes a `RoomMessageEventContentWithoutRelation`.
- `Timeline::send_attachment` now takes an `impl Into<PathBuf>` for the path of
  the file to send.

# 0.7.0

Initial release