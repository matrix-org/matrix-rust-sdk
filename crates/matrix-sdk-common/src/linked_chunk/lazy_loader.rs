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

use std::marker::PhantomData;

use super::{
    Chunk, ChunkContent, ChunkIdentifier, ChunkIdentifierGenerator, Ends, LinkedChunk,
    ObservableUpdates, RawChunk, Update,
};

/// Build a new `LinkedChunk` with a single chunk that is supposed to be the
/// last one.
pub fn from_last_chunk<const CAP: usize, Item, Gap>(
    chunk: Option<RawChunk<Item, Gap>>,
    chunk_identifier_generator: ChunkIdentifierGenerator,
) -> Result<Option<LinkedChunk<CAP, Item, Gap>>, LazyLoaderError> {
    let Some(mut chunk) = chunk else {
        return Ok(None);
    };

    // Check consistency before creating the `LinkedChunk`.
    {
        // The number of items is not too large.
        if let ChunkContent::Items(items) = &chunk.content
            && items.len() > CAP
        {
            return Err(LazyLoaderError::ChunkTooLarge { id: chunk.identifier });
        }

        // Chunk has no next chunk.
        if chunk.next.is_some() {
            return Err(LazyLoaderError::ChunkIsNotLast { id: chunk.identifier });
        }
    }

    // Create the `LinkedChunk` from a single chunk.
    {
        // Take the `previous` chunk and consider it becomes the `lazy_previous`.
        let lazy_previous = chunk.previous.take();

        // Transform the `RawChunk` into a `Chunk`.
        let mut chunk_ptr = Chunk::new_leaked(chunk.identifier, chunk.content);

        // Set the `lazy_previous` value!
        //
        // SAFETY: Pointer is convertible to a reference.
        unsafe { chunk_ptr.as_mut() }.lazy_previous = lazy_previous;

        Ok(Some(LinkedChunk {
            links: Ends { first: chunk_ptr, last: None },
            chunk_identifier_generator,
            updates: Some(ObservableUpdates::new()),
            marker: PhantomData,
        }))
    }
}

/// Insert a new chunk at the front of a `LinkedChunk`.
pub fn insert_new_first_chunk<const CAP: usize, Item, Gap>(
    linked_chunk: &mut LinkedChunk<CAP, Item, Gap>,
    mut new_first_chunk: RawChunk<Item, Gap>,
) -> Result<(), LazyLoaderError>
where
    Item: Clone,
    Gap: Clone,
{
    // Check `LinkedChunk` is going to be consistent after the insertion.
    {
        // The number of items is not too large.
        if let ChunkContent::Items(items) = &new_first_chunk.content
            && items.len() > CAP
        {
            return Err(LazyLoaderError::ChunkTooLarge { id: new_first_chunk.identifier });
        }

        // New chunk doesn't create a cycle.
        if let Some(previous_chunk) = new_first_chunk.previous
            && linked_chunk.chunks().any(|chunk| chunk.identifier() == previous_chunk)
        {
            return Err(LazyLoaderError::Cycle {
                new_chunk: new_first_chunk.identifier,
                with_chunk: previous_chunk,
            });
        }

        let first_chunk = linked_chunk.links.first_chunk();
        let expected_next_chunk = first_chunk.identifier();

        // New chunk has a next chunk.
        let Some(next_chunk) = new_first_chunk.next else {
            return Err(LazyLoaderError::MissingNextChunk { id: new_first_chunk.identifier });
        };

        // New chunk has a next chunk, and it is the first chunk of the `LinkedChunk`.
        if next_chunk != expected_next_chunk {
            return Err(LazyLoaderError::CannotConnectTwoChunks {
                new_chunk: new_first_chunk.identifier,
                with_chunk: expected_next_chunk,
            });
        }

        // Same check as before, but in reverse: the first chunk has a `lazy_previous`
        // to the new first chunk.
        if first_chunk.lazy_previous() != Some(new_first_chunk.identifier) {
            return Err(LazyLoaderError::CannotConnectTwoChunks {
                new_chunk: first_chunk.identifier,
                with_chunk: new_first_chunk.identifier,
            });
        }

        // Alright. All checks are made.
    }

    // Insert the new first chunk.
    {
        // Transform the `RawChunk` into a `Chunk`.
        let lazy_previous = new_first_chunk.previous.take();
        let mut new_first_chunk =
            Chunk::new_leaked(new_first_chunk.identifier, new_first_chunk.content);

        let links = &mut linked_chunk.links;

        // Update the first chunk.
        {
            let first_chunk = links.first_chunk_mut();

            debug_assert!(
                first_chunk.previous.is_none(),
                "The first chunk is not supposed to have a previous chunk"
            );

            // Move the `lazy_previous` if any.
            first_chunk.lazy_previous = None;
            unsafe { new_first_chunk.as_mut() }.lazy_previous = lazy_previous;

            // Link one way: `new_first_chunk` becomes the previous chunk of the first
            // chunk.
            first_chunk.previous = Some(new_first_chunk);
        }

        // Update `links`.
        {
            // Remember the pointer to the `first_chunk`.
            let old_first_chunk = links.first;

            // `new_first_chunk` becomes the new first chunk.
            links.first = new_first_chunk;

            // Link the other way: `old_first_chunk` becomes the next chunk of the first
            // chunk.
            links.first_chunk_mut().next = Some(old_first_chunk);

            debug_assert!(
                links.first_chunk().previous.is_none(),
                "The new first chunk is not supposed to have a previous chunk"
            );

            // Update the last chunk. If it's `Some(_)`, no need to update the last chunk
            // pointer. If it's `None`, it means we had only one chunk; now we have two, the
            // last chunk is the `old_first_chunk`.
            if links.last.is_none() {
                links.last = Some(old_first_chunk);
            }
        }
    }

    // Emit the updates.
    if let Some(updates) = linked_chunk.updates.as_mut() {
        let first_chunk = linked_chunk.links.first_chunk();
        emit_new_first_chunk_updates(first_chunk, updates);
    }

    Ok(())
}

