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

//! A linked chunk is the underlying data structure that holds all events.

/// A macro to test the items and the gap of a `LinkedChunk`.
/// A chunk is delimited by `[` and `]`. An item chunk has the form `[a, b,
/// c]` where `a`, `b` and `c` are items. A gap chunk has the form `[-]`.
///
/// For example, here is an assertion of 7 chunks: 1 items chunk, 1 gap
/// chunk, 2 items chunks, 1 gap chunk, 2 items chunk. `a` is the oldest
/// item of the oldest chunk (the first chunk), and `i` is the oldest (and
/// newest) item of the newest chunk (the last chunk).
///
/// ```rust,no_run
/// assert_items_eq!(linked_chunk, ['a'] [-] ['b', 'c', 'd'] ['e'] [-] ['f', 'g', 'h'] ['i']);
/// ```
#[cfg(test)]
macro_rules! assert_items_eq {
    ( @_ [ $iterator:ident ] { [-] $( $rest:tt )* } { $( $accumulator:tt )* } ) => {
        assert_items_eq!(
            @_
            [ $iterator ]
            { $( $rest )* }
            {
                $( $accumulator )*
                {
                    let chunk = $iterator .next().expect("next chunk (expect gap)");
                    assert!(chunk.is_gap(), "chunk should be a gap");
                }
            }
        )
    };

    ( @_ [ $iterator:ident ] { [ $( $item:expr ),* ] $( $rest:tt )* } { $( $accumulator:tt )* } ) => {
        assert_items_eq!(
            @_
            [ $iterator ]
            { $( $rest )* }
            {
                $( $accumulator )*
                {
                    let chunk = $iterator .next().expect("next chunk (expect items)");
                    assert!(chunk.is_items(), "chunk should contain items");

                    let $crate::event_cache::linked_chunk::ChunkContent::Items(items) = chunk.content() else {
                        unreachable!()
                    };

                    let mut items_iterator = items.iter();

                    $(
                        assert_eq!(items_iterator.next(), Some(& $item ));
                    )*

                    assert!(items_iterator.next().is_none(), "no more items");
                }
            }
        )
    };

    ( @_ [ $iterator:ident ] {} { $( $accumulator:tt )* } ) => {
        {
            $( $accumulator )*
            assert!( $iterator .next().is_none(), "no more chunks");
        }
    };

    ( $linked_chunk:expr, $( $all:tt )* ) => {
        assert_items_eq!(
            @_
            [ iterator ]
            { $( $all )* }
            {
                let mut iterator = $linked_chunk.chunks();
            }
        )
    }
}

mod as_vector;
mod updates;

use std::{
    fmt,
    marker::PhantomData,
    ops::Not,
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

use as_vector::*;
use updates::*;

/// Errors of [`LinkedChunk`].
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// A chunk identifier is invalid.
    #[error("The chunk identifier is invalid: `{identifier:?}`")]
    InvalidChunkIdentifier {
        /// The chunk identifier.
        identifier: ChunkIdentifier,
    },

    /// A chunk is a gap, and it was expected to be an items.
    #[error("The chunk is a gap: `{identifier:?}`")]
    ChunkIsAGap {
        /// The chunk identifier.
        identifier: ChunkIdentifier,
    },

    /// A chunk is an items, and it was expected to be a gap.
    #[error("The chunk is an item: `{identifier:?}`")]
    ChunkIsItems {
        /// The chunk identifier.
        identifier: ChunkIdentifier,
    },

    /// An item index is invalid.
    #[error("The item index is invalid: `{index}`")]
    InvalidItemIndex {
        /// The index.
        index: usize,
    },
}

/// Links of a `LinkedChunk`, i.e. the first and last [`Chunk`].
///
/// This type was introduced to avoid borrow checking errors when mutably
/// referencing a subset of fields of a `LinkedChunk`.
struct Ends<const CHUNK_CAPACITY: usize, Item, Gap> {
    /// The first chunk.
    first: NonNull<Chunk<CHUNK_CAPACITY, Item, Gap>>,
    /// The last chunk.
    last: Option<NonNull<Chunk<CHUNK_CAPACITY, Item, Gap>>>,
}

impl<const CAP: usize, Item, Gap> Ends<CAP, Item, Gap> {
    /// Get the first chunk, as an immutable reference.
    fn first_chunk(&self) -> &Chunk<CAP, Item, Gap> {
        unsafe { self.first.as_ref() }
    }

    /// Get the latest chunk, as an immutable reference.
    fn latest_chunk(&self) -> &Chunk<CAP, Item, Gap> {
        unsafe { self.last.unwrap_or(self.first).as_ref() }
    }

    /// Get the latest chunk, as a mutable reference.
    fn latest_chunk_mut(&mut self) -> &mut Chunk<CAP, Item, Gap> {
        unsafe { self.last.as_mut().unwrap_or(&mut self.first).as_mut() }
    }

    /// Get the chunk as a reference, from its identifier, if it exists.
    fn chunk(&self, identifier: ChunkIdentifier) -> Option<&Chunk<CAP, Item, Gap>> {
        let mut chunk = self.latest_chunk();

        loop {
            if chunk.identifier() == identifier {
                return Some(chunk);
            }

            chunk = chunk.previous()?;
        }
    }

    /// Get the chunk as a mutable reference, from its identifier, if it exists.
    fn chunk_mut(&mut self, identifier: ChunkIdentifier) -> Option<&mut Chunk<CAP, Item, Gap>> {
        let mut chunk = self.latest_chunk_mut();

        loop {
            if chunk.identifier() == identifier {
                return Some(chunk);
            }

            chunk = chunk.previous_mut()?;
        }
    }
}

/// The [`LinkedChunk`] structure.
///
/// It is similar to a linked list, except that it contains many items `Item`
/// instead of a single one. A chunk has a maximum capacity of `CHUNK_CAPACITY`.
/// Once a chunk is full, a new chunk is created. Not all chunks are necessarily
/// entirely full. A chunk can represents a `Gap` between other chunks.
pub struct LinkedChunk<const CHUNK_CAPACITY: usize, Item, Gap> {
    /// The links to the chunks, i.e. the first and the last chunk.
    links: Ends<CHUNK_CAPACITY, Item, Gap>,

    /// The number of items hold by this linked chunk.
    length: usize,

    /// The generator of chunk identifiers.
    chunk_identifier_generator: ChunkIdentifierGenerator,

    /// All updates that have been made on this `LinkedChunk`. If this field is
    /// `Some(…)`, update history is enabled, otherwise, if it's `None`, update
    /// history is disabled.
    updates: Option<ObservableUpdates<Item, Gap>>,

    /// Marker.
    marker: PhantomData<Box<Chunk<CHUNK_CAPACITY, Item, Gap>>>,
}

impl<const CAP: usize, Item, Gap> LinkedChunk<CAP, Item, Gap> {
    /// Create a new [`Self`].
    pub fn new() -> Self {
        Self {
            links: Ends {
                // INVARIANT: The first chunk must always be an Items, not a Gap.
                first: Chunk::new_items_leaked(ChunkIdentifierGenerator::FIRST_IDENTIFIER),
                last: None,
            },
            length: 0,
            chunk_identifier_generator: ChunkIdentifierGenerator::new_from_scratch(),
            updates: None,
            marker: PhantomData,
        }
    }

