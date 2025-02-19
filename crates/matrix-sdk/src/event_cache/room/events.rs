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

use eyeball_im::VectorDiff;
pub use matrix_sdk_base::event_cache::{Event, Gap};
use matrix_sdk_base::{
    apply_redaction,
    event_cache::store::DEFAULT_CHUNK_CAPACITY,
    linked_chunk::{
        lazy_loader::{self, LazyLoaderError},
        ChunkContent, ChunkIdentifierGenerator, RawChunk,
    },
};
use matrix_sdk_common::linked_chunk::{
    AsVector, Chunk, ChunkIdentifier, EmptyChunk, Error, Iter, IterBackward, LinkedChunk,
    ObservableUpdates, Position,
};
use ruma::{
    events::{room::redaction::SyncRoomRedactionEvent, AnySyncTimelineEvent, MessageLikeEventType},
    OwnedEventId, RoomVersionId,
};
use tracing::{error, instrument, trace, warn};

/// This type represents all events of a single room.
#[derive(Debug)]
pub struct RoomEvents {
    /// The real in-memory storage for all the events.
    chunks: LinkedChunk<DEFAULT_CHUNK_CAPACITY, Event, Gap>,

    /// Type mapping [`Update`]s from [`Self::chunks`] to [`VectorDiff`]s.
    ///
    /// [`Update`]: matrix_sdk_base::linked_chunk::Update
    chunks_updates_as_vectordiffs: AsVector<Event, Gap>,
}

impl Default for RoomEvents {
    fn default() -> Self {
        Self::new()
    }
}

impl RoomEvents {
    /// Build a new [`RoomEvents`] struct with zero events.
    pub fn new() -> Self {
        Self::with_initial_linked_chunk(None)
    }

    /// Build a new [`RoomEvents`] struct with prior chunks knowledge.
    ///
    /// The provided [`LinkedChunk`] must have been built with update history.
    pub fn with_initial_linked_chunk(
        linked_chunk: Option<LinkedChunk<DEFAULT_CHUNK_CAPACITY, Event, Gap>>,
    ) -> Self {
        let mut linked_chunk = linked_chunk.unwrap_or_else(LinkedChunk::new_with_update_history);

        let chunks_updates_as_vectordiffs = linked_chunk
            .as_vector()
            // SAFETY: The `LinkedChunk` has been built with `new_with_update_history`, so
            // `as_vector` must return `Some(…)`.
            .expect("`LinkedChunk` must have been built with `new_with_update_history`");

        Self { chunks: linked_chunk, chunks_updates_as_vectordiffs }
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

    /// Replace the events with the given last chunk of events and generator.
    ///
    /// This clears all the chunks in memory before resetting to the new chunk,
    /// if provided.
    pub(super) fn replace_with(
        &mut self,
        last_chunk: Option<RawChunk<Event, Gap>>,
        chunk_identifier_generator: ChunkIdentifierGenerator,
    ) -> Result<(), LazyLoaderError> {
        lazy_loader::replace_with(&mut self.chunks, last_chunk, chunk_identifier_generator)
    }

    /// If the given event is a redaction, try to retrieve the to-be-redacted
    /// event in the chunk, and replace it by the redacted form.
    #[instrument(skip_all)]
    fn maybe_apply_new_redaction(&mut self, room_version: &RoomVersionId, event: &Event) {
        let raw_event = event.raw();

        // Do not deserialise the entire event if we aren't certain it's a
        // `m.room.redaction`. It saves a non-negligible amount of computations.
        let Ok(Some(MessageLikeEventType::RoomRedaction)) =
            raw_event.get_field::<MessageLikeEventType>("type")
        else {
            return;
        };

        // It is a `m.room.redaction`! We can deserialize it entirely.

        let Ok(AnySyncTimelineEvent::MessageLike(
            ruma::events::AnySyncMessageLikeEvent::RoomRedaction(redaction),
        )) = event.raw().deserialize()
        else {
            return;
        };

        let Some(event_id) = redaction.redacts(room_version) else {
            warn!("missing target event id from the redaction event");
            return;
        };

        // Replace the redacted event by a redacted form, if we knew about it.
        let mut items = self.chunks.items();

        if let Some((pos, target_event)) =
            items.find(|(_, item)| item.event_id().as_deref() == Some(event_id))
        {
            // Don't redact already redacted events.
            if let Ok(deserialized) = target_event.raw().deserialize() {
                match deserialized {
                    AnySyncTimelineEvent::MessageLike(ev) => {
                        if ev.original_content().is_none() {
                            // Already redacted.
                            return;
                        }
                    }
                    AnySyncTimelineEvent::State(ev) => {
                        if ev.original_content().is_none() {
                            // Already redacted.
                            return;
                        }
                    }
                }
            }

            if let Some(redacted_event) = apply_redaction(
                target_event.raw(),
                event.raw().cast_ref::<SyncRoomRedactionEvent>(),
                room_version,
            ) {
                let mut copy = target_event.clone();

                // It's safe to cast `redacted_event` here:
                // - either the event was an `AnyTimelineEvent` cast to `AnySyncTimelineEvent`
                //   when calling .raw(), so it's still one under the hood.
                // - or it wasn't, and it's a plain `AnySyncTimelineEvent` in this case.
                copy.replace_raw(redacted_event.cast());

                // Get rid of the immutable borrow on self.chunks.
                drop(items);

                self.chunks
                    .replace_item_at(pos, copy)
                    .expect("should have been a valid position of an item");
            }
        } else {
            trace!("redacted event is missing from the linked chunk");
        }

        // TODO: remove all related events too!
    }

    /// Callback to call whenever we touch events in the database.
    pub fn on_new_events<'a>(
        &mut self,
        room_version: &RoomVersionId,
        events: impl Iterator<Item = &'a Event>,
    ) {
        for ev in events {
            self.maybe_apply_new_redaction(room_version, ev);
        }
    }

