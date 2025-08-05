// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use std::sync::{Arc, RwLock};

use eyeball_im::VectorDiff;

use super::{
    Position,
    updates::{ReaderToken, Update, UpdatesInner},
};
use crate::linked_chunk::{ChunkMetadata, UpdateToVectorDiff};

/// A tracker for the order of items in a linked chunk.
///
/// This can be used to determine the absolute ordering of an item, and thus the
/// relative ordering of two items in a linked chunk, in an
/// efficient manner, thanks to [`OrderTracker::ordering`]. Internally, it
/// keeps track of the relative ordering of the chunks themselves; given a
/// [`Position`] in a linked chunk, the item ordering is the lexicographic
/// ordering of the chunk in the linked chunk, and the internal position within
/// the chunk. For the sake of ease, we return the absolute vector index of the
/// item in the linked chunk.
///
/// It requires the full links' metadata to be provided at creation time, so
/// that it can also give an order for an item that's not loaded yet, in the
/// context of lazy-loading.
#[derive(Debug)]
pub struct OrderTracker<Item, Gap> {
    /// Strong reference to [`UpdatesInner`].
    updates: Arc<RwLock<UpdatesInner<Item, Gap>>>,

    /// The token to read the updates.
    token: ReaderToken,

    /// Mapper from `Update` to `VectorDiff`.
    mapper: UpdateToVectorDiff<Item, NullAccumulator<Item>>,
}

struct NullAccumulator<Item> {
    _phantom: std::marker::PhantomData<Item>,
}

#[cfg(not(tarpaulin_include))]
impl<Item> std::fmt::Debug for NullAccumulator<Item> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NullAccumulator")
    }
}

impl<Item> super::UpdatesAccumulator<Item> for NullAccumulator<Item> {
    fn new(_num_updates_hint: usize) -> Self {
        Self { _phantom: std::marker::PhantomData }
    }
}

impl<Item> Extend<VectorDiff<Item>> for NullAccumulator<Item> {
    fn extend<T: IntoIterator<Item = VectorDiff<Item>>>(&mut self, _iter: T) {
        // This is a no-op, as we don't want to accumulate anything.
    }
}