    /// Create a new [`Self`] with a history of updates.
    ///
    /// When [`Self`] is built with update history, the
    /// [`ObservableUpdates::take`] method must be called to consume and
    /// clean the updates. See [`Self::updates`].
    pub fn new_with_update_history() -> Self {
        Self {
            links: Ends {
                // INVARIANT: The first chunk must always be an Items, not a Gap.
                first: Chunk::new_items_leaked(ChunkIdentifierGenerator::FIRST_IDENTIFIER),
                last: None,
            },
            length: 0,
            chunk_identifier_generator: ChunkIdentifierGenerator::new_from_scratch(),
            updates: Some(ObservableUpdates::new()),
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
        Item: Clone,
        Gap: Clone,
        I: IntoIterator<Item = Item>,
        I::IntoIter: ExactSizeIterator,
    {
        let items = items.into_iter();
        let number_of_items = items.len();

        let last_chunk = self.links.latest_chunk_mut();

        // Push the items.
        let last_chunk =
            last_chunk.push_items(items, &self.chunk_identifier_generator, &mut self.updates);

        debug_assert!(last_chunk.is_last_chunk(), "`last_chunk` must be… the last chunk");

        // We need to update `self.last` if and only if `last_chunk` _is not_ the first
        // chunk, and _is_ the last chunk (ensured by the `debug_assert!` above).
        if last_chunk.is_first_chunk().not() {
            // Maybe `last_chunk` is the same as the previous `self.last` chunk, but it's
            // OK.
            self.links.last = Some(last_chunk.as_ptr());
        }

        self.length += number_of_items;
    }

    /// Push a gap at the end of the [`LinkedChunk`], i.e. after the last
    /// chunk.
    pub fn push_gap_back(&mut self, content: Gap)
    where
        Item: Clone,
        Gap: Clone,
    {
        let last_chunk = self.links.latest_chunk_mut();
        last_chunk.insert_next(
            Chunk::new_gap_leaked(self.chunk_identifier_generator.next(), content),
            &mut self.updates,
        );

        self.links.last = last_chunk.next;
    }

    /// Insert items at a specified position in the [`LinkedChunk`].
    ///
    /// Because the `position` can be invalid, this method returns a
    /// `Result`.
    pub fn insert_items_at<I>(&mut self, items: I, position: Position) -> Result<(), Error>
    where
        Item: Clone,
        Gap: Clone,
        I: IntoIterator<Item = Item>,
        I::IntoIter: ExactSizeIterator,
    {
        let chunk_identifier = position.chunk_identifier();
        let item_index = position.index();

        let chunk = self
            .links
            .chunk_mut(chunk_identifier)
            .ok_or(Error::InvalidChunkIdentifier { identifier: chunk_identifier })?;

        let (chunk, number_of_items) = match &mut chunk.content {
            ChunkContent::Gap(..) => {
                return Err(Error::ChunkIsAGap { identifier: chunk_identifier })
            }

            ChunkContent::Items(current_items) => {
                let current_items_length = current_items.len();

                if item_index > current_items_length {
                    return Err(Error::InvalidItemIndex { index: item_index });
                }

                // Prepare the items to be pushed.
                let items = items.into_iter();
                let number_of_items = items.len();

                (
                    // Push at the end of the current items.
                    if item_index == current_items_length {
                        chunk
                            // Push the new items.
                            .push_items(items, &self.chunk_identifier_generator, &mut self.updates)
                    }
                    // Insert inside the current items.
                    else {
                        if let Some(updates) = self.updates.as_mut() {
                            updates.push(Update::DetachLastItems {
                                at: Position(chunk_identifier, item_index),
                            });
                        }

                        // Split the items.
                        let detached_items = current_items.split_off(item_index);

                        let chunk = chunk
                            // Push the new items.
                            .push_items(items, &self.chunk_identifier_generator, &mut self.updates);

                        if let Some(updates) = self.updates.as_mut() {
                            updates.push(Update::StartReattachItems);
                        }

                        let chunk = chunk
                            // Finally, push the items that have been detached.
                            .push_items(
                                detached_items.into_iter(),
                                &self.chunk_identifier_generator,
                                &mut self.updates,
                            );

                        if let Some(updates) = self.updates.as_mut() {
                            updates.push(Update::EndReattachItems);
                        }

                        chunk
                    },
                    number_of_items,
                )
            }
        };

        // We need to update `self.last` if and only if `chunk` _is not_ the first
        // chunk, and _is_ the last chunk.
        if chunk.is_first_chunk().not() && chunk.is_last_chunk() {
            // Maybe `chunk` is the same as the previous `self.last` chunk, but it's
            // OK.
            self.links.last = Some(chunk.as_ptr());
        }

        self.length += number_of_items;

        Ok(())
    }

    /// Insert a gap at a specified position in the [`LinkedChunk`].
    ///
    /// Because the `position` can be invalid, this method returns a
    /// `Result`.
    pub fn insert_gap_at(&mut self, content: Gap, position: Position) -> Result<(), Error>
    where
        Item: Clone,
        Gap: Clone,
    {
        let chunk_identifier = position.chunk_identifier();
        let item_index = position.index();

        let chunk = self
            .links
            .chunk_mut(chunk_identifier)
            .ok_or(Error::InvalidChunkIdentifier { identifier: chunk_identifier })?;

        // If `item_index` is 0, we don't want to split the current items chunk to
        // insert a new gap chunk, otherwise it would create an empty current items
        // chunk. Let's handle this case in particular.
        //
        // Of course this optimisation applies if there is a previous chunk. Remember
        // the invariant: a `Gap` cannot be the first chunk.
        if item_index == 0 && chunk.is_items() && chunk.previous.is_some() {
            let previous_chunk = chunk
                .previous_mut()
                // SAFETY: The `previous` chunk exists because we have tested
                // `chunk.previous.is_some()` in the `if` statement.
                .expect("Previous chunk must be present");

            previous_chunk.insert_next(
                Chunk::new_gap_leaked(self.chunk_identifier_generator.next(), content),
                &mut self.updates,
            );

            // We don't need to update `self.last` because we have inserted a new chunk
            // before `chunk`.

            return Ok(());
        }

        let chunk = match &mut chunk.content {
            ChunkContent::Gap(..) => {
                return Err(Error::ChunkIsAGap { identifier: chunk_identifier });
            }

            ChunkContent::Items(current_items) => {
                let current_items_length = current_items.len();

                if item_index >= current_items_length {
                    return Err(Error::InvalidItemIndex { index: item_index });
                }

                if let Some(updates) = self.updates.as_mut() {
                    updates.push(Update::DetachLastItems {
                        at: Position(chunk_identifier, item_index),
                    });
                }

                // Split the items.
                let detached_items = current_items.split_off(item_index);

                let chunk = chunk
                    // Insert a new gap chunk.
                    .insert_next(
                        Chunk::new_gap_leaked(self.chunk_identifier_generator.next(), content),
                        &mut self.updates,
                    );

                if let Some(updates) = self.updates.as_mut() {
                    updates.push(Update::StartReattachItems);
                }

                let chunk = chunk
                    // Insert a new items chunk.
                    .insert_next(
                        Chunk::new_items_leaked(self.chunk_identifier_generator.next()),
                        &mut self.updates,
                    )
                    // Finally, push the items that have been detached.
                    .push_items(
                        detached_items.into_iter(),
                        &self.chunk_identifier_generator,
                        &mut self.updates,
                    );

                if let Some(updates) = self.updates.as_mut() {
                    updates.push(Update::EndReattachItems);
                }

                chunk
            }
        };

        // We need to update `self.last` if and only if `chunk` _is not_ the first
        // chunk, and _is_ the last chunk.
        if chunk.is_first_chunk().not() && chunk.is_last_chunk() {
            // Maybe `chunk` is the same as the previous `self.last` chunk, but it's
            // OK.
            self.links.last = Some(chunk.as_ptr());
        }

        Ok(())
    }

    /// Replace the gap identified by `chunk_identifier`, by items.
    ///
    /// Because the `chunk_identifier` can represent non-gap chunk, this method
    /// returns a `Result`.
    ///
    /// This method returns a reference to the (first if many) newly created
    /// `Chunk` that contains the `items`.
    pub fn replace_gap_at<I>(
        &mut self,
        items: I,
        chunk_identifier: ChunkIdentifier,
    ) -> Result<&Chunk<CAP, Item, Gap>, Error>
    where
        Item: Clone,
        Gap: Clone,
        I: IntoIterator<Item = Item>,
        I::IntoIter: ExactSizeIterator,
    {
        let chunk_ptr;
        let new_chunk_ptr;

        {
            let chunk = self
                .links
                .chunk_mut(chunk_identifier)
                .ok_or(Error::InvalidChunkIdentifier { identifier: chunk_identifier })?;

            debug_assert!(chunk.is_first_chunk().not(), "A gap cannot be the first chunk");

            let (maybe_last_chunk_ptr, number_of_items) = match &mut chunk.content {
                ChunkContent::Gap(..) => {
                    let items = items.into_iter();
                    let number_of_items = items.len();

                    let last_inserted_chunk = chunk
                        // Insert a new items chunk…
                        .insert_next(
                            Chunk::new_items_leaked(self.chunk_identifier_generator.next()),
                            &mut self.updates,
                        )
                        // … and insert the items.
                        .push_items(items, &self.chunk_identifier_generator, &mut self.updates);

                    (
                        last_inserted_chunk.is_last_chunk().then(|| last_inserted_chunk.as_ptr()),
                        number_of_items,
                    )
                }
                ChunkContent::Items(..) => {
                    return Err(Error::ChunkIsItems { identifier: chunk_identifier })
                }
            };

            new_chunk_ptr = chunk
                .next
                // SAFETY: A new `Chunk` has just been inserted, so it exists.
                .unwrap();

            // Now that new items have been pushed, we can unlink the gap chunk.
            chunk.unlink(&mut self.updates);

            // Get the pointer to `chunk`.
            chunk_ptr = chunk.as_ptr();

            // Update `self.last` if the gap chunk was the last chunk.
            if let Some(last_chunk_ptr) = maybe_last_chunk_ptr {
                self.links.last = Some(last_chunk_ptr);
            }

            self.length += number_of_items;

            // Stop borrowing `chunk`.
        }

        // Re-box the chunk, and let Rust does its job.
        //
        // SAFETY: `chunk` is unlinked and not borrowed anymore. `LinkedChunk` doesn't
        // use it anymore, it's a leak. It is time to re-`Box` it and drop it.
        let _chunk_boxed = unsafe { Box::from_raw(chunk_ptr.as_ptr()) };

        Ok(
            // SAFETY: `new_chunk_ptr` is valid, non-null and well-aligned. It's taken from
            // `chunk`, and that's how the entire `LinkedChunk` type works. Pointer construction
            // safety is guaranteed by `Chunk::new_items_leaked` and `Chunk::new_gap_leaked`.
            unsafe { new_chunk_ptr.as_ref() },
        )
    }

    /// Search backwards for a chunk, and return its identifier.
    pub fn chunk_identifier<'a, P>(&'a self, mut predicate: P) -> Option<ChunkIdentifier>
    where
        P: FnMut(&'a Chunk<CAP, Item, Gap>) -> bool,
    {
        self.rchunks().find_map(|chunk| predicate(chunk).then(|| chunk.identifier()))
    }

    /// Search backwards for an item, and return its position.
    pub fn item_position<'a, P>(&'a self, mut predicate: P) -> Option<Position>
    where
        P: FnMut(&'a Item) -> bool,
    {
        self.ritems().find_map(|(item_position, item)| predicate(item).then_some(item_position))
    }

    /// Iterate over the chunks, backwards.
    ///
    /// It iterates from the last to the first chunk.
    pub fn rchunks(&self) -> IterBackward<'_, CAP, Item, Gap> {
        IterBackward::new(self.links.latest_chunk())
    }

    /// Iterate over the chunks, forward.
    ///
    /// It iterates from the first to the last chunk.
    pub fn chunks(&self) -> Iter<'_, CAP, Item, Gap> {
        Iter::new(self.links.first_chunk())
    }