    /// Push events after all events or gaps.
    ///
    /// The last event in `events` is the most recent one.
    pub fn push_events<I>(&mut self, events: I)
    where
        I: IntoIterator<Item = Event>,
        I::IntoIter: ExactSizeIterator,
    {
        self.chunks.push_items_back(events);
    }

    /// Push a gap after all events or gaps.
    pub fn push_gap(&mut self, gap: Gap) {
        self.chunks.push_gap_back(gap);
    }

    /// Insert events at a specified position.
    pub fn insert_events_at(
        &mut self,
        events: Vec<Event>,
        position: Position,
    ) -> Result<(), Error> {
        self.chunks.insert_items_at(events, position)?;
        Ok(())
    }

    /// Insert a gap at a specified position.
    pub fn insert_gap_at(&mut self, gap: Gap, position: Position) -> Result<(), Error> {
        self.chunks.insert_gap_at(gap, position)
    }

    /// Remove a gap at the given position.
    ///
    /// Returns the next insert position, if any, left after the gap that has
    /// just been removed.
    pub fn remove_gap_at(&mut self, gap: ChunkIdentifier) -> Result<Option<Position>, Error> {
        self.chunks.remove_gap_at(gap)
    }

    /// Replace the gap identified by `gap_identifier`, by events.
    ///
    /// Because the `gap_identifier` can represent non-gap chunk, this method
    /// returns a `Result`.
    ///
    /// This method returns the position of the (first if many) newly created
    /// `Chunk` that   contains the `items`.
    pub fn replace_gap_at(
        &mut self,
        events: Vec<Event>,
        gap_identifier: ChunkIdentifier,
    ) -> Result<Option<Position>, Error> {
        let next_pos = if events.is_empty() {
            // There are no new events, so there's no need to create a new empty items
            // chunk; instead, remove the gap.
            self.chunks.remove_gap_at(gap_identifier)?
        } else {
            // Replace the gap by new events.
            Some(self.chunks.replace_gap_at(events, gap_identifier)?.first_position())
        };

        Ok(next_pos)
    }

