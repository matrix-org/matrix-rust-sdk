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
    sync::{Arc, RwLock},
    task::Waker,
};

use super::{ChunkIdentifier, Position};

/// Represent the updates that have happened inside a [`LinkedChunk`].
///
/// To retrieve the updates, use [`LinkedChunk::updates`].
///
/// These updates are useful to store a `LinkedChunk` in another form of
/// storage, like a database or something similar.
///
/// [`LinkedChunk`]: super::LinkedChunk
/// [`LinkedChunk::updates`]: super::LinkedChunk::updates
#[derive(Debug, Clone, PartialEq)]
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

    /// Items are pushed inside a chunk of kind Items.
    PushItems {
        /// The [`Position`] of the items.
        ///
        /// This value is given to prevent the need for position computations by
        /// the update readers. Items are pushed, so the positions should be
        /// incrementally computed from the previous items, which requires the
        /// reading of the last previous item. With `at`, the update readers no
        /// longer need to do so.
        at: Position,

        /// The items.
        items: Vec<Item>,
    },

    /// An item has been replaced in the linked chunk.
    ///
    /// The `at` position MUST resolve to the actual position an existing *item*
    /// (not a gap).
    ReplaceItem {
        /// The position of the item that's being replaced.
        at: Position,

        /// The new value for the item.
        item: Item,
    },

    /// An item has been removed inside a chunk of kind Items.
    RemoveItem {
        /// The [`Position`] of the item.
        at: Position,
    },

    /// The last items of a chunk have been detached, i.e. the chunk has been
    /// truncated.
    DetachLastItems {
        /// The split position. Before this position (`..position`), items are
        /// kept, from this position (`position..`), items are
        /// detached.
        at: Position,
    },

    /// Detached items (see [`Self::DetachLastItems`]) starts being reattached.
    StartReattachItems,

    /// Reattaching items (see [`Self::StartReattachItems`]) is finished.
    EndReattachItems,

    /// All chunks have been cleared, i.e. all items and all gaps have been
    /// dropped.
    Clear,
}

impl<Item, Gap> Update<Item, Gap> {
    /// Get the items from the [`Update`] if any.
    ///
    /// This function is useful if you only care about the items from the
    /// [`Update`] and not what kind of update it was and where the items
    /// should be placed.
    ///
    /// [`Update`] variants which don't contain any items will return an empty
    /// [`Vec`].
    pub fn into_items(self) -> Vec<Item> {
        match self {
            Update::NewItemsChunk { .. }
            | Update::NewGapChunk { .. }
            | Update::RemoveChunk(_)
            | Update::RemoveItem { .. }
            | Update::DetachLastItems { .. }
            | Update::StartReattachItems
            | Update::EndReattachItems
            | Update::Clear => vec![],
            Update::PushItems { items, .. } => items,
            Update::ReplaceItem { item, .. } => vec![item],
        }
    }
}

/// A collection of [`Update`]s that can be observed.
///
/// Get a value for this type with [`LinkedChunk::updates`].
///
/// [`LinkedChunk::updates`]: super::LinkedChunk::updates
#[derive(Debug)]
pub struct ObservableUpdates<Item, Gap> {
    pub(super) inner: Arc<RwLock<UpdatesInner<Item, Gap>>>,
}

impl<Item, Gap> ObservableUpdates<Item, Gap> {
    /// Create a new [`ObservableUpdates`].
    pub(super) fn new() -> Self {
        Self { inner: Arc::new(RwLock::new(UpdatesInner::new())) }
    }

    /// Push a new update.
    pub(super) fn push(&mut self, update: Update<Item, Gap>) {
        self.inner.write().unwrap().push(update);
    }

    /// Clear all pending updates.
    pub(super) fn clear_pending(&mut self) {
        self.inner.write().unwrap().clear_pending();
    }

    /// Take new updates.
    ///
    /// Updates that have been taken will not be read again.
    pub fn take(&mut self) -> Vec<Update<Item, Gap>>
    where
        Item: Clone,
        Gap: Clone,
    {
        self.inner.write().unwrap().take().to_owned()
    }

