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

use std::collections::{BTreeMap, BTreeSet};

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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LatestReadReceipt {
    /// The id of the event the read receipt is referring to. (Not the read
    /// receipt event id.)
    event_id: OwnedEventId,
}

/// Public data about read receipts collected during processing of that room.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub(crate) struct RoomReadReceipts {
    /// Does the room have unread messages?
    pub num_unread: u64,

    /// Does the room have unread events that should notify?
    pub num_notifications: u64,

    /// Does the room have messages causing highlights for the users? (aka
    /// mentions)
    pub num_mentions: u64,

    /// The latest read receipt (main-threaded or unthreaded) known for the
    /// room. TODO: move this over to the `Room` struct, no need to memoize
    /// it here
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
    #[serde(default)]
    pending: BTreeSet<OwnedEventId>,
}

impl RoomReadReceipts {
    /// Update the [`RoomReadReceipts`] unread counts according to the new
    /// event.
    ///
    /// Returns whether a new event triggered a new unread/notification/mention.
    #[inline(always)]
    fn account_event(&mut self, event: &SyncTimelineEvent, user_id: &UserId) -> bool {
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
    fn find_and_account_events<'a>(
        &mut self,
        receipt_event_id: &EventId,
        user_id: &UserId,
        events: impl IntoIterator<Item = &'a SyncTimelineEvent>,
    ) -> bool {
        let mut counting_receipts = false;

        for event in events {
            if counting_receipts {
                self.account_event(event, user_id);
            } else if let Some(event_id) = event.event_id() {
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
    // Index all the events (from event_id to their position in the sync stream).
    // TODO: partially cache this index, invalidate upon gappy sync
    let mut all_events = previous_events_provider.for_room(room_id);
    all_events.extend(new_events.iter().cloned());

    let event_id_to_pos =
        BTreeMap::from_iter(all_events.iter().enumerate().filter_map(|(pos, event)| {
            event.event_id().and_then(|event_id| Some((event_id, pos)))
        }));

    // We're looking for a receipt that has a position that is at least further (>)
    // than the one we knew about (if any).
    let mut best_receipt = None;
    let mut best_pos = read_receipts
        .latest_active
        .as_ref()
        .and_then(|r| event_id_to_pos.get(&r.event_id))
        .copied();

    // Try to match stashes receipts against the new events.
    read_receipts.pending.retain(|event_id| {
        if let Some(event_pos) = event_id_to_pos.get(event_id) {
            // We now have a position for an event that had a read receipt, but wasn't found
            // before. Consider if it is the most recent now.
            if let Some(best_pos) = best_pos.as_mut() {
                // Note: by using a strict comparison here, we protect against the
                // server sending a receipt on the same event multiple times.
                if *event_pos > *best_pos {
                    *best_pos = *event_pos;
                    best_receipt = Some(event_id.clone());
                }
            } else {
                // We didn't have a previous receipt, this is the first one we
                // store: remember it.
                best_pos = Some(*event_pos);
                best_receipt = Some(event_id.clone());
            }

            // Remove this stashed read receipt from the pending list, as it's been
            // reconciled with its event.
            false
        } else {
            // Keep it for further iterations.
            true
        }
    });

    if let Some(receipt_event) = receipt_event {
        trace!("Got a new receipt event!");

        // Now consider new receipts.
        for (event_id, receipts) in &receipt_event.0 {
            for ty in [ReceiptType::Read, ReceiptType::ReadPrivate] {
                if let Some(receipt) = receipts.get(&ty).and_then(|receipts| receipts.get(user_id))
                {
                    if matches!(receipt.thread, ReceiptThread::Main | ReceiptThread::Unthreaded) {
                        if let Some(event_pos) = event_id_to_pos.get(event_id) {
                            if let Some(best_pos) = best_pos.as_mut() {
                                // Note: by using a strict comparison here, we protect against the
                                // server sending a receipt on the same event multiple times.
                                if *event_pos > *best_pos {
                                    *best_pos = *event_pos;
                                    best_receipt = Some(event_id.clone());
                                }
                            } else {
                                // We didn't have a previous receipt, this is the first one we
                                // store: remember it.
                                best_pos = Some(*event_pos);
                                best_receipt = Some(event_id.clone());
                            }
                        } else {
                            // It's a new pending receipt.
                            read_receipts.pending.insert(event_id.clone());
                        }
                    }
                }
            }
        }
    }

    // I swear the `LatestReadReceipt` might get handy at some point.
    let new_receipt = best_receipt.map(|r| LatestReadReceipt { event_id: r.clone() });

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
        return Ok(read_receipts.find_and_account_events(&event_id, user_id, &all_events));
    }

    // If we haven't returned at this point, it means:
    // - we didn't have an active read receipt, *or* we had one and it hasn't been
    //   replaced,
    // - all pending receipts refered to events we still don't know about,
    // - there was no new receipt event, *or* all receipts mentioned in that event
    //   were older than
    // the active one.
    //
    // In that case, accumulate all events as part of the current batch, and wait
    // for the next receipt.

    trace!(
        "Default path: no new active read receipt, so including all {} new events.",
        new_events.len()
    );
    let mut new_receipt = false;
    for event in new_events {
        if read_receipts.account_event(event, user_id) {
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
    use std::ops::Not as _;

    use eyeball_im::Vector;
    use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
    use matrix_sdk_test::{sync_timeline_event, EventBuilder};
    use ruma::{
        event_id,
        events::receipt::{ReceiptThread, ReceiptType},
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
        receipts.account_event(&event, user_id);
        assert_eq!(receipts.num_unread, 0);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 0);

        // An interesting event from someone else does count as a new unread message.
        let event = make_event(user_id!("@bob:example.org"), Vec::new());
        let mut receipts = RoomReadReceipts::default();
        receipts.account_event(&event, user_id);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 0);

        // Push actions computed beforehand are respected.
        let event = make_event(user_id!("@bob:example.org"), vec![Action::Notify]);
        let mut receipts = RoomReadReceipts::default();
        receipts.account_event(&event, user_id);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 0);
        assert_eq!(receipts.num_notifications, 1);

        let event = make_event(
            user_id!("@bob:example.org"),
            vec![Action::SetTweak(ruma::push::Tweak::Highlight(true))],
        );
        let mut receipts = RoomReadReceipts::default();
        receipts.account_event(&event, user_id);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 1);
        assert_eq!(receipts.num_notifications, 0);

        let event = make_event(
            user_id!("@bob:example.org"),
            vec![Action::SetTweak(ruma::push::Tweak::Highlight(true)), Action::Notify],
        );
        let mut receipts = RoomReadReceipts::default();
        receipts.account_event(&event, user_id);
        assert_eq!(receipts.num_unread, 1);
        assert_eq!(receipts.num_mentions, 1);
        assert_eq!(receipts.num_notifications, 1);

        // Technically this `push_actions` set would be a bug somewhere else, but let's
        // make sure to resist against it.
        let event = make_event(user_id!("@bob:example.org"), vec![Action::Notify, Action::Notify]);
        let mut receipts = RoomReadReceipts::default();
        receipts.account_event(&event, user_id);
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
        assert!(receipts.find_and_account_events(ev0, user_id, &[]).not());
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
            ..Default::default()
        };
        assert!(receipts
            .find_and_account_events(ev0, user_id, &[make_event(event_id!("$1"))],)
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
            ..Default::default()
        };
        assert!(receipts.find_and_account_events(ev0, user_id, &[make_event(ev0)]));
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
        assert!(receipts
            .find_and_account_events(
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
            ..Default::default()
        };
        assert!(receipts.find_and_account_events(
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

    fn sync_timeline_message(
        sender: &UserId,
        event_id: impl serde::Serialize,
        body: impl serde::Serialize,
    ) -> SyncTimelineEvent {
        SyncTimelineEvent::new(sync_timeline_event!({
            "sender": sender,
            "type": "m.room.message",
            "event_id": event_id,
            "origin_server_ts": 42,
            "content": { "body": body, "msgtype": "m.text" },
        }))
    }

    /// Smoke test for `compute_notifications`.
    #[test]
    fn test_basic_compute_notifications() {
        let user_id = user_id!("@alice:example.org");
        let other_user_id = user_id!("@bob:example.org");
        let room_id = room_id!("!room:example.org");
        let receipt_event_id = event_id!("$1");

        let mut previous_events = Vector::new();

        let ev1 = sync_timeline_message(other_user_id, receipt_event_id, "A");
        let ev2 = sync_timeline_message(other_user_id, "$2", "A");

        let receipt_event = EventBuilder::new().make_receipt_event_content([(
            receipt_event_id.to_owned(),
            ReceiptType::Read,
            user_id.to_owned(),
            ReceiptThread::Unthreaded,
        )]);

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

        // It did find the receipt event (ev1).
        assert_eq!(read_receipts.num_unread, 1);

        // Receive the same receipt event, with a new sync event.
        previous_events.push_back(ev1);
        previous_events.push_back(ev2);

        let new_event = sync_timeline_message(other_user_id, "$3", "A");
        compute_notifications(
            user_id,
            room_id,
            Some(&receipt_event),
            &previous_events,
            &[new_event],
            &mut read_receipts,
        )
        .unwrap();

        // Only the new event should be added.
        assert_eq!(read_receipts.num_unread, 2);
    }
}
