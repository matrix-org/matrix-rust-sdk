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

use std::{
    collections::{BTreeMap, HashMap},
    iter::once,
};

use matrix_sdk_base::{
    serde_helpers::{extract_redaction_target, extract_relation, extract_thread_root},
    sync::Timeline,
};
use ruma::{
    EventId, OwnedEventId, UserId,
    events::{
        AnySyncEphemeralRoomEvent,
        receipt::{ReceiptEventContent, ReceiptThread},
        relation::RelationType,
    },
    room_version_rules::RedactionRules,
    serde::Raw,
};

use super::{
    super::{Result, states::StateLockReadGuard},
    read_receipts::MaybeReceiptEventContent,
    room::RoomEventCacheState,
    thread::ThreadEventCacheState,
};

pub async fn aggregate_timeline_and_read_receipts_for_threads<'sync, 'state>(
    timeline: &'sync Timeline,
    own_read_receipts: BTreeMap<OwnedEventId, MaybeReceiptEventContent>,
    existing_threads: StateLockReadGuard<'state, HashMap<OwnedEventId, ThreadEventCacheState>>,
    maybe_room: Option<StateLockReadGuard<'state, RoomEventCacheState>>,
    redaction_rules: &'sync RedactionRules,
) -> Result<HashMap<OwnedEventId, (Timeline, MaybeReceiptEventContent)>> {
    let mut new_events_by_thread = HashMap::new();

    let default_entry = || {
        (
            Timeline {
                limited: timeline.limited,
                prev_batch: timeline.prev_batch.clone(),
                events: Vec::new(),
            },
            MaybeReceiptEventContent::none(),
        )
    };

    // Look for in-thread events, i.e. events that are part of threads.
    for (nth, event) in timeline.events.iter().enumerate() {
        match extract_relation(event.raw()) {
            // Ohh, this event relates to another event!
            Some((relation_type, related_event_id)) => match relation_type {
                // `related_event` represents a thread root.
                RelationType::Thread => {
                    new_events_by_thread
                        .entry(related_event_id)
                        .or_insert_with(default_entry)
                        .0
                        .events
                        .push(event.clone());
                }

                // `event` represents an annotation (e.g. reactions), a
                // replacement (an edit), a reference or something custom. Let's
                // see if the `related_event_id` is an in-thread event.
                RelationType::Annotation
                | RelationType::Replacement
                | RelationType::Reference
                | _ => {
                    // The related event may be the thread root itself. It may
                    // not be included in the sync results, so it must be
                    // matched against the known threads directly.
                    if existing_threads.contains_key(&related_event_id) {
                        new_events_by_thread
                            .entry(related_event_id)
                            .or_insert_with(default_entry)
                            .0
                            .events
                            .push(event.clone());
                    }
                    // Otherwise, look for the related event in `timeline`
                    // backwards.
                    else if let Some(thread_root) = match timeline.events[..nth]
                        .iter()
                        .rev()
                        .find(|event| event.event_id() == Some(&related_event_id))
                    {
                        // The related event has been found in the `timeline`!
                        // Extract its thread root.
                        Some(related_event) => extract_thread_root(related_event.raw()),

                        // Not in `timeline`, okay, look for the related event
                        // in the `room` as it knows about all the events, and
                        // then extract its thread root.
                        None => match &maybe_room {
                            Some(room) => room.find_event(&related_event_id).await?.and_then(
                                |(_location, related_event)| {
                                    extract_thread_root(related_event.raw())
                                },
                            ),
                            None => None,
                        },
                    } {
                        new_events_by_thread
                            .entry(thread_root)
                            .or_insert_with(default_entry)
                            .0
                            .events
                            .push(event.clone());
                    }
                }
            },

            // No explicit relation, okay, but it can still be related to a
            // thread!
            None => {
                // We previously found events that are part of a thread, but we
                // didn't see the thread root yet. And guess what? This might be
                // this event!
                if let Some(event_id) = event.event_id()
                    && existing_threads.contains_key(event_id)
                {
                    new_events_by_thread
                        .entry(event_id.to_owned())
                        .or_insert_with(default_entry)
                        .0
                        .events
                        .push(event.clone());
                }
                // Otherwise, this event might be a redaction that applies to a thread.
                else if let Some(redaction_target) =
                    extract_redaction_target(event.raw(), redaction_rules)
                    && match &maybe_room {
                        Some(room) => room.find_event(&redaction_target).await?.is_some(),
                        None => false,
                    }
                {
                    // The redacted event exists (in the room, because it
                    // contains _all_ the events) **but** the event has been
                    // redacted (in the room). It's no more possible to extract
                    // its thread root (because this information has been
                    // removed).
                    //
                    // But we need to know if the event is part of a thread to
                    // apply the redaction in the thread too. No other choice
                    // than doing a full search…

                    let mut associated_thread_root = None;

                    for thread in existing_threads.values() {
                        if thread.find_event(&redaction_target).await?.is_some() {
                            associated_thread_root = Some(thread.thread_id.clone());
                            break;
                        }
                    }

                    // We've found the thread owning the event being redacted!
                    if let Some(thread_root) = associated_thread_root {
                        new_events_by_thread
                            .entry(thread_root)
                            .or_insert_with(default_entry)
                            .0
                            .events
                            .push(event.clone());
                    }
                }
            }
        }
    }

    for (thread_root, read_receipts) in own_read_receipts {
        // Create an empty `Timeline` if it doesn't exist so that it triggers
        // the update for this thread in `Caches`. This is done by
        // `default_entry`.
        new_events_by_thread.entry(thread_root).or_insert_with(default_entry).1 = read_receipts;
    }

    Ok(new_events_by_thread)
}

