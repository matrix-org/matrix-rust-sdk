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
//! unread status of rooms, via [`matrix_sdk::ruma::UnreadNotificationCounts`],
//! it's not reliable for encrypted rooms. Indeed, the server doesn't have
//! access to the content of encrypted events, so it can only makes guesses when
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
//! The only public method in that module is [`compute_notifications`], which
//! updates the `RoomInfo` in place according to the new counts.
#![allow(dead_code)] // too many different build configurations, I give up

use eyeball_im::Vector;
use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
use ruma::{
    events::{
        poll::{start::PollStartEventContent, unstable_start::UnstablePollStartEventContent},
        receipt::{ReceiptEventContent, ReceiptThread, ReceiptType},
        room::message::Relation,
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, OriginalSyncMessageLikeEvent,
        SyncMessageLikeEvent,
    },
    serde::Raw,
    EventId, OwnedEventId, RoomId, UserId,
};
use serde::{Deserialize, Serialize};
use tracing::{instrument, trace};

use crate::error::Result;

/// Information about read receipts collected during processing of that room.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub(crate) struct RoomReadReceipts {
    /// Does the room have unread messages?
    pub num_unread: u64,

    /// Does the room have unread events that should notify?
    pub num_notifications: u64,

    /// Does the room have messages causing highlights for the users? (aka
    /// mentions)
    pub num_mentions: u64,

    /// The id of the event the last unthreaded (or main-threaded, for better
    /// compatibility with clients that have thread support) read receipt is
    /// attached to.
    latest_read_receipt_event_id: Option<OwnedEventId>,
}

impl RoomReadReceipts {
    /// Update the [`RoomReadReceipts`] unread counts according to the new
    /// event.
    ///
    /// Returns whether a new event triggered a new unread/notification/mention.
    #[inline(always)]
    fn update_for_event(&mut self, event: &SyncTimelineEvent, user_id: &UserId) -> bool {
        let mut has_unread = false;

        if marks_as_unread(&event.event, user_id) {
            self.num_unread += 1;
            has_unread = true
        }

        let mut has_notify = false;
        let mut has_mention = false;

        for action in &event.push_actions {
            if !has_notify && action.should_notify() {
                self.num_notifications += 1;
                has_notify = true;
            }
            if !has_mention && action.is_highlight() {
                self.num_mentions += 1;
                has_mention = true;
            }
        }

        has_unread || has_notify || has_mention
    }

    #[inline(always)]
    fn reset(&mut self) {
        self.num_unread = 0;
        self.num_notifications = 0;
        self.num_mentions = 0;
    }

    /// Try to find the event to which the receipt attaches to, and if found,
    /// will update the notification count in the room.
    fn find_and_count_events<'a>(
        &mut self,
        receipt_event_id: &EventId,
        user_id: &UserId,
        events: impl IntoIterator<Item = &'a SyncTimelineEvent>,
    ) -> bool {
        let mut counting_receipts = false;

        for event in events {
            if counting_receipts {
                self.update_for_event(event, user_id);
            } else if let Ok(Some(event_id)) = event.event.get_field::<OwnedEventId>("event_id") {
                if event_id == receipt_event_id {
                    // Bingo! Switch over to the counting state, after resetting the
                    // previous counts.
                    trace!("Found the event the receipt was referring to! Starting to count.");
                    self.reset();
                    counting_receipts = true;
                }
            }
        }

        counting_receipts
    }
}

/// Provider for timeline events prior to the current sync.
pub trait PreviousEventsProvider: Send + Sync {
    /// Returns the list of known timeline events, in sync order, for the given
    /// room.
    fn for_room(&self, room_id: &RoomId) -> Vector<SyncTimelineEvent>;
}

impl PreviousEventsProvider for () {
    fn for_room(&self, _: &RoomId) -> Vector<SyncTimelineEvent> {
        Vector::new()
    }
}

