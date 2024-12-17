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

use std::cmp::Ordering;

use eyeball_im::VectorDiff;
pub use matrix_sdk_base::event_cache::{Event, Gap};
use matrix_sdk_base::{
    event_cache::store::DEFAULT_CHUNK_CAPACITY,
    linked_chunk::{AsVector, IterBackward, ObservableUpdates},
};
use matrix_sdk_common::linked_chunk::{
    Chunk, ChunkIdentifier, EmptyChunk, Error, LinkedChunk, Position,
};
use ruma::OwnedEventId;
use tracing::{debug, error, warn};

use super::{
    super::deduplicator::{Decoration, Deduplicator},
    chunk_debug_string,
};

/// This type represents all events of a single room.
#[derive(Debug)]
pub struct RoomEvents {
    /// The real in-memory storage for all the events.
    chunks: LinkedChunk<DEFAULT_CHUNK_CAPACITY, Event, Gap>,

    /// Type mapping [`Update`]s from [`Self::chunks`] to [`VectorDiff`]s.
    ///
    /// [`Update`]: matrix_sdk_base::linked_chunk::Update
    chunks_updates_as_vectordiffs: AsVector<Event, Gap>,

    /// The events deduplicator instance to help finding duplicates.
    deduplicator: Deduplicator,
}

impl Default for RoomEvents {
    fn default() -> Self {
        Self::new()
    }
}

impl RoomEvents {
    /// Build a new [`RoomEvents`] struct with zero events.
    pub fn new() -> Self {
        Self::with_initial_chunks(None)
    }

    /// Build a new [`RoomEvents`] struct with prior chunks knowledge.
    ///
    /// The provided [`LinkedChunk`] must have been built with update history.
    pub fn with_initial_chunks(
        chunks: Option<LinkedChunk<DEFAULT_CHUNK_CAPACITY, Event, Gap>>,
    ) -> Self {
        let mut chunks = chunks.unwrap_or_else(LinkedChunk::new_with_update_history);

        let chunks_updates_as_vectordiffs = chunks
            .as_vector()
            // SAFETY: The `LinkedChunk` has been built with `new_with_update_history`, so
            // `as_vector` must return `Some(…)`.
            .expect("`LinkedChunk` must have been built with `new_with_update_history`");

        // Let the deduplicator know about initial events.
        let deduplicator =
            Deduplicator::with_initial_events(chunks.items().map(|(_pos, event)| event));

        Self { chunks, chunks_updates_as_vectordiffs, deduplicator }
    }

    /// Returns whether the room has at least one event.
    pub fn is_empty(&self) -> bool {
        self.chunks.num_items() == 0
    }

    /// Clear all events.
    ///
    /// All events, all gaps, everything is dropped, move into the void, into
    /// the ether, forever.
    pub fn reset(&mut self) {
        self.chunks.clear();
    }

    /// Push events after all events or gaps.
    ///
    /// The last event in `events` is the most recent one.
    pub fn push_events<I>(&mut self, events: I) -> AddEventReport
    where
        I: IntoIterator<Item = Event>,
    {
        let (unique_events, duplicated_event_ids) =
            self.filter_duplicated_events(events.into_iter());

        let report = AddEventReport {
            num_new_unique: unique_events.len(),
            num_duplicated: duplicated_event_ids.len(),
        };

        // Remove the _old_ duplicated events!
        //
        // We don't have to worry the removals can change the position of the existing
        // events, because we are pushing all _new_ `events` at the back.
        self.remove_events(duplicated_event_ids);

        // Push new `events`.
        self.chunks.push_items_back(unique_events);

        report
    }

    /// Push a gap after all events or gaps.
    pub fn push_gap(&mut self, gap: Gap) {
        self.chunks.push_gap_back(gap);
    }

