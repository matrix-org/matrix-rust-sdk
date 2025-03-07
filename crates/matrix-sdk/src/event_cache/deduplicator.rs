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

use std::{collections::BTreeSet, fmt, sync::Mutex};

use growable_bloom_filter::{GrowableBloom, GrowableBloomBuilder};
use matrix_sdk_base::{event_cache::store::EventCacheStoreLock, linked_chunk::Position};
use ruma::{OwnedEventId, OwnedRoomId};
use tracing::{debug, error};

use super::{
    room::events::{Event, RoomEvents},
    EventCacheError,
};

/// A `Deduplicator` helps to find duplicate events.
pub enum Deduplicator {
    InMemory(BloomFilterDeduplicator),
    PersistentStore(StoreDeduplicator),
}

impl Deduplicator {
    /// Create an empty deduplicator instance that uses an internal Bloom
    /// filter.
    ///
    /// Such a deduplicator is stateful, with no initial known events, and it
    /// will learn over time by using a Bloom filter which events are
    /// duplicates or not.
    ///
    /// When the persistent storage of the event cache is enabled by default,
    /// this constructor (and the associated variant) will be removed.
    pub fn new_memory_based() -> Self {
        Self::InMemory(BloomFilterDeduplicator::new())
    }

    /// Create new store-based deduplicator that will run queries against the
    /// store to find if any event is deduplicated or not.
    ///
    /// This deduplicator is stateless.
    ///
    /// When the persistent storage of the event cache is enabled by default,
    /// this will become the default, and [`Deduplicator`] will be replaced
    /// with [`StoreDeduplicator`].
    pub fn new_store_based(room_id: OwnedRoomId, store: EventCacheStoreLock) -> Self {
        Self::PersistentStore(StoreDeduplicator { room_id, store })
    }

    /// Find duplicates in the given collection of events, and return both
    /// valid events (those with an event id) as well as the event ids of
    /// duplicate events along with their position.
    pub async fn filter_duplicate_events(
        &self,
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

        Ok(match self {
            Deduplicator::InMemory(dedup) => dedup.filter_duplicate_events(events, room_events),
            Deduplicator::PersistentStore(dedup) => {
                dedup.filter_duplicate_events(events, room_events).await?
            }
        })
    }
}

/// A deduplication mechanism based on the persistent storage associated to the
/// event cache.
///
/// It will use queries to the persistent storage to figure when events are
/// duplicates or not, making it entirely stateless.
pub struct StoreDeduplicator {
    /// The room this deduplicator applies to.
    room_id: OwnedRoomId,
    /// The actual event cache store implementation used to query events.
    store: EventCacheStoreLock,
}

impl StoreDeduplicator {
    async fn filter_duplicate_events(
        &self,
        events: Vec<Event>,
        room_events: &RoomEvents,
    ) -> Result<DeduplicationOutcome, EventCacheError> {
        let store = self.store.lock().await?;

        // Let the store do its magic ✨
        let duplicated_event_ids = store
            .filter_duplicated_events(
                &self.room_id,
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
}

/// `BloomFilterDeduplicator` is an efficient type to find duplicated events,
/// using an in-memory cache.
///
/// It uses a [bloom filter] to provide a memory efficient probabilistic answer
/// to: “has event E been seen already?”. False positives are possible, while
/// false negatives are impossible. In the case of a positive reply, we fallback
/// to a linear (backward) search on all events to check whether it's a false
/// positive or not
///
/// [bloom filter]: https://en.wikipedia.org/wiki/Bloom_filter
pub struct BloomFilterDeduplicator {
    bloom_filter: Mutex<GrowableBloom>,
}

impl fmt::Debug for BloomFilterDeduplicator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Deduplicator").finish_non_exhaustive()
    }
}

impl BloomFilterDeduplicator {
    // Note: don't use too high numbers here, or the amount of allocated memory will
    // explode. See https://github.com/matrix-org/matrix-rust-sdk/pull/4231 for details.
    const APPROXIMATED_MAXIMUM_NUMBER_OF_EVENTS: usize = 1_000;
    const DESIRED_FALSE_POSITIVE_RATE: f64 = 0.01;

    /// Create a new `Deduplicator` with no prior knowledge of known events.
    fn new() -> Self {
        let bloom_filter = GrowableBloomBuilder::new()
            .estimated_insertions(Self::APPROXIMATED_MAXIMUM_NUMBER_OF_EVENTS)
            .desired_error_ratio(Self::DESIRED_FALSE_POSITIVE_RATE)
            .build();
        Self { bloom_filter: Mutex::new(bloom_filter) }
    }

