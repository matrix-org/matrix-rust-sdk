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

#![allow(dead_code)]

use std::{
    fmt,
    marker::PhantomData,
    ops::Not,
    ptr::NonNull,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

/// Errors of [`LinkedChunk`].
#[derive(Debug)]
pub enum LinkedChunkError {
    InvalidChunkIdentifier { identifier: ChunkIdentifier },
    ChunkIsAGap { identifier: ChunkIdentifier },
    ChunkIsItems { identifier: ChunkIdentifier },
    InvalidItemIndex { index: usize },
}

/// The [`LinkedChunk`] structure.
///
/// It is similar to a linked list, except that it contains many items `Item`
/// instead of a single one. A chunk has a maximum capacity of `CHUNK_CAPACITY`.
/// Once a chunk is full, a new chunk is created. Not all chunks are necessarily
/// entirely full. A chunk can represents a `Gap` between other chunks.
pub struct LinkedChunk<Item, Gap, const CHUNK_CAPACITY: usize> {
    /// The first chunk.
    first: NonNull<Chunk<Item, Gap, CHUNK_CAPACITY>>,
    /// The last chunk.
    last: Option<NonNull<Chunk<Item, Gap, CHUNK_CAPACITY>>>,
    /// The number of items hold by this linked chunk.
    length: usize,
    /// The generator of chunk identifiers.
    chunk_identifier_generator: ChunkIdentifierGenerator,
    /// Marker.
    marker: PhantomData<Box<Chunk<Item, Gap, CHUNK_CAPACITY>>>,
}

impl<Item, Gap, const CAP: usize> LinkedChunk<Item, Gap, CAP> {
    /// Create a new [`Self`].
    pub fn new() -> Self {
        Self {
            // INVARIANT: The first chunk must always be an Items, not a Gap.
            first: Chunk::new_items_leaked(ChunkIdentifierGenerator::FIRST_IDENTIFIER),
            last: None,
            length: 0,
            chunk_identifier_generator: ChunkIdentifierGenerator::new_from_scratch(),
            marker: PhantomData,
        }
    }

    /// Get the number of items in this linked chunk.
    pub fn len(&self) -> usize {
        self.length
    }

    /// Push items at the end of the [`LinkedChunk`], i.e. on the last
    /// chunk.
    ///
    /// If the last chunk doesn't have enough space to welcome all `items`,
    /// then new chunks can be created (and linked appropriately).
    pub fn push_items_back<I>(&mut self, items: I)
    where
        I: IntoIterator<Item = Item>,
        I::IntoIter: ExactSizeIterator,
    {
        let items = items.into_iter();
        let number_of_items = items.len();

        let chunk_identifier_generator = self.chunk_identifier_generator.clone();

        let last_chunk = self.latest_chunk_mut();

        // Push the items.
        let last_chunk = last_chunk.push_items(items, &chunk_identifier_generator);

        debug_assert!(last_chunk.is_last_chunk(), "`last_chunk` must be… the last chunk");

        // We need to update `self.last` if and only if `last_chunk` _is not_ the first
        // chunk, and _is_ the last chunk (ensured by the `debug_assert!` above).
        if last_chunk.is_first_chunk().not() {
            // Maybe `last_chunk` is the same as the previous `self.last` chunk, but it's
            // OK.
            self.last = Some(NonNull::from(last_chunk));
        }

        self.length += number_of_items;
    }

    /// Push a gap at the end of the [`LinkedChunk`], i.e. after the last
    /// chunk.
    pub fn push_gap_back(&mut self, content: Gap) {
        let next_identifier = self.chunk_identifier_generator.generate_next().unwrap();

        let last_chunk = self.latest_chunk_mut();
        last_chunk.insert_next(Chunk::new_gap_leaked(next_identifier, content));

        self.last = last_chunk.next;
    }

    /// Insert items at a specified position in the [`LinkedChunk`].
    ///
    /// Because the `position` can be invalid, this method returns a
    /// `Result`.
    pub fn insert_items_at<I>(
        &mut self,
        items: I,
        position: ItemPosition,
    ) -> Result<(), LinkedChunkError>
    where
        I: IntoIterator<Item = Item>,
        I::IntoIter: ExactSizeIterator,
    {
        let chunk_identifier = position.chunk_identifier();
        let item_index = position.item_index();

        let chunk_identifier_generator = self.chunk_identifier_generator.clone();

        let chunk = self
            .chunk_mut(chunk_identifier)
            .ok_or(LinkedChunkError::InvalidChunkIdentifier { identifier: chunk_identifier })?;

        let (chunk, number_of_items) = match &mut chunk.content {
            ChunkContent::Gap(..) => {
                return Err(LinkedChunkError::ChunkIsAGap { identifier: chunk_identifier })
            }
            ChunkContent::Items(current_items) => {
                let current_items_length = current_items.len();

                if item_index >= current_items_length {
                    return Err(LinkedChunkError::InvalidItemIndex { index: item_index });
                }

                // The `ItemPosition` is computed from the latest items. Here, we manipulate the
                // items in their original order: the last item comes last. Let's adjust
                // `item_index`.
                let item_index = current_items_length - 1 - item_index;

                // Split the items.
                let detached_items = current_items.split_off(item_index);

                // Prepare the items to be pushed.
                let items = items.into_iter();
                let number_of_items = items.len();

                (
                    chunk
                        // Push the new items.
                        .push_items(items, &chunk_identifier_generator)
                        // Finally, push the items that have been detached.
                        .push_items(detached_items.into_iter(), &chunk_identifier_generator),
                    number_of_items,
                )
            }
        };

        // We need to update `self.last` if and only if `chunk` _is not_ the first
        // chunk, and _is_ the last chunk.
        if chunk.is_first_chunk().not() && chunk.is_last_chunk() {
            // Maybe `chunk` is the same as the previous `self.last` chunk, but it's
            // OK.
            self.last = Some(NonNull::from(chunk));
        }

        self.length += number_of_items;

        Ok(())
    }

    /// Insert a gap at a specified position in the [`LinkedChunk`].
    ///
    /// Because the `position` can be invalid, this method returns a
    /// `Result`.
    pub fn insert_gap_at(
        &mut self,
        position: ItemPosition,
        content: Gap,
    ) -> Result<(), LinkedChunkError> {
        let chunk_identifier = position.chunk_identifier();
        let item_index = position.item_index();

        let chunk_identifier_generator = self.chunk_identifier_generator.clone();

        let chunk = self
            .chunk_mut(chunk_identifier)
            .ok_or(LinkedChunkError::InvalidChunkIdentifier { identifier: chunk_identifier })?;

        let chunk = match &mut chunk.content {
            ChunkContent::Gap(..) => {
                return Err(LinkedChunkError::ChunkIsAGap { identifier: chunk_identifier });
            }
            ChunkContent::Items(current_items) => {
                let current_items_length = current_items.len();

                if item_index >= current_items_length {
                    return Err(LinkedChunkError::InvalidItemIndex { index: item_index });
                }

                // The `ItemPosition` is computed from the latest items. Here, we manipulate the
                // items in their original order: the last item comes last. Let's adjust
                // `item_index`.
                let item_index = current_items_length - 1 - item_index;

                // Split the items.
                let detached_items = current_items.split_off(item_index);

                chunk
                    // Insert a new gap chunk.
                    .insert_next(Chunk::new_gap_leaked(
                        chunk_identifier_generator.generate_next().unwrap(),
                        content,
                    ))
                    // Insert a new items chunk.
                    .insert_next(Chunk::new_items_leaked(
                        chunk_identifier_generator.generate_next().unwrap(),
                    ))
                    // Finally, push the items that have been detached.
                    .push_items(detached_items.into_iter(), &chunk_identifier_generator)
            }
        };

        // We need to update `self.last` if and only if `chunk` _is not_ the first
        // chunk, and _is_ the last chunk.
        if chunk.is_first_chunk().not() && chunk.is_last_chunk() {
            // Maybe `chunk` is the same as the previous `self.last` chunk, but it's
            // OK.
            self.last = Some(NonNull::from(chunk));
        }

        Ok(())
    }

    /// Replace the gap identified by `chunk_identifier`, by items.
    ///
    /// Because the `chunk_identifier` can represent non-gap chunk, this method
    /// returns a `Result`.
    pub fn replace_gap_at<I>(
        &mut self,
        items: I,
        chunk_identifier: ChunkIdentifier,
    ) -> Result<(), LinkedChunkError>
    where
        I: IntoIterator<Item = Item>,
        I::IntoIter: ExactSizeIterator,
    {
        let chunk_identifier_generator = self.chunk_identifier_generator.clone();
        let chunk_ptr;

        {
            let chunk = self
                .chunk_mut(chunk_identifier)
                .ok_or(LinkedChunkError::InvalidChunkIdentifier { identifier: chunk_identifier })?;

            debug_assert!(chunk.is_first_chunk().not(), "A gap cannot be the first chunk");

            let (previous, number_of_items) = match &mut chunk.content {
                ChunkContent::Gap(..) => {
                    let items = items.into_iter();
                    let number_of_items = items.len();

                    // Find the previous chunk…
                    //
                    // SAFETY: `unwrap` is safe because we are ensured `chunk` is not the first
                    // chunk, so a previous chunk always exists.
                    let previous = chunk.previous_mut().unwrap();

                    // … and insert the items on it.
                    (previous.push_items(items, &chunk_identifier_generator), number_of_items)
                }
                ChunkContent::Items(..) => {
                    return Err(LinkedChunkError::ChunkIsItems { identifier: chunk_identifier })
                }
            };

            // Get the pointer to `chunk` via `previous`.
            //
            // SAFETY: `unwrap` is safe because we are ensured the next of the previous
            // chunk is `chunk` itself.
            chunk_ptr = previous.next.unwrap();

            // Get the pointer to the `previous` via `chunk`.
            let previous_ptr = chunk.previous;

            // Now that new items have been pushed, we can unlink the gap chunk.
            chunk.unlink();

            // Update `self.last` if the gap chunk was the last chunk.
            if chunk.is_last_chunk() {
                self.last = previous_ptr;
            }

            self.length += number_of_items;

            // Stop borrowing `chunk`.
        }

        // Re-box the chunk, and let Rust does its job.
        //
        // SAFETY: `chunk` is unlinked but it still exists in memory! We have its
        // pointer, which is valid and well aligned.
        let _chunk_boxed = unsafe { Box::from_raw(chunk_ptr.as_ptr()) };

        Ok(())
    }

    /// Get the chunk as a reference, from its identifier, if it exists.
    fn chunk(&self, identifier: ChunkIdentifier) -> Option<&Chunk<Item, Gap, CAP>> {
        let mut chunk = self.latest_chunk();

        loop {
            if chunk.identifier() == identifier {
                return Some(chunk);
            }

            chunk = chunk.previous()?;
        }
    }

    /// Get the chunk as a mutable reference, from its identifier, if it exists.
    fn chunk_mut(&mut self, identifier: ChunkIdentifier) -> Option<&mut Chunk<Item, Gap, CAP>> {
        let mut chunk = self.latest_chunk_mut();

        loop {
            if chunk.identifier() == identifier {
                return Some(chunk);
            }

            chunk = chunk.previous_mut()?;
        }
    }

    /// Search for a chunk, and return its identifier.
    pub fn chunk_identifier<'a, P>(&'a self, mut predicate: P) -> Option<ChunkIdentifier>
    where
        P: FnMut(&'a Chunk<Item, Gap, CAP>) -> bool,
    {
        self.rchunks().find_map(|chunk| predicate(chunk).then_some(chunk.identifier()))
    }

    /// Search for an item, and return its position.
    pub fn item_position<'a, P>(&'a self, mut predicate: P) -> Option<ItemPosition>
    where
        P: FnMut(&'a Item) -> bool,
    {
        self.ritems().find_map(|(item_position, item)| predicate(item).then_some(item_position))
    }

    /// Iterate over the chunks, backward.
    ///
    /// It iterates from the last to the first chunk.
    pub fn rchunks(&self) -> LinkedChunkIterBackward<'_, Item, Gap, CAP> {
        self.rchunks_from(self.latest_chunk().identifier())
            .expect("`iter_chunks_from` cannot fail because at least one empty chunk must exist")
    }

    /// Iterate over the chunks, starting from `identifier`, backward.
    ///
    /// It iterates from the chunk with the identifier `identifier` to the first
    /// chunk.
    pub fn rchunks_from(
        &self,
        identifier: ChunkIdentifier,
    ) -> Result<LinkedChunkIterBackward<'_, Item, Gap, CAP>, LinkedChunkError> {
        Ok(LinkedChunkIterBackward::new(
            self.chunk(identifier)
                .ok_or(LinkedChunkError::InvalidChunkIdentifier { identifier })?,
        ))
    }

    /// Iterate over the chunks, starting from `position`, forward.
    ///
    /// It iterates from the chunk with the identifier `identifier` to the last
    /// chunk.
    pub fn chunks_from(
        &self,
        identifier: ChunkIdentifier,
    ) -> Result<LinkedChunkIter<'_, Item, Gap, CAP>, LinkedChunkError> {
        Ok(LinkedChunkIter::new(
            self.chunk(identifier)
                .ok_or(LinkedChunkError::InvalidChunkIdentifier { identifier })?,
        ))
    }

    /// Iterate over the items, backward.
    ///
    /// It iterates from the last to the first item.
    pub fn ritems(&self) -> impl Iterator<Item = (ItemPosition, &Item)> {
        self.ritems_from(self.latest_chunk().identifier().to_last_item_position())
            .expect("`iter_items_from` cannot fail because at least one empty chunk must exist")
    }

    /// Iterate over the items, forward.
    ///
    /// It iterates from the first to the last item.
    pub fn items(&self) -> impl Iterator<Item = (ItemPosition, &T)> {
        let first_chunk = self.first_chunk();
        let ChunkContent::Items(items) = first_chunk.content() else {
            unreachable!("The first chunk is necessarily an `Items`");
        };

        self.items_from(ItemPosition(ChunkIdentifierGenerator::FIRST_IDENTIFIER, items.len()))
            .expect("`items` cannot fail because at least one empty chunk must exist")
    }

    /// Iterate over the items, starting from `position`, backward.
    ///
    /// It iterates from the item at `position` to the first item.
    pub fn ritems_from(
        &self,
        position: ItemPosition,
    ) -> Result<impl Iterator<Item = (ItemPosition, &Item)>, LinkedChunkError> {
        Ok(self
            .rchunks_from(position.chunk_identifier())?
            .filter_map(|chunk| match &chunk.content {
                ChunkContent::Gap(..) => None,
                ChunkContent::Items(items) => {
                    Some(items.iter().rev().enumerate().map(move |(item_index, item)| {
                        (ItemPosition(chunk.identifier(), item_index), item)
                    }))
                }
            })
            .flatten()
            .skip(position.item_index()))
    }

    /// Iterate over the items, starting from `position`, forward.
    ///
    /// It iterates from the item at `position` to the last item.
    pub fn items_from(
        &self,
        position: ItemPosition,
    ) -> Result<impl Iterator<Item = (ItemPosition, &Item)>, LinkedChunkError> {
        Ok(self
            .chunks_from(position.chunk_identifier())?
            .filter_map(|chunk| match &chunk.content {
                ChunkContent::Gap(..) => None,
                ChunkContent::Items(items) => {
                    Some(items.iter().rev().enumerate().rev().map(move |(item_index, item)| {
                        (ItemPosition(chunk.identifier(), item_index), item)
                    }))
                }
            })
            .flatten())
    }

    /// Get the first chunk, as an immutable reference.
    fn first_chunk(&self) -> &Chunk<T, U, C> {
        unsafe { self.first.as_ref() }
    }

    /// Get the latest chunk, as an immutable reference.
    fn latest_chunk(&self) -> &Chunk<Item, Gap, CAP> {
        unsafe { self.last.unwrap_or(self.first).as_ref() }
    }

    /// Get the latest chunk, as a mutable reference.
    fn latest_chunk_mut(&mut self) -> &mut Chunk<Item, Gap, CAP> {
        unsafe { self.last.as_mut().unwrap_or(&mut self.first).as_mut() }
    }
}

