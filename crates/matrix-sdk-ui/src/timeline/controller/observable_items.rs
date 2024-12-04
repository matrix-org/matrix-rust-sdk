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
    cmp::Ordering,
    collections::{vec_deque::Iter, VecDeque},
    ops::Deref,
    sync::Arc,
};

use eyeball_im::{
    ObservableVector, ObservableVectorEntries, ObservableVectorEntry, ObservableVectorTransaction,
    ObservableVectorTransactionEntry, VectorSubscriber,
};
use imbl::Vector;
use ruma::EventId;

use super::{state::EventMeta, TimelineItem};

/// An `ObservableItems` is a type similar to
/// [`ObservableVector<Arc<TimelineItem>>`] except the API is limited and,
/// internally, maintains the mapping between remote events and timeline items.
#[derive(Debug)]
pub struct ObservableItems {
    /// All timeline items.
    ///
    /// Yeah, there are here!
    items: ObservableVector<Arc<TimelineItem>>,

    /// List of all the remote events as received in the timeline, even the ones
    /// that are discarded in the timeline items.
    ///
    /// This is useful to get this for the moment as it helps the `Timeline` to
    /// compute read receipts and read markers. It also helps to map event to
    /// timeline item, see [`EventMeta::timeline_item_index`] to learn more.
    all_remote_events: AllRemoteEvents,
}

impl ObservableItems {
    /// Create an empty `ObservableItems`.
    pub fn new() -> Self {
        Self {
            // Upstream default capacity is currently 16, which is making
            // sliding-sync tests with 20 events lag. This should still be
            // small enough.
            items: ObservableVector::with_capacity(32),
            all_remote_events: AllRemoteEvents::default(),
        }
    }

    /// Get a reference to all remote events.
    pub fn all_remote_events(&self) -> &AllRemoteEvents {
        &self.all_remote_events
    }

    /// Check whether there is timeline items.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Subscribe to timeline item updates.
    pub fn subscribe(&self) -> VectorSubscriber<Arc<TimelineItem>> {
        self.items.subscribe()
    }

    /// Get a clone of all timeline items.
    ///
    /// Note that it doesn't clone `Self`, only the inner timeline items.
    pub fn clone(&self) -> Vector<Arc<TimelineItem>> {
        self.items.clone()
    }

    /// Start a new transaction to make multiple updates as one unit.
    pub fn transaction(&mut self) -> ObservableItemsTransaction<'_> {
        ObservableItemsTransaction {
            items: self.items.transaction(),
            all_remote_events: &mut self.all_remote_events,
        }
    }

    /// Replace the timeline item at position `timeline_item_index` by
    /// `timeline_item`.
    ///
    /// # Panics
    ///
    /// Panics if `timeline_item_index > total_number_of_timeline_items`.
    pub fn replace(
        &mut self,
        timeline_item_index: usize,
        timeline_item: Arc<TimelineItem>,
    ) -> Arc<TimelineItem> {
        self.items.set(timeline_item_index, timeline_item)
    }

    /// Get an iterator over all the entries in this `ObservableItems`.
    pub fn entries(&mut self) -> ObservableItemsEntries<'_> {
        ObservableItemsEntries(self.items.entries())
    }

    /// Call the given closure for every element in this `ObservableItems`,
    /// with an entry struct that allows updating that element.
    pub fn for_each<F>(&mut self, mut f: F)
    where
        F: FnMut(ObservableItemsEntry<'_>),
    {
        self.items.for_each(|entry| f(ObservableItemsEntry(entry)))
    }
}

// It's fine to deref to an immutable reference to `Vector`.
impl Deref for ObservableItems {
    type Target = Vector<Arc<TimelineItem>>;

    fn deref(&self) -> &Self::Target {
        &self.items
    }
}

/// An “iterator“ that yields entries into an `ObservableItems`.
pub struct ObservableItemsEntries<'a>(ObservableVectorEntries<'a, Arc<TimelineItem>>);

impl ObservableItemsEntries<'_> {
    /// Advance this iterator, yielding an `ObservableItemsEntry` for the next
    /// item in the timeline, or `None` if all items have been visited.
    pub fn next(&mut self) -> Option<ObservableItemsEntry<'_>> {
        self.0.next().map(ObservableItemsEntry)
    }
}

