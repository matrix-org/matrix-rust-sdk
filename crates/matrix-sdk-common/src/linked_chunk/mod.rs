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

#![allow(rustdoc::private_intra_doc_links)]

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

                    let $crate::linked_chunk::ChunkContent::Items(items) = chunk.content() else {
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
pub mod lazy_loader;
mod order_tracker;
pub mod relational;
mod updates;

use std::{
    fmt::{self, Display},
    marker::PhantomData,
    ptr::NonNull,
    sync::atomic::{self, AtomicU64},
};

pub use as_vector::*;
pub use order_tracker::OrderTracker;
use ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId};
use serde::{Deserialize, Serialize};
pub use updates::*;

/// An identifier for a linked chunk; borrowed variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LinkedChunkId<'a> {
    Room(&'a RoomId),
    Thread(&'a RoomId, &'a EventId),
}

impl Display for LinkedChunkId<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Room(room_id) => write!(f, "{room_id}"),
            Self::Thread(room_id, thread_root) => {
                write!(f, "{room_id}:thread:{thread_root}")
            }
        }
    }
}

impl LinkedChunkId<'_> {
    pub fn storage_key(&self) -> impl '_ + AsRef<[u8]> {
        match self {
            LinkedChunkId::Room(room_id) => room_id.to_string(),
            LinkedChunkId::Thread(room_id, event_id) => format!("t:{room_id}:{event_id}"),
        }
    }

    pub fn to_owned(&self) -> OwnedLinkedChunkId {
        match self {
            LinkedChunkId::Room(room_id) => OwnedLinkedChunkId::Room((*room_id).to_owned()),
            LinkedChunkId::Thread(room_id, event_id) => {
                OwnedLinkedChunkId::Thread((*room_id).to_owned(), (*event_id).to_owned())
            }
        }
    }
}

impl<'a> From<&'a OwnedLinkedChunkId> for LinkedChunkId<'a> {
    fn from(value: &'a OwnedLinkedChunkId) -> Self {
        value.as_ref()
    }
}

impl PartialEq<&OwnedLinkedChunkId> for LinkedChunkId<'_> {
    fn eq(&self, other: &&OwnedLinkedChunkId) -> bool {
        match (self, other) {
            (LinkedChunkId::Room(a), OwnedLinkedChunkId::Room(b)) => *a == b,
            (LinkedChunkId::Thread(r, ev), OwnedLinkedChunkId::Thread(r2, ev2)) => {
                r == r2 && ev == ev2
            }
            (LinkedChunkId::Room(..), OwnedLinkedChunkId::Thread(..))
            | (LinkedChunkId::Thread(..), OwnedLinkedChunkId::Room(..)) => false,
        }
    }
}

impl PartialEq<LinkedChunkId<'_>> for OwnedLinkedChunkId {
    fn eq(&self, other: &LinkedChunkId<'_>) -> bool {
        other.eq(&self)
    }
}

/// An identifier for a linked chunk; owned variant.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OwnedLinkedChunkId {
    Room(OwnedRoomId),
    Thread(OwnedRoomId, OwnedEventId),
}

impl Display for OwnedLinkedChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ref().fmt(f)
    }
}

impl OwnedLinkedChunkId {
    pub fn as_ref(&self) -> LinkedChunkId<'_> {
        match self {
            OwnedLinkedChunkId::Room(room_id) => LinkedChunkId::Room(room_id.as_ref()),
            OwnedLinkedChunkId::Thread(room_id, event_id) => {
                LinkedChunkId::Thread(room_id.as_ref(), event_id.as_ref())
            }
        }
    }

    pub fn room_id(&self) -> &RoomId {
        match self {
            OwnedLinkedChunkId::Room(room_id) => room_id,
            OwnedLinkedChunkId::Thread(room_id, ..) => room_id,
        }
    }
}

impl From<LinkedChunkId<'_>> for OwnedLinkedChunkId {
    fn from(value: LinkedChunkId<'_>) -> Self {
        value.to_owned()
    }
}

