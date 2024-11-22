# Changelog

All notable changes to this project will be documented in this file.

## [matrix-sdk-ui-0.8.0] - 2024-11-19

### Bug Fixes

- Disable `share_pos()` inside `RoomListService`.

- `UtdHookManager` no longer re-reports UTD events as late decryptions.
  ([#3480](https://github.com/matrix-org/matrix-rust-sdk/pull/3480))

- Messages that we were unable to decrypt no longer display a red padlock.
  ([#3956](https://github.com/matrix-org/matrix-rust-sdk/issues/3956))

- `UtdHookManager` no longer reports UTD events that were already reported in a
  previous session.
  ([#3519](https://github.com/matrix-org/matrix-rust-sdk/pull/3519))

- Implement proper redact handling in the widget driver.
 This allows the Rust SDK widget driver to support widgets that
 rely on redacting.

- Add `reason` field to `TimelineItemContent::RoomMembership`

- Add `m.room.join_rules` to the required state

- `EncryptionSyncService` and `Notification` are using
  `Client::cross_process_store_locks_holder_name`.

- Add `ClientBuilder::cross_process_store_locks_holder_name`.

- Implement unwedging for media uploads

- For `Timeline::send_*` fns, treat the passed `caption` parameter as markdown
  and use the HTML generated from it as the `formatted_caption` if there is
  none.

- Send state from state sync and not from timeline to widget
  ([#4254](https://github.com/matrix-org/matrix-rust-sdk/pull/4254))

- Allow aborting media uploads

- Add `is_direct` and `fn inviter` to `RoomPreview`

- Add `RoomPreviewInfo::num_active_members`

- Make `RoomPreviewInfo::room_type` an enum, not an optional String

- Add support for including captions with file uploads.

- Check if the user is allowed to do a room mention before trying to send a call
  notify event.
  ([#4271](https://github.com/matrix-org/matrix-rust-sdk/pull/4271))

### Documentation

- Start an architecture document with a high-level description of the crates

- Improve documentation of `Client::observe_events`.


### Features

- Add `m.room.join_rules` to the required state.

- `EncryptionSyncService` and `Notification` are using
  `Client::cross_process_store_locks_holder_name`.


### Refactor

- [**breaking**] `Timeline::edit` now takes a `RoomMessageEventContentWithoutRelation`.

- [**breaking**] `Timeline::send_attachment` now takes an `impl Into<PathBuf>`
  for the path of the file to send.

- [**breaking**] `Timeline::item_by_transaction_id` has been renamed to
  `Timeline::local_item_by_transaction_id` (always returns local echoes).

- Remove duplicated fields in media event contents

- Use `SendHandle` for media uploads too

- Move `event_cache_store/` to `event_cache/store/` in `matrix-sdk-base`.

- Move `formatted_caption_from` to the SDK, rename it

- Tidy up and start commenting the widget code

- Get rid of `ProcessingContext` and inline it in its callers

- Get rid of unused `limits` parameter when constructing a `WidgetMachine`

- Use a specialized mutex for locking access to the state store and `being_sent`


# 0.7.0

Initial release