/// A handle to a single timeline item in an `ObservableItems`.
pub struct ObservableItemsEntry<'a>(ObservableVectorEntry<'a, Arc<TimelineItem>>);

impl ObservableItemsEntry<'_> {
    /// Replace the timeline item by `timeline_item`.
    pub fn replace(this: &mut Self, timeline_item: Arc<TimelineItem>) -> Arc<TimelineItem> {
        ObservableVectorEntry::set(&mut this.0, timeline_item)
    }
}

impl Deref for ObservableItemsEntry<'_> {
    type Target = Arc<TimelineItem>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A transaction that allows making multiple updates to an `ObservableItems` as
/// an atomic unit.
///
/// For updates from the transaction to have affect, it has to be finalized with
/// [`Self::commit`]. If the transaction is dropped without that method being
/// called, the updates will be discarded.
#[derive(Debug)]
pub struct ObservableItemsTransaction<'observable_items> {
    items: ObservableVectorTransaction<'observable_items, Arc<TimelineItem>>,
    all_remote_events: &'observable_items mut AllRemoteEvents,
}

impl<'observable_items> ObservableItemsTransaction<'observable_items> {
    /// Get a reference to the timeline index at position `timeline_item_index`.
    pub fn get(&self, timeline_item_index: usize) -> Option<&Arc<TimelineItem>> {
        self.items.get(timeline_item_index)
    }

    /// Get a reference to all remote events.
    pub fn all_remote_events(&self) -> &AllRemoteEvents {
        self.all_remote_events
    }

    /// Remove a remote event at position `event_index`.
    ///
    /// Not to be confused with removing a timeline item!
    pub fn remove_remote_event(&mut self, event_index: usize) -> Option<EventMeta> {
        self.all_remote_events.remove(event_index)
    }

    /// Push a new remote event at the front of all remote events.
    ///
    /// Not to be confused with pushing front a timeline item!
    pub fn push_front_remote_event(&mut self, event_meta: EventMeta) {
        self.all_remote_events.push_front(event_meta);
    }

    /// Push a new remote event at the back of all remote events.
    ///
    /// Not to be confused with pushing back a timeline item!
    pub fn push_back_remote_event(&mut self, event_meta: EventMeta) {
        self.all_remote_events.push_back(event_meta);
    }

    /// Get a remote event by using an event ID.
    pub fn get_remote_event_by_event_id_mut(
        &mut self,
        event_id: &EventId,
    ) -> Option<&mut EventMeta> {
        self.all_remote_events.get_by_event_id_mut(event_id)
    }

    /// Replace a timeline item at position `timeline_item_index` by
    /// `timeline_item`.
    pub fn replace(
        &mut self,
        timeline_item_index: usize,
        timeline_item: Arc<TimelineItem>,
    ) -> Arc<TimelineItem> {
        self.items.set(timeline_item_index, timeline_item)
    }

    /// Remove a timeline item at position `timeline_item_index`.
    pub fn remove(&mut self, timeline_item_index: usize) -> Arc<TimelineItem> {
        let removed_timeline_item = self.items.remove(timeline_item_index);
        self.all_remote_events.timeline_item_has_been_removed_at(timeline_item_index);

        removed_timeline_item
    }

    /// Insert a new `timeline_item` at position `timeline_item_index`, with an
    /// optionally associated `event_index`.
    ///
    /// If `event_index` is `Some(_)`, it means `timeline_item_index` has an
    /// associated remote event (at position `event_index`) that maps to it.
    /// Otherwise, if it is `None`, it means there is no remote event associated
    /// to it; that's the case for virtual timeline item for example. See
    /// [`EventMeta::timeline_item_index`] to learn more.
    pub fn insert(
        &mut self,
        timeline_item_index: usize,
        timeline_item: Arc<TimelineItem>,
        event_index: Option<usize>,
    ) {
        self.items.insert(timeline_item_index, timeline_item);
        self.all_remote_events.timeline_item_has_been_inserted_at(timeline_item_index, event_index);
    }