    /// Insert events at a specified position.
    pub fn insert_events_at<I>(
        &mut self,
        events: I,
        mut position: Position,
    ) -> Result<AddEventReport, Error>
    where
        I: IntoIterator<Item = Event>,
    {
        let (unique_events, duplicated_event_ids) =
            self.filter_duplicated_events(events.into_iter());

        let report = AddEventReport {
            num_new_unique: unique_events.len(),
            num_duplicated: duplicated_event_ids.len(),
        };

        // Remove the _old_ duplicated events!
        //
        // We **have to worry* the removals can change the position of the
        // existing events. We **have** to update the `position`
        // argument value for each removal.
        self.remove_events_and_update_insert_position(duplicated_event_ids, &mut position);

        self.chunks.insert_items_at(unique_events, position)?;

        Ok(report)
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
    ) -> Result<(AddEventReport, Option<Position>), Error>
    where
        I: IntoIterator<Item = Event>,
    {
        let (unique_events, duplicated_event_ids) =
            self.filter_duplicated_events(events.into_iter());

        let report = AddEventReport {
            num_new_unique: unique_events.len(),
            num_duplicated: duplicated_event_ids.len(),
        };

        // Remove the _old_ duplicated events!
        //
        // We don't have to worry the removals can change the position of the existing
        // events, because we are replacing a gap: its identifier will not change
        // because of the removals.
        self.remove_events(duplicated_event_ids);

        let next_pos = if unique_events.is_empty() {
            // There are no new events, so there's no need to create a new empty items
            // chunk; instead, remove the gap.
            self.chunks.remove_gap_at(gap_identifier)?
        } else {
            // Replace the gap by new events.
            Some(self.chunks.replace_gap_at(unique_events, gap_identifier)?.first_position())
        };
        Ok((report, next_pos))
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
    pub fn chunks(
        &self,
    ) -> matrix_sdk_common::linked_chunk::Iter<'_, DEFAULT_CHUNK_CAPACITY, Event, Gap> {
        self.chunks.chunks()
    }

    /// Iterate over the chunks, backward.
    ///
    /// The most recent chunk comes first.
    pub fn rchunks(&self) -> IterBackward<'_, DEFAULT_CHUNK_CAPACITY, Event, Gap> {
        self.chunks.rchunks()
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

    /// Get all updates from the room events as [`VectorDiff`].
    ///
    /// Be careful that each `VectorDiff` is returned only once!
    ///
    /// See [`AsVector`] to learn more.
    ///
    /// [`Update`]: matrix_sdk_base::linked_chunk::Update
    #[allow(unused)] // gonna be useful very soon! but we need it now for test purposes
    pub fn updates_as_vector_diffs(&mut self) -> Vec<VectorDiff<Event>> {
        self.chunks_updates_as_vectordiffs.take()
    }

    /// Get a mutable reference to the [`LinkedChunk`] updates, aka
    /// [`ObservableUpdates`].
    pub(super) fn updates(&mut self) -> &mut ObservableUpdates<Event, Gap> {
        self.chunks.updates().expect("this is always built with an update history in the ctor")
    }

