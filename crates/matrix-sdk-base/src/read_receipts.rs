// Copyright 2023 The Matrix.org Foundation C.I.C.
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
//! messages, unread notifications and unread highlights in a room.
//!
//! Counting unread messages is performed by looking at the latest receipt of
//! the current user, and inferring which events are following it, according to
//! the sync ordering.
//!
//! For notifications and highlights to be precisely accounted for, we also need
//! to pay attention to the user's notification settings. Fortunately, this is
//! also something we need to for notifications, so we can reuse this code.
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
//! We do have an unbounded, in-memory cache for sync events, as part of sliding
//! sync. It's reset as soon as we get a "limited" (gappy) sync for a room. Not
//! as ideal as an on-disk timeline, but it's sufficient to do some interesting
//! computations already.
//!
//! ### How-to
//!
//! When we call `compute_unread_counts`, that's for one of two reasons (and
//! maybe both at once, or maybe none at all):
//! - we received a new receipt
//! - new events came in.
//!
//! A read receipt is considered _active_ if it's been received from sync
//! *and* it matches a known event in the in-memory sync events cache.
//!
//! The *latest active* receipt is the one that's active, with the latest order
//! (according to sync ordering, aka position in the sync cache).
//!
//! The problem of keeping a precise read count is thus equivalent to finding
//! the latest active receipt, and counting interesting events after it (in the
//! sync ordering).
//!
//! When we get new events, we'll incorporate them into an inverse mapping of
//! event id -> sync order (`event_id_to_pos`). This gives us a simple way to
//! select a "better" active receipt, using the `ReceiptSelector`. An event that
//! has a read receipt can be passed to `ReceiptSelector::try_select_later`,
//! which compares the order of the current best active, to that of the new
//! event, and records the better one, if applicable.
//!
//! When we receive a new receipt event in
//! `ReceiptSelector::handle_new_receipt`, if we find a {public|private}
//! {main-threaded|unthreaded} receipt attached to an event, there are two
//! possibilities:
//! - we knew the event, so we can immediately try to select it as a better
//!   event with `try_select_later`,
//! - or we don't, which may mean the receipt refers to a past event we lost
//!   track of (because of a restart of the app — remember the cache is mostly
//!   memory-only, and a few items on disk), or the receipt refers to a future
//!   event. To cover for the latter possibility, we stash the receipt and mark
//!   it as pending (we only keep a limited number of pending read receipts
//!   using a `RingBuffer`).
//!
//! That means that when we receive new events, we'll check if their id matches
//! one of the pending receipts in `handle_pending_receipts`; if so, we can
//! remove it from the pending set, and try to consider it a better receipt with
//! `try_select_later`. If not, it's still pending, until it'll be forgotten or
//! matched.
//!
//! Once we have a new *better active receipt*, we'll save it in the
//! `RoomReadReceipt` data (stored in `RoomInfo`), and we'll compute the counts,
//! starting from the event the better active receipt was referring to.
//!
//! If we *don't* have a better active receipt, that means that all the events
//! received in that sync batch aren't referred to by a known read receipt,
//! _and_ we didn't get a new better receipt that matched known events. In that
//! case, we can just consider that all the events are new, and count them as
//! such.
//!
//! ### Edge cases
//!
//! - `compute_unread_counts` is called after receiving a sliding sync response,
//!   at a time where we haven't tried to "reconcile" the cached timeline items
//!   with the new ones. The only kind of reconciliation we'd do anyways is
//!   clearing the timeline if it was limited, which equates to having common
//!   events ids in both sets. As a matter of fact, we have to manually handle
//!   this edge case here. I hope that having an event database will help avoid
//!   this kind of workaround here later.
//! - In addition to that, and as noted in the timeline code, it seems that
//!   sliding sync could return the same event multiple times in a sync
//!   timeline, leading to incorrect results. We have to take that into account
//!   by resetting the read counts *every* time we see an event that was the
//!   target of the latest active read receipt.
#![allow(dead_code)] // too many different build configurations, I give up

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
};

use matrix_sdk_common::{
    deserialized_responses::TimelineEvent, ring_buffer::RingBuffer,
    serde_helpers::extract_thread_root,
};
use ruma::{
    EventId, OwnedEventId, OwnedUserId, RoomId, UserId,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, OriginalSyncMessageLikeEvent,
        SyncMessageLikeEvent,
        poll::{start::PollStartEventContent, unstable_start::UnstablePollStartEventContent},
        receipt::{ReceiptEventContent, ReceiptThread, ReceiptType},
        room::message::Relation,
    },
    serde::Raw,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument, trace, warn};

use crate::ThreadingSupport;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct LatestReadReceipt {
    /// The id of the event the read receipt is referring to. (Not the read
    /// receipt event id.)
    event_id: OwnedEventId,
}

/// Public data about read receipts collected during processing of that room.
///
/// Remember that each time a field of `RoomReadReceipts` is updated in
/// `compute_unread_counts`, this function must return true!
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RoomReadReceipts {
    /// Does the room have unread messages?
    pub num_unread: u64,

    /// Does the room have unread events that should notify?
    pub num_notifications: u64,

    /// Does the room have messages causing highlights for the users? (aka
    /// mentions)
    pub num_mentions: u64,

    /// The latest read receipt (main-threaded or unthreaded) known for the
    /// room.
    #[serde(default)]
    latest_active: Option<LatestReadReceipt>,

    /// Read receipts that haven't been matched to their event.
    ///
    /// This might mean that the read receipt is in the past further than we
    /// recall (i.e. before the first event we've ever cached), or in the
    /// future (i.e. the event is lagging behind because of federation).
    ///
    /// Note: this contains event ids of the event *targets* of the receipts,
    /// not the event ids of the receipt events themselves.
    #[serde(default = "new_nonempty_ring_buffer")]
    pending: RingBuffer<OwnedEventId>,
}

