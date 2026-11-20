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
    OwnedEventId, UserId,
    events::{
        AnySyncEphemeralRoomEvent,
        receipt::{ReceiptEventContent, ReceiptThread},
        relation::RelationType,
    },
    room_version_rules::RedactionRules,
};

use super::{
    super::{Result, states::StateLockReadGuard},
    read_receipts::{MaybeReceiptEventContent, ReadReceiptTuple},
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
        .map(|(_group, read_receipts)| read_receipts)
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

    for (thread_root, read_receipt) in
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
            .extend(once(read_receipt));
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

/// Read receipts, from all users, to be broadcast to the room and thread
/// caches' subscribers.
///
/// See [`group_all_read_receipts_by_thread`].
#[derive(Debug, Default)]
pub struct ReadReceiptsToBroadcast {
    /// Read receipts for the room, i.e. unthreaded or on the main timeline.
    pub room: Option<ReceiptEventContent>,

    /// Read receipts for each thread, keyed by thread root.
    pub threads: BTreeMap<OwnedEventId, ReceiptEventContent>,
}

/// Group all the read receipts found in (deserialised) ephemeral events, for
/// all users, by the room or the thread they belong to.
///
/// Contrary to [`aggregate_timeline_and_read_receipts_for_room`] and
/// [`aggregate_timeline_and_read_receipts_for_threads`], which only keep the
/// receipts of the own user (used to compute the unread counts), this keeps
/// receipts from every user, so that they can be broadcast to subscribers.
pub fn group_all_read_receipts_by_thread(
    events: &[AnySyncEphemeralRoomEvent],
) -> ReadReceiptsToBroadcast {
    let mut result = ReadReceiptsToBroadcast::default();

    let receipt_contents = events.iter().filter_map(|ephemeral| match ephemeral {
        AnySyncEphemeralRoomEvent::Receipt(receipt_event) => Some(&receipt_event.content),
        _ => None,
    });

    for content in receipt_contents {
        for (event_id, receipts) in content.iter() {
            for (receipt_type, user_receipts) in receipts {
                for (user_id, receipt) in user_receipts {
                    let target = match &receipt.thread {
                        ReceiptThread::Main | ReceiptThread::Unthreaded => {
                            result.room.get_or_insert_with(|| ReceiptEventContent(BTreeMap::new()))
                        }
                        ReceiptThread::Thread(thread_root) => result
                            .threads
                            .entry(thread_root.clone())
                            .or_insert_with(|| ReceiptEventContent(BTreeMap::new())),
                        _ => continue,
                    };

                    target
                        .entry(event_id.clone())
                        .or_default()
                        .entry(receipt_type.clone())
                        .or_default()
                        .insert(user_id.clone(), receipt.clone());
                }
            }
        }
    }

    result
}

/// Filter (deserialised) ephemeral events to only keep the read receipts
/// matching a particular predicate and user id.
///
/// Read receipts have a deep structure (an entanglement of `BTreeMap`). The
/// returned iterator returns the tuple `(OwnedEventId, (ReceiptType, (UserId,
/// Receipt)))`. However, the `predicate` is applied on the `Receipt::thread`
/// field, the leaf of the read receipt.
///
/// Only the receipts matching `user_id` are kept.
///
/// The predicate is used with [`Iterator::filter_map`], and thus can return a
/// group key (it can be anything). This group key is associated to the tuple
/// mentioned earlier, and should be used to “group” read receipts. This is
/// useful when one wants to group read receipts by their `ReceiptThread` for
/// example. The predicate is evaluated for each receipt, so the receipts (of
/// different types) for the same event can end up in different groups.
fn filter_read_receipts_and_group_by<'e, F, G>(
    user_id: &'e UserId,
    events: &'e [AnySyncEphemeralRoomEvent],
    predicate: F,
) -> impl Iterator<Item = (G, ReadReceiptTuple<'e>)>
where
    F: Fn(&'e ReceiptThread) -> Option<G>,
{
    events
        .iter()
        .filter_map(|ephemeral| match ephemeral {
            AnySyncEphemeralRoomEvent::Receipt(receipt_event) => Some(receipt_event),
            _ => None,
        })
        .flat_map(|receipt_event| receipt_event.content.iter())
        .flat_map(|(event_id, event_receipts)| {
            event_receipts.iter().map(move |(receipt_type, user_ids_and_receipts)| {
                (event_id, (receipt_type, user_ids_and_receipts))
            })
        })
        .flat_map(|(event_id, (receipt_type, user_ids_and_receipts))| {
            user_ids_and_receipts
                // Filter by `user_id`.
                .get(user_id)
                .into_iter()
                .map(move |receipt| (event_id, (receipt_type, receipt)))
        })
        .filter_map(move |(event_id, (receipt_type, receipt))| {
            Some((predicate(&receipt.thread)?, (event_id, (receipt_type, (user_id, receipt)))))
        })
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::async_test;
    use ruma::{events::receipt::ReceiptType, user_id};
    use serde_json::json;
    use strass::assert_let;

    use super::{
        AnySyncEphemeralRoomEvent, ReceiptThread, aggregate_timeline_and_read_receipts_for_room,
        filter_read_receipts_and_group_by,
    };

    fn receipt_event(content: serde_json::Value) -> AnySyncEphemeralRoomEvent {
        serde_json::from_value(json!({ "type": "m.receipt", "content": content })).unwrap()
    }

    #[async_test]
    async fn test_filter_read_receipts_only_keeps_own_user_and_groups_per_receipt() {
        let own = user_id!("@me:server.org");

        // On `$ev`, `@alice` has a threaded receipt, while the own user has an
        // unthreaded one (read) and a threaded one (private). On `$other`, only
        // `@bob` has a receipt.
        let ephemeral = vec![receipt_event(json!({
            "$ev": {
                "m.read": {
                    "@alice:server.org": { "ts": 1, "thread_id": "$thread_root" },
                    "@me:server.org": { "ts": 2 },
                },
                "m.read.private": {
                    "@me:server.org": { "ts": 3, "thread_id": "$thread_root" },
                },
            },
            "$other": {
                "m.read": { "@bob:server.org": { "ts": 4, "thread_id": "$thread_root" } },
            },
        }))];

        // For a room timeline.
        let mut iterator =
            filter_read_receipts_and_group_by(own, &ephemeral, |thread| match thread {
                ReceiptThread::Main | ReceiptThread::Unthreaded => Some(()),
                _ => None,
            })
            .map(|(_group, read_receipts)| read_receipts);

        assert_let!(Some((event_id, (receipt_type, (user_id, _receipt)))) = iterator.next());
        assert_eq!(event_id.as_str(), "$ev");
        assert_eq!(receipt_type, &ReceiptType::Read);
        assert_eq!(user_id, own);

        assert!(iterator.next().is_none());

        // For a thread timeline.
        let mut iterator =
            filter_read_receipts_and_group_by(own, &ephemeral, |thread| match thread {
                ReceiptThread::Thread(thread_id) => Some(thread_id),
                _ => None,
            })
            .map(|(_group, read_receipts)| read_receipts);

        assert_let!(Some((event_id, (receipt_type, (user_id, _receipt)))) = iterator.next());
        assert_eq!(event_id.as_str(), "$ev");
        assert_eq!(receipt_type, &ReceiptType::ReadPrivate);
        assert_eq!(user_id, own);

        assert!(iterator.next().is_none());
    }

    #[async_test]
    async fn test_room_read_receipts_ignore_other_users() {
        let own = user_id!("@me:server.org");
        let ephemeral = vec![receipt_event(json!({
            "$ev": { "m.read": { "@alice:server.org": { "ts": 1 } } },
        }))];

        let (_, read_receipts) =
            aggregate_timeline_and_read_receipts_for_room(own, &Default::default(), &ephemeral);

        assert!(read_receipts.is_none());
    }
}
