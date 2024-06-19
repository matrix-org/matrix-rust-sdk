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

use std::ops::Not;

use bloomfilter::Bloom;
use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
use ruma::OwnedEventId;

use super::store::RoomEvents;

pub struct Deduplicator {
    bloom_filter: Bloom<OwnedEventId>,
}

impl Deduplicator {
    const APPROXIMATED_MAXIMUM_NUMBER_OF_EVENTS: usize = 800_000;
    const DESIRED_FALSE_POSITIVE_RATE: f64 = 0.001;
    const SEED_FOR_HASHER: &'static [u8; 32] = b"matrix_sdk_event_cache_deduptor!";

    pub fn new() -> Self {
        Self {
            bloom_filter: Bloom::new_for_fp_rate_with_seed(
                Self::APPROXIMATED_MAXIMUM_NUMBER_OF_EVENTS,
                Self::DESIRED_FALSE_POSITIVE_RATE,
                Self::SEED_FOR_HASHER,
            ),
        }
    }

    pub fn filter_and_learn<'a, I>(
        &'a mut self,
        events: I,
        room_events: &'a RoomEvents,
    ) -> impl Iterator<Item = I::Item> + 'a
    where
        I: Iterator<Item = SyncTimelineEvent> + 'a,
    {
        events.filter(|event| {
            let Some(event_id) = event.event_id() else {
                // The event has no `event_id`. Safe path: filter it out.
                return false;
            };

            if self.bloom_filter.check_and_set(&event_id) {
                // Bloom filter has false positives. We are NOT sure the event
                // is NOT present. Even if the false positive rate is low, we
                // need to iterate over all events to ensure it isn't present.

                room_events
                    .revents()
                    .any(|(_position, other_event)| {
                        other_event.event_id().as_ref() == Some(&event_id)
                    })
                    .not()
            } else {
                // Bloom filter has no false negatives. We are sure the event is NOT present: we
                // can keep it in the iterator.
                true
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_let;
    use matrix_sdk_test::{EventBuilder, ALICE};
    use ruma::{events::room::message::RoomMessageEventContent, owned_event_id, EventId};

    use super::*;

    fn sync_timeline_event(event_builder: &EventBuilder, event_id: &EventId) -> SyncTimelineEvent {
        SyncTimelineEvent::new(event_builder.make_sync_message_event_with_id(
            &*ALICE,
            &event_id,
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

        let mut deduplicator = Deduplicator::new();
        let room_events = RoomEvents::new();

        let mut events =
            deduplicator.filter_and_learn([event_0, event_1, event_2].into_iter(), &room_events);

        assert_let!(Some(event) = events.next());
        assert_eq!(event.event_id(), Some(event_id_0));

        assert_let!(Some(event) = events.next());
        assert_eq!(event.event_id(), Some(event_id_1));

        assert_let!(Some(event) = events.next());
        assert_eq!(event.event_id(), Some(event_id_2));

        assert!(events.next().is_none());
    }

    #[test]
    fn test_filter_duplicates() {
        let event_builder = EventBuilder::new();

        let event_id_0 = owned_event_id!("$ev0");
        let event_id_1 = owned_event_id!("$ev1");
        let event_id_2 = owned_event_id!("$ev2");

        let event_0 = sync_timeline_event(&event_builder, &event_id_0);
        let event_1 = sync_timeline_event(&event_builder, &event_id_1);
        let event_2 = sync_timeline_event(&event_builder, &event_id_2);

        let mut deduplicator = Deduplicator::new();
        let mut room_events = RoomEvents::new();

        // Simulate `event_1` is inserted inside `room_events`.
        {
            let mut events =
                deduplicator.filter_and_learn([event_1.clone()].into_iter(), &room_events);

            assert_let!(Some(event_1) = events.next());
            assert_eq!(event_1.event_id(), Some(event_id_1));

            assert!(events.next().is_none());

            drop(events); // make the borrow checker happy.

            // Now we can push `event_1` inside `room_events`.
            room_events.push_event(event_1);
        }

        // `event_1` will be duplicated.
        {
            let mut events = deduplicator
                .filter_and_learn([event_0, event_1, event_2].into_iter(), &room_events);

            assert_let!(Some(event) = events.next());
            assert_eq!(event.event_id(), Some(event_id_0));

            // `event_1` is missing.

            assert_let!(Some(event) = events.next());
            assert_eq!(event.event_id(), Some(event_id_2));

            assert!(events.next().is_none());
        }
    }
}
