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
    ObservableUpdates,
};

/// A temporary chunk representation in the [`LinkedChunkBuilder`].
///
/// Instead of using linking the chunks with pointers, this uses
/// [`ChunkIdentifier`] as the temporary links to the previous and next chunks,
/// which will get resolved later when re-building the full data structure. This
/// allows using chunks that references other chunks that aren't known yet.
struct TemporaryChunk<Item, Gap> {
    id: ChunkIdentifier,
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
pub struct LinkedChunkBuilder<const CAP: usize, Item, Gap> {
    /// Work-in-progress chunks.
    chunks: BTreeMap<ChunkIdentifier, TemporaryChunk<Item, Gap>>,

    /// Is the final `LinkedChunk` expected to include an update history, as if
    /// it were created with [`LinkedChunk::new_with_update_history`]?
    build_with_update_history: bool,
}

impl<const CAP: usize, Item, Gap> Default for LinkedChunkBuilder<CAP, Item, Gap> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CAP: usize, Item, Gap> LinkedChunkBuilder<CAP, Item, Gap> {
    /// Create an empty [`LinkedChunkBuilder`] with no update history.
    pub fn new() -> Self {
        Self { chunks: Default::default(), build_with_update_history: false }
    }

    /// Stash a gap chunk with its content.
    ///
    /// This can be called even if the previous and next chunks have not been
    /// added yet. Resolving these chunks will happen at the time of calling
    /// [`LinkedChunkBuilder::build()`].
    pub fn push_gap(
        &mut self,
        previous: Option<ChunkIdentifier>,
        id: ChunkIdentifier,
        next: Option<ChunkIdentifier>,
        content: Gap,
    ) {
        let chunk = TemporaryChunk { id, previous, next, content: ChunkContent::Gap(content) };
        self.chunks.insert(id, chunk);
    }

    /// Stash an item chunk with its contents.
    ///
    /// This can be called even if the previous and next chunks have not been
    /// added yet. Resolving these chunks will happen at the time of calling
    /// [`LinkedChunkBuilder::build()`].
    pub fn push_items(
        &mut self,
        previous: Option<ChunkIdentifier>,
        id: ChunkIdentifier,
        next: Option<ChunkIdentifier>,
        items: impl IntoIterator<Item = Item>,
    ) {
        let chunk = TemporaryChunk {
            id,
            previous,
            next,
            content: ChunkContent::Items(items.into_iter().collect()),
        };
        self.chunks.insert(id, chunk);
    }

    /// Request that the resulting linked chunk will have an update history, as
    /// if it were created with [`LinkedChunk::new_with_update_history`].
    pub fn with_update_history(&mut self) {
        self.build_with_update_history = true;
    }

    /// Run all error checks before reconstructing the full linked chunk.
    ///
    /// Must be called after checking `self.chunks` isn't empty in
    /// [`Self::build`].
    ///
    /// Returns the identifier of the first chunk.
    fn check_consistency(&mut self) -> Result<ChunkIdentifier, LinkedChunkBuilderError> {
        // Look for the first id.
        let first_id =
            self.chunks.iter().find_map(|(id, chunk)| chunk.previous.is_none().then_some(*id));

        // There's no first chunk, but we've checked that `self.chunks` isn't empty:
        // it's a malformed list.
        let Some(first_id) = first_id else {
            return Err(LinkedChunkBuilderError::MissingFirstChunk);
        };

        // We're going to iterate from the first to the last chunk.
        // Keep track of chunks we've already visited.
        let mut visited = HashSet::new();

        // Start from the first chunk.
        let mut maybe_cur = Some(first_id);

        while let Some(cur) = maybe_cur {
            // The chunk must be referenced in `self.chunks`.
            let Some(chunk) = self.chunks.get(&cur) else {
                return Err(LinkedChunkBuilderError::MissingChunk { id: cur });
            };

            if let ChunkContent::Items(items) = &chunk.content {
                if items.len() > CAP {
                    return Err(LinkedChunkBuilderError::ChunkTooLarge { id: cur });
                }
            }

            // If it's not the first chunk,
            if cur != first_id {
                // It must have a previous link.
                let Some(prev) = chunk.previous else {
                    return Err(LinkedChunkBuilderError::MultipleFirstChunks {
                        first_candidate: first_id,
                        second_candidate: cur,
                    });
                };

                // And we must have visited its predecessor at this point, since we've
                // iterated from the first chunk.
                if !visited.contains(&prev) {
                    return Err(LinkedChunkBuilderError::MissingChunk { id: prev });
                }
            }

            // Add the current chunk to the list of seen chunks.
            if !visited.insert(cur) {
                // If we didn't insert, then it was already visited: there's a cycle!
                return Err(LinkedChunkBuilderError::Cycle { repeated: cur });
            }

            // Move on to the next chunk. If it's none, we'll quit the loop.
            maybe_cur = chunk.next;
        }

        // If there are more chunks than those we've visited: some of them were not
        // linked to the "main" branch of the linked list, so we had multiple connected
        // components.
        if visited.len() != self.chunks.len() {
            return Err(LinkedChunkBuilderError::MultipleConnectedComponents);
        }

        Ok(first_id)
    }