    /// Push a new `timeline_item` at position 0, with an optionally associated
    /// `event_index`.
    ///
    /// If `event_index` is `Some(_)`, it means `timeline_item_index` has an
    /// associated remote event (at position `event_index`) that maps to it.
    /// Otherwise, if it is `None`, it means there is no remote event associated
    /// to it; that's the case for virtual timeline item for example. See
    /// [`EventMeta::timeline_item_index`] to learn more.
    pub fn push_front(&mut self, timeline_item: Arc<TimelineItem>, event_index: Option<usize>) {
        self.items.push_front(timeline_item);
        self.all_remote_events.timeline_item_has_been_inserted_at(0, event_index);
    }

    /// Push a new `timeline_item` at position `len() - 1`, with an optionally
    /// associated `event_index`.
    ///
    /// If `event_index` is `Some(_)`, it means `timeline_item_index` has an
    /// associated remote event (at position `event_index`) that maps to it.
    /// Otherwise, if it is `None`, it means there is no remote event associated
    /// to it; that's the case for virtual timeline item for example. See
    /// [`EventMeta::timeline_item_index`] to learn more.
    pub fn push_back(&mut self, timeline_item: Arc<TimelineItem>, event_index: Option<usize>) {
        self.items.push_back(timeline_item);
        self.all_remote_events
            .timeline_item_has_been_inserted_at(self.items.len().saturating_sub(1), event_index);
    }

    /// Clear all timeline items and all remote events.
    pub fn clear(&mut self) {
        self.items.clear();
        self.all_remote_events.clear();
    }

    /// Call the given closure for every element in this `ObservableItems`,
    /// with an entry struct that allows updating that element.
    pub fn for_each<F>(&mut self, mut f: F)
    where
        F: FnMut(ObservableItemsTransactionEntry<'_, 'observable_items>),
    {
        self.items.for_each(|entry| f(ObservableItemsTransactionEntry(entry)))
    }

    /// Commit this transaction, persisting the changes and notifying
    /// subscribers.
    pub fn commit(self) {
        self.items.commit()
    }
}

// It's fine to deref to an immutable reference to `Vector`.
impl Deref for ObservableItemsTransaction<'_> {
    type Target = Vector<Arc<TimelineItem>>;

    fn deref(&self) -> &Self::Target {
        &self.items
    }
}

/// A handle to a single timeline item in an `ObservableItemsTransaction`.
pub struct ObservableItemsTransactionEntry<'a, 'observable_items>(
    ObservableVectorTransactionEntry<'a, 'observable_items, Arc<TimelineItem>>,
);

impl ObservableItemsTransactionEntry<'_, '_> {
    /// Replace the timeline item by `timeline_item`.
    pub fn replace(this: &mut Self, timeline_item: Arc<TimelineItem>) -> Arc<TimelineItem> {
        ObservableVectorTransactionEntry::set(&mut this.0, timeline_item)
    }

    /// Remove this timeline item.
    ///
    /// # Safety
    ///
    /// This method doesn't update `AllRemoteEvents`. Be sure that the caller
    /// doesn't break the mapping between remote events and timeline items. See
    /// [`EventMeta::timeline_item_index`] to learn more.
    pub unsafe fn remove(this: Self) {
        ObservableVectorTransactionEntry::remove(this.0);
    }
}

impl Deref for ObservableItemsTransactionEntry<'_, '_> {
    type Target = Arc<TimelineItem>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A type for all remote events.
///
/// Having this type helps to know exactly which parts of the code and how they
/// use all remote events. It also helps to give a bit of semantics on top of
/// them.
#[derive(Clone, Debug, Default)]
pub struct AllRemoteEvents(VecDeque<EventMeta>);

impl AllRemoteEvents {
    /// Return a front-to-back iterator over all remote events.
    pub fn iter(&self) -> Iter<'_, EventMeta> {
        self.0.iter()
    }

    /// Remove all remote events.
    fn clear(&mut self) {
        self.0.clear();
    }

    /// Insert a new remote event at the front of all the others.
    fn push_front(&mut self, event_meta: EventMeta) {
        // If there is an associated `timeline_item_index`, shift all the
        // `timeline_item_index` that come after this one.
        if let Some(new_timeline_item_index) = event_meta.timeline_item_index {
            self.increment_all_timeline_item_index_after(new_timeline_item_index);
        }

        // Push the event.
        self.0.push_front(event_meta)
    }

