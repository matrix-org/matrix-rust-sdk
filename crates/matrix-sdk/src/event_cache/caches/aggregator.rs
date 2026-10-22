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

use std::collections::{BTreeMap, HashMap};

use matrix_sdk_base::{
    serde_helpers::{extract_redaction_target, extract_relation, extract_thread_root},
    sync::Timeline,
};
use ruma::{
    OwnedEventId, UserId,
    events::{
        AnySyncEphemeralRoomEvent,
        receipt::{ReceiptThread, Receipts},
        relation::RelationType,
    },
    room_version_rules::RedactionRules,
};

use super::{
    super::{Result, states::StateLockReadGuard},
    read_receipts::MaybeReceiptEventContent,
    room::RoomEventCacheState,
    thread::ThreadEventCacheState,
};

pub fn aggregate_timeline_and_read_receipts_for_room(
    own_user_id: &UserId,
    timeline: &Timeline,
    ephemeral: &[AnySyncEphemeralRoomEvent],
) -> (Timeline, MaybeReceiptEventContent) {
    (
        timeline.clone(),
        filter_read_receipts_and_group_by(own_user_id, ephemeral, |receipt_thread| {
            match receipt_thread {
                ReceiptThread::Main | ReceiptThread::Unthreaded => Some(()),
                _ => None,
            }
        })
        .map(|(_receipt_thread, (event_id, event_receipts))| (event_id.clone(), event_receipts))
        .collect(),
    )
}

pub async fn aggregate_timeline_and_read_receipts_for_threads<'sync, 'state>(
    own_user_id: &'sync UserId,
    timeline: &'sync Timeline,
    ephemeral: &'sync [AnySyncEphemeralRoomEvent],
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
                    // First, look for the related event in `timeline`
                    // backwards.
                    if let Some(thread_root) = match timeline.events[..nth]
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

            // No explicit relation, okay, but it can still be related to a thread!
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

    for (thread_root, (read_receipt_event_id, read_receipt_event)) in
        filter_read_receipts_and_group_by(own_user_id, ephemeral, |receipt_thread| {
            match receipt_thread {
                ReceiptThread::Thread(thread_id) => Some(thread_id),
                _ => None,
            }
        })
    {
        // 1. Create an empty `Timeline` if it doesn't exist so that it triggers
        //    the update for this thread in `Caches`. This is done by
        //    `default_entry`.
        // 2. Accumulate the read receipt event.
        new_events_by_thread
            .entry(thread_root.to_owned())
            .or_insert_with(default_entry)
            .1
            .add(read_receipt_event_id.clone(), read_receipt_event);
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

            // No explicit relation, but it can be a redaction of a pinned-event!
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

/// Filter (deserialised) ephemeral events to only keep the read receipts
/// matching a particular predicate and user id.
///
/// Read receipts have a deep structure (an entanglement of `BTreeMap`). The
/// returned iterator returns the tuple `(OwnedEventId, Receipts)`, which is the
/// second level. However, the `predicate` is applied on the fourth level,
/// directly on the leaf of the read receipts.
///
/// The predicate will return a group key (it can be anything). This group key
/// is associated to the tuple mentioned earlier, and should be used to “group”
/// read receipts. This is useful when one wants to group read receipts by their
/// `ReceiptThread` for example.
///
/// The same event can hold receipts for different threads (e.g. a thread root
/// can have a `Main` receipt from one user and a `Thread` receipt from
/// another), so one event can be returned once per group key, and each
/// returned `Receipts` only contains the leaves matching that group key.
fn filter_read_receipts_and_group_by<'e, F, G>(
    user_id: &'e UserId,
    events: &'e [AnySyncEphemeralRoomEvent],
    predicate: F,
) -> impl Iterator<Item = (G, (&'e OwnedEventId, Receipts))>
where
    F: Fn(&'e ReceiptThread) -> Option<G>,
    G: Ord,
{
    events
        .iter()
        .filter_map(|ephemeral| match ephemeral {
            AnySyncEphemeralRoomEvent::Receipt(receipt_event) => Some(receipt_event),
            _ => None,
        })
        .flat_map(|receipt_event| receipt_event.content.iter())
        .flat_map(move |(event_id, event_receipts)| {
            let mut groups = BTreeMap::<G, Receipts>::new();

            for (receipt_type, user_receipts) in event_receipts {
                // Only process receipts for the matching user id
                let Some(receipt) = user_receipts.get(user_id) else { continue };
                
                if let Some(group) = predicate(&receipt.thread) {
                    groups
                        .entry(group)
                        .or_default()
                        .entry(receipt_type.clone())
                        .or_default()
                        .insert(user_id.to_owned(), receipt.clone());
                }
            }

            groups.into_iter().map(move |(group, receipts)| (group, (event_id, receipts)))
        })
}