impl<Item, Gap, const C: usize> Drop for LinkedChunk<Item, Gap, C> {
    fn drop(&mut self) {
        // Take the latest chunk.
        let mut current_chunk_ptr = self.last.or(Some(self.first));

        // As long as we have another chunk…
        while let Some(chunk_ptr) = current_chunk_ptr {
            // Disconnect the chunk by updating `previous_chunk.next` pointer.
            let previous_ptr = unsafe { chunk_ptr.as_ref() }.previous;

            if let Some(mut previous_ptr) = previous_ptr {
                unsafe { previous_ptr.as_mut() }.next = None;
            }

            // Re-box the chunk, and let Rust does its job.
            let _chunk_boxed = unsafe { Box::from_raw(chunk_ptr.as_ptr()) };

            // Update the `current_chunk_ptr`.
            current_chunk_ptr = previous_ptr;
        }

        // At this step, all chunks have been dropped, including
        // `self.first`.
    }
}

/// A [`LinkedChunk`] can be safely sent over thread boundaries if `Item: Send`
/// and `Gap: Send`. The only unsafe part if around the `NonNull`, but the API
/// and the lifetimes to deref them are designed safely.
unsafe impl<Item: Send, Ugap: Send, const CAP: usize> Send for LinkedChunk<Item, Ugap, CAP> {}