pub fn aggregate_timeline_for_pinned_events(
    timeline: &Timeline,
    pinned_event_ids: &[OwnedEventId],
    redaction_rules: &RedactionRules,
) -> Timeline {
    let mut new_timeline = Timeline {
        limited: timeline.limited,
        prev_batch: timeline.prev_batch.clone(),
        events: Vec::new(),
    };

    // No events are pinned? The `Timeline` must be empty.
    if pinned_event_ids.is_empty() {
        return new_timeline;
    }

    // Look for events that relate to pinned events. We already know the
    // pinned-events, we don't need to look for them. We are only interested by
    // related events.
    for event in &timeline.events {
        match extract_relation(event.raw()) {
            // Ohh, this event relates to another event!
            Some((relation_type, related_event_id)) => match relation_type {
                // `event` relates to a thread: not what we want.
                RelationType::Thread => {}

                // `event` represents an annotation (e.g. reactions), a
                // replacement (an edit), a reference or something custom. Let's
                // see if the `related_event_id` is a pinned-event.
                RelationType::Annotation
                | RelationType::Replacement
                | RelationType::Reference
                | _ => {
                    if pinned_event_ids.contains(&related_event_id) {
                        new_timeline.events.push(event.clone());
                    }
                }
            },

            // No explicit relation, but it can be a redaction of a
            // pinned-event!
            None => {
                if let Some(redaction_target) =
                    extract_redaction_target(event.raw(), redaction_rules)
                    && pinned_event_ids.contains(&redaction_target)
                {
                    new_timeline.events.push(event.clone());
                }
            }
        }
    }

    new_timeline
}

/// The ephemeral events of a sync response, as needed by the caches.
///
/// Read receipts are grouped by the room or the thread they belong to. Each
/// group exists twice:
///
/// - with the receipts of all users, to be broadcast to the subscribers,
/// - with the receipts of the own user only, to compute the unread counts.
///
/// See [`Self::from_sync`].
pub struct EphemeralEvents {
    /// Read receipts of all users for the room, i.e. unthreaded or on the main
    /// timeline.
    pub room_read_receipts: Option<ReceiptEventContent>,

    /// Read receipts of the own user for the room.
    pub own_room_read_receipts: MaybeReceiptEventContent,

