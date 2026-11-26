# Changelog

All notable changes to this project will be documented in this file.

<!-- changelog start -->

## [0.20.0](https://github.com/matrix-org/matrix-rust-sdk/tree/0.20.0) - 2026-11-25

### Added

- `RoomListService` enables the sticky events sliding sync extension
  ([MSC4480](https://github.com/matrix-org/matrix-spec-proposals/pull/4480))
  when built with the `unstable-msc4354` feature.
  ([#7014](https://github.com/matrix-org/matrix-rust-sdk/pull/7014))
- The `timeline::TimelineFocusKind` enum gains a new method: `room_id`. It is a
  small thing, but it can be useful in some cases!
  ([#7034](https://github.com/matrix-org/matrix-rust-sdk/pull/7034))
- Add `Timeline::retry_send()` and `Timeline::abort_send()`, with `SendTarget`,
  to retry or abort a pending edit, redaction, or reaction on an item, or the
  item itself while it's a local echo. Aborting the last pending edit puts the
  message, or the poll's question, back to what it was before. Both return
  whether there was anything of that kind left to act on.
  ([#7039](https://github.com/matrix-org/matrix-rust-sdk/pull/7039))
- Add `EncryptedMessage::utd_cause()`, which returns the `UtdCause` for
  un-decryptable events received via a Megolm session, so clients can tell users
  why a message could not be decrypted. It returns `None` for events received
  via Olm or an unknown algorithm.
  ([#7122](https://github.com/matrix-org/matrix-rust-sdk/pull/7122))
- Add `Timeline::edit_with_attachment()`, which replaces the attachment of a
  message the current user sent, or adds one to a message which had none,
  through the send queue.
  ([#7127](https://github.com/matrix-org/matrix-rust-sdk/pull/7127))
- Add custom gallery and item fields to `Timeline::send_gallery`.
  ([#7154](https://github.com/matrix-org/matrix-rust-sdk/pull/7154))
- Add `EventTimelineItem::redaction_reason()`, the reason given with the event's
  redaction: from the server's `redacted_because`, or from our own redaction
  while it is still pending.
  ([#7357](https://github.com/matrix-org/matrix-rust-sdk/pull/7357))
- Add `Timeline::fully_read_event_id` and
  `Timeline::subscribe_fully_read_event_id` to get and observe the ID of the
  closest visible event at or before the user's `m.fully_read` marker, which may
  point to an event that isn't rendered (e.g. a reaction or an edit). If the
  marker's event isn't loaded, or no visible event before it is, this is the
  marker's own event ID.
  ([#7360](https://github.com/matrix-org/matrix-rust-sdk/pull/7360))
- Add `image_packs::image_packs()`, which returns the
  [image packs](https://spec.matrix.org/v1.19/client-server-api/#image-packs)
  that can be used in a room: the packs enabled globally through
  `m.image_pack.rooms`, followed by the room's own packs and the packs of the
  joined spaces it belongs to.
  ([#7497](https://github.com/matrix-org/matrix-rust-sdk/pull/7497))
- Add `SpaceService::joined_ancestors_of()`, which returns all the joined spaces
  a room or space descends from.
  ([#7497](https://github.com/matrix-org/matrix-rust-sdk/pull/7497))
- Add `NotificationItem::can_send_message`, telling whether the current user is
  allowed to send an `m.room.message` event in the room of the notification,
  according to the room's power levels.
  ([#7544](https://github.com/matrix-org/matrix-rust-sdk/pull/7544))
- Add `ImagePack::is_enabled_globally`, telling whether the user enabled the
  pack in all the rooms through their `m.image_pack.rooms` account data.
  ([#7554](https://github.com/matrix-org/matrix-rust-sdk/pull/7554))

### Changed

- In a `Timeline`, a `MsgLikeContent::thread_summary` is **no longer** set to
  `None` if all in-thread events are redacted. Instead,
  `ThreadSummary::num_replies` becomes `0` and `ThreadSummary::latest_reply`
  becomes `None`. A `MsgLikeContent::thread_summary` is `None` if and only if
  the event is **not** a thread root.
  ([#7034](https://github.com/matrix-org/matrix-rust-sdk/pull/7034))
- [**breaking**] Reactions move from `TimelineItemContent::reactions()` back to
  `EventTimelineItem::reactions()`, returning a reference instead of an
  `Option`. Only the `MsgLike` variant could hold them, so an annotation on a
  membership change, a profile change, another state event, a call invite or an
  unparsable event was dropped. MSC2677 makes any event type eligible for an
  annotation. ([#7049](https://github.com/matrix-org/matrix-rust-sdk/pull/7049))
- [**breaking**] The `votes` field from `PollResult` is now a map of `String` to
  `Vec<OwnedUserId>` rather than `Vec<String>`. The strings were already Matrix
  user IDs but this change makes it obvious.
  ([#7161](https://github.com/matrix-org/matrix-rust-sdk/pull/7161))
- Rooms without a latest event are no longer sorted after every room having one
  in the room list. They get a timestamp back-filled from the rooms around them
  by recency stamp. Rooms with the same timestamp are ordered by their
  recency stamp.
  ([#7377](https://github.com/matrix-org/matrix-rust-sdk/pull/7377))
- Room info notable updates are now handled in batches by the room list. All the
  updates already waiting are processed at once, at most once per room. A
  burst of updates reorders the list in one go instead of room by room.
  ([#7379](https://github.com/matrix-org/matrix-rust-sdk/pull/7379))
- The `RoomListService` now requests the `m.room.image_pack` state events of all
  rooms, so that the image packs of a room and of its spaces are available
  locally. ([#7497](https://github.com/matrix-org/matrix-rust-sdk/pull/7497))
- Migrated from the unmaintained chrono crate to jiff. This is an internal
  change only.
  ([#7525](https://github.com/matrix-org/matrix-rust-sdk/pull/7525))

### Fixed

- A pending edit now follows the send queue's updates to its content, so an edit
  replacing an attachment points at the uploaded media as soon as the upload is
  done, instead of a local cache entry that no longer exists.
  ([#6921](https://github.com/matrix-org/matrix-rust-sdk/pull/6921))
- A redacted timeline item keeps the redacted event's raw JSON again, so the
  redaction's `unsigned.redacted_because` (and its reason) is available on a
  live timeline, not only on one rebuilt from the cache.
  ([#7213](https://github.com/matrix-org/matrix-rust-sdk/pull/7213))
- Reply previews of a redacted message keep the redacted message's own sender
  and timestamp instead of showing the redactor as its author.
  ([#7234](https://github.com/matrix-org/matrix-rust-sdk/pull/7234))
- When a user sends the same reaction from two devices, the timeline keeps
  showing it even if the local copy fails or is aborted. Before, it disappeared
  or showed as failed.
  ([#7236](https://github.com/matrix-org/matrix-rust-sdk/pull/7236))
- Fix a race in `Timeline` creation where events received while the timeline was
  being built could be added twice, resulting in duplicated items.
  ([#7284](https://github.com/matrix-org/matrix-rust-sdk/pull/7284))
- The timeline's read marker is now displayed when the `m.fully_read` event
  isn't rendered (e.g. a reaction or an edit), after the closest previous
  visible event.
  ([#7355](https://github.com/matrix-org/matrix-rust-sdk/pull/7355))
- Fix a bug where the `Timeline` was cleared but was containing at least a local
  event and at least a remote event not mapped to any timeline item (like a
  reaction). The mapping between timeline items and remote events could become
  broken. This is fixed by introducing a new
  `ObservableItemsTransaction::clear_remotes_only` method to clean up the code
  in `TimelineStateTransaction::clear` and to address the problem correctly.
  ([#7372](https://github.com/matrix-org/matrix-rust-sdk/pull/7372))
- The room list no longer ignores room info updates that only change the bump
  stamp, so a room without a latest event now moves when its bump stamp changes.
  ([#7379](https://github.com/matrix-org/matrix-rust-sdk/pull/7379))
- Fix a bug where `SyncService` enters offline mode when an access token is
  revoked, instead of terminating with an error.
  ([#7425](https://github.com/matrix-org/matrix-rust-sdk/pull/7425))
- In offline mode, `SyncService` now waits 1 second instead of 100 milliseconds
  between `/versions` requests when the server returns an error that isn't
  retried with exponential backoff. This avoids hammering the homeserver while
  waiting to come back online.
  ([#7484](https://github.com/matrix-org/matrix-rust-sdk/pull/7484))

## [0.19.1](https://github.com/matrix-org/matrix-rust-sdk/tree/0.19.1) - 2026-09-18

No significant changes.

## [0.19.0](https://github.com/matrix-org/matrix-rust-sdk/tree/0.19.0) - 2026-09-16

### Removed

- [**breaking**] `EncryptionSyncService::run_fixed_iterations` has been replaced
  by `EncryptionSyncService::run_iterations`, which returns a stream yielding
  once per iteration; take as many items from it as iterations you want to run.
  ([#6982](https://github.com/matrix-org/matrix-rust-sdk/pull/6982))

### Added

- Added a new `edit_revisions` method on `Timeline` that returns the edit
  history of an event.
  ([#6630](https://github.com/matrix-org/matrix-rust-sdk/pull/6630))
- Make `matrix-sdk-ui` sliding sync services use the client-owned sync presence
  value for generated requests, so background sync flows can avoid marking the
  user online without UI-service-specific presence APIs.
  ([#6672](https://github.com/matrix-org/matrix-rust-sdk/pull/6672))
- Add a reactive `search::SearchService` that aggregates results of different
  kinds (currently only messages) into a single list of typed `SearchResult`s,
  observed as a stream of `VectorDiff`s. Gated behind the new
  `experimental-search` feature.
  ([#6695](https://github.com/matrix-org/matrix-rust-sdk/pull/6695))
- Add `status` and `call` fields to the timeline `Profile`, carrying the
  sender's
  [MSC4426](https://github.com/matrix-org/matrix-spec-proposals/pull/4426) user
  status and call indicator from their global profile.
  ([#6704](https://github.com/matrix-org/matrix-rust-sdk/pull/6704))
- Add `SyncServiceBuilder::with_profiles_extension` to enable the Profiles
  sliding sync extension, which syncs global profile fields such as `m.status`
  and `m.call`.
  ([#6726](https://github.com/matrix-org/matrix-rust-sdk/pull/6726))
- The timeline's `AttachmentConfig` gained an `extra_content` field, and the
  timeline a `send_with_extra_content()` method, forwarded to the underlying
  send queue. Extra fields never override the fields of the event itself.
  ([#6812](https://github.com/matrix-org/matrix-rust-sdk/pull/6812))
- The timeline gained a `toggle_reaction_with_extra_content()` method,
  forwarding extra top-level content fields to the underlying send queue when a
  reaction is added. Extra fields never override the fields of the event itself.
  ([#6848](https://github.com/matrix-org/matrix-rust-sdk/pull/6848))
- Add `Timeline::send_location`, moved from the FFI bindings. It builds a
  `m.location` room message, sends it through the send queue, and returns the
  `SendHandle`. It optionally sends the location as a reply, with the same
  semantics as `Timeline::send_reply`.
  ([#6891](https://github.com/matrix-org/matrix-rust-sdk/pull/6891))
- Add `RoomListService::remove_room_subscriptions`, which removes the
  subscriptions of the given rooms, and
  `RoomListService::reset_and_add_room_subscriptions`, which replaces the whole
  subscription set and marks the members of the new rooms as missing. Both
  forward to the room subscription methods of `SlidingSync` with the room list
  settings. ([#6932](https://github.com/matrix-org/matrix-rust-sdk/pull/6932))
- `SpaceService` has two new cheap accessor functions for asking about the
  ancestors of a given room or space:
  - `joined_parent_ids_of_child()`: returns the room IDs of a room or space's
  direct parent spaces that are known, without recomputing the space graph or
  building any expensive `SpaceRoom` instances.
  - `top_level_ancestors_of()`: returns the IDs of the top-level joined space(s)
  a room descends from.
- [**breaking**] Allow specifying membership states for
  `TimelineEventCondition::MembershipChange`, with a new
  `MembershipChangeFilter` enum.
  ([#6644](https://github.com/matrix-org/matrix-rust-sdk/pull/6644))
- The `TimelineItemContent::RtcNotification` now contains an optional
  `ActiveCallInfo` struct, which provides information about the active call
  associated with the notification. This allows to render in the timeline more
  detailed context about the active call (members, join state, etc...)
  ([#6668](https://github.com/matrix-org/matrix-rust-sdk/pull/6668))

  ([#6967](https://github.com/matrix-org/matrix-rust-sdk/pull/6967))
- Allow filtering out or only including custom message-like and state events
  using `TimelineEventCondition::AnyCustomMessageLikeEvent` and
  `TimelineEventCondition::AnyCustomStateEvent`.
  ([#6969](https://github.com/matrix-org/matrix-rust-sdk/pull/6969))
- Add `EventTimelineItem::edit_send_state()` and
  `EventTimelineItem::redaction_send_state()`, exposing the send state of our
  own pending edits and redactions.
  ([#6974](https://github.com/matrix-org/matrix-rust-sdk/pull/6974))
- Add `NotificationClientTimeouts` and `NotificationClient::with_timeouts` to
  make the timeouts applied while fetching a notification configurable. The
  defaults are unchanged.
  ([#7023](https://github.com/matrix-org/matrix-rust-sdk/pull/7023))

### Changed

- [**breaking**] Send redactions issued via `Timeline::redact` through the send
  queue. ([#6428](https://github.com/matrix-org/matrix-rust-sdk/pull/6428))
- `RoomListService::set_room_subscriptions` now updates subscriptions
  non-destructively.
  ([#6652](https://github.com/matrix-org/matrix-rust-sdk/pull/6652))
- [**breaking**] `SpaceRoom::heroes` is now `Option<Vec<RoomHeroWithProfile>>`
  instead of `Option<Vec<RoomHero>>`, exposing each hero's status and call
  fields from their global profile. These fields are only populated when syncing
  via sliding sync with the profiles extension enabled.
  ([#6733](https://github.com/matrix-org/matrix-rust-sdk/pull/6733))
- [**breaking**] `Timeline::send_reply` now returns the `SendHandle` of the
  queued reply, instead of `()`. Replies go through the send queue like any
  other message, so callers can now track, abort, or retry them, as they already
  can with `Timeline::send`.
  ([#6881](https://github.com/matrix-org/matrix-rust-sdk/pull/6881))
- `RoomListService::subscribe_to_rooms` is renamed to
  `RoomListService::set_room_subscriptions`, to match the room subscription
  methods of `SlidingSync`.
  ([#6927](https://github.com/matrix-org/matrix-rust-sdk/pull/6927))
- This patch changes the `room_list_service::filters::unread` filter to
  `read_receipts` and adds a new parameter: `ReadReceiptsCategory`. Before it
  was looking at the `ReadReceipts::num_notifications` field only, now it can
  look at the following field: `num_mentions`, `num_notifications` or
  `num_messages`.

  The condition where `Room::is_marked_unread` makes the room to be selected if
  there is no unread is kept because (i) it's a manual operation from the user,
  (ii) it signals the room is unread but for an unknown reason, it could be
  anything, so it's important and should be displayed regardless of the number
  of unread.

  Before:

  ```rust
  filters::unread()
  ```

  After:

  ```rust
  filters::read_receipts(filters::ReadReceiptsCategory::Notifications)
  ``` ([#6928](https://github.com/matrix-org/matrix-rust-sdk/pull/6928))
- `Timeline::redact()` now forwards its `reason` when the target is a local
  echo, instead of silently dropping it: if the echo was being sent and the send
  completes, the server-side redaction carries the reason.
  ([#6931](https://github.com/matrix-org/matrix-rust-sdk/pull/6931))
- The `ThreadSummary::public_read_receipt_event_id` and
  `ThreadSummary::private_read_receipt_event_id` fields have been removed. They
  were a hack introduced in the past and no longer make sense. Use `ThreadInfo`
  if you need information about the read receipts, with
  `matrix_sdk::event_cache::ThreadEventCache::read_receipts()`.
  ([#6938](https://github.com/matrix-org/matrix-rust-sdk/pull/6938))
- [**breaking**] `ReactionStatus` is gone: `ReactionInfo::status` is replaced by
  `ReactionInfo::send_state`, an `Option<EventSendState>` that is `None` for
  reactions received from the server. Every aggregation (reactions, edits,
  redactions) now carries the same send state, and failed, retried and uploading
  aggregations are reflected on their timeline item instead of being ignored.
  ([#6974](https://github.com/matrix-org/matrix-rust-sdk/pull/6974))
- When an event in a notification can't be decrypted, the encryption sync run to
  obtain the missing room key now attempts decryption after every iteration and
  stops as soon as it succeeds, instead of always running two iterations before
  trying once. Two iterations are still run at least, and further ones only
  while a deadline of two poll timeouts has not passed, so the time spent when
  the key never arrives is unchanged.
  ([#6982](https://github.com/matrix-org/matrix-rust-sdk/pull/6982))
- [**breaking**] The Profiles sliding sync extension (MSC4262) is now always
  enabled like the other extensions.
  `SyncServiceBuilder::with_profiles_extension` has been removed.
  ([#6984](https://github.com/matrix-org/matrix-rust-sdk/pull/6984))
- Expose `TimelineEventFilter` and `TimelineEventCondition` in the FFI layer
  when the `uniffi` feature is enabled.
  ([#6985](https://github.com/matrix-org/matrix-rust-sdk/pull/6985))
- Opening a thread timeline whose events are already in the event cache now
  looks up the related events of all thread events concurrently, instead of
  awaiting one event cache query per thread event. The related events are
  collected in the same order as before, so the timeline items are unchanged.
  ([#6994](https://github.com/matrix-org/matrix-rust-sdk/pull/6994))

### Fixed

- Only report duplicate read receipts within the same receipt thread.
  ([#6637](https://github.com/matrix-org/matrix-rust-sdk/pull/6637))
- `Timeline::mark_as_read` and `Timeline::send_single_receipt` now decide
  whether to send a read receipt based only on the real receipts the homeserver
  knows about, ignoring local-only implicit receipts. Both also avoid pointing a
  receipt at one of the current user's own events, moving it to the latest event
  from another user instead. Previously a receipt could be left behind unread
  messages when the user's own messages were the most recent, leaving the
  homeserver's push/badge count stuck.
  ([#6837](https://github.com/matrix-org/matrix-rust-sdk/pull/6837))
- Joined child spaces whose parent space the user has left now appear at the top
  level if they have no other joined parent spaces.
  ([#6858](https://github.com/matrix-org/matrix-rust-sdk/pull/6858))
- Fixed redacted messages not having thread summaries.
  ([#6897](https://github.com/matrix-org/matrix-rust-sdk/pull/6897))
- Show the UTD error for events from `TimelineItemContent::from_event()` instead
  of the generic "Unsupported event".
  ([#6898](https://github.com/matrix-org/matrix-rust-sdk/pull/6898))
- `Timeline::edit()` now keeps a local echo's thread or reply relation when
  replacing its content. The edited content is relation-free by type, and
  replacing the pending event's content wholesale used to detach it: editing a
  not-yet-sent thread reply made it go out as a top-level message.
  ([#6936](https://github.com/matrix-org/matrix-rust-sdk/pull/6936))
- When the Timeline received an update from the Event Cache about an event
  removal, if the Timeline contained a local timeline item, it could have
  resulted in a broken mapping between the timeline items and the remote events.
  The consequences to that are numerous, but for instance, some timeline items
  could have been wrongly removed, or not removed when expected.
  ([#6983](https://github.com/matrix-org/matrix-rust-sdk/pull/6983))
- Keep holding the permit while the encryption sync stream is running.
  ([#7018](https://github.com/matrix-org/matrix-rust-sdk/pull/7018))

## [0.18.0](https://github.com/matrix-org/matrix-rust-sdk/tree/0.18.0) - 2026-06-02

### Changed

- [**breaking**] `SpaceRoom::new_from_known` and `SpaceRoom::new_from_summary`
  are now asynchronous so we can properly check if they are DMs on demand
  instead of trusting the pre-computed value. Some other related functions are
  now `async` too.
  ([#6561](https://github.com/matrix-org/matrix-rust-sdk/pull/6561))

### Fixed

- Remove the ability to reply to live location events.
  ([#6563](https://github.com/matrix-org/matrix-rust-sdk/pull/6563))

## [0.17.0] - 2026-05-08

### Security fixes

- Reject invalid edits as candidates for timeline updates.
  ([#6454](https://github.com/matrix-org/matrix-rust-sdk/pull/6454), Moderate,
  [CVE-2026-45057](https://www.cve.org/CVERecord?id=CVE-2026-45057),
  [GHSA-h97m-27fx-42rx](https://github.com/matrix-org/matrix-rust-sdk/security/advisories/GHSA-h97m-27fx-42rx))

### Bug fixes

- Fix a possible panic in `RoomList::entries_with_dynamic_adapters`.
  ([#6459](https://github.com/matrix-org/matrix-rust-sdk/pull/6459))
- Keep stopped `beacon_info` live location sessions visible in
  `Room::latest_event()`, so room summaries still show the last live location
  sharing session after it ends.
  ([#6437](https://github.com/matrix-org/matrix-rust-sdk/pull/6437))
- Allow setting a custom Sliding Sync connection ID and timeline limit on
  `RoomListService`.
  ([#6289](https://github.com/matrix-org/matrix-rust-sdk/pull/6289))
- Don't show a "sent in clear" shield on live location timeline items in
  encrypted rooms, since `beacon_info` is a state event that cannot be
  encrypted by design.
  ([#6308](https://github.com/matrix-org/matrix-rust-sdk/pull/6308))
- Include secondary relations when re-initializing a threaded timeline after a
  lag. ([#6209](https://github.com/matrix-org/matrix-rust-sdk/pull/6209))
- Ensure that the display name of a `Room` in a `NotificationStatus` coming
  from a `NotificationClient` excludes service members.
  ([#6136](https://github.com/matrix-org/matrix-rust-sdk/pull/6136))
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

- `SpaceRoom` and `NotificationItem` now have an `is_dm` field to indicate
  whether the room is a DM room.
  ([#6537](https://github.com/matrix-org/matrix-rust-sdk/pull/6537))
- Add a list of `declined_by: Vec<OwnedUserId>` to the
  `TimelineItemContent::RtcNotification`, this will contain the list of users
  that have declined the call.
  ([#6494](https://github.com/matrix-org/matrix-rust-sdk/pull/6494))
- [**breaking**] Add the `suggested` field to the `SpaceRoom` struct, which
  indicates whether a space's admins have marked that sub-space/room as a
  "suggested" one to join.
  ([6417](https://github.com/matrix-org/matrix-rust-sdk/pull/6417))
- Handle local echoes of redactions in the timeline.
  ([#6250](https://github.com/matrix-org/matrix-rust-sdk/pull/6250))
- [**breaking**] Remove support for `native-tls` and remove all feature
  flags for selecting TLS backend, as `rustls` is the now the only supported
  TLS backend.
  ([#6409](https://github.com/matrix-org/matrix-rust-sdk/pull/6409))
- Add `call_intent` to `TimelineItemContent::RtcNotification`
  ([#6412](https://github.com/matrix-org/matrix-rust-sdk/pull/6412))
- Introduce a `ThreadListService` which offers reactive interfaces for rendering
  and managing the list of threads from a particular room.
  ([#6311](https://github.com/matrix-org/matrix-rust-sdk/pull/6311))
- [**breaking**] Remove the `Room::load_thread_list` in favor of the new
  `ThreadListService`
  ([#6311](https://github.com/matrix-org/matrix-rust-sdk/pull/6311))
- Add support for
  [MSC3489](https://github.com/matrix-org/matrix-spec-proposals/pull/3489) live
  location sharing through a new `TimelineItemContent::LiveLocation` variant.
- The internal timeline unique ID may be recycled when an event is deduplicated
  from the timeline, so that embedders can notice that it's the same item and
  avoid unnecessary re-rendering.
  ([#6228](https://github.com/matrix-org/matrix-rust-sdk/pull/6228))
- [**breaking**] Add `NotificationState.EventRedacted` enum value, to handle the
  case where a notification resolves to a redacted event.
  ([#6203](https://github.com/matrix-org/matrix-rust-sdk/pull/6203))
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
  the room list
  ([#6025](https://github.com/matrix-org/matrix-rust-sdk/pull/6025))
- Utilize the cache and include common relations when focusing a timeline on an
  event without requestion context.
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
- `Room::load_event_with_relations` now also calls `/relations` to fetch related
  events when falling back to network mode after a cache miss.
  ([#5930](https://github.com/matrix-org/matrix-rust-sdk/pull/5930))
- Expose `EventTimelineItem::forwarder` and `forwarder_profile`, which, if
  present, provide the ID and profile of the user who forwarded the keys used to
  decrypt the event as part of an
  [MSC4268](https://github.com/matrix-org/matrix-spec-proposals/pull/4268) key
  bundle. ([#6000](https://github.com/matrix-org/matrix-rust-sdk/pull/6000))

### Refactor

- Use `DmRoomDefinition` to check if a room should be considered part of the
  `RoomCategory::People` or `RoomCategory::Room` when using room list filters.
  ([#6490](https://github.com/matrix-org/matrix-rust-sdk/pull/6490))
- [**breaking**] `AnyOtherStateEventContentChange::RoomAliases` was removed.
  This state event type was removed from the Matrix specification a while ago,
  and support for it has been removed in Ruma.
  ([#6414](https://github.com/matrix-org/matrix-rust-sdk/pull/6414))
- [**breaking**] Move `LiveLocation` out of `TimelineItemContent` and into
  `MsgLikeKind` so it has access to `MsgLikeContent` `reactions`.
  ([#6286](https://github.com/matrix-org/matrix-rust-sdk/pull/6286))
- [**breaking**] Rename `AnyOtherFullStateEventContent` to
  `AnyOtherStateEventContentChange` to match the name change in the upstream
  types. ([#6218](https://github.com/matrix-org/matrix-rust-sdk/pull/6218))
- [**breaking**] Remove `WithLocking` from `EncryptionSyncService`, the locking
  mechanism will be taken from the parent `Client` with
  `Client::cross_process_store_config`.
  ([#6160](https://github.com/matrix-org/matrix-rust-sdk/pull/6160))
- [**breaking**] The [`Timeline::pin_event`] and [`Timeline::unpin_event`]
  methods have been moved to the SDK crate, in the `Room` object. Users can
  replace previous uses with `timeline.room().pin_event()` etc.
  ([#6106](https://github.com/matrix-org/matrix-rust-sdk/pull/6106))
- [**breaking**] Refactored `is_last_admin` to `is_last_owner` the check will
  now account also for v12 rooms, where creators and users with PL 150 matter.
  ([#6036](https://github.com/matrix-org/matrix-rust-sdk/pull/6036))
- [**breaking**] The `SpaceService` will no longer auto-subscribe to required
  client events when invoking the `subscribe_to_joined_spaces` but instead do it
  through its, now async, constructor.
  ([#5972](https://github.com/matrix-org/matrix-rust-sdk/pull/5972))
- [**breaking**] The `SpaceService`'s `joined_spaces` method has been renamed
  `top_level_joined_spaces` and `subscribe_to_joined_spaces` to
  `space_service.subscribe_to_top_level_joined_spaces`
  ([#5972](https://github.com/matrix-org/matrix-rust-sdk/pull/5972))
- `RoomListService::subscribe_to_rooms` now forgets previous subscriptions.
  ([#6012](https://github.com/matrix-org/matrix-rust-sdk/pull/6012))

## [0.16.1] - 2026-05-08

No notable changes in this release.

## [0.16.0] - 2025-12-04

### Features

- [**breaking**] `TimelineBuilder::track_read_marker_and_receipts` now takes a
  parameter to allow tracking to be enabled for all events (like before) or only
  for message-like events (which prevents read receipts from being placed on
  state events).
  ([#5900](https://github.com/matrix-org/matrix-rust-sdk/pull/5900))

## [0.15.0] - 2025-11-27

### Features

- Expose `is_space` in `NotificationItem`, allowing clients to determine if the
  room that triggered the notification is a space.
- [**breaking**] The `LatestEventValue::Local` type gains 2 new fields: `sender`
  and `profile`.
  ([#5885](https://github.com/matrix-org/matrix-rust-sdk/pull/5885))
- Add push actions to `NotificationItem`.
  ([#5835](https://github.com/matrix-org/matrix-rust-sdk/pull/5835))
- Add support for top level space ordering through
  [MSC3230](https://github.com/matrix-org/matrix-spec-proposals/pull/3230) and
  `m.space_order` room account data fields
  ([#5799](https://github.com/matrix-org/matrix-rust-sdk/pull/5799))

### Refactor

- `Timeline::latest_event` will return the latest event in the timeline, not the
  latest item of the timeline if it's an event.
- `TimelineFocusKind::Event` can now handle both the existing event pagination
  and thread pagination if the focused event is part of a thread
  ([#5678](https://github.com/matrix-org/matrix-rust-sdk/pull/5678)).
- [**breaking**] The `Room` type in `room_list_service` is renamed to
  `RoomListItem`.
  ([#5684](https://github.com/matrix-org/matrix-rust-sdk/pull/5684))

### Bug fixes

- `Timeline::latest_event_id` won't take threaded events into account on
  live/event focused timelines if `hide_threaded_events` is enabled. This fixes
  a bug in `Timeline::mark_as_read` that incorrectly tried to send a read
  receipt for threaded events that aren't really part of those timelines.
  ([#5864](https://github.com/matrix-org/matrix-rust-sdk/pull/5864/))
- Avoid replacing timeline items when the encryption info is unchanged.
  ([#5660](https://github.com/matrix-org/matrix-rust-sdk/pull/5660))
- Improvement performance of `RoomList` by introducing a new `RoomListItem` type
  (that replaces the `Room` type).
  ([#5684](https://github.com/matrix-org/matrix-rust-sdk/pull/5684))

## [0.14.0] - 2025-09-04

### Features

- Add a new [`SpaceService`] that provides high level reactive interfaces for
  listing the user's joined top level spaces as long as their children.
  ([#5509](https://github.com/matrix-org/matrix-rust-sdk/pull/5509))
- Add `new_filter_low_priority` and `new_filter_non_low_priority` filters to the
  room list filtering system, allowing clients to filter rooms based on their
  low priority status. The filters use the `Room::is_low_priority()` method
  which checks for the `m.lowpriority` room tag.
  ([#5508](https://github.com/matrix-org/matrix-rust-sdk/pull/5508))
- [**breaking**] Refactor the `non_space` filter into a `space` filter,
  favouring its use in combination with the `not` filter.
  ([#5508](https://github.com/matrix-org/matrix-rust-sdk/pull/5508))
- [**breaking**] Space rooms are now being retrieved through sliding sync and
  the newly introduced [`room_list_service::filters::new_filter_non_space`]
  filter should be used to exclude them from any room list.
  ([5479](https://github.com/matrix-org/matrix-rust-sdk/pull/5479))
- [**breaking**] [`Timeline::send_gallery()`] now automatically fills in the
  thread relationship, based on the timeline focus. As a result, the
  `GalleryConfig::reply()` builder method has been replaced with
  `GalleryConfig::in_reply_to`, and only takes an optional event id (the event
  that is effectively replied to) instead of the `Reply` type. The proper way to
  start a thread with a gallery event is now thus to create a threaded-focused
  timeline, and then use `Timeline::send_gallery()`.
  ([5427](https://github.com/matrix-org/matrix-rust-sdk/pull/5427))
- [**breaking**] [`Timeline::send_attachment()`] now automatically fills in the
  thread relationship, based on the timeline focus. As a result, there's a new
  `matrix_sdk_ui::timeline::AttachmentConfig` type in town, that has a
  simplified optional parameter `replied_to` of type `OwnedEventId` instead of
  the `Reply` type and that must be used in place of
  `matrix_sdk::attachment::AttachmentConfig`. The proper way to start a thread
  with a media attachment is now thus to create a threaded-focused timeline, and
  then use `Timeline::send_attachment()`.
  ([5427](https://github.com/matrix-org/matrix-rust-sdk/pull/5427))
- [**breaking**] [`Timeline::send_reply()`] now automatically fills in the
  thread relationship, based on the timeline focus. As a result, it only takes
  an `OwnedEventId` parameter, instead of the `Reply` type. The proper way to
  start a thread is now thus to create a threaded-focused timeline, and then use
  `Timeline::send()`.
  ([5427](https://github.com/matrix-org/matrix-rust-sdk/pull/5427))
- `Timeline::send()` will now automatically fill the thread relationship, if the
  timeline has a thread focus, and the sent event doesn't have a prefilled
  `relates_to` field (i.e. a relationship).
  ([5427](https://github.com/matrix-org/matrix-rust-sdk/pull/5427))

### Refactor

- [**breaking**] The MSRV has been bumped to Rust 1.88.
  ([#5431](https://github.com/matrix-org/matrix-rust-sdk/pull/5431))

### Bug fixes

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
- Add `EmbeddedEvent::timestamp` and `EmbeddedEvent::identifier` which are
  already available in regular timeline items.
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

- [**breaking**] [`TimelineItemContent::reactions()`] returns an
  `Option<&ReactionsByKeyBySender>` instead of `ReactionsByKeyBySender`. This
  reflects the fact that some timeline items cannot hold reactions at all.
- `NotificationItem::room_join_rule` is now optional to reflect that the join
  rule state event might be missing, in which case it will be set to `None`. The
  `NotificationItem::is_public` field has been replaced with a method that
  returns an `Option<bool>`, based on the same logic.
  ([#5278](https://github.com/matrix-org/matrix-rust-sdk/pull/5278))

### Bug fixes

- Introduce `Timeline` regions, which helps to remove a class of bugs in the
  `Timeline` where items could be inserted in the wrong _regions_, such as
  a remote timeline item before the `TimelineStart` virtual timeline item.
  ([#5000](https://github.com/matrix-org/matrix-rust-sdk/pull/5000))
- `NotificationClient` will filter out events sent by ignored users on
  `get_notification` and `get_notifications`.
  ([#5081](https://github.com/matrix-org/matrix-rust-sdk/pull/5081))

### Features

- `Timeline::send_single_receipt()` and `Timeline::send_multiple_receipts()` now
  also unset the unread flag of the room if an unthreaded read receipt is sent.
  ([#5055](https://github.com/matrix-org/matrix-rust-sdk/pull/5055))
- `Timeline::mark_as_read()` unsets the unread flag of the room if it was set.
  ([#5055](https://github.com/matrix-org/matrix-rust-sdk/pull/5055))
- Add new method `Timeline::send_gallery` to allow sending MSC4274-style
  galleries.
  ([#5125](https://github.com/matrix-org/matrix-rust-sdk/pull/5125))

## [0.11.0] - 2025-04-11

### Bug fixes

### Features

- [**breaking**] Optionally allow starting threads with `Timeline::send_reply`.
  ([#4819](https://github.com/matrix-org/matrix-rust-sdk/pull/4819))
- [**breaking**] Push `RepliedToInfo`, `ReplyContent`, `EnforceThread` and
  `UnsupportedReplyItem` (becoming `ReplyError`) down into matrix_sdk.
  [`Timeline::send_reply()`] now takes an event ID rather than a
  `RepliedToInfo`. `Timeline::replied_to_info_from_event_id` has been made
  private in `matrix_sdk`.
  ([#4842](https://github.com/matrix-org/matrix-rust-sdk/pull/4842))
- Allow sending media as (thread) replies. The reply behaviour can be configured
  through new fields on [`AttachmentConfig`].
  ([#4852](https://github.com/matrix-org/matrix-rust-sdk/pull/4852))

### Refactor

- [**breaking**] Reactions on a given timeline item have been moved from
  [`EventTimelineItem::reactions()`] to [`TimelineItemContent::reactions()`];
  they're thus available from an [`EventTimelineItem`] by calling
  `.content().reactions()`. They're also returned by ownership (cloned) instead
  of by reference.
  ([#4576](https://github.com/matrix-org/matrix-rust-sdk/pull/4576))
- [**breaking**] The parameters `event_id` and `enforce_thread` on
  [`Timeline::send_reply()`] have been wrapped in a `reply` struct parameter.
  ([#4880](https://github.com/matrix-org/matrix-rust-sdk/pull/4880/))

## [0.10.0] - 2025-02-04

### Bug fixes

- Don't consider rooms in the banned state to be non-left rooms. This bug was
  introduced due to the introduction of the banned state for rooms, and the
  non-left room filter did not take the new room state into account.
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

- [**breaking**] `Timeline::paginate_forwards` and
  `Timeline::paginate_backwards` are unified to work on a live or focused
  timeline. `Timeline::live_paginate_*` and `Timeline::focused_paginate_*` have
  been removed
  ([#4584](https://github.com/matrix-org/matrix-rust-sdk/pull/4584)).

- [**breaking**] `Timeline::subscribe_batched` replaces
  `Timeline::subscribe`. `subscribe` has been removed in
  [#4567](https://github.com/matrix-org/matrix-rust-sdk/pull/4567),
  and `subscribe_batched` has been renamed to `subscribe` in
  [#4585](https://github.com/matrix-org/matrix-rust-sdk/pull/4585).

## [0.9.0] - 2024-12-18

### Bug fixes

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

### Bug fixes

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

- [**breaking**] `Timeline::edit` now takes a
  `RoomMessageEventContentWithoutRelation`.

- [**breaking**] `Timeline::send_attachment` now takes an `impl Into<PathBuf>`
  for the path of the file to send.

- [**breaking**] `Timeline::item_by_transaction_id` has been renamed to
  `Timeline::local_item_by_transaction_id` (always returns local echoes).

## 0.7.0

Initial release
