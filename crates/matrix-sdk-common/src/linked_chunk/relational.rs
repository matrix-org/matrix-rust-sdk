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

use ruma::{OwnedRoomId, RoomId};

use crate::linked_chunk::{ChunkIdentifier, Position, Update};

/// A row of the [`RelationalLinkedChunk::chunks`].
#[derive(Debug, PartialEq)]
struct ChunkRow {
    room_id: OwnedRoomId,
    previous_chunk: Option<ChunkIdentifier>,
    chunk: ChunkIdentifier,
    next_chunk: Option<ChunkIdentifier>,
}

/// A row of the [`RelationalLinkedChunk::items`].
#[derive(Debug, PartialEq)]
struct ItemRow<Item, Gap> {
    room_id: OwnedRoomId,
    position: Position,
    item: Either<Item, Gap>,
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
pub struct RelationalLinkedChunk<Item, Gap> {
    /// Chunks.
    chunks: Vec<ChunkRow>,

    /// Items.
    items: Vec<ItemRow<Item, Gap>>,
}

impl<Item, Gap> RelationalLinkedChunk<Item, Gap> {
    /// Create a new relational linked chunk.
    pub fn new() -> Self {
        Self { chunks: Vec::new(), items: Vec::new() }
    }

    /// Apply [`Update`]s. That's the only way to write data inside this
    /// relational linked chunk.
    pub fn apply_updates(&mut self, room_id: &RoomId, updates: Vec<Update<Item, Gap>>) {
        for update in updates {
            match update {
                Update::NewItemsChunk { previous, new, next } => {
                    insert_chunk(&mut self.chunks, room_id, previous, new, next);
                }

                Update::NewGapChunk { previous, new, next, gap } => {
                    insert_chunk(&mut self.chunks, room_id, previous, new, next);
                    self.items.push(ItemRow {
                        room_id: room_id.to_owned(),
                        position: Position::new(new, 0),
                        item: Either::Gap(gap),
                    });
                }

                Update::RemoveChunk(chunk_identifier) => {
                    remove_chunk(&mut self.chunks, room_id, chunk_identifier);

                    let indices_to_remove = self
                        .items
                        .iter()
                        .enumerate()
                        .filter_map(
                            |(nth, ItemRow { room_id: room_id_candidate, position, .. })| {
                                (room_id == room_id_candidate
                                    && position.chunk_identifier() == chunk_identifier)
                                    .then_some(nth)
                            },
                        )
                        .collect::<Vec<_>>();

                    for index_to_remove in indices_to_remove.into_iter().rev() {
                        self.items.remove(index_to_remove);
                    }
                }

                Update::PushItems { mut at, items } => {
                    for item in items {
                        self.items.push(ItemRow {
                            room_id: room_id.to_owned(),
                            position: at,
                            item: Either::Item(item),
                        });
                        at.increment_index();
                    }
                }

                Update::RemoveItem { at } => {
                    let mut entry_to_remove = None;

                    for (nth, ItemRow { room_id: room_id_candidate, position, .. }) in
                        self.items.iter_mut().enumerate()
                    {
                        // Filter by room ID.
                        if room_id != room_id_candidate {
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

                    self.items.remove(entry_to_remove.expect("Remove an unknown item"));
                }

                Update::DetachLastItems { at } => {
                    let indices_to_remove = self
                        .items
                        .iter()
                        .enumerate()
                        .filter_map(
                            |(nth, ItemRow { room_id: room_id_candidate, position, .. })| {
                                (room_id == room_id_candidate
                                    && position.chunk_identifier() == at.chunk_identifier()
                                    && position.index() >= at.index())
                                .then_some(nth)
                            },
                        )
                        .collect::<Vec<_>>();

                    for index_to_remove in indices_to_remove.into_iter().rev() {
                        self.items.remove(index_to_remove);
                    }
                }

                Update::StartReattachItems | Update::EndReattachItems => { /* nothing */ }
            }
        }

        fn insert_chunk(
            chunks: &mut Vec<ChunkRow>,
            room_id: &RoomId,
            previous: Option<ChunkIdentifier>,
            new: ChunkIdentifier,
            next: Option<ChunkIdentifier>,
        ) {
            // Find the previous chunk, and update its next chunk.
            if let Some(previous) = previous {
                let entry_for_previous_chunk = chunks
                    .iter_mut()
                    .find(|ChunkRow { room_id: room_id_candidate, chunk, .. }| {
                        room_id == room_id_candidate && *chunk == previous
                    })
                    .expect("Previous chunk should be present");

                // Link the chunk.
                entry_for_previous_chunk.next_chunk = Some(new);
            }

            // Find the next chunk, and update its previous chunk.
            if let Some(next) = next {
                let entry_for_next_chunk = chunks
                    .iter_mut()
                    .find(|ChunkRow { room_id: room_id_candidate, chunk, .. }| {
                        room_id == room_id_candidate && *chunk == next
                    })
                    .expect("Next chunk should be present");

                // Link the chunk.
                entry_for_next_chunk.previous_chunk = Some(new);
            }

            // Insert the chunk.
            chunks.push(ChunkRow {
                room_id: room_id.to_owned(),
                previous_chunk: previous,
                chunk: new,
                next_chunk: next,
            });
        }

        fn remove_chunk(
            chunks: &mut Vec<ChunkRow>,
            room_id: &RoomId,
            chunk_to_remove: ChunkIdentifier,
        ) {
            let entry_nth_to_remove = chunks
                .iter()
                .enumerate()
                .find_map(|(nth, ChunkRow { room_id: room_id_candidate, chunk, .. })| {
                    (room_id == room_id_candidate && *chunk == chunk_to_remove).then_some(nth)
                })
                .expect("Remove an unknown chunk");

            let ChunkRow { room_id, previous_chunk: previous, next_chunk: next, .. } =
                chunks.remove(entry_nth_to_remove);

            // Find the previous chunk, and update its next chunk.
            if let Some(previous) = previous {
                let entry_for_previous_chunk = chunks
                    .iter_mut()
                    .find(|ChunkRow { room_id: room_id_candidate, chunk, .. }| {
                        &room_id == room_id_candidate && *chunk == previous
                    })
                    .expect("Previous chunk should be present");

                // Insert the chunk.
                entry_for_previous_chunk.next_chunk = next;
            }

            // Find the next chunk, and update its previous chunk.
            if let Some(next) = next {
                let entry_for_next_chunk = chunks
                    .iter_mut()
                    .find(|ChunkRow { room_id: room_id_candidate, chunk, .. }| {
                        &room_id == room_id_candidate && *chunk == next
                    })
                    .expect("Next chunk should be present");

                // Insert the chunk.
                entry_for_next_chunk.previous_chunk = previous;
            }
        }
    }
}

impl<Item, Gap> Default for RelationalLinkedChunk<Item, Gap> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use ruma::room_id;

    use super::{ChunkIdentifier as CId, *};

    #[test]
    fn test_new_items_chunk() {
        let room_id = room_id!("!r0:matrix.org");
        let mut relational_linked_chunk = RelationalLinkedChunk::<char, ()>::new();

        relational_linked_chunk.apply_updates(
            room_id,
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
                    room_id: room_id.to_owned(),
                    previous_chunk: Some(CId::new(3)),
                    chunk: CId::new(0),
                    next_chunk: Some(CId::new(1))
                },
                ChunkRow {
                    room_id: room_id.to_owned(),
                    previous_chunk: Some(CId::new(0)),
                    chunk: CId::new(1),
                    next_chunk: None
                },
                ChunkRow {
                    room_id: room_id.to_owned(),
                    previous_chunk: None,
                    chunk: CId::new(2),
                    next_chunk: Some(CId::new(3))
                },
                ChunkRow {
                    room_id: room_id.to_owned(),
                    previous_chunk: Some(CId::new(2)),
                    chunk: CId::new(3),
                    next_chunk: Some(CId::new(0))
                },
            ],
        );
        // Items have not been modified.
        assert!(relational_linked_chunk.items.is_empty());
    }