impl Default for RoomReadReceipts {
    fn default() -> Self {
        Self {
            num_unread: Default::default(),
            num_notifications: Default::default(),
            num_mentions: Default::default(),
            latest_active: Default::default(),
            pending: new_nonempty_ring_buffer(),
        }
    }
}

fn new_nonempty_ring_buffer() -> RingBuffer<OwnedEventId> {
    // 10 pending read receipts per room should be enough for everyone.
    // SAFETY: `unwrap` is safe because 10 is not zero.
    RingBuffer::new(NonZeroUsize::new(10).unwrap())
}

impl RoomReadReceipts {
    /// Update the [`RoomReadReceipts`] unread counts according to the new
    /// event.
    ///
    /// Returns whether a new event triggered a new unread/notification/mention.
    #[inline(always)]
    fn process_event(
        &mut self,
        event: &TimelineEvent,
        user_id: &UserId,
        threading_support: ThreadingSupport,
    ) {
        if matches!(threading_support, ThreadingSupport::Enabled { .. })
            && extract_thread_root(event.raw()).is_some()
        {
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
        threading_support: ThreadingSupport,
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
                self.process_event(event, user_id, threading_support);
            }
        }

        counting_receipts
    }
}

/// Small helper to select the "best" receipt (that with the biggest sync
/// order).
struct ReceiptSelector {
    /// Mapping of known event IDs to their sync order.
    event_id_to_pos: BTreeMap<OwnedEventId, usize>,
    /// The event with the greatest sync order, for which we had a read-receipt,
    /// so far.
    latest_event_with_receipt: Option<OwnedEventId>,
    /// The biggest sync order attached to the `best_receipt`.
    latest_event_pos: Option<usize>,
}

impl ReceiptSelector {
    fn new(all_events: &[TimelineEvent], latest_active_receipt_event: Option<&EventId>) -> Self {
        let event_id_to_pos = Self::create_sync_index(all_events.iter());

        let best_pos =
            latest_active_receipt_event.and_then(|event_id| event_id_to_pos.get(event_id)).copied();

        // Note: `best_receipt` isn't initialized to the latest active receipt, if set,
        // so that `finish` will return only *new* better receipts, making it
        // possible to take the fast path in `compute_unread_counts` where every
        // event is considered new.
        Self { latest_event_pos: best_pos, latest_event_with_receipt: None, event_id_to_pos }
    }

    /// Create a mapping of `event_id` -> sync order for all events that have an
    /// `event_id`.
    fn create_sync_index<'a>(
        events: impl Iterator<Item = &'a TimelineEvent> + 'a,
    ) -> BTreeMap<OwnedEventId, usize> {
        // TODO: this should be cached and incrementally updated.
        BTreeMap::from_iter(
            events
                .enumerate()
                .filter_map(|(pos, event)| event.event_id().map(|event_id| (event_id, pos))),
        )
    }

    /// Consider the current event and its position as a better read receipt.
    #[instrument(skip(self), fields(prev_pos = ?self.latest_event_pos, prev_receipt = ?self.latest_event_with_receipt))]
    fn try_select_later(&mut self, event_id: &EventId, event_pos: usize) {
        // We now have a position for an event that had a read receipt, but wasn't found
        // before. Consider if it is the most recent now.
        if let Some(best_pos) = self.latest_event_pos.as_mut() {
            // Note: by using a lax comparison here, we properly handle the case where we
            // received events that we have already seen with a persisted read
            // receipt.
            if event_pos >= *best_pos {
                *best_pos = event_pos;
                self.latest_event_with_receipt = Some(event_id.to_owned());
                debug!("saving better");
            } else {
                trace!("not better, keeping previous");
            }
        } else {
            // We didn't have a previous receipt, this is the first one we
            // store: remember it.
            self.latest_event_pos = Some(event_pos);
            self.latest_event_with_receipt = Some(event_id.to_owned());
            debug!("saving for the first time");
        }
    }

    /// Try to match pending receipts against new events.
    #[instrument(skip_all)]
    fn handle_pending_receipts(&mut self, pending: &mut RingBuffer<OwnedEventId>) {
        // Try to match stashed receipts against the new events.
        pending.retain(|event_id| {
            if let Some(event_pos) = self.event_id_to_pos.get(event_id) {
                // Maybe select this read receipt as it might be better than the ones we had.
                trace!(%event_id, "matching event against its stashed receipt");
                self.try_select_later(event_id, *event_pos);

                // Remove this stashed read receipt from the pending list, as it's been
                // reconciled with its event.
                false
            } else {
                // Keep it for further iterations.
                true
            }
        });
    }

    /// Try to match the receipts inside a receipt event against any of the
    /// events we know about.
    ///
    /// If we find a receipt (for the current user) for an event we know, call
    /// `try_select_later` to see whether this is our new latest receipted
    /// event.
    ///
    /// Returns any receipts (for the current user) that we could not match
    /// against any event - these are "pending".
    #[instrument(skip_all)]
    fn handle_new_receipt(
        &mut self,
        user_id: &UserId,
        receipt_event: &ReceiptEventContent,
    ) -> Vec<OwnedEventId> {
        let mut pending = Vec::new();
        // Now consider new receipts.
        for (event_id, receipts) in &receipt_event.0 {
            for ty in [ReceiptType::Read, ReceiptType::ReadPrivate] {
                if let Some(receipts) = receipts.get(&ty)
                    && let Some(receipt) = receipts.get(user_id)
                    && matches!(receipt.thread, ReceiptThread::Main | ReceiptThread::Unthreaded)
                {
                    trace!(%event_id, "found new candidate");
                    if let Some(event_pos) = self.event_id_to_pos.get(event_id) {
                        self.try_select_later(event_id, *event_pos);
                    } else {
                        // It's a new pending receipt.
                        trace!(%event_id, "stashed as pending");
                        pending.push(event_id.clone());
                    }
                }
            }
        }
        pending
    }

    /// Try to match an implicit receipt, that is, the one we get for events we
    /// sent ourselves.
    #[instrument(skip_all)]
    fn try_match_implicit(&mut self, user_id: &UserId, new_events: &[TimelineEvent]) {
        for ev in new_events {
            // Get the `sender` field, if any, or skip this event.
            let Ok(Some(sender)) = ev.raw().get_field::<OwnedUserId>("sender") else { continue };
            if sender == user_id {
                // Get the event id, if any, or skip this event.
                let Some(event_id) = ev.event_id() else { continue };
                if let Some(event_pos) = self.event_id_to_pos.get(&event_id) {
                    trace!(%event_id, "found an implicit receipt candidate");
                    self.try_select_later(&event_id, *event_pos);
                }
            }
        }
    }

    /// Returns the event id referred to by a new later active read receipt.
    ///
    /// If it's not set, we can consider that each new event is *after* the
    /// previous active read receipt.
    fn select(self) -> Option<LatestReadReceipt> {
        self.latest_event_with_receipt.map(|event_id| LatestReadReceipt { event_id })
    }
}