    /// Read receipts of all users for each thread, grouped by thread root.
    pub thread_read_receipts: BTreeMap<OwnedEventId, ReceiptEventContent>,

    /// Read receipts of the own user for each thread, grouped by thread root.
    pub own_thread_read_receipts: BTreeMap<OwnedEventId, MaybeReceiptEventContent>,
}

/// The room or thread a read receipt belongs to. This is a helper to be able to
/// categorize read receipts while using only references.
#[derive(Clone, Copy)]
enum ReadReceiptGroup<'a> {
    Room,
    Thread(&'a EventId),
}

impl<'a> ReadReceiptGroup<'a> {
    fn new(receipt_thread: &'a ReceiptThread) -> Option<Self> {
        match receipt_thread {
            ReceiptThread::Main | ReceiptThread::Unthreaded => Some(Self::Room),
            ReceiptThread::Thread(thread_root) => Some(Self::Thread(thread_root)),
            _ => None,
        }
    }
}

impl EphemeralEvents {
    /// Deserialize the ephemeral events of a sync response, and group their
    /// read receipts, in a single pass.
    ///
    /// Read receipts are moved out of the ephemeral events, not cloned. Only
    /// the receipts of the own user are cloned, since they live in two groups.
    pub fn from_sync(own_user_id: &UserId, ephemeral: Vec<Raw<AnySyncEphemeralRoomEvent>>) -> Self {
        let mut this = Self {
            room_read_receipts: None,
            own_room_read_receipts: MaybeReceiptEventContent::none(),
            thread_read_receipts: BTreeMap::new(),
            own_thread_read_receipts: BTreeMap::new(),
        };

        for event in ephemeral {
            // Read receipts are the only ephemeral events the caches care
            // about.
            let Ok(AnySyncEphemeralRoomEvent::Receipt(receipt_event)) = event.deserialize() else {
                continue;
            };

            for (event_id, receipts) in receipt_event.content.0 {
                for (receipt_type, user_receipts) in receipts {
                    for (user_id, receipt) in user_receipts {
                        let Some(group) = ReadReceiptGroup::new(&receipt.thread) else {
                            continue;
                        };

                        // The own user's receipts are needed twice: to compute
                        // the unread counts, and to be broadcast.
                        if user_id == own_user_id {
                            this.own_receipts_mut(group).extend(once((
                                &event_id,
                                (&receipt_type, (own_user_id, &receipt)),
                            )));
                        }

                        this.all_receipts_mut(group)
                            .0
                            .entry(event_id.clone())
                            .or_default()
                            .entry(receipt_type.clone())
                            .or_default()
                            .insert(user_id, receipt);
                    }
                }
            }
        }

        this
    }

    /// Get the receipts of all users for a group, creating it if needed.
    fn all_receipts_mut(&mut self, group: ReadReceiptGroup<'_>) -> &mut ReceiptEventContent {
        match group {
            ReadReceiptGroup::Room => {
                self.room_read_receipts.get_or_insert_with(|| ReceiptEventContent(BTreeMap::new()))
            }
            ReadReceiptGroup::Thread(thread_root) => self
                .thread_read_receipts
                .entry(thread_root.to_owned())
                .or_insert_with(|| ReceiptEventContent(BTreeMap::new())),
        }
    }

    /// Get the receipts of the own user for a group, creating it if needed.
    fn own_receipts_mut(&mut self, group: ReadReceiptGroup<'_>) -> &mut MaybeReceiptEventContent {
        match group {
            ReadReceiptGroup::Room => &mut self.own_room_read_receipts,
            ReadReceiptGroup::Thread(thread_root) => self
                .own_thread_read_receipts
                .entry(thread_root.to_owned())
                .or_insert_with(MaybeReceiptEventContent::none),
        }
    }
}

#[cfg(test)]
mod tests {
    use ruma::{event_id, events::receipt::ReceiptType, serde::Raw, user_id};
    use serde_json::json;