/// Emit updates whenever a new first chunk is inserted at the front of a
/// `LinkedChunk`.
fn emit_new_first_chunk_updates<const CAP: usize, Item, Gap>(
    chunk: &Chunk<CAP, Item, Gap>,
    updates: &mut ObservableUpdates<Item, Gap>,
) where
    Item: Clone,
    Gap: Clone,
{
    let previous = chunk.previous().map(Chunk::identifier).or(chunk.lazy_previous);
    let new = chunk.identifier();
    let next = chunk.next().map(Chunk::identifier);

    match chunk.content() {
        ChunkContent::Gap(gap) => {
            updates.push(Update::NewGapChunk { previous, new, next, gap: gap.clone() });
        }
        ChunkContent::Items(items) => {
            updates.push(Update::NewItemsChunk { previous, new, next });
            updates.push(Update::PushItems { at: chunk.first_position(), items: items.clone() });
        }
    }
}

/// Replace the items with the given last chunk of items and generator.
///
/// This clears all the chunks in memory before resetting to the new chunk,
/// if provided.
pub fn replace_with<const CAP: usize, Item, Gap>(
    linked_chunk: &mut LinkedChunk<CAP, Item, Gap>,
    chunk: Option<RawChunk<Item, Gap>>,
    chunk_identifier_generator: ChunkIdentifierGenerator,
) -> Result<(), LazyLoaderError>
where
    Item: Clone,
    Gap: Clone,
{
    let Some(mut chunk) = chunk else {
        // This is equivalent to clearing the linked chunk, and overriding the chunk ID
        // generator afterwards. But, if there was no chunks in the DB, the generator
        // should be reset too, so it's entirely equivalent to a clear.
        linked_chunk.clear();
        return Ok(());
    };

    // Check consistency before replacing the `LinkedChunk`.
    // The number of items is not too large.
    if let ChunkContent::Items(items) = &chunk.content
        && items.len() > CAP
    {
        return Err(LazyLoaderError::ChunkTooLarge { id: chunk.identifier });
    }

    // Chunk has no next chunk.
    if chunk.next.is_some() {
        return Err(LazyLoaderError::ChunkIsNotLast { id: chunk.identifier });
    }

    // The last chunk is now valid.
    linked_chunk.chunk_identifier_generator = chunk_identifier_generator;

    // Take the `previous` chunk and consider it becomes the `lazy_previous`.
    let lazy_previous = chunk.previous.take();

    // Transform the `RawChunk` into a `Chunk`.
    let mut chunk_ptr = Chunk::new_leaked(chunk.identifier, chunk.content);

    // Set the `lazy_previous` value!
    //
    // SAFETY: Pointer is convertible to a reference.
    unsafe { chunk_ptr.as_mut() }.lazy_previous = lazy_previous;

    // Replace the first link with the new pointer.
    linked_chunk.links.replace_with(chunk_ptr);

    if let Some(updates) = linked_chunk.updates.as_mut() {
        // Clear the previous updates, as we're about to insert a clear they would be
        // useless.
        updates.clear_pending();
        updates.push(Update::Clear);

        emit_new_first_chunk_updates(linked_chunk.links.first_chunk(), updates);
    }

    Ok(())
}