    #[test]
    fn test_new_gap_chunk() {
        let room_id = room_id!("!r0:matrix.org");
        let mut relational_linked_chunk = RelationalLinkedChunk::<char, ()>::new();

        relational_linked_chunk.apply_updates(
            room_id,
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
                    room_id: room_id.to_owned(),
                    previous_chunk: None,
                    chunk: CId::new(0),
                    next_chunk: Some(CId::new(1))
                },
                ChunkRow {
                    room_id: room_id.to_owned(),
                    previous_chunk: Some(CId::new(0)),
                    chunk: CId::new(1),
                    next_chunk: Some(CId::new(2))
                },
                ChunkRow {
                    room_id: room_id.to_owned(),
                    previous_chunk: Some(CId::new(1)),
                    chunk: CId::new(2),
                    next_chunk: None
                },
            ],
        );
        // Items contains the gap.
        assert_eq!(
            relational_linked_chunk.items,
            &[ItemRow {
                room_id: room_id.to_owned(),
                position: Position::new(CId::new(1), 0),
                item: Either::Gap(())
            }],
        );
    }

    #[test]
    fn test_remove_chunk() {
        let room_id = room_id!("!r0:matrix.org");
        let mut relational_linked_chunk = RelationalLinkedChunk::<char, ()>::new();

        relational_linked_chunk.apply_updates(
            room_id,
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
                    room_id: room_id.to_owned(),
                    previous_chunk: None,
                    chunk: CId::new(0),
                    next_chunk: Some(CId::new(2))
                },
                ChunkRow {
                    room_id: room_id.to_owned(),
                    previous_chunk: Some(CId::new(0)),
                    chunk: CId::new(2),
                    next_chunk: None
                },
            ],
        );
        // Items no longer contains the gap.
        assert!(relational_linked_chunk.items.is_empty());
    }

    #[test]
    fn test_push_items() {
        let room_id = room_id!("!r0:matrix.org");
        let mut relational_linked_chunk = RelationalLinkedChunk::<char, ()>::new();

        relational_linked_chunk.apply_updates(
            room_id,
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
                    room_id: room_id.to_owned(),
                    previous_chunk: None,
                    chunk: CId::new(0),
                    next_chunk: Some(CId::new(1))
                },
                ChunkRow {
                    room_id: room_id.to_owned(),
                    previous_chunk: Some(CId::new(0)),
                    chunk: CId::new(1),
                    next_chunk: None
                },
            ],
        );
        // Items contains the pushed items.
        assert_eq!(
            relational_linked_chunk.items,
            &[
                ItemRow {
                    room_id: room_id.to_owned(),
                    position: Position::new(CId::new(0), 0),
                    item: Either::Item('a')
                },
                ItemRow {
                    room_id: room_id.to_owned(),
                    position: Position::new(CId::new(0), 1),
                    item: Either::Item('b')
                },
                ItemRow {
                    room_id: room_id.to_owned(),
                    position: Position::new(CId::new(0), 2),
                    item: Either::Item('c')
                },
                ItemRow {
                    room_id: room_id.to_owned(),
                    position: Position::new(CId::new(1), 0),
                    item: Either::Item('x')
                },
                ItemRow {
                    room_id: room_id.to_owned(),
                    position: Position::new(CId::new(1), 1),
                    item: Either::Item('y')
                },
                ItemRow {
                    room_id: room_id.to_owned(),
                    position: Position::new(CId::new(1), 2),
                    item: Either::Item('z')
                },
                ItemRow {
                    room_id: room_id.to_owned(),
                    position: Position::new(CId::new(0), 3),
                    item: Either::Item('d')
                },
                ItemRow {
                    room_id: room_id.to_owned(),
                    position: Position::new(CId::new(0), 4),
                    item: Either::Item('e')
                },
            ],
        );
    }

    #[test]
    fn test_remove_item() {
        let room_id = room_id!("!r0:matrix.org");
        let mut relational_linked_chunk = RelationalLinkedChunk::<char, ()>::new();

        relational_linked_chunk.apply_updates(
            room_id,
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
                room_id: room_id.to_owned(),
                previous_chunk: None,
                chunk: CId::new(0),
                next_chunk: None
            }],
        );
        // Items contains the pushed items.
        assert_eq!(
            relational_linked_chunk.items,
            &[
                ItemRow {
                    room_id: room_id.to_owned(),
                    position: Position::new(CId::new(0), 0),
                    item: Either::Item('b')
                },
                ItemRow {
                    room_id: room_id.to_owned(),
                    position: Position::new(CId::new(0), 1),
                    item: Either::Item('c')
                },
                ItemRow {
                    room_id: room_id.to_owned(),
                    position: Position::new(CId::new(0), 2),
                    item: Either::Item('e')
                },
            ],
        );
    }

    #[test]
    fn test_detach_last_items() {
        let room_id = room_id!("!r0:matrix.org");
        let mut relational_linked_chunk = RelationalLinkedChunk::<char, ()>::new();

        relational_linked_chunk.apply_updates(
            room_id,
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
                    room_id: room_id.to_owned(),
                    previous_chunk: None,
                    chunk: CId::new(0),
                    next_chunk: Some(CId::new(1))
                },
                ChunkRow {
                    room_id: room_id.to_owned(),
                    previous_chunk: Some(CId::new(0)),
                    chunk: CId::new(1),
                    next_chunk: None
                },
            ],
        );
        // Items contains the pushed items.
        assert_eq!(
            relational_linked_chunk.items,
            &[
                ItemRow {
                    room_id: room_id.to_owned(),
                    position: Position::new(CId::new(0), 0),
                    item: Either::Item('a')
                },
                ItemRow {
                    room_id: room_id.to_owned(),
                    position: Position::new(CId::new(0), 1),
                    item: Either::Item('b')
                },
                ItemRow {
                    room_id: room_id.to_owned(),
                    position: Position::new(CId::new(1), 0),
                    item: Either::Item('x')
                },
                ItemRow {
                    room_id: room_id.to_owned(),
                    position: Position::new(CId::new(1), 1),
                    item: Either::Item('y')
                },
                ItemRow {
                    room_id: room_id.to_owned(),
                    position: Position::new(CId::new(1), 2),
                    item: Either::Item('z')
                },
            ],
        );
    }

    #[test]
    fn test_start_and_end_reattach_items() {
        let room_id = room_id!("!r0:matrix.org");
        let mut relational_linked_chunk = RelationalLinkedChunk::<char, ()>::new();

        relational_linked_chunk
            .apply_updates(room_id, vec![Update::StartReattachItems, Update::EndReattachItems]);

        // Nothing happened.
        assert!(relational_linked_chunk.chunks.is_empty());
        assert!(relational_linked_chunk.items.is_empty());
    }
}