/// Given a set of events coming from sync, for a room, update the
/// [`RoomReadReceipts`]'s counts of unread messages, notifications and
/// highlights' in place.
///
/// A provider of previous events may be required to reconcile a read receipt
/// that has been just received for an event that came in a previous sync.
///
/// See this module's documentation for more information.
///
/// Returns a boolean indicating if a field changed value in the read receipts.
#[instrument(skip_all, fields(room_id = %room_id, ?read_receipts))]
pub(crate) fn compute_notifications<PEP: PreviousEventsProvider>(
    user_id: &UserId,
    room_id: &RoomId,
    receipt_event: Option<&ReceiptEventContent>,
    previous_events_provider: &PEP,
    new_events: &[SyncTimelineEvent],
    read_receipts: &mut RoomReadReceipts,
) -> Result<bool> {
    let prev_latest_receipt_event_id = read_receipts.latest_read_receipt_event_id.clone();

    if let Some(receipt_event) = receipt_event {
        trace!("Got a new receipt event!");

        // Find a private or public read receipt for the current user.
        let mut receipt_event_id = None;
        if let Some((event_id, receipt)) = receipt_event
            .user_receipt(user_id, ReceiptType::Read)
            .or_else(|| receipt_event.user_receipt(user_id, ReceiptType::ReadPrivate))
        {
            if receipt.thread == ReceiptThread::Unthreaded || receipt.thread == ReceiptThread::Main
            {
                // The server can return the same receipt multiple times.
                // Do not consider it if it's the same as the previous one, to avoid doing
                // wasteful work.
                if Some(event_id) != prev_latest_receipt_event_id.as_deref() {
                    receipt_event_id = Some(event_id.to_owned());
                }
            }
        }

        if let Some(receipt_event_id) = receipt_event_id {
            // We've found the id of an event to which the receipt attaches. The associated
            // event may either come from the new batch of events associated to
            // this sync, or it may live in the past timeline events we know
            // about.

            // First, save the event id as the latest one that has a read receipt.
            read_receipts.latest_read_receipt_event_id = Some(receipt_event_id.clone());

            // Try to find if the read receipt refers to an event from the current sync, to
            // avoid searching the cached timeline events.
            trace!("We got a new event with a read receipt: {receipt_event_id}. Search in new events...");
            if read_receipts.find_and_count_events(&receipt_event_id, user_id, new_events) {
                // It did, so our work here is done.
                // Always return true here; we saved at least the latest read receipt.
                return Ok(true);
            }

            // We didn't find the event attached to the receipt in the new batches of
            // events. It's possible it's referring to an event we've already
            // seen. In that case, try to find it.
            let previous_events = previous_events_provider.for_room(room_id);

            trace!("Couldn't find the event attached to the receipt in the new events; looking in past events too now...");
            if read_receipts.find_and_count_events(
                &receipt_event_id,
                user_id,
                previous_events.iter().chain(new_events.iter()),
            ) {
                // It did refer to an old event, so our work here is done.
                // Always return true here; we saved at least the latest read receipt.
                return Ok(true);
            }
        }
    }

    if let Some(receipt_event_id) = prev_latest_receipt_event_id {
        // There's no new read-receipt here. We assume the cached events have been
        // properly processed, and we only need to process the new events based
        // on the previous receipt.
        trace!("No new receipts, or couldn't find attached event; looking if the past latest known receipt refers to a new event...");
        if read_receipts.find_and_count_events(&receipt_event_id, user_id, new_events) {
            // We found the event to which the previous receipt attached to (so we at least
            // reset the counts once), our work is done here.
            return Ok(true);
        }
    }

    // If we haven't returned at this point, it means that either we had no previous
    // read receipt, or the previous read receipt was not attached to any new
    // event.
    //
    // In that case, accumulate all events as part of the current batch, and wait
    // for the next receipt.
    trace!("Default path: including all new events for the receipts count.");
    let mut new_receipt = false;
    for event in new_events {
        if read_receipts.update_for_event(event, user_id) {
            new_receipt = true;
        }
    }

    Ok(new_receipt)
}