/// A [`LinkedChunk`] can be safely share between threads if `Item: Sync` and
/// `Gap: Sync`. The only unsafe part if around the `NonNull`, but the API and
/// the lifetimes to deref them are designed safely.
unsafe impl<Item: Sync, Gap: Sync, const CAP: usize> Sync for LinkedChunk<Item, Gap, CAP> {}

/// Generator for [`Chunk`]'s identifier.
///
/// Each [`Chunk`] has a unique identifier. This generator generates the unique
/// identifiers.
///
/// In order to keep good performance, a unique identifier is simply a `u64`
/// (see [`ChunkIdentifier`]). Generating a new unique identifier boils down to
/// incrementing by one the previous identifier. Note that this is not an index:
/// it _is_ an identifier.
///
/// Cloning this type is shallow, and thus cheap.
#[derive(Clone)]
struct ChunkIdentifierGenerator {
    next: Arc<AtomicU64>,
}

impl ChunkIdentifierGenerator {
    /// The first identifier.
    const FIRST_IDENTIFIER: ChunkIdentifier = ChunkIdentifier(0);

    /// Create the generator assuming the current [`LinkedChunk`] it belongs to
    /// is empty.
    pub fn new_from_scratch() -> Self {
        Self { next: Arc::new(AtomicU64::new(Self::FIRST_IDENTIFIER.0)) }
    }

