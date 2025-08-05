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

//! Implementation for a _relational linked chunk_, see
//! [`RelationalLinkedChunk`].

use std::{
    collections::{BTreeMap, HashMap},
    hash::Hash,
};

use ruma::{OwnedEventId, OwnedRoomId, RoomId};

use super::{ChunkContent, ChunkIdentifierGenerator, RawChunk};
use crate::{
    deserialized_responses::TimelineEvent,
    linked_chunk::{
        ChunkIdentifier, ChunkMetadata, LinkedChunkId, OwnedLinkedChunkId, Position, Update,
    },
};

/// A row of the [`RelationalLinkedChunk::chunks`].
#[derive(Debug, PartialEq)]
struct ChunkRow {
    linked_chunk_id: OwnedLinkedChunkId,
    previous_chunk: Option<ChunkIdentifier>,
    chunk: ChunkIdentifier,
    next_chunk: Option<ChunkIdentifier>,
}

/// A row of the [`RelationalLinkedChunk::items`].
#[derive(Debug, PartialEq)]
struct ItemRow<ItemId, Gap> {
    linked_chunk_id: OwnedLinkedChunkId,
    position: Position,
    item: Either<ItemId, Gap>,
}

/// Kind of item.
#[derive(Debug, PartialEq)]
enum Either<Item, Gap> {
    /// The content is an item.
    Item(Item),

    /// The content is a gap.
    Gap(Gap),
}

/// A [`LinkedChunk`] but with a relational layout, similar to what we
/// would have in a database.
///
/// This is used by memory stores. The idea is to have a data layout that is
/// similar for memory stores and for relational database stores, to represent a
/// [`LinkedChunk`].
///
/// This type is also designed to receive [`Update`]. Applying `Update`s
/// directly on a [`LinkedChunk`] is not ideal and particularly not trivial as
/// the `Update`s do _not_ match the internal data layout of the `LinkedChunk`,
/// they have been designed for storages, like a relational database for
/// example.
///
/// This type is not as performant as [`LinkedChunk`] (in terms of memory
/// layout, CPU caches etc.). It is only designed to be used in memory stores,
/// which are mostly used for test purposes or light usage of the SDK.
///
/// [`LinkedChunk`]: super::LinkedChunk
#[derive(Debug)]
pub struct RelationalLinkedChunk<ItemId, Item, Gap> {
    /// Chunks.
    chunks: Vec<ChunkRow>,

    /// Items chunks.
    items_chunks: Vec<ItemRow<ItemId, Gap>>,

    /// The items' content themselves.
    items: HashMap<OwnedLinkedChunkId, BTreeMap<ItemId, (Item, Option<Position>)>>,
}

/// The [`IndexableItem`] trait is used to mark items that can be indexed into a
/// [`RelationalLinkedChunk`].
pub trait IndexableItem {
    type ItemId: Hash + PartialEq + Eq + Clone;

    /// Return the identifier of the item.
    fn id(&self) -> Self::ItemId;
}

impl IndexableItem for TimelineEvent {
    type ItemId = OwnedEventId;

    fn id(&self) -> Self::ItemId {
        self.event_id()
            .expect("all events saved into a relational linked chunk must have a valid event id")
    }
}

