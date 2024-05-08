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
    collections::HashMap,
    fmt,
    marker::PhantomData,
    ops::Not,
    pin::Pin,
    ptr::NonNull,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock, Weak,
    },
    task::{Context, Poll, Waker},
};

use futures_core::Stream;

/// Errors of [`LinkedChunk`].
#[derive(thiserror::Error, Debug)]
pub enum LinkedChunkError {
    #[error("The chunk identifier is invalid: `{identifier:?}`")]
    InvalidChunkIdentifier { identifier: ChunkIdentifier },

    #[error("The chunk is a gap: `{identifier:?}`")]
    ChunkIsAGap { identifier: ChunkIdentifier },

    #[error("The chunk is an item: `{identifier:?}`")]
    ChunkIsItems { identifier: ChunkIdentifier },

    #[error("The item index is invalid: `{index}`")]
    InvalidItemIndex { index: usize },
}

/// Represent the updates that have happened inside a [`LinkedChunk`].
///
/// To retrieve the updates, use [`LinkedChunk::updates`].
///
/// These updates are useful to store a `LinkedChunk` in another form of
/// storage, like a database or something similar.
#[derive(Debug, PartialEq)]
pub enum LinkedChunkUpdate<Item, Gap> {
    /// A new chunk of kind Items has been created.
    NewItemsChunk {
        /// The identifier of the previous chunk of this new chunk.
        previous: Option<ChunkIdentifier>,

        /// The identifier of the new chunk.
        new: ChunkIdentifier,

        /// The identifier of the next chunk of this new chunk.
        next: Option<ChunkIdentifier>,
    },

    /// A new chunk of kind Gap has been created.
    NewGapChunk {
        /// The identifier of the previous chunk of this new chunk.
        previous: Option<ChunkIdentifier>,

        /// The identifier of the new chunk.
        new: ChunkIdentifier,

        /// The identifier of the next chunk of this new chunk.
        next: Option<ChunkIdentifier>,

        /// The content of the chunk.
        gap: Gap,
    },

    /// A chunk has been removed.
    RemoveChunk(ChunkIdentifier),

    /// Items are inserted inside a chunk of kind Items.
    InsertItems {
        /// [`Position`] of the items.
        at: Position,

        /// The items.
        items: Vec<Item>,
    },

    /// A chunk of kind Items has been truncated.
    TruncateItems {
        /// The identifier of the chunk.
        chunk: ChunkIdentifier,

        /// The new length of the chunk.
        length: usize,
    },
}

impl<Item, Gap> Clone for LinkedChunkUpdate<Item, Gap>
where
    Item: Clone,
    Gap: Clone,
{
    fn clone(&self) -> Self {
        match self {
            Self::NewItemsChunk { previous, new, next } => Self::NewItemsChunk {
                previous: previous.clone(),
                new: new.clone(),
                next: next.clone(),
            },
            Self::NewGapChunk { previous, new, next, gap } => Self::NewGapChunk {
                previous: previous.clone(),
                new: new.clone(),
                next: next.clone(),
                gap: gap.clone(),
            },
            Self::RemoveChunk(identifier) => Self::RemoveChunk(identifier.clone()),
            Self::InsertItems { at, items } => {
                Self::InsertItems { at: at.clone(), items: items.clone() }
            }
            Self::TruncateItems { chunk, length } => {
                Self::TruncateItems { chunk: chunk.clone(), length: length.clone() }
            }
        }
    }
}

/// A collection of [`LinkedChunkUpdate`].
///
/// Get a value for this type with [`LinkedChunk::updates`].
pub struct LinkedChunkUpdates<Item, Gap> {
    inner: Arc<RwLock<LinkedChunkUpdatesInner<Item, Gap>>>,
}

/// A token used to represent readers that read the updates in
/// [`LinkedChunkUpdatesInner`].
type ReaderToken = usize;

/// Inner type for [`LinkedChunkUpdates`].
///
/// The particularity of this type is that multiple readers can read the
/// updates. A reader has a [`ReaderToken`]. The public API (i.e.
/// [`LinkedChunkUpdates`]) is considered to be the _main reader_ (it has the
/// token [`Self::MAIN_READER_TOKEN`]).
///
/// An update that have been read by all readers are garbage collected to be
/// removed from the memory. An update will never be read twice by the same
/// reader.
///
/// Why do we need multiple readers? The public API reads the updates with
/// [`LinkedChunkUpdates::take`], but the private API must also read the updates
/// for example with [`LinkedChunkUpdatesSubscriber`]. Of course, they can be
/// multiple `LinkedChunkUpdatesSubscriber`s at the same time. Hence the need of
/// supporting multiple readers.
struct LinkedChunkUpdatesInner<Item, Gap> {
    /// All the updates that have not been read by all readers.
    updates: Vec<LinkedChunkUpdate<Item, Gap>>,

    /// Updates are stored in [`Self::updates`]. Multiple readers can read them.
    /// A reader is identified by a [`ReaderToken`].
    ///
    /// To each reader token is associated an index that represents the index of
    /// the last reading. It is used to never return the same update twice.
    last_index_per_reader: HashMap<ReaderToken, usize>,