    /// Create the generator assuming the current [`LinkedChunk`] it belongs to
    /// is not empty, i.e. it already has some [`Chunk`] in it.
    pub fn new_from_previous_chunk_identifier(last_chunk_identifier: ChunkIdentifier) -> Self {
        Self { next: Arc::new(AtomicU64::new(last_chunk_identifier.0)) }
    }

    /// Generate the next unique identifier.
    ///
    /// Note that it can fail if there is no more unique identifier available.
    /// In this case, `Result::Err` contains the previous unique identifier.
    pub fn generate_next(&self) -> Result<ChunkIdentifier, ChunkIdentifier> {
        let previous = self.next.fetch_add(1, Ordering::Relaxed);
        let current = self.next.load(Ordering::Relaxed);

        // Check for overflows.
        // unlikely — TODO: call `std::intrinsics::unlikely` once it's stable.
        if current < previous {
            return Err(ChunkIdentifier(previous));
        }

        Ok(ChunkIdentifier(current))
    }
}

/// The unique identifier of a chunk in a [`LinkedChunk`].
///
/// It is not the position of the chunk, just its unique identifier.
///
/// Learn more with [`ChunkIdentifierGenerator`].
#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(transparent)]
pub struct ChunkIdentifier(u64);

impl ChunkIdentifier {
    /// Transform the `ChunkIdentifier` into an `ItemPosition` representing the
    /// last item position.
    fn to_last_item_position(self) -> ItemPosition {
        ItemPosition(self, 0)
    }
}

/// The position of an item in a [`LinkedChunk`].
///
/// It's a pair of a chunk position and an item index. `(…, 0)` represents
/// the last item in the chunk.
#[derive(Debug, PartialEq)]
pub struct ItemPosition(ChunkIdentifier, usize);

impl ItemPosition {
    /// Get the chunk identifier of the item.
    pub fn chunk_identifier(&self) -> ChunkIdentifier {
        self.0
    }

    /// Get the item index inside its chunk.
    pub fn item_index(&self) -> usize {
        self.1
    }
}

/// An iterator over a [`LinkedChunk`] that traverses the chunk in backward
/// direction (i.e. it calls `previous` on each chunk to make progress).
pub struct LinkedChunkIterBackward<'a, Item, Gap, const CAP: usize> {
    chunk: Option<&'a Chunk<Item, Gap, CAP>>,
}

impl<'a, Item, Gap, const CAP: usize> LinkedChunkIterBackward<'a, Item, Gap, CAP> {
    /// Create a new [`LinkedChunkIter`] from a particular [`Chunk`].
    fn new(from_chunk: &'a Chunk<Item, Gap, CAP>) -> Self {
        Self { chunk: Some(from_chunk) }
    }
}

impl<'a, Item, Gap, const CAP: usize> Iterator for LinkedChunkIterBackward<'a, Item, Gap, CAP> {
    type Item = &'a Chunk<Item, Gap, CAP>;

    fn next(&mut self) -> Option<Self::Item> {
        self.chunk.map(|chunk| {
            self.chunk = chunk.previous();

            chunk
        })
    }
}

/// An iterator over a [`LinkedChunk`] that traverses the chunk in forward
/// direction (i.e. it calls `next` on each chunk to make progress).
pub struct LinkedChunkIter<'a, Item, Gap, const CAP: usize> {
    chunk: Option<&'a Chunk<Item, Gap, CAP>>,
}

impl<'a, Item, Gap, const CAP: usize> LinkedChunkIter<'a, Item, Gap, CAP> {
    /// Create a new [`LinkedChunkIter`] from a particular [`Chunk`].
    fn new(from_chunk: &'a Chunk<Item, Gap, CAP>) -> Self {
        Self { chunk: Some(from_chunk) }
    }
}

impl<'a, Item, Gap, const CAP: usize> Iterator for LinkedChunkIter<'a, Item, Gap, CAP> {
    type Item = &'a Chunk<Item, Gap, CAP>;

    fn next(&mut self) -> Option<Self::Item> {
        self.chunk.map(|chunk| {
            self.chunk = chunk.next();

            chunk
        })
    }
}

/// This enum represents the content of a [`Chunk`].
#[derive(Debug)]
pub enum ChunkContent<Item, Gap> {
    /// The chunk represents a gap in the linked chunk, i.e. a hole. It
    /// means that some items are missing in this location.
    Gap(Gap),

    /// The chunk contains items.
    Items(Vec<Item>),
}

/// A chunk is a node in the [`LinkedChunk`].
pub struct Chunk<Item, Gap, const CAPACITY: usize> {
    /// The previous chunk.
    previous: Option<NonNull<Chunk<Item, Gap, CAPACITY>>>,

    /// The next chunk.
    next: Option<NonNull<Chunk<Item, Gap, CAPACITY>>>,

    /// Unique identifier.
    identifier: ChunkIdentifier,

    /// The content of the chunk.
    content: ChunkContent<Item, Gap>,
}

impl<Item, Gap, const CAPACITY: usize> Chunk<Item, Gap, CAPACITY> {
    /// Create a new gap chunk.
    fn new_gap(identifier: ChunkIdentifier, content: Gap) -> Self {
        Self::new(identifier, ChunkContent::Gap(content))
    }

    /// Create a new items chunk.
    fn new_items(identifier: ChunkIdentifier) -> Self {
        Self::new(identifier, ChunkContent::Items(Vec::with_capacity(CAPACITY)))
    }

    fn new(identifier: ChunkIdentifier, content: ChunkContent<Item, Gap>) -> Self {
        Self { previous: None, next: None, identifier, content }
    }

    /// Create a new gap chunk, but box it and leak it.
    fn new_gap_leaked(identifier: ChunkIdentifier, content: Gap) -> NonNull<Self> {
        let chunk = Self::new_gap(identifier, content);
        let chunk_box = Box::new(chunk);

        NonNull::from(Box::leak(chunk_box))
    }

    /// Create a new items chunk, but box it and leak it.
    fn new_items_leaked(identifier: ChunkIdentifier) -> NonNull<Self> {
        let chunk = Self::new_items(identifier);
        let chunk_box = Box::new(chunk);

        NonNull::from(Box::leak(chunk_box))
    }