impl<ItemId, Item, Gap> RelationalLinkedChunk<ItemId, Item, Gap>
where
    Item: IndexableItem<ItemId = ItemId>,
    ItemId: Hash + PartialEq + Eq + Clone + Ord,
{
    /// Create a new relational linked chunk.
    pub fn new() -> Self {
        Self { chunks: Vec::new(), items_chunks: Vec::new(), items: HashMap::new() }
    }

    /// Removes all the chunks and items from this relational linked chunk.
    pub fn clear(&mut self) {
        self.chunks.clear();
        self.items_chunks.clear();
        self.items.clear();
    }

    /// Apply [`Update`]s. That's the only way to write data inside this
    /// relational linked chunk.
    pub fn apply_updates(
        &mut self,
        linked_chunk_id: LinkedChunkId<'_>,
        updates: Vec<Update<Item, Gap>>,
    ) {
        for update in updates {
            match update {
                Update::NewItemsChunk { previous, new, next } => {
                    Self::insert_chunk(&mut self.chunks, linked_chunk_id, previous, new, next);
                }

                Update::NewGapChunk { previous, new, next, gap } => {
                    Self::insert_chunk(&mut self.chunks, linked_chunk_id, previous, new, next);
                    self.items_chunks.push(ItemRow {
                        linked_chunk_id: linked_chunk_id.to_owned(),
                        position: Position::new(new, 0),
                        item: Either::Gap(gap),
                    });
                }

                Update::RemoveChunk(chunk_identifier) => {
                    Self::remove_chunk(&mut self.chunks, linked_chunk_id, chunk_identifier);

                    let indices_to_remove = self
                        .items_chunks
                        .iter()
                        .enumerate()
                        .filter_map(
                            |(
                                nth,
                                ItemRow {
                                    linked_chunk_id: linked_chunk_id_candidate,
                                    position,
                                    ..
                                },
                            )| {
                                (linked_chunk_id == linked_chunk_id_candidate
                                    && position.chunk_identifier() == chunk_identifier)
                                    .then_some(nth)
                            },
                        )
                        .collect::<Vec<_>>();

                    for index_to_remove in indices_to_remove.into_iter().rev() {
                        self.items_chunks.remove(index_to_remove);
                    }
                }

                Update::PushItems { mut at, items } => {
                    for item in items {
                        let item_id = item.id();
                        self.items
                            .entry(linked_chunk_id.to_owned())
                            .or_default()
                            .insert(item_id.clone(), (item, Some(at)));
                        self.items_chunks.push(ItemRow {
                            linked_chunk_id: linked_chunk_id.to_owned(),
                            position: at,
                            item: Either::Item(item_id),
                        });
                        at.increment_index();
                    }
                }

                Update::ReplaceItem { at, item } => {
                    let existing = self
                        .items_chunks
                        .iter_mut()
                        .find(|item| item.position == at)
                        .expect("trying to replace at an unknown position");
                    assert!(
                        matches!(existing.item, Either::Item(..)),
                        "trying to replace a gap with an item"
                    );
                    let item_id = item.id();
                    self.items
                        .entry(linked_chunk_id.to_owned())
                        .or_default()
                        .insert(item_id.clone(), (item, Some(at)));
                    existing.item = Either::Item(item_id);
                }

                Update::RemoveItem { at } => {
                    let mut entry_to_remove = None;

                    for (
                        nth,
                        ItemRow { linked_chunk_id: linked_chunk_id_candidate, position, .. },
                    ) in self.items_chunks.iter_mut().enumerate()
                    {
                        // Filter by linked chunk id.
                        if linked_chunk_id != &*linked_chunk_id_candidate {
                            continue;
                        }

                        // Find the item to remove.
                        if *position == at {
                            debug_assert!(entry_to_remove.is_none(), "Found the same entry twice");

                            entry_to_remove = Some(nth);
                        }

                        // Update all items that come _after_ `at` to shift their index.
                        if position.chunk_identifier() == at.chunk_identifier()
                            && position.index() > at.index()
                        {
                            position.decrement_index();
                        }
                    }

                    self.items_chunks.remove(entry_to_remove.expect("Remove an unknown item"));
                    // We deliberately keep the item in the items collection.
                }

                Update::DetachLastItems { at } => {
                    let indices_to_remove = self
                        .items_chunks
                        .iter()
                        .enumerate()
                        .filter_map(
                            |(
                                nth,
                                ItemRow {
                                    linked_chunk_id: linked_chunk_id_candidate,
                                    position,
                                    ..
                                },
                            )| {
                                (linked_chunk_id == linked_chunk_id_candidate
                                    && position.chunk_identifier() == at.chunk_identifier()
                                    && position.index() >= at.index())
                                .then_some(nth)
                            },
                        )
                        .collect::<Vec<_>>();

                    for index_to_remove in indices_to_remove.into_iter().rev() {
                        self.items_chunks.remove(index_to_remove);
                    }
                }

                Update::StartReattachItems | Update::EndReattachItems => { /* nothing */ }

                Update::Clear => {
                    self.chunks.retain(|chunk| chunk.linked_chunk_id != linked_chunk_id);
                    self.items_chunks.retain(|chunk| chunk.linked_chunk_id != linked_chunk_id);
                    // We deliberately leave the items intact.
                }
            }
        }
    }

    fn insert_chunk(
        chunks: &mut Vec<ChunkRow>,
        linked_chunk_id: LinkedChunkId<'_>,
        previous: Option<ChunkIdentifier>,
        new: ChunkIdentifier,
        next: Option<ChunkIdentifier>,
    ) {
        // Find the previous chunk, and update its next chunk.
        if let Some(previous) = previous {
            let entry_for_previous_chunk = chunks
                .iter_mut()
                .find(|ChunkRow { linked_chunk_id: linked_chunk_id_candidate, chunk, .. }| {
                    linked_chunk_id == linked_chunk_id_candidate && *chunk == previous
                })
                .expect("Previous chunk should be present");

            // Link the chunk.
            entry_for_previous_chunk.next_chunk = Some(new);
        }

        // Find the next chunk, and update its previous chunk.
        if let Some(next) = next {
            let entry_for_next_chunk = chunks
                .iter_mut()
                .find(|ChunkRow { linked_chunk_id: linked_chunk_id_candidate, chunk, .. }| {
                    linked_chunk_id == linked_chunk_id_candidate && *chunk == next
                })
                .expect("Next chunk should be present");

            // Link the chunk.
            entry_for_next_chunk.previous_chunk = Some(new);
        }

        // Insert the chunk.
        chunks.push(ChunkRow {
            linked_chunk_id: linked_chunk_id.to_owned(),
            previous_chunk: previous,
            chunk: new,
            next_chunk: next,
        });
    }

    fn remove_chunk(
        chunks: &mut Vec<ChunkRow>,
        linked_chunk_id: LinkedChunkId<'_>,
        chunk_to_remove: ChunkIdentifier,
    ) {
        let entry_nth_to_remove = chunks
            .iter()
            .enumerate()
            .find_map(
                |(nth, ChunkRow { linked_chunk_id: linked_chunk_id_candidate, chunk, .. })| {
                    (linked_chunk_id == linked_chunk_id_candidate && *chunk == chunk_to_remove)
                        .then_some(nth)
                },
            )
            .expect("Remove an unknown chunk");

        let ChunkRow { linked_chunk_id, previous_chunk: previous, next_chunk: next, .. } =
            chunks.remove(entry_nth_to_remove);

        // Find the previous chunk, and update its next chunk.
        if let Some(previous) = previous {
            let entry_for_previous_chunk = chunks
                .iter_mut()
                .find(|ChunkRow { linked_chunk_id: linked_chunk_id_candidate, chunk, .. }| {
                    &linked_chunk_id == linked_chunk_id_candidate && *chunk == previous
                })
                .expect("Previous chunk should be present");

            // Insert the chunk.
            entry_for_previous_chunk.next_chunk = next;
        }

        // Find the next chunk, and update its previous chunk.
        if let Some(next) = next {
            let entry_for_next_chunk = chunks
                .iter_mut()
                .find(|ChunkRow { linked_chunk_id: linked_chunk_id_candidate, chunk, .. }| {
                    &linked_chunk_id == linked_chunk_id_candidate && *chunk == next
                })
                .expect("Next chunk should be present");

            // Insert the chunk.
            entry_for_next_chunk.previous_chunk = previous;
        }
    }

    /// Return an iterator that yields items of a particular linked chunk, in no
    /// particular order.
    pub fn unordered_linked_chunk_items<'a>(
        &'a self,
        target: &OwnedLinkedChunkId,
    ) -> impl Iterator<Item = (&'a Item, Position)> + use<'a, ItemId, Item, Gap> {
        self.items.get(target).into_iter().flat_map(|items| {
            // Only keep items which have a position.
            items.values().filter_map(|(item, pos)| pos.map(|pos| (item, pos)))
        })
    }

    /// Return an iterator over all items of all linked chunks of a room, along
    /// with their positions, if available.
    ///
    /// The only items which will NOT have a position are those saved with
    /// [`Self::save_item`].
    ///
    /// This will include out-of-band items.
    pub fn items<'a>(
        &'a self,
        room_id: &'a RoomId,
    ) -> impl Iterator<Item = (&'a Item, Option<Position>)> {
        self.items
            .iter()
            .filter_map(move |(linked_chunk_id, items)| {
                (linked_chunk_id.room_id() == room_id).then_some(items)
            })
            .flat_map(|items| items.values().map(|(item, pos)| (item, *pos)))
    }

    /// Save a single item "out-of-band" in the relational linked chunk.
    pub fn save_item(&mut self, room_id: OwnedRoomId, item: Item) {
        let id = item.id();
        let linked_chunk_id = OwnedLinkedChunkId::Room(room_id);

        let map = self.items.entry(linked_chunk_id).or_default();
        if let Some(prev_value) = map.get_mut(&id) {
            // If the item already exists, we keep the position.
            prev_value.0 = item;
        } else {
            map.insert(id, (item, None));
        }
    }
}

