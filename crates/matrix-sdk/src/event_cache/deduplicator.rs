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
use matrix_sdk_base::event_cache::store::EventCacheStoreLock;
use ruma::{OwnedEventId, OwnedRoomId};
use tracing::{debug, warn};

use super::{
    room::events::{Event, RoomEvents},
    EventCacheError,
};

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
    /// When the persistent storage is enabled by default, this constructor
    /// (and the associated variant) will be removed.
    pub fn new_memory_based() -> Self {
        Self::InMemory(BloomFilterDeduplicator::new())
    }

    /// Create new store-based deduplicator that will run queries against the
    /// store to find if any event is deduplicated or not.
    ///
    /// This deduplicator is stateless.
    ///
    /// When the persistent storage is enabled by default, this will become the
    /// default, and [`Deduplicator`] will be replaced with
    /// [`StoreDeduplicator`].
    pub fn new_store_based(room_id: OwnedRoomId, store: EventCacheStoreLock) -> Self {
        Self::PersistentStore(StoreDeduplicator { room_id, store })
    }

    /// Find duplicates in the given collection of events, and return both
    /// valid events (those with an event id) as well as the event ids of
    /// duplicate events.
    pub async fn filter_duplicate_events<I>(
        &self,
        events: I,
        room_events: &RoomEvents,
    ) -> Result<(Vec<Event>, Vec<OwnedEventId>), EventCacheError>
    where
        I: Iterator<Item = Event>,
    {
        match self {
            Deduplicator::InMemory(dedup) => Ok(dedup.filter_duplicate_events(events, room_events)),
            Deduplicator::PersistentStore(dedup) => dedup.filter_duplicate_events(events).await,
        }
    }
}

/// A deduplication mechanism based on the persistent storage associated to the
/// event cache.
///
/// It will use queries to the persistent storage to figure where events are
/// duplicates or not, making it entirely stateless.
pub struct StoreDeduplicator {
    /// The room this deduplicator applies to.
    room_id: OwnedRoomId,
    /// The actual event cache store implementation used to query events.
    store: EventCacheStoreLock,
}