    /// Iterate over the chunks, starting from `identifier`, backward.
    ///
    /// It iterates from the chunk with the identifier `identifier` to the first
    /// chunk.
    pub fn rchunks_from(
        &self,
        identifier: ChunkIdentifier,
    ) -> Result<IterBackward<'_, CAP, Item, Gap>, Error> {
        Ok(IterBackward::new(
            self.links.chunk(identifier).ok_or(Error::InvalidChunkIdentifier { identifier })?,
        ))
    }

    /// Iterate over the chunks, starting from `position`, forward.
    ///
    /// It iterates from the chunk with the identifier `identifier` to the last
    /// chunk.
    pub fn chunks_from(
        &self,
        identifier: ChunkIdentifier,
    ) -> Result<Iter<'_, CAP, Item, Gap>, Error> {
        Ok(Iter::new(
            self.links.chunk(identifier).ok_or(Error::InvalidChunkIdentifier { identifier })?,
        ))
    }

    /// Iterate over the items, backward.
    ///
    /// It iterates from the last to the first item.
    pub fn ritems(&self) -> impl Iterator<Item = (Position, &Item)> {
        self.ritems_from(self.links.latest_chunk().last_position())
            .expect("`ritems_from` cannot fail because at least one empty chunk must exist")
    }

    /// Iterate over the items, forward.
    ///
    /// It iterates from the first to the last item.
    pub fn items(&self) -> impl Iterator<Item = (Position, &Item)> {
        let first_chunk = self.links.first_chunk();

        self.items_from(first_chunk.first_position())
            .expect("`items` cannot fail because at least one empty chunk must exist")
    }

    /// Iterate over the items, starting from `position`, backward.
    ///
    /// It iterates from the item at `position` to the first item.
    pub fn ritems_from(
        &self,
        position: Position,
    ) -> Result<impl Iterator<Item = (Position, &Item)>, Error> {
        Ok(self
            .rchunks_from(position.chunk_identifier())?
            .filter_map(|chunk| match &chunk.content {
                ChunkContent::Gap(..) => None,
                ChunkContent::Items(items) => {
                    let identifier = chunk.identifier();

                    Some(
                        items.iter().enumerate().rev().map(move |(item_index, item)| {
                            (Position(identifier, item_index), item)
                        }),
                    )
                }
            })
            .flatten()
            .skip_while({
                let expected_index = position.index();

                move |(Position(_chunk_identifier, item_index), _item)| {
                    *item_index != expected_index
                }
            }))
    }

    /// Iterate over the items, starting from `position`, forward.
    ///
    /// It iterates from the item at `position` to the last item.
    pub fn items_from(
        &self,
        position: Position,
    ) -> Result<impl Iterator<Item = (Position, &Item)>, Error> {
        Ok(self
            .chunks_from(position.chunk_identifier())?
            .filter_map(|chunk| match &chunk.content {
                ChunkContent::Gap(..) => None,
                ChunkContent::Items(items) => {
                    let identifier = chunk.identifier();

                    Some(
                        items.iter().enumerate().map(move |(item_index, item)| {
                            (Position(identifier, item_index), item)
                        }),
                    )
                }
            })
            .flatten()
            .skip(position.index()))
    }

    /// Get a mutable reference to the `LinkedChunk` updates, aka
    /// [`ObservableUpdates`].
    ///
    /// If the `Option` becomes `None`, it will disable update history. Thus, be
    /// careful when you want to empty the update history: do not use
    /// `Option::take()` directly but rather [`ObservableUpdates::take`] for
    /// example.
    ///
    /// It returns `None` if updates are disabled, i.e. if this linked chunk has
    /// been constructed with [`Self::new`], otherwise, if it's been constructed
    /// with [`Self::new_with_update_history`], it returns `Some(…)`.
    pub fn updates(&mut self) -> Option<&mut ObservableUpdates<Item, Gap>> {
        self.updates.as_mut()
    }

    /// Get updates as [`eyeball_im::VectorDiff`], see [`AsVector`] to learn
    /// more.
    ///
    /// It returns `None` if updates are disabled, i.e. if this linked chunk has
    /// been constructed with [`Self::new`], otherwise, if it's been constructed
    /// with [`Self::new_with_update_history`], it returns `Some(…)`.
    pub fn as_vector(&mut self) -> Option<AsVector<Item, Gap>> {
        let (updates, token) = self
            .updates
            .as_mut()
            .map(|updates| (updates.inner.clone(), updates.new_reader_token()))?;
        let chunk_iterator = self.chunks();

        Some(AsVector::new(updates, token, chunk_iterator))
    }
}