impl<ItemId, Item, Gap> RelationalLinkedChunk<ItemId, Item, Gap>
where
    Gap: Clone,
    Item: Clone,
    ItemId: Hash + PartialEq + Eq + Ord,
{
    /// Loads all the chunks.
    ///
    /// Return an error result if the data was malformed in the struct, with a
    /// string message explaining details about the error.
    #[doc(hidden)]
    pub fn load_all_chunks(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<RawChunk<Item, Gap>>, String> {
        self.chunks
            .iter()
            .filter(|chunk| chunk.linked_chunk_id == linked_chunk_id)
            .map(|chunk_row| load_raw_chunk(self, chunk_row, linked_chunk_id))
            .collect::<Result<Vec<_>, String>>()
    }

    /// Loads all the chunks' metadata.
    ///
    /// Return an error result if the data was malformed in the struct, with a
    /// string message explaining details about the error.
    #[doc(hidden)]
    pub fn load_all_chunks_metadata(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<ChunkMetadata>, String> {
        self.chunks
            .iter()
            .filter(|chunk| chunk.linked_chunk_id == linked_chunk_id)
            .map(|chunk_row| load_raw_chunk_metadata(self, chunk_row, linked_chunk_id))
            .collect::<Result<Vec<_>, String>>()
    }

    pub fn load_last_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<(Option<RawChunk<Item, Gap>>, ChunkIdentifierGenerator), String> {
        // Find the latest chunk identifier to generate a `ChunkIdentifierGenerator`.
        let chunk_identifier_generator = match self
            .chunks
            .iter()
            .filter_map(|chunk_row| {
                (chunk_row.linked_chunk_id == linked_chunk_id).then_some(chunk_row.chunk)
            })
            .max()
        {
            Some(last_chunk_identifier) => {
                ChunkIdentifierGenerator::new_from_previous_chunk_identifier(last_chunk_identifier)
            }
            None => ChunkIdentifierGenerator::new_from_scratch(),
        };

        // Find the last chunk.
        let mut number_of_chunks = 0;
        let mut chunk_row = None;

        for chunk_row_candidate in &self.chunks {
            if chunk_row_candidate.linked_chunk_id == linked_chunk_id {
                number_of_chunks += 1;

                if chunk_row_candidate.next_chunk.is_none() {
                    chunk_row = Some(chunk_row_candidate);

                    break;
                }
            }
        }

        let chunk_row = match chunk_row {
            // Chunk has been found, all good.
            Some(chunk_row) => chunk_row,

            // Chunk is not found and there is zero chunk for this room, this is consistent, all
            // good.
            None if number_of_chunks == 0 => {
                return Ok((None, chunk_identifier_generator));
            }

            // Chunk is not found **but** there are chunks for this room, this is inconsistent. The
            // linked chunk is malformed.
            //
            // Returning `Ok(None)` would be invalid here: we must return an error.
            None => {
                return Err(
                    "last chunk is not found but chunks exist: the linked chunk contains a cycle"
                        .to_owned(),
                );
            }
        };

        // Build the chunk.
        load_raw_chunk(self, chunk_row, linked_chunk_id)
            .map(|raw_chunk| (Some(raw_chunk), chunk_identifier_generator))
    }

    pub fn load_previous_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        before_chunk_identifier: ChunkIdentifier,
    ) -> Result<Option<RawChunk<Item, Gap>>, String> {
        // Find the chunk before the chunk identified by `before_chunk_identifier`.
        let Some(chunk_row) = self.chunks.iter().find(|chunk_row| {
            chunk_row.linked_chunk_id == linked_chunk_id
                && chunk_row.next_chunk == Some(before_chunk_identifier)
        }) else {
            // Chunk is not found.
            return Ok(None);
        };

        // Build the chunk.
        load_raw_chunk(self, chunk_row, linked_chunk_id).map(Some)
    }
}

impl<ItemId, Item, Gap> Default for RelationalLinkedChunk<ItemId, Item, Gap>
where
    Item: IndexableItem<ItemId = ItemId>,
    ItemId: Hash + PartialEq + Eq + Clone + Ord,
{
    fn default() -> Self {
        Self::new()
    }
}

/// Loads a single chunk along all its items.
///
/// The code of this method must be kept in sync with that of
/// [`load_raw_chunk_metadata`] below.
fn load_raw_chunk<ItemId, Item, Gap>(
    relational_linked_chunk: &RelationalLinkedChunk<ItemId, Item, Gap>,
    chunk_row: &ChunkRow,
    linked_chunk_id: LinkedChunkId<'_>,
) -> Result<RawChunk<Item, Gap>, String>
where
    Item: Clone,
    Gap: Clone,
    ItemId: Hash + PartialEq + Eq + Ord,
{
    // Find all items that correspond to the chunk.
    let mut items = relational_linked_chunk
        .items_chunks
        .iter()
        .filter(|item_row| {
            item_row.linked_chunk_id == linked_chunk_id
                && item_row.position.chunk_identifier() == chunk_row.chunk
        })
        .peekable();

    let Some(first_item) = items.peek() else {
        // No item. It means it is a chunk of kind `Items` and that it is empty!
        return Ok(RawChunk {
            content: ChunkContent::Items(Vec::new()),
            previous: chunk_row.previous_chunk,
            identifier: chunk_row.chunk,
            next: chunk_row.next_chunk,
        });
    };

    Ok(match first_item.item {
        // This is a chunk of kind `Items`.
        Either::Item(_) => {
            // Collect all the items.
            let mut collected_items = Vec::new();

            for item_row in items {
                match &item_row.item {
                    Either::Item(item_id) => {
                        collected_items.push((item_id, item_row.position.index()))
                    }

                    Either::Gap(_) => {
                        return Err(format!(
                            "unexpected gap in items chunk {}",
                            chunk_row.chunk.index()
                        ));
                    }
                }
            }

            // Sort them by their position.
            collected_items.sort_unstable_by_key(|(_item, index)| *index);

            RawChunk {
                content: ChunkContent::Items(
                    collected_items
                        .into_iter()
                        .filter_map(|(item_id, _index)| {
                            Some(
                                relational_linked_chunk
                                    .items
                                    .get(&linked_chunk_id.to_owned())?
                                    .get(item_id)?
                                    .0
                                    .clone(),
                            )
                        })
                        .collect(),
                ),
                previous: chunk_row.previous_chunk,
                identifier: chunk_row.chunk,
                next: chunk_row.next_chunk,
            }
        }

        Either::Gap(ref gap) => {
            assert!(items.next().is_some(), "we just peeked the gap");

            // We shouldn't have more than one item row for this chunk.
            if items.next().is_some() {
                return Err(format!(
                    "there shouldn't be more than one item row attached in gap chunk {}",
                    chunk_row.chunk.index()
                ));
            }

            RawChunk {
                content: ChunkContent::Gap(gap.clone()),
                previous: chunk_row.previous_chunk,
                identifier: chunk_row.chunk,
                next: chunk_row.next_chunk,
            }
        }
    })
}

/// Loads the metadata for a single chunk.
///
/// The code of this method must be kept in sync with that of [`load_raw_chunk`]
/// above.
fn load_raw_chunk_metadata<ItemId, Item, Gap>(
    relational_linked_chunk: &RelationalLinkedChunk<ItemId, Item, Gap>,
    chunk_row: &ChunkRow,
    linked_chunk_id: LinkedChunkId<'_>,
) -> Result<ChunkMetadata, String>
where
    Item: Clone,
    Gap: Clone,
    ItemId: Hash + PartialEq + Eq,
{
    // Find all items that correspond to the chunk.
    let mut items = relational_linked_chunk
        .items_chunks
        .iter()
        .filter(|item_row| {
            item_row.linked_chunk_id == linked_chunk_id
                && item_row.position.chunk_identifier() == chunk_row.chunk
        })
        .peekable();

    let Some(first_item) = items.peek() else {
        // No item. It means it is a chunk of kind `Items` and that it is empty!
        return Ok(ChunkMetadata {
            num_items: 0,
            previous: chunk_row.previous_chunk,
            identifier: chunk_row.chunk,
            next: chunk_row.next_chunk,
        });
    };

    Ok(match first_item.item {
        // This is a chunk of kind `Items`.
        Either::Item(_) => {
            // Count all the items. We add an additional filter that will exclude gaps, in
            // case the chunk is malformed, but we should not have to, in theory.

            let mut num_items = 0;
            for item in items {
                match &item.item {
                    Either::Item(_) => num_items += 1,
                    Either::Gap(_) => {
                        return Err(format!(
                            "unexpected gap in items chunk {}",
                            chunk_row.chunk.index()
                        ));
                    }
                }
            }

            ChunkMetadata {
                num_items,
                previous: chunk_row.previous_chunk,
                identifier: chunk_row.chunk,
                next: chunk_row.next_chunk,
            }
        }

        Either::Gap(..) => {
            assert!(items.next().is_some(), "we just peeked the gap");

            // We shouldn't have more than one item row for this chunk.
            if items.next().is_some() {
                return Err(format!(
                    "there shouldn't be more than one item row attached in gap chunk {}",
                    chunk_row.chunk.index()
                ));
            }

            ChunkMetadata {
                // By convention, a gap has 0 items.
                num_items: 0,
                previous: chunk_row.previous_chunk,
                identifier: chunk_row.chunk,
                next: chunk_row.next_chunk,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use assert_matches::assert_matches;
    use ruma::room_id;

    use super::{super::lazy_loader::from_all_chunks, ChunkIdentifier as CId, *};

    impl IndexableItem for char {
        type ItemId = char;

        fn id(&self) -> Self::ItemId {
            *self
        }
    }

    #[test]
    fn test_new_items_chunk() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = OwnedLinkedChunkId::Room(room_id.to_owned());

        let mut relational_linked_chunk = RelationalLinkedChunk::<_, char, ()>::new();

        relational_linked_chunk.apply_updates(
            linked_chunk_id.as_ref(),
            vec![
                // 0
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // 1 after 0
                Update::NewItemsChunk { previous: Some(CId::new(0)), new: CId::new(1), next: None },
                // 2 before 0
                Update::NewItemsChunk { previous: None, new: CId::new(2), next: Some(CId::new(0)) },
                // 3 between 2 and 0
                Update::NewItemsChunk {
                    previous: Some(CId::new(2)),
                    new: CId::new(3),
                    next: Some(CId::new(0)),
                },
            ],
        );

        // Chunks are correctly linked.
        assert_eq!(
            relational_linked_chunk.chunks,
            &[
                ChunkRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    previous_chunk: Some(CId::new(3)),
                    chunk: CId::new(0),
                    next_chunk: Some(CId::new(1))
                },
                ChunkRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    previous_chunk: Some(CId::new(0)),
                    chunk: CId::new(1),
                    next_chunk: None
                },
                ChunkRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    previous_chunk: None,
                    chunk: CId::new(2),
                    next_chunk: Some(CId::new(3))
                },
                ChunkRow {
                    linked_chunk_id,
                    previous_chunk: Some(CId::new(2)),
                    chunk: CId::new(3),
                    next_chunk: Some(CId::new(0))
                },
            ],
        );

        // Items have not been modified.
        assert!(relational_linked_chunk.items_chunks.is_empty());
    }

    #[test]
    fn test_new_gap_chunk() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = OwnedLinkedChunkId::Room(room_id.to_owned());

        let mut relational_linked_chunk = RelationalLinkedChunk::<_, char, ()>::new();

        relational_linked_chunk.apply_updates(
            linked_chunk_id.as_ref(),
            vec![
                // 0
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // 1 after 0
                Update::NewGapChunk {
                    previous: Some(CId::new(0)),
                    new: CId::new(1),
                    next: None,
                    gap: (),
                },
                // 2 after 1
                Update::NewItemsChunk { previous: Some(CId::new(1)), new: CId::new(2), next: None },
            ],
        );

        // Chunks are correctly linked.
        assert_eq!(
            relational_linked_chunk.chunks,
            &[
                ChunkRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    previous_chunk: None,
                    chunk: CId::new(0),
                    next_chunk: Some(CId::new(1))
                },
                ChunkRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    previous_chunk: Some(CId::new(0)),
                    chunk: CId::new(1),
                    next_chunk: Some(CId::new(2))
                },
                ChunkRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    previous_chunk: Some(CId::new(1)),
                    chunk: CId::new(2),
                    next_chunk: None
                },
            ],
        );
        // Items contains the gap.
        assert_eq!(
            relational_linked_chunk.items_chunks,
            &[ItemRow {
                linked_chunk_id,
                position: Position::new(CId::new(1), 0),
                item: Either::Gap(())
            }],
        );
    }

    #[test]
    fn test_remove_chunk() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = OwnedLinkedChunkId::Room(room_id.to_owned());

        let mut relational_linked_chunk = RelationalLinkedChunk::<_, char, ()>::new();

        relational_linked_chunk.apply_updates(
            linked_chunk_id.as_ref(),
            vec![
                // 0
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // 1 after 0
                Update::NewGapChunk {
                    previous: Some(CId::new(0)),
                    new: CId::new(1),
                    next: None,
                    gap: (),
                },
                // 2 after 1
                Update::NewItemsChunk { previous: Some(CId::new(1)), new: CId::new(2), next: None },
                // remove 1
                Update::RemoveChunk(CId::new(1)),
            ],
        );

        // Chunks are correctly linked.
        assert_eq!(
            relational_linked_chunk.chunks,
            &[
                ChunkRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    previous_chunk: None,
                    chunk: CId::new(0),
                    next_chunk: Some(CId::new(2))
                },
                ChunkRow {
                    linked_chunk_id,
                    previous_chunk: Some(CId::new(0)),
                    chunk: CId::new(2),
                    next_chunk: None
                },
            ],
        );

        // Items no longer contains the gap.
        assert!(relational_linked_chunk.items_chunks.is_empty());
    }

    #[test]
    fn test_push_items() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = OwnedLinkedChunkId::Room(room_id.to_owned());

        let mut relational_linked_chunk = RelationalLinkedChunk::<_, char, ()>::new();

        relational_linked_chunk.apply_updates(
            linked_chunk_id.as_ref(),
            vec![
                // new chunk (this is not mandatory for this test, but let's try to be realistic)
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // new items on 0
                Update::PushItems { at: Position::new(CId::new(0), 0), items: vec!['a', 'b', 'c'] },
                // new chunk (to test new items are pushed in the correct chunk)
                Update::NewItemsChunk { previous: Some(CId::new(0)), new: CId::new(1), next: None },
                // new items on 1
                Update::PushItems { at: Position::new(CId::new(1), 0), items: vec!['x', 'y', 'z'] },
                // new items on 0 again
                Update::PushItems { at: Position::new(CId::new(0), 3), items: vec!['d', 'e'] },
            ],
        );

        // Chunks are correctly linked.
        assert_eq!(
            relational_linked_chunk.chunks,
            &[
                ChunkRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    previous_chunk: None,
                    chunk: CId::new(0),
                    next_chunk: Some(CId::new(1))
                },
                ChunkRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    previous_chunk: Some(CId::new(0)),
                    chunk: CId::new(1),
                    next_chunk: None
                },
            ],
        );
        // Items contains the pushed items.
        assert_eq!(
            relational_linked_chunk.items_chunks,
            &[
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(0), 0),
                    item: Either::Item('a')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(0), 1),
                    item: Either::Item('b')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(0), 2),
                    item: Either::Item('c')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(1), 0),
                    item: Either::Item('x')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(1), 1),
                    item: Either::Item('y')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(1), 2),
                    item: Either::Item('z')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(0), 3),
                    item: Either::Item('d')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(0), 4),
                    item: Either::Item('e')
                },
            ],
        );
    }

    #[test]
    fn test_remove_item() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = OwnedLinkedChunkId::Room(room_id.to_owned());

        let mut relational_linked_chunk = RelationalLinkedChunk::<_, char, ()>::new();

        relational_linked_chunk.apply_updates(
            linked_chunk_id.as_ref(),
            vec![
                // new chunk (this is not mandatory for this test, but let's try to be realistic)
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // new items on 0
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec!['a', 'b', 'c', 'd', 'e'],
                },
                // remove an item: 'a'
                Update::RemoveItem { at: Position::new(CId::new(0), 0) },
                // remove an item: 'd'
                Update::RemoveItem { at: Position::new(CId::new(0), 2) },
            ],
        );

        // Chunks are correctly linked.
        assert_eq!(
            relational_linked_chunk.chunks,
            &[ChunkRow {
                linked_chunk_id: linked_chunk_id.clone(),
                previous_chunk: None,
                chunk: CId::new(0),
                next_chunk: None
            }],
        );
        // Items contains the pushed items.
        assert_eq!(
            relational_linked_chunk.items_chunks,
            &[
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(0), 0),
                    item: Either::Item('b')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(0), 1),
                    item: Either::Item('c')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(0), 2),
                    item: Either::Item('e')
                },
            ],
        );
    }

    #[test]
    fn test_detach_last_items() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = OwnedLinkedChunkId::Room(room_id.to_owned());

        let mut relational_linked_chunk = RelationalLinkedChunk::<_, char, ()>::new();

        relational_linked_chunk.apply_updates(
            linked_chunk_id.as_ref(),
            vec![
                // new chunk
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // new chunk
                Update::NewItemsChunk { previous: Some(CId::new(0)), new: CId::new(1), next: None },
                // new items on 0
                Update::PushItems {
                    at: Position::new(CId::new(0), 0),
                    items: vec!['a', 'b', 'c', 'd', 'e'],
                },
                // new items on 1
                Update::PushItems { at: Position::new(CId::new(1), 0), items: vec!['x', 'y', 'z'] },
                // detach last items on 0
                Update::DetachLastItems { at: Position::new(CId::new(0), 2) },
            ],
        );

        // Chunks are correctly linked.
        assert_eq!(
            relational_linked_chunk.chunks,
            &[
                ChunkRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    previous_chunk: None,
                    chunk: CId::new(0),
                    next_chunk: Some(CId::new(1))
                },
                ChunkRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    previous_chunk: Some(CId::new(0)),
                    chunk: CId::new(1),
                    next_chunk: None
                },
            ],
        );
        // Items contains the pushed items.
        assert_eq!(
            relational_linked_chunk.items_chunks,
            &[
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(0), 0),
                    item: Either::Item('a')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(0), 1),
                    item: Either::Item('b')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(1), 0),
                    item: Either::Item('x')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(1), 1),
                    item: Either::Item('y')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(1), 2),
                    item: Either::Item('z')
                },
            ],
        );
    }

    #[test]
    fn test_start_and_end_reattach_items() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = OwnedLinkedChunkId::Room(room_id.to_owned());

        let mut relational_linked_chunk = RelationalLinkedChunk::<_, char, ()>::new();

        relational_linked_chunk.apply_updates(
            linked_chunk_id.as_ref(),
            vec![Update::StartReattachItems, Update::EndReattachItems],
        );

        // Nothing happened.
        assert!(relational_linked_chunk.chunks.is_empty());
        assert!(relational_linked_chunk.items_chunks.is_empty());
    }

    #[test]
    fn test_clear() {
        let r0 = room_id!("!r0:matrix.org");
        let linked_chunk_id0 = OwnedLinkedChunkId::Room(r0.to_owned());

        let r1 = room_id!("!r1:matrix.org");
        let linked_chunk_id1 = OwnedLinkedChunkId::Room(r1.to_owned());

        let mut relational_linked_chunk = RelationalLinkedChunk::<_, char, ()>::new();

        relational_linked_chunk.apply_updates(
            linked_chunk_id0.as_ref(),
            vec![
                // new chunk (this is not mandatory for this test, but let's try to be realistic)
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // new items on 0
                Update::PushItems { at: Position::new(CId::new(0), 0), items: vec!['a', 'b', 'c'] },
            ],
        );

        relational_linked_chunk.apply_updates(
            linked_chunk_id1.as_ref(),
            vec![
                // new chunk (this is not mandatory for this test, but let's try to be realistic)
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // new items on 0
                Update::PushItems { at: Position::new(CId::new(0), 0), items: vec!['x'] },
            ],
        );

        // Chunks are correctly linked.
        assert_eq!(
            relational_linked_chunk.chunks,
            &[
                ChunkRow {
                    linked_chunk_id: linked_chunk_id0.to_owned(),
                    previous_chunk: None,
                    chunk: CId::new(0),
                    next_chunk: None,
                },
                ChunkRow {
                    linked_chunk_id: linked_chunk_id1.to_owned(),
                    previous_chunk: None,
                    chunk: CId::new(0),
                    next_chunk: None,
                }
            ],
        );

        // Items contains the pushed items.
        assert_eq!(
            relational_linked_chunk.items_chunks,
            &[
                ItemRow {
                    linked_chunk_id: linked_chunk_id0.to_owned(),
                    position: Position::new(CId::new(0), 0),
                    item: Either::Item('a')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id0.to_owned(),
                    position: Position::new(CId::new(0), 1),
                    item: Either::Item('b')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id0.to_owned(),
                    position: Position::new(CId::new(0), 2),
                    item: Either::Item('c')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id1.to_owned(),
                    position: Position::new(CId::new(0), 0),
                    item: Either::Item('x')
                },
            ],
        );

        // Now, time for a clean up.
        relational_linked_chunk.apply_updates(linked_chunk_id0.as_ref(), vec![Update::Clear]);

        // Only items from r1 remain.
        assert_eq!(
            relational_linked_chunk.chunks,
            &[ChunkRow {
                linked_chunk_id: linked_chunk_id1.to_owned(),
                previous_chunk: None,
                chunk: CId::new(0),
                next_chunk: None,
            }],
        );

        assert_eq!(
            relational_linked_chunk.items_chunks,
            &[ItemRow {
                linked_chunk_id: linked_chunk_id1.to_owned(),
                position: Position::new(CId::new(0), 0),
                item: Either::Item('x')
            },],
        );
    }

    #[test]
    fn test_load_empty_linked_chunk() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = OwnedLinkedChunkId::Room(room_id.to_owned());

        // When I reload the linked chunk components from an empty store,
        let relational_linked_chunk = RelationalLinkedChunk::<_, char, char>::new();
        let result = relational_linked_chunk.load_all_chunks(linked_chunk_id.as_ref()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_load_all_chunks_with_empty_items() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = OwnedLinkedChunkId::Room(room_id.to_owned());

        let mut relational_linked_chunk = RelationalLinkedChunk::<_, char, char>::new();

        // When I store an empty items chunks,
        relational_linked_chunk.apply_updates(
            linked_chunk_id.as_ref(),
            vec![Update::NewItemsChunk { previous: None, new: CId::new(0), next: None }],
        );

        // It correctly gets reloaded as such.
        let lc = from_all_chunks::<3, _, _>(
            relational_linked_chunk.load_all_chunks(linked_chunk_id.as_ref()).unwrap(),
        )
        .expect("building succeeds")
        .expect("this leads to a non-empty linked chunk");

        assert_items_eq!(lc, []);
    }

    #[test]
    fn test_rebuild_linked_chunk() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = OwnedLinkedChunkId::Room(room_id.to_owned());

        let mut relational_linked_chunk = RelationalLinkedChunk::<_, char, char>::new();

        relational_linked_chunk.apply_updates(
            linked_chunk_id.as_ref(),
            vec![
                // new chunk
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // new items on 0
                Update::PushItems { at: Position::new(CId::new(0), 0), items: vec!['a', 'b', 'c'] },
                // a gap chunk
                Update::NewGapChunk {
                    previous: Some(CId::new(0)),
                    new: CId::new(1),
                    next: None,
                    gap: 'g',
                },
                // another items chunk
                Update::NewItemsChunk { previous: Some(CId::new(1)), new: CId::new(2), next: None },
                // new items on 0
                Update::PushItems { at: Position::new(CId::new(2), 0), items: vec!['d', 'e', 'f'] },
            ],
        );

        let lc = from_all_chunks::<3, _, _>(
            relational_linked_chunk.load_all_chunks(linked_chunk_id.as_ref()).unwrap(),
        )
        .expect("building succeeds")
        .expect("this leads to a non-empty linked chunk");

        // The linked chunk is correctly reloaded.
        assert_items_eq!(lc, ['a', 'b', 'c'] [-] ['d', 'e', 'f']);
    }

    #[test]
    fn test_replace_item() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = OwnedLinkedChunkId::Room(room_id.to_owned());

        let mut relational_linked_chunk = RelationalLinkedChunk::<_, char, ()>::new();

        relational_linked_chunk.apply_updates(
            linked_chunk_id.as_ref(),
            vec![
                // new chunk (this is not mandatory for this test, but let's try to be realistic)
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                // new items on 0
                Update::PushItems { at: Position::new(CId::new(0), 0), items: vec!['a', 'b', 'c'] },
                // update item at (0; 1).
                Update::ReplaceItem { at: Position::new(CId::new(0), 1), item: 'B' },
            ],
        );

        // Chunks are correctly linked.
        assert_eq!(
            relational_linked_chunk.chunks,
            &[ChunkRow {
                linked_chunk_id: linked_chunk_id.clone(),
                previous_chunk: None,
                chunk: CId::new(0),
                next_chunk: None,
            },],
        );

        // Items contains the pushed *and* replaced items.
        assert_eq!(
            relational_linked_chunk.items_chunks,
            &[
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(0), 0),
                    item: Either::Item('a')
                },
                ItemRow {
                    linked_chunk_id: linked_chunk_id.clone(),
                    position: Position::new(CId::new(0), 1),
                    item: Either::Item('B')
                },
                ItemRow {
                    linked_chunk_id,
                    position: Position::new(CId::new(0), 2),
                    item: Either::Item('c')
                },
            ],
        );
    }

    #[test]
    fn test_unordered_events() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = OwnedLinkedChunkId::Room(room_id.to_owned());

        let other_room_id = room_id!("!r1:matrix.org");
        let other_linked_chunk_id = OwnedLinkedChunkId::Room(other_room_id.to_owned());

        let mut relational_linked_chunk = RelationalLinkedChunk::<_, char, ()>::new();

        relational_linked_chunk.apply_updates(
            linked_chunk_id.as_ref(),
            vec![
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                Update::PushItems { at: Position::new(CId::new(0), 0), items: vec!['a', 'b', 'c'] },
                Update::NewItemsChunk { previous: Some(CId::new(0)), new: CId::new(1), next: None },
                Update::PushItems { at: Position::new(CId::new(1), 0), items: vec!['d', 'e', 'f'] },
            ],
        );

        relational_linked_chunk.apply_updates(
            other_linked_chunk_id.as_ref(),
            vec![
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                Update::PushItems { at: Position::new(CId::new(0), 0), items: vec!['x', 'y', 'z'] },
            ],
        );

        let events = BTreeMap::from_iter(
            relational_linked_chunk.unordered_linked_chunk_items(&linked_chunk_id),
        );

        assert_eq!(events.len(), 6);
        assert_eq!(*events.get(&'a').unwrap(), Position::new(CId::new(0), 0));
        assert_eq!(*events.get(&'b').unwrap(), Position::new(CId::new(0), 1));
        assert_eq!(*events.get(&'c').unwrap(), Position::new(CId::new(0), 2));
        assert_eq!(*events.get(&'d').unwrap(), Position::new(CId::new(1), 0));
        assert_eq!(*events.get(&'e').unwrap(), Position::new(CId::new(1), 1));
        assert_eq!(*events.get(&'f').unwrap(), Position::new(CId::new(1), 2));
    }

    #[test]
    fn test_load_last_chunk() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = OwnedLinkedChunkId::Room(room_id.to_owned());

        let mut relational_linked_chunk = RelationalLinkedChunk::<_, char, ()>::new();

        // Case #1: no last chunk.
        {
            let (last_chunk, chunk_identifier_generator) =
                relational_linked_chunk.load_last_chunk(linked_chunk_id.as_ref()).unwrap();

            assert!(last_chunk.is_none());
            assert_eq!(chunk_identifier_generator.current(), 0);
        }

        // Case #2: only one chunk is present.
        {
            relational_linked_chunk.apply_updates(
                linked_chunk_id.as_ref(),
                vec![
                    Update::NewItemsChunk { previous: None, new: CId::new(42), next: None },
                    Update::PushItems { at: Position::new(CId::new(42), 0), items: vec!['a', 'b'] },
                ],
            );

            let (last_chunk, chunk_identifier_generator) =
                relational_linked_chunk.load_last_chunk(linked_chunk_id.as_ref()).unwrap();

            assert_matches!(last_chunk, Some(last_chunk) => {
                assert_eq!(last_chunk.identifier, 42);
                assert!(last_chunk.previous.is_none());
                assert!(last_chunk.next.is_none());
                assert_matches!(last_chunk.content, ChunkContent::Items(items) => {
                    assert_eq!(items.len(), 2);
                    assert_eq!(items, &['a', 'b']);
                });
            });
            assert_eq!(chunk_identifier_generator.current(), 42);
        }

        // Case #3: more chunks are present.
        {
            relational_linked_chunk.apply_updates(
                linked_chunk_id.as_ref(),
                vec![
                    Update::NewItemsChunk {
                        previous: Some(CId::new(42)),
                        new: CId::new(7),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(CId::new(7), 0),
                        items: vec!['c', 'd', 'e'],
                    },
                ],
            );

            let (last_chunk, chunk_identifier_generator) =
                relational_linked_chunk.load_last_chunk(linked_chunk_id.as_ref()).unwrap();

            assert_matches!(last_chunk, Some(last_chunk) => {
                assert_eq!(last_chunk.identifier, 7);
                assert_matches!(last_chunk.previous, Some(previous) => {
                    assert_eq!(previous, 42);
                });
                assert!(last_chunk.next.is_none());
                assert_matches!(last_chunk.content, ChunkContent::Items(items) => {
                    assert_eq!(items.len(), 3);
                    assert_eq!(items, &['c', 'd', 'e']);
                });
            });
            assert_eq!(chunk_identifier_generator.current(), 42);
        }
    }

    #[test]
    fn test_load_last_chunk_with_a_cycle() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = OwnedLinkedChunkId::Room(room_id.to_owned());
        let mut relational_linked_chunk = RelationalLinkedChunk::<_, char, ()>::new();

        relational_linked_chunk.apply_updates(
            linked_chunk_id.as_ref(),
            vec![
                Update::NewItemsChunk { previous: None, new: CId::new(0), next: None },
                Update::NewItemsChunk {
                    // Because `previous` connects to chunk #0, it will create a cycle.
                    // Chunk #0 will have a `next` set to chunk #1! Consequently, the last chunk
                    // **does not exist**. We have to detect this cycle.
                    previous: Some(CId::new(0)),
                    new: CId::new(1),
                    next: Some(CId::new(0)),
                },
            ],
        );

        relational_linked_chunk.load_last_chunk(linked_chunk_id.as_ref()).unwrap_err();
    }

    #[test]
    fn test_load_previous_chunk() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = OwnedLinkedChunkId::Room(room_id.to_owned());
        let mut relational_linked_chunk = RelationalLinkedChunk::<_, char, ()>::new();

        // Case #1: no chunk at all, equivalent to having an inexistent
        // `before_chunk_identifier`.
        {
            let previous_chunk = relational_linked_chunk
                .load_previous_chunk(linked_chunk_id.as_ref(), CId::new(153))
                .unwrap();

            assert!(previous_chunk.is_none());
        }

        // Case #2: there is one chunk only: we request the previous on this
        // one, it doesn't exist.
        {
            relational_linked_chunk.apply_updates(
                linked_chunk_id.as_ref(),
                vec![Update::NewItemsChunk { previous: None, new: CId::new(42), next: None }],
            );

            let previous_chunk = relational_linked_chunk
                .load_previous_chunk(linked_chunk_id.as_ref(), CId::new(42))
                .unwrap();

            assert!(previous_chunk.is_none());
        }

        // Case #3: there is two chunks.
        {
            relational_linked_chunk.apply_updates(
                linked_chunk_id.as_ref(),
                vec![
                    // new chunk before the one that exists.
                    Update::NewItemsChunk {
                        previous: None,
                        new: CId::new(7),
                        next: Some(CId::new(42)),
                    },
                    Update::PushItems {
                        at: Position::new(CId::new(7), 0),
                        items: vec!['a', 'b', 'c'],
                    },
                ],
            );

            let previous_chunk = relational_linked_chunk
                .load_previous_chunk(linked_chunk_id.as_ref(), CId::new(42))
                .unwrap();

            assert_matches!(previous_chunk, Some(previous_chunk) => {
                assert_eq!(previous_chunk.identifier, 7);
                assert!(previous_chunk.previous.is_none());
                assert_matches!(previous_chunk.next, Some(next) => {
                    assert_eq!(next, 42);
                });
                assert_matches!(previous_chunk.content, ChunkContent::Items(items) => {
                    assert_eq!(items.len(), 3);
                    assert_eq!(items, &['a', 'b', 'c']);
                });
            });
        }
    }
}