/// A pretty inefficient, test-only, function to rebuild a full `LinkedChunk`.
#[doc(hidden)]
pub fn from_all_chunks<const CAP: usize, Item, Gap>(
    mut chunks: Vec<RawChunk<Item, Gap>>,
) -> Result<Option<LinkedChunk<CAP, Item, Gap>>, LazyLoaderError>
where
    Item: Clone,
    Gap: Clone,
{
    if chunks.is_empty() {
        return Ok(None);
    }

    // Sort by `next` so that the search for the next chunk is faster (it should
    // come first). The chunk with the biggest next chunk identifier comes first.
    // Chunk with no next chunk comes last.
    chunks.sort_by(|a, b| b.next.cmp(&a.next));

    let last_chunk = chunks
        .pop()
        // SAFETY: `chunks` is guaranteed to not be empty, `pop` cannot fail.
        .expect("`chunks` is supposed to not be empty, we must be able to `pop` an item");
    let last_chunk_identifier = last_chunk.identifier;
    let chunk_identifier_generator =
        ChunkIdentifierGenerator::new_from_previous_chunk_identifier(last_chunk_identifier);

    let Some(mut linked_chunk) = from_last_chunk(Some(last_chunk), chunk_identifier_generator)?
    else {
        return Ok(None);
    };

    let mut next_chunk = last_chunk_identifier;

    while let Some(chunk) = chunks
        .iter()
        .position(|chunk| chunk.next == Some(next_chunk))
        .map(|index| chunks.remove(index))
    {
        next_chunk = chunk.identifier;
        insert_new_first_chunk(&mut linked_chunk, chunk)?;
    }

    let first_chunk = linked_chunk.links.first_chunk();

    // It is expected that **all chunks** are passed to this function. If there was
    // a previous chunk, `insert_new_first_chunk` has erased it and moved it to
    // `lazy_previous`. Hence, let's check both (the former condition isn't
    // necessary, but better be robust).
    if first_chunk.previous().is_some() || first_chunk.lazy_previous.is_some() {
        return Err(LazyLoaderError::ChunkIsNotFirst { id: first_chunk.identifier() });
    }

    if !chunks.is_empty() {
        return Err(LazyLoaderError::MultipleConnectedComponents);
    }

    Ok(Some(linked_chunk))
}

#[derive(thiserror::Error, Debug)]
pub enum LazyLoaderError {
    #[error("chunk with id {} has a next chunk, it is supposed to be the last chunk", id.index())]
    ChunkIsNotLast { id: ChunkIdentifier },

    #[error("chunk with id {} forms a cycle with chunk with id {}", new_chunk.index(), with_chunk.index())]
    Cycle { new_chunk: ChunkIdentifier, with_chunk: ChunkIdentifier },

    #[error("chunk with id {} is supposed to have a next chunk", id.index())]
    MissingNextChunk { id: ChunkIdentifier },