    pub fn build(mut self) -> Result<Option<LinkedChunk<CAP, Item, Gap>>, LinkedChunkBuilderError> {
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
            return Err(LinkedChunkBuilderError::MissingFirstChunk);
        };

        let mut prev_chunk_ptr = first_chunk_ptr;

        while let Some(id) = next_chunk_id {
            let Some((new_next, mut chunk_ptr)) = graduate_chunk(id) else {
                // Can't really happen, but oh well.
                return Err(LinkedChunkBuilderError::MissingChunk { id });
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

        let updates =
            if self.build_with_update_history { Some(ObservableUpdates::new()) } else { None };

        Ok(Some(LinkedChunk { links, chunk_identifier_generator, updates, marker: PhantomData }))
    }
}

#[derive(thiserror::Error, Debug)]
pub enum LinkedChunkBuilderError {
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
mod tests {
    use assert_matches::assert_matches;

    use super::LinkedChunkBuilder;
    use crate::linked_chunk::{ChunkIdentifier, LinkedChunkBuilderError};

    #[test]
    fn test_empty() {
        let lcb = LinkedChunkBuilder::<3, char, char>::new();

        // Building an empty linked chunk works, and returns `None`.
        let lc = lcb.build().unwrap();
        assert!(lc.is_none());
    }

    #[test]
    fn test_success() {
        let mut lcb = LinkedChunkBuilder::<3, char, char>::new();

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
        let mut lcb = LinkedChunkBuilder::<3, char, char>::new();

        let cid0 = ChunkIdentifier::new(0);

        // Adding a chunk with 4 items will fail, because the max capacity specified in
        // the builder generics is 3.
        lcb.push_items(None, cid0, None, vec!['a', 'b', 'c', 'd']);

        let res = lcb.build();
        assert_matches!(res, Err(LinkedChunkBuilderError::ChunkTooLarge { id }) => {
            assert_eq!(id, cid0);
        });
    }

    #[test]
    fn test_missing_first_chunk() {
        let mut lcb = LinkedChunkBuilder::<3, char, char>::new();

        let cid0 = ChunkIdentifier::new(0);
        let cid1 = ChunkIdentifier::new(1);
        let cid2 = ChunkIdentifier::new(2);

        lcb.push_gap(Some(cid2), cid0, Some(cid1), 'g');
        lcb.push_items(Some(cid0), cid1, Some(cid2), ['a', 'b', 'c']);
        lcb.push_items(Some(cid1), cid2, Some(cid0), ['d', 'e', 'f']);

        let res = lcb.build();
        assert_matches!(res, Err(LinkedChunkBuilderError::MissingFirstChunk));
    }

    #[test]
    fn test_multiple_first_chunks() {
        let mut lcb = LinkedChunkBuilder::<3, char, char>::new();

        let cid0 = ChunkIdentifier::new(0);
        let cid1 = ChunkIdentifier::new(1);

        lcb.push_gap(None, cid0, Some(cid1), 'g');
        // Second chunk lies and pretends to be the first too.
        lcb.push_items(None, cid1, Some(cid0), ['a', 'b', 'c']);

        let res = lcb.build();
        assert_matches!(res, Err(LinkedChunkBuilderError::MultipleFirstChunks { first_candidate, second_candidate }) => {
            assert_eq!(first_candidate, cid0);
            assert_eq!(second_candidate, cid1);
        });
    }

    #[test]
    fn test_missing_chunk() {
        let mut lcb = LinkedChunkBuilder::<3, char, char>::new();

        let cid0 = ChunkIdentifier::new(0);
        let cid1 = ChunkIdentifier::new(1);
        lcb.push_gap(None, cid0, Some(cid1), 'g');

        let res = lcb.build();
        assert_matches!(res, Err(LinkedChunkBuilderError::MissingChunk { id }) => {
            assert_eq!(id, cid1);
        });
    }

    #[test]
    fn test_cycle() {
        let mut lcb = LinkedChunkBuilder::<3, char, char>::new();

        let cid0 = ChunkIdentifier::new(0);
        let cid1 = ChunkIdentifier::new(1);
        lcb.push_gap(None, cid0, Some(cid1), 'g');
        lcb.push_gap(Some(cid0), cid1, Some(cid0), 'g');

        let res = lcb.build();
        assert_matches!(res, Err(LinkedChunkBuilderError::Cycle { repeated }) => {
            assert_eq!(repeated, cid0);
        });
    }

    #[test]
    fn test_multiple_connected_components() {
        let mut lcb = LinkedChunkBuilder::<3, char, char>::new();

        let cid0 = ChunkIdentifier::new(0);
        let cid1 = ChunkIdentifier::new(1);
        let cid2 = ChunkIdentifier::new(2);

        // cid0 and cid1 are linked to each other.
        lcb.push_gap(None, cid0, Some(cid1), 'g');
        lcb.push_items(Some(cid0), cid1, None, ['a', 'b', 'c']);
        // cid2 stands on its own.
        lcb.push_items(None, cid2, None, ['d', 'e', 'f']);

        let res = lcb.build();
        assert_matches!(res, Err(LinkedChunkBuilderError::MultipleConnectedComponents));
    }
}