    /// Deduplicate `events` considering all events in `Self::chunks`.
    ///
    /// The returned tuple contains (i) the unique events, and (ii) the
    /// duplicated events (by ID).
    fn filter_duplicated_events<'a, I>(&'a mut self, events: I) -> (Vec<Event>, Vec<OwnedEventId>)
    where
        I: Iterator<Item = Event> + 'a,
    {
        let mut duplicated_event_ids = Vec::new();

        let deduplicated_events = self
            .deduplicator
            .scan_and_learn(events, self)
            .filter_map(|decorated_event| match decorated_event {
                Decoration::Unique(event) => Some(event),
                Decoration::Duplicated(event) => {
                    debug!(event_id = ?event.event_id(), "Found a duplicated event");

                    duplicated_event_ids.push(
                        event
                            .event_id()
                            // SAFETY: An event with no ID is decorated as `Decoration::Invalid`.
                            // Thus, it's safe to unwrap the `Option<OwnedEventId>` here.
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
            .collect();

        (deduplicated_events, duplicated_event_ids)
    }

    /// Return a nice debug string (a vector of lines) for the linked chunk of
    /// events for this room.
    pub fn debug_string(&self) -> Vec<String> {
        let mut result = Vec::new();
        for c in self.chunks() {
            let content = chunk_debug_string(c.content());
            let line = format!("chunk #{}: {content}", c.identifier().index());
            result.push(line);
        }
        result
    }
}

// Private implementations, implementation specific.
impl RoomEvents {
    /// Remove some events from `Self::chunks`.
    ///
    /// This method iterates over all event IDs in `event_ids` and removes the
    /// associated event (if it exists) from `Self::chunks`.
    ///
    /// This is used to remove duplicated events, see
    /// [`Self::filter_duplicated_events`].
    fn remove_events(&mut self, event_ids: Vec<OwnedEventId>) {
        for event_id in event_ids {
            let Some(event_position) = self.revents().find_map(|(position, event)| {
                (event.event_id().as_ref() == Some(&event_id)).then_some(position)
            }) else {
                error!(?event_id, "Cannot find the event to remove");

                continue;
            };

            self.chunks
                .remove_item_at(
                    event_position,
                    // If removing an event results in an empty chunk, the empty chunk is removed
                    // because nothing is going to be inserted in it apparently, otherwise the
                    // `Self::remove_events_and_update_insert_position` method would have been
                    // used.
                    EmptyChunk::Remove,
                )
                .expect("Failed to remove an event we have just found");
        }
    }

    /// Remove all events from `Self::chunks` and update a fix [`Position`].
    ///
    /// This method iterates over all event IDs in `event_ids` and removes the
    /// associated event (if it exists) from `Self::chunks`, exactly like
    /// [`Self::remove_events`]. The difference is that it will maintain a
    /// [`Position`] according to the removals. This is useful for example if
    /// one needs to insert events at a particular position, but it first
    /// collects events that must be removed before the insertions (e.g.
    /// duplicated events). One has to remove events, but also to maintain the
    /// `Position` to its correct initial _target_. Let's see a practical
    /// example:
    ///
    /// ```text
    /// // Pseudo-code.
    ///
    /// let room_events = room_events(['a', 'b', 'c']);
    /// let position = position_of('b' in room_events);
    /// room_events.remove_events(['a'])
    ///
    /// // `position` no longer targets 'b', it now targets 'c', because all
    /// // items have shifted to the left once. Instead, let's do:
    ///
    /// let room_events = room_events(['a', 'b', 'c']);
    /// let position = position_of('b' in room_events);
    /// room_events.remove_events_and_update_insert_position(['a'], &mut position)
    ///
    /// // `position` has been updated to still target 'b'.
    /// ```
    ///
    /// This is used to remove duplicated events, see
    /// [`Self::filter_duplicated_events`].
    fn remove_events_and_update_insert_position(
        &mut self,
        event_ids: Vec<OwnedEventId>,
        position: &mut Position,
    ) {
        for event_id in event_ids {
            let Some(event_position) = self.revents().find_map(|(position, event)| {
                (event.event_id().as_ref() == Some(&event_id)).then_some(position)
            }) else {
                error!(?event_id, "Cannot find the event to remove");

                continue;
            };

            self.chunks
                .remove_item_at(
                    event_position,
                    // If removing an event results in an empty chunk, the empty chunk is kept
                    // because maybe something is going to be inserted in it!
                    EmptyChunk::Keep,
                )
                .expect("Failed to remove an event we have just found");

            // A `Position` is composed of a `ChunkIdentifier` and an index.
            // The `ChunkIdentifier` is stable, i.e. it won't change if an
            // event is removed in another chunk. It means we only need to
            // update `position` if the removal happened in **the same
            // chunk**.
            if event_position.chunk_identifier() == position.chunk_identifier() {
                // Now we can compare the position indices.
                match event_position.index().cmp(&position.index()) {
                    // `event_position`'s index < `position`'s index
                    Ordering::Less => {
                        // An event has been removed _before_ the new
                        // events: `position` needs to be shifted to the
                        // left by 1.
                        position.decrement_index();
                    }

                    // `event_position`'s index >= `position`'s index
                    Ordering::Equal | Ordering::Greater => {
                        // An event has been removed at the _same_ position of
                        // or _after_ the new events: `position` does _NOT_ need
                        // to be modified.
                    }
                }
            }
        }
    }
}

pub(in crate::event_cache) struct AddEventReport {
    /// Number of new unique events that have been added.
    num_new_unique: usize,
    /// Number of events which have been deduplicated.
    num_duplicated: usize,
}

impl AddEventReport {
    /// Were all the events (at least one) we added already known?
    pub fn deduplicated_all_new_events(&self) -> bool {
        self.num_new_unique > 0 && self.num_new_unique == self.num_duplicated
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use matrix_sdk_test::event_factory::EventFactory;
    use ruma::{user_id, EventId, OwnedEventId};

    use super::*;

    macro_rules! assert_events_eq {
        ( $events_iterator:expr, [ $( ( $event_id:ident at ( $chunk_identifier:literal, $index:literal ) ) ),* $(,)? ] ) => {
            {
                let mut events = $events_iterator;

                $(
                    assert_let!(Some((position, event)) = events.next());
                    assert_eq!(position.chunk_identifier(), $chunk_identifier );
                    assert_eq!(position.index(), $index );
                    assert_eq!(event.event_id().unwrap(), $event_id );
                )*

                assert!(events.next().is_none(), "No more events are expected");
            }
        };
    }

    fn new_event(event_id: &str) -> (OwnedEventId, Event) {
        let event_id = EventId::parse(event_id).unwrap();
        let event = EventFactory::new()
            .text_msg("")
            .sender(user_id!("@mnt_io:matrix.org"))
            .event_id(&event_id)
            .into_sync();

        (event_id, event)
    }

    #[test]
    fn test_new_room_events_has_zero_events() {
        let room_events = RoomEvents::new();

        assert_eq!(room_events.events().count(), 0);
    }

    #[test]
    fn test_push_events() {
        let (event_id_0, event_0) = new_event("$ev0");
        let (event_id_1, event_1) = new_event("$ev1");
        let (event_id_2, event_2) = new_event("$ev2");

        let mut room_events = RoomEvents::new();

        room_events.push_events([event_0, event_1]);
        room_events.push_events([event_2]);

        assert_events_eq!(
            room_events.events(),
            [
                (event_id_0 at (0, 0)),
                (event_id_1 at (0, 1)),
                (event_id_2 at (0, 2)),
            ]
        );
    }

    #[test]
    fn test_push_events_with_duplicates() {
        let (event_id_0, event_0) = new_event("$ev0");
        let (event_id_1, event_1) = new_event("$ev1");

        let mut room_events = RoomEvents::new();

        room_events.push_events([event_0.clone(), event_1]);

        assert_events_eq!(
            room_events.events(),
            [
                (event_id_0 at (0, 0)),
                (event_id_1 at (0, 1)),
            ]
        );

        // Everything is alright. Now let's push a duplicated event.
        room_events.push_events([event_0]);

        assert_events_eq!(
            room_events.events(),
            [
                // The first `event_id_0` has been removed.
                (event_id_1 at (0, 0)),
                (event_id_0 at (0, 1)),
            ]
        );
    }

    #[test]
    fn test_push_events_with_duplicates_on_a_chunk_of_one_event() {
        let (event_id_0, event_0) = new_event("$ev0");

        let mut room_events = RoomEvents::new();

        // The first chunk can never be removed, so let's create a gap, then a new
        // chunk.
        room_events.push_gap(Gap { prev_token: "hello".to_owned() });
        room_events.push_events([event_0.clone()]);

        assert_events_eq!(
            room_events.events(),
            [
                (event_id_0 at (2, 0)),
            ]
        );

        // Everything is alright. Now let's push a duplicated event.
        room_events.push_events([event_0]);

        // The event has been removed, then the chunk was empty, so removed, and a new
        // chunk has been created with identifier 3.
        assert_events_eq!(
            room_events.events(),
            [
                (event_id_0 at (3, 0)),
            ]
        );
    }

    #[test]
    fn test_push_gap() {
        let (event_id_0, event_0) = new_event("$ev0");
        let (event_id_1, event_1) = new_event("$ev1");

        let mut room_events = RoomEvents::new();

        room_events.push_events([event_0]);
        room_events.push_gap(Gap { prev_token: "hello".to_owned() });
        room_events.push_events([event_1]);

        assert_events_eq!(
            room_events.events(),
            [
                (event_id_0 at (0, 0)),
                (event_id_1 at (2, 0)),
            ]
        );

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
        let (event_id_0, event_0) = new_event("$ev0");
        let (event_id_1, event_1) = new_event("$ev1");
        let (event_id_2, event_2) = new_event("$ev2");

        let mut room_events = RoomEvents::new();

        room_events.push_events([event_0, event_1]);

        let position_of_event_1 = room_events
            .events()
            .find_map(|(position, event)| {
                (event.event_id().unwrap() == event_id_1).then_some(position)
            })
            .unwrap();

        room_events.insert_events_at([event_2], position_of_event_1).unwrap();

        assert_events_eq!(
            room_events.events(),
            [
                (event_id_0 at (0, 0)),
                (event_id_2 at (0, 1)),
                (event_id_1 at (0, 2)),
            ]
        );
    }

    #[test]
    fn test_insert_events_at_with_duplicates() {
        let (event_id_0, event_0) = new_event("$ev0");
        let (event_id_1, event_1) = new_event("$ev1");
        let (event_id_2, event_2) = new_event("$ev2");
        let (event_id_3, event_3) = new_event("$ev3");

        let mut room_events = RoomEvents::new();

        room_events.push_events([event_0.clone(), event_1, event_2]);

        let position_of_event_2 = room_events
            .events()
            .find_map(|(position, event)| {
                (event.event_id().unwrap() == event_id_2).then_some(position)
            })
            .unwrap();

        assert_events_eq!(
            room_events.events(),
            [
                (event_id_0 at (0, 0)),
                (event_id_1 at (0, 1)),
                (event_id_2 at (0, 2)),
            ]
        );

        // Everything is alright. Now let's insert a duplicated events!
        room_events.insert_events_at([event_0, event_3], position_of_event_2).unwrap();

        assert_events_eq!(
            room_events.events(),
            [
                // The first `event_id_0` has been removed.
                (event_id_1 at (0, 0)),
                (event_id_0 at (0, 1)),
                (event_id_3 at (0, 2)),
                (event_id_2 at (0, 3)),
            ]
        );
    }

    #[test]
    fn test_insert_events_at_with_duplicates_on_a_chunk_of_one_event() {
        let (event_id_0, event_0) = new_event("$ev0");

        let mut room_events = RoomEvents::new();

        // The first chunk can never be removed, so let's create a gap, then a new
        // chunk.
        room_events.push_gap(Gap { prev_token: "hello".to_owned() });
        room_events.push_events([event_0.clone()]);

        let position_of_event_0 = room_events
            .events()
            .find_map(|(position, event)| {
                (event.event_id().unwrap() == event_id_0).then_some(position)
            })
            .unwrap();

        room_events.insert_events_at([event_0], position_of_event_0).unwrap();

        // Event has been removed, the chunk was empty, but it was kept so that the
        // position was still valid and the new event can be inserted.
        assert_events_eq!(
            room_events.events(),
            [
                (event_id_0 at (2, 0)),
            ]
        );
    }

    #[test]
    fn test_insert_gap_at() {
        let (event_id_0, event_0) = new_event("$ev0");
        let (event_id_1, event_1) = new_event("$ev1");

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

        assert_events_eq!(
            room_events.events(),
            [
                (event_id_0 at (0, 0)),
                (event_id_1 at (2, 0)),
            ]
        );

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
        let (event_id_0, event_0) = new_event("$ev0");
        let (event_id_1, event_1) = new_event("$ev1");
        let (event_id_2, event_2) = new_event("$ev2");

        let mut room_events = RoomEvents::new();

        room_events.push_events([event_0]);
        room_events.push_gap(Gap { prev_token: "hello".to_owned() });

        let chunk_identifier_of_gap = room_events
            .chunks()
            .find_map(|chunk| chunk.is_gap().then_some(chunk.identifier()))
            .unwrap();

        room_events.replace_gap_at([event_1, event_2], chunk_identifier_of_gap).unwrap();

        assert_events_eq!(
            room_events.events(),
            [
                (event_id_0 at (0, 0)),
                (event_id_1 at (2, 0)),
                (event_id_2 at (2, 1)),
            ]
        );

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
        let (event_id_0, event_0) = new_event("$ev0");
        let (event_id_1, event_1) = new_event("$ev1");
        let (event_id_2, event_2) = new_event("$ev2");

        let mut room_events = RoomEvents::new();

        room_events.push_events([event_0.clone(), event_1]);
        room_events.push_gap(Gap { prev_token: "hello".to_owned() });

        let chunk_identifier_of_gap = room_events
            .chunks()
            .find_map(|chunk| chunk.is_gap().then_some(chunk.identifier()))
            .unwrap();

        assert_events_eq!(
            room_events.events(),
            [
                (event_id_0 at (0, 0)),
                (event_id_1 at (0, 1)),
            ]
        );

        // Everything is alright. Now let's replace a gap with a duplicated event.
        room_events.replace_gap_at([event_0, event_2], chunk_identifier_of_gap).unwrap();

        assert_events_eq!(
            room_events.events(),
            [
                // The first `event_id_0` has been removed.
                (event_id_1 at (0, 0)),
                (event_id_0 at (2, 0)),
                (event_id_2 at (2, 1)),
            ]
        );

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
    fn test_replace_gap_at_with_no_new_events() {
        let (_, event_0) = new_event("$ev0");
        let (_, event_1) = new_event("$ev1");
        let (_, event_2) = new_event("$ev2");

        let mut room_events = RoomEvents::new();

        room_events.push_events([event_0, event_1]);
        room_events.push_gap(Gap { prev_token: "middle".to_owned() });
        room_events.push_events([event_2]);
        room_events.push_gap(Gap { prev_token: "end".to_owned() });

        // Remove the first gap.
        let first_gap_id = room_events
            .chunks()
            .find_map(|chunk| chunk.is_gap().then_some(chunk.identifier()))
            .unwrap();

        // The next insert position is the next chunk's start.
        let (report, pos) = room_events.replace_gap_at([], first_gap_id).unwrap();
        assert_eq!(pos, Some(Position::new(ChunkIdentifier::new(2), 0)));
        assert_eq!(report.num_new_unique, 0);
        assert_eq!(report.num_duplicated, 0);

        // Remove the second gap.
        let second_gap_id = room_events
            .chunks()
            .find_map(|chunk| chunk.is_gap().then_some(chunk.identifier()))
            .unwrap();

        // No next insert position.
        let (report, pos) = room_events.replace_gap_at([], second_gap_id).unwrap();
        assert!(pos.is_none());
        assert_eq!(report.num_new_unique, 0);
        assert_eq!(report.num_duplicated, 0);
    }

    #[test]
    fn test_remove_events() {
        let (event_id_0, event_0) = new_event("$ev0");
        let (event_id_1, event_1) = new_event("$ev1");
        let (event_id_2, event_2) = new_event("$ev2");
        let (event_id_3, event_3) = new_event("$ev3");

        // Push some events.
        let mut room_events = RoomEvents::new();
        room_events.push_events([event_0, event_1]);
        room_events.push_gap(Gap { prev_token: "hello".to_owned() });
        room_events.push_events([event_2, event_3]);

        assert_events_eq!(
            room_events.events(),
            [
                (event_id_0 at (0, 0)),
                (event_id_1 at (0, 1)),
                (event_id_2 at (2, 0)),
                (event_id_3 at (2, 1)),
            ]
        );
        assert_eq!(room_events.chunks().count(), 3);

        // Remove some events.
        room_events.remove_events(vec![event_id_1, event_id_3]);

        assert_events_eq!(
            room_events.events(),
            [
                (event_id_0 at (0, 0)),
                (event_id_2 at (2, 0)),
            ]
        );

        // Ensure chunks are removed once empty.
        room_events.remove_events(vec![event_id_2]);

        assert_events_eq!(
            room_events.events(),
            [
                (event_id_0 at (0, 0)),
            ]
        );
        assert_eq!(room_events.chunks().count(), 2);
    }

    #[test]
    fn test_remove_events_unknown_event() {
        let (event_id_0, _event_0) = new_event("$ev0");

        // Push ZERO event.
        let mut room_events = RoomEvents::new();

        assert_events_eq!(room_events.events(), []);

        // Remove one undefined event.
        // No error is expected.
        room_events.remove_events(vec![event_id_0]);

        assert_events_eq!(room_events.events(), []);

        let mut events = room_events.events();
        assert!(events.next().is_none());
    }

    #[test]
    fn test_remove_events_and_update_insert_position() {
        let (event_id_0, event_0) = new_event("$ev0");
        let (event_id_1, event_1) = new_event("$ev1");
        let (event_id_2, event_2) = new_event("$ev2");
        let (event_id_3, event_3) = new_event("$ev3");
        let (event_id_4, event_4) = new_event("$ev4");
        let (event_id_5, event_5) = new_event("$ev5");
        let (event_id_6, event_6) = new_event("$ev6");
        let (event_id_7, event_7) = new_event("$ev7");
        let (event_id_8, event_8) = new_event("$ev8");

        // Push some events.
        let mut room_events = RoomEvents::new();
        room_events.push_events([event_0, event_1, event_2, event_3, event_4, event_5, event_6]);
        room_events.push_gap(Gap { prev_token: "raclette".to_owned() });
        room_events.push_events([event_7, event_8]);

        assert_eq!(room_events.chunks().count(), 3);

        fn position_of(room_events: &RoomEvents, event_id: &EventId) -> Position {
            room_events
                .events()
                .find_map(|(position, event)| {
                    (event.event_id().unwrap() == event_id).then_some(position)
                })
                .unwrap()
        }

        // In the same chunk…
        {
            // Get the position of `event_4`.
            let mut position = position_of(&room_events, &event_id_4);

            // Remove one event BEFORE `event_4`.
            //
            // The position must move to the left by 1.
            {
                let previous_position = position;
                room_events
                    .remove_events_and_update_insert_position(vec![event_id_0], &mut position);

                assert_eq!(previous_position.chunk_identifier(), position.chunk_identifier());
                assert_eq!(previous_position.index() - 1, position.index());

                // It still represents the position of `event_4`.
                assert_eq!(position, position_of(&room_events, &event_id_4));
            }

            // Remove one event AFTER `event_4`.
            //
            // The position must not move.
            {
                let previous_position = position;
                room_events
                    .remove_events_and_update_insert_position(vec![event_id_5], &mut position);

                assert_eq!(previous_position.chunk_identifier(), position.chunk_identifier());
                assert_eq!(previous_position.index(), position.index());

                // It still represents the position of `event_4`.
                assert_eq!(position, position_of(&room_events, &event_id_4));
            }

            // Remove one event: `event_4`.
            //
            // The position must not move.
            {
                let previous_position = position;
                room_events
                    .remove_events_and_update_insert_position(vec![event_id_4], &mut position);

                assert_eq!(previous_position.chunk_identifier(), position.chunk_identifier());
                assert_eq!(previous_position.index(), position.index());
            }

            // Check the events.
            assert_events_eq!(
                room_events.events(),
                [
                    (event_id_1 at (0, 0)),
                    (event_id_2 at (0, 1)),
                    (event_id_3 at (0, 2)),
                    (event_id_6 at (0, 3)),
                    (event_id_7 at (2, 0)),
                    (event_id_8 at (2, 1)),
                ]
            );
        }

        // In another chunk…
        {
            // Get the position of `event_7`.
            let mut position = position_of(&room_events, &event_id_7);

            // Remove one event BEFORE `event_7`.
            //
            // The position must not move because it happens in another chunk.
            {
                let previous_position = position;
                room_events
                    .remove_events_and_update_insert_position(vec![event_id_1], &mut position);

                assert_eq!(previous_position.chunk_identifier(), position.chunk_identifier());
                assert_eq!(previous_position.index(), position.index());

                // It still represents the position of `event_7`.
                assert_eq!(position, position_of(&room_events, &event_id_7));
            }

            // Check the events.
            assert_events_eq!(
                room_events.events(),
                [
                    (event_id_2 at (0, 0)),
                    (event_id_3 at (0, 1)),
                    (event_id_6 at (0, 2)),
                    (event_id_7 at (2, 0)),
                    (event_id_8 at (2, 1)),
                ]
            );
        }

        // In the same chunk, but remove multiple events, just for the fun and to ensure
        // the loop works correctly.
        {
            // Get the position of `event_6`.
            let mut position = position_of(&room_events, &event_id_6);

            // Remove three events BEFORE `event_6`.
            //
            // The position must move.
            {
                let previous_position = position;
                room_events.remove_events_and_update_insert_position(
                    vec![event_id_2, event_id_3, event_id_7, event_id_8],
                    &mut position,
                );

                assert_eq!(previous_position.chunk_identifier(), position.chunk_identifier());
                assert_eq!(previous_position.index() - 2, position.index());

                // It still represents the position of `event_6`.
                assert_eq!(position, position_of(&room_events, &event_id_6));
            }

            // Check the events.
            assert_events_eq!(
                room_events.events(),
                [
                    (event_id_6 at (0, 0)),
                ]
            );
        }

        // Ensure no chunk has been removed.
        assert_eq!(room_events.chunks().count(), 3);
    }

    #[test]
    fn test_reset() {
        let (event_id_0, event_0) = new_event("$ev0");
        let (event_id_1, event_1) = new_event("$ev1");
        let (event_id_2, event_2) = new_event("$ev2");
        let (event_id_3, event_3) = new_event("$ev3");

        // Push some events.
        let mut room_events = RoomEvents::new();
        room_events.push_events([event_0, event_1]);
        room_events.push_gap(Gap { prev_token: "raclette".to_owned() });
        room_events.push_events([event_2]);

        // Read the updates as `VectorDiff`.
        let diffs = room_events.updates_as_vector_diffs();

        assert_eq!(diffs.len(), 2);

        assert_matches!(
            &diffs[0],
            VectorDiff::Append { values } => {
                assert_eq!(values.len(), 2);
                assert_eq!(values[0].event_id(), Some(event_id_0));
                assert_eq!(values[1].event_id(), Some(event_id_1));
            }
        );
        assert_matches!(
            &diffs[1],
            VectorDiff::Append { values } => {
                assert_eq!(values.len(), 1);
                assert_eq!(values[0].event_id(), Some(event_id_2));
            }
        );

        // Now we can reset and see what happens.
        room_events.reset();
        room_events.push_events([event_3]);

        // Read the updates as `VectorDiff`.
        let diffs = room_events.updates_as_vector_diffs();

        assert_eq!(diffs.len(), 2);

        assert_matches!(&diffs[0], VectorDiff::Clear);
        assert_matches!(
            &diffs[1],
            VectorDiff::Append { values } => {
                assert_eq!(values.len(), 1);
                assert_eq!(values[0].event_id(), Some(event_id_3));
            }
        );
    }
}
