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

use std::{cmp::Ordering, fmt};

use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
use ruma::OwnedEventId;
use tracing::error;

use super::{
    deduplicator::{Decoration, Deduplicator},
    linked_chunk::{Chunk, ChunkIdentifier, Error, Iter, LinkedChunk, Position},
};

/// An alias for the real event type.
pub type Event = SyncTimelineEvent;

#[derive(Clone, Debug)]
pub struct Gap {
    /// The token to use in the query, extracted from a previous "from" /
    /// "end" field of a `/messages` response.
    pub prev_token: String,
}

const DEFAULT_CHUNK_CAPACITY: usize = 128;

/// This type represents all events of a single room.
pub struct RoomEvents {
    /// The real in-memory storage for all the events.
    chunks: LinkedChunk<DEFAULT_CHUNK_CAPACITY, Event, Gap>,

    /// The events deduplicator instance to help finding duplicates.
    deduplicator: Deduplicator,
}

impl Default for RoomEvents {
    fn default() -> Self {
        Self::new()
    }
}

impl RoomEvents {
    /// Build a new `Self` with zero events.
    pub fn new() -> Self {
        Self { chunks: LinkedChunk::new(), deduplicator: Deduplicator::new() }
    }

    /// Clear all events.
    pub fn reset(&mut self) {
        self.chunks = LinkedChunk::new();
    }