/// Errors of [`LinkedChunk`].
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// A chunk identifier is invalid.
    #[error("The chunk identifier is invalid: `{identifier:?}`")]
    InvalidChunkIdentifier {
        /// The chunk identifier.
        identifier: ChunkIdentifier,
    },

    /// A chunk is a gap chunk, and it was expected to be an items.
    #[error("The chunk is a gap: `{identifier:?}`")]
    ChunkIsAGap {
        /// The chunk identifier.
        identifier: ChunkIdentifier,
    },

    /// A chunk is an items chunk, and it was expected to be a gap.
    #[error("The chunk is an item: `{identifier:?}`")]
    ChunkIsItems {
        /// The chunk identifier.
        identifier: ChunkIdentifier,
    },

    /// A chunk is an items chunk, and it was expected to be empty.
    #[error("The chunk is a non-empty item chunk: `{identifier:?}`")]
    RemovingNonEmptyItemsChunk {
        /// The chunk identifier.
        identifier: ChunkIdentifier,
    },

    /// We're trying to remove the only chunk in the `LinkedChunk`, and it can't
    /// be empty.
    #[error("Trying to remove the only chunk, but a linked chunk can't be empty")]
    RemovingLastChunk,

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

    /// Get the first chunk, as a mutable reference.
    fn first_chunk_mut(&mut self) -> &mut Chunk<CAP, Item, Gap> {
        unsafe { self.first.as_mut() }
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

    /// Drop all the chunks, leaving the chunk in an uninitialized state,
    /// because `Self::first` is a dangling pointer.
    ///
    /// SAFETY: the caller is responsible of ensuring that this is the last use
    /// of the linked chunk, or that first will be re-initialized before any
    /// other use.
    unsafe fn clear(&mut self) {
        // Loop over all chunks, from the last to the first chunk, and drop them.
        // Take the latest chunk.
        let mut current_chunk_ptr = self.last.or(Some(self.first));

        // As long as we have another chunk…
        while let Some(chunk_ptr) = current_chunk_ptr {
            // Fetch the previous chunk pointer.
            let previous_ptr = unsafe { chunk_ptr.as_ref() }.previous;

            // Re-box the chunk, and let Rust do its job.
            let _chunk_boxed = unsafe { Box::from_raw(chunk_ptr.as_ptr()) };

            // Update the `current_chunk_ptr`.
            current_chunk_ptr = previous_ptr;
        }

        // At this step, all chunks have been dropped, including `self.first`.
        self.first = NonNull::dangling();
        self.last = None;
    }

    /// Drop all chunks, and replace the first one with the one provided as an
    /// argument.
    fn replace_with(&mut self, first_chunk: NonNull<Chunk<CAP, Item, Gap>>) {
        // SAFETY: we're resetting `self.first` afterwards.
        unsafe {
            self.clear();
        }

        // At this step, all chunks have been dropped, including `self.first`.
        self.first = first_chunk;
    }

    /// Drop all chunks, and re-create the default first one.
    ///
    /// The default first chunk is an empty items chunk, with the identifier
    /// [`ChunkIdentifierGenerator::FIRST_IDENTIFIER`].
    fn reset(&mut self) {
        self.replace_with(Chunk::new_items_leaked(ChunkIdentifierGenerator::FIRST_IDENTIFIER));
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

    /// The generator of chunk identifiers.
    chunk_identifier_generator: ChunkIdentifierGenerator,

    /// All updates that have been made on this `LinkedChunk`. If this field is
    /// `Some(…)`, update history is enabled, otherwise, if it's `None`, update
    /// history is disabled.
    updates: Option<ObservableUpdates<Item, Gap>>,

    /// Marker.
    marker: PhantomData<Box<Chunk<CHUNK_CAPACITY, Item, Gap>>>,
}

impl<const CAP: usize, Item, Gap> Default for LinkedChunk<CAP, Item, Gap> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CAP: usize, Item, Gap> LinkedChunk<CAP, Item, Gap> {
    /// Create a new [`Self`].
    pub fn new() -> Self {
        Self {
            links: Ends {
                first: Chunk::new_items_leaked(ChunkIdentifierGenerator::FIRST_IDENTIFIER),
                last: None,
            },
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
        let first_chunk_identifier = ChunkIdentifierGenerator::FIRST_IDENTIFIER;

        let mut updates = ObservableUpdates::new();
        updates.push(Update::NewItemsChunk {
            previous: None,
            new: first_chunk_identifier,
            next: None,
        });

        Self {
            links: Ends { first: Chunk::new_items_leaked(first_chunk_identifier), last: None },
            chunk_identifier_generator: ChunkIdentifierGenerator::new_from_scratch(),
            updates: Some(updates),
            marker: PhantomData,
        }
    }

    /// Clear all the chunks.
    pub fn clear(&mut self) {
        // Clear `self.links`.
        self.links.reset();

        // Clear `self.chunk_identifier_generator`.
        self.chunk_identifier_generator = ChunkIdentifierGenerator::new_from_scratch();

        // “Clear” `self.updates`.
        if let Some(updates) = self.updates.as_mut() {
            // Clear the previous updates, as we're about to insert a clear they would be
            // useless.
            updates.clear_pending();
            updates.push(Update::Clear);
            updates.push(Update::NewItemsChunk {
                previous: None,
                new: ChunkIdentifierGenerator::FIRST_IDENTIFIER,
                next: None,
            })
        }
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

        let last_chunk = self.links.latest_chunk_mut();

        // Push the items.
        let last_chunk =
            last_chunk.push_items(items, &self.chunk_identifier_generator, &mut self.updates);

        debug_assert!(last_chunk.is_last_chunk(), "`last_chunk` must be… the last chunk");

        // We need to update `self.links.last` if and only if `last_chunk` _is not_ the
        // first chunk, and _is_ the last chunk (ensured by the `debug_assert!`
        // above).
        if !last_chunk.is_first_chunk() {
            // Maybe `last_chunk` is the same as the previous `self.links.last` chunk, but
            // it's OK.
            self.links.last = Some(last_chunk.as_ptr());
        }
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
    pub fn insert_items_at<I>(&mut self, position: Position, items: I) -> Result<(), Error>
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

        let chunk = match &mut chunk.content {
            ChunkContent::Gap(..) => {
                return Err(Error::ChunkIsAGap { identifier: chunk_identifier });
            }

            ChunkContent::Items(current_items) => {
                let current_items_length = current_items.len();

                if item_index > current_items_length {
                    return Err(Error::InvalidItemIndex { index: item_index });
                }

                // Prepare the items to be pushed.
                let items = items.into_iter();

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
                }
            }
        };

        // We need to update `self.links.last` if and only if `chunk` _is not_ the first
        // chunk, and _is_ the last chunk.
        if !chunk.is_first_chunk() && chunk.is_last_chunk() {
            // Maybe `chunk` is the same as the previous `self.links.last` chunk, but it's
            // OK.
            self.links.last = Some(chunk.as_ptr());
        }

        Ok(())
    }

    /// Remove item at a specified position in the [`LinkedChunk`].
    ///
    /// `position` must point to a valid item, otherwise the method returns
    /// `Err`.
    ///
    /// The chunk containing the item represented by `position` may be empty
    /// once the item has been removed. In this case, the chunk will be removed.
    pub fn remove_item_at(&mut self, position: Position) -> Result<Item, Error> {
        let chunk_identifier = position.chunk_identifier();
        let item_index = position.index();

        let mut chunk_ptr = None;
        let removed_item;

        {
            let chunk = self
                .links
                .chunk_mut(chunk_identifier)
                .ok_or(Error::InvalidChunkIdentifier { identifier: chunk_identifier })?;

            let current_items = match &mut chunk.content {
                ChunkContent::Gap(..) => {
                    return Err(Error::ChunkIsAGap { identifier: chunk_identifier });
                }
                ChunkContent::Items(current_items) => current_items,
            };

            if item_index >= current_items.len() {
                return Err(Error::InvalidItemIndex { index: item_index });
            }

            removed_item = current_items.remove(item_index);

            if let Some(updates) = self.updates.as_mut() {
                updates.push(Update::RemoveItem { at: Position(chunk_identifier, item_index) })
            }

            // If the chunk is empty and not the first one, we can remove it.
            if current_items.is_empty() && !chunk.is_first_chunk() {
                // Unlink `chunk`.
                chunk.unlink(self.updates.as_mut());

                chunk_ptr = Some(chunk.as_ptr());

                // We need to update `self.links.last` if and only if `chunk` _is_ the last
                // chunk. The new last chunk is the chunk before `chunk`.
                if chunk.is_last_chunk() {
                    self.links.last = chunk.previous;
                }
            }

            // Stop borrowing `chunk`.
        }

        if let Some(chunk_ptr) = chunk_ptr {
            // `chunk` has been unlinked.

            // Re-box the chunk, and let Rust do its job.
            //
            // SAFETY: `chunk` is unlinked and not borrowed anymore. `LinkedChunk` doesn't
            // use it anymore, it's a leak. It is time to re-`Box` it and drop it.
            let _chunk_boxed = unsafe { Box::from_raw(chunk_ptr.as_ptr()) };
        }

        Ok(removed_item)
    }

    /// Replace item at a specified position in the [`LinkedChunk`].
    ///
    /// `position` must point to a valid item, otherwise the method returns
    /// `Err`.
    pub fn replace_item_at(&mut self, position: Position, item: Item) -> Result<(), Error>
    where
        Item: Clone,
    {
        let chunk_identifier = position.chunk_identifier();
        let item_index = position.index();

        let chunk = self
            .links
            .chunk_mut(chunk_identifier)
            .ok_or(Error::InvalidChunkIdentifier { identifier: chunk_identifier })?;

        match &mut chunk.content {
            ChunkContent::Gap(..) => {
                return Err(Error::ChunkIsAGap { identifier: chunk_identifier });
            }

            ChunkContent::Items(current_items) => {
                if item_index >= current_items.len() {
                    return Err(Error::InvalidItemIndex { index: item_index });
                }

                // Avoid one spurious clone by notifying about the update *before* applying it.
                if let Some(updates) = self.updates.as_mut() {
                    updates.push(Update::ReplaceItem {
                        at: Position(chunk_identifier, item_index),
                        item: item.clone(),
                    });
                }

                current_items[item_index] = item;
            }
        }

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

        let chunk = match &mut chunk.content {
            ChunkContent::Gap(..) => {
                return Err(Error::ChunkIsAGap { identifier: chunk_identifier });
            }

            ChunkContent::Items(current_items) => {
                // If `item_index` is 0, we don't want to split the current items chunk to
                // insert a new gap chunk, otherwise it would create an empty current items
                // chunk. Let's handle this case in particular.
                if item_index == 0 {
                    let chunk_was_first = chunk.is_first_chunk();
                    let chunk_was_last = chunk.is_last_chunk();

                    let new_chunk = chunk.insert_before(
                        Chunk::new_gap_leaked(self.chunk_identifier_generator.next(), content),
                        self.updates.as_mut(),
                    );

                    let new_chunk_ptr = new_chunk.as_ptr();
                    let chunk_ptr = chunk.as_ptr();

                    // `chunk` was the first: let's update `self.links.first`.
                    //
                    // If `chunk` was not the first but was the last, there is nothing to do,
                    // `self.links.last` is already up-to-date.
                    if chunk_was_first {
                        self.links.first = new_chunk_ptr;

                        // `chunk` was the first __and__ the last: let's set `self.links.last`.
                        if chunk_was_last {
                            self.links.last = Some(chunk_ptr);
                        }
                    }

                    return Ok(());
                }

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

        // We need to update `self.links.last` if and only if `chunk` _is not_ the first
        // chunk, and _is_ the last chunk.
        if !chunk.is_first_chunk() && chunk.is_last_chunk() {
            // Maybe `chunk` is the same as the previous `self.links.last` chunk, but it's
            // OK.
            self.links.last = Some(chunk.as_ptr());
        }

        Ok(())
    }

    /// Remove a chunk with the given identifier iff it's empty.
    ///
    /// A chunk is considered empty if:
    /// - it's a gap chunk, or
    /// - it's an items chunk with no items.
    ///
    /// This returns the next insert position, viz. the start of the next
    /// chunk, if any, or none if there was no next chunk.
    pub fn remove_empty_chunk_at(
        &mut self,
        chunk_identifier: ChunkIdentifier,
    ) -> Result<Option<Position>, Error> {
        // Check that we're not removing the last chunk.
        if self.links.first_chunk().is_last_chunk() {
            return Err(Error::RemovingLastChunk);
        }

        let chunk = self
            .links
            .chunk_mut(chunk_identifier)
            .ok_or(Error::InvalidChunkIdentifier { identifier: chunk_identifier })?;

        if chunk.num_items() > 0 {
            return Err(Error::RemovingNonEmptyItemsChunk { identifier: chunk_identifier });
        }

        let chunk_was_first = chunk.is_first_chunk();
        let chunk_was_last = chunk.is_last_chunk();
        let next_ptr = chunk.next;
        let previous_ptr = chunk.previous;
        let position_of_next = chunk.next().map(|next| next.first_position());

        chunk.unlink(self.updates.as_mut());

        let chunk_ptr = chunk.as_ptr();

        // If the chunk is the first one, we need to update `self.links.first`…
        if chunk_was_first {
            // … if and only if there is a next chunk.
            if let Some(next_ptr) = next_ptr {
                self.links.first = next_ptr;
            }
        }

        if chunk_was_last {
            self.links.last = previous_ptr;
        }

        // SAFETY: `chunk` is unlinked and not borrowed anymore. `LinkedChunk` doesn't
        // use it anymore, it's a leak. It is time to re-`Box` it and drop it.
        let _chunk_boxed = unsafe { Box::from_raw(chunk_ptr.as_ptr()) };

        // Return the first position of the next chunk, if any.
        Ok(position_of_next)
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

            if chunk.is_items() {
                return Err(Error::ChunkIsItems { identifier: chunk_identifier });
            }

            let chunk_was_first = chunk.is_first_chunk();

            let maybe_last_chunk_ptr = {
                let items = items.into_iter();

                let last_inserted_chunk = chunk
                    // Insert a new items chunk…
                    .insert_next(
                        Chunk::new_items_leaked(self.chunk_identifier_generator.next()),
                        &mut self.updates,
                    )
                    // … and insert the items.
                    .push_items(items, &self.chunk_identifier_generator, &mut self.updates);

                last_inserted_chunk.is_last_chunk().then(|| last_inserted_chunk.as_ptr())
            };

            new_chunk_ptr = chunk
                .next
                // SAFETY: A new `Chunk` has just been inserted, so it exists.
                .unwrap();

            // Now that new items have been pushed, we can unlink the gap chunk.
            chunk.unlink(self.updates.as_mut());

            // Get the pointer to `chunk`.
            chunk_ptr = chunk.as_ptr();

            // Update `self.links.first` if the gap chunk was the first chunk.
            if chunk_was_first {
                self.links.first = new_chunk_ptr;
            }

            // Update `self.links.last` if the gap (so the new) chunk was (is) the last
            // chunk.
            if let Some(last_chunk_ptr) = maybe_last_chunk_ptr {
                self.links.last = Some(last_chunk_ptr);
            }

            // Stop borrowing `chunk`.
        }

        // Re-box the chunk, and let Rust do its job.
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

                move |(Position(chunk_identifier, item_index), _item)| {
                    *chunk_identifier == position.chunk_identifier()
                        && *item_index != expected_index
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
    #[must_use]
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

    /// Get an [`OrderTracker`] for the linked chunk, which can be used to
    /// compare the relative position of two events in this linked chunk.
    ///
    /// A pre-requisite is that the linked chunk has been constructed with
    /// [`Self::new_with_update_history`], and that if the linked chunk is
    /// lazily-loaded, an iterator over the fully-loaded linked chunk is
    /// passed at construction time here.
    pub fn order_tracker(
        &mut self,
        all_chunks: Option<Vec<ChunkMetadata>>,
    ) -> Option<OrderTracker<Item, Gap>>
    where
        Item: Clone,
    {
        let (updates, token) = self
            .updates
            .as_mut()
            .map(|updates| (updates.inner.clone(), updates.new_reader_token()))?;

        Some(OrderTracker::new(
            updates,
            token,
            all_chunks.unwrap_or_else(|| {
                // Consider the linked chunk as fully loaded.
                self.chunks()
                    .map(|chunk| ChunkMetadata {
                        identifier: chunk.identifier(),
                        num_items: chunk.num_items(),
                        previous: chunk.previous().map(|prev| prev.identifier()),
                        next: chunk.next().map(|next| next.identifier()),
                    })
                    .collect()
            }),
        ))
    }

    /// Returns the number of items of the linked chunk.
    pub fn num_items(&self) -> usize {
        self.items().count()
    }
}

impl<const CAP: usize, Item, Gap> Drop for LinkedChunk<CAP, Item, Gap> {
    fn drop(&mut self) {
        // Clear the links, which will drop all the chunks.
        //
        // Calling `Self::clear` would be an error as we don't want to emit an
        // `Update::Clear` when `self` is dropped. Instead, we only care about
        // freeing memory correctly. Rust can take care of everything except the
        // pointers in `self.links`, hence the specific call to `self.links.clear()`.
        //
        // SAFETY: this is the last use of the linked chunk, so leaving it in a dangling
        // state is fine.
        unsafe {
            self.links.clear();
        }
    }
}

/// A [`LinkedChunk`] can be safely sent over thread boundaries if `Item: Send`
/// and `Gap: Send`. The only unsafe part is around the `NonNull`, but the API
/// and the lifetimes to deref them are designed safely.
unsafe impl<const CAP: usize, Item: Send, Gap: Send> Send for LinkedChunk<CAP, Item, Gap> {}

/// A [`LinkedChunk`] can be safely share between threads if `Item: Sync` and
/// `Gap: Sync`. The only unsafe part is around the `NonNull`, but the API and
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
#[derive(Debug)]
pub struct ChunkIdentifierGenerator {
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
    fn next(&self) -> ChunkIdentifier {
        let previous = self.next.fetch_add(1, atomic::Ordering::Relaxed);

        // Check for overflows.
        // unlikely — TODO: call `std::intrinsics::unlikely` once it's stable.
        if previous == u64::MAX {
            panic!(
                "No more chunk identifiers available. Congrats, you did it. \
                 2^64 identifiers have been consumed."
            )
        }

        ChunkIdentifier(previous + 1)
    }

    /// Get the current chunk identifier.
    //
    // This is hidden because it's used only in the tests.
    #[doc(hidden)]
    pub fn current(&self) -> ChunkIdentifier {
        ChunkIdentifier(self.next.load(atomic::Ordering::Relaxed))
    }
}

/// The unique identifier of a chunk in a [`LinkedChunk`].
///
/// It is not the position of the chunk, just its unique identifier.
///
/// Learn more with [`ChunkIdentifierGenerator`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct ChunkIdentifier(u64);

impl ChunkIdentifier {
    /// Create a new [`ChunkIdentifier`].
    pub fn new(identifier: u64) -> Self {
        Self(identifier)
    }

    /// Get the underlying identifier.
    pub fn index(&self) -> u64 {
        self.0
    }
}

impl PartialEq<u64> for ChunkIdentifier {
    fn eq(&self, other: &u64) -> bool {
        self.0 == *other
    }
}

/// The position of something inside a [`Chunk`].
///
/// It's a pair of a chunk position and an item index.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Position(ChunkIdentifier, usize);

impl Position {
    /// Create a new [`Position`].
    pub fn new(chunk_identifier: ChunkIdentifier, index: usize) -> Self {
        Self(chunk_identifier, index)
    }

    /// Get the chunk identifier of the item.
    pub fn chunk_identifier(&self) -> ChunkIdentifier {
        self.0
    }

    /// Get the index inside the chunk.
    pub fn index(&self) -> usize {
        self.1
    }

    /// Decrement the index part (see [`Self::index`]), i.e. subtract 1.
    ///
    /// # Panic
    ///
    /// This method will panic if it will underflow, i.e. if the index is 0.
    pub fn decrement_index(&mut self) {
        self.1 = self.1.checked_sub(1).expect("Cannot decrement the index because it's already 0");
    }

    /// Increment the index part (see [`Self::index`]), i.e. add 1.
    ///
    /// # Panic
    ///
    /// This method will panic if it will overflow, i.e. if the index is larger
    /// than `usize::MAX`.
    pub fn increment_index(&mut self) {
        self.1 = self.1.checked_add(1).expect("Cannot increment the index because it's too large");
    }
}

/// An iterator over a [`LinkedChunk`] that traverses the chunk in backward
/// direction (i.e. it calls `previous` on each chunk to make progress).
#[derive(Debug)]
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
        self.chunk.inspect(|chunk| self.chunk = chunk.previous())
    }
}

/// An iterator over a [`LinkedChunk`] that traverses the chunk in forward
/// direction (i.e. it calls `next` on each chunk to make progress).
#[derive(Debug)]
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
        self.chunk.inspect(|chunk| self.chunk = chunk.next())
    }
}

