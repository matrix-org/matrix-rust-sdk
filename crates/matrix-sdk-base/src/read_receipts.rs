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

use eyeball_im::Vector;
use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
use ruma::{
    events::{
        poll::{start::PollStartEventContent, unstable_start::UnstablePollStartEventContent},
        receipt::{ReceiptThread, ReceiptType},
        room::message::Relation,
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, OriginalSyncMessageLikeEvent,
        SyncMessageLikeEvent,
    },
    serde::Raw,
    EventId, OwnedEventId, RoomId, UserId,
};
use tracing::{instrument, trace};

use super::BaseClient;
use crate::{error::Result, store::StateChanges, RoomInfo};

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
/// [`RoomInfo`]'s counts of unread messages, notifications and highlights' in
/// place.
///
/// A provider of previous events may be required to reconcile a read receipt
/// that has been just received for an event that came in a previous sync.
///
/// See this module's documentation for more information.
#[instrument(skip_all, fields(room_id = %room_info.room_id))]
pub(crate) fn compute_notifications<PEP: PreviousEventsProvider>(
    client: &BaseClient,
    changes: &StateChanges,
    previous_events_provider: &PEP,
    new_events: &[SyncTimelineEvent],
    room_info: &mut RoomInfo,
) -> Result<()> {
    let user_id = &client.session_meta().unwrap().user_id;
    let prev_latest_receipt_event_id = room_info.read_receipts.latest_read_receipt_event_id.clone();

    if let Some(receipt_event) = changes.receipts.get(room_info.room_id()) {
        trace!("Got a new receipt event!");

        // Find a private or public read receipt for the current user.
        let mut receipt_event_id = None;
        if let Some((event_id, receipt)) = receipt_event
            .user_receipt(user_id, ReceiptType::Read)
            .or_else(|| receipt_event.user_receipt(user_id, ReceiptType::ReadPrivate))
        {
            if receipt.thread == ReceiptThread::Unthreaded || receipt.thread == ReceiptThread::Main
            {
                receipt_event_id = Some(event_id.to_owned());
            }
        }

        if let Some(receipt_event_id) = receipt_event_id {
            // We've found the id of an event to which the receipt attaches. The associated
            // event may either come from the new batch of events associated to
            // this sync, or it may live in the past timeline events we know
            // about.

            // First, save the event id as the latest one that has a read receipt.
            room_info.read_receipts.latest_read_receipt_event_id = Some(receipt_event_id.clone());

            // Try to find if the read receipt refers to an event from the current sync, to
            // avoid searching the cached timeline events.
            trace!("We got a new event with a read receipt: {receipt_event_id}. Search in new events...");
            if find_and_count_events(&receipt_event_id, user_id, new_events, room_info) {
                // It did, so our work here is done.
                return Ok(());
            }

            // We didn't find the event attached to the receipt in the new batches of
            // events. It's possible it's referring to an event we've already
            // seen. In that case, try to find it.
            let previous_events = previous_events_provider.for_room(&room_info.room_id);

            trace!("Couldn't find the event attached to the receipt in the new events; looking in past events too now...");
            if find_and_count_events(
                &receipt_event_id,
                user_id,
                previous_events.iter().chain(new_events.iter()),
                room_info,
            ) {
                // It did refer to an old event, so our work here is done.
                return Ok(());
            }
        }
    }

    if let Some(receipt_event_id) = prev_latest_receipt_event_id {
        // There's no new read-receipt here. We assume the cached events have been
        // properly processed, and we only need to process the new events based
        // on the previous receipt.
        trace!("No new receipts, or couldn't find attached event; looking if the past latest known receipt refers to a new event...");
        if find_and_count_events(&receipt_event_id, user_id, new_events, room_info) {
            // We found the event to which the previous receipt attached to, so our work is
            // done here.
            return Ok(());
        }
    }

    // If we haven't returned at this point, it means that either we had no previous
    // read receipt, or the previous read receipt was not attached to any new
    // event.
    //
    // In that case, accumulate all events as part of the current batch, and wait
    // for the next receipt.
    trace!("Default path: including all new events for the receipts count.");
    for event in new_events {
        count_unread_and_mentions(event, user_id, room_info);
    }

    Ok(())
}

#[inline(always)]
fn count_unread_and_mentions(
    event: &SyncTimelineEvent,
    user_id: &UserId,
    room_info: &mut RoomInfo,
) {
    if marks_as_unread(&event.event, user_id) {
        room_info.read_receipts.num_unread += 1;
    }
    for action in &event.push_actions {
        if action.should_notify() {
            room_info.read_receipts.num_notifications += 1;
        }
        if action.is_highlight() {
            room_info.read_receipts.num_mentions += 1;
        }
    }
}

/// Try to find the event to which the receipt attaches to, and if found, will
/// update the notification count in the room.
///
/// Returns a boolean indicating if it's found the event and updated the count.
fn find_and_count_events<'a>(
    receipt_event_id: &EventId,
    user_id: &UserId,
    events: impl IntoIterator<Item = &'a SyncTimelineEvent>,
    room_info: &mut RoomInfo,
) -> bool {
    let mut counting_receipts = false;
    for event in events {
        if counting_receipts {
            count_unread_and_mentions(event, user_id, room_info);
        } else if let Ok(Some(event_id)) = event.event.get_field::<OwnedEventId>("event_id") {
            if event_id == receipt_event_id {
                // Bingo! Switch over to the counting state, after resetting the
                // previous counts.
                trace!("Found the event the receipt was referring to! Starting to count.");
                room_info.read_receipts.num_unread = 0;
                room_info.read_receipts.num_notifications = 0;
                room_info.read_receipts.num_mentions = 0;
                counting_receipts = true;
            }
        }
    }
    counting_receipts
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
    use std::ops::Not as _;

    use matrix_sdk_test::sync_timeline_event;
    use ruma::{event_id, user_id};

    use crate::read_receipts::marks_as_unread;

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
}