    /// Check whether this current chunk is a gap chunk.
    fn is_gap(&self) -> bool {
        matches!(self.content, ChunkContent::Gap(..))
    }

    /// Check whether this current chunk is an items  chunk.
    fn is_items(&self) -> bool {
        !self.is_gap()
    }

    /// Check whether this current chunk is the first chunk.
    fn is_first_chunk(&self) -> bool {
        self.previous.is_none()
    }

    /// Check whether this current chunk is the last chunk.
    fn is_last_chunk(&self) -> bool {
        self.next.is_none()
    }

    /// Get the unique identifier of the chunk.
    fn identifier(&self) -> ChunkIdentifier {
        self.identifier
    }

    /// The length of the chunk, i.e. how many items are in it.
    ///
    /// It will always return 0 if it's a gap chunk.
    fn len(&self) -> usize {
        match &self.content {
            ChunkContent::Gap(..) => 0,
            ChunkContent::Items(items) => items.len(),
        }
    }

    /// Push items on the current chunk.
    ///
    /// If the chunk doesn't have enough spaces to welcome `new_items`, new
    /// chunk will be inserted next, and correctly linked.
    ///
    /// This method returns the last inserted chunk if any, or the current
    /// chunk. Basically, it returns the chunk onto which new computations
    /// must happen.
    ///
    /// Pushing items will always create new chunks if necessary, but it
    /// will never merge them, so that we avoid updating too much chunks.
    fn push_items<I>(
        &mut self,
        mut new_items: I,
        chunk_identifier_generator: &ChunkIdentifierGenerator,
    ) -> &mut Self
    where
        I: Iterator<Item = Item> + ExactSizeIterator,
    {
        let number_of_new_items = new_items.len();
        let chunk_length = self.len();

        // A small optimisation. Skip early if there is no new items.
        if number_of_new_items == 0 {
            return self;
        }

        match &mut self.content {
            // Cannot push items on a `Gap`. Let's insert a new `Items` chunk to push the
            // items onto it.
            ChunkContent::Gap(..) => {
                self
                    // Insert a new items chunk.
                    .insert_next(Self::new_items_leaked(
                        chunk_identifier_generator.generate_next().unwrap(),
                    ))
                    // Now push the new items on the next chunk, and return the result of
                    // `push_items`.
                    .push_items(new_items, chunk_identifier_generator)
            }

            ChunkContent::Items(items) => {
                // Calculate the free space of the current chunk.
                let free_space = CAPACITY.saturating_sub(chunk_length);

                // There is enough space to push all the new items.
                if number_of_new_items <= free_space {
                    items.extend(new_items);

                    // Return the current chunk.
                    self
                } else {
                    if free_space > 0 {
                        // Take all possible items to fill the free space.
                        items.extend(new_items.by_ref().take(free_space));
                    }

                    self
                        // Insert a new items chunk.
                        .insert_next(Self::new_items_leaked(
                            chunk_identifier_generator.generate_next().unwrap(),
                        ))
                        // Now push the rest of the new items on the next chunk, and return the
                        // result of `push_items`.
                        .push_items(new_items, chunk_identifier_generator)
                }
            }
        }
    }

    /// Insert a new chunk after the current one.
    ///
    /// The respective [`Self::previous`] and [`Self::next`] of the current
    /// and new chunk will be updated accordingly.
    fn insert_next(&mut self, mut new_chunk_ptr: NonNull<Self>) -> &mut Self {
        let new_chunk = unsafe { new_chunk_ptr.as_mut() };

        // Update the next chunk if any.
        if let Some(next_chunk) = self.next_mut() {
            // Link back to the new chunk.
            next_chunk.previous = Some(new_chunk_ptr);

            // Link the new chunk to the next chunk.
            new_chunk.next = self.next;
        }

        // Link to the new chunk.
        self.next = Some(new_chunk_ptr);
        // Link the new chunk to this one.
        new_chunk.previous = Some(NonNull::from(self));

        new_chunk
    }

    /// Unlink this chunk.
    ///
    /// Be careful: `self` won't belong to `LinkedChunk` anymore, and should be
    /// dropped appropriately.
    fn unlink(&mut self) {
        let previous_ptr = self.previous;
        let next_ptr = self.next;

        if let Some(previous) = self.previous_mut() {
            previous.next = next_ptr;
        }

        if let Some(next) = self.next_mut() {
            next.previous = previous_ptr;
        }
    }

    /// Get a reference to the previous chunk if any.
    fn previous(&self) -> Option<&Self> {
        self.previous.map(|non_null| unsafe { non_null.as_ref() })
    }

    /// Get a mutable to the previous chunk if any.
    fn previous_mut(&mut self) -> Option<&mut Self> {
        self.previous.as_mut().map(|non_null| unsafe { non_null.as_mut() })
    }

    /// Get a reference to the next chunk if any.
    fn next(&self) -> Option<&Self> {
        self.next.map(|non_null| unsafe { non_null.as_ref() })
    }

    /// Get a mutable reference to the next chunk if any.
    fn next_mut(&mut self) -> Option<&mut Self> {
        self.next.as_mut().map(|non_null| unsafe { non_null.as_mut() })
    }
}

impl<Item, Gap, const CAP: usize> fmt::Debug for LinkedChunk<Item, Gap, CAP>
where
    Item: fmt::Debug,
    Gap: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        formatter
            .debug_struct("LinkedChunk")
            .field("first (deref)", unsafe { self.first.as_ref() })
            .field("last", &self.last)
            .field("length", &self.length)
            .finish()
    }
}