impl StoreDeduplicator {
    async fn filter_duplicate_events<I>(
        &self,
        events: I,
    ) -> Result<(Vec<Event>, Vec<OwnedEventId>), EventCacheError>
    where
        I: Iterator<Item = Event>,
    {
        let store = self.store.lock().await?;

        // Collect event ids as we "validate" events (i.e. check they have a valid event
        // id.)
        let mut event_ids = Vec::new();
        let events = events
            .filter_map(|event| {
                if let Some(event_id) = event.event_id() {
                    event_ids.push(event_id);
                    Some(event)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        // Let the store do its magic ✨
        let duplicates = store.filter_duplicated_events(&self.room_id, event_ids).await?;

        Ok((events, duplicates))
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
    /// duplicate events.
    fn filter_duplicate_events<'a, I>(
        &'a self,
        events: I,
        room_events: &'a RoomEvents,
    ) -> (Vec<Event>, Vec<OwnedEventId>)
    where
        I: Iterator<Item = Event> + 'a,
    {
        let mut duplicated_event_ids = Vec::new();

        let events = self
            .scan_and_learn(events, room_events)
            .filter_map(|decorated_event| match decorated_event {
                Decoration::Unique(event) => Some(event),
                Decoration::Duplicated(event) => {
                    debug!(event_id = ?event.event_id(), "Found a duplicated event");

                    duplicated_event_ids.push(
                        event
                            .event_id()
                            // SAFETY: An event with no ID is decorated as
                            // `Decoration::Invalid`. Thus, it's
                            // safe to unwrap the `Option<OwnedEventId>` here.
                            .expect("The event has no ID"),
                    );

                    // Keep the new event!
                    Some(event)
                }
                Decoration::Invalid(event) => {
                    warn!(?event, "Found an event with no ID");
                    None
                }
            })
            .collect::<Vec<_>>();

        (events, duplicated_event_ids)
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
        // `new_scanned_events` is not a field of `Self` because it is used only detect
        // duplicates in `new_events_to_scan`.
        let mut new_scanned_events = BTreeSet::new();

        new_events_to_scan.map(move |event| {
            let Some(event_id) = event.event_id() else {
                // The event has no `event_id`.
                return Decoration::Invalid(event);
            };

            if self.bloom_filter.lock().unwrap().check_and_set(&event_id) {
                // Oh oh, it looks like we have found a duplicate!
                //
                // However, bloom filters have false positives. We are NOT sure the event is NOT
                // present. Even if the false positive rate is low, we need to
                // iterate over all events to ensure it isn't present.

                // First, let's ensure `event` is not a duplicate from `new_events_to_scan`,
                // i.e. if the iterator itself contains duplicated events! We use a `BTreeSet`,
                // otherwise using a bloom filter again may generate false positives.
                if new_scanned_events.contains(&event_id) {
                    // The iterator contains a duplicated `event`.
                    return Decoration::Duplicated(event);
                }

                // Second, we can iterate over all events to ensure `event` is not present in
                // `existing_events`.
                let duplicated = existing_events.revents().any(|(_position, other_event)| {
                    other_event.event_id().as_ref() == Some(&event_id)
                });

                new_scanned_events.insert(event_id);

                if duplicated {
                    Decoration::Duplicated(event)
                } else {
                    Decoration::Unique(event)
                }
            } else {
                new_scanned_events.insert(event_id);

                // Bloom filter has no false negatives. We are sure the event is NOT present: we
                // can keep it in the iterator.
                Decoration::Unique(event)
            }
        })
    }
}

/// Information about the scanned collection of events.
#[derive(Debug)]
enum Decoration<I> {
    /// This event is not duplicated.
    Unique(I),

    /// This event is duplicated.
    Duplicated(I),

    /// This event is invalid (i.e. not well formed).
    Invalid(I),
}

#[cfg(test)]
mod tests {
    use assert_matches2::{assert_let, assert_matches};
    use matrix_sdk_base::deserialized_responses::TimelineEvent;
    use matrix_sdk_test::event_factory::EventFactory;
    use ruma::{owned_event_id, user_id, EventId};

    use super::*;

    fn sync_timeline_event(event_id: &EventId) -> TimelineEvent {
        EventFactory::new()
            .text_msg("")
            .sender(user_id!("@mnt_io:matrix.org"))
            .event_id(event_id)
            .into_event()
    }

    #[test]
    fn test_filter_no_duplicate() {
        let event_id_0 = owned_event_id!("$ev0");
        let event_id_1 = owned_event_id!("$ev1");
        let event_id_2 = owned_event_id!("$ev2");

        let event_0 = sync_timeline_event(&event_id_0);
        let event_1 = sync_timeline_event(&event_id_1);
        let event_2 = sync_timeline_event(&event_id_2);

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
    fn test_filter_duplicates_in_new_events() {
        let event_id_0 = owned_event_id!("$ev0");
        let event_id_1 = owned_event_id!("$ev1");

        let event_0 = sync_timeline_event(&event_id_0);
        let event_1 = sync_timeline_event(&event_id_1);

        let deduplicator = BloomFilterDeduplicator::new();
        let existing_events = RoomEvents::new();

        let mut events = deduplicator.scan_and_learn(
            [
                event_0.clone(), // OK
                event_0,         // Not OK
                event_1,         // OK
            ]
            .into_iter(),
            &existing_events,
        );

        assert_let!(Some(Decoration::Unique(event)) = events.next());
        assert_eq!(event.event_id(), Some(event_id_0.clone()));

        assert_let!(Some(Decoration::Duplicated(event)) = events.next());
        assert_eq!(event.event_id(), Some(event_id_0));

        assert_let!(Some(Decoration::Unique(event)) = events.next());
        assert_eq!(event.event_id(), Some(event_id_1));

        assert!(events.next().is_none());
    }

    #[test]
    fn test_filter_duplicates_with_existing_events() {
        let event_id_0 = owned_event_id!("$ev0");
        let event_id_1 = owned_event_id!("$ev1");
        let event_id_2 = owned_event_id!("$ev2");

        let event_0 = sync_timeline_event(&event_id_0);
        let event_1 = sync_timeline_event(&event_id_1);
        let event_2 = sync_timeline_event(&event_id_2);

        let deduplicator = BloomFilterDeduplicator::new();
        let mut existing_events = RoomEvents::new();

        // Simulate `event_1` is inserted inside `existing_events`.
        {
            let mut events =
                deduplicator.scan_and_learn([event_1.clone()].into_iter(), &existing_events);

            assert_let!(Some(Decoration::Unique(event_1)) = events.next());
            assert_eq!(event_1.event_id(), Some(event_id_1.clone()));

            assert!(events.next().is_none());

            drop(events); // make the borrow checker happy.

            // Now we can push `event_1` inside `existing_events`.
            existing_events.push_events([event_1]);
        }

        // `event_1` will be duplicated.
        {
            let mut events = deduplicator.scan_and_learn(
                [
                    event_0, // OK
                    event_1, // Not OK
                    event_2, // Ok
                ]
                .into_iter(),
                &existing_events,
            );

            assert_let!(Some(Decoration::Unique(event)) = events.next());
            assert_eq!(event.event_id(), Some(event_id_0));

            assert_let!(Some(Decoration::Duplicated(event)) = events.next());
            assert_eq!(event.event_id(), Some(event_id_1));

            assert_let!(Some(Decoration::Unique(event)) = events.next());
            assert_eq!(event.event_id(), Some(event_id_2));

            assert!(events.next().is_none());
        }
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
                let event = sync_timeline_event(&EventId::parse(format!("$event{i}")).unwrap());
                let mut it = dedup.scan_and_learn([event].into_iter(), &existing_events);

                assert_matches!(it.next(), Some(Decoration::Unique(..)));
                assert_matches!(it.next(), None);
            }

            dedups.push(dedup);
        }
    }
}