    /// Subscribe to updates by using a [`Stream`].
    #[cfg(test)]
    pub(super) fn subscribe(&mut self) -> UpdatesSubscriber<Item, Gap> {
        // A subscriber is a new update reader, it needs its own token.
        let token = self.new_reader_token();

        UpdatesSubscriber::new(Arc::downgrade(&self.inner), token)
    }

    /// Generate a new [`ReaderToken`].
    pub(super) fn new_reader_token(&mut self) -> ReaderToken {
        let mut inner = self.inner.write().unwrap();

        // Add 1 before reading the `last_token`, in this particular order, because the
        // 0 token is reserved by `MAIN_READER_TOKEN`.
        inner.last_token += 1;
        let last_token = inner.last_token;

        inner.last_index_per_reader.insert(last_token, 0);

        last_token
    }
}

/// A token used to represent readers that read the updates in
/// [`UpdatesInner`].
pub(super) type ReaderToken = usize;

/// Inner type for [`ObservableUpdates`].
///
/// The particularity of this type is that multiple readers can read the
/// updates. A reader has a [`ReaderToken`]. The public API (i.e.
/// [`ObservableUpdates`]) is considered to be the _main reader_ (it has the
/// token [`Self::MAIN_READER_TOKEN`]).
///
/// An update that have been read by all readers are garbage collected to be
/// removed from the memory. An update will never be read twice by the same
/// reader.
///
/// Why do we need multiple readers? The public API reads the updates with
/// [`ObservableUpdates::take`], but the private API must also read the updates
/// for example with [`UpdatesSubscriber`]. Of course, they can be multiple
/// `UpdatesSubscriber`s at the same time. Hence the need of supporting multiple
/// readers.
#[derive(Debug)]
pub(super) struct UpdatesInner<Item, Gap> {
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