    /// Deduplicate `events` considering all events in `Self::chunks`.
    ///
    /// For the moment, duplicated events will be logged but not removed from
    /// the resulting iterator.
    fn deduplicate<'a, I>(&'a mut self, events: I) -> (Vec<Event>, Vec<OwnedEventId>)
    where
        I: Iterator<Item = Event> + 'a,
    {
        let mut duplicated_event_ids = Vec::new();

        let deduplicated_events = self
            .deduplicator
            .scan_and_learn(events, self)
            .filter_map(|decorated_event| match decorated_event {
                Decoration::Ok(event) => Some(event),
                Decoration::Duplicated(event) => {
                    error!(event_id = ?event.event_id(), "Found a duplicated event");

                    duplicated_event_ids.push(event.event_id().expect("The event has no ID"));

                    // Keep the new event!
                    Some(event)
                }
                Decoration::Invalid(event) => {
                    error!(?event, "Found an invalid event");

                    None
                }
            })
            .collect();

        (deduplicated_events, duplicated_event_ids)
    }

    /// Push events after all events or gaps.
    ///
    /// The last event in `events` is the most recent one.
    pub fn push_events<I>(&mut self, events: I)
    where
        I: IntoIterator<Item = Event>,
    {
        let (events, duplicated_event_ids) = self.deduplicate(events.into_iter());

        // Remove the _old_ duplicated events!
        {
            // We don't have to worry the removals can change the position of the existing
            // events, because we are pushing all _new_ `events` at the back.
            for duplicated_event_id in duplicated_event_ids {
                let Some(duplicated_event_position) =
                    self.revents().find_map(|(position, event)| {
                        (event.event_id().as_ref() == Some(&duplicated_event_id))
                            .then_some(position)
                    })
                else {
                    error!(
                        ?duplicated_event_id,
                        "A duplicated event has been detected, but it's position seems unknown"
                    );

                    continue;
                };

                self.chunks
                    .remove_item_at(duplicated_event_position)
                    .expect("Failed to remove an event we have just found");
            }
        }

        // Push new `events`.
        self.chunks.push_items_back(events);
    }

    /// Push a gap after all events or gaps.
    pub fn push_gap(&mut self, gap: Gap) {
        self.chunks.push_gap_back(gap)
    }

    /// Insert events at a specified position.
    pub fn insert_events_at<I>(&mut self, events: I, mut position: Position) -> Result<(), Error>
    where
        I: IntoIterator<Item = Event>,
    {
        let (events, duplicated_event_ids) = self.deduplicate(events.into_iter());

        // Remove the _old_ duplicated events!
        {
            // We **have to worry* the removals can change the position of the existing
            // events. We **have** to update the `position` argument value for each removal.
            for duplicated_event_id in duplicated_event_ids {
                let Some(duplicated_event_position) =
                    self.revents().find_map(|(position, event)| {
                        (event.event_id().as_ref() == Some(&duplicated_event_id))
                            .then_some(position)
                    })
                else {
                    error!(
                        ?duplicated_event_id,
                        "A duplicated event has been detected, but it's position seems unknown"
                    );

                    continue;
                };

                self.chunks
                    .remove_item_at(duplicated_event_position)
                    .expect("Failed to remove an event we have just found");

                // A `Position` is composed of a `ChunkIdentifier` and an index.
                // The `ChunkIdentifier` is stable, i.e. it won't change if an
                // event is removed in another chunk. It means we only need to
                // update `position` if the removal happened in **the same
                // chunk**.
                if duplicated_event_position.chunk_identifier() == position.chunk_identifier() {
                    // Now we can compare the the position indices.
                    match duplicated_event_position.index().cmp(&position.index()) {
                        // `duplicated_event_position`'s index < `position`'s index
                        Ordering::Less => {
                            // An event has been removed _before_ the new
                            // events: `position` needs to be shifted to the
                            // left by 1.
                            position.move_index_to_the_left();
                        }

                        // `duplicated_event_position`'s index == `position`'s index
                        Ordering::Equal => {
                            // An event has been removed at the same position of
                            // the new events: `position` does _NOT_ need tp be
                            // modified.
                        }

                        // `duplicated_event_position`'s index > `position`'s index
                        Ordering::Greater => {
                            // An event has been removed _after_ the new events:
                            // `position` does _NOT_ need to be modified.
                        }
                    }
                }
            }
        }

        self.chunks.insert_items_at(events, position)
    }

    /// Insert a gap at a specified position.
    pub fn insert_gap_at(&mut self, gap: Gap, position: Position) -> Result<(), Error> {
        self.chunks.insert_gap_at(gap, position)
    }

    /// Replace the gap identified by `gap_identifier`, by events.
    ///
    /// Because the `gap_identifier` can represent non-gap chunk, this method
    /// returns a `Result`.
    ///
    /// This method returns a reference to the (first if many) newly created
    /// `Chunk` that contains the `items`.
    pub fn replace_gap_at<I>(
        &mut self,
        events: I,
        gap_identifier: ChunkIdentifier,
    ) -> Result<&Chunk<DEFAULT_CHUNK_CAPACITY, Event, Gap>, Error>
    where
        I: IntoIterator<Item = Event>,
    {
        let (events, duplicated_event_ids) = self.deduplicate(events.into_iter());

        // Remove the _old_ duplicated events!
        {
            // We don't have to worry the removals can change the position of the existing
            // events, because we are replacing a gap: its identifier will not change
            // because of the removals.
            for duplicated_event_id in duplicated_event_ids {
                let Some(duplicated_event_position) =
                    self.revents().find_map(|(position, event)| {
                        (event.event_id().as_ref() == Some(&duplicated_event_id))
                            .then_some(position)
                    })
                else {
                    error!(
                        ?duplicated_event_id,
                        "A duplicated event has been detected, but it's position seems unknown"
                    );

                    continue;
                };

                self.chunks
                    .remove_item_at(duplicated_event_position)
                    .expect("Failed to remove an event we have just found");
            }
        }

        self.chunks.replace_gap_at(events, gap_identifier)
    }

    /// Search for a chunk, and return its identifier.
    pub fn chunk_identifier<'a, P>(&'a self, predicate: P) -> Option<ChunkIdentifier>
    where
        P: FnMut(&'a Chunk<DEFAULT_CHUNK_CAPACITY, Event, Gap>) -> bool,
    {
        self.chunks.chunk_identifier(predicate)
    }

    /// Iterate over the chunks, forward.
    ///
    /// The oldest chunk comes first.
    pub fn chunks(&self) -> Iter<'_, DEFAULT_CHUNK_CAPACITY, Event, Gap> {
        self.chunks.chunks()
    }

    /// Iterate over the events, backward.
    ///
    /// The most recent event comes first.
    pub fn revents(&self) -> impl Iterator<Item = (Position, &Event)> {
        self.chunks.ritems()
    }

    /// Iterate over the events, forward.
    ///
    /// The oldest event comes first.
    pub fn events(&self) -> impl Iterator<Item = (Position, &Event)> {
        self.chunks.items()
    }
}