/// Returns true if there's an event common to both groups of events, based on
/// their event id.
fn events_intersects<'a>(
    previous_events: impl Iterator<Item = &'a TimelineEvent>,
    new_events: &[TimelineEvent],
) -> bool {
    let previous_events_ids = BTreeSet::from_iter(previous_events.filter_map(|ev| ev.event_id()));
    new_events
        .iter()
        .any(|ev| ev.event_id().is_some_and(|event_id| previous_events_ids.contains(&event_id)))
}

/// Given a set of events coming from sync, for a room, update the
/// [`RoomReadReceipts`]'s counts of unread messages, notifications and
/// highlights' in place.
///
/// A provider of previous events may be required to reconcile a read receipt
/// that has been just received for an event that came in a previous sync.
///
/// See this module's documentation for more information.
#[instrument(skip_all, fields(room_id = %room_id))]
pub(crate) fn compute_unread_counts(
    user_id: &UserId,
    room_id: &RoomId,
    receipt_event: Option<&ReceiptEventContent>,
    mut previous_events: Vec<TimelineEvent>,
    new_events: &[TimelineEvent],
    read_receipts: &mut RoomReadReceipts,
    threading_support: ThreadingSupport,
) {
    debug!(?read_receipts, "Starting");

    let all_events = if events_intersects(previous_events.iter(), new_events) {
        // The previous and new events sets can intersect, for instance if we restored
        // previous events from the disk cache, or a timeline was limited. This
        // means the old events will be cleared, because we don't reconcile
        // timelines in the event cache (yet). As a result, forget
        // about the previous events.
        new_events.to_owned()
    } else {
        previous_events.extend(new_events.iter().cloned());
        previous_events
    };

    let new_receipt = {
        let mut selector = ReceiptSelector::new(
            &all_events,
            read_receipts.latest_active.as_ref().map(|receipt| &*receipt.event_id),
        );

        selector.try_match_implicit(user_id, new_events);
        selector.handle_pending_receipts(&mut read_receipts.pending);
        if let Some(receipt_event) = receipt_event {
            let new_pending = selector.handle_new_receipt(user_id, receipt_event);
            if !new_pending.is_empty() {
                read_receipts.pending.extend(new_pending);
            }
        }
        selector.select()
    };

    if let Some(new_receipt) = new_receipt {
        // We've found the id of an event to which the receipt attaches. The associated
        // event may either come from the new batch of events associated to
        // this sync, or it may live in the past timeline events we know
        // about.

        let event_id = new_receipt.event_id.clone();

        // First, save the event id as the latest one that has a read receipt.
        trace!(%event_id, "Saving a new active read receipt");
        read_receipts.latest_active = Some(new_receipt);

        // The event for the receipt is in `all_events`, so we'll find it and can count
        // safely from here.
        read_receipts.find_and_process_events(
            &event_id,
            user_id,
            all_events.iter(),
            threading_support,
        );

        debug!(?read_receipts, "after finding a better receipt");
        return;
    }

    // If we haven't returned at this point, it means we don't have any new "active"
    // read receipt. So either there was a previous one further in the past, or
    // none.
    //
    // In that case, accumulate all events as part of the current batch, and wait
    // for the next receipt.

    for event in new_events {
        read_receipts.process_event(event, user_id, threading_support);
    }

    debug!(?read_receipts, "no better receipt, {} new events", new_events.len());
}