    #[error(
        "chunk with id {} cannot be connected to chunk with id {} because the identifiers do not match",
        new_chunk.index(),
        with_chunk.index()
    )]
    CannotConnectTwoChunks { new_chunk: ChunkIdentifier, with_chunk: ChunkIdentifier },

    #[error("chunk with id {} is too large", id.index())]
    ChunkTooLarge { id: ChunkIdentifier },

    #[doc(hidden)]
    #[error("the last chunk is missing")]
    MissingLastChunk,

    #[doc(hidden)]
    #[error("chunk with id {} has a previous chunk, it is supposed to be the first chunk", id.index())]
    ChunkIsNotFirst { id: ChunkIdentifier },

    #[doc(hidden)]
    #[error("multiple connected components")]
    MultipleConnectedComponents,
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;

    use super::{
        super::Position, ChunkContent, ChunkIdentifier, ChunkIdentifierGenerator, LazyLoaderError,
        LinkedChunk, RawChunk, Update, from_all_chunks, from_last_chunk, insert_new_first_chunk,
        replace_with,
    };

    #[test]
    fn test_from_last_chunk_err_too_much_items() {
        let last_chunk = RawChunk {
            previous: None,
            identifier: ChunkIdentifier::new(0),
            next: None,
            content: ChunkContent::Items(vec!['a', 'b', 'c']),
        };
        let chunk_identifier_generator =
            ChunkIdentifierGenerator::new_from_previous_chunk_identifier(ChunkIdentifier::new(0));

        let maybe_linked_chunk =
            from_last_chunk::<2, char, ()>(Some(last_chunk), chunk_identifier_generator);

        assert_matches!(
            maybe_linked_chunk,
            Err(LazyLoaderError::ChunkTooLarge { id }) => {
                assert_eq!(id, 0);
            }
        );
    }

    #[test]
    fn test_from_last_chunk_err_is_not_last_chunk() {
        let last_chunk = RawChunk {
            previous: None,
            identifier: ChunkIdentifier::new(0),
            next: Some(ChunkIdentifier::new(42)),
            content: ChunkContent::Items(vec!['a']),
        };
        let chunk_identifier_generator =
            ChunkIdentifierGenerator::new_from_previous_chunk_identifier(ChunkIdentifier::new(0));

        let maybe_linked_chunk =
            from_last_chunk::<2, char, ()>(Some(last_chunk), chunk_identifier_generator);

        assert_matches!(
            maybe_linked_chunk,
            Err(LazyLoaderError::ChunkIsNotLast { id }) => {
                assert_eq!(id, 0);
            }
        );
    }

    #[test]
    fn test_from_last_chunk_none() {
        let chunk_identifier_generator =
            ChunkIdentifierGenerator::new_from_previous_chunk_identifier(ChunkIdentifier::new(0));

        let maybe_linked_chunk =
            from_last_chunk::<2, char, ()>(None, chunk_identifier_generator).unwrap();

        assert!(maybe_linked_chunk.is_none());
    }

    #[test]
    fn test_from_last_chunk() {
        let last_chunk = RawChunk {
            previous: Some(ChunkIdentifier::new(42)),
            identifier: ChunkIdentifier::new(0),
            next: None,
            content: ChunkContent::Items(vec!['a']),
        };
        let chunk_identifier_generator =
            ChunkIdentifierGenerator::new_from_previous_chunk_identifier(ChunkIdentifier::new(0));

        let maybe_linked_chunk =
            from_last_chunk::<2, char, ()>(Some(last_chunk), chunk_identifier_generator).unwrap();

        assert_matches!(maybe_linked_chunk, Some(mut linked_chunk) => {
            let mut chunks = linked_chunk.chunks();

            assert_matches!(chunks.next(), Some(chunk) => {
                assert_eq!(chunk.identifier(), 0);
                // The chunk's previous has been set to `None`
                assert!(chunk.previous().is_none());
            });
            assert!(chunks.next().is_none());

            // It has updates enabled.
            assert!(linked_chunk.updates().is_some());
        });
    }

    #[test]
    fn test_insert_new_first_chunk_err_too_much_items() {
        let new_first_chunk = RawChunk {
            previous: None,
            identifier: ChunkIdentifier::new(0),
            next: None,
            content: ChunkContent::Items(vec!['a', 'b', 'c']),
        };

        let mut linked_chunk = LinkedChunk::<2, char, ()>::new();

        let result = insert_new_first_chunk(&mut linked_chunk, new_first_chunk);

        assert_matches!(result, Err(LazyLoaderError::ChunkTooLarge { id }) => {
            assert_eq!(id, 0);
        });
    }

    #[test]
    fn test_insert_new_first_chunk_err_cycle() {
        let new_first_chunk = RawChunk {
            previous: Some(ChunkIdentifier::new(0)),
            identifier: ChunkIdentifier::new(1),
            next: Some(ChunkIdentifier::new(0)),
            content: ChunkContent::Gap(()),
        };

        let mut linked_chunk = LinkedChunk::<2, char, ()>::new();
        let result = insert_new_first_chunk(&mut linked_chunk, new_first_chunk);

        assert_matches!(result, Err(LazyLoaderError::Cycle { new_chunk, with_chunk }) => {
            assert_eq!(new_chunk, 1);
            assert_eq!(with_chunk, 0);
        });
    }

    #[test]
    fn test_insert_new_first_chunk_err_missing_next_chunk() {
        let new_first_chunk = RawChunk {
            previous: None,
            identifier: ChunkIdentifier::new(0),
            next: None,
            content: ChunkContent::Gap(()),
        };

        let mut linked_chunk = LinkedChunk::<2, char, ()>::new();

        let result = insert_new_first_chunk(&mut linked_chunk, new_first_chunk);

        assert_matches!(result, Err(LazyLoaderError::MissingNextChunk { id }) => {
            assert_eq!(id, 0);
        });
    }

    #[test]
    fn test_insert_new_first_chunk_err_cannot_connect_two_chunks() {
        let new_first_chunk = RawChunk {
            previous: None,
            identifier: ChunkIdentifier::new(1),
            next: Some(ChunkIdentifier::new(42)),
            content: ChunkContent::Gap(()),
        };

        let mut linked_chunk = LinkedChunk::<2, char, ()>::new();
        linked_chunk.push_gap_back(());

        let result = insert_new_first_chunk(&mut linked_chunk, new_first_chunk);

        assert_matches!(result, Err(LazyLoaderError::CannotConnectTwoChunks { new_chunk, with_chunk }) => {
            assert_eq!(new_chunk, 1);
            assert_eq!(with_chunk, 0);
        });
    }

    #[test]
    fn test_insert_new_first_chunk_err_cannot_connect_two_chunks_before_no_lazy_previous() {
        let new_first_chunk = RawChunk {
            previous: None,
            identifier: ChunkIdentifier::new(1),
            next: Some(ChunkIdentifier::new(0)),
            content: ChunkContent::Gap(()),
        };

        let mut linked_chunk = LinkedChunk::<2, char, ()>::new();
        linked_chunk.push_gap_back(());

        let result = insert_new_first_chunk(&mut linked_chunk, new_first_chunk);

        assert_matches!(result, Err(LazyLoaderError::CannotConnectTwoChunks { new_chunk, with_chunk }) => {
            assert_eq!(new_chunk, 0);
            assert_eq!(with_chunk, 1);
        });
    }

    #[test]
    fn test_insert_new_first_chunk_gap() {
        let new_first_chunk = RawChunk {
            previous: None,
            identifier: ChunkIdentifier::new(1),
            next: Some(ChunkIdentifier::new(0)),
            content: ChunkContent::Gap(()),
        };

        let mut linked_chunk = LinkedChunk::<5, char, ()>::new_with_update_history();
        linked_chunk.push_items_back(vec!['a', 'b']);
        linked_chunk.links.first_chunk_mut().lazy_previous = Some(ChunkIdentifier::new(1));

        // Drain initial updates.
        let _ = linked_chunk.updates().unwrap().take();

        insert_new_first_chunk(&mut linked_chunk, new_first_chunk).unwrap();

        // Iterate forwards to ensure forwards links are okay.
        {
            let mut chunks = linked_chunk.chunks();

            assert_matches!(chunks.next(), Some(chunk) => {
                assert_eq!(chunk.identifier(), 1);
                assert!(chunk.is_gap());
            });
            assert_matches!(chunks.next(), Some(chunk) => {
                assert_eq!(chunk.identifier(), 0);
                assert!(chunk.is_items());
            });
            assert!(chunks.next().is_none());
        }

        // Iterate backwards to ensure backwards links are okay.
        {
            let mut rchunks = linked_chunk.rchunks();

            assert_eq!(rchunks.next().unwrap().identifier(), 0);
            assert_eq!(rchunks.next().unwrap().identifier(), 1);
            assert!(rchunks.next().is_none());
        }

        // Check updates.
        {
            let updates = linked_chunk.updates().unwrap().take();

            assert_eq!(updates.len(), 1);
            assert_eq!(
                updates,
                [Update::NewGapChunk {
                    previous: None,
                    new: ChunkIdentifier::new(1),
                    next: Some(ChunkIdentifier::new(0)),
                    gap: (),
                }]
            );
        }
    }

    #[test]
    fn test_insert_new_first_chunk_items() {
        let new_first_chunk = RawChunk {
            previous: None,
            identifier: ChunkIdentifier::new(1),
            next: Some(ChunkIdentifier::new(0)),
            content: ChunkContent::Items(vec!['c', 'd']),
        };

        let mut linked_chunk = LinkedChunk::<5, char, ()>::new_with_update_history();
        linked_chunk.push_items_back(vec!['a', 'b']);
        linked_chunk.links.first_chunk_mut().lazy_previous = Some(ChunkIdentifier::new(1));

        // Drain initial updates.
        let _ = linked_chunk.updates().unwrap().take();

        insert_new_first_chunk(&mut linked_chunk, new_first_chunk).unwrap();

        // Iterate forwards to ensure forwards links are okay.
        {
            let mut chunks = linked_chunk.chunks();

            assert_matches!(chunks.next(), Some(chunk) => {
                assert_eq!(chunk.identifier(), 1);
                assert!(chunk.is_items());
            });
            assert_matches!(chunks.next(), Some(chunk) => {
                assert_eq!(chunk.identifier(), 0);
                assert!(chunk.is_items());
            });
            assert!(chunks.next().is_none());
        }

        // Iterate backwards to ensure backwards links are okay.
        {
            let mut rchunks = linked_chunk.rchunks();

            assert_eq!(rchunks.next().unwrap().identifier(), 0);
            assert_eq!(rchunks.next().unwrap().identifier(), 1);
            assert!(rchunks.next().is_none());
        }

        // Check updates.
        {
            let updates = linked_chunk.updates().unwrap().take();

            assert_eq!(updates.len(), 2);
            assert_eq!(
                updates,
                [
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(1),
                        next: Some(ChunkIdentifier::new(0)),
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: vec!['c', 'd']
                    }
                ]
            );
        }
    }

    #[test]
    fn test_replace_with_chunk_too_large() {
        // Start with a linked chunk with 3 chunks: one item, one gap, one item.
        let mut linked_chunk = LinkedChunk::<2, char, ()>::new();
        linked_chunk.push_items_back(vec!['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(vec!['c', 'd']);

        // Try to replace it with a last chunk that has too many items.
        let chunk_identifier_generator = ChunkIdentifierGenerator::new_from_scratch();

        let chunk_id = ChunkIdentifier::new(1);
        let raw_chunk = RawChunk {
            previous: Some(ChunkIdentifier::new(0)),
            identifier: chunk_id,
            next: None,
            content: ChunkContent::Items(vec!['e', 'f', 'g', 'h']),
        };

        let err = replace_with(&mut linked_chunk, Some(raw_chunk), chunk_identifier_generator)
            .unwrap_err();
        assert_matches!(err, LazyLoaderError::ChunkTooLarge { id } => {
            assert_eq!(chunk_id, id);
        });
    }

    #[test]
    fn test_replace_with_next_chunk() {
        // Start with a linked chunk with 3 chunks: one item, one gap, one item.
        let mut linked_chunk = LinkedChunk::<2, char, ()>::new();
        linked_chunk.push_items_back(vec!['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(vec!['c', 'd']);

        // Try to replace it with a last chunk that has too many items.
        let chunk_identifier_generator = ChunkIdentifierGenerator::new_from_scratch();

        let chunk_id = ChunkIdentifier::new(1);
        let raw_chunk = RawChunk {
            previous: Some(ChunkIdentifier::new(0)),
            identifier: chunk_id,
            next: Some(ChunkIdentifier::new(2)),
            content: ChunkContent::Items(vec!['e', 'f']),
        };

        let err = replace_with(&mut linked_chunk, Some(raw_chunk), chunk_identifier_generator)
            .unwrap_err();
        assert_matches!(err, LazyLoaderError::ChunkIsNotLast { id } => {
            assert_eq!(chunk_id, id);
        });
    }

    #[test]
    fn test_replace_with_empty() {
        // Start with a linked chunk with 3 chunks: one item, one gap, one item.
        let mut linked_chunk = LinkedChunk::<2, char, ()>::new_with_update_history();
        linked_chunk.push_items_back(vec!['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(vec!['c', 'd']);

        // Drain initial updates.
        let _ = linked_chunk.updates().unwrap().take();

        // Replace it with… you know, nothing (jon snow).
        let chunk_identifier_generator =
            ChunkIdentifierGenerator::new_from_previous_chunk_identifier(
                ChunkIdentifierGenerator::FIRST_IDENTIFIER,
            );
        replace_with(&mut linked_chunk, None, chunk_identifier_generator).unwrap();

        // The linked chunk still has updates enabled.
        assert!(linked_chunk.updates().is_some());

        // Check the linked chunk only contains the default empty events chunk.
        let mut it = linked_chunk.chunks();

        assert_matches!(it.next(), Some(chunk) => {
            assert_eq!(chunk.identifier(), ChunkIdentifier::new(0));
            assert!(chunk.is_items());
            assert!(chunk.next().is_none());
            assert_matches!(chunk.content(), ChunkContent::Items(items) => {
                assert!(items.is_empty());
            });
        });

        // And there's no other chunk.
        assert_matches!(it.next(), None);

        // Check updates.
        {
            let updates = linked_chunk.updates().unwrap().take();

            assert_eq!(updates.len(), 2);
            assert_eq!(
                updates,
                [
                    Update::Clear,
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                ]
            );
        }
    }

    #[test]
    fn test_replace_with_non_empty() {
        // Start with a linked chunk with 3 chunks: one item, one gap, one item.
        let mut linked_chunk = LinkedChunk::<2, char, ()>::new_with_update_history();
        linked_chunk.push_items_back(vec!['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(vec!['c', 'd']);

        // Drain initial updates.
        let _ = linked_chunk.updates().unwrap().take();

        // Replace it with a single chunk (sorry, jon).
        let chunk_identifier_generator =
            ChunkIdentifierGenerator::new_from_previous_chunk_identifier(ChunkIdentifier::new(42));

        let chunk_id = ChunkIdentifier::new(1);
        let chunk = RawChunk {
            previous: Some(ChunkIdentifier::new(0)),
            identifier: chunk_id,
            next: None,
            content: ChunkContent::Items(vec!['e', 'f']),
        };
        replace_with(&mut linked_chunk, Some(chunk), chunk_identifier_generator).unwrap();

        // The linked chunk still has updates enabled.
        assert!(linked_chunk.updates().is_some());

        let mut it = linked_chunk.chunks();

        // The first chunk is an event chunks with the expected items.
        assert_matches!(it.next(), Some(chunk) => {
            assert_eq!(chunk.identifier(), chunk_id);
            assert!(chunk.next().is_none());
            assert_matches!(chunk.content(), ChunkContent::Items(items) => {
                assert_eq!(*items, vec!['e', 'f']);
            });
        });

        // Nothing more.
        assert!(it.next().is_none());

        // Check updates.
        {
            let updates = linked_chunk.updates().unwrap().take();

            assert_eq!(updates.len(), 3);
            assert_eq!(
                updates,
                [
                    Update::Clear,
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(0)),
                        new: chunk_id,
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: vec!['e', 'f']
                    }
                ]
            );
        }
    }

    #[test]
    fn test_from_all_chunks_empty() {
        // Building an empty linked chunk works, and returns `None`.
        let lc = from_all_chunks::<3, char, ()>(vec![]).unwrap();
        assert!(lc.is_none());
    }

    #[test]
    fn test_from_all_chunks_success() {
        let cid0 = ChunkIdentifier::new(0);
        let cid1 = ChunkIdentifier::new(1);
        // Note: cid2 is missing on purpose, to confirm that it's fine to have holes in
        // the chunk id space.
        let cid3 = ChunkIdentifier::new(3);

        // Check that we can successfully create a linked chunk, independently of the
        // order in which chunks are added.
        //
        // The final chunk will contain [cid0 <-> cid1 <-> cid3], in this order.

        let chunks = vec![
            // Adding chunk cid0.
            RawChunk {
                previous: None,
                identifier: cid0,
                next: Some(cid1),
                content: ChunkContent::Items(vec!['a', 'b', 'c']),
            },
            // Adding chunk cid3.
            RawChunk {
                previous: Some(cid1),
                identifier: cid3,
                next: None,
                content: ChunkContent::Items(vec!['d', 'e']),
            },
            // Adding chunk cid1.
            RawChunk {
                previous: Some(cid0),
                identifier: cid1,
                next: Some(cid3),
                content: ChunkContent::Gap('g'),
            },
        ];

        let mut lc = from_all_chunks::<3, _, _>(chunks)
            .expect("building works")
            .expect("returns a non-empty linked chunk");

        // Check the entire content first.
        assert_items_eq!(lc, ['a', 'b', 'c'] [-] ['d', 'e']);

        // Run checks on the first chunk.
        let mut chunks = lc.chunks();
        let first_chunk = chunks.next().unwrap();
        {
            assert!(first_chunk.previous().is_none());
            assert_eq!(first_chunk.identifier(), cid0);
        }

        // Run checks on the second chunk.
        let second_chunk = chunks.next().unwrap();
        {
            assert_eq!(second_chunk.identifier(), first_chunk.next().unwrap().identifier());
            assert_eq!(second_chunk.previous().unwrap().identifier(), first_chunk.identifier());
            assert_eq!(second_chunk.identifier(), cid1);
        }

        // Run checks on the third chunk.
        let third_chunk = chunks.next().unwrap();
        {
            assert_eq!(third_chunk.identifier(), second_chunk.next().unwrap().identifier());
            assert_eq!(third_chunk.previous().unwrap().identifier(), second_chunk.identifier());
            assert!(third_chunk.next().is_none());
            assert_eq!(third_chunk.identifier(), cid3);
        }

        // There's no more chunk.
        assert!(chunks.next().is_none());

        // The linked chunk had 5 items.
        assert_eq!(lc.num_items(), 5);

        // Now, if we add a new chunk, its identifier should be the previous one we used
        // + 1.
        lc.push_gap_back('h');

        let last_chunk = lc.chunks().last().unwrap();
        assert_eq!(last_chunk.identifier(), ChunkIdentifier::new(cid3.index() + 1));
    }

    #[test]
    fn test_from_all_chunks_chunk_too_large() {
        let cid0 = ChunkIdentifier::new(0);

        // Adding a chunk with 4 items will fail, because the max capacity specified in
        // the builder generics is 3.
        let res = from_all_chunks::<3, char, ()>(vec![RawChunk {
            previous: None,
            identifier: cid0,
            next: None,
            content: ChunkContent::Items(vec!['a', 'b', 'c', 'd']),
        }]);
        assert_matches!(res, Err(LazyLoaderError::ChunkTooLarge { id }) => {
            assert_eq!(id, cid0);
        });
    }

    #[test]
    fn test_from_all_chunks_missing_first_chunk() {
        let cid0 = ChunkIdentifier::new(0);
        let cid1 = ChunkIdentifier::new(1);
        let cid2 = ChunkIdentifier::new(2);

        let res = from_all_chunks::<3, char, char>(vec![
            RawChunk {
                previous: Some(cid2),
                identifier: cid0,
                next: Some(cid1),
                content: ChunkContent::Gap('g'),
            },
            RawChunk {
                previous: Some(cid0),
                identifier: cid1,
                next: None,
                content: ChunkContent::Items(vec!['a', 'b', 'c']),
            },
        ]);
        assert_matches!(res, Err(LazyLoaderError::ChunkIsNotFirst { id }) => {
            assert_eq!(id, cid0);
        });
    }

    #[test]
    fn test_from_all_chunks_multiple_first_chunks() {
        let cid0 = ChunkIdentifier::new(0);
        let cid1 = ChunkIdentifier::new(1);

        let res = from_all_chunks::<3, char, char>(vec![
            RawChunk {
                previous: None,
                identifier: cid0,
                next: None,
                content: ChunkContent::Gap('g'),
            },
            // Second chunk lies and pretends to be the first too.
            RawChunk {
                previous: None,
                identifier: cid1,
                next: None,
                content: ChunkContent::Gap('G'),
            },
        ]);

        assert_matches!(res, Err(LazyLoaderError::MultipleConnectedComponents));
    }

    #[test]
    fn test_from_all_chunks_cycle() {
        let cid0 = ChunkIdentifier::new(0);
        let cid1 = ChunkIdentifier::new(1);

        let res = from_all_chunks::<3, char, char>(vec![
            RawChunk {
                previous: None,
                identifier: cid0,
                next: None,
                content: ChunkContent::Gap('g'),
            },
            RawChunk {
                previous: Some(cid0),
                identifier: cid1,
                next: Some(cid0),
                content: ChunkContent::Gap('G'),
            },
        ]);

        assert_matches!(res, Err(LazyLoaderError::Cycle { new_chunk, with_chunk }) => {
            assert_eq!(new_chunk, cid1);
            assert_eq!(with_chunk, cid0);
        });
    }

    #[test]
    fn test_from_all_chunks_multiple_connected_components() {
        let cid0 = ChunkIdentifier::new(0);
        let cid1 = ChunkIdentifier::new(1);
        let cid2 = ChunkIdentifier::new(2);

        let res = from_all_chunks::<3, char, char>(vec![
            // cid0 and cid1 are linked to each other.
            RawChunk {
                previous: None,
                identifier: cid0,
                next: Some(cid1),
                content: ChunkContent::Gap('g'),
            },
            RawChunk {
                previous: Some(cid0),
                identifier: cid1,
                next: None,
                content: ChunkContent::Gap('G'),
            },
            // cid2 stands on its own.
            RawChunk {
                previous: None,
                identifier: cid2,
                next: None,
                content: ChunkContent::Gap('h'),
            },
        ]);

        assert_matches!(res, Err(LazyLoaderError::MultipleConnectedComponents));
    }
}