    /// Clear all pending updates.
    fn clear_pending(&mut self) {
        self.updates.clear();

        // Reset all the per-reader indices.
        for idx in self.last_index_per_reader.values_mut() {
            *idx = 0;
        }

        // No need to wake the wakers; they're waiting for a new update, and we
        // just made them all disappear.
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
    pub(super) fn take_with_token(&mut self, token: ReaderToken) -> &[Update<Item, Gap>] {
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

    /// Has the given reader, identified by its [`ReaderToken`], some pending
    /// updates, or has it consumed all the pending updates?
    pub(super) fn is_reader_up_to_date(&self, token: ReaderToken) -> bool {
        *self.last_index_per_reader.get(&token).expect("unknown reader token") == self.updates.len()
    }

    /// Return the number of updates in the buffer.
    #[cfg(test)]
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

/// A subscriber to [`ObservableUpdates`]. It is helpful to receive updates via
/// a [`Stream`].
#[cfg(test)]
pub(super) struct UpdatesSubscriber<Item, Gap> {
    /// Weak reference to [`UpdatesInner`].
    ///
    /// Using a weak reference allows [`ObservableUpdates`] to be dropped
    /// freely even if a subscriber exists.
    updates: std::sync::Weak<RwLock<UpdatesInner<Item, Gap>>>,

    /// The token to read the updates.
    token: ReaderToken,
}

#[cfg(test)]
impl<Item, Gap> UpdatesSubscriber<Item, Gap> {
    /// Create a new [`Self`].
    #[cfg(test)]
    fn new(updates: std::sync::Weak<RwLock<UpdatesInner<Item, Gap>>>, token: ReaderToken) -> Self {
        Self { updates, token }
    }
}

#[cfg(test)]
impl<Item, Gap> futures_core::Stream for UpdatesSubscriber<Item, Gap>
where
    Item: Clone,
    Gap: Clone,
{
    type Item = Vec<Update<Item, Gap>>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let Some(updates) = self.updates.upgrade() else {
            // The `ObservableUpdates` has been dropped. It's time to close this stream.
            return std::task::Poll::Ready(None);
        };

        let mut updates = updates.write().unwrap();
        let the_updates = updates.take_with_token(self.token);

        // No updates.
        if the_updates.is_empty() {
            // Let's register the waker.
            updates.wakers.push(context.waker().clone());

            // The stream is pending.
            return std::task::Poll::Pending;
        }

        // There is updates! Let's forward them in this stream.
        std::task::Poll::Ready(Some(the_updates.to_owned()))
    }
}

#[cfg(test)]
impl<Item, Gap> Drop for UpdatesSubscriber<Item, Gap> {
    fn drop(&mut self) {
        // Remove `Self::token` from `UpdatesInner::last_index_per_reader`.
        // This is important so that the garbage collector can do its jobs correctly
        // without a dead dangling reader token.
        if let Some(updates) = self.updates.upgrade() {
            let mut updates = updates.write().unwrap();

            // Remove the reader token from `UpdatesInner`.
            // It's safe to ignore the result of `remove` here: `None` means the token was
            // already removed (note: it should be unreachable).
            let _ = updates.last_index_per_reader.remove(&self.token);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        task::{Context, Poll, Wake},
    };

    use assert_matches::assert_matches;
    use futures_core::Stream;
    use futures_util::pin_mut;

    use super::{super::LinkedChunk, ChunkIdentifier, Position, UpdatesInner};
    use crate::linked_chunk::Update;

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

        // There is an initial update.
        {
            let updates = linked_chunk.updates().unwrap();

            assert_eq!(
                updates.take(),
                &[NewItemsChunk { previous: None, new: ChunkIdentifier(0), next: None }],
            );
            assert_eq!(
                updates.inner.write().unwrap().take_with_token(other_token),
                &[NewItemsChunk { previous: None, new: ChunkIdentifier(0), next: None }],
            );
        }

        // No new update.
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
                    PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a'] },
                    PushItems { at: Position(ChunkIdentifier(0), 1), items: vec!['b'] },
                    PushItems { at: Position(ChunkIdentifier(0), 2), items: vec!['c'] },
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
                    PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a'] },
                    PushItems { at: Position(ChunkIdentifier(0), 1), items: vec!['b'] },
                    PushItems { at: Position(ChunkIdentifier(0), 2), items: vec!['c'] },
                    PushItems { at: Position(ChunkIdentifier(0), 3), items: vec!['d'] },
                    PushItems { at: Position(ChunkIdentifier(0), 4), items: vec!['e'] },
                    PushItems { at: Position(ChunkIdentifier(0), 5), items: vec!['f'] },
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
        // “main” will have its index updated from 0 to 3.
        // “other” will have its index updated from 6 to 3.
        {
            let updates = linked_chunk.updates().unwrap();

            assert_eq!(
                updates.take(),
                &[
                    PushItems { at: Position(ChunkIdentifier(0), 3), items: vec!['d'] },
                    PushItems { at: Position(ChunkIdentifier(0), 4), items: vec!['e'] },
                    PushItems { at: Position(ChunkIdentifier(0), 5), items: vec!['f'] },
                    PushItems { at: Position(ChunkIdentifier(0), 6), items: vec!['g'] },
                    PushItems { at: Position(ChunkIdentifier(0), 7), items: vec!['h'] },
                    PushItems { at: Position(ChunkIdentifier(0), 8), items: vec!['i'] },
                ]
            );
            assert_eq!(
                updates.inner.write().unwrap().take_with_token(other_token),
                &[
                    PushItems { at: Position(ChunkIdentifier(0), 6), items: vec!['g'] },
                    PushItems { at: Position(ChunkIdentifier(0), 7), items: vec!['h'] },
                    PushItems { at: Position(ChunkIdentifier(0), 8), items: vec!['i'] },
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

    struct CounterWaker {
        number_of_wakeup: Mutex<usize>,
    }

    impl Wake for CounterWaker {
        fn wake(self: Arc<Self>) {
            *self.number_of_wakeup.lock().unwrap() += 1;
        }
    }

    #[test]
    fn test_updates_stream() {
        use super::Update::*;

        let counter_waker = Arc::new(CounterWaker { number_of_wakeup: Mutex::new(0) });
        let waker = counter_waker.clone().into();
        let mut context = Context::from_waker(&waker);

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        let updates_subscriber = linked_chunk.updates().unwrap().subscribe();
        pin_mut!(updates_subscriber);

        // Initial update, stream is ready.
        assert_matches!(
            updates_subscriber.as_mut().poll_next(&mut context),
            Poll::Ready(Some(items)) => {
                assert_eq!(
                    items,
                    &[NewItemsChunk { previous: None, new: ChunkIdentifier(0), next: None }]
                );
            }
        );
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
                    &[PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a'] }]
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
                NewItemsChunk { previous: None, new: ChunkIdentifier(0), next: None },
                PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a'] },
                PushItems { at: Position(ChunkIdentifier(0), 1), items: vec!['b'] },
                PushItems { at: Position(ChunkIdentifier(0), 2), items: vec!['c'] },
            ]
        );
        assert_matches!(
            updates_subscriber.as_mut().poll_next(&mut context),
            Poll::Ready(Some(items)) => {
                assert_eq!(
                    items,
                    &[
                        PushItems { at: Position(ChunkIdentifier(0), 1), items: vec!['b'] },
                        PushItems { at: Position(ChunkIdentifier(0), 2), items: vec!['c'] },
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
    fn test_updates_multiple_streams() {
        use super::Update::*;

        let counter_waker1 = Arc::new(CounterWaker { number_of_wakeup: Mutex::new(0) });
        let counter_waker2 = Arc::new(CounterWaker { number_of_wakeup: Mutex::new(0) });

        let waker1 = counter_waker1.clone().into();
        let waker2 = counter_waker2.clone().into();

        let mut context1 = Context::from_waker(&waker1);
        let mut context2 = Context::from_waker(&waker2);

        let mut linked_chunk = LinkedChunk::<3, char, ()>::new_with_update_history();

        let updates_subscriber1 = linked_chunk.updates().unwrap().subscribe();
        pin_mut!(updates_subscriber1);

        // Scope for `updates_subscriber2`.
        let updates_subscriber2_token = {
            let updates_subscriber2 = linked_chunk.updates().unwrap().subscribe();
            pin_mut!(updates_subscriber2);

            // Initial updates, streams are ready.
            assert_matches!(
                updates_subscriber1.as_mut().poll_next(&mut context1),
                Poll::Ready(Some(items)) => {
                    assert_eq!(
                        items,
                        &[NewItemsChunk { previous: None, new: ChunkIdentifier(0), next: None }]
                    );
                }
            );
            assert_matches!(updates_subscriber1.as_mut().poll_next(&mut context1), Poll::Pending);
            assert_eq!(*counter_waker1.number_of_wakeup.lock().unwrap(), 0);

            assert_matches!(
                updates_subscriber2.as_mut().poll_next(&mut context2),
                Poll::Ready(Some(items)) => {
                    assert_eq!(
                        items,
                        &[NewItemsChunk { previous: None, new: ChunkIdentifier(0), next: None }]
                    );
                }
            );
            assert_matches!(updates_subscriber2.as_mut().poll_next(&mut context2), Poll::Pending);
            assert_eq!(*counter_waker2.number_of_wakeup.lock().unwrap(), 0);

            // Let's generate an update.
            linked_chunk.push_items_back(['a']);

            // The wakers must have been called.
            assert_eq!(*counter_waker1.number_of_wakeup.lock().unwrap(), 1);
            assert_eq!(*counter_waker2.number_of_wakeup.lock().unwrap(), 1);

            // There is an update! Right after that, the streams are pending again.
            assert_matches!(
                updates_subscriber1.as_mut().poll_next(&mut context1),
                Poll::Ready(Some(items)) => {
                    assert_eq!(
                        items,
                        &[PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a'] }]
                    );
                }
            );
            assert_matches!(updates_subscriber1.as_mut().poll_next(&mut context1), Poll::Pending);
            assert_matches!(
                updates_subscriber2.as_mut().poll_next(&mut context2),
                Poll::Ready(Some(items)) => {
                    assert_eq!(
                        items,
                        &[PushItems { at: Position(ChunkIdentifier(0), 0), items: vec!['a'] }]
                    );
                }
            );
            assert_matches!(updates_subscriber2.as_mut().poll_next(&mut context2), Poll::Pending);

            // Let's generate two other updates.
            linked_chunk.push_items_back(['b']);
            linked_chunk.push_items_back(['c']);

            // A waker is consumed when called. The first call to `push_items_back` will
            // call and consume the wakers. The second call to `push_items_back` will do
            // nothing as the wakers have been consumed. New wakers will be registered on
            // polling.
            //
            // So, the waker must have been called only once for the two updates.
            assert_eq!(*counter_waker1.number_of_wakeup.lock().unwrap(), 2);
            assert_eq!(*counter_waker2.number_of_wakeup.lock().unwrap(), 2);

            // Let's poll `updates_subscriber1` only.
            assert_matches!(
                updates_subscriber1.as_mut().poll_next(&mut context1),
                Poll::Ready(Some(items)) => {
                    assert_eq!(
                        items,
                        &[
                            PushItems { at: Position(ChunkIdentifier(0), 1), items: vec!['b'] },
                            PushItems { at: Position(ChunkIdentifier(0), 2), items: vec!['c'] },
                        ]
                    );
                }
            );
            assert_matches!(updates_subscriber1.as_mut().poll_next(&mut context1), Poll::Pending);

            // For the sake of this test, we also need to advance the main reader token.
            let _ = linked_chunk.updates().unwrap().take();
            let _ = linked_chunk.updates().unwrap().take();

            // If we inspect the garbage collector state, `a`, `b` and `c` should still be
            // present because not all of them have been consumed by `updates_subscriber2`
            // yet.
            {
                let updates = linked_chunk.updates().unwrap();

                let inner = updates.inner.read().unwrap();

                // Inspect number of updates in memory.
                // We get 2 because the garbage collector runs before data are taken, not after:
                // `updates_subscriber2` has read `a` only, so `b` and `c` remain.
                assert_eq!(inner.len(), 2);

                // Inspect the indices.
                let indices = &inner.last_index_per_reader;

                assert_eq!(indices.get(&updates_subscriber1.token), Some(&2));
                assert_eq!(indices.get(&updates_subscriber2.token), Some(&0));
            }

            // Poll `updates_subscriber1` again: there is no new update so it must be
            // pending.
            assert_matches!(updates_subscriber1.as_mut().poll_next(&mut context1), Poll::Pending);

            // The state of the garbage collector is unchanged: `a`, `b` and `c` are still
            // in memory.
            {
                let updates = linked_chunk.updates().unwrap();

                let inner = updates.inner.read().unwrap();

                // Inspect number of updates in memory. Value is unchanged.
                assert_eq!(inner.len(), 2);

                // Inspect the indices. They are unchanged.
                let indices = &inner.last_index_per_reader;

                assert_eq!(indices.get(&updates_subscriber1.token), Some(&2));
                assert_eq!(indices.get(&updates_subscriber2.token), Some(&0));
            }

            updates_subscriber2.token
            // Drop `updates_subscriber2`!
        };

        // `updates_subscriber2` has been dropped. Poll `updates_subscriber1` again:
        // still no new update, but it will run the garbage collector again, and this
        // time `updates_subscriber2` is not “retaining” `b` and `c`. The garbage
        // collector must be empty.
        assert_matches!(updates_subscriber1.as_mut().poll_next(&mut context1), Poll::Pending);

        // Inspect the garbage collector.
        {
            let updates = linked_chunk.updates().unwrap();

            let inner = updates.inner.read().unwrap();

            // Inspect number of updates in memory.
            assert_eq!(inner.len(), 0);

            // Inspect the indices.
            let indices = &inner.last_index_per_reader;

            assert_eq!(indices.get(&updates_subscriber1.token), Some(&0));
            assert_eq!(indices.get(&updates_subscriber2_token), None); // token is unknown!
        }

        // When dropping the `LinkedChunk`, it closes the stream.
        drop(linked_chunk);
        assert_matches!(updates_subscriber1.as_mut().poll_next(&mut context1), Poll::Ready(None));
    }

    #[test]
    fn test_update_into_items() {
        let updates: Update<_, u32> =
            Update::PushItems { at: Position::new(ChunkIdentifier(0), 0), items: vec![1, 2, 3] };

        assert_eq!(updates.into_items(), vec![1, 2, 3]);

        let updates: Update<u32, u32> = Update::Clear;
        assert!(updates.into_items().is_empty());

        let updates: Update<u32, u32> =
            Update::RemoveItem { at: Position::new(ChunkIdentifier(0), 0) };
        assert!(updates.into_items().is_empty());

        let updates: Update<u32, u32> =
            Update::ReplaceItem { at: Position::new(ChunkIdentifier(0), 0), item: 42 };
        assert_eq!(updates.into_items(), vec![42]);
    }
}
