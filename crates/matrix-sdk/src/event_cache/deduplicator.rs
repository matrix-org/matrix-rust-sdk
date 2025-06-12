// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! Simple but efficient types to find duplicated events. See [`Deduplicator`]
//! to learn more.

use std::collections::BTreeSet;

use matrix_sdk_base::{
    event_cache::store::EventCacheStoreLock,
    linked_chunk::{LinkedChunkId, Position},
};
use ruma::{OwnedEventId, RoomId};

use super::{
    room::events::{Event, RoomEvents},
    EventCacheError,
};

/// Find duplicates in the given collection of events, and return both
/// valid events (those with an event id) as well as the event ids of
/// duplicate events along with their position.
pub async fn filter_duplicate_events(
    room_id: &RoomId,
    store: &EventCacheStoreLock,
    mut events: Vec<Event>,
    room_events: &RoomEvents,
) -> Result<DeduplicationOutcome, EventCacheError> {
    // Remove all events with no ID, or that is duplicated inside `events`, i.e.
    // `events` contains duplicated events in itself, e.g. `[$e0, $e1, $e0]`, here
    // `$e0` is duplicated in within `events`.
    {
        let mut event_ids = BTreeSet::new();

        events.retain(|event| {
            let Some(event_id) = event.event_id() else {
                // No event ID? Bye bye.
                return false;
            };

            // Already seen this event in `events`? Bye bye.
            if event_ids.contains(&event_id) {
                return false;
            }

            event_ids.insert(event_id);

            // Let's keep this event!
            true
        });
    }

    let store = store.lock().await?;

    // Let the store do its magic ✨
    let duplicated_event_ids = store
        .filter_duplicated_events(
            LinkedChunkId::Room(room_id),
            events.iter().filter_map(|event| event.event_id()).collect(),
        )
        .await?;

    // Separate duplicated events in two collections: ones that are in-memory, ones
    // that are in the store.
    let (in_memory_duplicated_event_ids, in_store_duplicated_event_ids) = {
        // Collect all in-memory chunk identifiers.
        let in_memory_chunk_identifiers =
            room_events.chunks().map(|chunk| chunk.identifier()).collect::<Vec<_>>();

        let mut in_memory = vec![];
        let mut in_store = vec![];

        for (duplicated_event_id, position) in duplicated_event_ids {
            if in_memory_chunk_identifiers.contains(&position.chunk_identifier()) {
                in_memory.push((duplicated_event_id, position));
            } else {
                in_store.push((duplicated_event_id, position));
            }
        }

        (in_memory, in_store)
    };

    Ok(DeduplicationOutcome {
        all_events: events,
        in_memory_duplicated_event_ids,
        in_store_duplicated_event_ids,
    })
}

pub(super) struct DeduplicationOutcome {
    /// All events passed to the deduplicator.
    ///
    /// All events in this collection have a valid event ID.
    ///
    /// This collection does not contain duplicated events in itself.
    pub all_events: Vec<Event>,

    /// Events in [`Self::all_events`] that are duplicated and present in
    /// memory. It means they have been loaded from the store if any.
    ///
    /// Events are sorted by their position, from the newest to the oldest
    /// (position is descending).
    pub in_memory_duplicated_event_ids: Vec<(OwnedEventId, Position)>,

    /// Events in [`Self::all_events`] that are duplicated and present in
    /// the store. It means they have **NOT** been loaded from the store into
    /// memory yet.
    ///
    /// Events are sorted by their position, from the newest to the oldest
    /// (position is descending).
    pub in_store_duplicated_event_ids: Vec<(OwnedEventId, Position)>,
}

#[cfg(test)]
#[cfg(not(target_family = "wasm"))] // These tests uses the cross-process lock, so need time support.
mod tests {
    use matrix_sdk_base::{deserialized_responses::TimelineEvent, linked_chunk::ChunkIdentifier};
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{owned_event_id, serde::Raw, user_id, EventId};

    use super::*;

    fn timeline_event(event_id: &EventId) -> TimelineEvent {
        EventFactory::new()
            .text_msg("")
            .sender(user_id!("@mnt_io:matrix.org"))
            .event_id(event_id)
            .into_event()
    }

