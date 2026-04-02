// Copyright 2026 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Client-side read receipts computation
//!
//! While Matrix servers have the ability to provide basic information about the
//! unread status of rooms, via [`crate::sync::UnreadNotificationsCount`], it's
//! not reliable for encrypted rooms. Indeed, the server doesn't have access to
//! the content of encrypted events, so it can only makes guesses when
//! estimating unread and highlight counts.
//!
//! Instead, this module provides facilities to compute the number of unread
//! messages, unread notifications (based on the push rules) and unread
//! highlights in a room.
//!
//! Counting unread messages is performed by looking at the latest receipt of
//! the current user, and inferring which events are following it, according to
//! the sync ordering.
//!
//! For notifications and highlights to be precisely accounted for, we also need
//! to pay attention to the user's notification settings. Fortunately, this is
//! also something we need for notifications, so we can reuse this code.
//!
//! Of course, not all events are created equal, and some are less interesting
//! than others, and shouldn't cause a room to be marked unread. This module's
//! `marks_as_unread` function shows the opiniated set of rules that will filter
//! out uninterested events.
//!
//! The only `pub(crate)` method in that module is `compute_unread_counts`,
//! which updates the `RoomInfo` in place according to the new counts.
//!
//! ## Implementation details: How to get the latest receipt?
//!
//! ### Preliminary context
//!
//! We reuse a room event cache's linked chunk, and iterate over the events that
//! are stored in memory.
//!
//! ### How-to
//!
//! When we call `compute_unread_counts`, that's for one of two reasons (and
//! maybe both at once, or maybe none at all):
//! - we received a new receipt
//! - new events came in.
//!
//! A read receipt is considered _active_ if it's been received from sync
//! *and* it matches a known event in the in-memory linked chunk.
//!
//! The *latest active* receipt is the one that's active, with the latest order
//! (according to the event cache ordering, aka its position in the linked
//! chunk).
//!
//! The problem of keeping a precise read count is thus equivalent to finding
//! the latest active receipt, and counting interesting events after it.
//!
//! When we need to recompute the unread counts, we go through all the linked
//! chunk's events to select a "better" active receipt, using the following
//! rules:
//!
//! - an event we sent counts as a read receipt (it's called the implicit read
//!   receipt in the spec),
//! - an event which is referenced in the read receipt event content (either a
//!   private or a public read receipt, of type unthreaded or main, to keep
//!   maximal compatibility with thread-unaware clients),
//! - a previously stashed read receipt we've received from a read receipt event
//!   content, but for which we couldn't find the corresponding event. It's
//!   possible that a read receipt is received before the corresponding event.
//!
//! The read receipt that wins is always the one that points to the most recent
//! in the linked chunk ordering. In other words, the receipt type (as described
//! above) doesn't matter; it's the relative position in the linked chunk
//! ordering which does.
//!
//! Once we have a new *better active receipt*, we'll save it in the
//! `RoomReadReceipt` data (stored in `RoomInfo`), and we'll compute the counts,
//! starting from the event the better active receipt was referring to.
//!
//! If we *don't* have a better active receipt, that means that all the events
//! received in that batch aren't referred to by a known read receipt, _and_ we
//! didn't get a new better receipt that matched known events. In that case, we
//! can just consider that all the events are new, and count them as such.

use matrix_sdk_base::{
    read_receipts::{LatestReadReceipt, RoomReadReceipts},
    serde_helpers::extract_relation,
    store::DynStateStore,
};
use matrix_sdk_common::{
    deserialized_responses::TimelineEvent, ring_buffer::RingBuffer,
    serde_helpers::extract_thread_root,
};
use ruma::{
    EventId, OwnedEventId, OwnedUserId, RoomId, UserId,
    events::{
        AnySyncTimelineEvent, MessageLikeEventType,
        receipt::{ReceiptEventContent, ReceiptThread, ReceiptType},
        relation::RelationType,
    },
    serde::Raw,
};
use tracing::{debug, instrument, trace, warn};

use crate::event_cache::{
    automatic_pagination::AutomaticPagination, caches::event_linked_chunk::EventLinkedChunk,
};

trait RoomReadReceiptsExt {
    /// Update the [`RoomReadReceipts`] unread counts according to the new
    /// event.
    ///
    /// Returns whether a new event triggered a new unread/notification/mention.
    fn process_event(
        &mut self,
        event: &TimelineEvent,
        user_id: &UserId,
        with_threading_support: bool,
    );

    fn reset(&mut self);

    /// Try to find the event to which the receipt attaches to, and if found,
    /// will update the notification count in the room.
    fn find_and_process_events<'a>(
        &mut self,
        receipt_event_id: &EventId,
        user_id: &UserId,
        events: impl IntoIterator<Item = &'a TimelineEvent>,
        with_threading_support: bool,
    ) -> bool;
}

