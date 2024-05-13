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
    collections::HashMap,
    pin::Pin,
    sync::{Arc, RwLock, Weak},
    task::{Context, Poll, Waker},
};

use futures_core::Stream;

use super::{ChunkIdentifier, Position};

/// Represent the updates that have happened inside a [`LinkedChunk`].
///
/// To retrieve the updates, use [`LinkedChunk::updates`].
///
/// These updates are useful to store a `LinkedChunk` in another form of
/// storage, like a database or something similar.
#[derive(Debug, PartialEq)]
pub enum Update<Item, Gap> {
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

impl<Item, Gap> Clone for Update<Item, Gap>
where
    Item: Clone,
    Gap: Clone,
{
    fn clone(&self) -> Self {
        match self {
            Self::NewItemsChunk { previous, new, next } => {
                Self::NewItemsChunk { previous: *previous, new: *new, next: *next }
            }
            Self::NewGapChunk { previous, new, next, gap } => {
                Self::NewGapChunk { previous: *previous, new: *new, next: *next, gap: gap.clone() }
            }
            Self::RemoveChunk(identifier) => Self::RemoveChunk(*identifier),
            Self::InsertItems { at, items } => Self::InsertItems { at: *at, items: items.clone() },
            Self::TruncateItems { chunk, length } => {
                Self::TruncateItems { chunk: *chunk, length: *length }
            }
        }
    }
}

/// A collection of [`Update`].
///
/// Get a value for this type with [`LinkedChunk::updates`].
pub struct Updates<Item, Gap> {
    inner: Arc<RwLock<UpdatesInner<Item, Gap>>>,
}

/// A token used to represent readers that read the updates in
/// [`UpdatesInner`].
type ReaderToken = usize;

/// Inner type for [`Updates`].
///
/// The particularity of this type is that multiple readers can read the
/// updates. A reader has a [`ReaderToken`]. The public API (i.e.
/// [`Updates`]) is considered to be the _main reader_ (it has the token
/// [`Self::MAIN_READER_TOKEN`]).
///
/// An update that have been read by all readers are garbage collected to be
/// removed from the memory. An update will never be read twice by the same
/// reader.
///
/// Why do we need multiple readers? The public API reads the updates with
/// [`Updates::take`], but the private API must also read the updates for
/// example with [`UpdatesSubscriber`]. Of course, they can be multiple
/// `UpdatesSubscriber`s at the same time. Hence the need of supporting multiple
/// readers.
struct UpdatesInner<Item, Gap> {
    /// All the updates that have not been read by all readers.
    updates: Vec<Update<Item, Gap>>,

    /// Updates are stored in [`Self::updates`]. Multiple readers can read them.
    /// A reader is identified by a [`ReaderToken`].
    ///
    /// To each reader token is associated an index that represents the index of
    /// the last reading. It is used to never return the same update twice.
    last_index_per_reader: HashMap<ReaderToken, usize>,

    /// The last generated token. This is useful to generate new token.
    last_token: ReaderToken,

    /// Pending wakers for [`UpdateSubscriber`]s. A waker is removed
    /// everytime it is called.
    wakers: Vec<Waker>,
}

impl<Item, Gap> UpdatesInner<Item, Gap> {
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
    fn push(&mut self, update: Update<Item, Gap>) {
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
    fn take(&mut self) -> &[Update<Item, Gap>] {
        self.take_with_token(Self::MAIN_READER_TOKEN)
    }

    /// Take new updates with a particular reader token.
    ///
    /// Updates are stored in [`Self::updates`]. Multiple readers can read them.
    /// A reader is identified by a [`ReaderToken`]. Every reader can
    /// take/read/consume each update only once. An internal index is stored
    /// per reader token to know where to start reading updates next time this
    /// method is called.
    fn take_with_token(&mut self, token: ReaderToken) -> &[Update<Item, Gap>] {
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
        let min_index = self.last_index_per_reader.values().min().copied().unwrap_or(0);

        if min_index > 0 {
            let _ = self.updates.drain(0..min_index);

            // Let's shift the indices to the left by `min_index` to preserve them.
            for index in self.last_index_per_reader.values_mut() {
                *index -= min_index;
            }
        }
    }
}

impl<Item, Gap> Updates<Item, Gap> {
    /// Create a new [`Self`].
    pub(super) fn new() -> Self {
        Self { inner: Arc::new(RwLock::new(UpdatesInner::new())) }
    }

    /// Push a new update.
    pub(super) fn push(&mut self, update: Update<Item, Gap>) {
        self.inner.write().unwrap().push(update);
    }

    /// Take new updates.
    ///
    /// Updates that have been taken will not be read again.
    pub(super) fn take(&mut self) -> Vec<Update<Item, Gap>>
    where
        Item: Clone,
        Gap: Clone,
    {
        self.inner.write().unwrap().take().to_owned()
    }

    /// Subscribe to updates by using a [`Stream`].
    fn subscribe(&mut self) -> UpdatesSubscriber<Item, Gap> {
        // A subscriber is a new update reader, it needs its own token.
        let token = {
            let mut inner = self.inner.write().unwrap();
            inner.last_token += 1;

            let last_token = inner.last_token;
            inner.last_index_per_reader.insert(last_token, 0);

            last_token
        };

        UpdatesSubscriber { updates: Arc::downgrade(&self.inner), token }
    }
}

/// A subscriber to [`Updates`]. It is helpful to receive updates via a
/// [`Stream`].
struct UpdatesSubscriber<Item, Gap> {
    /// Weak reference to [`UpdatesInner`].
    ///
    /// Using a weak reference allows [`Updates`] to be dropped
    /// freely even if a subscriber exists.
    updates: Weak<RwLock<UpdatesInner<Item, Gap>>>,

    /// The token to read the updates.
    token: ReaderToken,
}

impl<Item, Gap> Stream for UpdatesSubscriber<Item, Gap>
where
    Item: Clone,
    Gap: Clone,
{
    type Item = Vec<Update<Item, Gap>>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(updates) = self.updates.upgrade() else {
            // The `Updates` has been dropped. It's time to close this stream.
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

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        task::{Context, Poll, Wake},
    };

    use assert_matches::assert_matches;
    use futures_util::pin_mut;

    use super::{super::LinkedChunk, ChunkIdentifier, Position, Stream, UpdatesInner};

    #[test]
    fn test_updates_take_and_garbage_collector() {
        use super::Update::*;

        let mut linked_chunk = LinkedChunk::<10, char, ()>::new_with_update_history();

        // Simulate another updates “reader”, it can a subscriber.
        let main_token = UpdatesInner::<char, ()>::MAIN_READER_TOKEN;
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
        use super::Update::*;

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
}