    /// The last generated token. This is useful to generate new token.
    last_token: ReaderToken,

    /// Pending wakers for [`LinkedChunkUpdateSubscriber`]s. A waker is removed
    /// everytime it is called.
    wakers: Vec<Waker>,
}

impl<Item, Gap> LinkedChunkUpdatesInner<Item, Gap> {
    /// The token used by the main reader. See [`Self::take`] to learn more.
    const MAIN_READER_TOKEN: ReaderToken = 0;

    /// Create a new [`Self`].
    fn new() -> Self {
        Self {
            updates: Vec::with_capacity(8),
            last_index_per_reader: {
                let mut map = HashMap::with_capacity(2);
                map.insert(Self::MAIN_READER_TOKEN, 0);

                map
            },
            last_token: Self::MAIN_READER_TOKEN,
            wakers: Vec::with_capacity(2),
        }
    }

    /// Push a new update.
    fn push(&mut self, update: LinkedChunkUpdate<Item, Gap>) {
        self.updates.push(update);

        // Wake them up \o/.
        for waker in self.wakers.drain(..) {
            waker.wake();
        }
    }

    /// Take new updates; it considers the caller is the main reader, i.e. it
    /// will use the [`Self::MAIN_READER_TOKEN`].
    ///
    /// Updates that have been read will never be read again by the current
    /// reader.
    ///
    /// Learn more by reading [`Self::take_with_token`].
    fn take(&mut self) -> &[LinkedChunkUpdate<Item, Gap>] {
        self.take_with_token(Self::MAIN_READER_TOKEN)
    }

    /// Take new updates with a particular reader token.
    ///
    /// Updates are stored in [`Self::updates`]. Multiple readers can read them.
    /// A reader is identified by a [`ReaderToken`]. Every reader can
    /// take/read/consume each update only once. An internal index is stored
    /// per reader token to know where to start reading updates next time this
    /// method is called.
    fn take_with_token(&mut self, token: ReaderToken) -> &[LinkedChunkUpdate<Item, Gap>] {
        // Let's garbage collect unused updates.
        self.garbage_collect();

        let index = self
            .last_index_per_reader
            .get_mut(&token)
            .expect("Given `UpdatesToken` does not map to any index");

        // Read new updates, and update the index.
        let slice = &self.updates[*index..];
        *index = self.updates.len();

        slice
    }

    /// Return the number of updates in the buffer.
    fn len(&self) -> usize {
        self.updates.len()
    }

    /// Garbage collect unused updates. An update is considered unused when it's
    /// been read by all readers.
    ///
    /// Basically, it reduces to finding the smallest last index for all
    /// readers, and clear from 0 to that index.
    fn garbage_collect(&mut self) {
        let min_index = self.last_index_per_reader.values().min().map(|min| *min).unwrap_or(0);

        if min_index > 0 {
            let _ = self.updates.drain(0..min_index);

            // Let's shift the indices to the left by `min_index` to preserve them.
            for index in self.last_index_per_reader.values_mut() {
                *index -= min_index;
            }
        }
    }
}

impl<Item, Gap> LinkedChunkUpdates<Item, Gap> {
    /// Create a new [`Self`].
    fn new() -> Self {
        Self { inner: Arc::new(RwLock::new(LinkedChunkUpdatesInner::new())) }
    }

    /// Push a new update.
    fn push(&mut self, update: LinkedChunkUpdate<Item, Gap>) {
        self.inner.write().unwrap().push(update);
    }

    /// Take new updates.
    ///
    /// Updates that have been taken will not be read again.
    pub fn take(&mut self) -> Vec<LinkedChunkUpdate<Item, Gap>>
    where
        Item: Clone,
        Gap: Clone,
    {
        self.inner.write().unwrap().take().to_owned()
    }

    /// Subscribe to updates by using a [`Stream`].
    fn subscribe(&mut self) -> LinkedChunkUpdatesSubscriber<Item, Gap> {
        // A subscriber is a new update reader, it needs its own token.
        let token = {
            let mut inner = self.inner.write().unwrap();
            inner.last_token += 1;

            let last_token = inner.last_token;
            inner.last_index_per_reader.insert(last_token, 0);

            last_token
        };

        LinkedChunkUpdatesSubscriber { updates: Arc::downgrade(&self.inner), token }
    }
}

/// A subscriber to [`LinkedChunkUpdates`]. It is helpful to receive updates via
/// a [`Stream`].
struct LinkedChunkUpdatesSubscriber<Item, Gap> {
    /// Weak reference to [`LinkedChunkUpdatesInner`].
    ///
    /// Using a weak reference allows [`LinkedChunkUpdates`] to be dropped
    /// freely even if a subscriber exists.
    updates: Weak<RwLock<LinkedChunkUpdatesInner<Item, Gap>>>,

    /// The token to read the updates.
    token: ReaderToken,
}

