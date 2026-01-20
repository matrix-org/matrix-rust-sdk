# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Bug Fixes

- Fix the `is_last_admin` check in `LeaveSpaceRoom` since it was not
  accounting for the membership state.
  [#6032](https://github.com/matrix-org/matrix-rust-sdk/pull/6032)
- [**breaking**] `LatestEventValue::Local { is_sending: bool }` is replaced
  by [`state: LatestEventValueLocalState`] to represent 3 states: `IsSending`,
  `HasBeenSent` and `CannotBeSent`.
  ([#5968](https://github.com/matrix-org/matrix-rust-sdk/pull/5968/))
- Fix the redecryption of events in timelines built using the
  `TimelineFocus` of `PinnedEvents`, `Thread`, `Event`.
  ([#5955](https://github.com/matrix-org/matrix-rust-sdk/pull/5955))

### Features

- [**breaking**] Extend `TimelineFocus::Event` to allow marking the target
  event as the root of a thread.
  ([#6050](https://github.com/matrix-org/matrix-rust-sdk/pull/6050))
- [**breaking**] Remove `TimelineEventTypeFilter` which has been replaced by
  the more generic `TimelineEventFilter`.
  ([#6070](https://github.com/matrix-org/matrix-rust-sdk/pull/6070/))
- Add `TimelineEventFilter` for filtering events based on their type or
  content. For content filtering, only membership and profile change filters
  are available as of now.
  ([#6048](https://github.com/matrix-org/matrix-rust-sdk/pull/6048/))
- Introduce `SpaceFilter`s as a mechanism for narrowing down what's displayed in
  the room list ([#6025](https://github.com/matrix-org/matrix-rust-sdk/pull/6025))
- Utilize the cache and include common relations when focusing a timeline on an event without
  requestion context.
  ([#5858](https://github.com/matrix-org/matrix-rust-sdk/pull/5858))
- [**breaking**] `EventTimelineItem::get_shield` now returns a new type,
  `TimelineEventShieldState`, which extends the old `ShieldState` with a code
  for `SentInClear`, now that the latter has been removed from `ShieldState`.
  ([#5959](https://github.com/matrix-org/matrix-rust-sdk/pull/5959))
- Add `SpaceService::get_space_room` to get a space
  given its id from the space graph if available.
  ([#5944](https://github.com/matrix-org/matrix-rust-sdk/pull/5944))
- [**breaking**]: The new Latest Event API replaces the old API. All the
  `new_` prefixes have been removed. The following methods are removed:
  `EventTimelineItem::from_latest_event`, and `Timeline::latest_event`. See the
  documentation of `matrix_sdk::latest_event` to learn about the new API.
  ([#5624](https://github.com/matrix-org/matrix-rust-sdk/pull/5624/))
- `Room::load_event_with_relations` now also calls `/relations` to fetch related events when falling back
  to network mode after a cache miss.
  ([#5930](https://github.com/matrix-org/matrix-rust-sdk/pull/5930))
- Expose `EventTimelineItem::forwarder` and `forwarder_profile`, which, if present, provide the ID and profile of
  the user who forwarded the keys used to decrypt the event as part of an [MSC4268](https://github.com/matrix-org/matrix-spec-proposals/pull/4268)
  key bundle.
  ([#6000](https://github.com/matrix-org/matrix-rust-sdk/pull/6000))
  
### Refactor

- [**breaking**] Refactored `is_last_admin` to `is_last_owner` the check will now
  account also for v12 rooms, where creators and users with PL 150 matter.
  ([#6036](https://github.com/matrix-org/matrix-rust-sdk/pull/6036))
- [**breaking**] The `SpaceService` will no longer auto-subscribe to required
  client events when invoking the `subscribe_to_joined_spaces` but instead do it
  through its, now async, constructor.
  ([#5972](https://github.com/matrix-org/matrix-rust-sdk/pull/5972))
- [**breaking**] The `SpaceService`'s `joined_spaces` method has been renamed
  `top_level_joined_spaces` and `subscribe_to_joined_spaces` to `space_service.subscribe_to_top_level_joined_spaces`
  ([#5972](https://github.com/matrix-org/matrix-rust-sdk/pull/5972))
- `RoomListService::subscribe_to_rooms` now forgets previous subscriptions.
  ([#6012](https://github.com/matrix-org/matrix-rust-sdk/pull/6012))

## [0.16.0] - 2025-12-04

### Features

- [**breaking**] `TimelineBuilder::track_read_marker_and_receipts` now takes a parameter to allow tracking to be enabled
  for all events (like before) or only for message-like events (which prevents read receipts from being placed on state
  events).
  ([#5900](https://github.com/matrix-org/matrix-rust-sdk/pull/5900))

## [0.15.0] - 2025-11-27

### Features

- Expose `is_space` in `NotificationItem`, allowing clients to determine if the room that triggered the notification is a space.
- [**breaking**] The `LatestEventValue::Local` type gains 2 new fields: `sender`
  and `profile`.
  ([#5885](https://github.com/matrix-org/matrix-rust-sdk/pull/5885))
- Add push actions to `NotificationItem`.
  ([#5835](https://github.com/matrix-org/matrix-rust-sdk/pull/5835))
- Add support for top level space ordering through [MSC3230](https://github.com/matrix-org/matrix-spec-proposals/pull/3230)
  and `m.space_order` room account data fields ([#5799](https://github.com/matrix-org/matrix-rust-sdk/pull/5799))

### Refactor

- `Timeline::latest_event` will return the latest event in the timeline, not the latest item of the timeline if it's
  an event.
- `TimelineFocusKind::Event` can now handle both the existing event pagination and thread pagination if the focused 
  event is part of a thread ([#5678](https://github.com/matrix-org/matrix-rust-sdk/pull/5678)).
- [**breaking**] The `Room` type in `room_list_service` is renamed to
  `RoomListItem`.
  ([#5684](https://github.com/matrix-org/matrix-rust-sdk/pull/5684))

### Bug Fixes

- `Timeline::latest_event_id` won't take threaded events into account on live/event focused timelines if `hide_threaded_events` is enabled. This fixes a bug in `Timeline::mark_as_read` that incorrectly tried to send a read receipt for threaded events that aren't really part of those timelines. ([#5864](https://github.com/matrix-org/matrix-rust-sdk/pull/5864/))
- Avoid replacing timeline items when the encryption info is unchanged.
  ([#5660](https://github.com/matrix-org/matrix-rust-sdk/pull/5660))
- Improvement performance of `RoomList` by introducing a new `RoomListItem` type
  (that replaces the `Room` type).
  ([#5684](https://github.com/matrix-org/matrix-rust-sdk/pull/5684))

## [0.14.0] - 2025-09-04

### Features
- Add a new [`SpaceService`] that provides high level reactive interfaces for listing 
  the user's joined top level spaces as long as their children.
  ([#5509](https://github.com/matrix-org/matrix-rust-sdk/pull/5509))
- Add `new_filter_low_priority` and `new_filter_non_low_priority` filters to the room list filtering system,
  allowing clients to filter rooms based on their low priority status. The filters use the `Room::is_low_priority()` 
  method which checks for the `m.lowpriority` room tag.
  ([#5508](https://github.com/matrix-org/matrix-rust-sdk/pull/5508))
- [**breaking**] Refactor the `non_space` filter into a `space` filter, favouring its use in combination with the
  `not` filter. ([#5508](https://github.com/matrix-org/matrix-rust-sdk/pull/5508))
- [**breaking**] Space rooms are now being retrieved through sliding sync and the newly introduced 
  [`room_list_service::filters::new_filter_non_space`] filter should be used to exclude them from any room list.
  ([5479](https://github.com/matrix-org/matrix-rust-sdk/pull/5479))
- [**breaking**] [`Timeline::send_gallery()`] now automatically fills in the thread relationship,
  based on the timeline focus. As a result, the `GalleryConfig::reply()` builder method has been
  replaced with `GalleryConfig::in_reply_to`, and only takes an optional event id (the event that is
  effectively replied to) instead of the `Reply` type. The proper way to start a thread with a
  gallery event is now thus to create a threaded-focused timeline, and then use
  `Timeline::send_gallery()`.
  ([5427](https://github.com/matrix-org/matrix-rust-sdk/pull/5427))
- [**breaking**] [`Timeline::send_attachment()`] now automatically fills in the thread
  relationship, based on the timeline focus. As a result, there's a new
  `matrix_sdk_ui::timeline::AttachmentConfig` type in town, that has a simplified optional parameter
  `replied_to` of type `OwnedEventId` instead of the `Reply` type and that must be used in place of
  `matrix_sdk::attachment::AttachmentConfig`. The proper way to start a thread with a media
  attachment is now thus to create a threaded-focused timeline, and then use
  `Timeline::send_attachment()`.
  ([5427](https://github.com/matrix-org/matrix-rust-sdk/pull/5427))
- [**breaking**] [`Timeline::send_reply()`] now automatically fills in the thread relationship,
  based on the timeline focus. As a result, it only takes an `OwnedEventId` parameter, instead of
  the `Reply` type. The proper way to start a thread is now thus to create a threaded-focused
  timeline, and then use `Timeline::send()`.
  ([5427](https://github.com/matrix-org/matrix-rust-sdk/pull/5427))
- `Timeline::send()` will now automatically fill the thread relationship, if the timeline has a
  thread focus, and the sent event doesn't have a prefilled `relates_to` field (i.e. a relationship).
  ([5427](https://github.com/matrix-org/matrix-rust-sdk/pull/5427))

### Refactor

- [**breaking**] The MSRV has been bumped to Rust 1.88.
  ([#5431](https://github.com/matrix-org/matrix-rust-sdk/pull/5431)) 

### Bug Fixes

- Correctly remove unable-to-decrypt items that have been decrypted but contain
  unsupported event types.
  ([#5463](https://github.com/matrix-org/matrix-rust-sdk/pull/5463))

## [0.13.0] - 2025-07-10

### Features

- Infer timeline read receipt threads for the `send_single_receipt` method from
  the focus mode and associated `hide_threaded_events` flag.
  ([5325](https://github.com/matrix-org/matrix-rust-sdk/pull/5325))
- Add `NotificationItem::room_topic` to the `NotificationItem` struct, which
  contains the topic of the room. This is useful for displaying the room topic
  in notifications.
  ([#5300](https://github.com/matrix-org/matrix-rust-sdk/pull/5300))
- Add `EmbeddedEvent::timestamp` and `EmbeddedEvent::identifier` which are already
  available in regular timeline items.
  ([#5331](https://github.com/matrix-org/matrix-rust-sdk/pull/5331))
- `RoomListService::subscribe_to_rooms` becomes `async` and automatically calls
  `matrix_sdk::latest_events::LatestEvents::listen_to_room`
  ([#5369](https://github.com/matrix-org/matrix-rust-sdk/pull/5369))

### Refactor

- [**breaking**] The function provided to `TimelineBuilder::event_filter()`
  must take `RoomVersionRules` as second argument instead of a `RoomVersionId`.
  The `default_event_filter()` reflects that change.
  ([#5337](https://github.com/matrix-org/matrix-rust-sdk/pull/5337))

## [0.12.0] - 2025-06-10

### Refactor

- [**breaking**] [`TimelineItemContent::reactions()`] returns an `Option<&ReactionsByKeyBySender>`
  instead of `ReactionsByKeyBySender`. This reflects the fact that some timeline items cannot hold
  reactions at all.
- `NotificationItem::room_join_rule` is now optional to reflect that the join rule
  state event might be missing, in which case it will be set to `None`. The
  `NotificationItem::is_public` field has been replaced with a method that returns an `Option<bool>`, based on the same logic.
  ([#5278](https://github.com/matrix-org/matrix-rust-sdk/pull/5278))

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