    /// Insert a new remote event at the back of all the others.
    fn push_back(&mut self, event_meta: EventMeta) {
        // If there is an associated `timeline_item_index`, shift all the
        // `timeline_item_index` that come after this one.
        if let Some(new_timeline_item_index) = event_meta.timeline_item_index {
            self.increment_all_timeline_item_index_after(new_timeline_item_index);
        }

        // Push the event.
        self.0.push_back(event_meta)
    }

    /// Remove one remote event at a specific index, and return it if it exists.
    fn remove(&mut self, event_index: usize) -> Option<EventMeta> {
        // Remove the event.
        let event_meta = self.0.remove(event_index)?;

        // If there is an associated `timeline_item_index`, shift all the
        // `timeline_item_index` that come after this one.
        if let Some(removed_timeline_item_index) = event_meta.timeline_item_index {
            self.decrement_all_timeline_item_index_after(removed_timeline_item_index);
        };

        Some(event_meta)
    }

    /// Return a reference to the last remote event if it exists.
    pub fn last(&self) -> Option<&EventMeta> {
        self.0.back()
    }

    /// Return the index of the last remote event if it exists.
    pub fn last_index(&self) -> Option<usize> {
        self.0.len().checked_sub(1)
    }

    /// Get a mutable reference to a specific remote event by its ID.
    pub fn get_by_event_id_mut(&mut self, event_id: &EventId) -> Option<&mut EventMeta> {
        self.0.iter_mut().rev().find(|event_meta| event_meta.event_id == event_id)
    }

    /// Shift to the right all timeline item indexes that are equal to or
    /// greater than `new_timeline_item_index`.
    fn increment_all_timeline_item_index_after(&mut self, new_timeline_item_index: usize) {
        for event_meta in self.0.iter_mut() {
            if let Some(timeline_item_index) = event_meta.timeline_item_index.as_mut() {
                if *timeline_item_index >= new_timeline_item_index {
                    *timeline_item_index += 1;
                }
            }
        }
    }

    /// Shift to the left all timeline item indexes that are greater than
    /// `removed_wtimeline_item_index`.
    fn decrement_all_timeline_item_index_after(&mut self, removed_timeline_item_index: usize) {
        for event_meta in self.0.iter_mut() {
            if let Some(timeline_item_index) = event_meta.timeline_item_index.as_mut() {
                if *timeline_item_index > removed_timeline_item_index {
                    *timeline_item_index -= 1;
                }
            }
        }
    }

    /// Notify that a timeline item has been inserted at
    /// `new_timeline_item_index`. If `event_index` is `Some(_)`, it means the
    /// remote event at `event_index` must be mapped to
    /// `new_timeline_item_index`.
    fn timeline_item_has_been_inserted_at(
        &mut self,
        new_timeline_item_index: usize,
        event_index: Option<usize>,
    ) {
        self.increment_all_timeline_item_index_after(new_timeline_item_index);

        if let Some(event_index) = event_index {
            if let Some(event_meta) = self.0.get_mut(event_index) {
                event_meta.timeline_item_index = Some(new_timeline_item_index);
            }
        }
    }

    /// Notify that a timeline item has been removed at
    /// `new_timeline_item_index`. If `event_index` is `Some(_)`, it means the
    /// remote event at `event_index` must be unmapped.
    fn timeline_item_has_been_removed_at(&mut self, timeline_item_index_to_remove: usize) {
        for event_meta in self.0.iter_mut() {
            let mut remove_timeline_item_index = false;

            // A `timeline_item_index` is removed. Let's shift all indexes that come
            // after the removed one.
            if let Some(timeline_item_index) = event_meta.timeline_item_index.as_mut() {
                match (*timeline_item_index).cmp(&timeline_item_index_to_remove) {
                    Ordering::Equal => {
                        remove_timeline_item_index = true;
                    }

                    Ordering::Greater => {
                        *timeline_item_index -= 1;
                    }

                    Ordering::Less => {}
                }
            }

            // This is the `event_meta` that holds the `timeline_item_index` that is being
            // removed. So let's clean it.
            if remove_timeline_item_index {
                event_meta.timeline_item_index = None;
            }
        }
    }
}
