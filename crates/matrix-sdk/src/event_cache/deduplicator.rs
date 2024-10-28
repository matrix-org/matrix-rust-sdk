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

use super::room::events::{Event, RoomEvents};

/// `Deduplicator` is an efficient type to find duplicated events.
///
/// It uses a [bloom filter] to provide a memory efficient probabilistic answer
/// to: “has event E been seen already?”. False positives are possible, while
/// false negatives are impossible. In the case of a positive reply, we fallback
/// to a linear (backward) search on all events to check whether it's a false
/// positive or not
///
/// [bloom filter]: https://en.wikipedia.org/wiki/Bloom_filter
pub struct Deduplicator {
    bloom_filter: Mutex<GrowableBloom>,
}

impl fmt::Debug for Deduplicator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Deduplicator").finish_non_exhaustive()
    }
}

impl Deduplicator {
    const APPROXIMATED_MAXIMUM_NUMBER_OF_EVENTS: usize = 800_000;
    const DESIRED_FALSE_POSITIVE_RATE: f64 = 0.001;

    /// Create a new `Deduplicator`.
    pub fn new() -> Self {
        Self {
            bloom_filter: Mutex::new(
                GrowableBloomBuilder::new()
                    .estimated_insertions(Self::APPROXIMATED_MAXIMUM_NUMBER_OF_EVENTS)
                    .desired_error_ratio(Self::DESIRED_FALSE_POSITIVE_RATE)
                    .build(),
            ),
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
    pub fn scan_and_learn<'a, I>(
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
pub enum Decoration<I> {
    /// This event is not duplicated.
    Unique(I),

    /// This event is duplicated.
    Duplicated(I),

    /// This event is invalid (i.e. not well formed).
    Invalid(I),
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_let;
    use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
    use matrix_sdk_test::{EventBuilder, ALICE};
    use ruma::{events::room::message::RoomMessageEventContent, owned_event_id, EventId};

    use super::*;

    fn sync_timeline_event(event_builder: &EventBuilder, event_id: &EventId) -> SyncTimelineEvent {
        SyncTimelineEvent::new(event_builder.make_sync_message_event_with_id(
            *ALICE,
            event_id,
            RoomMessageEventContent::text_plain("foo"),
        ))
    }

    #[test]
    fn test_filter_no_duplicate() {
        let event_builder = EventBuilder::new();

        let event_id_0 = owned_event_id!("$ev0");
        let event_id_1 = owned_event_id!("$ev1");
        let event_id_2 = owned_event_id!("$ev2");

        let event_0 = sync_timeline_event(&event_builder, &event_id_0);
        let event_1 = sync_timeline_event(&event_builder, &event_id_1);
        let event_2 = sync_timeline_event(&event_builder, &event_id_2);

        let deduplicator = Deduplicator::new();
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
        let event_builder = EventBuilder::new();

        let event_id_0 = owned_event_id!("$ev0");
        let event_id_1 = owned_event_id!("$ev1");

        let event_0 = sync_timeline_event(&event_builder, &event_id_0);
        let event_1 = sync_timeline_event(&event_builder, &event_id_1);

        let deduplicator = Deduplicator::new();
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
        let event_builder = EventBuilder::new();

        let event_id_0 = owned_event_id!("$ev0");
        let event_id_1 = owned_event_id!("$ev1");
        let event_id_2 = owned_event_id!("$ev2");

        let event_0 = sync_timeline_event(&event_builder, &event_id_0);
        let event_1 = sync_timeline_event(&event_builder, &event_id_1);
        let event_2 = sync_timeline_event(&event_builder, &event_id_2);

        let deduplicator = Deduplicator::new();
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
}