/// This enum represents the content of a [`Chunk`].
#[derive(Clone, Debug)]
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

    /// If this chunk is the first one, and if the `LinkedChunk` is loaded
    /// lazily, chunk-by-chunk, this is the identifier of the previous chunk.
    /// This previous chunk is not loaded yet, so it's impossible to get a
    /// pointer to it yet. However we know its identifier.
    lazy_previous: Option<ChunkIdentifier>,

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
        Self { previous: None, lazy_previous: None, next: None, identifier, content }
    }

    /// Create a new chunk given some content, but box it and leak it.
    fn new_leaked(identifier: ChunkIdentifier, content: ChunkContent<Item, Gap>) -> NonNull<Self> {
        let chunk = Self::new(identifier, content);
        let chunk_box = Box::new(chunk);

        NonNull::from(Box::leak(chunk_box))
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

    /// Is this the definitive first chunk, even in the presence of
    /// lazy-loading?
    pub fn is_definitive_head(&self) -> bool {
        self.previous.is_none() && self.lazy_previous.is_none()
    }

    /// Check whether this current chunk is the first chunk.
    fn is_first_chunk(&self) -> bool {
        self.previous.is_none()
    }

    /// Check whether this current chunk is the last chunk.
    fn is_last_chunk(&self) -> bool {
        self.next.is_none()
    }

    /// Return the link to the previous chunk, if it was loaded lazily.
    ///
    /// Doc hidden because this is mostly for internal debugging purposes.
    #[doc(hidden)]
    pub fn lazy_previous(&self) -> Option<ChunkIdentifier> {
        self.lazy_previous
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
            ChunkContent::Items(items) => Position(identifier, items.len().saturating_sub(1)),
        }
    }

    /// The number of items in the linked chunk.
    ///
    /// It will always return 0 if it's a gap chunk.
    pub fn num_items(&self) -> usize {
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
        // A small optimisation. Skip early if there is no new items.
        if new_items.len() == 0 {
            return self;
        }

        let identifier = self.identifier();
        let prev_num_items = self.num_items();

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
                let free_space = CAPACITY.saturating_sub(prev_num_items);

                // There is enough space to push all the new items.
                if new_items.len() <= free_space {
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

    /// Insert a new chunk before the current one.
    ///
    /// The respective [`Self::previous`] and [`Self::next`] of the current
    /// and new chunk will be updated accordingly.
    fn insert_before(
        &mut self,
        mut new_chunk_ptr: NonNull<Self>,
        updates: Option<&mut ObservableUpdates<Item, Gap>>,
    ) -> &mut Self
    where
        Gap: Clone,
    {
        let new_chunk = unsafe { new_chunk_ptr.as_mut() };

        // Update the previous chunk if any.
        if let Some(previous_chunk) = self.previous_mut() {
            // Link back to the new chunk.
            previous_chunk.next = Some(new_chunk_ptr);

            // Link the new chunk to the next chunk.
            new_chunk.previous = self.previous;
        }
        // No previous: `self` is the first! We need to move the `lazy_previous` from `self` to
        // `new_chunk`.
        else {
            new_chunk.lazy_previous = self.lazy_previous.take();
        }

        // Link to the new chunk.
        self.previous = Some(new_chunk_ptr);
        // Link the new chunk to this one.
        new_chunk.next = Some(self.as_ptr());

        if let Some(updates) = updates {
            let previous = new_chunk.previous().map(Chunk::identifier).or(new_chunk.lazy_previous);
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
    fn unlink(&mut self, updates: Option<&mut ObservableUpdates<Item, Gap>>) {
        let previous_ptr = self.previous;
        let next_ptr = self.next;
        // If `self` is not the first, `lazy_previous` might be set on its previous
        // chunk. Otherwise, if `lazy_previous` is set on `self`, it means it's the
        // first chunk and it must be moved onto the next chunk.
        let lazy_previous = self.lazy_previous.take();

        if let Some(previous) = self.previous_mut() {
            previous.next = next_ptr;
        }

        if let Some(next) = self.next_mut() {
            next.previous = previous_ptr;
            next.lazy_previous = lazy_previous;
        }

        if let Some(updates) = updates {
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

/// The raw representation of a linked chunk, as persisted in storage.
///
/// It may rebuilt into [`Chunk`] and shares the same internal representation,
/// except that links are materialized using [`ChunkIdentifier`] instead of raw
/// pointers to the previous and next chunks.
#[derive(Clone, Debug)]
pub struct RawChunk<Item, Gap> {
    /// Content section of the linked chunk.
    pub content: ChunkContent<Item, Gap>,

    /// Link to the previous chunk, via its identifier.
    pub previous: Option<ChunkIdentifier>,

    /// Current chunk's identifier.
    pub identifier: ChunkIdentifier,

    /// Link to the next chunk, via its identifier.
    pub next: Option<ChunkIdentifier>,
}

/// A simplified [`RawChunk`] that only contains the number of items in a chunk,
/// instead of its type.
#[derive(Clone, Debug)]
pub struct ChunkMetadata {
    /// The number of items in this chunk.
    ///
    /// By convention, a gap chunk contains 0 items.
    pub num_items: usize,

    /// Link to the previous chunk, via its identifier.
    pub previous: Option<ChunkIdentifier>,

    /// Current chunk's identifier.
    pub identifier: ChunkIdentifier,

    /// Link to the next chunk, via its identifier.
    pub next: Option<ChunkIdentifier>,
}

#[cfg(test)]
mod tests {
    use std::{
        ops::Not,
        sync::{Arc, atomic::Ordering},
    };

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

        assert_eq!(items.num_items(), 0);

        // This test also ensures that `Drop` for `LinkedChunk` works when
        // there is only one chunk.
    }

    #[test]
    fn test_updates() {
        assert!(LinkedChunk::<3, char, ()>::new().updates().is_none());
        assert!(LinkedChunk::<3, char, ()>::new_with_update_history().updates().is_some());
    }

    #[test]
    fn test_new_with_initial_update() {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[NewItemsChunk { previous: None, new: ChunkIdentifier(0), next: None }]
        );
    }

    #[test]
    fn test_push_items() {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        // Ignore initial update.
        let _ = linked_chunk.updates().unwrap().take();

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

        assert_eq!(linked_chunk.num_items(), 10);
    }

    #[test]
    fn test_push_gap() {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        // Ignore initial update.
        let _ = linked_chunk.updates().unwrap().take();

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

        assert_eq!(linked_chunk.num_items(), 9);
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
    fn test_ritems_with_final_gap() -> Result<(), Error> {
        let mut linked_chunk = LinkedChunk::<3, char, ()>::new();
        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['c', 'd', 'e']);
        linked_chunk.push_gap_back(());

        let mut iterator = linked_chunk.ritems();

        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(2), 2), 'e')));
        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(2), 1), 'd')));
        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(2), 0), 'c')));
        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(0), 1), 'b')));
        assert_matches!(iterator.next(), Some((Position(ChunkIdentifier(0), 0), 'a')));
        assert_matches!(iterator.next(), None);

        Ok(())
    }

    #[test]
    fn test_ritems_empty() {
        let linked_chunk = LinkedChunk::<2, char, ()>::new();
        let mut iterator = linked_chunk.ritems();

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
    fn test_items_empty() {
        let linked_chunk = LinkedChunk::<2, char, ()>::new();
        let mut iterator = linked_chunk.items();

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

        // Ignore initial update.
        let _ = linked_chunk.updates().unwrap().take();

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
            let pos_e = linked_chunk.item_position(|item| *item == 'e').unwrap();

            // Insert 4 elements, so that it overflows the chunk capacity. It's important to
            // see whether chunks are correctly updated and linked.
            linked_chunk.insert_items_at(pos_e, ['w', 'x', 'y', 'z'])?;

            assert_items_eq!(
                linked_chunk,
                ['a', 'b', 'c'] ['d', 'w', 'x'] ['y', 'z', 'e'] ['f']
            );
            assert_eq!(linked_chunk.num_items(), 10);
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
            let pos_a = linked_chunk.item_position(|item| *item == 'a').unwrap();
            linked_chunk.insert_items_at(pos_a, ['l', 'm', 'n', 'o'])?;

            assert_items_eq!(
                linked_chunk,
                ['l', 'm', 'n'] ['o', 'a', 'b'] ['c'] ['d', 'w', 'x'] ['y', 'z', 'e'] ['f']
            );
            assert_eq!(linked_chunk.num_items(), 14);
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
            let pos_c = linked_chunk.item_position(|item| *item == 'c').unwrap();
            linked_chunk.insert_items_at(pos_c, ['r', 's'])?;

            assert_items_eq!(
                linked_chunk,
                ['l', 'm', 'n'] ['o', 'a', 'b'] ['r', 's', 'c'] ['d', 'w', 'x'] ['y', 'z', 'e'] ['f']
            );
            assert_eq!(linked_chunk.num_items(), 16);
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
            let pos_f = linked_chunk.item_position(|item| *item == 'f').unwrap();
            let pos_f = Position(pos_f.chunk_identifier(), pos_f.index() + 1);

            linked_chunk.insert_items_at(pos_f, ['p', 'q'])?;
            assert_items_eq!(
                linked_chunk,
                ['l', 'm', 'n'] ['o', 'a', 'b'] ['r', 's', 'c'] ['d', 'w', 'x'] ['y', 'z', 'e'] ['f', 'p', 'q']
            );
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[PushItems { at: Position(ChunkIdentifier(3), 1), items: vec!['p', 'q'] }]
            );
            assert_eq!(linked_chunk.num_items(), 18);
        }

        // Insert in a chunk that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(Position(ChunkIdentifier(128), 0), ['u', 'v'],),
                Err(Error::InvalidChunkIdentifier { identifier: ChunkIdentifier(128) })
            );
            assert!(linked_chunk.updates().unwrap().take().is_empty());
        }

        // Insert in a chunk that exists, but at an item that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(Position(ChunkIdentifier(0), 128), ['u', 'v'],),
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
                linked_chunk.insert_items_at(Position(ChunkIdentifier(6), 0), ['u', 'v'],),
                Err(Error::ChunkIsAGap { identifier: ChunkIdentifier(6) })
            );
        }

        assert_eq!(linked_chunk.num_items(), 18);

        Ok(())
    }

    #[test]
    fn test_insert_items_at_last_chunk() -> Result<(), Error> {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        // Ignore initial update.
        let _ = linked_chunk.updates().unwrap().take();

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
        let pos_e = linked_chunk.item_position(|item| *item == 'e').unwrap();

        // Insert 4 elements, so that it overflows the chunk capacity. It's important to
        // see whether chunks are correctly updated and linked.
        linked_chunk.insert_items_at(pos_e, ['w', 'x', 'y', 'z'])?;

        assert_items_eq!(
            linked_chunk,
            ['a', 'b', 'c'] ['d', 'w', 'x'] ['y', 'z', 'e'] ['f']
        );
        assert_eq!(linked_chunk.num_items(), 10);
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

        Ok(())
    }

    #[test]
    fn test_insert_items_at_first_chunk() -> Result<(), Error> {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        // Ignore initial update.
        let _ = linked_chunk.updates().unwrap().take();

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

        // Insert inside the first chunk.
        let pos_a = linked_chunk.item_position(|item| *item == 'a').unwrap();
        linked_chunk.insert_items_at(pos_a, ['l', 'm', 'n', 'o'])?;

        assert_items_eq!(
            linked_chunk,
            ['l', 'm', 'n'] ['o', 'a', 'b'] ['c'] ['d', 'e', 'f']
        );
        assert_eq!(linked_chunk.num_items(), 10);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                DetachLastItems { at: Position(ChunkIdentifier(0), 0) },
                PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['l', 'm', 'n'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(0)),
                    new: ChunkIdentifier(2),
                    next: Some(ChunkIdentifier(1)),
                },
                PushItems { at: Position(ChunkIdentifier(2), 0), items: vec!['o'] },
                StartReattachItems,
                PushItems { at: Position(ChunkIdentifier(2), 1), items: vec!['a', 'b'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(2)),
                    new: ChunkIdentifier(3),
                    next: Some(ChunkIdentifier(1)),
                },
                PushItems { at: Position(ChunkIdentifier(3), 0), items: vec!['c'] },
                EndReattachItems,
            ]
        );

        Ok(())
    }

    #[test]
    fn test_insert_items_at_middle_chunk() -> Result<(), Error> {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        // Ignore initial update.
        let _ = linked_chunk.updates().unwrap().take();

        linked_chunk.push_items_back(['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e', 'f'] ['g', 'h']);
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
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(1)),
                    new: ChunkIdentifier(2),
                    next: None,
                },
                PushItems { at: Position(ChunkIdentifier(2), 0), items: vec!['g', 'h'] },
            ]
        );

        let pos_d = linked_chunk.item_position(|item| *item == 'd').unwrap();
        linked_chunk.insert_items_at(pos_d, ['r', 's'])?;

        assert_items_eq!(
            linked_chunk,
            ['a', 'b', 'c'] ['r', 's', 'd'] ['e', 'f'] ['g', 'h']
        );
        assert_eq!(linked_chunk.num_items(), 10);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                DetachLastItems { at: Position(ChunkIdentifier(1), 0) },
                PushItems { at: Position(ChunkIdentifier(1), 0), items: vec!['r', 's'] },
                StartReattachItems,
                PushItems { at: Position(ChunkIdentifier(1), 2), items: vec!['d'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(1)),
                    new: ChunkIdentifier(3),
                    next: Some(ChunkIdentifier(2)),
                },
                PushItems { at: Position(ChunkIdentifier(3), 0), items: vec!['e', 'f'] },
                EndReattachItems,
            ]
        );

        Ok(())
    }

    #[test]
    fn test_insert_items_at_end_of_chunk() -> Result<(), Error> {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        // Ignore initial update.
        let _ = linked_chunk.updates().unwrap().take();

        linked_chunk.push_items_back(['a', 'b', 'c', 'd', 'e']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a', 'b', 'c'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(0)),
                    new: ChunkIdentifier(1),
                    next: None,
                },
                PushItems { at: Position(ChunkIdentifier(1), 0), items: vec!['d', 'e'] },
            ]
        );

        // Insert at the end of a chunk.
        let pos_e = linked_chunk.item_position(|item| *item == 'e').unwrap();
        let pos_after_e = Position(pos_e.chunk_identifier(), pos_e.index() + 1);

        linked_chunk.insert_items_at(pos_after_e, ['p', 'q'])?;
        assert_items_eq!(
            linked_chunk,
            ['a', 'b', 'c'] ['d', 'e', 'p'] ['q']
        );
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                PushItems { at: Position(ChunkIdentifier(1), 2), items: vec!['p'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(1)),
                    new: ChunkIdentifier(2),
                    next: None
                },
                PushItems { at: Position(ChunkIdentifier(2), 0), items: vec!['q'] }
            ]
        );
        assert_eq!(linked_chunk.num_items(), 7);

        Ok(())
    }

    #[test]
    fn test_insert_items_at_errs() -> Result<(), Error> {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        // Ignore initial update.
        let _ = linked_chunk.updates().unwrap().take();

        linked_chunk.push_items_back(['a', 'b', 'c']);
        linked_chunk.push_gap_back(());
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] [-]);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a', 'b', 'c'] },
                NewGapChunk {
                    previous: Some(ChunkIdentifier(0)),
                    new: ChunkIdentifier(1),
                    next: None,
                    gap: (),
                },
            ]
        );

        // Insert in a chunk that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(Position(ChunkIdentifier(128), 0), ['u', 'v'],),
                Err(Error::InvalidChunkIdentifier { identifier: ChunkIdentifier(128) })
            );
            assert!(linked_chunk.updates().unwrap().take().is_empty());
        }

        // Insert in a chunk that exists, but at an item that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(Position(ChunkIdentifier(0), 128), ['u', 'v'],),
                Err(Error::InvalidItemIndex { index: 128 })
            );
            assert!(linked_chunk.updates().unwrap().take().is_empty());
        }

        // Insert in a gap.
        {
            assert_matches!(
                linked_chunk.insert_items_at(Position(ChunkIdentifier(1), 0), ['u', 'v'],),
                Err(Error::ChunkIsAGap { identifier: ChunkIdentifier(1) })
            );
        }

        Ok(())
    }

    #[test]
    fn test_remove_item_at() -> Result<(), Error> {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        // Ignore initial update.
        let _ = linked_chunk.updates().unwrap().take();

        linked_chunk.push_items_back(['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k']);
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e', 'f'] ['g', 'h', 'i'] ['j', 'k']);
        assert_eq!(linked_chunk.num_items(), 11);

        // Ignore previous updates.
        let _ = linked_chunk.updates().unwrap().take();

        // Remove the last item of the middle chunk, 3 times. The chunk is empty after
        // that. The chunk is removed.
        {
            let position_of_f = linked_chunk.item_position(|item| *item == 'f').unwrap();
            let removed_item = linked_chunk.remove_item_at(position_of_f)?;

            assert_eq!(removed_item, 'f');
            assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d', 'e'] ['g', 'h', 'i'] ['j', 'k']);
            assert_eq!(linked_chunk.num_items(), 10);

            let position_of_e = linked_chunk.item_position(|item| *item == 'e').unwrap();
            let removed_item = linked_chunk.remove_item_at(position_of_e)?;

            assert_eq!(removed_item, 'e');
            assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d'] ['g', 'h', 'i'] ['j', 'k']);
            assert_eq!(linked_chunk.num_items(), 9);

            let position_of_d = linked_chunk.item_position(|item| *item == 'd').unwrap();
            let removed_item = linked_chunk.remove_item_at(position_of_d)?;

            assert_eq!(removed_item, 'd');
            assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['g', 'h', 'i'] ['j', 'k']);
            assert_eq!(linked_chunk.num_items(), 8);

            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[
                    RemoveItem { at: Position(ChunkIdentifier(1), 2) },
                    RemoveItem { at: Position(ChunkIdentifier(1), 1) },
                    RemoveItem { at: Position(ChunkIdentifier(1), 0) },
                    RemoveChunk(ChunkIdentifier(1)),
                ]
            );
        }

        // Remove the first item of the first chunk, 3 times. The chunk is empty after
        // that. The chunk is NOT removed because it's the first chunk.
        {
            let first_position = linked_chunk.item_position(|item| *item == 'a').unwrap();
            let removed_item = linked_chunk.remove_item_at(first_position)?;

            assert_eq!(removed_item, 'a');
            assert_items_eq!(linked_chunk, ['b', 'c'] ['g', 'h', 'i'] ['j', 'k']);
            assert_eq!(linked_chunk.num_items(), 7);

            let removed_item = linked_chunk.remove_item_at(first_position)?;

            assert_eq!(removed_item, 'b');
            assert_items_eq!(linked_chunk, ['c'] ['g', 'h', 'i'] ['j', 'k']);
            assert_eq!(linked_chunk.num_items(), 6);

            let removed_item = linked_chunk.remove_item_at(first_position)?;

            assert_eq!(removed_item, 'c');
            assert_items_eq!(linked_chunk, [] ['g', 'h', 'i'] ['j', 'k']);
            assert_eq!(linked_chunk.num_items(), 5);

            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[
                    RemoveItem { at: Position(ChunkIdentifier(0), 0) },
                    RemoveItem { at: Position(ChunkIdentifier(0), 0) },
                    RemoveItem { at: Position(ChunkIdentifier(0), 0) },
                ]
            );
        }

        // Remove the first item of the middle chunk, 3 times. The chunk is empty after
        // that. The chunk is removed.
        {
            let first_position = linked_chunk.item_position(|item| *item == 'g').unwrap();
            let removed_item = linked_chunk.remove_item_at(first_position)?;

            assert_eq!(removed_item, 'g');
            assert_items_eq!(linked_chunk, [] ['h', 'i'] ['j', 'k']);
            assert_eq!(linked_chunk.num_items(), 4);

            let removed_item = linked_chunk.remove_item_at(first_position)?;

            assert_eq!(removed_item, 'h');
            assert_items_eq!(linked_chunk, [] ['i'] ['j', 'k']);
            assert_eq!(linked_chunk.num_items(), 3);

            let removed_item = linked_chunk.remove_item_at(first_position)?;

            assert_eq!(removed_item, 'i');
            assert_items_eq!(linked_chunk, [] ['j', 'k']);
            assert_eq!(linked_chunk.num_items(), 2);

            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[
                    RemoveItem { at: Position(ChunkIdentifier(2), 0) },
                    RemoveItem { at: Position(ChunkIdentifier(2), 0) },
                    RemoveItem { at: Position(ChunkIdentifier(2), 0) },
                    RemoveChunk(ChunkIdentifier(2)),
                ]
            );
        }

        // Remove the last item of the last chunk, twice. The chunk is empty after that.
        // The chunk is removed.
        {
            let position_of_k = linked_chunk.item_position(|item| *item == 'k').unwrap();
            let removed_item = linked_chunk.remove_item_at(position_of_k)?;

            assert_eq!(removed_item, 'k');
            #[rustfmt::skip]
            assert_items_eq!(linked_chunk, [] ['j']);
            assert_eq!(linked_chunk.num_items(), 1);

            let position_of_j = linked_chunk.item_position(|item| *item == 'j').unwrap();
            let removed_item = linked_chunk.remove_item_at(position_of_j)?;

            assert_eq!(removed_item, 'j');
            assert_items_eq!(linked_chunk, []);
            assert_eq!(linked_chunk.num_items(), 0);

            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[
                    RemoveItem { at: Position(ChunkIdentifier(3), 1) },
                    RemoveItem { at: Position(ChunkIdentifier(3), 0) },
                    RemoveChunk(ChunkIdentifier(3)),
                ]
            );
        }

        // Add a couple more items, delete one, add a gap, and delete more items.
        {
            linked_chunk.push_items_back(['a', 'b', 'c', 'd']);

            #[rustfmt::skip]
            assert_items_eq!(linked_chunk, ['a', 'b', 'c'] ['d']);
            assert_eq!(linked_chunk.num_items(), 4);

            // Delete at a limit position (right after `c`), that is invalid.
            assert_matches!(
                linked_chunk.remove_item_at(Position(ChunkIdentifier(0), 3)),
                Err(Error::InvalidItemIndex { index: 3 })
            );

            // Delete at an out-of-bound position (way after `c`), that is invalid.
            assert_matches!(
                linked_chunk.remove_item_at(Position(ChunkIdentifier(0), 42)),
                Err(Error::InvalidItemIndex { index: 42 })
            );

            let position_of_c = linked_chunk.item_position(|item| *item == 'c').unwrap();
            linked_chunk.insert_gap_at((), position_of_c)?;

            assert_items_eq!(linked_chunk, ['a', 'b'] [-] ['c'] ['d']);
            assert_eq!(linked_chunk.num_items(), 4);

            // Ignore updates.
            let _ = linked_chunk.updates().unwrap().take();

            let position_of_c = linked_chunk.item_position(|item| *item == 'c').unwrap();
            let removed_item = linked_chunk.remove_item_at(position_of_c)?;

            assert_eq!(removed_item, 'c');
            assert_items_eq!(linked_chunk, ['a', 'b'] [-] ['d']);
            assert_eq!(linked_chunk.num_items(), 3);

            let position_of_d = linked_chunk.item_position(|item| *item == 'd').unwrap();
            let removed_item = linked_chunk.remove_item_at(position_of_d)?;

            assert_eq!(removed_item, 'd');
            assert_items_eq!(linked_chunk, ['a', 'b'] [-]);
            assert_eq!(linked_chunk.num_items(), 2);

            let first_position = linked_chunk.item_position(|item| *item == 'a').unwrap();
            let removed_item = linked_chunk.remove_item_at(first_position)?;

            assert_eq!(removed_item, 'a');
            assert_items_eq!(linked_chunk, ['b'] [-]);
            assert_eq!(linked_chunk.num_items(), 1);

            let removed_item = linked_chunk.remove_item_at(first_position)?;

            assert_eq!(removed_item, 'b');
            assert_items_eq!(linked_chunk, [] [-]);
            assert_eq!(linked_chunk.num_items(), 0);

            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[
                    RemoveItem { at: Position(ChunkIdentifier(6), 0) },
                    RemoveChunk(ChunkIdentifier(6)),
                    RemoveItem { at: Position(ChunkIdentifier(4), 0) },
                    RemoveChunk(ChunkIdentifier(4)),
                    RemoveItem { at: Position(ChunkIdentifier(0), 0) },
                    RemoveItem { at: Position(ChunkIdentifier(0), 0) },
                ]
            );
        }

        Ok(())
    }

    #[test]
    fn test_insert_gap_at() -> Result<(), Error> {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        // Ignore initial update.
        let _ = linked_chunk.updates().unwrap().take();

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

        // Insert at the beginning of a chunk. The targeted chunk is the first chunk.
        // `Ends::first` and `Ends::last` may be updated differently.
        {
            let position_of_a = linked_chunk.item_position(|item| *item == 'a').unwrap();
            linked_chunk.insert_gap_at((), position_of_a)?;

            // A new empty chunk is NOT created, i.e. `['a']` is not split into `[]` +
            // `['a']` because it's a waste of space.
            assert_items_eq!(linked_chunk, [-] ['a'] [-] ['b', 'c'] ['d', 'e', 'f']);
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[NewGapChunk {
                    previous: None,
                    new: ChunkIdentifier(4),
                    next: Some(ChunkIdentifier(0)),
                    gap: (),
                },]
            );
        }

        // Insert at the beginning of a chunk. The targeted chunk is not the first
        // chunk. `Ends::first` and `Ends::last` may be updated differently.
        {
            let position_of_d = linked_chunk.item_position(|item| *item == 'd').unwrap();
            linked_chunk.insert_gap_at((), position_of_d)?;

            // A new empty chunk is NOT created, i.e. `['d', 'e', 'f']` is not
            // split into `[]` + `['d', 'e', 'f']` because it's a waste of
            // space.
            assert_items_eq!(linked_chunk, [-] ['a'] [-] ['b', 'c'] [-] ['d', 'e', 'f']);
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[NewGapChunk {
                    previous: Some(ChunkIdentifier(3)),
                    new: ChunkIdentifier(5),
                    next: Some(ChunkIdentifier(1)),
                    gap: (),
                }]
            );
        }

        // Insert in an empty chunk.
        {
            // Replace a gap by empty items.
            let gap_identifier = linked_chunk.chunk_identifier(Chunk::is_gap).unwrap();
            let position = linked_chunk.replace_gap_at([], gap_identifier)?.first_position();

            assert_items_eq!(linked_chunk, [-] ['a'] [-] ['b', 'c'] [] ['d', 'e', 'f']);

            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(5)),
                        new: ChunkIdentifier(6),
                        next: Some(ChunkIdentifier(1)),
                    },
                    RemoveChunk(ChunkIdentifier(5)),
                ]
            );

            linked_chunk.insert_gap_at((), position)?;

            assert_items_eq!(linked_chunk, [-] ['a'] [-] ['b', 'c'] [-] [] ['d', 'e', 'f']);
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[NewGapChunk {
                    previous: Some(ChunkIdentifier(3)),
                    new: ChunkIdentifier(7),
                    next: Some(ChunkIdentifier(6)),
                    gap: (),
                }]
            );
        }

        // Insert in a chunk that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(Position(ChunkIdentifier(128), 0), ['u', 'v'],),
                Err(Error::InvalidChunkIdentifier { identifier: ChunkIdentifier(128) })
            );
            assert!(linked_chunk.updates().unwrap().take().is_empty());
        }

        // Insert in a chunk that exists, but at an item that does not exist.
        {
            assert_matches!(
                linked_chunk.insert_items_at(Position(ChunkIdentifier(0), 128), ['u', 'v'],),
                Err(Error::InvalidItemIndex { index: 128 })
            );
            assert!(linked_chunk.updates().unwrap().take().is_empty());
        }

        // Insert in an existing gap.
        {
            // It is impossible to get the item position inside a gap. It's only possible if
            // the item position is crafted by hand or is outdated.
            let position_of_a_gap = Position(ChunkIdentifier(2), 0);
            assert_matches!(
                linked_chunk.insert_gap_at((), position_of_a_gap),
                Err(Error::ChunkIsAGap { identifier: ChunkIdentifier(2) })
            );
            assert!(linked_chunk.updates().unwrap().take().is_empty());
        }

        assert_eq!(linked_chunk.num_items(), 6);

        Ok(())
    }

    #[test]
    fn test_replace_gap_at_middle() -> Result<(), Error> {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        // Ignore initial update.
        let _ = linked_chunk.updates().unwrap().take();

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
        let gap_identifier = linked_chunk.chunk_identifier(Chunk::is_gap).unwrap();
        assert_eq!(gap_identifier, ChunkIdentifier(1));

        let new_chunk = linked_chunk.replace_gap_at(['d', 'e', 'f', 'g', 'h'], gap_identifier)?;
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

        assert_eq!(linked_chunk.num_items(), 9);

        Ok(())
    }

    #[test]
    fn test_replace_gap_at_end() -> Result<(), Error> {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        // Ignore initial update.
        let _ = linked_chunk.updates().unwrap().take();

        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        assert_items_eq!(linked_chunk, ['a', 'b'] [-]);
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
            ]
        );

        // Replace a gap at the end of the linked chunk.
        let gap_identifier = linked_chunk.chunk_identifier(Chunk::is_gap).unwrap();
        assert_eq!(gap_identifier, ChunkIdentifier(1));

        let new_chunk = linked_chunk.replace_gap_at(['w', 'x', 'y', 'z'], gap_identifier)?;
        assert_eq!(new_chunk.identifier(), ChunkIdentifier(2));
        assert_items_eq!(
            linked_chunk,
            ['a', 'b'] ['w', 'x', 'y'] ['z']
        );
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(1)),
                    new: ChunkIdentifier(2),
                    next: None,
                },
                PushItems { at: Position(ChunkIdentifier(2), 0), items: vec!['w', 'x', 'y'] },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(2)),
                    new: ChunkIdentifier(3),
                    next: None,
                },
                PushItems { at: Position(ChunkIdentifier(3), 0), items: vec!['z'] },
                RemoveChunk(ChunkIdentifier(1)),
            ]
        );

        assert_eq!(linked_chunk.num_items(), 6);

        Ok(())
    }

    #[test]
    fn test_replace_gap_at_beginning() -> Result<(), Error> {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        // Ignore initial update.
        let _ = linked_chunk.updates().unwrap().take();

        linked_chunk.push_items_back(['a', 'b']);
        assert_items_eq!(linked_chunk, ['a', 'b']);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a', 'b'] },]
        );

        // Replace a gap at the beginning of the linked chunk.
        let position_of_a = linked_chunk.item_position(|item| *item == 'a').unwrap();
        linked_chunk.insert_gap_at((), position_of_a).unwrap();
        assert_items_eq!(
            linked_chunk,
            [-] ['a', 'b']
        );
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[NewGapChunk {
                previous: None,
                new: ChunkIdentifier(1),
                next: Some(ChunkIdentifier(0)),
                gap: (),
            }]
        );

        let gap_identifier = linked_chunk.chunk_identifier(Chunk::is_gap).unwrap();
        assert_eq!(gap_identifier, ChunkIdentifier(1));

        let new_chunk = linked_chunk.replace_gap_at(['x'], gap_identifier)?;
        assert_eq!(new_chunk.identifier(), ChunkIdentifier(2));
        assert_items_eq!(
            linked_chunk,
            ['x'] ['a', 'b']
        );
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(1)),
                    new: ChunkIdentifier(2),
                    next: Some(ChunkIdentifier(0)),
                },
                PushItems { at: Position(ChunkIdentifier(2), 0), items: vec!['x'] },
                RemoveChunk(ChunkIdentifier(1)),
            ]
        );

        assert_eq!(linked_chunk.num_items(), 3);

        Ok(())
    }

    #[test]
    fn test_remove_empty_chunk_at() -> Result<(), Error> {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        // Ignore initial update.
        let _ = linked_chunk.updates().unwrap().take();

        linked_chunk.insert_gap_at((), Position(ChunkIdentifier(0), 0)).unwrap();
        linked_chunk.push_items_back(['a', 'b']);
        linked_chunk.push_gap_back(());
        linked_chunk.push_items_back(['l', 'm']);
        linked_chunk.push_gap_back(());
        assert_items_eq!(linked_chunk, [-] ['a', 'b'] [-] ['l', 'm'] [-]);
        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                NewGapChunk {
                    previous: None,
                    new: ChunkIdentifier(1),
                    next: Some(ChunkIdentifier(0)),
                    gap: (),
                },
                PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a', 'b'] },
                NewGapChunk {
                    previous: Some(ChunkIdentifier(0)),
                    new: ChunkIdentifier(2),
                    next: None,
                    gap: (),
                },
                NewItemsChunk {
                    previous: Some(ChunkIdentifier(2)),
                    new: ChunkIdentifier(3),
                    next: None,
                },
                PushItems { at: Position(ChunkIdentifier(3), 0), items: vec!['l', 'm'] },
                NewGapChunk {
                    previous: Some(ChunkIdentifier(3)),
                    new: ChunkIdentifier(4),
                    next: None,
                    gap: (),
                },
            ]
        );

        // Try to remove a chunk that's not empty.
        let err = linked_chunk.remove_empty_chunk_at(ChunkIdentifier(0)).unwrap_err();
        assert_matches!(err, Error::RemovingNonEmptyItemsChunk { .. });

        // Try to remove an unknown gap chunk.
        let err = linked_chunk.remove_empty_chunk_at(ChunkIdentifier(42)).unwrap_err();
        assert_matches!(err, Error::InvalidChunkIdentifier { .. });

        // Remove the gap in the middle.
        let maybe_next = linked_chunk.remove_empty_chunk_at(ChunkIdentifier(2)).unwrap();
        let next = maybe_next.unwrap();
        // The next insert position at the start of the next chunk.
        assert_eq!(next.chunk_identifier(), ChunkIdentifier(3));
        assert_eq!(next.index(), 0);
        assert_items_eq!(linked_chunk, [-] ['a', 'b'] ['l', 'm'] [-]);
        assert_eq!(linked_chunk.updates().unwrap().take(), &[RemoveChunk(ChunkIdentifier(2))]);

        // Remove the gap at the end.
        let next = linked_chunk.remove_empty_chunk_at(ChunkIdentifier(4)).unwrap();
        // It was the last chunk, so there's no next insert position.
        assert!(next.is_none());
        assert_items_eq!(linked_chunk, [-] ['a', 'b'] ['l', 'm']);
        assert_eq!(linked_chunk.updates().unwrap().take(), &[RemoveChunk(ChunkIdentifier(4))]);

        // Remove the gap at the beginning.
        let maybe_next = linked_chunk.remove_empty_chunk_at(ChunkIdentifier(1)).unwrap();
        let next = maybe_next.unwrap();
        assert_eq!(next.chunk_identifier(), ChunkIdentifier(0));
        assert_eq!(next.index(), 0);
        assert_items_eq!(linked_chunk, ['a', 'b'] ['l', 'm']);
        assert_eq!(linked_chunk.updates().unwrap().take(), &[RemoveChunk(ChunkIdentifier(1))]);

        Ok(())
    }

    #[test]
    fn test_remove_empty_last_chunk() {
        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        // Ignore initial update.
        let _ = linked_chunk.updates().unwrap().take();

        assert_items_eq!(linked_chunk, []);
        assert!(linked_chunk.updates().unwrap().take().is_empty());

        // Try to remove the first chunk.
        let err = linked_chunk.remove_empty_chunk_at(ChunkIdentifier(0)).unwrap_err();
        assert_matches!(err, Error::RemovingLastChunk);
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

    #[test]
    fn test_is_first_and_last_chunk() {
        let mut linked_chunk = LinkedChunk::<3, char, ()>::new();

        let mut chunks = linked_chunk.chunks().peekable();
        assert!(chunks.peek().unwrap().is_first_chunk());
        assert!(chunks.next().unwrap().is_last_chunk());
        assert!(chunks.next().is_none());

        linked_chunk.push_items_back(['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h']);

        let mut chunks = linked_chunk.chunks().peekable();
        assert!(chunks.next().unwrap().is_first_chunk());
        assert!(chunks.peek().unwrap().is_first_chunk().not());
        assert!(chunks.next().unwrap().is_last_chunk().not());
        assert!(chunks.next().unwrap().is_last_chunk());
        assert!(chunks.next().is_none());
    }

    // Test `LinkedChunk::clear`. This test creates a `LinkedChunk` with `new` to
    // avoid creating too much confusion with `Update`s. The next test
    // `test_clear_emit_an_update_clear` uses `new_with_update_history` and only
    // test `Update::Clear`.
    #[test]
    fn test_clear() {
        let mut linked_chunk = LinkedChunk::<3, Arc<char>, Arc<()>>::new();

        let item = Arc::new('a');
        let gap = Arc::new(());

        linked_chunk.push_items_back([
            item.clone(),
            item.clone(),
            item.clone(),
            item.clone(),
            item.clone(),
        ]);
        linked_chunk.push_gap_back(gap.clone());
        linked_chunk.push_items_back([item.clone()]);

        assert_eq!(Arc::strong_count(&item), 7);
        assert_eq!(Arc::strong_count(&gap), 2);
        assert_eq!(linked_chunk.num_items(), 6);
        assert_eq!(linked_chunk.chunk_identifier_generator.next.load(Ordering::SeqCst), 3);

        // Now, we can clear the linked chunk and see what happens.
        linked_chunk.clear();

        assert_eq!(Arc::strong_count(&item), 1);
        assert_eq!(Arc::strong_count(&gap), 1);
        assert_eq!(linked_chunk.num_items(), 0);
        assert_eq!(linked_chunk.chunk_identifier_generator.next.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_clear_emit_an_update_clear() {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[NewItemsChunk {
                previous: None,
                new: ChunkIdentifierGenerator::FIRST_IDENTIFIER,
                next: None
            }]
        );

        linked_chunk.clear();

        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[
                Clear,
                NewItemsChunk {
                    previous: None,
                    new: ChunkIdentifierGenerator::FIRST_IDENTIFIER,
                    next: None
                }
            ]
        );
    }

    #[test]
    fn test_replace_item() {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        linked_chunk.push_items_back(['a', 'b', 'c']);
        linked_chunk.push_gap_back(());
        // Sanity check.
        assert_items_eq!(linked_chunk, ['a', 'b', 'c'] [-]);

        // Drain previous updates.
        let _ = linked_chunk.updates().unwrap().take();

        // Replace item in bounds.
        linked_chunk.replace_item_at(Position(ChunkIdentifier(0), 1), 'B').unwrap();
        assert_items_eq!(linked_chunk, ['a', 'B', 'c'] [-]);

        assert_eq!(
            linked_chunk.updates().unwrap().take(),
            &[ReplaceItem { at: Position(ChunkIdentifier(0), 1), item: 'B' }]
        );

        // Attempt to replace out-of-bounds.
        assert_matches!(
            linked_chunk.replace_item_at(Position(ChunkIdentifier(0), 3), 'Z'),
            Err(Error::InvalidItemIndex { index: 3 })
        );

        // Attempt to replace gap.
        assert_matches!(
            linked_chunk.replace_item_at(Position(ChunkIdentifier(1), 0), 'Z'),
            Err(Error::ChunkIsAGap { .. })
        );
    }

    #[test]
    fn test_lazy_previous() {
        use std::marker::PhantomData;

        use super::{Ends, ObservableUpdates, Update::*};

        // Imagine the linked chunk is lazily loaded.
        let first_chunk_identifier = ChunkIdentifier(0);
        let mut first_loaded_chunk = Chunk::new_items_leaked(ChunkIdentifier(1));
        unsafe { first_loaded_chunk.as_mut() }.lazy_previous = Some(first_chunk_identifier);

        let mut linked_chunk = LinkedChunk::<3, char, ()> {
            links: Ends { first: first_loaded_chunk, last: None },
            chunk_identifier_generator:
                ChunkIdentifierGenerator::new_from_previous_chunk_identifier(ChunkIdentifier(1)),
            updates: Some(ObservableUpdates::new()),
            marker: PhantomData,
        };

        // Insert items in the first loaded chunk (chunk 1), with an overflow to a new
        // chunk.
        {
            linked_chunk.push_items_back(['a', 'b', 'c', 'd']);

            assert_items_eq!(linked_chunk, ['a', 'b', 'c']['d']);

            // Assert where `lazy_previous` is set.
            {
                let mut chunks = linked_chunk.chunks();

                assert_matches!(chunks.next(), Some(chunk) => {
                    assert_eq!(chunk.identifier(), 1);
                    assert_eq!(chunk.lazy_previous, Some(ChunkIdentifier(0)));
                });
                assert_matches!(chunks.next(), Some(chunk) => {
                    assert_eq!(chunk.identifier(), 2);
                    assert!(chunk.lazy_previous.is_none());
                });
                assert!(chunks.next().is_none());
            }

            // In the updates, we observe nothing else than the usual bits.
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[
                    PushItems { at: Position(ChunkIdentifier(1), 0), items: vec!['a', 'b', 'c'] },
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(1)),
                        new: ChunkIdentifier(2),
                        next: None,
                    },
                    PushItems { at: Position(ChunkIdentifier(2), 0), items: vec!['d'] }
                ]
            );
        }

        // Now insert a gap at the head of the loaded linked chunk.
        {
            linked_chunk.insert_gap_at((), Position(ChunkIdentifier(1), 0)).unwrap();

            assert_items_eq!(linked_chunk, [-] ['a', 'b', 'c'] ['d']);

            // Assert where `lazy_previous` is set.
            {
                let mut chunks = linked_chunk.chunks();

                assert_matches!(chunks.next(), Some(chunk) => {
                    assert_eq!(chunk.identifier(), 3);
                    // `lazy_previous` has moved here!
                    assert_eq!(chunk.lazy_previous, Some(ChunkIdentifier(0)));
                });
                assert_matches!(chunks.next(), Some(chunk) => {
                    assert_eq!(chunk.identifier(), 1);
                    // `lazy_previous` has moved from here.
                    assert!(chunk.lazy_previous.is_none());
                });
                assert_matches!(chunks.next(), Some(chunk) => {
                    assert_eq!(chunk.identifier(), 2);
                    assert!(chunk.lazy_previous.is_none());
                });
                assert!(chunks.next().is_none());
            }

            // In the updates, we observe that the new gap **has** a previous chunk!
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[NewGapChunk {
                    // 0 is the lazy, not-loaded-yet chunk.
                    previous: Some(ChunkIdentifier(0)),
                    new: ChunkIdentifier(3),
                    next: Some(ChunkIdentifier(1)),
                    gap: ()
                }]
            );
        }

        // Next, replace the gap by items to see how it reacts to unlink.
        {
            linked_chunk.replace_gap_at(['w', 'x', 'y', 'z'], ChunkIdentifier(3)).unwrap();

            assert_items_eq!(linked_chunk, ['w', 'x', 'y'] ['z'] ['a', 'b', 'c'] ['d']);

            // Assert where `lazy_previous` is set.
            {
                let mut chunks = linked_chunk.chunks();

                assert_matches!(chunks.next(), Some(chunk) => {
                    assert_eq!(chunk.identifier(), 4);
                    // `lazy_previous` has moved here!
                    assert_eq!(chunk.lazy_previous, Some(ChunkIdentifier(0)));
                });
                assert_matches!(chunks.next(), Some(chunk) => {
                    assert_eq!(chunk.identifier(), 5);
                    assert!(chunk.lazy_previous.is_none());
                });
                assert_matches!(chunks.next(), Some(chunk) => {
                    assert_eq!(chunk.identifier(), 1);
                    assert!(chunk.lazy_previous.is_none());
                });
                assert_matches!(chunks.next(), Some(chunk) => {
                    assert_eq!(chunk.identifier(), 2);
                    assert!(chunk.lazy_previous.is_none());
                });
                assert!(chunks.next().is_none());
            }

            // In the updates, we observe nothing than the usual bits.
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[
                    // The new chunk is inserted…
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(3)),
                        new: ChunkIdentifier(4),
                        next: Some(ChunkIdentifier(1)),
                    },
                    // … and new items are pushed in it.
                    PushItems { at: Position(ChunkIdentifier(4), 0), items: vec!['w', 'x', 'y'] },
                    // Another new chunk is inserted…
                    NewItemsChunk {
                        previous: Some(ChunkIdentifier(4)),
                        new: ChunkIdentifier(5),
                        next: Some(ChunkIdentifier(1)),
                    },
                    // … and new items are pushed in it.
                    PushItems { at: Position(ChunkIdentifier(5), 0), items: vec!['z'] },
                    // Finally, the gap is removed!
                    RemoveChunk(ChunkIdentifier(3)),
                ]
            );
        }

        // Finally, let's re-insert a gap to ensure the lazy-previous is set
        // correctly. It is similar to the beginning of this test, but this is a
        // frequent pattern in how the linked chunk is used.
        {
            linked_chunk.insert_gap_at((), Position(ChunkIdentifier(4), 0)).unwrap();

            assert_items_eq!(linked_chunk, [-] ['w', 'x', 'y'] ['z'] ['a', 'b', 'c'] ['d']);

            // Assert where `lazy_previous` is set.
            {
                let mut chunks = linked_chunk.chunks();

                assert_matches!(chunks.next(), Some(chunk) => {
                    assert_eq!(chunk.identifier(), 6);
                    // `lazy_previous` has moved here!
                    assert_eq!(chunk.lazy_previous, Some(ChunkIdentifier(0)));
                });
                assert_matches!(chunks.next(), Some(chunk) => {
                    assert_eq!(chunk.identifier(), 4);
                    // `lazy_previous` has moved from here.
                    assert!(chunk.lazy_previous.is_none());
                });
                assert_matches!(chunks.next(), Some(chunk) => {
                    assert_eq!(chunk.identifier(), 5);
                    assert!(chunk.lazy_previous.is_none());
                });
                assert_matches!(chunks.next(), Some(chunk) => {
                    assert_eq!(chunk.identifier(), 1);
                    assert!(chunk.lazy_previous.is_none());
                });
                assert_matches!(chunks.next(), Some(chunk) => {
                    assert_eq!(chunk.identifier(), 2);
                    assert!(chunk.lazy_previous.is_none());
                });
                assert!(chunks.next().is_none());
            }

            // In the updates, we observe that the new gap **has** a previous chunk!
            assert_eq!(
                linked_chunk.updates().unwrap().take(),
                &[NewGapChunk {
                    // 0 is the lazy, not-loaded-yet chunk.
                    previous: Some(ChunkIdentifier(0)),
                    new: ChunkIdentifier(6),
                    next: Some(ChunkIdentifier(4)),
                    gap: ()
                }]
            );
        }
    }
}