impl RoomReadReceiptsExt for RoomReadReceipts {
    /// Update the [`RoomReadReceipts`] unread counts according to the new
    /// event.
    ///
    /// Returns whether a new event triggered a new unread/notification/mention.
    #[inline(always)]
    fn process_event(
        &mut self,
        event: &TimelineEvent,
        user_id: &UserId,
        with_threading_support: bool,
    ) {
        if with_threading_support && extract_thread_root(event.raw()).is_some() {
            return;
        }

        if marks_as_unread(event.raw(), user_id) {
            self.num_unread += 1;
        }

        let mut has_notify = false;
        let mut has_mention = false;

        let Some(actions) = event.push_actions() else {
            return;
        };

        for action in actions.iter() {
            if !has_notify && action.should_notify() {
                self.num_notifications += 1;
                has_notify = true;
            }
            if !has_mention && action.is_highlight() {
                self.num_mentions += 1;
                has_mention = true;
            }
        }
    }

    #[inline(always)]
    fn reset(&mut self) {
        self.num_unread = 0;
        self.num_notifications = 0;
        self.num_mentions = 0;
    }

    /// Try to find the event to which the receipt attaches to, and if found,
    /// will update the notification count in the room.
    #[instrument(skip_all)]
    fn find_and_process_events<'a>(
        &mut self,
        receipt_event_id: &EventId,
        user_id: &UserId,
        events: impl IntoIterator<Item = &'a TimelineEvent>,
        with_threading_support: bool,
    ) -> bool {
        let mut counting_receipts = false;

        for event in events {
            // Sliding sync sometimes sends the same event multiple times, so it can be at
            // the beginning and end of a batch, for instance. In that case, just reset
            // every time we see the event matching the receipt.
            if let Some(event_id) = event.event_id()
                && event_id == receipt_event_id
            {
                // Bingo! Switch over to the counting state, after resetting the
                // previous counts.
                trace!("Found the event the receipt was referring to! Starting to count.");
                self.reset();
                counting_receipts = true;
                continue;
            }

            if counting_receipts {
                self.process_event(event, user_id, with_threading_support);
            }
        }

        counting_receipts
    }
}

/// The receipt types we look for, in order of priority (the first ones are more
/// likely to be ahead in the timeline, so we look for them first).
const ALL_RECEIPT_TYPES: [ReceiptType; 2] = [ReceiptType::ReadPrivate, ReceiptType::Read];

/// Return a new better (i.e. more recent) receipt based on a search of the
/// linked chunk.
///
/// A receipt is selected if:
///
/// - it's an implicit read receipt (i.e. an event we've sent),
/// - it's holding onto a new read receipt we've just received,
/// - it was a pending receipt for which we found the event now,
///
/// A receipt returned in this function **must** point to an event that is in
/// the linked chunk.
fn select_best_receipt(
    user_id: &UserId,
    linked_chunk: &EventLinkedChunk,
    pending_receipts: &mut RingBuffer<OwnedEventId>,
    new_receipt_event: Option<&ReceiptEventContent>,
    latest_active: Option<&EventId>,
    with_threading_support: bool,
) -> Option<OwnedEventId> {
    // If we had a new receipt event, add the main/unthreaded receipts it contains
    // to the pending receipts list. We'll try to chase them later.
    if let Some(receipt_event) = new_receipt_event {
        for (event_id, receipts) in &receipt_event.0 {
            for ty in ALL_RECEIPT_TYPES {
                if let Some(receipts) = receipts.get(&ty)
                    && let Some(receipt) = receipts.get(user_id)
                    && matches!(receipt.thread, ReceiptThread::Main | ReceiptThread::Unthreaded)
                {
                    // Add it to the pending receipts list.
                    trace!(%event_id, "found new receipt (added to pending)");
                    pending_receipts.push(event_id.clone());
                }
            }
        }
    }

    // This loop folds two actions at once:
    // - try to find the most recent receipt, by looking at the events in reverse
    //   order (i.e. from the most recent to the least recent),
    // - try to match stashed receipts against known events in the linked chunk, so
    //   as to shrink the stash of pending receipts.
    //
    // We can early exit out of this loop, as soon as there's no more work to do,
    // i.e., we've found a better receipt, *and* there's no more pending receipt
    // to try to match against events in the linked chunk.

    let mut receipt = None;

    for (event, event_id) in
        linked_chunk.revents().filter_map(|(_pos, ev)| Some((ev, ev.event_id()?)))
    {
        if receipt.is_none() {
            // Try to see if the latest active receipt is still the most recent receipt.
            if latest_active == Some(&event_id) {
                // The latest active receipt is still the most recent receipt, so keep it.
                trace!(active = %event_id, "the latest active receipt is still the most recent; stopping search");
                receipt = Some(event_id.clone());
            }
            // Try to find an implicit read receipt (i.e. an event sent by the current
            // user).
            //
            // If the client is enabled with threading support, skip events that are in threads.
            else if event.sender().as_deref() == Some(user_id)
                && (!with_threading_support || extract_thread_root(event.raw()).is_none())
            {
                trace!(implicit = %event_id, "found an implicit receipt; stopping search");
                receipt = Some(event_id.clone());
            }
        }

        // Early exit condition (see the comment above): we've already found a most
        // recent receipt, and there's no other pending receipts to match against known
        // events.
        if receipt.is_some() && pending_receipts.is_empty() {
            trace!("exiting loop; found a better receipt, and no more pending receipt to match");
            break;
        }

        // Try to match pending receipts to events known in the linked chunk. If we
        // haven't found any receipt yet, the first matched pending receipt is a better
        // one!
        pending_receipts.retain(|pending| {
            if *pending == event_id {
                if receipt.is_none() {
                    trace!(pending = %event_id, "found a pending receipt; stopping search");
                    receipt = Some(event_id.clone());
                } else {
                    trace!(%event_id, "discarding a pending receipt that wasn't selected");
                }

                // Don't keep the pending receipt in the pending list: we've already identified
                // a better, more recent receipt at this point (found == Some).
                false
            } else {
                // Keep the receipt, in case the associated event shows up later.
                true
            }
        });
    }

    receipt
}