impl<Item, Gap> OrderTracker<Item, Gap>
where
    Item: Clone,
{
    /// Create a new [`OrderTracker`].
    ///
    /// The `all_chunks_metadata` parameter must include the metadata for *all*
    /// chunks (the full collection, even if the linked chunk is
    /// lazy-loaded).
    ///
    /// They must be ordered by their links in the linked chunk, i.e. the first
    /// chunk in the vector is the first chunk in the linked chunk, the
    /// second in the vector is the first's next chunk, and so on. If that
    /// precondition doesn't hold, then the ordering of items will be undefined.
    pub(super) fn new(
        updates: Arc<RwLock<UpdatesInner<Item, Gap>>>,
        token: ReaderToken,
        all_chunks_metadata: Vec<ChunkMetadata>,
    ) -> Self {
        // Drain previous updates so that this type is synced with `Updates`.
        {
            let mut updates = updates.write().unwrap();
            let _ = updates.take_with_token(token);
        }

        Self { updates, token, mapper: UpdateToVectorDiff::from_metadata(all_chunks_metadata) }
    }

    /// Force flushing of the updates manually.
    ///
    /// If `inhibit` is `true` (which is useful in the case of lazy-loading
    /// related updates, which shouldn't affect the canonical, persisted
    /// linked chunk), the updates are ignored; otherwise, they are consumed
    /// normally.
    pub fn flush_updates(&mut self, inhibit: bool) {
        if inhibit {
            // Ignore the updates.
            let _ = self.updates.write().unwrap().take_with_token(self.token);
        } else {
            // Consume the updates.
            let mut updater = self.updates.write().unwrap();
            let updates = updater.take_with_token(self.token);
            let _ = self.mapper.map(updates);
        }
    }

    /// Apply some out-of-band updates to the ordering tracker.
    ///
    /// This must only be used when the updates do not affect the observed
    /// linked chunk, but would affect the fully-loaded collection.
    pub fn map_updates(&mut self, updates: &[Update<Item, Gap>]) {
        let _ = self.mapper.map(updates);
    }

    /// Given an event's position, returns its final ordering in the current
    /// state of the linked chunk as a vector.
    ///
    /// Useful to compare the ordering of multiple events.
    ///
    /// Precondition: the reader must be up to date, i.e.
    /// [`Self::flush_updates`] must have been called before this method.
    ///
    /// Will return `None` if the position doesn't match a known chunk in the
    /// linked chunk, or if the chunk is a gap.
    pub fn ordering(&self, event_pos: Position) -> Option<usize> {
        // Check the precondition: there must not be any pending updates for this
        // reader.
        debug_assert!(self.updates.read().unwrap().is_reader_up_to_date(self.token));

        // Find the chunk that contained the event.
        let mut ordering = 0;
        for (chunk_id, chunk_length) in &self.mapper.chunks {
            if *chunk_id == event_pos.chunk_identifier() {
                let offset_within_chunk = event_pos.index();
                if offset_within_chunk >= *chunk_length {
                    // The event is out of bounds for this chunk, return None.
                    return None;
                }
                // The final ordering is the number of items before the event, plus its own
                // index within the chunk.
                return Some(ordering + offset_within_chunk);
            }
            // This is not the target chunk yet, so add the size of the current chunk to the
            // number of seen items, and continue.
            ordering += *chunk_length;
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk_test_macros::async_test;

    use crate::linked_chunk::{
        ChunkContent, ChunkIdentifier, ChunkIdentifierGenerator, ChunkMetadata, LinkedChunk,
        OrderTracker, Position, RawChunk, Update, lazy_loader::from_last_chunk,
    };

    #[async_test]
    async fn test_linked_chunk_without_update_history_no_tracking() {
        let mut linked_chunk = LinkedChunk::<10, char, ()>::new();
        assert_matches!(linked_chunk.order_tracker(None), None);
    }

    /// Given a fully-loaded linked chunk, checks that the ordering of an item
    /// is effectively the same as its index in an iteration of items.
    fn assert_order_fully_loaded(
        linked_chunk: &LinkedChunk<3, char, ()>,
        tracker: &OrderTracker<char, ()>,
    ) {
        assert_order(linked_chunk, tracker, 0);
    }

    /// Given a linked chunk with an offset representing the number of items not
    /// loaded yet, checks that the ordering of an item is effectively the
    /// same as its index+offset in an iteration of items.
    fn assert_order(
        linked_chunk: &LinkedChunk<3, char, ()>,
        tracker: &OrderTracker<char, ()>,
        offset: usize,
    ) {
        for (i, (item_pos, _value)) in linked_chunk.items().enumerate() {
            assert_eq!(tracker.ordering(item_pos), Some(i + offset));
        }
    }

    #[async_test]
    async fn test_non_lazy_updates() {
        // Assume the linked chunk is fully loaded, so we have all the chunks at
        // our disposal.
        let mut linked_chunk = LinkedChunk::<3, _, _>::new_with_update_history();

        let mut tracker = linked_chunk.order_tracker(None).unwrap();

        // Let's apply some updates to the live linked chunk.

        // Pushing new items.
        {
            linked_chunk.push_items_back(['a', 'b', 'c']);
            tracker.flush_updates(false);
            assert_order_fully_loaded(&linked_chunk, &tracker);
        }

        // Pushing a gap.
        {
            linked_chunk.push_gap_back(());
            tracker.flush_updates(false);
            assert_order_fully_loaded(&linked_chunk, &tracker);
        }

        // Inserting items in the middle.
        {
            let pos_b = linked_chunk.item_position(|c| *c == 'b').unwrap();
            linked_chunk.insert_items_at(pos_b, ['d', 'e']).unwrap();
            tracker.flush_updates(false);
            assert_order_fully_loaded(&linked_chunk, &tracker);
        }

        // Inserting a gap in the middle.
        {
            let c_pos = linked_chunk.item_position(|c| *c == 'c').unwrap();
            linked_chunk.insert_gap_at((), c_pos).unwrap();
            tracker.flush_updates(false);
            assert_order_fully_loaded(&linked_chunk, &tracker);
        }

        // Replacing a gap with items.
        {
            let last_gap =
                linked_chunk.rchunks().filter(|c| c.is_gap()).last().unwrap().identifier();
            linked_chunk.replace_gap_at(['f', 'g'], last_gap).unwrap();
            tracker.flush_updates(false);
            assert_order_fully_loaded(&linked_chunk, &tracker);
        }

        // Removing an item.
        {
            let a_pos = linked_chunk.item_position(|c| *c == 'd').unwrap();
            linked_chunk.remove_item_at(a_pos).unwrap();
            tracker.flush_updates(false);
            assert_order_fully_loaded(&linked_chunk, &tracker);
        }

        // Replacing an item.
        {
            let b_pos = linked_chunk.item_position(|c| *c == 'e').unwrap();
            linked_chunk.replace_item_at(b_pos, 'E').unwrap();
            tracker.flush_updates(false);
            assert_order_fully_loaded(&linked_chunk, &tracker);
        }

        // Clearing all items.
        {
            linked_chunk.clear();
            tracker.flush_updates(false);
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(0), 0)), None);
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(3), 0)), None);
        }
    }

    #[async_test]
    async fn test_lazy_loading() {
        // Assume that all the chunks haven't been loaded yet, so we have a few of them
        // in some memory, and some of them are still in an hypothetical
        // database.
        let db_metadata = vec![
            // Hypothetical non-empty items chunk with items 'a', 'b', 'c'.
            ChunkMetadata {
                previous: None,
                identifier: ChunkIdentifier(0),
                next: Some(ChunkIdentifier(1)),
                num_items: 3,
            },
            // Hypothetical gap chunk.
            ChunkMetadata {
                previous: Some(ChunkIdentifier(0)),
                identifier: ChunkIdentifier(1),
                next: Some(ChunkIdentifier(2)),
                num_items: 0,
            },
            // Hypothetical non-empty items chunk with items 'd', 'e', 'f'.
            ChunkMetadata {
                previous: Some(ChunkIdentifier(1)),
                identifier: ChunkIdentifier(2),
                next: Some(ChunkIdentifier(3)),
                num_items: 3,
            },
            // Hypothetical non-empty items chunk with items 'g'.
            ChunkMetadata {
                previous: Some(ChunkIdentifier(2)),
                identifier: ChunkIdentifier(3),
                next: None,
                num_items: 1,
            },
        ];

        // The in-memory linked chunk contains the latest chunk only.
        let mut linked_chunk = from_last_chunk::<3, _, ()>(
            Some(RawChunk {
                content: ChunkContent::Items(vec!['g']),
                previous: Some(ChunkIdentifier(2)),
                identifier: ChunkIdentifier(3),
                next: None,
            }),
            ChunkIdentifierGenerator::new_from_previous_chunk_identifier(ChunkIdentifier(3)),
        )
        .expect("could recreate the linked chunk")
        .expect("the linked chunk isn't empty");

        let tracker = linked_chunk.order_tracker(Some(db_metadata)).unwrap();

        // At first, even if the main linked chunk is empty, the order tracker can
        // compute the position for unloaded items.

        // Order of 'a':
        assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(0), 0)), Some(0));
        // Order of 'b':
        assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(0), 1)), Some(1));
        // Order of 'c':
        assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(0), 2)), Some(2));
        // An invalid position in a known chunk returns no ordering.
        assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(0), 42)), None);

        // A gap chunk doesn't have an ordering.
        assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(1), 0)), None);
        assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(1), 42)), None);

        // Order of 'd':
        assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(2), 0)), Some(3));
        // Order of 'e':
        assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(2), 1)), Some(4));
        // Order of 'f':
        assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(2), 2)), Some(5));
        // No subsequent entry in the same chunk, it's been split when inserting g.
        assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(2), 3)), None);

        // Order of 'g':
        assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(3), 0)), Some(6));
        // This was the final entry so far.
        assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(3), 1)), None);
    }

    #[async_test]
    async fn test_lazy_updates() {
        // Assume that all the chunks haven't been loaded yet, so we have a few of them
        // in some memory, and some of them are still in an hypothetical
        // database.
        let db_metadata = vec![
            // Hypothetical non-empty items chunk with items 'a', 'b'.
            ChunkMetadata {
                previous: None,
                identifier: ChunkIdentifier(0),
                next: Some(ChunkIdentifier(1)),
                num_items: 2,
            },
            // Hypothetical gap chunk.
            ChunkMetadata {
                previous: Some(ChunkIdentifier(0)),
                identifier: ChunkIdentifier(1),
                next: Some(ChunkIdentifier(2)),
                num_items: 0,
            },
            // Hypothetical non-empty items chunk with items 'd', 'e', 'f'.
            ChunkMetadata {
                previous: Some(ChunkIdentifier(1)),
                identifier: ChunkIdentifier(2),
                next: Some(ChunkIdentifier(3)),
                num_items: 3,
            },
            // Hypothetical non-empty items chunk with items 'g'.
            ChunkMetadata {
                previous: Some(ChunkIdentifier(2)),
                identifier: ChunkIdentifier(3),
                next: None,
                num_items: 1,
            },
        ];

        // The in-memory linked chunk contains the latest chunk only.
        let mut linked_chunk = from_last_chunk(
            Some(RawChunk {
                content: ChunkContent::Items(vec!['g']),
                previous: Some(ChunkIdentifier(2)),
                identifier: ChunkIdentifier(3),
                next: None,
            }),
            ChunkIdentifierGenerator::new_from_previous_chunk_identifier(ChunkIdentifier(3)),
        )
        .expect("could recreate the linked chunk")
        .expect("the linked chunk isn't empty");

        let mut tracker = linked_chunk.order_tracker(Some(db_metadata)).unwrap();

        // Sanity checks on the initial state.
        {
            // Order of 'b':
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(0), 1)), Some(1));
            // Order of 'g':
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(3), 0)), Some(5));
        }

        // Let's apply some updates to the live linked chunk.

        // Pushing new items.
        {
            linked_chunk.push_items_back(['h', 'i']);
            tracker.flush_updates(false);

            // Order of items not loaded:
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(0), 1)), Some(1));
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(3), 0)), Some(5));
            // The loaded items are off by 5 (the absolute order of g).
            assert_order(&linked_chunk, &tracker, 5);
        }

        // Pushing a gap.
        let gap_id = {
            linked_chunk.push_gap_back(());
            tracker.flush_updates(false);

            // The gap doesn't have an ordering.
            let last_chunk = linked_chunk.rchunks().next().unwrap();
            assert!(last_chunk.is_gap());
            assert_eq!(tracker.ordering(Position::new(last_chunk.identifier(), 0)), None);
            assert_eq!(tracker.ordering(Position::new(last_chunk.identifier(), 42)), None);

            // The previous items are still ordered.
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(0), 1)), Some(1));
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(3), 0)), Some(5));
            // The loaded items are off by 5 (the absolute order of g).
            assert_order(&linked_chunk, &tracker, 5);

            last_chunk.identifier()
        };

        // Inserting items in the middle.
        {
            let pos_h = linked_chunk.item_position(|c| *c == 'h').unwrap();
            linked_chunk.insert_items_at(pos_h, ['j', 'k']).unwrap();
            tracker.flush_updates(false);

            // The previous items are still ordered.
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(0), 1)), Some(1));
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(3), 0)), Some(5));
            // The loaded items are off by 5 (the absolute order of g).
            assert_order(&linked_chunk, &tracker, 5);
        }

        // Replacing a gap with items.
        {
            linked_chunk.replace_gap_at(['l', 'm'], gap_id).unwrap();
            tracker.flush_updates(false);

            // The previous items are still ordered.
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(0), 1)), Some(1));
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(3), 0)), Some(5));
            // The loaded items are off by 5 (the absolute order of g).
            assert_order(&linked_chunk, &tracker, 5);
        }

        // Removing an item.
        {
            let j_pos = linked_chunk.item_position(|c| *c == 'j').unwrap();
            linked_chunk.remove_item_at(j_pos).unwrap();
            tracker.flush_updates(false);

            // The previous items are still ordered.
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(0), 1)), Some(1));
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(3), 0)), Some(5));
            // The loaded items are off by 5 (the absolute order of g).
            assert_order(&linked_chunk, &tracker, 5);
        }

        // Replacing an item.
        {
            let k_pos = linked_chunk.item_position(|c| *c == 'k').unwrap();
            linked_chunk.replace_item_at(k_pos, 'K').unwrap();
            tracker.flush_updates(false);

            // The previous items are still ordered.
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(0), 1)), Some(1));
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(3), 0)), Some(5));
            // The loaded items are off by 5 (the absolute order of g).
            assert_order(&linked_chunk, &tracker, 5);
        }

        // Clearing all items.
        {
            linked_chunk.clear();
            tracker.flush_updates(false);
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(0), 0)), None);
            assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(3), 0)), None);
        }
    }

    #[async_test]
    async fn test_out_of_band_updates() {
        // Assume that all the chunks haven't been loaded yet, so we have a few of them
        // in some memory, and some of them are still in an hypothetical
        // database.
        let db_metadata = vec![
            // Hypothetical non-empty items chunk with items 'a', 'b'.
            ChunkMetadata {
                previous: None,
                identifier: ChunkIdentifier(0),
                next: Some(ChunkIdentifier(1)),
                num_items: 2,
            },
            // Hypothetical gap chunk.
            ChunkMetadata {
                previous: Some(ChunkIdentifier(0)),
                identifier: ChunkIdentifier(1),
                next: Some(ChunkIdentifier(2)),
                num_items: 0,
            },
            // Hypothetical non-empty items chunk with items 'd', 'e', 'f'.
            ChunkMetadata {
                previous: Some(ChunkIdentifier(1)),
                identifier: ChunkIdentifier(2),
                next: Some(ChunkIdentifier(3)),
                num_items: 3,
            },
            // Hypothetical non-empty items chunk with items 'g'.
            ChunkMetadata {
                previous: Some(ChunkIdentifier(2)),
                identifier: ChunkIdentifier(3),
                next: None,
                num_items: 1,
            },
        ];

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        let mut tracker = linked_chunk.order_tracker(Some(db_metadata)).unwrap();

        // Sanity checks.
        // Order of 'b':
        assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(0), 1)), Some(1));
        // Order of 'e':
        assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(2), 1)), Some(3));

        // It's possible to apply updates out of band, i.e. without affecting the
        // observed linked chunk. This can be useful when an update only applies
        // to a database, but not to the in-memory linked chunk.
        tracker.map_updates(&[Update::RemoveChunk(ChunkIdentifier::new(0))]);

        // 'b' doesn't exist anymore, so its ordering is now undefined.
        assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(0), 1)), None);
        // 'e' has been shifted back by 2 places, aka the number of items in the first
        // chunk.
        assert_eq!(tracker.ordering(Position::new(ChunkIdentifier::new(2), 1)), Some(1));
    }
}
