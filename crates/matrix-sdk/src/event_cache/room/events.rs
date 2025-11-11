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

use as_variant::as_variant;
use eyeball_im::VectorDiff;
pub use matrix_sdk_base::event_cache::{Event, Gap};
use matrix_sdk_base::{
    event_cache::store::DEFAULT_CHUNK_CAPACITY,
    linked_chunk::{
        ChunkContent, ChunkIdentifierGenerator, ChunkMetadata, OrderTracker, RawChunk,
        lazy_loader::{self, LazyLoaderError},
    },
};
use matrix_sdk_common::linked_chunk::{
    AsVector, Chunk, ChunkIdentifier, Error, Iter, IterBackward, LinkedChunk, ObservableUpdates,
    Position,
};
use tracing::trace;

/// This type represents a linked chunk of events for a single room or thread.
#[derive(Debug)]
pub(in crate::event_cache) struct EventLinkedChunk {
    /// The real in-memory storage for all the events.
    chunks: LinkedChunk<DEFAULT_CHUNK_CAPACITY, Event, Gap>,

    /// Type mapping [`Update`]s from [`Self::chunks`] to [`VectorDiff`]s.
    ///
    /// [`Update`]: matrix_sdk_base::linked_chunk::Update
    chunks_updates_as_vectordiffs: AsVector<Event, Gap>,

    /// Tracker of the events ordering in this room.
    pub order_tracker: OrderTracker<Event, Gap>,
}

impl Default for EventLinkedChunk {
    fn default() -> Self {
        Self::new()
    }
}

impl EventLinkedChunk {
    /// Build a new [`EventLinkedChunk`] struct with zero events.
    pub fn new() -> Self {
        Self::with_initial_linked_chunk(None, None)
    }