impl fmt::Debug for RoomEvents {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        formatter.debug_struct("RoomEvents").field("chunk", &self.chunks).finish()
    }
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_let;
    use matrix_sdk_test::{EventBuilder, ALICE};
    use ruma::{events::room::message::RoomMessageEventContent, EventId, OwnedEventId};

    use super::*;

    fn new_event(event_builder: &EventBuilder, event_id: &str) -> (OwnedEventId, Event) {
        let event_id = EventId::parse(event_id).unwrap();

        let event = SyncTimelineEvent::new(event_builder.make_sync_message_event_with_id(
            *ALICE,
            &event_id,
            RoomMessageEventContent::text_plain("foo"),
        ));

        (event_id, event)
    }

    #[test]
    fn test_new_room_events_has_zero_events() {
        let room_events = RoomEvents::new();

        assert_eq!(room_events.chunks.len(), 0);
    }

    #[test]
    fn test_push_events() {
        let event_builder = EventBuilder::new();

        let (event_id_0, event_0) = new_event(&event_builder, "$ev0");
        let (event_id_1, event_1) = new_event(&event_builder, "$ev1");
        let (event_id_2, event_2) = new_event(&event_builder, "$ev2");

        let mut room_events = RoomEvents::new();

        room_events.push_events([event_0, event_1]);
        room_events.push_events([event_2]);

        {
            let mut events = room_events.events();

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 0);
            assert_eq!(event.event_id().unwrap(), event_id_0);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 1);
            assert_eq!(event.event_id().unwrap(), event_id_1);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 2);
            assert_eq!(event.event_id().unwrap(), event_id_2);

            assert!(events.next().is_none());
        }
    }

    #[test]
    fn test_push_events_with_duplicates() {
        let event_builder = EventBuilder::new();

        let (event_id_0, event_0) = new_event(&event_builder, "$ev0");
        let (event_id_1, event_1) = new_event(&event_builder, "$ev1");

        let mut room_events = RoomEvents::new();

        room_events.push_events([event_0.clone(), event_1]);

        {
            let mut events = room_events.events();

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 0);
            assert_eq!(event.event_id().unwrap(), event_id_0);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 1);
            assert_eq!(event.event_id().unwrap(), event_id_1);

            assert!(events.next().is_none());
        }

        // Everything is alright. Now let's push a duplicated event.
        room_events.push_events([event_0]);

        {
            let mut events = room_events.events();

            // The first `event_id_0` has been removed.
            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 0);
            assert_eq!(event.event_id().unwrap(), event_id_1);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 1);
            assert_eq!(event.event_id().unwrap(), event_id_0);

            assert!(events.next().is_none());
        }
    }

    #[test]
    fn test_push_gap() {
        let event_builder = EventBuilder::new();

        let (event_id_0, event_0) = new_event(&event_builder, "$ev0");
        let (event_id_1, event_1) = new_event(&event_builder, "$ev1");

        let mut room_events = RoomEvents::new();

        room_events.push_events([event_0]);
        room_events.push_gap(Gap { prev_token: "hello".to_owned() });
        room_events.push_events([event_1]);

        {
            let mut events = room_events.events();

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 0);
            assert_eq!(event.event_id().unwrap(), event_id_0);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 2);
            assert_eq!(position.index(), 0);
            assert_eq!(event.event_id().unwrap(), event_id_1);

            assert!(events.next().is_none());
        }

        {
            let mut chunks = room_events.chunks();

            assert_let!(Some(chunk) = chunks.next());
            assert!(chunk.is_items());

            assert_let!(Some(chunk) = chunks.next());
            assert!(chunk.is_gap());

            assert_let!(Some(chunk) = chunks.next());
            assert!(chunk.is_items());

            assert!(chunks.next().is_none());
        }
    }

    #[test]
    fn test_insert_events_at() {
        let event_builder = EventBuilder::new();

        let (event_id_0, event_0) = new_event(&event_builder, "$ev0");
        let (event_id_1, event_1) = new_event(&event_builder, "$ev1");
        let (event_id_2, event_2) = new_event(&event_builder, "$ev2");

        let mut room_events = RoomEvents::new();

        room_events.push_events([event_0, event_1]);

        let position_of_event_1 = room_events
            .events()
            .find_map(|(position, event)| {
                (event.event_id().unwrap() == event_id_1).then_some(position)
            })
            .unwrap();

        room_events.insert_events_at([event_2], position_of_event_1).unwrap();

        {
            let mut events = room_events.events();

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 0);
            assert_eq!(event.event_id().unwrap(), event_id_0);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 1);
            assert_eq!(event.event_id().unwrap(), event_id_2);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 2);
            assert_eq!(event.event_id().unwrap(), event_id_1);

            assert!(events.next().is_none());
        }
    }

    #[test]
    fn test_insert_events_at_with_duplicates() {
        let event_builder = EventBuilder::new();

        let (event_id_0, event_0) = new_event(&event_builder, "$ev0");
        let (event_id_1, event_1) = new_event(&event_builder, "$ev1");
        let (event_id_2, event_2) = new_event(&event_builder, "$ev2");
        let (event_id_3, event_3) = new_event(&event_builder, "$ev3");

        let mut room_events = RoomEvents::new();

        room_events.push_events([event_0.clone(), event_1, event_2]);

        let position_of_event_2 = room_events
            .events()
            .find_map(|(position, event)| {
                (event.event_id().unwrap() == event_id_2).then_some(position)
            })
            .unwrap();

        {
            let mut events = room_events.events();

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 0);
            assert_eq!(event.event_id().unwrap(), event_id_0);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 1);
            assert_eq!(event.event_id().unwrap(), event_id_1);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 2);
            assert_eq!(event.event_id().unwrap(), event_id_2);

            assert!(events.next().is_none());
        }

        // Everything is alright. Now let's insert a duplicated events!
        room_events.insert_events_at([event_0, event_3], position_of_event_2).unwrap();

        {
            let mut events = room_events.events();

            // The first `event_id_0` has been removed.
            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 0);
            assert_eq!(event.event_id().unwrap(), event_id_1);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 1);
            assert_eq!(event.event_id().unwrap(), event_id_0);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 2);
            assert_eq!(event.event_id().unwrap(), event_id_3);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 3);
            assert_eq!(event.event_id().unwrap(), event_id_2);

            assert!(events.next().is_none());
        }
    }
    #[test]
    fn test_insert_gap_at() {
        let event_builder = EventBuilder::new();

        let (event_id_0, event_0) = new_event(&event_builder, "$ev0");
        let (event_id_1, event_1) = new_event(&event_builder, "$ev1");

        let mut room_events = RoomEvents::new();

        room_events.push_events([event_0, event_1]);

        let position_of_event_1 = room_events
            .events()
            .find_map(|(position, event)| {
                (event.event_id().unwrap() == event_id_1).then_some(position)
            })
            .unwrap();

        room_events
            .insert_gap_at(Gap { prev_token: "hello".to_owned() }, position_of_event_1)
            .unwrap();

        {
            let mut events = room_events.events();

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 0);
            assert_eq!(event.event_id().unwrap(), event_id_0);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 2);
            assert_eq!(position.index(), 0);
            assert_eq!(event.event_id().unwrap(), event_id_1);

            assert!(events.next().is_none());
        }

        {
            let mut chunks = room_events.chunks();

            assert_let!(Some(chunk) = chunks.next());
            assert!(chunk.is_items());

            assert_let!(Some(chunk) = chunks.next());
            assert!(chunk.is_gap());

            assert_let!(Some(chunk) = chunks.next());
            assert!(chunk.is_items());

            assert!(chunks.next().is_none());
        }
    }

    #[test]
    fn test_replace_gap_at() {
        let event_builder = EventBuilder::new();

        let (event_id_0, event_0) = new_event(&event_builder, "$ev0");
        let (event_id_1, event_1) = new_event(&event_builder, "$ev1");
        let (event_id_2, event_2) = new_event(&event_builder, "$ev2");

        let mut room_events = RoomEvents::new();

        room_events.push_events([event_0]);
        room_events.push_gap(Gap { prev_token: "hello".to_owned() });

        let chunk_identifier_of_gap = room_events
            .chunks()
            .find_map(|chunk| chunk.is_gap().then_some(chunk.first_position()))
            .unwrap()
            .chunk_identifier();

        room_events.replace_gap_at([event_1, event_2], chunk_identifier_of_gap).unwrap();

        {
            let mut events = room_events.events();

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 0);
            assert_eq!(event.event_id().unwrap(), event_id_0);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 2);
            assert_eq!(position.index(), 0);
            assert_eq!(event.event_id().unwrap(), event_id_1);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 2);
            assert_eq!(position.index(), 1);
            assert_eq!(event.event_id().unwrap(), event_id_2);

            assert!(events.next().is_none());
        }

        {
            let mut chunks = room_events.chunks();

            assert_let!(Some(chunk) = chunks.next());
            assert!(chunk.is_items());

            assert_let!(Some(chunk) = chunks.next());
            assert!(chunk.is_items());

            assert!(chunks.next().is_none());
        }
    }

    #[test]
    fn test_replace_gap_at_with_duplicates() {
        let event_builder = EventBuilder::new();

        let (event_id_0, event_0) = new_event(&event_builder, "$ev0");
        let (event_id_1, event_1) = new_event(&event_builder, "$ev1");
        let (event_id_2, event_2) = new_event(&event_builder, "$ev2");

        let mut room_events = RoomEvents::new();

        room_events.push_events([event_0.clone(), event_1]);
        room_events.push_gap(Gap { prev_token: "hello".to_owned() });

        let chunk_identifier_of_gap = room_events
            .chunks()
            .find_map(|chunk| chunk.is_gap().then_some(chunk.first_position()))
            .unwrap()
            .chunk_identifier();

        {
            let mut events = room_events.events();

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 0);
            assert_eq!(event.event_id().unwrap(), event_id_0);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 1);
            assert_eq!(event.event_id().unwrap(), event_id_1);

            assert!(events.next().is_none());
        }

        // Everything is alright. Now let's replace a gap with a duplicated event.
        room_events.replace_gap_at([event_0, event_2], chunk_identifier_of_gap).unwrap();

        {
            let mut events = room_events.events();

            // The first `event_id_0` has been removed.
            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 0);
            assert_eq!(position.index(), 0);
            assert_eq!(event.event_id().unwrap(), event_id_1);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 2);
            assert_eq!(position.index(), 0);
            assert_eq!(event.event_id().unwrap(), event_id_0);

            assert_let!(Some((position, event)) = events.next());
            assert_eq!(position.chunk_identifier(), 2);
            assert_eq!(position.index(), 1);
            assert_eq!(event.event_id().unwrap(), event_id_2);

            assert!(events.next().is_none());
        }

        {
            let mut chunks = room_events.chunks();

            assert_let!(Some(chunk) = chunks.next());
            assert!(chunk.is_items());

            assert_let!(Some(chunk) = chunks.next());
            assert!(chunk.is_items());

            assert!(chunks.next().is_none());
        }
    }
}