/// Try to find extra read receipts that were in the store but never saved in
/// the [`RoomReadReceipts`] data structure.
///
/// Doesn't return a `Result`, because this is entirely optional; if the store
/// fails to load these receipts, the worst that can happen is incorrect unread
/// counts until the next receipt event is received from sync.
async fn try_find_store_receipts(
    store: &DynStateStore,
    user_id: &UserId,
    room_id: &RoomId,
    read_receipts: &mut RoomReadReceipts,
) {
    for receipt_type in ALL_RECEIPT_TYPES {
        // Implementation note: we want to prioritize a `Unthreaded` receipt over a
        // `Main`-threaded one, for better compatibility with thread-unaware clients.
        for receipt_thread in [ReceiptThread::Unthreaded, ReceiptThread::Main] {
            if let Ok(Some((event_id, _receipt))) = store
                .get_user_room_receipt_event(
                    room_id,
                    receipt_type.clone(),
                    receipt_thread.clone(),
                    user_id,
                )
                .await
            {
                trace!(%event_id, ?receipt_type, ?receipt_thread, "Found a dormant receipt in the store");

                if read_receipts.latest_active.is_none() {
                    read_receipts.latest_active =
                        Some(LatestReadReceipt { event_id: event_id.clone() });
                } else {
                    // This loop has already flagged a read receipt as the new `latest_active`.
                    // Extra read receipts can go to the pending receipts list, as they're lower
                    // priority, by the implementation notes above.
                    read_receipts.pending.push(event_id.clone());
                }
            }
        }
    }
}

/// Given a set of events coming from sync, for a room, update the
/// [`RoomReadReceipts`]'s counts of unread messages, notifications and
/// highlights' in place.
///
/// See this module's documentation for more information.
#[instrument(skip_all, fields(room_id = %room_id))]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn compute_unread_counts(
    user_id: &UserId,
    room_id: &RoomId,
    receipt_event: Option<&ReceiptEventContent>,
    linked_chunk: &EventLinkedChunk,
    read_receipts: &mut RoomReadReceipts,
    with_threading_support: bool,
    automatic_pagination: Option<&AutomaticPagination>,
    state_store: &DynStateStore,
) {
    debug!(?read_receipts, "Starting");

    // If we don't have a latest active receipt for this room, try to reload one
    // from the state store into the `RoomReadReceipts`.
    if read_receipts.latest_active.is_none() {
        try_find_store_receipts(state_store, user_id, room_id, read_receipts).await;
    }

    let better_receipt = select_best_receipt(
        user_id,
        linked_chunk,
        &mut read_receipts.pending,
        receipt_event,
        read_receipts.latest_active.as_ref().map(|latest_active| latest_active.event_id.as_ref()),
        with_threading_support,
    );

    if let Some(event_id) = better_receipt {
        // We've found the id of an event to which the receipt attaches. The associated
        // event may either come from the new batch of events associated to
        // this sync, or it may live in the past timeline events we know
        // about.

        // First, save the event id as the latest one that has a read receipt.
        trace!(%event_id, "Saving a new active read receipt");
        read_receipts.latest_active = Some(LatestReadReceipt { event_id: event_id.clone() });

        // The event for the receipt is in the linked chunk, so we'll find it and can
        // count safely from here.
        read_receipts.find_and_process_events(
            &event_id,
            user_id,
            linked_chunk.events().map(|(_pos, event)| event),
            with_threading_support,
        );

        debug!(?read_receipts, "after finding a better receipt");
        return;
    }

    // Request a pagination: we haven't found a better receipt, but we haven't even
    // found the latest active receipt!
    if let Some(automatic_pagination) = automatic_pagination {
        if automatic_pagination.run_once(room_id) {
            trace!("Requested pagination to find a better receipt");
        } else {
            warn!("Failed to request pagination to find a better receipt");
        }
    }

    // If we haven't returned at this point, it means we don't have any new "active"
    // read receipt. So either there was a previous one further in the past, or
    // none.
    //
    // In that case, the number of unreads is *at most* the number of processed
    // events. Reset the number of unreads, and recount them all.
    read_receipts.reset();

    for (_pos, event) in linked_chunk.events() {
        read_receipts.process_event(event, user_id, with_threading_support);
    }

    debug!(?read_receipts, "no better receipt");
}