    /// Find duplicates in the given collection of events, and return both
    /// valid events (those with an event id) as well as the event ids of
    /// duplicate events along with their position.
    fn filter_duplicate_events(
        &self,
        events: Vec<Event>,
        room_events: &RoomEvents,
    ) -> DeduplicationOutcome {
        let mut duplicated_event_ids = Vec::new();

        let events = self
            .scan_and_learn(events.into_iter(), room_events)
            .map(|decorated_event| match decorated_event {
                Decoration::Unique(event) => event,
                Decoration::Duplicated((event, position)) => {
                    debug!(event_id = ?event.event_id(), "Found a duplicated event");

                    let event_id = event
                        .event_id()
                        // SAFETY: An event with no ID is not possible, as invalid events are
                        // already filtered out. Thus, it's safe to unwrap the
                        // `Option<OwnedEventId>` here.
                        .expect("The event has no ID");

                    duplicated_event_ids.push((event_id, position));

                    // Keep the new event!
                    event
                }
            })
            .collect::<Vec<_>>();

        DeduplicationOutcome {
            all_events: events,
            in_memory_duplicated_event_ids: duplicated_event_ids,
            in_store_duplicated_event_ids: vec![],
        }
    }

    /// Scan a collection of events and detect duplications.
    ///
    /// This method takes a collection of events `new_events_to_scan` and
    /// returns a new collection of events, where each event is decorated by
    /// a [`Decoration`], so that the caller can decide what to do with
    /// these events.
    ///
    /// Each scanned event will update `Self`'s internal state.
    ///
    /// `existing_events` represents all events of a room that already exist.
    fn scan_and_learn<'a, I>(
        &'a self,
        new_events_to_scan: I,
        existing_events: &'a RoomEvents,
    ) -> impl Iterator<Item = Decoration<I::Item>> + 'a
    where
        I: Iterator<Item = Event> + 'a,
    {
        new_events_to_scan.filter_map(move |event| {
            let Some(event_id) = event.event_id() else {
                // The event has no `event_id`. This is normally unreachable as event with no ID
                // are already filtered out.
                error!(?event, "Found an event with no ID");
                return None;
            };

            Some(if self.bloom_filter.lock().unwrap().check_and_set(&event_id) {
                // Oh oh, it looks like we have found a duplicate!
                //
                // However, bloom filters have false positives. We are NOT sure the event is NOT
                // present. Even if the false positive rate is low, we need to
                // iterate over all events to ensure it isn't present.
                //
                // We can iterate over all events to ensure `event` is not present in
                // `existing_events`.
                let position_of_the_duplicated_event =
                    existing_events.revents().find_map(|(position, other_event)| {
                        (other_event.event_id().as_ref() == Some(&event_id)).then_some(position)
                    });

                if let Some(position) = position_of_the_duplicated_event {
                    Decoration::Duplicated((event, position))
                } else {
                    Decoration::Unique(event)
                }
            } else {
                // Bloom filter has no false negatives. We are sure the event is NOT present: we
                // can keep it in the iterator.
                Decoration::Unique(event)
            })
        })
    }
}

/// Information about the scanned collection of events.
#[derive(Debug)]
enum Decoration<I> {
    /// This event is not duplicated.
    Unique(I),

    /// This event is duplicated.
    Duplicated((I, Position)),
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
mod tests {
    use assert_matches2::{assert_let, assert_matches};
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
    async fn test_filter_find_duplicates_in_the_input() {
        let event_id_0 = owned_event_id!("$ev0");
        let event_id_1 = owned_event_id!("$ev1");

        let event_0 = timeline_event(&event_id_0);
        let event_1 = timeline_event(&event_id_1);

        // It doesn't matter which deduplicator we peak, the feature is ensured by the
        // “frontend”, not the “backend” of the deduplicator.
        let deduplicator = Deduplicator::new_memory_based();
        let room_events = RoomEvents::new();

        let outcome = deduplicator
            .filter_duplicate_events(
                vec![
                    event_0.clone(), // Ok
                    event_1,         // Ok
                    event_0,         // Duplicated
                ],
                &room_events,
            )
            .await
            .unwrap();

        // We get 2 events, not 3, because one was duplicated.
        assert_eq!(outcome.all_events.len(), 2);
        assert_eq!(outcome.all_events[0].event_id(), Some(event_id_0));
        assert_eq!(outcome.all_events[1].event_id(), Some(event_id_1));
    }

    #[async_test]
    async fn test_filter_exclude_invalid_events_from_the_input() {
        let event_id_0 = owned_event_id!("$ev0");
        let event_id_1 = owned_event_id!("$ev1");

        let event_0 = timeline_event(&event_id_0);
        let event_1 = timeline_event(&event_id_1);
        // An event with no ID.
        let event_2 = TimelineEvent::new(Raw::from_json_string("{}".to_owned()).unwrap());

        // It doesn't matter which deduplicator we peak, the feature is ensured by the
        // “frontend”, not the “backend” of the deduplicator.
        let deduplicator = Deduplicator::new_memory_based();
        let room_events = RoomEvents::new();

        let outcome = deduplicator
            .filter_duplicate_events(
                vec![
                    event_0.clone(), // Ok
                    event_1,         // Ok
                    event_2,         // Invalid
                ],
                &room_events,
            )
            .await
            .unwrap();

        // We get 2 events, not 3, because one was invalid.
        assert_eq!(outcome.all_events.len(), 2);
        assert_eq!(outcome.all_events[0].event_id(), Some(event_id_0));
        assert_eq!(outcome.all_events[1].event_id(), Some(event_id_1));
    }