impl<const CAP: usize, Item, Gap> Drop for LinkedChunk<CAP, Item, Gap> {
    fn drop(&mut self) {
        // Take the latest chunk.
        let mut current_chunk_ptr = self.links.last.or(Some(self.links.first));

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
unsafe impl<const CAP: usize, Item: Send, Gap: Send> Send for LinkedChunk<CAP, Item, Gap> {}

/// A [`LinkedChunk`] can be safely share between threads if `Item: Sync` and
/// `Gap: Sync`. The only unsafe part if around the `NonNull`, but the API and
/// the lifetimes to deref them are designed safely.
unsafe impl<const CAP: usize, Item: Sync, Gap: Sync> Sync for LinkedChunk<CAP, Item, Gap> {}

/// Generator for [`Chunk`]'s identifier.
///
/// Each [`Chunk`] has a unique identifier. This generator generates the unique
/// identifiers.
///
/// In order to keep good performance, a unique identifier is simply a `u64`
/// (see [`ChunkIdentifier`]). Generating a new unique identifier boils down to
/// incrementing by one the previous identifier. Note that this is not an index:
/// it _is_ an identifier.
struct ChunkIdentifierGenerator {
    next: AtomicU64,
}

impl ChunkIdentifierGenerator {
    /// The first identifier.
    const FIRST_IDENTIFIER: ChunkIdentifier = ChunkIdentifier(0);

    /// Create the generator assuming the current [`LinkedChunk`] it belongs to
    /// is empty.
    pub fn new_from_scratch() -> Self {
        Self { next: AtomicU64::new(Self::FIRST_IDENTIFIER.0) }
    }

    /// Create the generator assuming the current [`LinkedChunk`] it belongs to
    /// is not empty, i.e. it already has some [`Chunk`] in it.
    pub fn new_from_previous_chunk_identifier(last_chunk_identifier: ChunkIdentifier) -> Self {
        Self { next: AtomicU64::new(last_chunk_identifier.0) }
    }

    /// Generate the next unique identifier.
    ///
    /// Note that it can fail if there is no more unique identifier available.
    /// In this case, this method will panic.
    pub fn next(&self) -> ChunkIdentifier {
        let previous = self.next.fetch_add(1, Ordering::Relaxed);

        // Check for overflows.
        // unlikely — TODO: call `std::intrinsics::unlikely` once it's stable.
        if previous == u64::MAX {
            panic!("No more chunk identifiers available. Congrats, you did it. 2^64 identifiers have been consumed.")
        }

        ChunkIdentifier(previous + 1)
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

/// The position of something inside a [`Chunk`].
///
/// It's a pair of a chunk position and an item index.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Position(ChunkIdentifier, usize);

impl Position {
    /// Get the chunk identifier of the item.
    pub fn chunk_identifier(&self) -> ChunkIdentifier {
        self.0
    }

    /// Get the index inside the chunk.
    pub fn index(&self) -> usize {
        self.1
    }
}

/// An iterator over a [`LinkedChunk`] that traverses the chunk in backward
/// direction (i.e. it calls `previous` on each chunk to make progress).
pub struct IterBackward<'a, const CAP: usize, Item, Gap> {
    chunk: Option<&'a Chunk<CAP, Item, Gap>>,
}

impl<'a, const CAP: usize, Item, Gap> IterBackward<'a, CAP, Item, Gap> {
    /// Create a new [`LinkedChunkIter`] from a particular [`Chunk`].
    fn new(from_chunk: &'a Chunk<CAP, Item, Gap>) -> Self {
        Self { chunk: Some(from_chunk) }
    }
}

impl<'a, const CAP: usize, Item, Gap> Iterator for IterBackward<'a, CAP, Item, Gap> {
    type Item = &'a Chunk<CAP, Item, Gap>;

    fn next(&mut self) -> Option<Self::Item> {
        self.chunk.map(|chunk| {
            self.chunk = chunk.previous();

            chunk
        })
    }
}

/// An iterator over a [`LinkedChunk`] that traverses the chunk in forward
/// direction (i.e. it calls `next` on each chunk to make progress).
pub struct Iter<'a, const CAP: usize, Item, Gap> {
    chunk: Option<&'a Chunk<CAP, Item, Gap>>,
}

impl<'a, const CAP: usize, Item, Gap> Iter<'a, CAP, Item, Gap> {
    /// Create a new [`LinkedChunkIter`] from a particular [`Chunk`].
    fn new(from_chunk: &'a Chunk<CAP, Item, Gap>) -> Self {
        Self { chunk: Some(from_chunk) }
    }
}

impl<'a, const CAP: usize, Item, Gap> Iterator for Iter<'a, CAP, Item, Gap> {
    type Item = &'a Chunk<CAP, Item, Gap>;

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
pub struct Chunk<const CAPACITY: usize, Item, Gap> {
    /// The previous chunk.
    previous: Option<NonNull<Chunk<CAPACITY, Item, Gap>>>,

    /// The next chunk.
    next: Option<NonNull<Chunk<CAPACITY, Item, Gap>>>,

    /// Unique identifier.
    identifier: ChunkIdentifier,

    /// The content of the chunk.
    content: ChunkContent<Item, Gap>,
}

impl<const CAPACITY: usize, Item, Gap> Chunk<CAPACITY, Item, Gap> {
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

    /// Get the pointer to `Self`.
    pub fn as_ptr(&self) -> NonNull<Self> {
        NonNull::from(self)
    }

    /// Check whether this current chunk is a gap chunk.
    pub fn is_gap(&self) -> bool {
        matches!(self.content, ChunkContent::Gap(..))
    }

    /// Check whether this current chunk is an items  chunk.
    pub fn is_items(&self) -> bool {
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
    pub fn identifier(&self) -> ChunkIdentifier {
        self.identifier
    }

    /// Get the content of the chunk.
    pub fn content(&self) -> &ChunkContent<Item, Gap> {
        &self.content
    }

    /// Get the [`Position`] of the first item if any.
    ///
    /// If the `Chunk` is a `Gap`, it returns `0` for the index.
    pub fn first_position(&self) -> Position {
        Position(self.identifier(), 0)
    }

    /// Get the [`Position`] of the last item if any.
    ///
    /// If the `Chunk` is a `Gap`, it returns `0` for the index.
    pub fn last_position(&self) -> Position {
        let identifier = self.identifier();

        match &self.content {
            ChunkContent::Gap(..) => Position(identifier, 0),
            ChunkContent::Items(items) => Position(identifier, items.len() - 1),
        }
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
        updates: &mut Option<ObservableUpdates<Item, Gap>>,
    ) -> &mut Self
    where
        I: Iterator<Item = Item> + ExactSizeIterator,
        Item: Clone,
        Gap: Clone,
    {
        let number_of_new_items = new_items.len();
        let chunk_length = self.len();

        // A small optimisation. Skip early if there is no new items.
        if number_of_new_items == 0 {
            return self;
        }

        let identifier = self.identifier();

        match &mut self.content {
            // Cannot push items on a `Gap`. Let's insert a new `Items` chunk to push the
            // items onto it.
            ChunkContent::Gap(..) => {
                self
                    // Insert a new items chunk.
                    .insert_next(Self::new_items_leaked(chunk_identifier_generator.next()), updates)
                    // Now push the new items on the next chunk, and return the result of
                    // `push_items`.
                    .push_items(new_items, chunk_identifier_generator, updates)
            }

            ChunkContent::Items(items) => {
                // Calculate the free space of the current chunk.
                let free_space = CAPACITY.saturating_sub(chunk_length);

                // There is enough space to push all the new items.
                if number_of_new_items <= free_space {
                    let start = items.len();
                    items.extend(new_items);

                    if let Some(updates) = updates.as_mut() {
                        updates.push(Update::PushItems {
                            at: Position(identifier, start),
                            items: items[start..].to_vec(),
                        });
                    }

                    // Return the current chunk.
                    self
                } else {
                    if free_space > 0 {
                        // Take all possible items to fill the free space.
                        let start = items.len();
                        items.extend(new_items.by_ref().take(free_space));

                        if let Some(updates) = updates.as_mut() {
                            updates.push(Update::PushItems {
                                at: Position(identifier, start),
                                items: items[start..].to_vec(),
                            });
                        }
                    }

                    self
                        // Insert a new items chunk.
                        .insert_next(
                            Self::new_items_leaked(chunk_identifier_generator.next()),
                            updates,
                        )
                        // Now push the rest of the new items on the next chunk, and return the
                        // result of `push_items`.
                        .push_items(new_items, chunk_identifier_generator, updates)
                }
            }
        }
    }

    /// Insert a new chunk after the current one.
    ///
    /// The respective [`Self::previous`] and [`Self::next`] of the current
    /// and new chunk will be updated accordingly.
    fn insert_next(
        &mut self,
        mut new_chunk_ptr: NonNull<Self>,
        updates: &mut Option<ObservableUpdates<Item, Gap>>,
    ) -> &mut Self
    where
        Gap: Clone,
    {
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
        new_chunk.previous = Some(self.as_ptr());

        if let Some(updates) = updates.as_mut() {
            let previous = new_chunk.previous().map(Chunk::identifier);
            let new = new_chunk.identifier();
            let next = new_chunk.next().map(Chunk::identifier);

            match new_chunk.content() {
                ChunkContent::Gap(gap) => {
                    updates.push(Update::NewGapChunk { previous, new, next, gap: gap.clone() })
                }

                ChunkContent::Items(..) => {
                    updates.push(Update::NewItemsChunk { previous, new, next })
                }
            }
        }

        new_chunk
    }

    /// Unlink this chunk.
    ///
    /// Be careful: `self` won't belong to `LinkedChunk` anymore, and should be
    /// dropped appropriately.
    fn unlink(&mut self, updates: &mut Option<ObservableUpdates<Item, Gap>>) {
        let previous_ptr = self.previous;
        let next_ptr = self.next;

        if let Some(previous) = self.previous_mut() {
            previous.next = next_ptr;
        }

        if let Some(next) = self.next_mut() {
            next.previous = previous_ptr;
        }

        if let Some(updates) = updates.as_mut() {
            updates.push(Update::RemoveChunk(self.identifier()));
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

impl<const CAP: usize, Item, Gap> fmt::Debug for LinkedChunk<CAP, Item, Gap>
where
    Item: fmt::Debug,
    Gap: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        formatter
            .debug_struct("LinkedChunk")
            .field("first (deref)", unsafe { self.links.first.as_ref() })
            .field("last", &self.links.last)
            .field("length", &self.length)
            .finish_non_exhaustive()
    }
}

impl<const CAP: usize, Item, Gap> fmt::Debug for Chunk<CAP, Item, Gap>
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
        Chunk, ChunkContent, ChunkIdentifier, ChunkIdentifierGenerator, Error, LinkedChunk,
        Position,
    };

    #[test]
    fn test_chunk_identifier_generator() {
        let generator = ChunkIdentifierGenerator::new_from_scratch();

        assert_eq!(generator.next(), ChunkIdentifier(1));
        assert_eq!(generator.next(), ChunkIdentifier(2));
        assert_eq!(generator.next(), ChunkIdentifier(3));
        assert_eq!(generator.next(), ChunkIdentifier(4));

        let generator =
            ChunkIdentifierGenerator::new_from_previous_chunk_identifier(ChunkIdentifier(42));

        assert_eq!(generator.next(), ChunkIdentifier(43));
        assert_eq!(generator.next(), ChunkIdentifier(44));
        assert_eq!(generator.next(), ChunkIdentifier(45));
        assert_eq!(generator.next(), ChunkIdentifier(46));
    }

    #[test]
    fn test_empty() {
        let items = LinkedChunk::<3, char, ()>::new();

        assert_eq!(items.len(), 0);

        // This test also ensures that `Drop` for `LinkedChunk` works when
        // there is only one chunk.
    }

    #[test]
    fn test_updates() {
        assert!(LinkedChunk::<3, char, ()>::new().updates().is_none());
        assert!(LinkedChunk::<3, char, ()>::new_with_update_history().updates().is_some());
    }

    #[test]
    fn test_push_items() {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();
        linked_chunk.push_items_back(['a']);

        assert_items_eq!(linked_chunk, ['a']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a'] }]
        );

        linked_chunk.push_items_back(['b', 'c']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[PushItems { at: Position(ChunkIdentifier(0), 1), items: vec!['b', 'c'] }]
        );

        linked_chunk.push_items_back(['d', 'e']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(0)),
                    new: ChunkIdentifier(1),
                    next: None
                },
                PushItems { at: Position(ChunkIdentifier(1), 0), items: vec!['d', 'e'] }
            ]
        );

        linked_chunk.push_items_back(['f', 'g', 'h', 'i', 'j']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e', 'f'] ['g', 'h', 'i'] ['j']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                PushItems { at: Position(ChunkIdentifier(1), 2), items: vec!['f'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(1)),
                    new: ChunkIdentifier(2),
                    next: None,
                },
                PushItems { at: Position(ChunkIdentifier(2), 0), items: vec!['g', 'h', 'i'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(2)),
                    new: ChunkIdentifier(3),
                    next: None,
                },
                PushItems { at: Position(ChunkIdentifier(3), 0), items: vec!['j'] },
            ]
        );

        assert_eq!(linked_chunk.len(), 10);
    }

    #[test]
    fn test_push_gap() {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();
        linked_chunk.push_items_back(['a']);
        assert_items_eq!(linked_chunk, ['a']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a'] }]
        );

        linked_chunk.push_gap_back(());
        assert_items_eq!(linked_chunk, ['a'] [-]);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[NewGapChunk {
                previous: Some(ChunkIdentifier(0)),
                new: ChunkIdentifier(1),
                next: None,
                gap: (),
            }]
        );

        linked_chunk.push_items_back(['b', 'c', 'd', 'e']);
        assert_items_eq!(linked_chunk, ['a'] [-] ['b', 'c', 'd'] ['e']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(1)),
                    new: ChunkIdentifier(2),
                    next: None,
                },
                PushItems { at: Position(ChunkIdentifier(2), 0), items: vec!['b', 'c', 'd'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(2)),
                    new: ChunkIdentifier(3),
                    next: None,
                },
                PushItems { at: Position(ChunkIdentifier(3), 0), items: vec!['e'] },
            ]
        );

        linked_chunk.push_gap_back(());
        linked_chunk.push_gap_back(()); // why not
        assert_items_eq!(linked_chunk, ['a'] [-] ['b', 'c', 'd'] ['e'] [-] [-]);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                NewGapChunk {
                    previous: Some(ChunkIdentifier(3)),
                    new: ChunkIdentifier(4),
                    next: None,
                    gap: (),
                },
                NewGapChunk {
                    previous: Some(ChunkIdentifier(4)),
                    new: ChunkIdentifier(5),
                    next: None,
                    gap: (),
                }
            ]
        );

        linked_chunk.push_items_back(['f', 'g', 'h', 'i']);
        assert_items_eq!(linked_chunk, ['a'] [-] ['b', 'c', 'd'] ['e'] [-] [-] ['f', 'g', 'h'] ['i']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(5)),
                    new: ChunkIdentifier(6),
                    next: None,
                },
                PushItems { at: Position(ChunkIdentifier(6), 0), items: vec!['f', 'g', 'h'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(6)),
                    new: ChunkIdentifier(7),
                    next: None,
                },
                PushItems { at: Position(ChunkIdentifier(7), 0), items: vec!['i'] },
            ]
        );

        assert_eq!(linked_chunk.len(), 9);
    }

    #[test]
    fn test_identifiers_and_positions() {
        let mut linked_chunk = LinkedChunk::<3, char, ()>::new();
        linked_chunk.push_items_back(['a', 'b', 'c', 'd', 'e', 'f']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['g', 'h', 'i', 'j']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e', 'f'] [-] ['g', 'h', 'i'] ['j']);

        assert_eq!(linked_chunk.chunk_identifier(Chunk::is_gap), Some(ChunkIdentifier(2)));
        assert_eq!(
            linked_chunk.item_position(|item| *item == 'e'),
            Some(Position(ChunkIdentifier(1), 1))
        );
    }

    #[test]
    fn test_rchunks() {
        let mut linked_chunk = LinkedChunk::<2, char, ()>::new();
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
    fn test_chunks() {
        let mut linked_chunk = LinkedChunk::<2, char, ()>::new();
        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['c', 'd', 'e']);

        let mut iterator = linked_chunk.chunks();

        assert_matches!(
            iterator.next(),
            Some(Chunk { identifier: ChunkIdentifier(0), content: ChunkContent::Items(items), .. }) => {
                assert_eq!(items, &['a', 'b']);
            }
        );
        assert_matches!(
            iterator.next(),
            Some(Chunk { identifier: ChunkIdentifier(1), content: ChunkContent::Gap(..), .. })
        );
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
    }

    #[test]
    fn test_rchunks_from() -> Result<(), Error> {
        let mut linked_chunk = LinkedChunk::<2, char, ()>::new();
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
    fn test_chunks_from() -> Result<(), Error> {
        let mut linked_chunk = LinkedChunk::<2, char, ()>::new();
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
        let mut linked_chunk = LinkedChunk::<2, char, ()>::new();
        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['c', 'd', 'e']);

        let mut iterator = linked_chunk.ritems();

        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(3), 0), 'e')));
        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(2), 1), 'd')));
        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(2), 0), 'c')));
        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(0), 1), 'b')));
        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(0), 0), 'a')));
        assert_matches!(iterator.next(), None);
    }

    #[test]
    fn test_items() {
        let mut linked_chunk = LinkedChunk::<2, char, ()>::new();
        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['c', 'd', 'e']);

        let mut iterator = linked_chunk.items();

        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(0), 0), 'a')));
        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(0), 1), 'b')));
        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(2), 0), 'c')));
        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(2), 1), 'd')));
        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(3), 0), 'e')));
        assert_matches!(iterator.next(), None);
    }

    #[test]
    fn test_ritems_from() -> Result<(), Error> {
        let mut linked_chunk = LinkedChunk::<2, char, ()>::new();
        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['c', 'd', 'e']);

        let mut iterator =
            linked_chunk.ritems_from(linked_chunk.item_position(|item| *item == 'c').unwrap())?;

        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(2), 0), 'c')));
        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(0), 1), 'b')));
        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(0), 0), 'a')));
        assert_matches!(iterator.next(), None);

        Ok(())
    }

    #[test]
    fn test_items_from() -> Result<(), Error> {
        let mut linked_chunk = LinkedChunk::<2, char, ()>::new();
        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['c', 'd', 'e']);

        let mut iterator =
            linked_chunk.items_from(linked_chunk.item_position(|item| *item == 'c').unwrap())?;

        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(2), 0), 'c')));
        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(2), 1), 'd')));
        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(3), 0), 'e')));
        assert_matches!(iterator.next(), None);

        Ok(())
    }

    #[test]
    fn test_insert_items_at() -> Result<(), Error> {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();
        linked_chunk.push_items_back(['a', 'b', 'c', 'd', 'e', 'f']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e', 'f']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a', 'b', 'c'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(0)),
                    new: ChunkIdentifier(1),
                    next: None,
                },
                PushItems { at: Position(ChunkIdentifier(1), 0), items: vec!['d', 'e', 'f'] },
            ]
        );

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
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[
                    DetachLastItems { at: Position(ChunkIdentifier(1), 1) },
                    PushItems { at: Position(ChunkIdentifier(1), 1), items: vec!['w', 'x'] },
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(1)),
                        new: ChunkIdentifier(2),
                        next: None,
                    },
                    PushItems { at: Position(ChunkIdentifier(2), 0), items: vec!['y', 'z'] },
                    StartReattachItems,
                    PushItems { at: Position(ChunkIdentifier(2), 2), items: vec!['e'] },
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(2)),
                        new: ChunkIdentifier(3),
                        next: None,
                    },
                    PushItems { at: Position(ChunkIdentifier(3), 0), items: vec!['f'] },
                    EndReattachItems,
                ]
            );
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
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[
                    DetachLastItems { at: Position(ChunkIdentifier(0), 0) },
                    PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['l', 'm', 'n'] },
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(0)),
                        new: ChunkIdentifier(4),
                        next: Some(ChunkIdentifier(1)),
                    },
                    PushItems { at: Position(ChunkIdentifier(4), 0), items: vec!['o'] },
                    StartReattachItems,
                    PushItems { at: Position(ChunkIdentifier(4), 1), items: vec!['a', 'b'] },
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(4)),
                        new: ChunkIdentifier(5),
                        next: Some(ChunkIdentifier(1)),
                    },
                    PushItems { at: Position(ChunkIdentifier(5), 0), items: vec!['c'] },
                    EndReattachItems,
                ]
            );
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
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[
                    DetachLastItems { at: Position(ChunkIdentifier(5), 0) },
                    PushItems { at: Position(ChunkIdentifier(5), 0), items: vec!['r', 's'] },
                    StartReattachItems,
                    PushItems { at: Position(ChunkIdentifier(5), 2), items: vec!['c'] },
                    EndReattachItems,
                ]
            );
        }

        // Insert at the end of a chunk.
        {
            let position_of_f = linked_chunk.item_position(|item| *item == 'f').unwrap();
            let position_after_f =
                Position(position_of_f.chunk_identifier(), position_of_f.index() + 1);

            linked_chunk.insert_items_at(['p', 'q'], position_after_f)?;
            assert_items_eq!(
                linked_chunk,
                ['l', 'm', 'n'] ['o', 'a', 'b'] ['r', 's', 'c'] ['d', 'w', 'x'] ['y', 'z', 'e'] ['f', 'p', 'q']
            );
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[PushItems { at: Position(ChunkIdentifier(3), 1), items: vec!['p', 'q'] }]
            );
            assert_eq!(linked_chunk.len(), 18);
        }

        // Insert in a chunk that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(['u', 'v'], Position(ChunkIdentifier(128), 0)),
                Err(Error::InvalidChunkIdentifier { identifier: ChunkIdentifier(128) })
            );
            assert!(linked_chunk.updates().unwrap().take().is_empty());
        }

        // Insert in a chunk that exists, but at an item that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(['u', 'v'], Position(ChunkIdentifier(0), 128)),
                Err(Error::InvalidItemIndex { index: 128 })
            );
            assert!(linked_chunk.updates().unwrap().take().is_empty());
        }

        // Insert in a gap.
        {
            // Add a gap to test the error.
            linked_chunk.push_gap_back(());
            assert_items_eq!(
                linked_chunk,
                ['l', 'm', 'n'] ['o', 'a', 'b'] ['r', 's', 'c'] ['d', 'w', 'x'] ['y', 'z', 'e'] ['f', 'p', 'q'] [-]
            );
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[NewGapChunk {
                    previous: Some(ChunkIdentifier(3)),
                    new: ChunkIdentifier(6),
                    next: None,
                    gap: ()
                }]
            );

            assert_matches!(
                linked_chunk.insert_items_at(['u', 'v'], Position(ChunkIdentifier(6), 0)),
                Err(Error::ChunkIsAGap { identifier: ChunkIdentifier(6) })
            );
        }

        assert_eq!(linked_chunk.len(), 18);

        Ok(())
    }

    #[test]
    fn test_insert_gap_at() -> Result<(), Error> {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();
        linked_chunk.push_items_back(['a', 'b', 'c', 'd', 'e', 'f']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e', 'f']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a', 'b', 'c'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(0)),
                    new: ChunkIdentifier(1),
                    next: None
                },
                PushItems { at: Position(ChunkIdentifier(1), 0), items: vec!['d', 'e', 'f'] },
            ]
        );

        // Insert in the middle of a chunk.
        {
            let position_of_b = linked_chunk.item_position(|item| *item == 'b').unwrap();
            linked_chunk.insert_gap_at((), position_of_b)?;

            assert_items_eq!(linked_chunk, ['a'] [-] ['b', 'c'] ['d', 'e', 'f']);
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[
                    DetachLastItems { at: Position(ChunkIdentifier(0), 1) },
                    NewGapChunk {
                        previous: Some(ChunkIdentifier(0)),
                        new: ChunkIdentifier(2),
                        next: Some(ChunkIdentifier(1)),
                        gap: (),
                    },
                    StartReattachItems,
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(2)),
                        new: ChunkIdentifier(3),
                        next: Some(ChunkIdentifier(1)),
                    },
                    PushItems { at: Position(ChunkIdentifier(3), 0), items: vec!['b', 'c'] },
                    EndReattachItems,
                ]
            );
        }

        // Insert at the beginning of a chunk + it's the first chunk.
        {
            let position_of_a = linked_chunk.item_position(|item| *item == 'a').unwrap();
            linked_chunk.insert_gap_at((), position_of_a)?;

            // A new empty chunk is created as the first chunk.
            assert_items_eq!(linked_chunk, [] [-] ['a'] [-] ['b', 'c'] ['d', 'e', 'f']);
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[
                    DetachLastItems { at: Position(ChunkIdentifier(0), 0) },
                    NewGapChunk {
                        previous: Some(ChunkIdentifier(0)),
                        new: ChunkIdentifier(4),
                        next: Some(ChunkIdentifier(2)),
                        gap: (),
                    },
                    StartReattachItems,
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(4)),
                        new: ChunkIdentifier(5),
                        next: Some(ChunkIdentifier(2)),
                    },
                    PushItems { at: Position(ChunkIdentifier(5), 0), items: vec!['a'] },
                    EndReattachItems,
                ]
            );
        }

        // Insert at the beginning of a chunk.
        {
            let position_of_d = linked_chunk.item_position(|item| *item == 'd').unwrap();
            linked_chunk.insert_gap_at((), position_of_d)?;

            // A new empty chunk is NOT created, i.e. `['d', 'e', 'f']` is not
            // split into `[]` + `['d', 'e', 'f']` because it's a waste of
            // space.
            assert_items_eq!(linked_chunk, [] [-] ['a'] [-] ['b', 'c'] [-] ['d', 'e', 'f']);
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[NewGapChunk {
                    previous: Some(ChunkIdentifier(3)),
                    new: ChunkIdentifier(6),
                    next: Some(ChunkIdentifier(1)),
                    gap: (),
                }]
            );
        }

        // Insert in an empty chunk + it's the first chunk.
        {
            let position_of_first_empty_chunk = Position(ChunkIdentifier(0), 0);
            assert_matches!(
                linked_chunk.insert_gap_at((), position_of_first_empty_chunk),
                Err(Error::InvalidItemIndex { index: 0 })
            );
            assert!(linked_chunk.updates().unwrap().take().is_empty());
        }

        // Insert in an empty chunk.
        {
            // Replace a gap by empty items.
            let gap_identifier = linked_chunk.chunk_identifier(Chunk::is_gap).unwrap();
            let position = linked_chunk.replace_gap_at([], gap_identifier)?.first_position();

            assert_items_eq!(linked_chunk, [] [-] ['a'] [-] ['b', 'c'] [] ['d', 'e', 'f']);
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(6)),
                        new: ChunkIdentifier(7),
                        next: Some(ChunkIdentifier(1)),
                    },
                    RemoveChunk(ChunkIdentifier(6)),
                ]
            );

            linked_chunk.insert_gap_at((), position)?;

            assert_items_eq!(linked_chunk, [] [-] ['a'] [-] ['b', 'c'] [-] [] ['d', 'e', 'f']);
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[NewGapChunk {
                    previous: Some(ChunkIdentifier(3)),
                    new: ChunkIdentifier(8),
                    next: Some(ChunkIdentifier(7)),
                    gap: (),
                }]
            );
        }

        // Insert in a chunk that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(['u', 'v'], Position(ChunkIdentifier(128), 0)),
                Err(Error::InvalidChunkIdentifier { identifier: ChunkIdentifier(128) })
            );
            assert!(linked_chunk.updates().unwrap().take().is_empty());
        }

        // Insert in a chunk that exists, but at an item that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(['u', 'v'], Position(ChunkIdentifier(0), 128)),
                Err(Error::InvalidItemIndex { index: 128 })
            );
            assert!(linked_chunk.updates().unwrap().take().is_empty());
        }

        // Insert in an existing gap.
        {
            // It is impossible to get the item position inside a gap. It's only possible if
            // the item position is crafted by hand or is outdated.
            let position_of_a_gap = Position(ChunkIdentifier(4), 0);
            assert_matches!(
                linked_chunk.insert_gap_at((), position_of_a_gap),
                Err(Error::ChunkIsAGap { identifier: ChunkIdentifier(4) })
            );
            assert!(linked_chunk.updates().unwrap().take().is_empty());
        }

        assert_eq!(linked_chunk.len(), 6);

        Ok(())
    }

    #[test]
    fn test_replace_gap_at() -> Result<(), Error> {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();
        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['l', 'm']);
        assert_items_eq!(linked_chunk, ['a', 'b'] [-] ['l', 'm']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a', 'b'] },
                NewGapChunk {
                    previous: Some(ChunkIdentifier(0)),
                    new: ChunkIdentifier(1),
                    next: None,
                    gap: (),
                },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(1)),
                    new: ChunkIdentifier(2),
                    next: None,
                },
                PushItems { at: Position(ChunkIdentifier(2), 0), items: vec!['l', 'm'] }
            ]
        );

        // Replace a gap in the middle of the linked chunk.
        {
            let gap_identifier = linked_chunk.chunk_identifier(Chunk::is_gap).unwrap();
            assert_eq!(gap_identifier, ChunkIdentifier(1));

            let new_chunk =
                linked_chunk.replace_gap_at(['d', 'e', 'f', 'g', 'h'], gap_identifier)?;
            assert_eq!(new_chunk.identifier(), ChunkIdentifier(3));
            assert_items_eq!(
                linked_chunk,
                ['a', 'b'] ['d', 'e', 'f'] ['g', 'h'] ['l', 'm']
            );
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(1)),
                        new: ChunkIdentifier(3),
                        next: Some(ChunkIdentifier(2)),
                    },
                    PushItems { at: Position(ChunkIdentifier(3), 0), items: vec!['d', 'e', 'f'] },
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(3)),
                        new: ChunkIdentifier(4),
                        next: Some(ChunkIdentifier(2)),
                    },
                    PushItems { at: Position(ChunkIdentifier(4), 0), items: vec!['g', 'h'] },
                    RemoveChunk(ChunkIdentifier(1)),
                ]
            );
        }

        // Replace a gap at the end of the linked chunk.
        {
            linked_chunk.push_gap_back(());
            assert_items_eq!(
                linked_chunk,
                ['a', 'b'] ['d', 'e', 'f'] ['g', 'h'] ['l', 'm'] [-]
            );
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[NewGapChunk {
                    previous: Some(ChunkIdentifier(2)),
                    new: ChunkIdentifier(5),
                    next: None,
                    gap: (),
                }]
            );

            let gap_identifier = linked_chunk.chunk_identifier(Chunk::is_gap).unwrap();
            assert_eq!(gap_identifier, ChunkIdentifier(5));

            let new_chunk = linked_chunk.replace_gap_at(['w', 'x', 'y', 'z'], gap_identifier)?;
            assert_eq!(new_chunk.identifier(), ChunkIdentifier(6));
            assert_items_eq!(
                linked_chunk,
                ['a', 'b'] ['d', 'e', 'f'] ['g', 'h'] ['l', 'm'] ['w', 'x', 'y'] ['z']
            );
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(5)),
                        new: ChunkIdentifier(6),
                        next: None,
                    },
                    PushItems { at: Position(ChunkIdentifier(6), 0), items: vec!['w', 'x', 'y'] },
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(6)),
                        new: ChunkIdentifier(7),
                        next: None,
                    },
                    PushItems { at: Position(ChunkIdentifier(7), 0), items: vec!['z'] },
                    RemoveChunk(ChunkIdentifier(5)),
                ]
            );
        }

        assert_eq!(linked_chunk.len(), 13);

        Ok(())
    }

    #[test]
    fn test_chunk_item_positions() {
        let mut linked_chunk = LinkedChunk::<3, char, ()>::new();
        linked_chunk.push_items_back(['a', 'b', 'c', 'd', 'e']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['f']);

        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e'] [-] ['f']);

        let mut iterator = linked_chunk.chunks();

        // First chunk.
        {
            let chunk = iterator.next().unwrap();
            assert_eq!(chunk.first_position(), Position(ChunkIdentifier(0), 0));
            assert_eq!(chunk.last_position(), Position(ChunkIdentifier(0), 2));
        }

        // Second chunk.
        {
            let chunk = iterator.next().unwrap();
            assert_eq!(chunk.first_position(), Position(ChunkIdentifier(1), 0));
            assert_eq!(chunk.last_position(), Position(ChunkIdentifier(1), 1));
        }

        // Gap.
        {
            let chunk = iterator.next().unwrap();
            assert_eq!(chunk.first_position(), Position(ChunkIdentifier(2), 0));
            assert_eq!(chunk.last_position(), Position(ChunkIdentifier(2), 0));
        }

        // Last chunk.
        {
            let chunk = iterator.next().unwrap();
            assert_eq!(chunk.first_position(), Position(ChunkIdentifier(3), 0));
            assert_eq!(chunk.last_position(), Position(ChunkIdentifier(3), 0));
        }
    }
}