/// Is the event worth marking a room as unread?
fn marks_as_unread(event: &Raw<AnySyncTimelineEvent>, user_id: &UserId) -> bool {
    // Parse the sender from the raw event.
    if event.get_field::<OwnedUserId>("sender").ok().flatten().as_deref() == Some(user_id) {
        tracing::trace!("not interesting because sent by the current user");
        return false;
    }

    let Some(event_type) = event.get_field::<MessageLikeEventType>("type").ok().flatten() else {
        tracing::trace!(
            "failed to parse event type for event with id {:?}, skipping it",
            event.get_field::<OwnedEventId>("event_id").ok().flatten()
        );
        return false;
    };

    match event_type {
        MessageLikeEventType::Message
        | MessageLikeEventType::PollStart
        | MessageLikeEventType::UnstablePollStart
        | MessageLikeEventType::PollEnd
        | MessageLikeEventType::UnstablePollEnd
        | MessageLikeEventType::RoomEncrypted
        | MessageLikeEventType::RoomMessage
        | MessageLikeEventType::Sticker => {}

        _ => {
            tracing::trace!("not interesting because not an interesting message-like");
            return false;
        }
    }

    // Filter out edits.
    if let Some((RelationType::Replacement, _)) = extract_relation(event) {
        tracing::trace!("not interesting because edited");
        return false;
    }

    // Filter out redacted events.
    #[derive(serde::Deserialize)]
    struct UnsignedContent {
        redacted_because: Option<Raw<AnySyncTimelineEvent>>,
    }

    // Filter out redactions.
    if let Ok(Some(UnsignedContent { redacted_because: Some(_redaction) })) =
        event.get_field::<UnsignedContent>("unsigned")
    {
        tracing::trace!("not interesting because redacted");
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, ops::Not as _};

    use matrix_sdk_base::read_receipts::RoomReadReceipts;
    use matrix_sdk_common::{deserialized_responses::TimelineEvent, ring_buffer::RingBuffer};
    use matrix_sdk_test::{ALICE, event_factory::EventFactory};
    use ruma::{
        EventId, UserId, event_id,
        events::{
            receipt::{ReceiptThread, ReceiptType},
            room::{member::MembershipState, message::MessageType},
        },
        owned_event_id,
        push::{Action, HighlightTweakValue, Tweak},
        room_id, user_id,
    };

    use super::marks_as_unread;
    use crate::event_cache::caches::{
        event_linked_chunk::EventLinkedChunk,
        read_receipts::{RoomReadReceiptsExt as _, select_best_receipt},
    };

    #[test]
    fn test_room_message_marks_as_unread() {
        let user_id = user_id!("@alice:example.org");
        let other_user_id = user_id!("@bob:example.org");

        let f = EventFactory::new();

        // A message from somebody else marks the room as unread...
        let ev = f.text_msg("A").event_id(event_id!("$ida")).sender(other_user_id).into_raw_sync();
        assert!(marks_as_unread(&ev, user_id));

        // ... but a message from ourselves doesn't.
        let ev = f.text_msg("A").event_id(event_id!("$ida")).sender(user_id).into_raw_sync();
        assert!(marks_as_unread(&ev, user_id).not());
    }

    #[test]
    fn test_room_edit_doesnt_mark_as_unread() {
        let user_id = user_id!("@alice:example.org");
        let other_user_id = user_id!("@bob:example.org");

        // An edit to a message from somebody else doesn't mark the room as unread.
        let ev = EventFactory::new()
            .text_msg("* edited message")
            .edit(
                event_id!("$someeventid:localhost"),
                MessageType::text_plain("edited message").into(),
            )
            .event_id(event_id!("$ida"))
            .sender(other_user_id)
            .into_raw_sync();

        assert!(marks_as_unread(&ev, user_id).not());
    }

    #[test]
    fn test_redaction_doesnt_mark_room_as_unread() {
        let user_id = user_id!("@alice:example.org");
        let other_user_id = user_id!("@bob:example.org");

        // A redact of a message from somebody else doesn't mark the room as unread.
        let ev = EventFactory::new()
            .redaction(event_id!("$151957878228ssqrj:localhost"))
            .sender(other_user_id)
            .event_id(event_id!("$151957878228ssqrJ:localhost"))
            .into_raw_sync();

        assert!(marks_as_unread(&ev, user_id).not());
    }

    #[test]
    fn test_reaction_doesnt_mark_room_as_unread() {
        let user_id = user_id!("@alice:example.org");
        let other_user_id = user_id!("@bob:example.org");

        // A reaction from somebody else to a message doesn't mark the room as unread.
        let ev = EventFactory::new()
            .reaction(event_id!("$15275047031IXQRj:localhost"), "👍")
            .sender(other_user_id)
            .event_id(event_id!("$15275047031IXQRi:localhost"))
            .into_raw_sync();

        assert!(marks_as_unread(&ev, user_id).not());
    }

    #[test]
    fn test_state_event_doesnt_mark_as_unread() {
        let user_id = user_id!("@alice:example.org");
        let event_id = event_id!("$1");

        let ev = EventFactory::new()
            .member(user_id)
            .membership(MembershipState::Join)
            .display_name("Alice")
            .event_id(event_id)
            .into_raw_sync();
        assert!(marks_as_unread(&ev, user_id).not());

        let other_user_id = user_id!("@bob:example.org");
        assert!(marks_as_unread(&ev, other_user_id).not());
    }

    #[test]
    fn test_count_unread_and_mentions() {
        fn make_event(user_id: &UserId, push_actions: Vec<Action>) -> TimelineEvent {
            let mut ev = EventFactory::new()
                .text_msg("A")
                .sender(user_id)
                .event_id(event_id!("$ida"))
                .into_event();
            ev.set_push_actions(push_actions);
            ev
        }

        let user_id = user_id!("@alice:example.org");
        let threading_support = false;

        // An interesting event from oneself doesn't count as a new unread message.
        let event = make_event(user_id, Vec::new());
        let mut receipts = RoomReadReceipts::default();
        receipts.process_event(&event, user_id, threading_support);
        assert_eq!(receipts.num_unread, 0);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 0);

        // An interesting event from someone else does count as a new unread message.
        let event = make_event(user_id!("@bob:example.org"), Vec::new());
        let mut receipts = RoomReadReceipts::default();
        receipts.process_event(&event, user_id, threading_support);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 0);

        // Push actions computed beforehand are respected.
        let event = make_event(user_id!("@bob:example.org"), vec![Action::Notify]);
        let mut receipts = RoomReadReceipts::default();
        receipts.process_event(&event, user_id, threading_support);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 1);

        let event = make_event(
            user_id!("@bob:example.org"),
            vec![Action::SetTweak(Tweak::Highlight(HighlightTweakValue::Yes))],
        );
        let mut receipts = RoomReadReceipts::default();
        receipts.process_event(&event, user_id, threading_support);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 1);
        assert_eq!(receipts.num_notifications, 0);

        let event = make_event(
            user_id!("@bob:example.org"),
            vec![Action::SetTweak(Tweak::Highlight(HighlightTweakValue::Yes)), Action::Notify],
        );
        let mut receipts = RoomReadReceipts::default();
        receipts.process_event(&event, user_id, threading_support);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 1);
        assert_eq!(receipts.num_notifications, 1);

        // Technically this `push_actions` set would be a bug somewhere else, but let's
        // make sure to resist against it.
        let event = make_event(user_id!("@bob:example.org"), vec![Action::Notify, Action::Notify]);
        let mut receipts = RoomReadReceipts::default();
        receipts.process_event(&event, user_id, threading_support);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 1);
    }

    #[test]
    fn test_find_and_process_events() {
        let ev0 = event_id!("$0");
        let user_id = user_id!("@alice:example.org");
        let thread_support = false;

        // When provided with no events, we report not finding the event to which the
        // receipt relates.
        let mut receipts = RoomReadReceipts::default();
        assert!(receipts.find_and_process_events(ev0, user_id, &[], thread_support).not());
        assert_eq!(receipts.num_unread, 0);
        assert_eq!(receipts.num_notifications, 0);
        assert_eq!(receipts.num_mentions, 0);

        // When provided with one event, that's not the receipt event, we don't count
        // it.
        fn make_event(event_id: &EventId) -> TimelineEvent {
            EventFactory::new()
                .text_msg("A")
                .sender(user_id!("@bob:example.org"))
                .event_id(event_id)
                .into()
        }

        let mut receipts = RoomReadReceipts {
            num_unread: 42,
            num_notifications: 13,
            num_mentions: 37,
            ..Default::default()
        };
        assert!(
            receipts
                .find_and_process_events(
                    ev0,
                    user_id,
                    &[make_event(event_id!("$1"))],
                    thread_support
                )
                .not()
        );
        assert_eq!(receipts.num_unread, 42);
        assert_eq!(receipts.num_notifications, 13);
        assert_eq!(receipts.num_mentions, 37);

        // When provided with one event that's the receipt target, we find it, reset the
        // count, and since there's nothing else, we stop there and end up with
        // zero counts.
        let mut receipts = RoomReadReceipts {
            num_unread: 42,
            num_notifications: 13,
            num_mentions: 37,
            ..Default::default()
        };
        assert!(receipts.find_and_process_events(ev0, user_id, &[make_event(ev0)], thread_support),);
        assert_eq!(receipts.num_unread, 0);
        assert_eq!(receipts.num_notifications, 0);
        assert_eq!(receipts.num_mentions, 0);

        // When provided with multiple events and not the receipt event, we do not count
        // anything..
        let mut receipts = RoomReadReceipts {
            num_unread: 42,
            num_notifications: 13,
            num_mentions: 37,
            ..Default::default()
        };
        assert!(
            receipts
                .find_and_process_events(
                    ev0,
                    user_id,
                    &[
                        make_event(event_id!("$1")),
                        make_event(event_id!("$2")),
                        make_event(event_id!("$3"))
                    ],
                    thread_support
                )
                .not()
        );
        assert_eq!(receipts.num_unread, 42);
        assert_eq!(receipts.num_notifications, 13);
        assert_eq!(receipts.num_mentions, 37);

        // When provided with multiple events including one that's the receipt event, we
        // find it and count from it.
        let mut receipts = RoomReadReceipts {
            num_unread: 42,
            num_notifications: 13,
            num_mentions: 37,
            ..Default::default()
        };
        assert!(receipts.find_and_process_events(
            ev0,
            user_id,
            &[
                make_event(event_id!("$1")),
                make_event(ev0),
                make_event(event_id!("$2")),
                make_event(event_id!("$3"))
            ],
            thread_support
        ));
        assert_eq!(receipts.num_unread, 2);
        assert_eq!(receipts.num_notifications, 0);
        assert_eq!(receipts.num_mentions, 0);

        // Even if duplicates are present in the new events list, the count is correct.
        let mut receipts = RoomReadReceipts {
            num_unread: 42,
            num_notifications: 13,
            num_mentions: 37,
            ..Default::default()
        };
        assert!(receipts.find_and_process_events(
            ev0,
            user_id,
            &[
                make_event(ev0),
                make_event(event_id!("$1")),
                make_event(ev0),
                make_event(event_id!("$2")),
                make_event(event_id!("$3"))
            ],
            thread_support
        ));
        assert_eq!(receipts.num_unread, 2);
        assert_eq!(receipts.num_notifications, 0);
        assert_eq!(receipts.num_mentions, 0);
    }

    #[test]
    fn test_compute_unread_counts_with_threading_enabled() {
        fn make_event(user_id: &UserId, thread_root: &EventId) -> TimelineEvent {
            EventFactory::new()
                .text_msg("A")
                .sender(user_id)
                .event_id(event_id!("$ida"))
                .in_thread(thread_root, event_id!("$latest_event"))
                .into_event()
        }

        let mut receipts = RoomReadReceipts::default();

        let own_alice = user_id!("@alice:example.org");
        let bob = user_id!("@bob:example.org");

        let threading_support = true;

        // Threaded messages from myself or other users shouldn't change the
        // unread counts.
        receipts.process_event(
            &make_event(own_alice, event_id!("$some_thread_root")),
            own_alice,
            threading_support,
        );
        receipts.process_event(
            &make_event(own_alice, event_id!("$some_other_thread_root")),
            own_alice,
            threading_support,
        );

        receipts.process_event(
            &make_event(bob, event_id!("$some_thread_root")),
            own_alice,
            threading_support,
        );
        receipts.process_event(
            &make_event(bob, event_id!("$some_other_thread_root")),
            own_alice,
            threading_support,
        );

        assert_eq!(receipts.num_unread, 0);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 0);

        // Processing an unthreaded message should still count as unread.
        receipts.process_event(
            &EventFactory::new().text_msg("A").sender(bob).event_id(event_id!("$ida")).into_event(),
            own_alice,
            threading_support,
        );

        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 0);
    }

    #[test]
    fn test_select_best_receipt_noop() {
        let room_id = room_id!("!roomid:example.org");
        let f = EventFactory::new().room(room_id).sender(*ALICE);

        // Create a non-empty linked chunk, with no messages sent by the current user.
        let mut lc = EventLinkedChunk::new();
        lc.push_events(vec![
            f.text_msg("Event 1").event_id(event_id!("$1")).into_event(),
            f.text_msg("Event 2").event_id(event_id!("$2")).into_event(),
            f.text_msg("Event 3").event_id(event_id!("$3")).into_event(),
        ]);

        let own_user_id = user_id!("@not_alice:example.org");

        // When there are no pending receipts,
        let mut pending_receipts = RingBuffer::new(NonZeroUsize::new(16).unwrap());
        // And no new receipt,
        let new_receipt_event = None;
        // And no active receipt,
        let active_receipt = None;
        let with_threading_support = false;

        // Then there's no best receipt.
        let receipt = select_best_receipt(
            own_user_id,
            &lc,
            &mut pending_receipts,
            new_receipt_event,
            active_receipt,
            with_threading_support,
        );
        assert!(receipt.is_none());
        // And there are no pending receipts.
        assert!(pending_receipts.is_empty());
    }

    #[test]
    fn test_select_best_receipt_implicit() {
        let room_id = room_id!("!roomid:example.org");
        let f = EventFactory::new().room(room_id).sender(*ALICE);
        let own_user_id = user_id!("@not_alice:example.org");

        // Create a non-empty linked chunk, with one message sent by the current user,
        // which will act as an implicit read receipt.
        let mut lc = EventLinkedChunk::new();
        lc.push_events(vec![
            f.text_msg("Event 1").event_id(event_id!("$1")).into_event(),
            f.text_msg("Event 2").event_id(event_id!("$2")).sender(own_user_id).into_event(),
            f.text_msg("Event 3").event_id(event_id!("$3")).into_event(),
        ]);

        // When there are no pending receipts,
        let mut pending_receipts = RingBuffer::new(NonZeroUsize::new(16).unwrap());
        // And no new receipt,
        let new_receipt_event = None;
        // And no active receipt,
        let active_receipt = None;
        let with_threading_support = false;

        // Then there's a new best receipt, which is the implicit one.
        let receipt = select_best_receipt(
            own_user_id,
            &lc,
            &mut pending_receipts,
            new_receipt_event,
            active_receipt,
            with_threading_support,
        );
        assert_eq!(receipt.unwrap(), event_id!("$2"));
        // And there are no pending receipts.
        assert!(pending_receipts.is_empty());
    }

    #[test]
    fn test_select_best_receipt_active_receipt() {
        let room_id = room_id!("!roomid:example.org");
        let f = EventFactory::new().room(room_id).sender(*ALICE);

        // Create a non-empty linked chunk, with no messages sent by the current user.
        let mut lc = EventLinkedChunk::new();
        lc.push_events(vec![
            f.text_msg("Event 1").event_id(event_id!("$1")).into_event(),
            f.text_msg("Event 2").event_id(event_id!("$2")).into_event(),
            f.text_msg("Event 3").event_id(event_id!("$3")).into_event(),
        ]);

        let own_user_id = user_id!("@not_alice:example.org");

        // When there are no pending receipts,
        let mut pending_receipts = RingBuffer::new(NonZeroUsize::new(16).unwrap());
        // And no new receipt,
        let new_receipt_event = None;
        // And an active receipt pointing at $2,
        let active_receipt = Some(event_id!("$2"));
        let with_threading_support = false;

        // Then the best receipt is still $2.
        let receipt = select_best_receipt(
            own_user_id,
            &lc,
            &mut pending_receipts,
            new_receipt_event,
            active_receipt,
            with_threading_support,
        );
        assert_eq!(receipt.unwrap(), event_id!("$2"));
        // And there are no pending receipts.
        assert!(pending_receipts.is_empty());
    }

    #[test]
    fn test_select_best_receipt_new_receipt_event() {
        let room_id = room_id!("!roomid:example.org");
        let f = EventFactory::new().room(room_id).sender(*ALICE);
        let own_user_id = user_id!("@not_alice:example.org");

        // Create a non-empty linked chunk, with no messages sent by the current user.
        let mut lc = EventLinkedChunk::new();
        lc.push_events(vec![
            f.text_msg("Event 1").event_id(event_id!("$1")).into_event(),
            f.text_msg("Event 2").event_id(event_id!("$2")).into_event(),
            f.text_msg("Event 3").event_id(event_id!("$3")).into_event(),
        ]);

        // When there are no pending receipts,
        let mut pending_receipts = RingBuffer::new(NonZeroUsize::new(16).unwrap());

        // And a new receipt event which points to $2,
        let new_receipt_event = Some(
            f.read_receipts()
                .add(event_id!("$2"), own_user_id, ReceiptType::Read, ReceiptThread::Unthreaded)
                .into_content(),
        );

        // And no active receipt,
        let active_receipt = None;
        let with_threading_support = false;

        // Then there's a new best receipt, which is the explicit one from the event
        let receipt = select_best_receipt(
            own_user_id,
            &lc,
            &mut pending_receipts,
            new_receipt_event.as_ref(),
            active_receipt,
            with_threading_support,
        );
        assert_eq!(receipt.unwrap(), event_id!("$2"));
        // And there are no pending receipts.
        assert!(pending_receipts.is_empty());
    }

    #[test]
    fn test_select_best_receipt_stashes_pending_receipts() {
        let room_id = room_id!("!roomid:example.org");
        let f = EventFactory::new().room(room_id).sender(*ALICE);
        let own_user_id = user_id!("@not_alice:example.org");

        // Create a non-empty linked chunk, with no messages sent by the current user.
        let mut lc = EventLinkedChunk::new();
        lc.push_events(vec![
            f.text_msg("Event 1").event_id(event_id!("$1")).into_event(),
            f.text_msg("Event 2").event_id(event_id!("$2")).into_event(),
            f.text_msg("Event 3").event_id(event_id!("$3")).into_event(),
        ]);

        // When there are no pending receipts,
        let mut pending_receipts = RingBuffer::new(NonZeroUsize::new(16).unwrap());

        // And a new receipt event, for an event we don't know about,
        let new_receipt_event = Some(
            f.read_receipts()
                .add(event_id!("$4"), own_user_id, ReceiptType::Read, ReceiptThread::Unthreaded)
                .into_content(),
        );

        // And no active receipt,
        let active_receipt = None;
        let with_threading_support = false;

        // Then there's no new best receipts.
        let receipt = select_best_receipt(
            own_user_id,
            &lc,
            &mut pending_receipts,
            new_receipt_event.as_ref(),
            active_receipt,
            with_threading_support,
        );

        assert!(receipt.is_none());
        // And there's a new pending receipt for $4.
        assert_eq!(pending_receipts.len(), 1);
        assert_eq!(pending_receipts.get(0).unwrap(), event_id!("$4"));
    }

    #[test]
    fn test_select_best_receipt_matched_pending_receipt() {
        let room_id = room_id!("!roomid:example.org");
        let f = EventFactory::new().room(room_id).sender(*ALICE);
        let own_user_id = user_id!("@not_alice:example.org");

        // Create a non-empty linked chunk, with no messages sent by the current user.
        let mut lc = EventLinkedChunk::new();
        lc.push_events(vec![
            f.text_msg("Event 1").event_id(event_id!("$1")).into_event(),
            f.text_msg("Event 2").event_id(event_id!("$2")).into_event(),
            f.text_msg("Event 3").event_id(event_id!("$3")).into_event(),
        ]);

        // When there is a pending receipt for $2,
        let mut pending_receipts = RingBuffer::new(NonZeroUsize::new(16).unwrap());
        pending_receipts.push(owned_event_id!("$2"));

        // And no new receipt event,
        let new_receipt_event = None;

        // And no active receipt,
        let active_receipt = None;
        let with_threading_support = false;

        // Then there's a new best receipt, which is the matched pending receipt.
        let receipt = select_best_receipt(
            own_user_id,
            &lc,
            &mut pending_receipts,
            new_receipt_event.as_ref(),
            active_receipt,
            with_threading_support,
        );
        assert_eq!(receipt.unwrap(), event_id!("$2"));
        // And there are no more pending receipts.
        assert!(pending_receipts.is_empty());
    }

    #[test]
    fn test_select_best_receipt_mixed() {
        let room_id = room_id!("!roomid:example.org");
        let f = EventFactory::new().room(room_id).sender(*ALICE);
        let own_user_id = user_id!("@not_alice:example.org");

        // Create a non-empty linked chunk, with one message sent by the current user,
        // which will act as an implicit read receipt.
        let mut lc = EventLinkedChunk::new();
        lc.push_events(vec![
            f.text_msg("Event 1").event_id(event_id!("$1")).into_event(),
            f.text_msg("Event 2").event_id(event_id!("$2")).into_event(),
            f.text_msg("Event 3").event_id(event_id!("$3")).sender(own_user_id).into_event(),
            f.text_msg("Event 4").event_id(event_id!("$4")).into_event(),
            f.text_msg("Event 5").event_id(event_id!("$5")).into_event(),
        ]);

        // When there is a pending receipt for $2, and $6,
        let mut pending_receipts = RingBuffer::new(NonZeroUsize::new(16).unwrap());
        pending_receipts.push(owned_event_id!("$2"));
        pending_receipts.push(owned_event_id!("$6"));

        // And a new receipt event pointing at $4 and $6,
        let new_receipt_event = Some(
            f.read_receipts()
                .add(event_id!("$4"), own_user_id, ReceiptType::Read, ReceiptThread::Unthreaded)
                .add(event_id!("$7"), own_user_id, ReceiptType::ReadPrivate, ReceiptThread::Main)
                .into_content(),
        );

        // And an active receipt point at $1,
        let active_receipt = Some(event_id!("$1"));

        let with_threading_support = false;

        // Then there's a new best receipt, which is the most advanced in the linked
        // chunk: $4.
        let receipt = select_best_receipt(
            own_user_id,
            &lc,
            &mut pending_receipts,
            new_receipt_event.as_ref(),
            active_receipt,
            with_threading_support,
        );
        assert_eq!(receipt.unwrap(), event_id!("$4"));

        // Receipt 6 is still pending, and there's a new pending receipt for 7 too. ($2
        // has been cleaned because it has been seen).
        assert_eq!(pending_receipts.len(), 2);
        assert!(pending_receipts.iter().any(|ev| ev == event_id!("$6")));
        assert!(pending_receipts.iter().any(|ev| ev == event_id!("$7")));
    }
}