impl<Item, Gap, const CAP: usize> fmt::Debug for Chunk<Item, Gap, CAP>
where
    Item: fmt::Debug,
    Gap: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        formatter
            .debug_struct("Chunk")
            .field("identifier", &self.identifier)
            .field("content", &self.content)
            .field("previous", &self.previous)
            .field("ptr", &std::ptr::from_ref(self))
            .field("next", &self.next)
            .field("next (deref)", &self.next.as_ref().map(|non_null| unsafe { non_null.as_ref() }))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;

    use super::{
        Chunk, ChunkContent, ChunkIdentifier, ChunkIdentifierGenerator, ItemPosition, LinkedChunk,
        LinkedChunkError,
    };

    macro_rules! assert_items_eq {
        ( @_ [ $iterator:ident, $chunk_index:ident, $item_index:ident ] { [-] $( $rest:tt )* } { $( $accumulator:tt )* } ) => {
            assert_items_eq!(
                @_
                [ $iterator, $chunk_index, $item_index ]
                { $( $rest )* }
                {
                    $( $accumulator )*
                    $chunk_index += 1;
                }
            )
        };

        ( @_ [ $iterator:ident, $chunk_index:ident, $item_index:ident ] { [ $( $item:expr ),* ] $( $rest:tt )* } { $( $accumulator:tt )* } ) => {
            assert_items_eq!(
                @_
                [ $iterator, $chunk_index, $item_index ]
                { $( $rest )* }
                {
                    $( $accumulator )*
                    let _expected_chunk_identifier = $iterator .peek().unwrap().1.chunk_identifier();
                    $(
                        assert_matches!(
                            $iterator .next(),
                            Some((chunk_index, ItemPosition(chunk_identifier, item_index), & $item )) => {
                                // Ensure the chunk index (from the enumeration) is correct.
                                assert_eq!(chunk_index, $chunk_index);
                                // Ensure the chunk identifier is the same for all items in this chunk.
                                assert_eq!(chunk_identifier, _expected_chunk_identifier);
                                // Ensure the item has the expected position.
                                assert_eq!(item_index, $item_index);
                            }
                        );
                        $item_index += 1;
                    )*
                    $item_index = 0;
                    $chunk_index += 1;
                }
            )
        };

        ( @_ [ $iterator:ident, $chunk_index:ident, $item_index:ident ] {} { $( $accumulator:tt )* } ) => {
            {
                let mut $chunk_index = 0;
                let mut $item_index = 0;
                $( $accumulator )*
            }
        };

        ( $linked_chunk:expr, $( $all:tt )* ) => {
            assert_items_eq!(
                @_
                [ iterator, _chunk_index, _item_index ]
                { $( $all )* }
                {
                    let mut iterator = $linked_chunk
                        .chunks_from(ChunkIdentifierGenerator::FIRST_IDENTIFIER)
                        .unwrap()
                        .enumerate()
                        .filter_map(|(chunk_index, chunk)| match &chunk.content {
                            ChunkContent::Gap(..) => None,
                            ChunkContent::Items(items) => {
                                Some(items.iter().enumerate().map(move |(item_index, item)| {
                                    (chunk_index, ItemPosition(chunk.identifier(), item_index), item)
                                }))
                            }
                        })
                        .flatten()
                        .peekable();
                }
            )
        }
    }

    #[test]
    fn test_chunk_identifier_generator() {
        let generator = ChunkIdentifierGenerator::new_from_scratch();

        assert_eq!(generator.generate_next(), Ok(ChunkIdentifier(1)));
        assert_eq!(generator.generate_next(), Ok(ChunkIdentifier(2)));
        assert_eq!(generator.generate_next(), Ok(ChunkIdentifier(3)));
        assert_eq!(generator.generate_next(), Ok(ChunkIdentifier(4)));

        let generator =
            ChunkIdentifierGenerator::new_from_previous_chunk_identifier(ChunkIdentifier(42));

        assert_eq!(generator.generate_next(), Ok(ChunkIdentifier(43)));
        assert_eq!(generator.generate_next(), Ok(ChunkIdentifier(44)));
        assert_eq!(generator.generate_next(), Ok(ChunkIdentifier(45)));
        assert_eq!(generator.generate_next(), Ok(ChunkIdentifier(46)));
    }

    #[test]
    fn test_empty() {
        let items = LinkedChunk::<char, (), 3>::new();

        assert_eq!(items.len(), 0);

        // This test also ensures that `Drop` for `LinkedChunk` works when
        // there is only one chunk.
    }

    #[test]
    fn test_push_items() {
        let mut linked_chunk = LinkedChunk::<char, (), 3>::new();
        linked_chunk.push_items_back(['a']);

        assert_items_eq!(linked_chunk, ['a']);

        linked_chunk.push_items_back(['b', 'c']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c']);

        linked_chunk.push_items_back(['d', 'e']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e']);

        linked_chunk.push_items_back(['f', 'g', 'h', 'i', 'j']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e', 'f'] ['g', 'h', 'i'] ['j']);

        assert_eq!(linked_chunk.len(), 10);
    }

    #[test]
    fn test_push_gap() {
        let mut linked_chunk = LinkedChunk::<char, (), 3>::new();
        linked_chunk.push_items_back(['a']);
        assert_items_eq!(linked_chunk, ['a']);

        linked_chunk.push_gap_back(());
        assert_items_eq!(linked_chunk, ['a'] [-]);

        linked_chunk.push_items_back(['b', 'c', 'd', 'e']);
        assert_items_eq!(linked_chunk, ['a'] [-] ['b', 'c', 'd'] ['e']);

        linked_chunk.push_gap_back(());
        linked_chunk.push_gap_back(()); // why not
        assert_items_eq!(linked_chunk, ['a'] [-] ['b', 'c', 'd'] ['e'] [-] [-]);

        linked_chunk.push_items_back(['f', 'g', 'h', 'i']);
        assert_items_eq!(linked_chunk, ['a'] [-] ['b', 'c', 'd'] ['e'] [-] [-] ['f', 'g', 'h'] ['i']);

        assert_eq!(linked_chunk.len(), 9);
    }

    #[test]
    fn test_identifiers_and_positions() {
        let mut linked_chunk = LinkedChunk::<char, (), 3>::new();
        linked_chunk.push_items_back(['a', 'b', 'c', 'd', 'e', 'f']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['g', 'h', 'i', 'j']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e', 'f'] [-] ['g', 'h', 'i'] ['j']);

        assert_eq!(linked_chunk.chunk_identifier(Chunk::is_gap), Some(ChunkIdentifier(2)));
        assert_eq!(
            linked_chunk.item_position(|item| *item == 'e'),
            Some(ItemPosition(ChunkIdentifier(1), 1))
        );
    }

    #[test]
    fn test_rchunks() {
        let mut linked_chunk = LinkedChunk::<char, (), 2>::new();
        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['c', 'd', 'e']);

        let mut iterator = linked_chunk.rchunks();

        assert_matches!(
            iterator.next(),
            Some(Chunk { identifier: ChunkIdentifier(3), content: ChunkContent::Items(items), .. }) => {
                assert_eq!(items, &['e']);
            }
        );
        assert_matches!(
            iterator.next(),
            Some(Chunk { identifier: ChunkIdentifier(2), content: ChunkContent::Items(items), .. }) => {
                assert_eq!(items, &['c', 'd']);
            }
        );
        assert_matches!(
            iterator.next(),
            Some(Chunk { identifier: ChunkIdentifier(1), content: ChunkContent::Gap(..), .. })
        );
        assert_matches!(
            iterator.next(),
            Some(Chunk { identifier: ChunkIdentifier(0), content: ChunkContent::Items(items), .. }) => {
                assert_eq!(items, &['a', 'b']);
            }
        );
        assert_matches!(iterator.next(), None);
    }

    #[test]
    fn test_rchunks_from() -> Result<(), LinkedChunkError> {
        let mut linked_chunk = LinkedChunk::<char, (), 2>::new();
        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['c', 'd', 'e']);

        let mut iterator = linked_chunk.rchunks_from(
            linked_chunk.item_position(|item| *item == 'c').unwrap().chunk_identifier(),
        )?;

        assert_matches!(
            iterator.next(),
            Some(Chunk { identifier: ChunkIdentifier(2), content: ChunkContent::Items(items), .. }) => {
                assert_eq!(items, &['c', 'd']);
            }
        );
        assert_matches!(
            iterator.next(),
            Some(Chunk { identifier: ChunkIdentifier(1), content: ChunkContent::Gap(..), .. })
        );
        assert_matches!(
            iterator.next(),
            Some(Chunk { identifier: ChunkIdentifier(0), content: ChunkContent::Items(items), .. }) => {
                assert_eq!(items, &['a', 'b']);
            }
        );
        assert_matches!(iterator.next(), None);

        Ok(())
    }

    #[test]
    fn test_chunks_from() -> Result<(), LinkedChunkError> {
        let mut linked_chunk = LinkedChunk::<char, (), 2>::new();
        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['c', 'd', 'e']);

        let mut iterator = linked_chunk.chunks_from(
            linked_chunk.item_position(|item| *item == 'c').unwrap().chunk_identifier(),
        )?;

        assert_matches!(
            iterator.next(),
            Some(Chunk { identifier: ChunkIdentifier(2), content: ChunkContent::Items(items), .. }) => {
                assert_eq!(items, &['c', 'd']);
            }
        );
        assert_matches!(
            iterator.next(),
            Some(Chunk { identifier: ChunkIdentifier(3), content: ChunkContent::Items(items), .. }) => {
                assert_eq!(items, &['e']);
            }
        );
        assert_matches!(iterator.next(), None);

        Ok(())
    }

    #[test]
    fn test_ritems() {
        let mut linked_chunk = LinkedChunk::<char, (), 2>::new();
        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['c', 'd', 'e']);

        let mut iterator = linked_chunk.ritems();

        assert_matches!(iterator.next(), Some((ItemPosition(ChunkIdentifier(3), 0), 'e')));
        assert_matches!(iterator.next(), Some((ItemPosition(ChunkIdentifier(2), 0), 'd')));
        assert_matches!(iterator.next(), Some((ItemPosition(ChunkIdentifier(2), 1), 'c')));
        assert_matches!(iterator.next(), Some((ItemPosition(ChunkIdentifier(0), 0), 'b')));
        assert_matches!(iterator.next(), Some((ItemPosition(ChunkIdentifier(0), 1), 'a')));
        assert_matches!(iterator.next(), None);
    }

    #[test]
    fn test_items() {
        let mut linked_chunk = LinkedChunk::<char, (), 2>::new();
        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['c', 'd', 'e']);

        let mut iterator = linked_chunk.items();

        assert_matches!(iterator.next(), Some((ItemPosition(ChunkIdentifier(0), 1), 'a')));
        assert_matches!(iterator.next(), Some((ItemPosition(ChunkIdentifier(0), 0), 'b')));
        assert_matches!(iterator.next(), Some((ItemPosition(ChunkIdentifier(2), 1), 'c')));
        assert_matches!(iterator.next(), Some((ItemPosition(ChunkIdentifier(2), 0), 'd')));
        assert_matches!(iterator.next(), Some((ItemPosition(ChunkIdentifier(3), 0), 'e')));
        assert_matches!(iterator.next(), None);
    }

    #[test]
    fn test_ritems_from() -> Result<(), LinkedChunkError> {
        let mut linked_chunk = LinkedChunk::<char, (), 2>::new();
        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['c', 'd', 'e']);

        let mut iterator =
            linked_chunk.ritems_from(linked_chunk.item_position(|item| *item == 'c').unwrap())?;

        assert_matches!(iterator.next(), Some((ItemPosition(ChunkIdentifier(2), 1), 'c')));
        assert_matches!(iterator.next(), Some((ItemPosition(ChunkIdentifier(0), 0), 'b')));
        assert_matches!(iterator.next(), Some((ItemPosition(ChunkIdentifier(0), 1), 'a')));
        assert_matches!(iterator.next(), None);

        Ok(())
    }

    #[test]
    fn test_items_from() -> Result<(), LinkedChunkError> {
        let mut linked_chunk = LinkedChunk::<char, (), 2>::new();
        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['c', 'd', 'e']);

        let mut iterator =
            linked_chunk.items_from(linked_chunk.item_position(|item| *item == 'c').unwrap())?;

        assert_matches!(iterator.next(), Some((ItemPosition(ChunkIdentifier(2), 1), 'c')));
        assert_matches!(iterator.next(), Some((ItemPosition(ChunkIdentifier(2), 0), 'd')));
        assert_matches!(iterator.next(), Some((ItemPosition(ChunkIdentifier(3), 0), 'e')));
        assert_matches!(iterator.next(), None);

        Ok(())
    }

    #[test]
    fn test_insert_items_at() -> Result<(), LinkedChunkError> {
        let mut linked_chunk = LinkedChunk::<char, (), 3>::new();
        linked_chunk.push_items_back(['a', 'b', 'c', 'd', 'e', 'f']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e', 'f']);

        // Insert inside the last chunk.
        {
            let position_of_e = linked_chunk.item_position(|item| *item == 'e').unwrap();

            // Insert 4 elements, so that it overflows the chunk capacity. It's important to
            // see whether chunks are correctly updated and linked.
            linked_chunk.insert_items_at(['w', 'x', 'y', 'z'], position_of_e)?;

            assert_items_eq!(
                linked_chunk,
                ['a', 'b', 'c'] ['d', 'w', 'x'] ['y', 'z', 'e'] ['f']
            );
            assert_eq!(linked_chunk.len(), 10);
        }

        // Insert inside the first chunk.
        {
            let position_of_a = linked_chunk.item_position(|item| *item == 'a').unwrap();
            linked_chunk.insert_items_at(['l', 'm', 'n', 'o'], position_of_a)?;

            assert_items_eq!(
                linked_chunk,
                ['l', 'm', 'n'] ['o', 'a', 'b'] ['c'] ['d', 'w', 'x'] ['y', 'z', 'e'] ['f']
            );
            assert_eq!(linked_chunk.len(), 14);
        }

        // Insert inside a middle chunk.
        {
            let position_of_c = linked_chunk.item_position(|item| *item == 'c').unwrap();
            linked_chunk.insert_items_at(['r', 's'], position_of_c)?;

            assert_items_eq!(
                linked_chunk,
                ['l', 'm', 'n'] ['o', 'a', 'b'] ['r', 's', 'c'] ['d', 'w', 'x'] ['y', 'z', 'e'] ['f']
            );
            assert_eq!(linked_chunk.len(), 16);
        }

        // Insert in a chunk that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(['u', 'v'], ItemPosition(ChunkIdentifier(128), 0)),
                Err(LinkedChunkError::InvalidChunkIdentifier { identifier: ChunkIdentifier(128) })
            );
        }

        // Insert in a chunk that exists, but at an item that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(['u', 'v'], ItemPosition(ChunkIdentifier(0), 128)),
                Err(LinkedChunkError::InvalidItemIndex { index: 128 })
            );
        }

        // Insert in a gap.
        {
            // Add a gap to test the error.
            linked_chunk.push_gap_back(());
            assert_items_eq!(
                linked_chunk,
                ['l', 'm', 'n'] ['o', 'a', 'b'] ['r', 's', 'c'] ['d', 'w', 'x'] ['y', 'z', 'e'] ['f'] [-]
            );

            assert_matches!(
                linked_chunk.insert_items_at(['u', 'v'], ItemPosition(ChunkIdentifier(6), 0),),
                Err(LinkedChunkError::ChunkIsAGap { identifier: ChunkIdentifier(6) })
            );
        }

        assert_eq!(linked_chunk.len(), 16);

        Ok(())
    }

    #[test]
    fn test_insert_gap_at() -> Result<(), LinkedChunkError> {
        let mut linked_chunk = LinkedChunk::<char, (), 3>::new();
        linked_chunk.push_items_back(['a', 'b', 'c', 'd', 'e', 'f']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e', 'f']);

        // Insert in the middle of a chunk.
        {
            let position_of_b = linked_chunk.item_position(|item| *item == 'b').unwrap();
            linked_chunk.insert_gap_at(position_of_b, ())?;

            assert_items_eq!(linked_chunk, ['a'] [-] ['b', 'c'] ['d', 'e', 'f']);
        }

        // Insert at the beginning of a chunk.
        {
            let position_of_a = linked_chunk.item_position(|item| *item == 'a').unwrap();
            linked_chunk.insert_gap_at(position_of_a, ())?;

            assert_items_eq!(linked_chunk, [] [-] ['a'] [-] ['b', 'c'] ['d', 'e', 'f']);
        }

        // Insert in a chunk that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(['u', 'v'], ItemPosition(ChunkIdentifier(128), 0)),
                Err(LinkedChunkError::InvalidChunkIdentifier { identifier: ChunkIdentifier(128) })
            );
        }

        // Insert in a chunk that exists, but at an item that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(['u', 'v'], ItemPosition(ChunkIdentifier(0), 128)),
                Err(LinkedChunkError::InvalidItemIndex { index: 128 })
            );
        }

        // Insert in an existing gap.
        {
            // It is impossible to get the item position inside a gap. It's only possible if
            // the item position is crafted by hand or is outdated.
            let position_of_a_gap = ItemPosition(ChunkIdentifier(4), 0);
            assert_matches!(
                linked_chunk.insert_gap_at(position_of_a_gap, ()),
                Err(LinkedChunkError::ChunkIsAGap { identifier: ChunkIdentifier(4) })
            );
        }

        assert_eq!(linked_chunk.len(), 6);

        Ok(())
    }

    #[test]
    fn test_replace_gap_at() -> Result<(), LinkedChunkError> {
        let mut linked_chunk = LinkedChunk::<char, (), 3>::new();
        linked_chunk.push_items_back(['a', 'b', 'c']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['l', 'm', 'n']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] [-] ['l', 'm', 'n']);

        // Replace a gap in the middle of the linked chunk.
        {
            let gap_identifier = linked_chunk.chunk_identifier(Chunk::is_gap).unwrap();
            assert_eq!(gap_identifier, ChunkIdentifier(1));

            linked_chunk.replace_gap_at(['d', 'e', 'f', 'g', 'h'], gap_identifier)?;
            assert_items_eq!(
                linked_chunk,
                ['a', 'b', 'c'] ['d', 'e', 'f'] ['g', 'h'] ['l', 'm', 'n']
            );
        }

        // Replace a gap at the end of the linked chunk.
        {
            linked_chunk.push_gap_back(());
            assert_items_eq!(
                linked_chunk,
                ['a', 'b', 'c'] ['d', 'e', 'f'] ['g', 'h'] ['l', 'm', 'n'] [-]
            );

            let gap_identifier = linked_chunk.chunk_identifier(Chunk::is_gap).unwrap();
            assert_eq!(gap_identifier, ChunkIdentifier(5));

            linked_chunk.replace_gap_at(['w', 'x', 'y', 'z'], gap_identifier)?;
            assert_items_eq!(
                linked_chunk,
                ['a', 'b', 'c'] ['d', 'e', 'f'] ['g', 'h'] ['l', 'm', 'n'] ['w', 'x', 'y'] ['z']
            );
        }

        assert_eq!(linked_chunk.len(), 15);

        Ok(())
    }
}