    #[async_test]
    async fn test_store_based_duplicated_event_ids_from_in_memory_vs_in_store() {
        use std::sync::Arc;

        use matrix_sdk_base::{
            event_cache::store::{EventCacheStore, MemoryStore},
            linked_chunk::Update,
        };
        use ruma::room_id;

        let event_id_0 = owned_event_id!("$ev0");
        let event_id_1 = owned_event_id!("$ev1");
        let event_id_2 = owned_event_id!("$ev2");
        let event_id_3 = owned_event_id!("$ev3");
        let event_id_4 = owned_event_id!("$ev4");

        // `event_0` and `event_1` are in the store.
        // `event_2` and `event_3` is in the store, but also in memory: it's loaded in
        // memory from the store.
        // `event_4` is nowhere, it's new.
        let event_0 = timeline_event(&event_id_0);
        let event_1 = timeline_event(&event_id_1);
        let event_2 = timeline_event(&event_id_2);
        let event_3 = timeline_event(&event_id_3);
        let event_4 = timeline_event(&event_id_4);

        let event_cache_store = Arc::new(MemoryStore::new());
        let room_id = room_id!("!fondue:raclette.ch");

        // Prefill the store with ev1 and ev2.
        event_cache_store
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![event_0.clone(), event_1.clone()],
                    },
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(42)),
                        new: ChunkIdentifier::new(0), // must match the chunk in `RoomEvents`, so 0. It simulates a lazy-load for example.
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(0), 0),
                        items: vec![event_2.clone(), event_3.clone()],
                    },
                ],
            )
            .await
            .unwrap();

        let event_cache_store = EventCacheStoreLock::new(event_cache_store, "hodor".to_owned());

        let mut room_events = RoomEvents::new();
        room_events.push_events([event_2.clone(), event_3.clone()]);

        let outcome = filter_duplicate_events(
            room_id,
            &event_cache_store,
            vec![event_0, event_1, event_2, event_3, event_4],
            &room_events,
        )
        .await
        .unwrap();

        // The deduplication says 5 events are valid.
        assert_eq!(outcome.all_events.len(), 5);
        assert_eq!(outcome.all_events[0].event_id(), Some(event_id_0.clone()));
        assert_eq!(outcome.all_events[1].event_id(), Some(event_id_1.clone()));
        assert_eq!(outcome.all_events[2].event_id(), Some(event_id_2.clone()));
        assert_eq!(outcome.all_events[3].event_id(), Some(event_id_3.clone()));
        assert_eq!(outcome.all_events[4].event_id(), Some(event_id_4.clone()));

        // From these 5 events, 2 are duplicated and have been loaded in memory.
        //
        // Note that events are sorted by their descending position.
        assert_eq!(outcome.in_memory_duplicated_event_ids.len(), 2);
        assert_eq!(
            outcome.in_memory_duplicated_event_ids[0],
            (event_id_2, Position::new(ChunkIdentifier::new(0), 0))
        );
        assert_eq!(
            outcome.in_memory_duplicated_event_ids[1],
            (event_id_3, Position::new(ChunkIdentifier::new(0), 1))
        );

        // From these 4 events, 2 are duplicated and live in the store only, they have
        // not been loaded in memory.
        //
        // Note that events are sorted by their descending position.
        assert_eq!(outcome.in_store_duplicated_event_ids.len(), 2);
        assert_eq!(
            outcome.in_store_duplicated_event_ids[0],
            (event_id_0, Position::new(ChunkIdentifier::new(42), 0))
        );
        assert_eq!(
            outcome.in_store_duplicated_event_ids[1],
            (event_id_1, Position::new(ChunkIdentifier::new(42), 1))
        );
    }

    #[async_test]
    async fn test_storage_deduplication() {
        use std::sync::Arc;

        use matrix_sdk_base::{
            event_cache::store::{EventCacheStore as _, MemoryStore},
            linked_chunk::{ChunkIdentifier, Position, Update},
        };
        use matrix_sdk_test::{ALICE, BOB};
        use ruma::{event_id, room_id};

        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let eid1 = event_id!("$1");
        let eid2 = event_id!("$2");
        let eid3 = event_id!("$3");

        let ev1 = f.text_msg("hello world").sender(*ALICE).event_id(eid1).into_event();
        let ev2 = f.text_msg("how's it going").sender(*BOB).event_id(eid2).into_event();
        let ev3 = f.text_msg("wassup").sender(*ALICE).event_id(eid3).into_event();
        // An invalid event (doesn't have an event id.).
        let ev4 = TimelineEvent::from_plaintext(Raw::from_json_string("{}".to_owned()).unwrap());

        // Prefill the store with ev1 and ev2.
        event_cache_store
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
                vec![
                    // Non empty items chunk.
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![ev1.clone()],
                    },
                    // And another items chunk, non-empty again.
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(42)),
                        new: ChunkIdentifier::new(43),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(43), 0),
                        items: vec![ev2.clone()],
                    },
                ],
            )
            .await
            .unwrap();

        // Wrap the store into its lock.
        let event_cache_store = EventCacheStoreLock::new(event_cache_store, "hodor".to_owned());

        let room_events = RoomEvents::new();
        let DeduplicationOutcome {
            all_events: events,
            in_memory_duplicated_event_ids,
            in_store_duplicated_event_ids,
        } = filter_duplicate_events(
            room_id,
            &event_cache_store,
            vec![ev1, ev2, ev3, ev4],
            &room_events,
        )
        .await
        .unwrap();

        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event_id().as_deref(), Some(eid1));
        assert_eq!(events[1].event_id().as_deref(), Some(eid2));
        assert_eq!(events[2].event_id().as_deref(), Some(eid3));

        assert!(in_memory_duplicated_event_ids.is_empty());

        assert_eq!(in_store_duplicated_event_ids.len(), 2);
        assert_eq!(
            in_store_duplicated_event_ids[0],
            (eid1.to_owned(), Position::new(ChunkIdentifier::new(42), 0))
        );
        assert_eq!(
            in_store_duplicated_event_ids[1],
            (eid2.to_owned(), Position::new(ChunkIdentifier::new(43), 0))
        );
    }
}