    /// Remove some events from the linked chunk.
    ///
    /// This method iterates over all event IDs in `event_ids` and removes the
    /// associated event (if it exists) from `Self::chunks`.
    pub fn remove_events_by_id(&mut self, event_ids: Vec<OwnedEventId>) {
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
                    // because nothing is going to be inserted in it apparently.
                    EmptyChunk::Remove,
                )
                .expect("Failed to remove an event we have just found");
        }
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
    pub fn updates_as_vector_diffs(&mut self) -> Vec<VectorDiff<Event>> {
        self.chunks_updates_as_vectordiffs.take()
    }

    /// Get a mutable reference to the [`LinkedChunk`] updates, aka
    /// [`ObservableUpdates`] to be consumed by the store.
    ///
    /// These updates are expected to be *only* forwarded to storage, as they
    /// might hide some underlying updates to the in-memory chunk; those
    /// updates should be reflected with manual updates to
    /// [`Self::chunks_updates_as_vectordiffs`].
    pub(super) fn store_updates(&mut self) -> &mut ObservableUpdates<Event, Gap> {
        self.chunks.updates().expect("this is always built with an update history in the ctor")
    }

    /// Return a nice debug string (a vector of lines) for the linked chunk of
    /// events for this room.
    pub fn debug_string(&self) -> Vec<String> {
        let mut result = Vec::new();

        for chunk in self.chunks() {
            let content = chunk_debug_string(chunk.content());
            let line = format!("chunk #{}: {content}", chunk.identifier().index());

            result.push(line);
        }

        result
    }
}

// Private implementations, implementation specific.
impl RoomEvents {
    pub(super) fn insert_new_chunk_as_first(
        &mut self,
        raw_new_first_chunk: RawChunk<Event, Gap>,
    ) -> Result<(), LazyLoaderError> {
        lazy_loader::insert_new_first_chunk(&mut self.chunks, raw_new_first_chunk)
    }
}

/// Create a debug string for a [`ChunkContent`] for an event/gap pair.
fn chunk_debug_string(content: &ChunkContent<Event, Gap>) -> String {
    match content {
        ChunkContent::Gap(Gap { prev_token }) => {
            format!("gap['{prev_token}']")
        }
        ChunkContent::Items(vec) => {
            let items = vec
                .iter()
                .map(|event| {
                    // Limit event ids to 8 chars *after* the $.
                    event.event_id().map_or_else(
                        || "<no event id>".to_owned(),
                        |id| id.as_str().chars().take(1 + 8).collect(),
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");

            format!("events[{items}]")
        }
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use matrix_sdk_test::{event_factory::EventFactory, ALICE, DEFAULT_TEST_ROOM_ID};
    use ruma::{event_id, user_id, EventId, OwnedEventId};

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
            .into_event();

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

        room_events.insert_events_at(vec![event_2], position_of_event_1).unwrap();

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

        room_events.replace_gap_at(vec![event_1, event_2], chunk_identifier_of_gap).unwrap();

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
        let pos = room_events.replace_gap_at(vec![], first_gap_id).unwrap();
        assert_eq!(pos, Some(Position::new(ChunkIdentifier::new(2), 0)));

        // Remove the second gap.
        let second_gap_id = room_events
            .chunks()
            .find_map(|chunk| chunk.is_gap().then_some(chunk.identifier()))
            .unwrap();

        // No next insert position.
        let pos = room_events.replace_gap_at(vec![], second_gap_id).unwrap();
        assert!(pos.is_none());
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
        room_events.remove_events_by_id(vec![event_id_1, event_id_3]);

        assert_events_eq!(
            room_events.events(),
            [
                (event_id_0 at (0, 0)),
                (event_id_2 at (2, 0)),
            ]
        );

        // Ensure chunks are removed once empty.
        room_events.remove_events_by_id(vec![event_id_2]);

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
        room_events.remove_events_by_id(vec![event_id_0]);

        assert_events_eq!(room_events.events(), []);

        let mut events = room_events.events();
        assert!(events.next().is_none());
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

    #[test]
    fn test_debug_string() {
        let event_factory = EventFactory::new().room(&DEFAULT_TEST_ROOM_ID).sender(*ALICE);

        let mut room_events = RoomEvents::new();
        room_events.push_events(vec![
            event_factory
                .text_msg("hey")
                .event_id(event_id!("$123456789101112131415617181920"))
                .into_event(),
            event_factory.text_msg("you").event_id(event_id!("$2")).into_event(),
        ]);
        room_events.push_gap(Gap { prev_token: "raclette".to_owned() });

        let output = room_events.debug_string();

        assert_eq!(output.len(), 2);
        assert_eq!(&output[0], "chunk #0: events[$12345678, $2]");
        assert_eq!(&output[1], "chunk #1: gap['raclette']");
    }
}