/// Is the event worth marking a room as unread?
fn marks_as_unread(event: &Raw<AnySyncTimelineEvent>, user_id: &UserId) -> bool {
    let event = match event.deserialize() {
        Ok(event) => event,
        Err(err) => {
            warn!(
                "couldn't deserialize event {:?}: {err}",
                event.get_field::<String>("event_id").ok().flatten()
            );
            return false;
        }
    };

    if event.sender() == user_id {
        // Not interested in one's own events.
        return false;
    }

    match event {
        AnySyncTimelineEvent::MessageLike(event) => {
            // Filter out redactions.
            let Some(content) = event.original_content() else {
                tracing::trace!("not interesting because redacted");
                return false;
            };

            // Filter out edits.
            if matches!(
                content.relation(),
                Some(ruma::events::room::encrypted::Relation::Replacement(..))
            ) {
                tracing::trace!("not interesting because edited");
                return false;
            }

            match event {
                AnySyncMessageLikeEvent::CallAnswer(_)
                | AnySyncMessageLikeEvent::CallInvite(_)
                | AnySyncMessageLikeEvent::RtcNotification(_)
                | AnySyncMessageLikeEvent::CallHangup(_)
                | AnySyncMessageLikeEvent::CallCandidates(_)
                | AnySyncMessageLikeEvent::CallNegotiate(_)
                | AnySyncMessageLikeEvent::CallReject(_)
                | AnySyncMessageLikeEvent::CallSelectAnswer(_)
                | AnySyncMessageLikeEvent::PollResponse(_)
                | AnySyncMessageLikeEvent::UnstablePollResponse(_)
                | AnySyncMessageLikeEvent::Reaction(_)
                | AnySyncMessageLikeEvent::RoomRedaction(_)
                | AnySyncMessageLikeEvent::KeyVerificationStart(_)
                | AnySyncMessageLikeEvent::KeyVerificationReady(_)
                | AnySyncMessageLikeEvent::KeyVerificationCancel(_)
                | AnySyncMessageLikeEvent::KeyVerificationAccept(_)
                | AnySyncMessageLikeEvent::KeyVerificationDone(_)
                | AnySyncMessageLikeEvent::KeyVerificationMac(_)
                | AnySyncMessageLikeEvent::KeyVerificationKey(_) => false,

                // For some reason, Ruma doesn't handle these two in `content.relation()` above.
                AnySyncMessageLikeEvent::PollStart(SyncMessageLikeEvent::Original(
                    OriginalSyncMessageLikeEvent {
                        content:
                            PollStartEventContent { relates_to: Some(Relation::Replacement(_)), .. },
                        ..
                    },
                ))
                | AnySyncMessageLikeEvent::UnstablePollStart(SyncMessageLikeEvent::Original(
                    OriginalSyncMessageLikeEvent {
                        content: UnstablePollStartEventContent::Replacement(_),
                        ..
                    },
                )) => false,

                AnySyncMessageLikeEvent::Message(_)
                | AnySyncMessageLikeEvent::PollStart(_)
                | AnySyncMessageLikeEvent::UnstablePollStart(_)
                | AnySyncMessageLikeEvent::PollEnd(_)
                | AnySyncMessageLikeEvent::UnstablePollEnd(_)
                | AnySyncMessageLikeEvent::RoomEncrypted(_)
                | AnySyncMessageLikeEvent::RoomMessage(_)
                | AnySyncMessageLikeEvent::Sticker(_) => true,

                _ => {
                    // What I don't know about, I don't care about.
                    warn!("unhandled timeline event type: {}", event.event_type());
                    false
                }
            }
        }

        AnySyncTimelineEvent::State(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, ops::Not as _};

    use matrix_sdk_common::{deserialized_responses::TimelineEvent, ring_buffer::RingBuffer};
    use matrix_sdk_test::event_factory::EventFactory;
    use ruma::{
        EventId, UserId, event_id,
        events::{
            receipt::{ReceiptThread, ReceiptType},
            room::{member::MembershipState, message::MessageType},
        },
        owned_event_id, owned_user_id,
        push::Action,
        room_id, user_id,
    };

    use super::compute_unread_counts;
    use crate::{
        ThreadingSupport,
        read_receipts::{ReceiptSelector, RoomReadReceipts, marks_as_unread},
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

        // An interesting event from oneself doesn't count as a new unread message.
        let event = make_event(user_id, Vec::new());
        let mut receipts = RoomReadReceipts::default();
        receipts.process_event(&event, user_id, ThreadingSupport::Disabled);
        assert_eq!(receipts.num_unread, 0);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 0);

        // An interesting event from someone else does count as a new unread message.
        let event = make_event(user_id!("@bob:example.org"), Vec::new());
        let mut receipts = RoomReadReceipts::default();
        receipts.process_event(&event, user_id, ThreadingSupport::Disabled);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 0);

        // Push actions computed beforehand are respected.
        let event = make_event(user_id!("@bob:example.org"), vec![Action::Notify]);
        let mut receipts = RoomReadReceipts::default();
        receipts.process_event(&event, user_id, ThreadingSupport::Disabled);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 1);

        let event = make_event(
            user_id!("@bob:example.org"),
            vec![Action::SetTweak(ruma::push::Tweak::Highlight(true))],
        );
        let mut receipts = RoomReadReceipts::default();
        receipts.process_event(&event, user_id, ThreadingSupport::Disabled);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 1);
        assert_eq!(receipts.num_notifications, 0);

        let event = make_event(
            user_id!("@bob:example.org"),
            vec![Action::SetTweak(ruma::push::Tweak::Highlight(true)), Action::Notify],
        );
        let mut receipts = RoomReadReceipts::default();
        receipts.process_event(&event, user_id, ThreadingSupport::Disabled);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 1);
        assert_eq!(receipts.num_notifications, 1);

        // Technically this `push_actions` set would be a bug somewhere else, but let's
        // make sure to resist against it.
        let event = make_event(user_id!("@bob:example.org"), vec![Action::Notify, Action::Notify]);
        let mut receipts = RoomReadReceipts::default();
        receipts.process_event(&event, user_id, ThreadingSupport::Disabled);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 1);
    }

    #[test]
    fn test_find_and_process_events() {
        let ev0 = event_id!("$0");
        let user_id = user_id!("@alice:example.org");

        // When provided with no events, we report not finding the event to which the
        // receipt relates.
        let mut receipts = RoomReadReceipts::default();
        assert!(
            receipts.find_and_process_events(ev0, user_id, &[], ThreadingSupport::Disabled).not()
        );
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
                    ThreadingSupport::Disabled
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
        assert!(receipts.find_and_process_events(
            ev0,
            user_id,
            &[make_event(ev0)],
            ThreadingSupport::Disabled
        ),);
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
                    ThreadingSupport::Disabled
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
            ThreadingSupport::Disabled
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
            ThreadingSupport::Disabled
        ));
        assert_eq!(receipts.num_unread, 2);
        assert_eq!(receipts.num_notifications, 0);
        assert_eq!(receipts.num_mentions, 0);
    }

    /// Smoke test for `compute_unread_counts`.
    #[test]
    fn test_basic_compute_unread_counts() {
        let user_id = user_id!("@alice:example.org");
        let other_user_id = user_id!("@bob:example.org");
        let room_id = room_id!("!room:example.org");
        let receipt_event_id = event_id!("$1");

        let mut previous_events = Vec::new();

        let f = EventFactory::new();
        let ev1 = f.text_msg("A").sender(other_user_id).event_id(receipt_event_id).into_event();
        let ev2 = f.text_msg("A").sender(other_user_id).event_id(event_id!("$2")).into_event();

        let receipt_event = f
            .read_receipts()
            .add(receipt_event_id, user_id, ReceiptType::Read, ReceiptThread::Unthreaded)
            .into_content();

        let mut read_receipts = RoomReadReceipts::default();
        compute_unread_counts(
            user_id,
            room_id,
            Some(&receipt_event),
            previous_events.clone(),
            &[ev1.clone(), ev2.clone()],
            &mut read_receipts,
            ThreadingSupport::Disabled,
        );

        // It did find the receipt event (ev1).
        assert_eq!(read_receipts.num_unread, 1);

        // Receive the same receipt event, with a new sync event.
        previous_events.push(ev1);
        previous_events.push(ev2);

        let new_event =
            f.text_msg("A").sender(other_user_id).event_id(event_id!("$3")).into_event();
        compute_unread_counts(
            user_id,
            room_id,
            Some(&receipt_event),
            previous_events,
            &[new_event],
            &mut read_receipts,
            ThreadingSupport::Disabled,
        );

        // Only the new event should be added.
        assert_eq!(read_receipts.num_unread, 2);
    }

    fn make_test_events(user_id: &UserId) -> Vec<TimelineEvent> {
        let f = EventFactory::new().sender(user_id);
        let ev1 = f.text_msg("With the lights out, it's less dangerous").event_id(event_id!("$1"));
        let ev2 = f.text_msg("Here we are now, entertain us").event_id(event_id!("$2"));
        let ev3 = f.text_msg("I feel stupid and contagious").event_id(event_id!("$3"));
        let ev4 = f.text_msg("Here we are now, entertain us").event_id(event_id!("$4"));
        let ev5 = f.text_msg("Hello, hello, hello, how low?").event_id(event_id!("$5"));
        [ev1, ev2, ev3, ev4, ev5].into_iter().map(Into::into).collect()
    }

    /// Test that when multiple receipts come in a single event, we can still
    /// find the latest one according to the sync order.
    #[test]
    fn test_compute_unread_counts_multiple_receipts_in_one_event() {
        let user_id = user_id!("@alice:example.org");
        let room_id = room_id!("!room:example.org");

        let all_events = make_test_events(user_id!("@bob:example.org"));
        let head_events: Vec<_> = all_events.iter().take(2).cloned().collect();
        let tail_events: Vec<_> = all_events.iter().skip(2).cloned().collect();

        // Given a receipt event marking events 1-3 as read using a combination of
        // different thread and privacy types,
        let f = EventFactory::new();
        for receipt_type_1 in &[ReceiptType::Read, ReceiptType::ReadPrivate] {
            for receipt_thread_1 in &[ReceiptThread::Unthreaded, ReceiptThread::Main] {
                for receipt_type_2 in &[ReceiptType::Read, ReceiptType::ReadPrivate] {
                    for receipt_thread_2 in &[ReceiptThread::Unthreaded, ReceiptThread::Main] {
                        let receipt_event = f
                            .read_receipts()
                            .add(
                                event_id!("$2"),
                                user_id,
                                receipt_type_1.clone(),
                                receipt_thread_1.clone(),
                            )
                            .add(
                                event_id!("$3"),
                                user_id,
                                receipt_type_2.clone(),
                                receipt_thread_2.clone(),
                            )
                            .add(
                                event_id!("$1"),
                                user_id,
                                receipt_type_1.clone(),
                                receipt_thread_2.clone(),
                            )
                            .into_content();

                        // When I compute the notifications for this room (with no new events),
                        let mut read_receipts = RoomReadReceipts::default();

                        compute_unread_counts(
                            user_id,
                            room_id,
                            Some(&receipt_event),
                            all_events.clone(),
                            &[],
                            &mut read_receipts,
                            ThreadingSupport::Disabled,
                        );

                        assert!(
                            read_receipts != Default::default(),
                            "read receipts have been updated"
                        );

                        // Then events 1-3 are considered read, but 4 and 5 are not.
                        assert_eq!(read_receipts.num_unread, 2);
                        assert_eq!(read_receipts.num_mentions, 0);
                        assert_eq!(read_receipts.num_notifications, 0);

                        // And when I compute notifications again, with some old and new events,
                        let mut read_receipts = RoomReadReceipts::default();
                        compute_unread_counts(
                            user_id,
                            room_id,
                            Some(&receipt_event),
                            head_events.clone(),
                            &tail_events,
                            &mut read_receipts,
                            ThreadingSupport::Disabled,
                        );

                        assert!(
                            read_receipts != Default::default(),
                            "read receipts have been updated"
                        );

                        // Then events 1-3 are considered read, but 4 and 5 are not.
                        assert_eq!(read_receipts.num_unread, 2);
                        assert_eq!(read_receipts.num_mentions, 0);
                        assert_eq!(read_receipts.num_notifications, 0);
                    }
                }
            }
        }
    }

    /// Updating the pending list should cause a change in the
    /// `RoomReadReceipts` fields, and `compute_unread_counts` should return
    /// true then.
    #[test]
    fn test_compute_unread_counts_updated_after_field_tracking() {
        let user_id = owned_user_id!("@alice:example.org");
        let room_id = room_id!("!room:example.org");

        let events = make_test_events(user_id!("@bob:example.org"));

        let receipt_event = EventFactory::new()
            .read_receipts()
            .add(event_id!("$6"), &user_id, ReceiptType::Read, ReceiptThread::Unthreaded)
            .into_content();

        let mut read_receipts = RoomReadReceipts::default();
        assert!(read_receipts.pending.is_empty());

        // Given a receipt event that contains a read receipt referring to an unknown
        // event, and some preexisting events with different ids,
        compute_unread_counts(
            &user_id,
            room_id,
            Some(&receipt_event),
            events,
            &[], // no new events
            &mut read_receipts,
            ThreadingSupport::Disabled,
        );

        // Then there are no unread events,
        assert_eq!(read_receipts.num_unread, 0);

        // And the event referred to by the read receipt is in the pending state.
        assert_eq!(read_receipts.pending.len(), 1);
        assert!(read_receipts.pending.iter().any(|ev| ev == event_id!("$6")));
    }

    #[test]
    fn test_compute_unread_counts_limited_sync() {
        let user_id = owned_user_id!("@alice:example.org");
        let room_id = room_id!("!room:example.org");

        let events = make_test_events(user_id!("@bob:example.org"));

        let receipt_event = EventFactory::new()
            .read_receipts()
            .add(event_id!("$1"), &user_id, ReceiptType::Read, ReceiptThread::Unthreaded)
            .into_content();

        // Sync with a read receipt *and* a single event that was already known: in that
        // case, only consider the new events in isolation, and compute the
        // correct count.
        let mut read_receipts = RoomReadReceipts::default();
        assert!(read_receipts.pending.is_empty());

        let ev0 = events[0].clone();

        compute_unread_counts(
            &user_id,
            room_id,
            Some(&receipt_event),
            events,
            &[ev0], // duplicate event!
            &mut read_receipts,
            ThreadingSupport::Disabled,
        );

        // All events are unread, and there's no pending receipt.
        assert_eq!(read_receipts.num_unread, 0);
        assert!(read_receipts.pending.is_empty());
    }

    #[test]
    fn test_receipt_selector_create_sync_index() {
        let uid = user_id!("@bob:example.org");

        let events = make_test_events(uid);

        // An event with no id.
        let ev6 = EventFactory::new().text_msg("yolo").sender(uid).no_event_id().into_event();

        let index = ReceiptSelector::create_sync_index(events.iter().chain(&[ev6]));

        assert_eq!(*index.get(event_id!("$1")).unwrap(), 0);
        assert_eq!(*index.get(event_id!("$2")).unwrap(), 1);
        assert_eq!(*index.get(event_id!("$3")).unwrap(), 2);
        assert_eq!(*index.get(event_id!("$4")).unwrap(), 3);
        assert_eq!(*index.get(event_id!("$5")).unwrap(), 4);
        assert_eq!(index.get(event_id!("$6")), None);

        assert_eq!(index.len(), 5);

        // Sync order are set according to the position in the vector.
        let index = ReceiptSelector::create_sync_index(
            [events[1].clone(), events[2].clone(), events[4].clone()].iter(),
        );

        assert_eq!(*index.get(event_id!("$2")).unwrap(), 0);
        assert_eq!(*index.get(event_id!("$3")).unwrap(), 1);
        assert_eq!(*index.get(event_id!("$5")).unwrap(), 2);

        assert_eq!(index.len(), 3);
    }

    #[test]
    fn test_receipt_selector_try_select_later() {
        let events = make_test_events(user_id!("@bob:example.org"));

        {
            // No initial active receipt, so the first receipt we get *will* win.
            let mut selector = ReceiptSelector::new(&[], None);
            selector.try_select_later(event_id!("$1"), 0);
            let best_receipt = selector.select();
            assert_eq!(best_receipt.unwrap().event_id, event_id!("$1"));
        }

        {
            // $3 is at pos 2, $1 at position 0, so $3 wins => no new change.
            let mut selector = ReceiptSelector::new(&events, Some(event_id!("$3")));
            selector.try_select_later(event_id!("$1"), 0);
            let best_receipt = selector.select();
            assert!(best_receipt.is_none());
        }

        {
            // The initial active receipt is returned, when it's part of the scanned
            // elements.
            let mut selector = ReceiptSelector::new(&events, Some(event_id!("$1")));
            selector.try_select_later(event_id!("$1"), 0);
            let best_receipt = selector.select();
            assert_eq!(best_receipt.unwrap().event_id, event_id!("$1"));
        }

        {
            // $3 is at pos 2, $4 at position 3, so $4 wins.
            let mut selector = ReceiptSelector::new(&events, Some(event_id!("$3")));
            selector.try_select_later(event_id!("$4"), 3);
            let best_receipt = selector.select();
            assert_eq!(best_receipt.unwrap().event_id, event_id!("$4"));
        }
    }

    #[test]
    fn test_receipt_selector_handle_pending_receipts_noop() {
        let sender = user_id!("@bob:example.org");
        let f = EventFactory::new().sender(sender);
        let ev1 = f.text_msg("yo").event_id(event_id!("$1")).into_event();
        let ev2 = f.text_msg("well?").event_id(event_id!("$2")).into_event();
        let events = &[ev1, ev2][..];

        {
            // No pending receipt => no better receipt.
            let mut selector = ReceiptSelector::new(events, None);

            let mut pending = RingBuffer::new(NonZeroUsize::new(16).unwrap());
            selector.handle_pending_receipts(&mut pending);

            assert!(pending.is_empty());

            let best_receipt = selector.select();
            assert!(best_receipt.is_none());
        }

        {
            // No pending receipt, and there was an active last receipt => no better
            // receipt.
            let mut selector = ReceiptSelector::new(events, Some(event_id!("$1")));

            let mut pending = RingBuffer::new(NonZeroUsize::new(16).unwrap());
            selector.handle_pending_receipts(&mut pending);

            assert!(pending.is_empty());

            let best_receipt = selector.select();
            assert!(best_receipt.is_none());
        }
    }

    #[test]
    fn test_receipt_selector_handle_pending_receipts_doesnt_match_known_events() {
        let sender = user_id!("@bob:example.org");
        let f = EventFactory::new().sender(sender);
        let ev1 = f.text_msg("yo").event_id(event_id!("$1")).into_event();
        let ev2 = f.text_msg("well?").event_id(event_id!("$2")).into_event();
        let events = &[ev1, ev2][..];

        {
            // A pending receipt for an event that is still missing => no better receipt.
            let mut selector = ReceiptSelector::new(events, None);

            let mut pending = RingBuffer::new(NonZeroUsize::new(16).unwrap());
            pending.push(owned_event_id!("$3"));
            selector.handle_pending_receipts(&mut pending);

            assert_eq!(pending.len(), 1);

            let best_receipt = selector.select();
            assert!(best_receipt.is_none());
        }

        {
            // Ditto but there was an active receipt => no better receipt.
            let mut selector = ReceiptSelector::new(events, Some(event_id!("$1")));

            let mut pending = RingBuffer::new(NonZeroUsize::new(16).unwrap());
            pending.push(owned_event_id!("$3"));
            selector.handle_pending_receipts(&mut pending);

            assert_eq!(pending.len(), 1);

            let best_receipt = selector.select();
            assert!(best_receipt.is_none());
        }
    }

    #[test]
    fn test_receipt_selector_handle_pending_receipts_matches_known_events_no_initial() {
        let sender = user_id!("@bob:example.org");
        let f = EventFactory::new().sender(sender);
        let ev1 = f.text_msg("yo").event_id(event_id!("$1")).into_event();
        let ev2 = f.text_msg("well?").event_id(event_id!("$2")).into_event();
        let events = &[ev1, ev2][..];

        {
            // A pending receipt for an event that is present => better receipt.
            let mut selector = ReceiptSelector::new(events, None);

            let mut pending = RingBuffer::new(NonZeroUsize::new(16).unwrap());
            pending.push(owned_event_id!("$2"));
            selector.handle_pending_receipts(&mut pending);

            // The receipt for $2 has been found.
            assert!(pending.is_empty());

            // The new receipt has been returned.
            let best_receipt = selector.select();
            assert_eq!(best_receipt.unwrap().event_id, event_id!("$2"));
        }

        {
            // Mixed found and not found receipt => better receipt.
            let mut selector = ReceiptSelector::new(events, None);

            let mut pending = RingBuffer::new(NonZeroUsize::new(16).unwrap());
            pending.push(owned_event_id!("$1"));
            pending.push(owned_event_id!("$3"));
            selector.handle_pending_receipts(&mut pending);

            // The receipt for $1 has been found, but not that for $3.
            assert_eq!(pending.len(), 1);
            assert!(pending.iter().any(|ev| ev == event_id!("$3")));

            let best_receipt = selector.select();
            assert_eq!(best_receipt.unwrap().event_id, event_id!("$1"));
        }
    }

    #[test]
    fn test_receipt_selector_handle_pending_receipts_matches_known_events_with_initial() {
        let sender = user_id!("@bob:example.org");
        let f = EventFactory::new().sender(sender);
        let ev1 = f.text_msg("yo").event_id(event_id!("$1")).into_event();
        let ev2 = f.text_msg("well?").event_id(event_id!("$2")).into_event();
        let events = &[ev1, ev2][..];

        {
            // Same, and there was an initial receipt that was less good than the one we
            // selected => better receipt.
            let mut selector = ReceiptSelector::new(events, Some(event_id!("$1")));

            let mut pending = RingBuffer::new(NonZeroUsize::new(16).unwrap());
            pending.push(owned_event_id!("$2"));
            selector.handle_pending_receipts(&mut pending);

            // The receipt for $2 has been found.
            assert!(pending.is_empty());

            // The new receipt has been returned.
            let best_receipt = selector.select();
            assert_eq!(best_receipt.unwrap().event_id, event_id!("$2"));
        }

        {
            // Same, but the previous receipt was better => no better receipt.
            let mut selector = ReceiptSelector::new(events, Some(event_id!("$2")));

            let mut pending = RingBuffer::new(NonZeroUsize::new(16).unwrap());
            pending.push(owned_event_id!("$1"));
            selector.handle_pending_receipts(&mut pending);

            // The receipt for $1 has been found.
            assert!(pending.is_empty());

            let best_receipt = selector.select();
            assert!(best_receipt.is_none());
        }
    }

    #[test]
    fn test_receipt_selector_handle_new_receipt() {
        let myself = user_id!("@alice:example.org");
        let events = make_test_events(user_id!("@bob:example.org"));

        let f = EventFactory::new();
        {
            // Thread receipts are ignored.
            let mut selector = ReceiptSelector::new(&events, None);

            let receipt_event = f
                .read_receipts()
                .add(
                    event_id!("$5"),
                    myself,
                    ReceiptType::Read,
                    ReceiptThread::Thread(owned_event_id!("$2")),
                )
                .into_content();

            let pending = selector.handle_new_receipt(myself, &receipt_event);
            assert!(pending.is_empty());

            let best_receipt = selector.select();
            assert!(best_receipt.is_none());
        }

        for receipt_type in [ReceiptType::Read, ReceiptType::ReadPrivate] {
            for receipt_thread in [ReceiptThread::Main, ReceiptThread::Unthreaded] {
                {
                    // Receipt for an event we don't know about => it's pending, and no better
                    // receipt.
                    let mut selector = ReceiptSelector::new(&events, None);

                    let receipt_event = f
                        .read_receipts()
                        .add(event_id!("$6"), myself, receipt_type.clone(), receipt_thread.clone())
                        .into_content();

                    let pending = selector.handle_new_receipt(myself, &receipt_event);
                    assert_eq!(pending[0], event_id!("$6"));
                    assert_eq!(pending.len(), 1);

                    let best_receipt = selector.select();
                    assert!(best_receipt.is_none());
                }

                {
                    // Receipt for an event we knew about, no initial active receipt => better
                    // receipt.
                    let mut selector = ReceiptSelector::new(&events, None);

                    let receipt_event = f
                        .read_receipts()
                        .add(event_id!("$3"), myself, receipt_type.clone(), receipt_thread.clone())
                        .into_content();

                    let pending = selector.handle_new_receipt(myself, &receipt_event);
                    assert!(pending.is_empty());

                    let best_receipt = selector.select();
                    assert_eq!(best_receipt.unwrap().event_id, event_id!("$3"));
                }

                {
                    // Receipt for an event we knew about, initial active receipt was better => no
                    // better receipt.
                    let mut selector = ReceiptSelector::new(&events, Some(event_id!("$4")));

                    let receipt_event = f
                        .read_receipts()
                        .add(event_id!("$3"), myself, receipt_type.clone(), receipt_thread.clone())
                        .into_content();

                    let pending = selector.handle_new_receipt(myself, &receipt_event);
                    assert!(pending.is_empty());

                    let best_receipt = selector.select();
                    assert!(best_receipt.is_none());
                }

                {
                    // Receipt for an event we knew about, initial active receipt was less good =>
                    // new better receipt.
                    let mut selector = ReceiptSelector::new(&events, Some(event_id!("$2")));

                    let receipt_event = f
                        .read_receipts()
                        .add(event_id!("$3"), myself, receipt_type.clone(), receipt_thread.clone())
                        .into_content();

                    let pending = selector.handle_new_receipt(myself, &receipt_event);
                    assert!(pending.is_empty());

                    let best_receipt = selector.select();
                    assert_eq!(best_receipt.unwrap().event_id, event_id!("$3"));
                }
            }
        } // end for

        {
            // Final boss: multiple receipts in the receipt event, the best one is used =>
            // new better receipt.
            let mut selector = ReceiptSelector::new(&events, Some(event_id!("$2")));

            let receipt_event = f
                .read_receipts()
                .add(event_id!("$4"), myself, ReceiptType::ReadPrivate, ReceiptThread::Unthreaded)
                .add(event_id!("$6"), myself, ReceiptType::ReadPrivate, ReceiptThread::Main)
                .add(event_id!("$3"), myself, ReceiptType::Read, ReceiptThread::Main)
                .into_content();

            let pending = selector.handle_new_receipt(myself, &receipt_event);
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0], event_id!("$6"));

            let best_receipt = selector.select();
            assert_eq!(best_receipt.unwrap().event_id, event_id!("$4"));
        }
    }

    #[test]
    fn test_try_match_implicit() {
        let myself = owned_user_id!("@alice:example.org");
        let bob = user_id!("@bob:example.org");

        let mut events = make_test_events(bob);

        // When the selector sees only other users' events,
        let mut selector = ReceiptSelector::new(&events, None);
        // And I search for my implicit read receipt,
        selector.try_match_implicit(&myself, &events);
        // Then I don't find any.
        let best_receipt = selector.select();
        assert!(best_receipt.is_none());

        // Now, if there are events I've written too...
        let f = EventFactory::new();
        events.push(
            f.text_msg("A mulatto, an albino")
                .sender(&myself)
                .event_id(event_id!("$6"))
                .into_event(),
        );
        events.push(
            f.text_msg("A mosquito, my libido").sender(bob).event_id(event_id!("$7")).into_event(),
        );

        let mut selector = ReceiptSelector::new(&events, None);
        // And I search for my implicit read receipt,
        selector.try_match_implicit(&myself, &events);
        // Then my last sent event counts as a read receipt.
        let best_receipt = selector.select();
        assert_eq!(best_receipt.unwrap().event_id, event_id!("$6"));
    }

    #[test]
    fn test_compute_unread_counts_with_implicit_receipt() {
        let user_id = user_id!("@alice:example.org");
        let bob = user_id!("@bob:example.org");
        let room_id = room_id!("!room:example.org");

        // Given a set of events sent by Bob,
        let mut events = make_test_events(bob);

        // One by me,
        let f = EventFactory::new();
        events.push(
            f.text_msg("A mulatto, an albino")
                .sender(user_id)
                .event_id(event_id!("$6"))
                .into_event(),
        );

        // And others by Bob,
        events.push(
            f.text_msg("A mosquito, my libido").sender(bob).event_id(event_id!("$7")).into_event(),
        );
        events.push(
            f.text_msg("A denial, a denial").sender(bob).event_id(event_id!("$8")).into_event(),
        );

        let events: Vec<_> = events.into_iter().collect();

        // I have a read receipt attached to one of Bob's event sent before my message,
        let receipt_event = f
            .read_receipts()
            .add(event_id!("$3"), user_id, ReceiptType::Read, ReceiptThread::Unthreaded)
            .into_content();

        let mut read_receipts = RoomReadReceipts::default();

        // And I compute the unread counts for all those new events (no previous events
        // in that room),
        compute_unread_counts(
            user_id,
            room_id,
            Some(&receipt_event),
            Vec::new(),
            &events,
            &mut read_receipts,
            ThreadingSupport::Disabled,
        );

        // Only the last two events sent by Bob count as unread.
        assert_eq!(read_receipts.num_unread, 2);

        // There are no pending receipts.
        assert!(read_receipts.pending.is_empty());

        // And the active receipt is the implicit one on my event.
        assert_eq!(read_receipts.latest_active.unwrap().event_id, event_id!("$6"));
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

        // Threaded messages from myself or other users shouldn't change the
        // unread counts.
        receipts.process_event(
            &make_event(own_alice, event_id!("$some_thread_root")),
            own_alice,
            ThreadingSupport::Enabled { with_subscriptions: false },
        );
        receipts.process_event(
            &make_event(own_alice, event_id!("$some_other_thread_root")),
            own_alice,
            ThreadingSupport::Enabled { with_subscriptions: false },
        );

        receipts.process_event(
            &make_event(bob, event_id!("$some_thread_root")),
            own_alice,
            ThreadingSupport::Enabled { with_subscriptions: false },
        );
        receipts.process_event(
            &make_event(bob, event_id!("$some_other_thread_root")),
            own_alice,
            ThreadingSupport::Enabled { with_subscriptions: false },
        );

        assert_eq!(receipts.num_unread, 0);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 0);

        // Processing an unthreaded message should still count as unread.
        receipts.process_event(
            &EventFactory::new().text_msg("A").sender(bob).event_id(event_id!("$ida")).into_event(),
            own_alice,
            ThreadingSupport::Enabled { with_subscriptions: false },
        );

        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 0);
    }
}