impl<Item, Gap> Stream for LinkedChunkUpdatesSubscriber<Item, Gap>
where
    Item: Clone,
    Gap: Clone,
{
    type Item = Vec<LinkedChunkUpdate<Item, Gap>>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(updates) = self.updates.upgrade() else {
            // The `LinkedChunkUpdates` has been dropped. It's time to close this stream.
            return Poll::Ready(None);
        };

        let mut updates = updates.write().unwrap();
        let the_updates = updates.take_with_token(self.token);

        // No updates.
        if the_updates.is_empty() {
            // Let's register the waker.
            updates.wakers.push(context.waker().clone());

            // The stream is pending.
            return Poll::Pending;
        }

        // There is updates! Let's forward them in this stream.
        Poll::Ready(Some(the_updates.to_owned()))
    }
}

/// Links of a `LinkedChunk`, i.e. the first and last [`Chunk`].
///
/// This type was introduced to avoid borrow checking errors when mutably
/// referencing a subset of fields of a `LinkedChunk`.
struct LinkedChunkEnds<const CHUNK_CAPACITY: usize, Item, Gap> {
    /// The first chunk.
    first: NonNull<Chunk<CHUNK_CAPACITY, Item, Gap>>,
    /// The last chunk.
    last: Option<NonNull<Chunk<CHUNK_CAPACITY, Item, Gap>>>,
}

impl<const CAP: usize, Item, Gap> LinkedChunkEnds<CAP, Item, Gap> {
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
    links: LinkedChunkEnds<CHUNK_CAPACITY, Item, Gap>,
    /// The number of items hold by this linked chunk.
    length: usize,
    /// The generator of chunk identifiers.
    chunk_identifier_generator: ChunkIdentifierGenerator,
    /// All updates that have been made on this `LinkedChunk`. If this field is
    /// `Some(…)`, update history is enabled, otherwise, if it's `None`, update
    /// history is disabled.
    updates: Option<LinkedChunkUpdates<Item, Gap>>,
    /// Marker.
    marker: PhantomData<Box<Chunk<CHUNK_CAPACITY, Item, Gap>>>,
}

