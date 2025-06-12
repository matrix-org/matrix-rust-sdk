# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

## [0.12.0] - 2025-06-10

### Refactor

- [**breaking**] [`TimelineItemContent::reactions()`] returns an `Option<&ReactionsByKeyBySender>`
  instead of `ReactionsByKeyBySender`. This reflects the fact that some timeline items cannot hold
  reactions at all.

### Bug Fixes

- Introduce `Timeline` regions, which helps to remove a class of bugs in the
  `Timeline` where items could be inserted in the wrong _regions_, such as
  a remote timeline item before the `TimelineStart` virtual timeline item.
  ([#5000](https://github.com/matrix-org/matrix-rust-sdk/pull/5000))
- `NotificationClient` will filter out events sent by ignored users on `get_notification` and `get_notifications`. ([#5081](https://github.com/matrix-org/matrix-rust-sdk/pull/5081))

### Features

- `Timeline::send_single_receipt()` and `Timeline::send_multiple_receipts()` now also unset the
  unread flag of the room if an unthreaded read receipt is sent.
  ([#5055](https://github.com/matrix-org/matrix-rust-sdk/pull/5055))
- `Timeline::mark_as_read()` unsets the unread flag of the room if it was set.
  ([#5055](https://github.com/matrix-org/matrix-rust-sdk/pull/5055))
- Add new method `Timeline::send_gallery` to allow sending MSC4274-style
  galleries.
  ([#5125](https://github.com/matrix-org/matrix-rust-sdk/pull/5125))

## [0.11.0] - 2025-04-11

### Bug Fixes

### Features

- [**breaking**] Optionally allow starting threads with `Timeline::send_reply`.
  ([#4819](https://github.com/matrix-org/matrix-rust-sdk/pull/4819))
- [**breaking**] Push `RepliedToInfo`, `ReplyContent`, `EnforceThread` and
  `UnsupportedReplyItem` (becoming `ReplyError`) down into matrix_sdk.
  [`Timeline::send_reply()`] now takes an event ID rather than a `RepliedToInfo`.
  `Timeline::replied_to_info_from_event_id` has been made private in `matrix_sdk`.
  ([#4842](https://github.com/matrix-org/matrix-rust-sdk/pull/4842))
- Allow sending media as (thread) replies. The reply behaviour can be configured
  through new fields on [`AttachmentConfig`].
  ([#4852](https://github.com/matrix-org/matrix-rust-sdk/pull/4852))

### Refactor

- [**breaking**] Reactions on a given timeline item have been moved from
  [`EventTimelineItem::reactions()`] to [`TimelineItemContent::reactions()`]; they're thus available
  from an [`EventTimelineItem`] by calling `.content().reactions()`. They're also returned by
  ownership (cloned) instead of by reference.
  ([#4576](https://github.com/matrix-org/matrix-rust-sdk/pull/4576))
- [**breaking**] The parameters `event_id` and `enforce_thread` on [`Timeline::send_reply()`]
  have been wrapped in a `reply` struct parameter.
  ([#4880](https://github.com/matrix-org/matrix-rust-sdk/pull/4880/))

## [0.10.0] - 2025-02-04

### Bug Fixes

- Don't consider rooms in the banned state to be non-left rooms. This bug was
  introduced due to the introduction of the banned state for rooms, and the
  non-left room filter did not take the new room stat into account.
  ([#4448](https://github.com/matrix-org/matrix-rust-sdk/pull/4448))

- Fix `EventTimelineItem::latest_edit_json()` when it is populated by a live
  edit. ([#4552](https://github.com/matrix-org/matrix-rust-sdk/pull/4552))

- Fix our own explicit read receipt being ignored when loading it from the
  state store, which resulted in our own read receipt being wrong sometimes.
  ([#4600](https://github.com/matrix-org/matrix-rust-sdk/pull/4600))

### Features

- [**breaking**] `Timeline::send_attachment()` now takes a type that implements
  `Into<AttachmentSource>` instead of a type that implements `Into<PathBuf>`.
  `AttachmentSource` allows to send an attachment either from a file, or with
  the bytes and the filename of the attachment. Note that all types that
  implement `Into<PathBuf>` also implement `Into<AttachmentSource>`.
  ([#4451](https://github.com/matrix-org/matrix-rust-sdk/pull/4451))

- [**breaking**] Add an "offline" mode to the `SyncService`. This allows the
  `SyncService` to attempt to restart the sync automatically. It can be enabled
  with the `SyncServiceBuilder::with_offline_mode` method. Due to this addition,
  the `SyncService::stop` method has been made infallible.
  ([#4592](https://github.com/matrix-org/matrix-rust-sdk/pull/4592))

### Refactor

- Drastically improve the performance of the `Timeline` when it receives
  hundreds and hundreds of events (approximately 10 times faster).
  ([#4601](https://github.com/matrix-org/matrix-rust-sdk/pull/4601),
  [#4608](https://github.com/matrix-org/matrix-rust-sdk/pull/4608),
  [#4612](https://github.com/matrix-org/matrix-rust-sdk/pull/4612))

- [**breaking**] `Timeline::paginate_forwards` and `Timeline::paginate_backwards`
  are unified to work on a live or focused timeline.
  `Timeline::live_paginate_*` and `Timeline::focused_paginate_*` have been
  removed ([#4584](https://github.com/matrix-org/matrix-rust-sdk/pull/4584)).

- [**breaking**] `Timeline::subscribe_batched` replaces
  `Timeline::subscribe`. `subscribe` has been removed in
  [#4567](https://github.com/matrix-org/matrix-rust-sdk/pull/4567),
  and `subscribe_batched` has been renamed to `subscribe` in
  [#4585](https://github.com/matrix-org/matrix-rust-sdk/pull/4585).

## [0.9.0] - 2024-12-18

### Bug Fixes

- Add the `m.room.create` and the `m.room.history_visibility` state events to
  the required state for the sync. These two state events are required to
  properly compute the room preview of a joined room.
  ([#4325](https://github.com/matrix-org/matrix-rust-sdk/pull/4325))

### Features

- Introduce a new variant to the `UtdCause` enum tailored for device-historical
  messages. These messages cannot be decrypted unless the client regains access
  to message history through key storage (e.g., room key backups).
  ([#4375](https://github.com/matrix-org/matrix-rust-sdk/pull/4375))

## [0.8.0] - 2024-11-19

### Bug Fixes

- Disable `share_pos()` inside `RoomListService`.

- `UtdHookManager` no longer re-reports UTD events as late decryptions.
  ([#3480](https://github.com/matrix-org/matrix-rust-sdk/pull/3480))

- Messages that we were unable to decrypt no longer display a red padlock.
  ([#3956](https://github.com/matrix-org/matrix-rust-sdk/issues/3956))

- `UtdHookManager` no longer reports UTD events that were already reported in a
  previous session.
  ([#3519](https://github.com/matrix-org/matrix-rust-sdk/pull/3519))

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


# 0.7.0

Initial release
