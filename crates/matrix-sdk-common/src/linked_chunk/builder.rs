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

use std::{
    collections::{BTreeMap, HashSet},
    marker::PhantomData,
};

use tracing::error;

use super::{
    Chunk, ChunkContent, ChunkIdentifier, ChunkIdentifierGenerator, Ends, LinkedChunk,
    ObservableUpdates, RawChunk, Update,
};

/// A reverse iterative builder for a [`LinkedChunk`].
///
/// This type helps to build a [`LinkedChunk`] iteratively: start with
/// [`Self::from_last_chunk`] to build a `LinkedChunk` with a single, last
/// chunk, then continue with [`Self::insert_new_first_chunk`] to insert a chunk
/// in the front of a `LinkedChunk`.
#[derive(Debug)]
pub struct LinkedChunkBuilder;

impl LinkedChunkBuilder {
    /// Build a new `LinkedChunk` with a single chunk that is supposed to be the
    /// last one.
    pub fn from_last_chunk<const CAP: usize, Item, Gap>(
        chunk: Option<RawChunk<Item, Gap>>,
        chunk_identifier_generator: ChunkIdentifierGenerator,
    ) -> Result<Option<LinkedChunk<CAP, Item, Gap>>, LinkedChunkBuilderError> {
        let Some(mut chunk) = chunk else {
            return Ok(None);
        };

        // Check consistency before creating the `LinkedChunk`.
        {
            // The number of items is not too large.
            if let ChunkContent::Items(items) = &chunk.content {
                if items.len() > CAP {
                    return Err(LinkedChunkBuilderError::ChunkTooLarge { id: chunk.identifier });
                }
            }

            // Chunk has no next chunk.
            if chunk.next.is_some() {
                return Err(LinkedChunkBuilderError::ChunkIsNotLast { id: chunk.identifier });
            }
        }

        // Create the `LinkedChunk` from a single chunk.
        {
            // This is the only chunk. Pretend it has no previous chunk if any.
            chunk.previous = None;

            // Transform the `RawChunk` into a `Chunk`.
            let chunk_ptr = Chunk::new_leaked(chunk.identifier, chunk.content);

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
        new_first_chunk: RawChunk<Item, Gap>,
    ) -> Result<&Chunk<CAP, Item, Gap>, LinkedChunkBuilderError>
    where
        Item: Clone,
        Gap: Clone,
    {
        // Check `LinkedChunk` is going to be consistent after the insertion.
        {
            // The number of items is not too large.
            if let ChunkContent::Items(items) = &new_first_chunk.content {
                if items.len() > CAP {
                    return Err(LinkedChunkBuilderError::ChunkTooLarge {
                        id: new_first_chunk.identifier,
                    });
                }
            }

            // New chunk has no previous chunk.
            if new_first_chunk.previous.is_some() {
                return Err(LinkedChunkBuilderError::ChunkIsNotFirst {
                    id: new_first_chunk.identifier,
                });
            }

            let expected_next_chunk = linked_chunk.links.first_chunk().identifier();

            // New chunk has a next chunk.
            let Some(next_chunk) = new_first_chunk.next else {
                return Err(LinkedChunkBuilderError::MissingNextChunk {
                    id: new_first_chunk.identifier,
                });
            };

            // New chunk has a next chunk, and it is the first chunk of the `LinkedChunk`.
            if next_chunk != expected_next_chunk {
                return Err(LinkedChunkBuilderError::CannotConnectTwoChunks {
                    new_chunk: new_first_chunk.identifier,
                    with_chunk: expected_next_chunk,
                });
            }

            // Alright. It's not possible to have a cycle within the chunks or
            // multiple connected components here. All checks are made.
        }

        // Insert the new first chunk.
        {
            // Transform the `RawChunk` into a `Chunk`.
            let new_first_chunk =
                Chunk::new_leaked(new_first_chunk.identifier, new_first_chunk.content);

            let links = &mut linked_chunk.links;

            debug_assert!(
                links.first_chunk().previous.is_none(),
                "The first chunk is supposed to not have a previous chunk"
            );

            // Link one way: `new_first_chunk` becomes the previous chunk of the first
            // chunk.
            links.first_chunk_mut().previous = Some(new_first_chunk);

            // Remember the pointer to the `first_chunk`.
            let old_first_chunk = links.first;

            // `new_first_chunk` becomes the new first chunk.
            links.first = new_first_chunk;

            // Link the other way: `old_first_chunk` becomes the next chunk of the first
            // chunk.
            links.first_chunk_mut().next = Some(old_first_chunk);

            debug_assert!(
                links.first_chunk().previous.is_none(),
                "The new first chunk is supposed to not have a previous chunk"
            );

            // Update the last chunk. If it's `Some(_)`, no need to update the last chunk
            // pointer. If it's `None`, it means we had only one chunk; now we have two, the
            // last chunk is the `old_first_chunk`.
            if links.last.is_none() {
                links.last = Some(old_first_chunk);
            }
        }

        // Emit the updates.
        if let Some(updates) = linked_chunk.updates.as_mut() {
            let first_chunk = linked_chunk.links.first_chunk();

            let previous = first_chunk.previous().map(Chunk::identifier);
            let new = first_chunk.identifier();
            let next = first_chunk.next().map(Chunk::identifier);

            match first_chunk.content() {
                ChunkContent::Gap(gap) => {
                    updates.push(Update::NewGapChunk { previous, new, next, gap: gap.clone() });
                }

                ChunkContent::Items(items) => {
                    updates.push(Update::NewItemsChunk { previous, new, next });
                    updates.push(Update::PushItems {
                        at: first_chunk.first_position(),
                        items: items.clone(),
                    });
                }
            }
        }

        Ok(linked_chunk.links.first_chunk())
    }
}

#[derive(thiserror::Error, Debug)]
pub enum LinkedChunkBuilderError {
    #[error("chunk with id {} has a previous chunk, it is supposed to be the first chunk", id.index())]
    ChunkIsNotFirst { id: ChunkIdentifier },