    #[async_test]
    async fn test_memory_based_duplicated_event_ids_from_in_memory_vs_in_store() {
        let event_id_0 = owned_event_id!("$ev0");
        let event_id_1 = owned_event_id!("$ev1");

        let event_0 = timeline_event(&event_id_0);
        let event_1 = timeline_event(&event_id_1);

        let mut deduplicator = Deduplicator::new_memory_based();
        let mut room_events = RoomEvents::new();
        // `event_0` is loaded in memory.
        // `event_1` is not loaded in memory, it's new.
        {
            let Deduplicator::InMemory(bloom_filter) = &mut deduplicator else {
                panic!("test is broken, but sky is beautiful");
            };
            bloom_filter.bloom_filter.lock().unwrap().insert(event_id_0.clone());
            room_events.push_events([event_0.clone()]);
        }

        let outcome = deduplicator
            .filter_duplicate_events(vec![event_0, event_1], &room_events)
            .await
            .unwrap();

        // The deduplication says 2 events are valid.
        assert_eq!(outcome.all_events.len(), 2);
        assert_eq!(outcome.all_events[0].event_id(), Some(event_id_0.clone()));
        assert_eq!(outcome.all_events[1].event_id(), Some(event_id_1));

        // From these 2 events, 1 is duplicated and has been loaded in memory.
        assert_eq!(outcome.in_memory_duplicated_event_ids.len(), 1);
        assert_eq!(
            outcome.in_memory_duplicated_event_ids[0],
            (event_id_0, Position::new(ChunkIdentifier::new(0), 0))
        );

        // From these 2 events, 0 are duplicated and live in the store.
        //
        // Note: with the Bloom filter, this value is always empty because there is no
        // store.
        assert!(outcome.in_store_duplicated_event_ids.is_empty());
    }

    #[cfg(not(target_arch = "wasm32"))] // This uses the cross-process lock, so needs time support.
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
                room_id,
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

        let deduplicator = Deduplicator::new_store_based(room_id.to_owned(), event_cache_store);
        let mut room_events = RoomEvents::new();
        room_events.push_events([event_2.clone(), event_3.clone()]);

        let outcome = deduplicator
            .filter_duplicate_events(
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

    #[test]
    fn test_bloom_filter_no_duplicate() {
        let event_id_0 = owned_event_id!("$ev0");
        let event_id_1 = owned_event_id!("$ev1");
        let event_id_2 = owned_event_id!("$ev2");

        let event_0 = timeline_event(&event_id_0);
        let event_1 = timeline_event(&event_id_1);
        let event_2 = timeline_event(&event_id_2);

        let deduplicator = BloomFilterDeduplicator::new();
        let existing_events = RoomEvents::new();

        let mut events =
            deduplicator.scan_and_learn([event_0, event_1, event_2].into_iter(), &existing_events);

        assert_let!(Some(Decoration::Unique(event)) = events.next());
        assert_eq!(event.event_id(), Some(event_id_0));

        assert_let!(Some(Decoration::Unique(event)) = events.next());
        assert_eq!(event.event_id(), Some(event_id_1));

        assert_let!(Some(Decoration::Unique(event)) = events.next());
        assert_eq!(event.event_id(), Some(event_id_2));

        assert!(events.next().is_none());
    }

    #[test]
    fn test_bloom_filter_growth() {
        // This test was used as a testbed to observe, using `valgrind --tool=massive`,
        // the total memory allocated by the deduplicator. We keep it checked in
        // to revive this experiment in the future, if needs be.

        let num_rooms = if let Ok(num_rooms) = std::env::var("ROOMS") {
            num_rooms.parse().unwrap()
        } else {
            10
        };

        let num_events = if let Ok(num_events) = std::env::var("EVENTS") {
            num_events.parse().unwrap()
        } else {
            100
        };

        let mut dedups = Vec::with_capacity(num_rooms);

        for _ in 0..num_rooms {
            let dedup = BloomFilterDeduplicator::new();
            let existing_events = RoomEvents::new();

            for i in 0..num_events {
                let event = timeline_event(&EventId::parse(format!("$event{i}")).unwrap());
                let mut it = dedup.scan_and_learn([event].into_iter(), &existing_events);

                assert_matches!(it.next(), Some(Decoration::Unique(..)));
                assert_matches!(it.next(), None);
            }

            dedups.push(dedup);
        }
    }

    #[cfg(not(target_arch = "wasm32"))] // This uses the cross-process lock, so needs time support.
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
        let ev4 = TimelineEvent::new(Raw::from_json_string("{}".to_owned()).unwrap());

        // Prefill the store with ev1 and ev2.
        event_cache_store
            .handle_linked_chunk_updates(
                room_id,
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

        let deduplicator = Deduplicator::new_store_based(room_id.to_owned(), event_cache_store);

        let room_events = RoomEvents::new();
        let DeduplicationOutcome {
            all_events: events,
            in_memory_duplicated_event_ids,
            in_store_duplicated_event_ids,
        } = deduplicator
            .filter_duplicate_events(vec![ev1, ev2, ev3, ev4], &room_events)
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