    /// Build a new [`EventLinkedChunk`] struct with prior chunks knowledge.
    ///
    /// The provided [`LinkedChunk`] must have been built with update history.
    pub fn with_initial_linked_chunk(
        linked_chunk: Option<LinkedChunk<DEFAULT_CHUNK_CAPACITY, Event, Gap>>,
        full_linked_chunk_metadata: Option<Vec<ChunkMetadata>>,
    ) -> Self {
        let mut linked_chunk = linked_chunk.unwrap_or_else(LinkedChunk::new_with_update_history);

        let chunks_updates_as_vectordiffs = linked_chunk
            .as_vector()
            .expect("`LinkedChunk` must have been built with `new_with_update_history`");

        let order_tracker = linked_chunk
            .order_tracker(full_linked_chunk_metadata)
            .expect("`LinkedChunk` must have been built with `new_with_update_history`");

        Self { chunks: linked_chunk, chunks_updates_as_vectordiffs, order_tracker }
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
    #[cfg(test)]
    pub(in crate::event_cache) fn push_events<I>(&mut self, events: I)
    where
        I: IntoIterator<Item = Event>,
        I::IntoIter: ExactSizeIterator,
    {
        self.chunks.push_items_back(events);
    }

    /// Replace the gap identified by `gap_identifier`, by events.
    ///
    /// Because the `gap_identifier` can represent non-gap chunk, this method
    /// returns a `Result`.
    ///
    /// This method returns the position of the (first if many) newly created
    /// `Chunk` that contains the `items`.
    fn replace_gap_at(
        &mut self,
        gap_identifier: ChunkIdentifier,
        events: Vec<Event>,
    ) -> Result<Option<Position>, Error> {
        // As an optimization, we'll remove the chunk if it's a gap that would be
        // replaced with no events.
        //
        // However, our linked chunk requires that it includes at least one chunk in the
        // in-memory representation. We could tweak this invariant, but in the
        // meanwhile, don't remove the gap chunk if it's the only one we know
        // about.
        let has_only_one_chunk = {
            let mut it = self.chunks.chunks();

            // If there's no chunks at all, then we won't be able to find the gap chunk.
            let _ =
                it.next().ok_or(Error::InvalidChunkIdentifier { identifier: gap_identifier })?;

            // If there's no next chunk, we can conclude there's only one.
            it.next().is_none()
        };

        let next_pos = if events.is_empty() && !has_only_one_chunk {
            // There are no new events, so there's no need to create a new empty items
            // chunk; instead, remove the gap.
            self.chunks.remove_empty_chunk_at(gap_identifier)?
        } else {
            // Replace the gap by new events.
            Some(self.chunks.replace_gap_at(events, gap_identifier)?.first_position())
        };

        Ok(next_pos)
    }

    /// Remove some events from the linked chunk.
    ///
    /// If a chunk becomes empty, it's going to be removed.
    pub fn remove_events_by_position(&mut self, mut positions: Vec<Position>) -> Result<(), Error> {
        sort_positions_descending(&mut positions);

        for position in positions {
            self.chunks.remove_item_at(position)?;
        }

        Ok(())
    }

    /// Replace event at a specified position.
    ///
    /// `position` must point to a valid item, otherwise the method returns an
    /// error.
    pub fn replace_event_at(&mut self, position: Position, event: Event) -> Result<(), Error> {
        self.chunks.replace_item_at(position, event)
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

    /// Return the order of an event in the room linked chunk.
    ///
    /// Can return `None` if the event can't be found in the linked chunk.
    pub fn event_order(&self, event_pos: Position) -> Option<usize> {
        self.order_tracker.ordering(event_pos)
    }

    #[cfg(any(test, debug_assertions))]
    #[allow(dead_code)] // Temporarily, until we figure out why it's crashing production builds.
    fn assert_event_ordering(&self) {
        let mut iter = self.chunks.items().enumerate();
        let Some((i, (first_event_pos, _))) = iter.next() else {
            return;
        };

        // Sanity check.
        assert_eq!(i, 0);

        // That's the offset in the full linked chunk. Will be 0 if the linked chunk is
        // entirely loaded, may be non-zero otherwise.
        let offset =
            self.event_order(first_event_pos).expect("first event's ordering must be known");

        for (i, (next_pos, _)) in iter {
            let next_index =
                self.event_order(next_pos).expect("next event's ordering must be known");
            assert_eq!(offset + i, next_index, "event ordering must be continuous");
        }
    }

    /// Get all updates from the room events as [`VectorDiff`].
    ///
    /// Be careful that each `VectorDiff` is returned only once!
    ///
    /// See [`AsVector`] to learn more.
    pub fn updates_as_vector_diffs(&mut self) -> Vec<VectorDiff<Event>> {
        let updates = self.chunks_updates_as_vectordiffs.take();

        self.order_tracker.flush_updates(false);

        updates
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

        for chunk in self.chunks.chunks() {
            let content =
                chunk_debug_string(chunk.identifier(), chunk.content(), &self.order_tracker);
            let lazy_previous = if let Some(cid) = chunk.lazy_previous() {
                format!(" (lazy previous = {})", cid.index())
            } else {
                "".to_owned()
            };
            let line = format!("chunk #{}{lazy_previous}: {content}", chunk.identifier().index());

            result.push(line);
        }

        result
    }

    /// Return the latest gap, if any.
    ///
    /// Latest means "closest to the end", or, since events are ordered
    /// according to the sync ordering, this means "the most recent one".
    pub fn rgap(&self) -> Option<Gap> {
        self.rchunks()
            .find_map(|chunk| as_variant!(chunk.content(), ChunkContent::Gap(gap) => gap.clone()))
    }

    /// Add the previous back-pagination token (if present), followed by the
    /// timeline events themselves.
    pub fn push_live_events(&mut self, new_gap: Option<Gap>, events: &[Event]) {
        if let Some(new_gap) = new_gap {
            // As a tiny optimization: remove the last chunk if it's an empty event
            // one, as it's not useful to keep it before a gap.
            let prev_chunk_to_remove = self.rchunks().next().and_then(|chunk| {
                (chunk.is_items() && chunk.num_items() == 0).then_some(chunk.identifier())
            });

            self.chunks.push_gap_back(new_gap);

            if let Some(prev_chunk_to_remove) = prev_chunk_to_remove {
                self.chunks
                    .remove_empty_chunk_at(prev_chunk_to_remove)
                    .expect("we just checked the chunk is there, and it's an empty item chunk");
            }
        }

        self.chunks.push_items_back(events.to_vec());
    }

    /// Finish a network back-pagination for this linked chunk by updating the
    /// in-memory linked chunk with the results.
    ///
    /// ## Arguments
    ///
    /// - `prev_gap_id`: the identifier of the previous gap, if any.
    /// - `new_gap`: the new gap to insert, if any. If missing, we've likely
    ///   reached the start of the timeline.
    /// - `events`: new events to insert, in the topological ordering (i.e. from
    ///   oldest to most recent).
    ///
    /// ## Returns
    ///
    /// Returns a boolean indicating whether we've hit the start of the
    /// timeline/linked chunk.
    pub fn finish_back_pagination(
        &mut self,
        prev_gap_id: Option<ChunkIdentifier>,
        new_gap: Option<Gap>,
        events: &[Event],
    ) -> bool {
        let first_event_pos = self.events().next().map(|(item_pos, _)| item_pos);

        // First, insert events.
        let insert_new_gap_pos = if let Some(gap_id) = prev_gap_id {
            // There is a prior gap, let's replace it with the new events!
            trace!("replacing previous gap with the back-paginated events");

            // Replace the gap with the events we just deduplicated. This might get rid of
            // the underlying gap, if the conditions are favorable to
            // us.
            self.replace_gap_at(gap_id, events.to_vec())
                .expect("gap_identifier is a valid chunk id we read previously")
        } else if let Some(pos) = first_event_pos {
            // No prior gap, but we had some events: assume we need to prepend events
            // before those.
            trace!("inserted events before the first known event");

            self.chunks
                .insert_items_at(pos, events.to_vec())
                .expect("pos is a valid position we just read above");

            Some(pos)
        } else {
            // No prior gap, and no prior events: push the events.
            trace!("pushing events received from back-pagination");

            self.chunks.push_items_back(events.to_vec());

            // A new gap may be inserted before the new events, if there are any.
            self.events().next().map(|(item_pos, _)| item_pos)
        };

        // And insert the new gap if needs be.
        //
        // We only do this when at least one new, non-duplicated event, has been added
        // to the chunk. Otherwise it means we've back-paginated all the
        // known events.
        let has_new_gap = new_gap.is_some();
        if let Some(new_gap) = new_gap {
            if let Some(new_pos) = insert_new_gap_pos {
                self.chunks
                    .insert_gap_at(new_gap, new_pos)
                    .expect("events_chunk_pos represents a valid chunk position");
            } else {
                self.chunks.push_gap_back(new_gap);
            }
        }

        // There could be an inconsistency between the network (which thinks we hit the
        // start of the timeline) and the disk (which has the initial empty
        // chunks), so tweak the `reached_start` value so that it reflects the
        // disk state in priority instead.

        let has_gaps = self.chunks().any(|chunk| chunk.is_gap());

        // Whether the first chunk has no predecessors or not.
        let first_chunk_is_definitive_head =
            self.chunks().next().map(|chunk| chunk.is_definitive_head());

        let network_reached_start = !has_new_gap;
        let reached_start =
            !has_gaps && first_chunk_is_definitive_head.unwrap_or(network_reached_start);

        trace!(
            ?network_reached_start,
            ?has_gaps,
            ?first_chunk_is_definitive_head,
            ?reached_start,
            "finished handling network back-pagination"
        );

        reached_start
    }
}

// Methods related to lazy-loading.
impl EventLinkedChunk {
    /// Inhibits all the linked chunk updates caused by the function `f` on the
    /// ordering tracker.
    ///
    /// Updates to the linked chunk that happen because of lazy loading must not
    /// be taken into account by the order tracker, otherwise the
    /// fully-loaded state (tracked by the order tracker) wouldn't match
    /// reality anymore. This provides a facility to help applying such
    /// updates.
    fn inhibit_updates_to_ordering_tracker<F: FnOnce(&mut Self) -> R, R>(&mut self, f: F) -> R {
        // Start by flushing previous pending updates to the chunk ordering, if any.
        self.order_tracker.flush_updates(false);

        // Call the function.
        let r = f(self);

        // Now, flush other pending updates which have been caused by the function, and
        // ignore them.
        self.order_tracker.flush_updates(true);

        r
    }

    /// Replace the events with the given last chunk of events and generator.
    ///
    /// Happens only during lazy loading.
    ///
    /// This clears all the chunks in memory before resetting to the new chunk,
    /// if provided.
    pub(super) fn replace_with(
        &mut self,
        last_chunk: Option<RawChunk<Event, Gap>>,
        chunk_identifier_generator: ChunkIdentifierGenerator,
    ) -> Result<(), LazyLoaderError> {
        // Since `replace_with` is used only to unload some chunks, we don't want it to
        // affect the chunk ordering.
        self.inhibit_updates_to_ordering_tracker(move |this| {
            lazy_loader::replace_with(&mut this.chunks, last_chunk, chunk_identifier_generator)
        })
    }

    /// Prepends a lazily-loaded chunk at the beginning of the linked chunk.
    pub(super) fn insert_new_chunk_as_first(
        &mut self,
        raw_new_first_chunk: RawChunk<Event, Gap>,
    ) -> Result<(), LazyLoaderError> {
        // This is only used when reinserting a chunk that was in persisted storage, so
        // we don't need to touch the chunk ordering for this.
        self.inhibit_updates_to_ordering_tracker(move |this| {
            lazy_loader::insert_new_first_chunk(&mut this.chunks, raw_new_first_chunk)
        })
    }
}

/// Create a debug string for a [`ChunkContent`] for an event/gap pair.
fn chunk_debug_string(
    chunk_id: ChunkIdentifier,
    content: &ChunkContent<Event, Gap>,
    order_tracker: &OrderTracker<Event, Gap>,
) -> String {
    match content {
        ChunkContent::Gap(Gap { prev_token }) => {
            format!("gap['{prev_token}']")
        }
        ChunkContent::Items(vec) => {
            let items = vec
                .iter()
                .enumerate()
                .map(|(i, event)| {
                    event.event_id().map_or_else(
                        || "<no event id>".to_owned(),
                        |id| {
                            let pos = Position::new(chunk_id, i);
                            let order = format!("#{}: ", order_tracker.ordering(pos).unwrap());

                            // Limit event ids to 8 chars *after* the $.
                            let event_id = id.as_str().chars().take(1 + 8).collect::<String>();

                            format!("{order}{event_id}")
                        },
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");

            format!("events[{items}]")
        }
    }
}

/// Sort positions of events so that events can be removed safely without
/// messing their position.
///
/// Events must be sorted by their position index, from greatest to lowest, so
/// that all positions remain valid inside the same chunk while they are being
/// removed. For the sake of debugability, we also sort by position chunk
/// identifier, but this is not required.
pub(super) fn sort_positions_descending(positions: &mut [Position]) {
    positions.sort_by(|a, b| {
        b.chunk_identifier()
            .cmp(&a.chunk_identifier())
            .then_with(|| a.index().cmp(&b.index()).reverse())
    });
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use matrix_sdk_test::{ALICE, DEFAULT_TEST_ROOM_ID, event_factory::EventFactory};
    use ruma::{EventId, OwnedEventId, event_id, user_id};

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
    fn test_new_event_linked_chunk_has_zero_events() {
        let linked_chunk = EventLinkedChunk::new();

        assert_eq!(linked_chunk.events().count(), 0);
    }

    #[test]
    fn test_replace_gap_at() {
        let (event_id_0, event_0) = new_event("$ev0");
        let (event_id_1, event_1) = new_event("$ev1");
        let (event_id_2, event_2) = new_event("$ev2");

        let mut linked_chunk = EventLinkedChunk::new();

        linked_chunk.chunks.push_items_back([event_0]);
        linked_chunk.chunks.push_gap_back(Gap { prev_token: "hello".to_owned() });

        let gap_chunk_id = linked_chunk
            .chunks()
            .find_map(|chunk| chunk.is_gap().then_some(chunk.identifier()))
            .unwrap();

        linked_chunk.replace_gap_at(gap_chunk_id, vec![event_1, event_2]).unwrap();

        assert_events_eq!(
            linked_chunk.events(),
            [
                (event_id_0 at (0, 0)),
                (event_id_1 at (2, 0)),
                (event_id_2 at (2, 1)),
            ]
        );

        {
            let mut chunks = linked_chunk.chunks();

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

        let mut linked_chunk = EventLinkedChunk::new();

        linked_chunk.chunks.push_items_back([event_0, event_1]);
        linked_chunk.chunks.push_gap_back(Gap { prev_token: "middle".to_owned() });
        linked_chunk.chunks.push_items_back([event_2]);
        linked_chunk.chunks.push_gap_back(Gap { prev_token: "end".to_owned() });

        // Remove the first gap.
        let first_gap_id = linked_chunk
            .chunks()
            .find_map(|chunk| chunk.is_gap().then_some(chunk.identifier()))
            .unwrap();

        // The next insert position is the next chunk's start.
        let pos = linked_chunk.replace_gap_at(first_gap_id, vec![]).unwrap();
        assert_eq!(pos, Some(Position::new(ChunkIdentifier::new(2), 0)));

        // Remove the second gap.
        let second_gap_id = linked_chunk
            .chunks()
            .find_map(|chunk| chunk.is_gap().then_some(chunk.identifier()))
            .unwrap();

        // No next insert position.
        let pos = linked_chunk.replace_gap_at(second_gap_id, vec![]).unwrap();
        assert!(pos.is_none());
    }

    #[test]
    fn test_remove_events() {
        let (event_id_0, event_0) = new_event("$ev0");
        let (event_id_1, event_1) = new_event("$ev1");
        let (event_id_2, event_2) = new_event("$ev2");
        let (event_id_3, event_3) = new_event("$ev3");

        // Push some events.
        let mut linked_chunk = EventLinkedChunk::new();
        linked_chunk.chunks.push_items_back([event_0, event_1]);
        linked_chunk.chunks.push_gap_back(Gap { prev_token: "hello".to_owned() });
        linked_chunk.chunks.push_items_back([event_2, event_3]);

        assert_events_eq!(
            linked_chunk.events(),
            [
                (event_id_0 at (0, 0)),
                (event_id_1 at (0, 1)),
                (event_id_2 at (2, 0)),
                (event_id_3 at (2, 1)),
            ]
        );
        assert_eq!(linked_chunk.chunks().count(), 3);

        // Remove some events.
        linked_chunk
            .remove_events_by_position(vec![
                Position::new(ChunkIdentifier::new(2), 1),
                Position::new(ChunkIdentifier::new(0), 1),
            ])
            .unwrap();

        assert_events_eq!(
            linked_chunk.events(),
            [
                (event_id_0 at (0, 0)),
                (event_id_2 at (2, 0)),
            ]
        );

        // Ensure chunks are removed once empty.
        linked_chunk
            .remove_events_by_position(vec![Position::new(ChunkIdentifier::new(2), 0)])
            .unwrap();

        assert_events_eq!(
            linked_chunk.events(),
            [
                (event_id_0 at (0, 0)),
            ]
        );
        assert_eq!(linked_chunk.chunks().count(), 2);
    }

    #[test]
    fn test_remove_events_unknown_event() {
        // Push ZERO event.
        let mut linked_chunk = EventLinkedChunk::new();

        assert_events_eq!(linked_chunk.events(), []);

        // Remove one undefined event.
        // An error is expected.
        linked_chunk
            .remove_events_by_position(vec![Position::new(ChunkIdentifier::new(42), 153)])
            .unwrap_err();

        assert_events_eq!(linked_chunk.events(), []);

        let mut events = linked_chunk.events();
        assert!(events.next().is_none());
    }

    #[test]
    fn test_reset() {
        let (event_id_0, event_0) = new_event("$ev0");
        let (event_id_1, event_1) = new_event("$ev1");
        let (event_id_2, event_2) = new_event("$ev2");
        let (event_id_3, event_3) = new_event("$ev3");

        // Push some events.
        let mut linked_chunk = EventLinkedChunk::new();
        linked_chunk.chunks.push_items_back([event_0, event_1]);
        linked_chunk.chunks.push_gap_back(Gap { prev_token: "raclette".to_owned() });
        linked_chunk.chunks.push_items_back([event_2]);

        // Read the updates as `VectorDiff`.
        let diffs = linked_chunk.updates_as_vector_diffs();

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
        linked_chunk.reset();
        linked_chunk.chunks.push_items_back([event_3]);

        // Read the updates as `VectorDiff`.
        let diffs = linked_chunk.updates_as_vector_diffs();

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

        let mut linked_chunk = EventLinkedChunk::new();
        linked_chunk.chunks.push_items_back(vec![
            event_factory
                .text_msg("hey")
                .event_id(event_id!("$123456789101112131415617181920"))
                .into_event(),
            event_factory.text_msg("you").event_id(event_id!("$2")).into_event(),
        ]);
        linked_chunk.chunks.push_gap_back(Gap { prev_token: "raclette".to_owned() });

        // Flush updates to the order tracker.
        let _ = linked_chunk.updates_as_vector_diffs();

        let output = linked_chunk.debug_string();

        assert_eq!(output.len(), 2);
        assert_eq!(&output[0], "chunk #0: events[#0: $12345678, #1: $2]");
        assert_eq!(&output[1], "chunk #1: gap['raclette']");
    }

    #[test]
    fn test_sort_positions_descending() {
        let mut positions = vec![
            Position::new(ChunkIdentifier::new(2), 1),
            Position::new(ChunkIdentifier::new(1), 0),
            Position::new(ChunkIdentifier::new(2), 0),
            Position::new(ChunkIdentifier::new(1), 1),
            Position::new(ChunkIdentifier::new(0), 0),
        ];

        sort_positions_descending(&mut positions);

        assert_eq!(
            positions,
            &[
                Position::new(ChunkIdentifier::new(2), 1),
                Position::new(ChunkIdentifier::new(2), 0),
                Position::new(ChunkIdentifier::new(1), 1),
                Position::new(ChunkIdentifier::new(1), 0),
                Position::new(ChunkIdentifier::new(0), 0),
            ]
        );
    }
}