/// Is the event worth marking a room as unread?
fn marks_as_unread(event: &Raw<AnySyncTimelineEvent>, user_id: &UserId) -> bool {
    let event = match event.deserialize() {
        Ok(event) => event,
        Err(err) => {
            tracing::debug!(
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
        ruma::events::AnySyncTimelineEvent::MessageLike(event) => {
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
                    tracing::debug!("unhandled timeline event type: {}", event.event_type());
                    false
                }
            }
        }

        ruma::events::AnySyncTimelineEvent::State(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ops::Not as _};

    use eyeball_im::Vector;
    use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
    use matrix_sdk_test::sync_timeline_event;
    use ruma::{
        assign, event_id,
        events::receipt::{Receipt, ReceiptEventContent, ReceiptThread, ReceiptType},
        push::Action,
        room_id, user_id, EventId, UserId,
    };

    use super::compute_notifications;
    use crate::{
        read_receipts::{marks_as_unread, RoomReadReceipts},
        PreviousEventsProvider,
    };

    #[test]
    fn test_room_message_marks_as_unread() {
        let user_id = user_id!("@alice:example.org");
        let other_user_id = user_id!("@bob:example.org");

        // A message from somebody else marks the room as unread...
        let ev = sync_timeline_event!({
            "sender": other_user_id,
            "type": "m.room.message",
            "event_id": "$ida",
            "origin_server_ts": 12344446,
            "content": { "body":"A", "msgtype": "m.text" },
        });
        assert!(marks_as_unread(&ev, user_id));

        // ... but a message from ourselves doesn't.
        let ev = sync_timeline_event!({
            "sender": user_id,
            "type": "m.room.message",
            "event_id": "$ida",
            "origin_server_ts": 12344446,
            "content": { "body":"A", "msgtype": "m.text" },
        });
        assert!(marks_as_unread(&ev, user_id).not());
    }

    #[test]
    fn test_room_edit_doesnt_mark_as_unread() {
        let user_id = user_id!("@alice:example.org");
        let other_user_id = user_id!("@bob:example.org");

        // An edit to a message from somebody else doesn't mark the room as unread.
        let ev = sync_timeline_event!({
            "sender": other_user_id,
            "type": "m.room.message",
            "event_id": "$ida",
            "origin_server_ts": 12344446,
            "content": {
                "body": " * edited message",
                "m.new_content": {
                    "body": "edited message",
                    "msgtype": "m.text"
                },
                "m.relates_to": {
                    "event_id": "$someeventid:localhost",
                    "rel_type": "m.replace"
                },
                "msgtype": "m.text"
            },
        });
        assert!(marks_as_unread(&ev, user_id).not());
    }

    #[test]
    fn test_redaction_doesnt_mark_room_as_unread() {
        let user_id = user_id!("@alice:example.org");
        let other_user_id = user_id!("@bob:example.org");

        // A redact of a message from somebody else doesn't mark the room as unread.
        let ev = sync_timeline_event!({
            "content": {
                "reason": "🛑"
            },
            "event_id": "$151957878228ssqrJ:localhost",
            "origin_server_ts": 151957878000000_u64,
            "sender": other_user_id,
            "type": "m.room.redaction",
            "redacts": "$151957878228ssqrj:localhost",
            "unsigned": {
                "age": 85
            }
        });

        assert!(marks_as_unread(&ev, user_id).not());
    }

    #[test]
    fn test_reaction_doesnt_mark_room_as_unread() {
        let user_id = user_id!("@alice:example.org");
        let other_user_id = user_id!("@bob:example.org");

        // A reaction from somebody else to a message doesn't mark the room as unread.
        let ev = sync_timeline_event!({
            "content": {
                "m.relates_to": {
                    "event_id": "$15275047031IXQRi:localhost",
                    "key": "👍",
                    "rel_type": "m.annotation"
                }
            },
            "event_id": "$15275047031IXQRi:localhost",
            "origin_server_ts": 159027581000000_u64,
            "sender": other_user_id,
            "type": "m.reaction",
            "unsigned": {
                "age": 85
            }
        });

        assert!(marks_as_unread(&ev, user_id).not());
    }

    #[test]
    fn test_state_event_doesnt_mark_as_unread() {
        let user_id = user_id!("@alice:example.org");
        let event_id = event_id!("$1");
        let ev = sync_timeline_event!({
            "content": {
                "displayname": "Alice",
                "membership": "join",
            },
            "event_id": event_id,
            "origin_server_ts": 1432135524678u64,
            "sender": user_id,
            "state_key": user_id,
            "type": "m.room.member",
        });

        assert!(marks_as_unread(&ev, user_id).not());

        let other_user_id = user_id!("@bob:example.org");
        assert!(marks_as_unread(&ev, other_user_id).not());
    }

    #[test]
    fn test_count_unread_and_mentions() {
        fn make_event(user_id: &UserId, push_actions: Vec<Action>) -> SyncTimelineEvent {
            SyncTimelineEvent {
                event: sync_timeline_event!({
                    "sender": user_id,
                    "type": "m.room.message",
                    "event_id": "$ida",
                    "origin_server_ts": 12344446,
                    "content": { "body":"A", "msgtype": "m.text" },
                }),
                encryption_info: None,
                push_actions,
            }
        }

        let user_id = user_id!("@alice:example.org");

        // An interesting event from oneself doesn't count as a new unread message.
        let event = make_event(user_id, Vec::new());
        let mut receipts = RoomReadReceipts::default();
        receipts.update_for_event(&event, user_id);
        assert_eq!(receipts.num_unread, 0);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 0);

        // An interesting event from someone else does count as a new unread message.
        let event = make_event(user_id!("@bob:example.org"), Vec::new());
        let mut receipts = RoomReadReceipts::default();
        receipts.update_for_event(&event, user_id);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 0);

        // Push actions computed beforehand are respected.
        let event = make_event(user_id!("@bob:example.org"), vec![Action::Notify]);
        let mut receipts = RoomReadReceipts::default();
        receipts.update_for_event(&event, user_id);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 1);

        let event = make_event(
            user_id!("@bob:example.org"),
            vec![Action::SetTweak(ruma::push::Tweak::Highlight(true))],
        );
        let mut receipts = RoomReadReceipts::default();
        receipts.update_for_event(&event, user_id);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 1);
        assert_eq!(receipts.num_notifications, 0);

        let event = make_event(
            user_id!("@bob:example.org"),
            vec![Action::SetTweak(ruma::push::Tweak::Highlight(true)), Action::Notify],
        );
        let mut receipts = RoomReadReceipts::default();
        receipts.update_for_event(&event, user_id);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 1);
        assert_eq!(receipts.num_notifications, 1);

        // Technically this `push_actions` set would be a bug somewhere else, but let's
        // make sure to resist against it.
        let event = make_event(user_id!("@bob:example.org"), vec![Action::Notify, Action::Notify]);
        let mut receipts = RoomReadReceipts::default();
        receipts.update_for_event(&event, user_id);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 1);
    }

    #[test]
    fn test_find_and_count_events() {
        let ev0 = event_id!("$0");
        let user_id = user_id!("@alice:example.org");

        // When provided with no events, we report not finding the event to which the
        // receipt relates.
        let mut receipts = RoomReadReceipts::default();
        assert!(receipts.find_and_count_events(ev0, user_id, &[]).not());
        assert_eq!(receipts.num_unread, 0);
        assert_eq!(receipts.num_notifications, 0);
        assert_eq!(receipts.num_mentions, 0);

        // When provided with one event, that's not the receipt event, we don't count
        // it.
        fn make_event(event_id: &EventId) -> SyncTimelineEvent {
            SyncTimelineEvent {
                event: sync_timeline_event!({
                    "sender": "@bob:example.org",
                    "type": "m.room.message",
                    "event_id": event_id,
                    "origin_server_ts": 12344446,
                    "content": { "body":"A", "msgtype": "m.text" },
                }),
                encryption_info: None,
                push_actions: Vec::new(),
            }
        }

        let mut receipts = RoomReadReceipts {
            num_unread: 42,
            num_notifications: 13,
            num_mentions: 37,
            latest_read_receipt_event_id: None,
        };
        assert!(receipts
            .find_and_count_events(ev0, user_id, &[make_event(event_id!("$1"))],)
            .not());
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
            latest_read_receipt_event_id: None,
        };
        assert!(receipts.find_and_count_events(ev0, user_id, &[make_event(ev0)]));
        assert_eq!(receipts.num_unread, 0);
        assert_eq!(receipts.num_notifications, 0);
        assert_eq!(receipts.num_mentions, 0);

        // When provided with multiple events and not the receipt event, we do not count
        // anything..
        let mut receipts = RoomReadReceipts {
            num_unread: 42,
            num_notifications: 13,
            num_mentions: 37,
            latest_read_receipt_event_id: None,
        };
        assert!(receipts
            .find_and_count_events(
                ev0,
                user_id,
                &[
                    make_event(event_id!("$1")),
                    make_event(event_id!("$2")),
                    make_event(event_id!("$3"))
                ],
            )
            .not());
        assert_eq!(receipts.num_unread, 42);
        assert_eq!(receipts.num_notifications, 13);
        assert_eq!(receipts.num_mentions, 37);

        // When provided with multiple events including one that's the receipt event, we
        // find it and count from it.
        let mut receipts = RoomReadReceipts {
            num_unread: 42,
            num_notifications: 13,
            num_mentions: 37,
            latest_read_receipt_event_id: None,
        };
        assert!(receipts.find_and_count_events(
            ev0,
            user_id,
            &[
                make_event(event_id!("$1")),
                make_event(ev0),
                make_event(event_id!("$2")),
                make_event(event_id!("$3"))
            ],
        ));
        assert_eq!(receipts.num_unread, 2);
        assert_eq!(receipts.num_notifications, 0);
        assert_eq!(receipts.num_mentions, 0);
    }

    impl PreviousEventsProvider for Vector<SyncTimelineEvent> {
        fn for_room(&self, _room_id: &ruma::RoomId) -> Vector<SyncTimelineEvent> {
            self.clone()
        }
    }

    #[test]
    fn test_compute_notifications() {
        let user_id = user_id!("@alice:example.org");
        let other_user_id = user_id!("@bob:example.org");
        let room_id = room_id!("!room:example.org");
        let receipt_event_id = event_id!("$1");

        let mut previous_events = Vector::new();

        let ev1 = SyncTimelineEvent::new(sync_timeline_event!({
            "sender": other_user_id,
            "type": "m.room.message",
            "event_id": receipt_event_id,
            "origin_server_ts": 12344446,
            "content": { "body":"A", "msgtype": "m.text" },
        }));

        let ev2 = SyncTimelineEvent::new(sync_timeline_event!({
            "sender": other_user_id,
            "type": "m.room.message",
            "event_id": "$2",
            "origin_server_ts": 12344446,
            "content": { "body":"A", "msgtype": "m.text" },
        }));

        let map = BTreeMap::from([(
            receipt_event_id.to_owned(),
            BTreeMap::from([(
                ReceiptType::Read,
                BTreeMap::from([(
                    user_id.to_owned(),
                    assign!(Receipt::default(), {
                        thread: ReceiptThread::Unthreaded
                    }),
                )]),
            )]),
        )]);
        let receipt_event = ReceiptEventContent(map);

        let mut read_receipts = Default::default();
        compute_notifications(
            user_id,
            room_id,
            Some(&receipt_event),
            &previous_events,
            &[ev1.clone(), ev2.clone()],
            &mut read_receipts,
        )
        .unwrap();

        // It did find the receipt event (ev1) and another one (ev2).
        assert_eq!(read_receipts.num_unread, 1);

        // Receive the same receipt event, with a new sync event.
        previous_events.push_back(ev1);
        previous_events.push_back(ev2);

        let sync_event = SyncTimelineEvent::new(sync_timeline_event!({
            "sender": other_user_id,
            "type": "m.room.message",
            "event_id": "$43",
            "origin_server_ts": 12344446,
            "content": { "body":"A", "msgtype": "m.text" },
        }));

        compute_notifications(
            user_id,
            room_id,
            Some(&receipt_event),
            &previous_events,
            &[sync_event],
            &mut read_receipts,
        )
        .unwrap();

        // Only the new event should be added.
        assert_eq!(read_receipts.num_unread, 2);
    }
}