impl<const CAP: usize, Item, Gap> LinkedChunk<CAP, Item, Gap> {
    /// Create a new [`Self`].
    pub fn new() -> Self {
        Self {
            links: LinkedChunkEnds {
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
    /// [`LinkedChunkUpdates::take`] method must be called to consume and clean
    /// the updates. See [`Self::updates`].
    pub fn new_with_update_history() -> Self {
        Self {
            links: LinkedChunkEnds {
                // INVARIANT: The first chunk must always be an Items, not a Gap.
                first: Chunk::new_items_leaked(ChunkIdentifierGenerator::FIRST_IDENTIFIER),
                last: None,
            },
            length: 0,
            chunk_identifier_generator: ChunkIdentifierGenerator::new_from_scratch(),
            updates: Some(LinkedChunkUpdates::new()),
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
    pub fn insert_items_at<I>(
        &mut self,
        items: I,
        position: Position,
    ) -> Result<(), LinkedChunkError>
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
            .ok_or(LinkedChunkError::InvalidChunkIdentifier { identifier: chunk_identifier })?;

        let (chunk, number_of_items) = match &mut chunk.content {
            ChunkContent::Gap(..) => {
                return Err(LinkedChunkError::ChunkIsAGap { identifier: chunk_identifier })
            }

            ChunkContent::Items(current_items) => {
                let current_items_length = current_items.len();

                if item_index > current_items_length {
                    return Err(LinkedChunkError::InvalidItemIndex { index: item_index });
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
                            updates.push(LinkedChunkUpdate::TruncateItems {
                                chunk: chunk_identifier,
                                length: item_index,
                            });
                        }

                        // Split the items.
                        let detached_items = current_items.split_off(item_index);

                        chunk
                            // Push the new items.
                            .push_items(items, &self.chunk_identifier_generator, &mut self.updates)
                            // Finally, push the items that have been detached.
                            .push_items(
                                detached_items.into_iter(),
                                &self.chunk_identifier_generator,
                                &mut self.updates,
                            )
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
    pub fn insert_gap_at(
        &mut self,
        content: Gap,
        position: Position,
    ) -> Result<(), LinkedChunkError>
    where
        Item: Clone,
        Gap: Clone,
    {
        let chunk_identifier = position.chunk_identifier();
        let item_index = position.index();

        let chunk = self
            .links
            .chunk_mut(chunk_identifier)
            .ok_or(LinkedChunkError::InvalidChunkIdentifier { identifier: chunk_identifier })?;

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
                return Err(LinkedChunkError::ChunkIsAGap { identifier: chunk_identifier });
            }

            ChunkContent::Items(current_items) => {
                let current_items_length = current_items.len();

                if item_index >= current_items_length {
                    return Err(LinkedChunkError::InvalidItemIndex { index: item_index });
                }

                if let Some(updates) = self.updates.as_mut() {
                    updates.push(LinkedChunkUpdate::TruncateItems {
                        chunk: chunk_identifier,
                        length: item_index,
                    });
                }

                // Split the items.
                let detached_items = current_items.split_off(item_index);

                chunk
                    // Insert a new gap chunk.
                    .insert_next(
                        Chunk::new_gap_leaked(self.chunk_identifier_generator.next(), content),
                        &mut self.updates,
                    )
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
    ) -> Result<&Chunk<CAP, Item, Gap>, LinkedChunkError>
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
                .ok_or(LinkedChunkError::InvalidChunkIdentifier { identifier: chunk_identifier })?;

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
                    return Err(LinkedChunkError::ChunkIsItems { identifier: chunk_identifier })
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
    pub fn rchunks(&self) -> LinkedChunkIterBackward<'_, CAP, Item, Gap> {
        LinkedChunkIterBackward::new(self.links.latest_chunk())
    }

    /// Iterate over the chunks, forward.
    ///
    /// It iterates from the first to the last chunk.
    pub fn chunks(&self) -> LinkedChunkIter<'_, CAP, Item, Gap> {
        LinkedChunkIter::new(self.links.first_chunk())
    }

    /// Iterate over the chunks, starting from `identifier`, backward.
    ///
    /// It iterates from the chunk with the identifier `identifier` to the first
    /// chunk.
    pub fn rchunks_from(
        &self,
        identifier: ChunkIdentifier,
    ) -> Result<LinkedChunkIterBackward<'_, CAP, Item, Gap>, LinkedChunkError> {
        Ok(LinkedChunkIterBackward::new(
            self.links
                .chunk(identifier)
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
    ) -> Result<LinkedChunkIter<'_, CAP, Item, Gap>, LinkedChunkError> {
        Ok(LinkedChunkIter::new(
            self.links
                .chunk(identifier)
                .ok_or(LinkedChunkError::InvalidChunkIdentifier { identifier })?,
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
    ) -> Result<impl Iterator<Item = (Position, &Item)>, LinkedChunkError> {
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
    ) -> Result<impl Iterator<Item = (Position, &Item)>, LinkedChunkError> {
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
    /// [`LinkedChunkUpdates`].
    ///
    /// The caller is responsible to clear these updates.
    ///
    /// If the `Option` becomes `None`, it will disable update history. Thus, be
    /// careful when you want to empty the update history: do not use
    /// `Option::take()` directly but rather [`LinkedChunkUpdates::take`] for
    /// example.
    pub fn updates(&mut self) -> Option<&mut LinkedChunkUpdates<Item, Gap>> {
        self.updates.as_mut()
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
pub struct LinkedChunkIterBackward<'a, const CAP: usize, Item, Gap> {
    chunk: Option<&'a Chunk<CAP, Item, Gap>>,
}

impl<'a, const CAP: usize, Item, Gap> LinkedChunkIterBackward<'a, CAP, Item, Gap> {
    /// Create a new [`LinkedChunkIter`] from a particular [`Chunk`].
    fn new(from_chunk: &'a Chunk<CAP, Item, Gap>) -> Self {
        Self { chunk: Some(from_chunk) }
    }
}

impl<'a, const CAP: usize, Item, Gap> Iterator for LinkedChunkIterBackward<'a, CAP, Item, Gap> {
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
pub struct LinkedChunkIter<'a, const CAP: usize, Item, Gap> {
    chunk: Option<&'a Chunk<CAP, Item, Gap>>,
}

impl<'a, const CAP: usize, Item, Gap> LinkedChunkIter<'a, CAP, Item, Gap> {
    /// Create a new [`LinkedChunkIter`] from a particular [`Chunk`].
    fn new(from_chunk: &'a Chunk<CAP, Item, Gap>) -> Self {
        Self { chunk: Some(from_chunk) }
    }
}

impl<'a, const CAP: usize, Item, Gap> Iterator for LinkedChunkIter<'a, CAP, Item, Gap> {
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
        updates: &mut Option<LinkedChunkUpdates<Item, Gap>>,
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
                        updates.push(LinkedChunkUpdate::InsertItems {
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
                            updates.push(LinkedChunkUpdate::InsertItems {
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
        updates: &mut Option<LinkedChunkUpdates<Item, Gap>>,
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
                ChunkContent::Gap(gap) => updates.push(LinkedChunkUpdate::NewGapChunk {
                    previous,
                    new,
                    next,
                    gap: gap.clone(),
                }),

                ChunkContent::Items(..) => {
                    updates.push(LinkedChunkUpdate::NewItemsChunk { previous, new, next })
                }
            }
        }

        new_chunk
    }

    /// Unlink this chunk.
    ///
    /// Be careful: `self` won't belong to `LinkedChunk` anymore, and should be
    /// dropped appropriately.
    fn unlink(&mut self, updates: &mut Option<LinkedChunkUpdates<Item, Gap>>) {
        let previous_ptr = self.previous;
        let next_ptr = self.next;

        if let Some(previous) = self.previous_mut() {
            previous.next = next_ptr;
        }

        if let Some(next) = self.next_mut() {
            next.previous = previous_ptr;
        }

        if let Some(updates) = updates.as_mut() {
            updates.push(LinkedChunkUpdate::RemoveChunk(self.identifier()));
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
    use std::{
        sync::{Arc, Mutex},
        task::{Context, Poll, Wake},
    };

    use assert_matches::assert_matches;
    use futures_util::pin_mut;

    use super::{
        Chunk, ChunkContent, ChunkIdentifier, ChunkIdentifierGenerator, LinkedChunk,
        LinkedChunkError, Position, Stream,
    };
    use crate::event_cache::linked_chunk::LinkedChunkUpdatesInner;

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

                        let ChunkContent::Items(items) = chunk.content() else { unreachable!() };

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
    fn test_updates_take_and_garbage_collector() {
        use super::LinkedChunkUpdate::*;

        let mut linked_chunk = LinkedChunk::<10, char, ()>::new_with_update_history();

        // Simulate another updates “reader”, it can a subscriber.
        let main_token = LinkedChunkUpdatesInner::<char, ()>::MAIN_READER_TOKEN;
        let other_token = {
            let updates = linked_chunk.updates().unwrap();
            let mut inner = updates.inner.write().unwrap();
            inner.last_token += 1;

            let other_token = inner.last_token;
            inner.last_index_per_reader.insert(other_token, 0);

            other_token
        };

        // There is no new update yet.
        {
            let updates = linked_chunk.updates().unwrap();

            assert!(updates.take().is_empty());
            assert!(updates.inner.write().unwrap().take_with_token(other_token).is_empty());
        }

        linked_chunk.push_items_back(['a']);
        linked_chunk.push_items_back(['b']);
        linked_chunk.push_items_back(['c']);

        // Scenario 1: “main” takes the new updates, “other” doesn't take the new
        // updates.
        //
        // 0   1   2   3
        // +---+---+---+
        // | a | b | c |
        // +---+---+---+
        //
        // “main” will move its index from 0 to 3.
        // “other” won't move its index.
        {
            let updates = linked_chunk.updates().unwrap();

            {
                // Inspect number of updates in memory.
                assert_eq!(updates.inner.read().unwrap().len(), 3);
            }

            assert_eq!(
                updates.take(),
                &[
                    InsertItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a'] },
                    InsertItems { at: Position(ChunkIdentifier(0), 1), items: vec!['b'] },
                    InsertItems { at: Position(ChunkIdentifier(0), 2), items: vec!['c'] },
                ]
            );

            {
                let inner = updates.inner.read().unwrap();

                // Inspect number of updates in memory.
                // It must be the same number as before as the garbage collector weren't not
                // able to remove any unused updates.
                assert_eq!(inner.len(), 3);

                // Inspect the indices.
                let indices = &inner.last_index_per_reader;

                assert_eq!(indices.get(&main_token), Some(&3));
                assert_eq!(indices.get(&other_token), Some(&0));
            }
        }

        linked_chunk.push_items_back(['d']);
        linked_chunk.push_items_back(['e']);
        linked_chunk.push_items_back(['f']);

        // Scenario 2: “other“ takes the new updates, “main” doesn't take the
        // new updates.
        //
        // 0   1   2   3   4   5   6
        // +---+---+---+---+---+---+
        // | a | b | c | d | e | f |
        // +---+---+---+---+---+---+
        //
        // “main” won't move its index.
        // “other” will move its index from 0 to 6.
        {
            let updates = linked_chunk.updates().unwrap();

            assert_eq!(
                updates.inner.write().unwrap().take_with_token(other_token),
                &[
                    InsertItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a'] },
                    InsertItems { at: Position(ChunkIdentifier(0), 1), items: vec!['b'] },
                    InsertItems { at: Position(ChunkIdentifier(0), 2), items: vec!['c'] },
                    InsertItems { at: Position(ChunkIdentifier(0), 3), items: vec!['d'] },
                    InsertItems { at: Position(ChunkIdentifier(0), 4), items: vec!['e'] },
                    InsertItems { at: Position(ChunkIdentifier(0), 5), items: vec!['f'] },
                ]
            );

            {
                let inner = updates.inner.read().unwrap();

                // Inspect number of updates in memory.
                // It must be the same number as before as the garbage collector will be able to
                // remove unused updates but at the next call…
                assert_eq!(inner.len(), 6);

                // Inspect the indices.
                let indices = &inner.last_index_per_reader;

                assert_eq!(indices.get(&main_token), Some(&3));
                assert_eq!(indices.get(&other_token), Some(&6));
            }
        }

        // Scenario 3: “other” take new updates, but there is none, “main”
        // doesn't take new updates. The garbage collector will run and collect
        // unused updates.
        //
        // 0   1   2   3
        // +---+---+---+
        // | d | e | f |
        // +---+---+---+
        //
        // “main” will have its index updated from 3 to 0.
        // “other” will have its index updated from 6 to 3.
        {
            let updates = linked_chunk.updates().unwrap();

            assert!(updates.inner.write().unwrap().take_with_token(other_token).is_empty());

            {
                let inner = updates.inner.read().unwrap();

                // Inspect number of updates in memory.
                // The garbage collector has removed unused updates.
                assert_eq!(inner.len(), 3);

                // Inspect the indices. They must have been adjusted.
                let indices = &inner.last_index_per_reader;

                assert_eq!(indices.get(&main_token), Some(&0));
                assert_eq!(indices.get(&other_token), Some(&3));
            }
        }

        linked_chunk.push_items_back(['g']);
        linked_chunk.push_items_back(['h']);
        linked_chunk.push_items_back(['i']);

        // Scenario 4: both “main” and “other” take the new updates.
        //
        // 0   1   2   3   4   5   6
        // +---+---+---+---+---+---+
        // | d | e | f | g | h | i |
        // +---+---+---+---+---+---+
        //
        // “main” will have its index updated from 3 to 0.
        // “other” will have its index updated from 6 to 3.
        {
            let updates = linked_chunk.updates().unwrap();

            assert_eq!(
                updates.take(),
                &[
                    InsertItems { at: Position(ChunkIdentifier(0), 3), items: vec!['d'] },
                    InsertItems { at: Position(ChunkIdentifier(0), 4), items: vec!['e'] },
                    InsertItems { at: Position(ChunkIdentifier(0), 5), items: vec!['f'] },
                    InsertItems { at: Position(ChunkIdentifier(0), 6), items: vec!['g'] },
                    InsertItems { at: Position(ChunkIdentifier(0), 7), items: vec!['h'] },
                    InsertItems { at: Position(ChunkIdentifier(0), 8), items: vec!['i'] },
                ]
            );
            assert_eq!(
                updates.inner.write().unwrap().take_with_token(other_token),
                &[
                    InsertItems { at: Position(ChunkIdentifier(0), 6), items: vec!['g'] },
                    InsertItems { at: Position(ChunkIdentifier(0), 7), items: vec!['h'] },
                    InsertItems { at: Position(ChunkIdentifier(0), 8), items: vec!['i'] },
                ]
            );

            {
                let inner = updates.inner.read().unwrap();

                // Inspect number of updates in memory.
                // The garbage collector had a chance to collect the first 3 updates.
                assert_eq!(inner.len(), 3);

                // Inspect the indices.
                let indices = &inner.last_index_per_reader;

                assert_eq!(indices.get(&main_token), Some(&3));
                assert_eq!(indices.get(&other_token), Some(&3));
            }
        }

        // Scenario 5: no more updates but they both try to take new updates.
        // The garbage collector will collect all updates as all of them as
        // been read already.
        //
        // “main” will have its index updated from 0 to 0.
        // “other” will have its index updated from 3 to 0.
        {
            let updates = linked_chunk.updates().unwrap();

            assert!(updates.take().is_empty());
            assert!(updates.inner.write().unwrap().take_with_token(other_token).is_empty());

            {
                let inner = updates.inner.read().unwrap();

                // Inspect number of updates in memory.
                // The garbage collector had a chance to collect all updates.
                assert_eq!(inner.len(), 0);

                // Inspect the indices.
                let indices = &inner.last_index_per_reader;

                assert_eq!(indices.get(&main_token), Some(&0));
                assert_eq!(indices.get(&other_token), Some(&0));
            }
        }
    }

    #[test]
    fn test_updates_stream() {
        use super::LinkedChunkUpdate::*;

        struct CounterWaker {
            number_of_wakeup: Mutex<usize>,
        }

        impl Wake for CounterWaker {
            fn wake(self: Arc<Self>) {
                *self.number_of_wakeup.lock().unwrap() += 1;
            }
        }

        let counter_waker = Arc::new(CounterWaker { number_of_wakeup: Mutex::new(0) });
        let waker = counter_waker.clone().into();
        let mut context = Context::from_waker(&waker);

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        let updates_subscriber = linked_chunk.updates().unwrap().subscribe();
        pin_mut!(updates_subscriber);

        // No update, stream is pending.
        assert_matches!(updates_subscriber.as_mut().poll_next(&mut context), Poll::Pending);
        assert_eq!(*counter_waker.number_of_wakeup.lock().unwrap(), 0);

        // Let's generate an update.
        linked_chunk.push_items_back(['a']);

        // The waker must have been called.
        assert_eq!(*counter_waker.number_of_wakeup.lock().unwrap(), 1);

        // There is an update! Right after that, the stream is pending again.
        assert_matches!(
            updates_subscriber.as_mut().poll_next(&mut context),
            Poll::Ready(Some(items)) => {
                assert_eq!(
                    items,
                    &[InsertItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a'] }]
                );
            }
        );
        assert_matches!(updates_subscriber.as_mut().poll_next(&mut context), Poll::Pending);

        // Let's generate two other updates.
        linked_chunk.push_items_back(['b']);
        linked_chunk.push_items_back(['c']);

        // The waker must have been called only once for the two updates.
        assert_eq!(*counter_waker.number_of_wakeup.lock().unwrap(), 2);

        // We can consume the updates without the stream, but the stream continues to
        // know it has updates.
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                InsertItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a'] },
                InsertItems { at: Position(ChunkIdentifier(0), 1), items: vec!['b'] },
                InsertItems { at: Position(ChunkIdentifier(0), 2), items: vec!['c'] },
            ]
        );
        assert_matches!(
            updates_subscriber.as_mut().poll_next(&mut context),
            Poll::Ready(Some(items)) => {
                assert_eq!(
                    items,
                    &[
                        InsertItems { at: Position(ChunkIdentifier(0), 1), items: vec!['b'] },
                        InsertItems { at: Position(ChunkIdentifier(0), 2), items: vec!['c'] },
                    ]
                );
            }
        );
        assert_matches!(updates_subscriber.as_mut().poll_next(&mut context), Poll::Pending);

        // When dropping the `LinkedChunk`, it closes the stream.
        drop(linked_chunk);
        assert_matches!(updates_subscriber.as_mut().poll_next(&mut context), Poll::Ready(None));

        // Wakers calls have not changed.
        assert_eq!(*counter_waker.number_of_wakeup.lock().unwrap(), 2);
    }

    #[test]
    fn test_push_items() {
        use super::LinkedChunkUpdate::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();
        linked_chunk.push_items_back(['a']);

        assert_items_eq!(linked_chunk, ['a']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[InsertItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a'] }]
        );

        linked_chunk.push_items_back(['b', 'c']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[InsertItems { at: Position(ChunkIdentifier(0), 1), items: vec!['b', 'c'] }]
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
                InsertItems { at: Position(ChunkIdentifier(1), 0), items: vec!['d', 'e'] }
            ]
        );

        linked_chunk.push_items_back(['f', 'g', 'h', 'i', 'j']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e', 'f'] ['g', 'h', 'i'] ['j']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                InsertItems { at: Position(ChunkIdentifier(1), 2), items: vec!['f'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(1)),
                    new: ChunkIdentifier(2),
                    next: None,
                },
                InsertItems { at: Position(ChunkIdentifier(2), 0), items: vec!['g', 'h', 'i'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(2)),
                    new: ChunkIdentifier(3),
                    next: None,
                },
                InsertItems { at: Position(ChunkIdentifier(3), 0), items: vec!['j'] },
            ]
        );

        assert_eq!(linked_chunk.len(), 10);
    }

    #[test]
    fn test_push_gap() {
        use super::LinkedChunkUpdate::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();
        linked_chunk.push_items_back(['a']);
        assert_items_eq!(linked_chunk, ['a']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[InsertItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a'] }]
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
                InsertItems { at: Position(ChunkIdentifier(2), 0), items: vec!['b', 'c', 'd'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(2)),
                    new: ChunkIdentifier(3),
                    next: None,
                },
                InsertItems { at: Position(ChunkIdentifier(3), 0), items: vec!['e'] },
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
                InsertItems { at: Position(ChunkIdentifier(6), 0), items: vec!['f', 'g', 'h'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(6)),
                    new: ChunkIdentifier(7),
                    next: None,
                },
                InsertItems { at: Position(ChunkIdentifier(7), 0), items: vec!['i'] },
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
    fn test_rchunks_from() -> Result<(), LinkedChunkError> {
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
    fn test_chunks_from() -> Result<(), LinkedChunkError> {
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
    fn test_ritems_from() -> Result<(), LinkedChunkError> {
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
    fn test_items_from() -> Result<(), LinkedChunkError> {
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
    fn test_insert_items_at() -> Result<(), LinkedChunkError> {
        use super::LinkedChunkUpdate::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();
        linked_chunk.push_items_back(['a', 'b', 'c', 'd', 'e', 'f']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e', 'f']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                InsertItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a', 'b', 'c'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(0)),
                    new: ChunkIdentifier(1),
                    next: None,
                },
                InsertItems { at: Position(ChunkIdentifier(1), 0), items: vec!['d', 'e', 'f'] },
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
                    TruncateItems { chunk: ChunkIdentifier(1), length: 1 },
                    InsertItems { at: Position(ChunkIdentifier(1), 1), items: vec!['w', 'x'] },
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(1)),
                        new: ChunkIdentifier(2),
                        next: None,
                    },
                    InsertItems { at: Position(ChunkIdentifier(2), 0), items: vec!['y', 'z'] },
                    InsertItems { at: Position(ChunkIdentifier(2), 2), items: vec!['e'] },
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(2)),
                        new: ChunkIdentifier(3),
                        next: None,
                    },
                    InsertItems { at: Position(ChunkIdentifier(3), 0), items: vec!['f'] },
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
                    TruncateItems { chunk: ChunkIdentifier(0), length: 0 },
                    InsertItems { at: Position(ChunkIdentifier(0), 0), items: vec!['l', 'm', 'n'] },
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(0)),
                        new: ChunkIdentifier(4),
                        next: Some(ChunkIdentifier(1)),
                    },
                    InsertItems { at: Position(ChunkIdentifier(4), 0), items: vec!['o'] },
                    InsertItems { at: Position(ChunkIdentifier(4), 1), items: vec!['a', 'b'] },
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(4)),
                        new: ChunkIdentifier(5),
                        next: Some(ChunkIdentifier(1)),
                    },
                    InsertItems { at: Position(ChunkIdentifier(5), 0), items: vec!['c'] },
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
                    TruncateItems { chunk: ChunkIdentifier(5), length: 0 },
                    InsertItems { at: Position(ChunkIdentifier(5), 0), items: vec!['r', 's'] },
                    InsertItems { at: Position(ChunkIdentifier(5), 2), items: vec!['c'] },
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
                &[InsertItems { at: Position(ChunkIdentifier(3), 1), items: vec!['p', 'q'] },]
            );
            assert_eq!(linked_chunk.len(), 18);
        }

        // Insert in a chunk that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(['u', 'v'], Position(ChunkIdentifier(128), 0)),
                Err(LinkedChunkError::InvalidChunkIdentifier { identifier: ChunkIdentifier(128) })
            );
            assert!(linked_chunk.updates().unwrap().take().is_empty());
        }

        // Insert in a chunk that exists, but at an item that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(['u', 'v'], Position(ChunkIdentifier(0), 128)),
                Err(LinkedChunkError::InvalidItemIndex { index: 128 })
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
                Err(LinkedChunkError::ChunkIsAGap { identifier: ChunkIdentifier(6) })
            );
        }

        assert_eq!(linked_chunk.len(), 18);

        Ok(())
    }

    #[test]
    fn test_insert_gap_at() -> Result<(), LinkedChunkError> {
        use super::LinkedChunkUpdate::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();
        linked_chunk.push_items_back(['a', 'b', 'c', 'd', 'e', 'f']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e', 'f']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                InsertItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a', 'b', 'c'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(0)),
                    new: ChunkIdentifier(1),
                    next: None
                },
                InsertItems { at: Position(ChunkIdentifier(1), 0), items: vec!['d', 'e', 'f'] }
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
                    TruncateItems { chunk: ChunkIdentifier(0), length: 1 },
                    NewGapChunk {
                        previous: Some(ChunkIdentifier(0)),
                        new: ChunkIdentifier(2),
                        next: Some(ChunkIdentifier(1)),
                        gap: (),
                    },
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(2)),
                        new: ChunkIdentifier(3),
                        next: Some(ChunkIdentifier(1)),
                    },
                    InsertItems { at: Position(ChunkIdentifier(3), 0), items: vec!['b', 'c'] }
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
                    TruncateItems { chunk: ChunkIdentifier(0), length: 0 },
                    NewGapChunk {
                        previous: Some(ChunkIdentifier(0)),
                        new: ChunkIdentifier(4),
                        next: Some(ChunkIdentifier(2)),
                        gap: (),
                    },
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(4)),
                        new: ChunkIdentifier(5),
                        next: Some(ChunkIdentifier(2)),
                    },
                    InsertItems { at: Position(ChunkIdentifier(5), 0), items: vec!['a'] }
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
                Err(LinkedChunkError::InvalidItemIndex { index: 0 })
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
                    RemoveChunk(ChunkIdentifier(6))
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
                Err(LinkedChunkError::InvalidChunkIdentifier { identifier: ChunkIdentifier(128) })
            );
            assert!(linked_chunk.updates().unwrap().take().is_empty());
        }

        // Insert in a chunk that exists, but at an item that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(['u', 'v'], Position(ChunkIdentifier(0), 128)),
                Err(LinkedChunkError::InvalidItemIndex { index: 128 })
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
                Err(LinkedChunkError::ChunkIsAGap { identifier: ChunkIdentifier(4) })
            );
            assert!(linked_chunk.updates().unwrap().take().is_empty());
        }

        assert_eq!(linked_chunk.len(), 6);

        Ok(())
    }

    #[test]
    fn test_replace_gap_at() -> Result<(), LinkedChunkError> {
        use super::LinkedChunkUpdate::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();
        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['l', 'm']);
        assert_items_eq!(linked_chunk, ['a', 'b'] [-] ['l', 'm']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                InsertItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a', 'b'] },
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
                InsertItems { at: Position(ChunkIdentifier(2), 0), items: vec!['l', 'm'] }
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
                    InsertItems { at: Position(ChunkIdentifier(3), 0), items: vec!['d', 'e', 'f'] },
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(3)),
                        new: ChunkIdentifier(4),
                        next: Some(ChunkIdentifier(2)),
                    },
                    InsertItems { at: Position(ChunkIdentifier(4), 0), items: vec!['g', 'h'] },
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
                    InsertItems { at: Position(ChunkIdentifier(6), 0), items: vec!['w', 'x', 'y'] },
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(6)),
                        new: ChunkIdentifier(7),
                        next: None,
                    },
                    InsertItems { at: Position(ChunkIdentifier(7), 0), items: vec!['z'] },
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