    use super::EphemeralEvents;

    fn receipt_event(content: serde_json::Value) -> Raw<super::AnySyncEphemeralRoomEvent> {
        Raw::new(&json!({ "type": "m.receipt", "content": content })).unwrap().cast_unchecked()
    }

    #[test]
    fn test_ephemeral_events_group_read_receipts() {
        let own = user_id!("@me:server.org");
        let alice = user_id!("@alice:server.org");
        let bob = user_id!("@bob:server.org");
        let thread_root = event_id!("$thread_root");

        let ephemeral = vec![
            // Not a read receipt: ignored.
            Raw::new(&json!({ "type": "m.typing", "content": { "user_ids": [] } }))
                .unwrap()
                .cast_unchecked(),
            receipt_event(json!({
                // Receipts in different groups (split path).
                "$ev": {
                    "m.read": {
                        "@alice:server.org": { "ts": 1, "thread_id": "$thread_root" },
                        "@me:server.org": { "ts": 2 },
                    },
                    "m.read.private": {
                        "@me:server.org": { "ts": 3, "thread_id": "$thread_root" },
                    },
                },
                // Receipts all in the same group (fast path).
                "$other": {
                    "m.read": { "@bob:server.org": { "ts": 4, "thread_id": "$thread_root" } },
                },
            })),
            // Another receipt for `$other`, to be merged.
            receipt_event(json!({
                "$other": {
                    "m.read": { "@alice:server.org": { "ts": 5, "thread_id": "$thread_root" } },
                },
            })),
        ];

        let EphemeralEvents {
            room_read_receipts,
            own_room_read_receipts,
            thread_read_receipts,
            own_thread_read_receipts,
        } = EphemeralEvents::from_sync(own, ephemeral);

        // Room, all users: only the own unthreaded receipt.
        let room = room_read_receipts.unwrap();
        assert_eq!(room.len(), 1);
        let receipts = &room[event_id!("$ev")];
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[&ReceiptType::Read].keys().collect::<Vec<_>>(), [own]);

        // Room, own user: the same.
        let own_room = own_room_read_receipts.into_inner().unwrap();
        assert_eq!(own_room.len(), 1);
        let receipts = &own_room[event_id!("$ev")];
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[&ReceiptType::Read].keys().collect::<Vec<_>>(), [own]);

        // Thread, all users.
        assert_eq!(thread_read_receipts.len(), 1);
        let thread = &thread_read_receipts[thread_root];
        assert_eq!(thread.len(), 2);

        let receipts = &thread[event_id!("$ev")];
        assert_eq!(receipts[&ReceiptType::Read].keys().collect::<Vec<_>>(), [alice]);
        assert_eq!(receipts[&ReceiptType::ReadPrivate].keys().collect::<Vec<_>>(), [own]);

        let receipts = &thread[event_id!("$other")];
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[&ReceiptType::Read].keys().collect::<Vec<_>>(), [alice, bob]);

        // Thread, own user: only the private receipt.
        assert_eq!(own_thread_read_receipts.len(), 1);
        let own_thread =
            own_thread_read_receipts.into_iter().next().unwrap().1.into_inner().unwrap();
        assert_eq!(own_thread.len(), 1);
        let receipts = &own_thread[event_id!("$ev")];
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[&ReceiptType::ReadPrivate].keys().collect::<Vec<_>>(), [own]);
    }

    #[test]
    fn test_ephemeral_events_without_own_read_receipts() {
        let own = user_id!("@me:server.org");
        let ephemeral = vec![receipt_event(json!({
            "$ev": { "m.read": { "@alice:server.org": { "ts": 1 } } },
        }))];

        let events = EphemeralEvents::from_sync(own, ephemeral);

        assert!(events.room_read_receipts.is_some());
        assert!(events.own_room_read_receipts.is_none());
        assert!(events.thread_read_receipts.is_empty());
        assert!(events.own_thread_read_receipts.is_empty());
    }
}