    #[error("chunk with id {} has a next chunk, it is supposed to be the last chunk", id.index())]
    ChunkIsNotLast { id: ChunkIdentifier },

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
}

#[cfg(test)]
mod tests {

    use assert_matches::assert_matches;

    use super::{
        ChunkContent, ChunkIdentifier, ChunkIdentifierGenerator, LinkedChunk, LinkedChunkBuilder,
        LinkedChunkBuilderError, RawChunk, Update,
    };
    use crate::linked_chunk::Position;

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

        let maybe_linked_chunk = LinkedChunkBuilder::from_last_chunk::<2, char, ()>(
            Some(last_chunk),
            chunk_identifier_generator,
        );

        assert_matches!(
            maybe_linked_chunk,
            Err(LinkedChunkBuilderError::ChunkTooLarge { id }) => {
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

        let maybe_linked_chunk = LinkedChunkBuilder::from_last_chunk::<2, char, ()>(
            Some(last_chunk),
            chunk_identifier_generator,
        );

        assert_matches!(
            maybe_linked_chunk,
            Err(LinkedChunkBuilderError::ChunkIsNotLast { id }) => {
                assert_eq!(id, 0);
            }
        );
    }

    #[test]
    fn test_from_last_chunk_none() {
        let chunk_identifier_generator =
            ChunkIdentifierGenerator::new_from_previous_chunk_identifier(ChunkIdentifier::new(0));

        let maybe_linked_chunk =
            LinkedChunkBuilder::from_last_chunk::<2, char, ()>(None, chunk_identifier_generator)
                .unwrap();

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

        let maybe_linked_chunk = LinkedChunkBuilder::from_last_chunk::<2, char, ()>(
            Some(last_chunk),
            chunk_identifier_generator,
        )
        .unwrap();

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

        let result = LinkedChunkBuilder::insert_new_first_chunk(&mut linked_chunk, new_first_chunk);

        assert_matches!(result, Err(LinkedChunkBuilderError::ChunkTooLarge { id }) => {
            assert_eq!(id, 0);
        });
    }

    #[test]
    fn test_insert_new_first_chunk_err_is_not_first() {
        let new_first_chunk = RawChunk {
            previous: Some(ChunkIdentifier::new(42)),
            identifier: ChunkIdentifier::new(0),
            next: None,
            content: ChunkContent::Gap(()),
        };

        let mut linked_chunk = LinkedChunk::<2, char, ()>::new();

        let result = LinkedChunkBuilder::insert_new_first_chunk(&mut linked_chunk, new_first_chunk);

        assert_matches!(result, Err(LinkedChunkBuilderError::ChunkIsNotFirst { id }) => {
            assert_eq!(id, 0);
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

        let result = LinkedChunkBuilder::insert_new_first_chunk(&mut linked_chunk, new_first_chunk);

        assert_matches!(result, Err(LinkedChunkBuilderError::MissingNextChunk { id }) => {
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

        let result = LinkedChunkBuilder::insert_new_first_chunk(&mut linked_chunk, new_first_chunk);

        assert_matches!(result, Err(LinkedChunkBuilderError::CannotConnectTwoChunks { new_chunk, with_chunk }) => {
            assert_eq!(new_chunk, 1);
            assert_eq!(with_chunk, 0);
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

        // Drain initial updates.
        let _ = linked_chunk.updates().unwrap().take();

        LinkedChunkBuilder::insert_new_first_chunk(&mut linked_chunk, new_first_chunk).unwrap();

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

        // Drain initial updates.
        let _ = linked_chunk.updates().unwrap().take();

        LinkedChunkBuilder::insert_new_first_chunk(&mut linked_chunk, new_first_chunk).unwrap();

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
}

/// A temporary chunk representation in the [`LinkedChunkBuilderTest`].
///
/// Instead of using linking the chunks with pointers, this uses
/// [`ChunkIdentifier`] as the temporary links to the previous and next chunks,
/// which will get resolved later when re-building the full data structure. This
/// allows using chunks that references other chunks that aren't known yet.
struct TemporaryChunk<Item, Gap> {
    previous: Option<ChunkIdentifier>,
    next: Option<ChunkIdentifier>,
    content: ChunkContent<Item, Gap>,
}

/// A data structure to rebuild a linked chunk from its raw representation.
///
/// A linked chunk can be rebuilt incrementally from its internal
/// representation, with the chunks being added *in any order*, as long as they
/// form a single connected component eventually (viz., there's no
/// subgraphs/sublists isolated from the one final linked list). If they don't,
/// then the final call to [`LinkedChunkBuilder::build()`] will result in an
/// error).
#[allow(missing_debug_implementations)]
#[doc(hidden)]
pub struct LinkedChunkBuilderTest<const CAP: usize, Item, Gap> {
    /// Work-in-progress chunks.
    chunks: BTreeMap<ChunkIdentifier, TemporaryChunk<Item, Gap>>,
}

impl<const CAP: usize, Item, Gap> Default for LinkedChunkBuilderTest<CAP, Item, Gap> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CAP: usize, Item, Gap> LinkedChunkBuilderTest<CAP, Item, Gap> {
    /// Create an empty [`LinkedChunkBuilder`] with no update history.
    pub fn new() -> Self {
        Self { chunks: Default::default() }
    }

    /// Stash a gap chunk with its content.
    ///
    /// This can be called even if the previous and next chunks have not been
    /// added yet. Resolving these chunks will happen at the time of calling
    /// [`LinkedChunkBuilder::build()`].
    fn push_gap(
        &mut self,
        previous: Option<ChunkIdentifier>,
        id: ChunkIdentifier,
        next: Option<ChunkIdentifier>,
        content: Gap,
    ) {
        let chunk = TemporaryChunk { previous, next, content: ChunkContent::Gap(content) };
        self.chunks.insert(id, chunk);
    }

    /// Stash an item chunk with its contents.
    ///
    /// This can be called even if the previous and next chunks have not been
    /// added yet. Resolving these chunks will happen at the time of calling
    /// [`LinkedChunkBuilder::build()`].
    fn push_items(
        &mut self,
        previous: Option<ChunkIdentifier>,
        id: ChunkIdentifier,
        next: Option<ChunkIdentifier>,
        items: impl IntoIterator<Item = Item>,
    ) {
        let chunk = TemporaryChunk {
            previous,
            next,
            content: ChunkContent::Items(items.into_iter().collect()),
        };
        self.chunks.insert(id, chunk);
    }

    /// Run all error checks before reconstructing the full linked chunk.
    ///
    /// Must be called after checking `self.chunks` isn't empty in
    /// [`Self::build`].
    ///
    /// Returns the identifier of the first chunk.
    fn check_consistency(&mut self) -> Result<ChunkIdentifier, LinkedChunkBuilderTestError> {
        // Look for the first id.
        let first_id =
            self.chunks.iter().find_map(|(id, chunk)| chunk.previous.is_none().then_some(*id));

        // There's no first chunk, but we've checked that `self.chunks` isn't empty:
        // it's a malformed list.
        let Some(first_id) = first_id else {
            return Err(LinkedChunkBuilderTestError::MissingFirstChunk);
        };

        // We're going to iterate from the first to the last chunk.
        // Keep track of chunks we've already visited.
        let mut visited = HashSet::new();

        // Start from the first chunk.
        let mut maybe_cur = Some(first_id);

        while let Some(cur) = maybe_cur {
            // The chunk must be referenced in `self.chunks`.
            let Some(chunk) = self.chunks.get(&cur) else {
                return Err(LinkedChunkBuilderTestError::MissingChunk { id: cur });
            };

            if let ChunkContent::Items(items) = &chunk.content {
                if items.len() > CAP {
                    return Err(LinkedChunkBuilderTestError::ChunkTooLarge { id: cur });
                }
            }

            // If it's not the first chunk,
            if cur != first_id {
                // It must have a previous link.
                let Some(prev) = chunk.previous else {
                    return Err(LinkedChunkBuilderTestError::MultipleFirstChunks {
                        first_candidate: first_id,
                        second_candidate: cur,
                    });
                };

                // And we must have visited its predecessor at this point, since we've
                // iterated from the first chunk.
                if !visited.contains(&prev) {
                    return Err(LinkedChunkBuilderTestError::MissingChunk { id: prev });
                }
            }

            // Add the current chunk to the list of seen chunks.
            if !visited.insert(cur) {
                // If we didn't insert, then it was already visited: there's a cycle!
                return Err(LinkedChunkBuilderTestError::Cycle { repeated: cur });
            }

            // Move on to the next chunk. If it's none, we'll quit the loop.
            maybe_cur = chunk.next;
        }

        // If there are more chunks than those we've visited: some of them were not
        // linked to the "main" branch of the linked list, so we had multiple connected
        // components.
        if visited.len() != self.chunks.len() {
            return Err(LinkedChunkBuilderTestError::MultipleConnectedComponents);
        }

        Ok(first_id)
    }

    pub fn build(
        mut self,
    ) -> Result<Option<LinkedChunk<CAP, Item, Gap>>, LinkedChunkBuilderTestError> {
        if self.chunks.is_empty() {
            return Ok(None);
        }

        // Run checks.
        let first_id = self.check_consistency()?;

        // We're now going to iterate from the first to the last chunk. As we're doing
        // this, we're also doing a few other things:
        //
        // - rebuilding the final `Chunk`s one by one, that will be linked using
        //   pointers,
        // - counting items from the item chunks we'll encounter,
        // - finding the max `ChunkIdentifier` (`max_chunk_id`).

        let mut max_chunk_id = first_id.index();

        // Small helper to graduate a temporary chunk into a final one. As we're doing
        // this, we're also updating the maximum chunk id (that will be used to
        // set up the id generator), and the number of items in this chunk.

        let mut graduate_chunk = |id: ChunkIdentifier| {
            let temp = self.chunks.remove(&id)?;

            // Update the maximum chunk identifier, while we're around.
            max_chunk_id = max_chunk_id.max(id.index());

            // Graduate the current temporary chunk into a final chunk.
            let chunk_ptr = Chunk::new_leaked(id, temp.content);

            Some((temp.next, chunk_ptr))
        };

        let Some((mut next_chunk_id, first_chunk_ptr)) = graduate_chunk(first_id) else {
            // Can't really happen, but oh well.
            return Err(LinkedChunkBuilderTestError::MissingFirstChunk);
        };

        let mut prev_chunk_ptr = first_chunk_ptr;

        while let Some(id) = next_chunk_id {
            let Some((new_next, mut chunk_ptr)) = graduate_chunk(id) else {
                // Can't really happen, but oh well.
                return Err(LinkedChunkBuilderTestError::MissingChunk { id });
            };

            let chunk = unsafe { chunk_ptr.as_mut() };

            // Link the current chunk to its previous one.
            let prev_chunk = unsafe { prev_chunk_ptr.as_mut() };
            prev_chunk.next = Some(chunk_ptr);
            chunk.previous = Some(prev_chunk_ptr);

            // Prepare for the next iteration.
            prev_chunk_ptr = chunk_ptr;
            next_chunk_id = new_next;
        }

        debug_assert!(self.chunks.is_empty());

        // Maintain the convention that `Ends::last` may be unset.
        let last_chunk_ptr = prev_chunk_ptr;
        let last_chunk_ptr =
            if first_chunk_ptr == last_chunk_ptr { None } else { Some(last_chunk_ptr) };
        let links = Ends { first: first_chunk_ptr, last: last_chunk_ptr };

        let chunk_identifier_generator =
            ChunkIdentifierGenerator::new_from_previous_chunk_identifier(ChunkIdentifier::new(
                max_chunk_id,
            ));

        let updates = Some(ObservableUpdates::new());

        Ok(Some(LinkedChunk { links, chunk_identifier_generator, updates, marker: PhantomData }))
    }

    /// Fills a linked chunk builder from all the given raw parts.
    pub fn from_raw_parts(raws: Vec<RawChunk<Item, Gap>>) -> Self {
        let mut this = Self::new();
        for raw in raws {
            match raw.content {
                ChunkContent::Gap(gap) => {
                    this.push_gap(raw.previous, raw.identifier, raw.next, gap);
                }
                ChunkContent::Items(vec) => {
                    this.push_items(raw.previous, raw.identifier, raw.next, vec);
                }
            }
        }
        this
    }
}

#[doc(hidden)]
#[derive(thiserror::Error, Debug)]
pub enum LinkedChunkBuilderTestError {
    #[error("chunk with id {} is too large", id.index())]
    ChunkTooLarge { id: ChunkIdentifier },

    #[error("there's no first chunk")]
    MissingFirstChunk,

    #[error("there are multiple first chunks")]
    MultipleFirstChunks { first_candidate: ChunkIdentifier, second_candidate: ChunkIdentifier },

    #[error("unable to resolve chunk with id {}", id.index())]
    MissingChunk { id: ChunkIdentifier },

    #[error("rebuilt chunks form a cycle: repeated identifier: {}", repeated.index())]
    Cycle { repeated: ChunkIdentifier },

    #[error("multiple connected components")]
    MultipleConnectedComponents,
}

#[cfg(test)]
mod linked_builder_test_tests {
    use assert_matches::assert_matches;

    use super::{ChunkIdentifier, LinkedChunkBuilderTest, LinkedChunkBuilderTestError};

    #[test]
    fn test_empty() {
        let lcb = LinkedChunkBuilderTest::<3, char, char>::new();

        // Building an empty linked chunk works, and returns `None`.
        let lc = lcb.build().unwrap();
        assert!(lc.is_none());
    }

    #[test]
    fn test_success() {
        let mut lcb = LinkedChunkBuilderTest::<3, char, char>::new();

        let cid0 = ChunkIdentifier::new(0);
        let cid1 = ChunkIdentifier::new(1);
        // Note: cid2 is missing on purpose, to confirm that it's fine to have holes in
        // the chunk id space.
        let cid3 = ChunkIdentifier::new(3);

        // Check that we can successfully create a linked chunk, independently of the
        // order in which chunks are added.
        //
        // The final chunk will contain [cid0 <-> cid1 <-> cid3], in this order.

        // Adding chunk cid0.
        lcb.push_items(None, cid0, Some(cid1), vec!['a', 'b', 'c']);
        // Adding chunk cid3.
        lcb.push_items(Some(cid1), cid3, None, vec!['d', 'e']);
        // Adding chunk cid1.
        lcb.push_gap(Some(cid0), cid1, Some(cid3), 'g');

        let mut lc =
            lcb.build().expect("building works").expect("returns a non-empty linked chunk");

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
    fn test_chunk_too_large() {
        let mut lcb = LinkedChunkBuilderTest::<3, char, char>::new();

        let cid0 = ChunkIdentifier::new(0);

        // Adding a chunk with 4 items will fail, because the max capacity specified in
        // the builder generics is 3.
        lcb.push_items(None, cid0, None, vec!['a', 'b', 'c', 'd']);

        let res = lcb.build();
        assert_matches!(res, Err(LinkedChunkBuilderTestError::ChunkTooLarge { id }) => {
            assert_eq!(id, cid0);
        });
    }

    #[test]
    fn test_missing_first_chunk() {
        let mut lcb = LinkedChunkBuilderTest::<3, char, char>::new();

        let cid0 = ChunkIdentifier::new(0);
        let cid1 = ChunkIdentifier::new(1);
        let cid2 = ChunkIdentifier::new(2);

        lcb.push_gap(Some(cid2), cid0, Some(cid1), 'g');
        lcb.push_items(Some(cid0), cid1, Some(cid2), ['a', 'b', 'c']);
        lcb.push_items(Some(cid1), cid2, Some(cid0), ['d', 'e', 'f']);

        let res = lcb.build();
        assert_matches!(res, Err(LinkedChunkBuilderTestError::MissingFirstChunk));
    }

    #[test]
    fn test_multiple_first_chunks() {
        let mut lcb = LinkedChunkBuilderTest::<3, char, char>::new();

        let cid0 = ChunkIdentifier::new(0);
        let cid1 = ChunkIdentifier::new(1);

        lcb.push_gap(None, cid0, Some(cid1), 'g');
        // Second chunk lies and pretends to be the first too.
        lcb.push_items(None, cid1, Some(cid0), ['a', 'b', 'c']);

        let res = lcb.build();
        assert_matches!(res, Err(LinkedChunkBuilderTestError::MultipleFirstChunks { first_candidate, second_candidate }) => {
            assert_eq!(first_candidate, cid0);
            assert_eq!(second_candidate, cid1);
        });
    }

    #[test]
    fn test_missing_chunk() {
        let mut lcb = LinkedChunkBuilderTest::<3, char, char>::new();

        let cid0 = ChunkIdentifier::new(0);
        let cid1 = ChunkIdentifier::new(1);
        lcb.push_gap(None, cid0, Some(cid1), 'g');

        let res = lcb.build();
        assert_matches!(res, Err(LinkedChunkBuilderTestError::MissingChunk { id }) => {
            assert_eq!(id, cid1);
        });
    }

    #[test]
    fn test_cycle() {
        let mut lcb = LinkedChunkBuilderTest::<3, char, char>::new();

        let cid0 = ChunkIdentifier::new(0);
        let cid1 = ChunkIdentifier::new(1);
        lcb.push_gap(None, cid0, Some(cid1), 'g');
        lcb.push_gap(Some(cid0), cid1, Some(cid0), 'g');

        let res = lcb.build();
        assert_matches!(res, Err(LinkedChunkBuilderTestError::Cycle { repeated }) => {
            assert_eq!(repeated, cid0);
        });
    }

    #[test]
    fn test_multiple_connected_components() {
        let mut lcb = LinkedChunkBuilderTest::<3, char, char>::new();

        let cid0 = ChunkIdentifier::new(0);
        let cid1 = ChunkIdentifier::new(1);
        let cid2 = ChunkIdentifier::new(2);

        // cid0 and cid1 are linked to each other.
        lcb.push_gap(None, cid0, Some(cid1), 'g');
        lcb.push_items(Some(cid0), cid1, None, ['a', 'b', 'c']);
        // cid2 stands on its own.
        lcb.push_items(None, cid2, None, ['d', 'e', 'f']);

        let res = lcb.build();
        assert_matches!(res, Err(LinkedChunkBuilderTestError::MultipleConnectedComponents));
    }
}
