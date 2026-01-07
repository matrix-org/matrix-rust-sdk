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
    collections::{VecDeque, vec_deque::Iter},
    iter::{Enumerate, Skip, Take},
    ops::{Deref, RangeBounds},
    sync::Arc,
};

use bitflags::bitflags;
use eyeball_im::{
    ObservableVector, ObservableVectorEntries, ObservableVectorEntry, ObservableVectorTransaction,
    ObservableVectorTransactionEntry, VectorSubscriber,
};
use imbl::Vector;
use ruma::EventId;

use super::{TimelineItem, metadata::EventMeta};

/// An `ObservableItems` is a type similar to
/// [`ObservableVector<Arc<TimelineItem>>`] except the API is limited and,
/// internally, maintains the mapping between remote events and timeline items.
///
/// # Regions
///
/// The `ObservableItems` holds all the invariants about the _position_ of the
/// items. It defines three regions where items can live:
///
/// 1. the _start_ region, which can only contain a single [`TimelineStart`],
/// 2. the _remotes_ region, which can only contain many [`Remote`] timeline
///    items with their decorations (only [`DateDivider`]s and [`ReadMarker`]s),
/// 3. the _locals_ region, which can only contain many [`Local`] timeline items
///    with their decorations (only [`DateDivider`]s).
///
/// The [`iter_all_regions`] method allows to iterate over all regions.
/// [`iter_remotes_region`] will restrict the iterator over the _remotes_
/// region, and so on. These iterators provide the absolute indices of the
/// items, so that it's harder to make mistakes when manipulating the indices of
/// items with operations like [`insert`], [`remove`], [`replace`] etc.
///
/// Other methods like [`push_local`] or [`push_date_divider`] insert the items
/// in the correct region, and check a couple of invariants.
///
/// [`TimelineStart`]: super::VirtualTimelineItem::TimelineStart
/// [`DateDivider`]: super::VirtualTimelineItem::DateDivider
/// [`ReadMarker`]: super::VirtualTimelineItem::ReadMarker
/// [`Remote`]: super::EventTimelineItemKind::Remote
/// [`Local`]: super::EventTimelineItemKind::Local
/// [`iter_all_regions`]: ObservableItemsTransaction::iter_all_regions
/// [`iter_remote_region`]: ObservableItemsTransaction::iter_remotes_region
/// [`insert`]: ObservableItemsTransaction::insert
/// [`remove`]: ObservableItemsTransaction::remove
/// [`replace`]: ObservableItemsTransaction::replace
/// [`push_local`]: ObservableItemsTransaction::push_local
/// [`push_date_divider`]: ObservableItemsTransaction::push_date_divider
#[derive(Debug)]
pub struct ObservableItems {
    /// All timeline items.
    ///
    /// Yeah, there are here! This [`ObservableVector`] contains all the
    /// timeline items that are rendered in your magnificent Matrix client.
    ///
    /// These items are the _core_ of the timeline, see [`TimelineItem`] to
    /// learn more.
    items: ObservableVector<Arc<TimelineItem>>,

    /// List of all the remote events as received in the timeline, even the ones
    /// that are discarded in the timeline items.
    ///
    /// The list of all remote events is used to compute the read receipts and
    /// read markers; additionally it's used to map events to timeline items,
    /// for more info about that, take a look at the documentation for
    /// [`EventMeta::timeline_item_index`].
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
    pub fn clone_items(&self) -> Vector<Arc<TimelineItem>> {
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
//
// We don't want, however, to deref to a mutable reference: it should be done
// via proper methods to control precisely the mapping between remote events and
// timeline items.
impl Deref for ObservableItems {
    type Target = Vector<Arc<TimelineItem>>;

    fn deref(&self) -> &Self::Target {
        &self.items
    }
}

/// An iterator that yields entries into an `ObservableItems`.
///
/// It doesn't implement [`Iterator`] though because of a lifetime conflict: the
/// returned `Iterator::Item` could live longer than the `Iterator` itself.
/// Ideally, `Iterator::next` should take a `&'a mut self`, but this is not
/// possible.
pub struct ObservableItemsEntries<'a>(ObservableVectorEntries<'a, Arc<TimelineItem>>);

impl ObservableItemsEntries<'_> {
    /// Advance this iterator, yielding an `ObservableItemsEntry` for the next
    /// item in the timeline, or `None` if all items have been visited.
    pub fn next(&mut self) -> Option<ObservableItemsEntry<'_>> {
        self.0.next().map(ObservableItemsEntry)
    }
}

/// A handle to a single timeline item in an `ObservableItems`.
#[derive(Debug)]
pub struct ObservableItemsEntry<'a>(ObservableVectorEntry<'a, Arc<TimelineItem>>);

impl ObservableItemsEntry<'_> {
    /// Replace the timeline item by `timeline_item`.
    pub fn replace(this: &mut Self, timeline_item: Arc<TimelineItem>) -> Arc<TimelineItem> {
        ObservableVectorEntry::set(&mut this.0, timeline_item)
    }
}

// It's fine to deref to an immutable reference to `Arc<TimelineItem>`.
//
// We don't want, however, to deref to a mutable reference: it should be done
// via proper methods to control precisely the mapping between remote events and
// timeline items.
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
/// [`ObservableItemsTransaction::commit`]. If the transaction is dropped
/// without that method being called, the updates will be discarded.
#[derive(Debug)]
pub struct ObservableItemsTransaction<'observable_items> {
    items: ObservableVectorTransaction<'observable_items, Arc<TimelineItem>>,
    all_remote_events: &'observable_items mut AllRemoteEvents,
}

impl<'observable_items> ObservableItemsTransaction<'observable_items> {
    /// Get a reference to the timeline item at position `timeline_item_index`.
    pub fn get(&self, timeline_item_index: usize) -> Option<&Arc<TimelineItem>> {
        self.items.get(timeline_item_index)
    }

    /// Get a reference to all remote events.
    pub fn all_remote_events(&self) -> &AllRemoteEvents {
        self.all_remote_events
    }

    /// Remove a remote event at the `event_index` position.
    ///
    /// Not to be confused with removing a timeline item!
    pub fn remove_remote_event(&mut self, event_index: usize) -> Option<EventMeta> {
        self.all_remote_events.remove(event_index)
    }

    /// Push a new remote event at the front of all remote events.
    ///
    /// Not to be confused with pushing a timeline item to the front!
    pub fn push_front_remote_event(&mut self, event_meta: EventMeta) {
        self.all_remote_events.push_front(event_meta);
    }

    /// Push a new remote event at the back of all remote events.
    ///
    /// Not to be confused with pushing a timeline item to the back!
    pub fn push_back_remote_event(&mut self, event_meta: EventMeta) {
        self.all_remote_events.push_back(event_meta);
    }

    /// Insert a new remote event at a specific index.
    ///
    /// Not to be confused with inserting a timeline item!
    pub fn insert_remote_event(&mut self, event_index: usize, event_meta: EventMeta) {
        self.all_remote_events.insert(event_index, event_meta);
    }

    /// Get a remote event by using an event ID.
    pub fn get_remote_event_by_event_id_mut(
        &mut self,
        event_id: &EventId,
    ) -> Option<&mut EventMeta> {
        self.all_remote_events.get_by_event_id_mut(event_id)
    }

    /// Get a remote event by using an event ID.
    pub fn get_remote_event_by_event_id(&self, event_id: &EventId) -> Option<&EventMeta> {
        self.all_remote_events.get_by_event_id(event_id)
    }

    /// Get the position of an event in the events array by its ID.
    pub fn position_by_event_id(&self, event_id: &EventId) -> Option<usize> {
        self.all_remote_events.position_by_event_id(event_id)
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

    /// Push a new [`Local`] timeline item.
    ///
    /// # Invariant
    ///
    /// A [`Local`] is always the last item.
    ///
    /// # Panics
    ///
    /// It panics if the provided `timeline_item` is not a [`Local`].
    ///
    /// [`Local`]: super::EventTimelineItemKind::Local
    pub fn push_local(&mut self, timeline_item: Arc<TimelineItem>) {
        assert!(timeline_item.is_local_echo(), "The provided `timeline_item` is not a `Local`");

        self.push_back(timeline_item, None);
    }

    /// Push a new [`DateDivider`] virtual timeline item.
    ///
    /// # Panics
    ///
    /// It panics if the provided `timeline_item` is not a [`DateDivider`].
    ///
    /// It also panics if the `timeline_item_index` points inside the _start_
    /// region.
    ///
    /// [`DateDivider`]: super::VirtualTimelineItem::DateDivider
    /// [`TimelineStart`]: super::VirtualTimelineItem::TimelineStart
    /// [`Local`]: super::EventTimelineItemKind::Local
    pub fn push_date_divider(
        &mut self,
        timeline_item_index: usize,
        timeline_item: Arc<TimelineItem>,
    ) {
        assert!(
            timeline_item.is_date_divider(),
            "The provided `timeline_item` is not a `DateDivider`"
        );

        // We are not inserting in the start region.
        if timeline_item_index == 0 && !self.items.is_empty() {
            assert!(
                matches!(self.items.get(timeline_item_index), Some(timeline_item) if !timeline_item.is_timeline_start())
            );
        }

        if timeline_item_index == self.len() {
            self.push_back(timeline_item, None);
        } else if timeline_item_index == 0 {
            self.push_front(timeline_item, None);
        } else {
            self.insert(timeline_item_index, timeline_item, None);
        }
    }

    /// Push a new [`TimelineStart`] virtual timeline item.
    ///
    /// # Invariant
    ///
    /// A [`TimelineStart`] is always the first item if present.
    ///
    /// # Panics
    ///
    /// It panics if the provided `timeline_item` is not a [`TimelineStart`].
    ///
    /// [`TimelineStart`]: super::VirtualTimelineItem::TimelineStart
    pub fn push_timeline_start_if_missing(&mut self, timeline_item: Arc<TimelineItem>) {
        assert!(
            timeline_item.is_timeline_start(),
            "The provided `timeline_item` is not a `TimelineStart`"
        );

        // The timeline start virtual item is necessarily the first item.
        if self.get(0).is_some_and(|item| item.is_timeline_start()) {
            return;
        }

        self.push_front(timeline_item, None);
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
        self.items.for_each(|entry| {
            f(ObservableItemsTransactionEntry { entry, all_remote_events: self.all_remote_events })
        })
    }

    /// Check whether there is at least one [`Local`] timeline item.
    ///
    /// [`Local`]: super::EventTimelineItemKind::Local
    pub fn has_local(&self) -> bool {
        matches!(self.items.last(), Some(timeline_item) if timeline_item.is_local_echo())
    }

    /// Return the index where to insert the first remote timeline
    /// item.
    pub fn first_remotes_region_index(&self) -> usize {
        if self.items.get(0).is_some_and(|item| item.is_timeline_start()) { 1 } else { 0 }
    }

    /// Iterate over all timeline items in the _remotes_ region.
    pub fn iter_remotes_region(&self) -> ObservableItemsTransactionIter<'_> {
        ObservableItemsTransactionIterBuilder::new(&self.items).with_remotes().build()
    }

    /// Iterate over all timeline items in the _remotes_ and _locals_ regions.
    pub fn iter_remotes_and_locals_regions(&self) -> ObservableItemsTransactionIter<'_> {
        ObservableItemsTransactionIterBuilder::new(&self.items).with_remotes().with_locals().build()
    }

    /// Iterate over all timeline items in the _locals_ region.
    pub fn iter_locals_region(&self) -> ObservableItemsTransactionIter<'_> {
        ObservableItemsTransactionIterBuilder::new(&self.items).with_locals().build()
    }

    /// Iterate over all timeline items (in all regions).
    pub fn iter_all_regions(&self) -> ObservableItemsTransactionIter<'_> {
        ObservableItemsTransactionIterBuilder::new(&self.items)
            .with_start()
            .with_remotes()
            .with_locals()
            .build()
    }

    /// Alias to [`Self::iter_all_regions`].
    ///
    /// This type has a `Deref` implementation to `ObservableVectorTransaction`,
    /// which has its own `iter` method. This method “overrides” it to ensure it
    /// is consistent with other iterator methods of this type, by aliasing it
    /// to [`Self::iter_all_regions`].
    #[allow(unused)] // We really don't want anybody to use the `self.items.iter()` method.
    #[deprecated = "This method is now aliased to `Self::iter_all_regions`"]
    pub fn iter(&self) -> ObservableItemsTransactionIter<'_> {
        self.iter_all_regions()
    }

    /// Commit this transaction, persisting the changes and notifying
    /// subscribers.
    pub fn commit(self) {
        self.items.commit()
    }
}

bitflags! {
    struct Regions: u8 {
        /// The _start_ region can only contain a single [`TimelineStart`].
        ///
        /// [`TimelineStart`]: super::VirtualTimelineItem::TimelineStart
        const START = 0b0000_0001;

        /// The _remotes_ region can only contain many [`Remote`] timeline items
        /// with their decorations (only [`DateDivider`]s and [`ReadMarker`]s).
        ///
        /// [`DateDivider`]: super::VirtualTimelineItem::DateDivider
        /// [`ReadMarker`]: super::VirtualTimelineItem::ReadMarker
        /// [`Remote`]: super::EventTimelineItemKind::Remote
        const REMOTES = 0b0000_0010;

        /// The _locals_ region can only contain many [`Local`] timeline items
        /// with their decorations (only [`DateDivider`]s).
        ///
        /// [`DateDivider`]: super::VirtualTimelineItem::DateDivider
        /// [`Local`]: super::EventTimelineItemKind::Local
        const LOCALS = 0b0000_0100;
    }
}

/// A builder for the [`ObservableItemsTransactionIter`].
struct ObservableItemsTransactionIterBuilder<'e> {
    /// The original items.
    items: &'e ObservableVectorTransaction<'e, Arc<TimelineItem>>,

    /// The regions to cover.
    regions: Regions,
}

impl<'e> ObservableItemsTransactionIterBuilder<'e> {
    /// Build a new [`Self`].
    fn new(items: &'e ObservableVectorTransaction<'e, Arc<TimelineItem>>) -> Self {
        Self { items, regions: Regions::empty() }
    }

    /// Include the _start_ region in the iterator.
    fn with_start(mut self) -> Self {
        self.regions.insert(Regions::START);
        self
    }

    /// Include the _remotes_ region in the iterator.
    fn with_remotes(mut self) -> Self {
        self.regions.insert(Regions::REMOTES);
        self
    }

    /// Include the _locals_ region in the iterator.
    fn with_locals(mut self) -> Self {
        self.regions.insert(Regions::LOCALS);
        self
    }

    /// Build the iterator.
    #[allow(clippy::iter_skip_zero)]
    fn build(self) -> ObservableItemsTransactionIter<'e> {
        // Calculate the size of the _start_ region.
        let size_of_start_region = if matches!(
            self.items.get(0),
            Some(first_timeline_item) if first_timeline_item.is_timeline_start()
        ) {
            1
        } else {
            0
        };

        // Calculate the size of the _locals_ region.
        let size_of_locals_region = self
            .items
            .deref()
            .iter()
            .rev()
            .take_while(|timeline_item| timeline_item.is_local_echo())
            .count();

        // Calculate the size of the _remotes_ region.
        let size_of_remotes_region =
            self.items.len() - size_of_start_region - size_of_locals_region;

        let with_start = self.regions.contains(Regions::START);
        let with_remotes = self.regions.contains(Regions::REMOTES);
        let with_locals = self.regions.contains(Regions::LOCALS);

        // Compute one iterator per combination of regions.
        let iter = self.items.deref().iter().enumerate();
        let inner = match (with_start, with_remotes, with_locals) {
            // Nothing.
            (false, false, false) => iter.skip(0).take(0),

            // Only the start region.
            (true, false, false) => iter.skip(0).take(size_of_start_region),

            // Only the remotes region.
            (false, true, false) => iter.skip(size_of_start_region).take(size_of_remotes_region),

            // The start region and the remotes regions.
            (true, true, false) => iter.skip(0).take(size_of_start_region + size_of_remotes_region),

            // Only the locals region.
            (false, false, true) => {
                iter.skip(size_of_start_region + size_of_remotes_region).take(size_of_locals_region)
            }

            // The start region and the locals regions.
            //
            // This combination isn't implemented yet (because it contains a hole), but it's also
            // not necessary in our current code base; it's fine to ignore it.
            (true, false, true) => unimplemented!(
                "Iterating over the start and the locals regions is not implemented yet"
            ),

            // The remotes and the locals regions.
            (false, true, true) => {
                iter.skip(size_of_start_region).take(size_of_remotes_region + size_of_locals_region)
            }

            // All regions.
            (true, true, true) => iter
                .skip(0)
                .take(size_of_start_region + size_of_remotes_region + size_of_locals_region),
        };

        ObservableItemsTransactionIter { inner }
    }
}

/// An iterator over timeline items.
pub(crate) struct ObservableItemsTransactionIter<'observable_items_transaction> {
    #[allow(clippy::type_complexity)]
    inner: Take<
        Skip<
            Enumerate<
                imbl::vector::Iter<
                    'observable_items_transaction,
                    Arc<TimelineItem>,
                    imbl::shared_ptr::DefaultSharedPtr,
                >,
            >,
        >,
    >,
}

impl<'e> Iterator for ObservableItemsTransactionIter<'e> {
    type Item = (usize, &'e Arc<TimelineItem>);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

impl ExactSizeIterator for ObservableItemsTransactionIter<'_> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl DoubleEndedIterator for ObservableItemsTransactionIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back()
    }
}

// It's fine to deref to an immutable reference to `Vector`.
//
// We don't want, however, to deref to a mutable reference: it should be done
// via proper methods to control precisely the mapping between remote events and
// timeline items.
impl Deref for ObservableItemsTransaction<'_> {
    type Target = Vector<Arc<TimelineItem>>;

    fn deref(&self) -> &Self::Target {
        &self.items
    }
}

/// A handle to a single timeline item in an `ObservableItemsTransaction`.
pub struct ObservableItemsTransactionEntry<'observable_transaction_items, 'observable_items> {
    entry: ObservableVectorTransactionEntry<
        'observable_transaction_items,
        'observable_items,
        Arc<TimelineItem>,
    >,
    all_remote_events: &'observable_transaction_items mut AllRemoteEvents,
}

impl ObservableItemsTransactionEntry<'_, '_> {
    /// Remove this timeline item.
    pub fn remove(this: Self) {
        let entry_index = ObservableVectorTransactionEntry::index(&this.entry);

        ObservableVectorTransactionEntry::remove(this.entry);
        this.all_remote_events.timeline_item_has_been_removed_at(entry_index);
    }
}

// It's fine to deref to an immutable reference to `Arc<TimelineItem>`.
//
// We don't want, however, to deref to a mutable reference: it should be done
// via proper methods to control precisely the mapping between remote events and
// timeline items.
impl Deref for ObservableItemsTransactionEntry<'_, '_> {
    type Target = Arc<TimelineItem>;

    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

#[cfg(test)]
mod observable_items_tests {
    use std::ops::Not;

    use assert_matches::assert_matches;
    use eyeball_im::VectorDiff;
    use ruma::{
        MilliSecondsSinceUnixEpoch,
        events::room::message::{MessageType, TextMessageEventContent},
        owned_user_id, uint,
    };
    use stream_assert::{assert_next_matches, assert_pending};

    use super::*;
    use crate::timeline::{
        EventSendState, EventTimelineItem, Message, MsgLikeContent, MsgLikeKind, TimelineDetails,
        TimelineItemContent, TimelineItemKind, TimelineUniqueId, VirtualTimelineItem,
        controller::RemoteEventOrigin,
        event_item::{EventTimelineItemKind, LocalEventTimelineItem, RemoteEventTimelineItem},
    };

    fn item(event_id: &str) -> Arc<TimelineItem> {
        TimelineItem::new(
            EventTimelineItem::new(
                owned_user_id!("@ivan:mnt.io"),
                TimelineDetails::Unavailable,
                None,
                None,
                MilliSecondsSinceUnixEpoch(0u32.into()),
                TimelineItemContent::MsgLike(MsgLikeContent {
                    kind: MsgLikeKind::Message(Message {
                        msgtype: MessageType::Text(TextMessageEventContent::plain("hello")),
                        edited: false,
                        mentions: None,
                    }),
                    reactions: Default::default(),
                    thread_root: None,
                    in_reply_to: None,
                    thread_summary: None,
                }),
                EventTimelineItemKind::Remote(RemoteEventTimelineItem {
                    event_id: event_id.parse().unwrap(),
                    transaction_id: None,
                    read_receipts: Default::default(),
                    is_own: false,
                    is_highlighted: false,
                    encryption_info: None,
                    original_json: None,
                    latest_edit_json: None,
                    origin: RemoteEventOrigin::Sync,
                }),
                false,
            ),
            TimelineUniqueId(format!("__eid_{event_id}")),
        )
    }

    fn local_item(transaction_id: &str) -> Arc<TimelineItem> {
        TimelineItem::new(
            EventTimelineItem::new(
                owned_user_id!("@ivan:mnt.io"),
                TimelineDetails::Unavailable,
                None,
                None,
                MilliSecondsSinceUnixEpoch(0u32.into()),
                TimelineItemContent::MsgLike(MsgLikeContent {
                    kind: MsgLikeKind::Message(Message {
                        msgtype: MessageType::Text(TextMessageEventContent::plain("hello")),
                        edited: false,
                        mentions: None,
                    }),
                    reactions: Default::default(),
                    thread_root: None,
                    in_reply_to: None,
                    thread_summary: None,
                }),
                EventTimelineItemKind::Local(LocalEventTimelineItem {
                    send_state: EventSendState::NotSentYet { progress: None },
                    transaction_id: transaction_id.into(),
                    send_handle: None,
                }),
                false,
            ),
            TimelineUniqueId(format!("__tid_{transaction_id}")),
        )
    }

    fn read_marker() -> Arc<TimelineItem> {
        TimelineItem::read_marker()
    }

    fn event_meta(event_id: &str) -> EventMeta {
        EventMeta {
            event_id: event_id.parse().unwrap(),
            thread_root_id: None,
            timeline_item_index: None,
            visible: false,
            can_show_read_receipts: false,
        }
    }

    macro_rules! assert_event_id {
        ( $timeline_item:expr, $event_id:literal $( , $message:expr )? $(,)? ) => {
            assert_eq!($timeline_item.as_event().unwrap().event_id().unwrap().as_str(), $event_id $( , $message)? );
        };
    }

    macro_rules! assert_transaction_id {
        ( $timeline_item:expr, $transaction_id:literal $( , $message:expr )? $(,)? ) => {
            assert_eq!($timeline_item.as_event().unwrap().transaction_id().unwrap().as_str(), $transaction_id $( , $message)? );
        };
    }

    macro_rules! assert_mapping {
        ( on $transaction:ident:
          | event_id | event_index | timeline_item_index |
          | $( - )+ | $( - )+ | $( - )+ |
          $(
            | $event_id:literal | $event_index:literal | $( $timeline_item_index:literal )? |
          )+
        ) => {
            let all_remote_events = $transaction .all_remote_events();

            $(
                // Remote event exists at this index…
                assert_matches!(all_remote_events.0.get( $event_index ), Some(EventMeta { event_id, timeline_item_index, .. }) => {
                    // … this is the remote event with the expected event ID
                    assert_eq!(
                        event_id.as_str(),
                        $event_id ,
                        concat!("event #", $event_index, " should have ID ", $event_id)
                    );


                    // (tiny hack to handle the case where `$timeline_item_index` is absent)
                    #[allow(unused_variables)]
                    let timeline_item_index_is_expected = false;
                    $(
                        let timeline_item_index_is_expected = true;
                        let _ = $timeline_item_index;
                    )?

                    if timeline_item_index_is_expected.not() {
                        // … this remote event does NOT map to a timeline item index
                        assert!(
                            timeline_item_index.is_none(),
                            concat!("event #", $event_index, " with ID ", $event_id, " should NOT map to a timeline item index" )
                        );
                    }

                    $(
                        // … this remote event maps to a timeline item index
                        assert_eq!(
                            *timeline_item_index,
                            Some( $timeline_item_index ),
                            concat!("event #", $event_index, " with ID ", $event_id, " should map to timeline item #", $timeline_item_index )
                        );

                        // … this timeline index exists
                        assert_matches!( $transaction .get( $timeline_item_index ), Some(timeline_item) => {
                            // … this timelime item has the expected event ID
                            assert_event_id!(
                                timeline_item,
                                $event_id ,
                                concat!("timeline item #", $timeline_item_index, " should map to event ID ", $event_id )
                            );
                        });
                    )?
                });
            )*
        }
    }

    #[test]
    fn test_is_empty() {
        let mut items = ObservableItems::new();

        assert!(items.is_empty());

        // Push one event to check if `is_empty` returns false.
        let mut transaction = items.transaction();
        transaction.push_back(item("$ev0"), Some(0));
        transaction.commit();

        assert!(items.is_empty().not());
    }

    #[test]
    fn test_subscribe() {
        let mut items = ObservableItems::new();
        let mut subscriber = items.subscribe().into_stream();

        // Push one event to check the subscriber is emitting something.
        let mut transaction = items.transaction();
        transaction.push_back(item("$ev0"), Some(0));
        transaction.commit();

        // It does!
        assert_next_matches!(subscriber, VectorDiff::PushBack { value: event } => {
            assert_event_id!(event, "$ev0");
        });
    }

    #[test]
    fn test_clone_items() {
        let mut items = ObservableItems::new();

        let mut transaction = items.transaction();
        transaction.push_back(item("$ev0"), Some(0));
        transaction.push_back(item("$ev1"), Some(1));
        transaction.commit();

        let items = items.clone_items();
        assert_eq!(items.len(), 2);
        assert_event_id!(items[0], "$ev0");
        assert_event_id!(items[1], "$ev1");
    }

    #[test]
    fn test_replace() {
        let mut items = ObservableItems::new();

        // Push one event that will be replaced.
        let mut transaction = items.transaction();
        transaction.push_back(item("$ev0"), Some(0));
        transaction.commit();

        // That's time to replace it!
        items.replace(0, item("$ev1"));

        let items = items.clone_items();
        assert_eq!(items.len(), 1);
        assert_event_id!(items[0], "$ev1");
    }

    #[test]
    fn test_entries() {
        let mut items = ObservableItems::new();

        // Push events to iterate on.
        let mut transaction = items.transaction();
        transaction.push_back(item("$ev0"), Some(0));
        transaction.push_back(item("$ev1"), Some(1));
        transaction.push_back(item("$ev2"), Some(2));
        transaction.commit();

        let mut entries = items.entries();

        assert_matches!(entries.next(), Some(entry) => {
            assert_event_id!(entry, "$ev0");
        });
        assert_matches!(entries.next(), Some(entry) => {
            assert_event_id!(entry, "$ev1");
        });
        assert_matches!(entries.next(), Some(entry) => {
            assert_event_id!(entry, "$ev2");
        });
        assert_matches!(entries.next(), None);
    }

    #[test]
    fn test_entry_replace() {
        let mut items = ObservableItems::new();

        // Push events to iterate on.
        let mut transaction = items.transaction();
        transaction.push_back(item("$ev0"), Some(0));
        transaction.commit();

        let mut entries = items.entries();

        // Replace one event by another one.
        assert_matches!(entries.next(), Some(mut entry) => {
            assert_event_id!(entry, "$ev0");
            ObservableItemsEntry::replace(&mut entry, item("$ev1"));
        });
        assert_matches!(entries.next(), None);

        // Check the new event.
        let mut entries = items.entries();

        assert_matches!(entries.next(), Some(entry) => {
            assert_event_id!(entry, "$ev1");
        });
        assert_matches!(entries.next(), None);
    }

    #[test]
    fn test_for_each() {
        let mut items = ObservableItems::new();

        // Push events to iterate on.
        let mut transaction = items.transaction();
        transaction.push_back(item("$ev0"), Some(0));
        transaction.push_back(item("$ev1"), Some(1));
        transaction.push_back(item("$ev2"), Some(2));
        transaction.commit();

        let mut nth = 0;

        // Iterate over events.
        items.for_each(|entry| {
            match nth {
                0 => {
                    assert_event_id!(entry, "$ev0");
                }
                1 => {
                    assert_event_id!(entry, "$ev1");
                }
                2 => {
                    assert_event_id!(entry, "$ev2");
                }
                _ => unreachable!(),
            }

            nth += 1;
        });
    }

    #[test]
    fn test_transaction_commit() {
        let mut items = ObservableItems::new();

        // Don't commit the transaction.
        let mut transaction = items.transaction();
        transaction.push_back(item("$ev0"), Some(0));
        drop(transaction);

        assert!(items.is_empty());

        // Commit the transaction.
        let mut transaction = items.transaction();
        transaction.push_back(item("$ev0"), Some(0));
        transaction.commit();

        assert!(items.is_empty().not());
    }

    #[test]
    fn test_transaction_get() {
        let mut items = ObservableItems::new();

        let mut transaction = items.transaction();
        transaction.push_back(item("$ev0"), Some(0));

        assert_matches!(transaction.get(0), Some(event) => {
            assert_event_id!(event, "$ev0");
        });
    }

    #[test]
    fn test_transaction_replace() {
        let mut items = ObservableItems::new();

        let mut transaction = items.transaction();
        transaction.push_back(item("$ev0"), Some(0));
        transaction.replace(0, item("$ev1"));

        assert_matches!(transaction.get(0), Some(event) => {
            assert_event_id!(event, "$ev1");
        });
    }

    #[test]
    fn test_transaction_insert() {
        let mut items = ObservableItems::new();

        let mut transaction = items.transaction();

        // Remote event with its timeline item.
        transaction.push_back_remote_event(event_meta("$ev0"));
        transaction.insert(0, item("$ev0"), Some(0));

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 0                   | // new
        }

        // Timeline item without a remote event (for example a read marker).
        transaction.insert(0, read_marker(), None);

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 1                   | // has shifted
        }

        // Remote event with its timeline item.
        transaction.push_back_remote_event(event_meta("$ev1"));
        transaction.insert(2, item("$ev1"), Some(1));

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 1                   |
            | "$ev1"   | 1           | 2                   | // new
        }

        // Remote event without a timeline item (for example a state event).
        transaction.push_back_remote_event(event_meta("$ev2"));

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 1                   |
            | "$ev1"   | 1           | 2                   |
            | "$ev2"   | 2           |                     | // new
        }

        // Remote event with its timeline item.
        transaction.push_back_remote_event(event_meta("$ev3"));
        transaction.insert(3, item("$ev3"), Some(3));

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 1                   |
            | "$ev1"   | 1           | 2                   |
            | "$ev2"   | 2           |                     |
            | "$ev3"   | 3           | 3                   | // new
        }

        // Timeline item with a remote event, but late.
        // I don't know if this case is possible in reality, but let's be robust.
        transaction.insert(3, item("$ev2"), Some(2));

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 1                   |
            | "$ev1"   | 1           | 2                   |
            | "$ev2"   | 2           | 3                   | // updated
            | "$ev3"   | 3           | 4                   | // has shifted
        }

        // Let's move the read marker for the fun.
        transaction.remove(0);
        transaction.insert(2, read_marker(), None);

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 0                   | // has shifted
            | "$ev1"   | 1           | 1                   | // has shifted
            | "$ev2"   | 2           | 3                   |
            | "$ev3"   | 3           | 4                   |
        }

        assert_eq!(transaction.len(), 5);
    }

    #[test]
    fn test_transaction_push_front() {
        let mut items = ObservableItems::new();

        let mut transaction = items.transaction();

        // Remote event with its timeline item.
        transaction.push_front_remote_event(event_meta("$ev0"));
        transaction.push_front(item("$ev0"), Some(0));

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 0                   | // new
        }

        // Timeline item without a remote event (for example a read marker).
        transaction.push_front(read_marker(), None);

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 1                   | // has shifted
        }

        // Remote event with its timeline item.
        transaction.push_front_remote_event(event_meta("$ev1"));
        transaction.push_front(item("$ev1"), Some(0));

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev1"   | 0           | 0                   | // new
            | "$ev0"   | 1           | 2                   | // has shifted
        }

        // Remote event without a timeline item (for example a state event).
        transaction.push_front_remote_event(event_meta("$ev2"));

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev2"   | 0           |                     |
            | "$ev1"   | 1           | 0                   | // has shifted
            | "$ev0"   | 2           | 2                   | // has shifted
        }

        // Remote event with its timeline item.
        transaction.push_front_remote_event(event_meta("$ev3"));
        transaction.push_front(item("$ev3"), Some(0));

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev3"   | 0           | 0                   | // new
            | "$ev2"   | 1           |                     |
            | "$ev1"   | 2           | 1                   | // has shifted
            | "$ev0"   | 3           | 3                   | // has shifted
        }

        assert_eq!(transaction.len(), 4);
    }

    #[test]
    fn test_transaction_push_back() {
        let mut items = ObservableItems::new();

        let mut transaction = items.transaction();

        // Remote event with its timeline item.
        transaction.push_back_remote_event(event_meta("$ev0"));
        transaction.push_back(item("$ev0"), Some(0));

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 0                   | // new
        }

        // Timeline item without a remote event (for example a read marker).
        transaction.push_back(read_marker(), None);

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 0                   |
        }

        // Remote event with its timeline item.
        transaction.push_back_remote_event(event_meta("$ev1"));
        transaction.push_back(item("$ev1"), Some(1));

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 0                   |
            | "$ev1"   | 1           | 2                   | // new
        }

        // Remote event without a timeline item (for example a state event).
        transaction.push_back_remote_event(event_meta("$ev2"));

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 0                   |
            | "$ev1"   | 1           | 2                   |
            | "$ev2"   | 2           |                     | // new
        }

        // Remote event with its timeline item.
        transaction.push_back_remote_event(event_meta("$ev3"));
        transaction.push_back(item("$ev3"), Some(3));

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 0                   |
            | "$ev1"   | 1           | 2                   |
            | "$ev2"   | 2           |                     |
            | "$ev3"   | 3           | 3                   | // new
        }

        assert_eq!(transaction.len(), 4);
    }

    #[test]
    fn test_transaction_remove() {
        let mut items = ObservableItems::new();

        let mut transaction = items.transaction();

        // Remote event with its timeline item.
        transaction.push_back_remote_event(event_meta("$ev0"));
        transaction.push_back(item("$ev0"), Some(0));

        // Timeline item without a remote event (for example a read marker).
        transaction.push_back(read_marker(), None);

        // Remote event with its timeline item.
        transaction.push_back_remote_event(event_meta("$ev1"));
        transaction.push_back(item("$ev1"), Some(1));

        // Remote event without a timeline item (for example a state event).
        transaction.push_back_remote_event(event_meta("$ev2"));

        // Remote event with its timeline item.
        transaction.push_back_remote_event(event_meta("$ev3"));
        transaction.push_back(item("$ev3"), Some(3));

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 0                   |
            | "$ev1"   | 1           | 2                   |
            | "$ev2"   | 2           |                     |
            | "$ev3"   | 3           | 3                   |
        }

        // Remove the timeline item that has no event.
        transaction.remove(1);

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 0                   |
            | "$ev1"   | 1           | 1                   | // has shifted
            | "$ev2"   | 2           |                     |
            | "$ev3"   | 3           | 2                   | // has shifted
        }

        // Remove an timeline item that has an event.
        transaction.remove(1);

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 0                   |
            | "$ev1"   | 1           |                     | // has been removed
            | "$ev2"   | 2           |                     |
            | "$ev3"   | 3           | 1                   | // has shifted
        }

        // Remove the last timeline item to test off by 1 error.
        transaction.remove(1);

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 0                   |
            | "$ev1"   | 1           |                     |
            | "$ev2"   | 2           |                     |
            | "$ev3"   | 3           |                     | // has been removed
        }

        // Remove all the items \o/
        transaction.remove(0);

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           |                     | // has been removed
            | "$ev1"   | 1           |                     |
            | "$ev2"   | 2           |                     |
            | "$ev3"   | 3           |                     |
        }

        assert!(transaction.is_empty());
    }

    #[test]
    fn test_transaction_clear() {
        let mut items = ObservableItems::new();

        let mut transaction = items.transaction();

        // Remote event with its timeline item.
        transaction.push_back_remote_event(event_meta("$ev0"));
        transaction.push_back(item("$ev0"), Some(0));

        // Timeline item without a remote event (for example a read marker).
        transaction.push_back(read_marker(), None);

        // Remote event with its timeline item.
        transaction.push_back_remote_event(event_meta("$ev1"));
        transaction.push_back(item("$ev1"), Some(1));

        // Remote event without a timeline item (for example a state event).
        transaction.push_back_remote_event(event_meta("$ev2"));

        // Remote event with its timeline item.
        transaction.push_back_remote_event(event_meta("$ev3"));
        transaction.push_back(item("$ev3"), Some(3));

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 0                   |
            | "$ev1"   | 1           | 2                   |
            | "$ev2"   | 2           |                     |
            | "$ev3"   | 3           | 3                   |
        }

        assert_eq!(transaction.all_remote_events().0.len(), 4);
        assert_eq!(transaction.len(), 4);

        // Let's clear everything.
        transaction.clear();

        assert!(transaction.all_remote_events().0.is_empty());
        assert!(transaction.is_empty());
    }

    #[test]
    fn test_transaction_for_each() {
        let mut items = ObservableItems::new();

        // Push events to iterate on.
        let mut transaction = items.transaction();
        transaction.push_back(item("$ev0"), Some(0));
        transaction.push_back(item("$ev1"), Some(1));
        transaction.push_back(item("$ev2"), Some(2));

        let mut nth = 0;

        // Iterate over events.
        transaction.for_each(|entry| {
            match nth {
                0 => {
                    assert_event_id!(entry, "$ev0");
                }
                1 => {
                    assert_event_id!(entry, "$ev1");
                }
                2 => {
                    assert_event_id!(entry, "$ev2");
                }
                _ => unreachable!(),
            }

            nth += 1;
        });
    }

    #[test]
    fn test_transaction_for_each_remove() {
        let mut items = ObservableItems::new();

        // Push events to iterate on.
        let mut transaction = items.transaction();

        transaction.push_back_remote_event(event_meta("$ev0"));
        transaction.push_back(item("$ev0"), Some(0));

        transaction.push_back_remote_event(event_meta("$ev1"));
        transaction.push_back(item("$ev1"), Some(1));

        transaction.push_back_remote_event(event_meta("$ev2"));
        transaction.push_back(item("$ev2"), Some(2));

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 0                   |
            | "$ev1"   | 1           | 1                   |
            | "$ev2"   | 2           | 2                   |
        }

        // Iterate over events, and remove one.
        transaction.for_each(|entry| {
            if entry.as_event().unwrap().event_id().unwrap().as_str() == "$ev1" {
                ObservableItemsTransactionEntry::remove(entry);
            }
        });

        assert_mapping! {
            on transaction:

            | event_id | event_index | timeline_item_index |
            |----------|-------------|---------------------|
            | "$ev0"   | 0           | 0                   |
            | "$ev2"   | 2           | 1                   | // has shifted
        }

        assert_eq!(transaction.all_remote_events().0.len(), 3);
        assert_eq!(transaction.len(), 2);
    }

    #[test]
    fn test_transaction_push_local() {
        let mut items = ObservableItems::new();

        let mut transaction = items.transaction();

        // Push a remote item.
        transaction.push_back(item("$ev0"), None);

        // Push a local item.
        transaction.push_local(local_item("t0"));

        // Push another local item.
        transaction.push_local(local_item("t1"));

        transaction.commit();

        let mut entries = items.entries();

        assert_matches!(entries.next(), Some(entry) => {
            assert_event_id!(entry, "$ev0");
        });
        assert_matches!(entries.next(), Some(entry) => {
            assert_transaction_id!(entry, "t0");
        });
        assert_matches!(entries.next(), Some(entry) => {
            assert_transaction_id!(entry, "t1");
        });
        assert_matches!(entries.next(), None);
    }

    #[test]
    #[should_panic]
    fn test_transaction_push_local_panic_not_a_local() {
        let mut items = ObservableItems::new();
        let mut transaction = items.transaction();
        transaction.push_local(item("$ev0"));
    }

    #[test]
    fn test_transaction_push_date_divider() {
        let mut items = ObservableItems::new();
        let mut stream = items.subscribe().into_stream();

        let mut transaction = items.transaction();

        transaction.push_date_divider(
            0,
            TimelineItem::new(
                TimelineItemKind::Virtual(VirtualTimelineItem::DateDivider(
                    MilliSecondsSinceUnixEpoch(uint!(10)),
                )),
                TimelineUniqueId("__foo".to_owned()),
            ),
        );
        transaction.push_date_divider(
            0,
            TimelineItem::new(
                TimelineItemKind::Virtual(VirtualTimelineItem::DateDivider(
                    MilliSecondsSinceUnixEpoch(uint!(20)),
                )),
                TimelineUniqueId("__bar".to_owned()),
            ),
        );
        transaction.push_date_divider(
            1,
            TimelineItem::new(
                TimelineItemKind::Virtual(VirtualTimelineItem::DateDivider(
                    MilliSecondsSinceUnixEpoch(uint!(30)),
                )),
                TimelineUniqueId("__baz".to_owned()),
            ),
        );
        transaction.commit();

        assert_next_matches!(stream, VectorDiff::PushBack { value: timeline_item } => {
            assert_matches!(timeline_item.as_virtual(), Some(VirtualTimelineItem::DateDivider(ms)) => {
                assert_eq!(u64::from(ms.0), 10);
            });
        });
        assert_next_matches!(stream, VectorDiff::PushFront { value: timeline_item } => {
            assert_matches!(timeline_item.as_virtual(), Some(VirtualTimelineItem::DateDivider(ms)) => {
                assert_eq!(u64::from(ms.0), 20);
            });
        });
        assert_next_matches!(stream, VectorDiff::Insert { index: 1, value: timeline_item } => {
            assert_matches!(timeline_item.as_virtual(), Some(VirtualTimelineItem::DateDivider(ms)) => {
                assert_eq!(u64::from(ms.0), 30);
            });
        });
        assert_pending!(stream);
    }

    #[test]
    #[should_panic]
    fn test_transaction_push_date_divider_panic_not_a_date_divider() {
        let mut items = ObservableItems::new();
        let mut transaction = items.transaction();

        transaction.push_date_divider(0, item("$ev0"));
    }

    #[test]
    #[should_panic]
    fn test_transaction_push_date_divider_panic_not_in_remotes_region() {
        let mut items = ObservableItems::new();
        let mut transaction = items.transaction();

        transaction.push_timeline_start_if_missing(TimelineItem::new(
            VirtualTimelineItem::TimelineStart,
            TimelineUniqueId("__id_start".to_owned()),
        ));
        transaction.push_date_divider(
            0,
            TimelineItem::new(
                VirtualTimelineItem::DateDivider(MilliSecondsSinceUnixEpoch(uint!(10))),
                TimelineUniqueId("__date_divider".to_owned()),
            ),
        );
    }

    #[test]
    fn test_transaction_push_timeline_start_if_missing() {
        let mut items = ObservableItems::new();

        let mut transaction = items.transaction();

        // Push an item.
        transaction.push_back(item("$ev0"), None);

        // Push the timeline start.
        transaction.push_timeline_start_if_missing(TimelineItem::new(
            VirtualTimelineItem::TimelineStart,
            TimelineUniqueId("__id_start".to_owned()),
        ));

        // Push another item.
        transaction.push_back(item("$ev1"), None);

        // Try to push the timeline start again.
        transaction.push_timeline_start_if_missing(TimelineItem::new(
            VirtualTimelineItem::TimelineStart,
            TimelineUniqueId("__id_start_again".to_owned()),
        ));

        transaction.commit();

        let mut entries = items.entries();

        assert_matches!(entries.next(), Some(entry) => {
            assert!(entry.is_timeline_start());
        });
        assert_matches!(entries.next(), Some(entry) => {
            assert_event_id!(entry, "$ev0");
        });
        assert_matches!(entries.next(), Some(entry) => {
            assert_event_id!(entry, "$ev1");
        });
        assert_matches!(entries.next(), None);
    }

    #[test]
    fn test_transaction_iter_all_regions() {
        let mut items = ObservableItems::new();

        let mut transaction = items.transaction();
        transaction.push_timeline_start_if_missing(TimelineItem::new(
            VirtualTimelineItem::TimelineStart,
            TimelineUniqueId("__start".to_owned()),
        ));
        transaction.push_back(item("$ev0"), None);
        transaction.push_back(item("$ev1"), None);
        transaction.push_back(item("$ev2"), None);
        transaction.push_local(local_item("t0"));
        transaction.push_local(local_item("t1"));
        transaction.push_local(local_item("t2"));

        // Iterate all regions.
        let mut iter = transaction.iter_all_regions();
        assert_matches!(iter.next(), Some((0, item)) => {
            assert!(item.is_timeline_start());
        });
        assert_matches!(iter.next(), Some((1, item)) => {
            assert_event_id!(item, "$ev0");
        });
        assert_matches!(iter.next(), Some((2, item)) => {
            assert_event_id!(item, "$ev1");
        });
        assert_matches!(iter.next(), Some((3, item)) => {
            assert_event_id!(item, "$ev2");
        });
        assert_matches!(iter.next(), Some((4, item)) => {
            assert_transaction_id!(item, "t0");
        });
        assert_matches!(iter.next(), Some((5, item)) => {
            assert_transaction_id!(item, "t1");
        });
        assert_matches!(iter.next(), Some((6, item)) => {
            assert_transaction_id!(item, "t2");
        });
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_transaction_iter_remotes_regions() {
        let mut items = ObservableItems::new();

        let mut transaction = items.transaction();
        transaction.push_timeline_start_if_missing(TimelineItem::new(
            VirtualTimelineItem::TimelineStart,
            TimelineUniqueId("__start".to_owned()),
        ));
        transaction.push_back(item("$ev0"), None);
        transaction.push_back(item("$ev1"), None);
        transaction.push_back(item("$ev2"), None);
        transaction.push_local(local_item("t0"));
        transaction.push_local(local_item("t1"));
        transaction.push_local(local_item("t2"));

        // Iterate the remotes region.
        let mut iter = transaction.iter_remotes_region();
        assert_matches!(iter.next(), Some((1, item)) => {
            assert_event_id!(item, "$ev0");
        });
        assert_matches!(iter.next(), Some((2, item)) => {
            assert_event_id!(item, "$ev1");
        });
        assert_matches!(iter.next(), Some((3, item)) => {
            assert_event_id!(item, "$ev2");
        });
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_transaction_iter_remotes_regions_with_no_start_region() {
        let mut items = ObservableItems::new();

        let mut transaction = items.transaction();
        transaction.push_back(item("$ev0"), None);
        transaction.push_back(item("$ev1"), None);
        transaction.push_back(item("$ev2"), None);
        transaction.push_local(local_item("t0"));
        transaction.push_local(local_item("t1"));
        transaction.push_local(local_item("t2"));

        // Iterate the remotes region.
        let mut iter = transaction.iter_remotes_region();
        assert_matches!(iter.next(), Some((0, item)) => {
            assert_event_id!(item, "$ev0");
        });
        assert_matches!(iter.next(), Some((1, item)) => {
            assert_event_id!(item, "$ev1");
        });
        assert_matches!(iter.next(), Some((2, item)) => {
            assert_event_id!(item, "$ev2");
        });
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_transaction_iter_remotes_regions_with_no_locals_region() {
        let mut items = ObservableItems::new();

        let mut transaction = items.transaction();
        transaction.push_back(item("$ev0"), None);
        transaction.push_back(item("$ev1"), None);
        transaction.push_back(item("$ev2"), None);

        // Iterate the remotes region.
        let mut iter = transaction.iter_remotes_region();
        assert_matches!(iter.next(), Some((0, item)) => {
            assert_event_id!(item, "$ev0");
        });
        assert_matches!(iter.next(), Some((1, item)) => {
            assert_event_id!(item, "$ev1");
        });
        assert_matches!(iter.next(), Some((2, item)) => {
            assert_event_id!(item, "$ev2");
        });
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_transaction_iter_locals_region() {
        let mut items = ObservableItems::new();

        let mut transaction = items.transaction();
        transaction.push_timeline_start_if_missing(TimelineItem::new(
            VirtualTimelineItem::TimelineStart,
            TimelineUniqueId("__start".to_owned()),
        ));
        transaction.push_back(item("$ev0"), None);
        transaction.push_back(item("$ev1"), None);
        transaction.push_back(item("$ev2"), None);
        transaction.push_local(local_item("t0"));
        transaction.push_local(local_item("t1"));
        transaction.push_local(local_item("t2"));

        // Iterate the locals region.
        let mut iter = transaction.iter_locals_region();
        assert_matches!(iter.next(), Some((4, item)) => {
            assert_transaction_id!(item, "t0");
        });
        assert_matches!(iter.next(), Some((5, item)) => {
            assert_transaction_id!(item, "t1");
        });
        assert_matches!(iter.next(), Some((6, item)) => {
            assert_transaction_id!(item, "t2");
        });
        assert!(iter.next().is_none());
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
    /// Return a reference to a remote event.
    pub fn get(&self, event_index: usize) -> Option<&EventMeta> {
        self.0.get(event_index)
    }

    /// Return a front-to-back iterator over all remote events.
    pub fn iter(&self) -> Iter<'_, EventMeta> {
        self.0.iter()
    }

    /// Return a front-to-back iterator covering ranges of all remote events
    /// described by `range`.
    pub fn range<R>(&self, range: R) -> Iter<'_, EventMeta>
    where
        R: RangeBounds<usize>,
    {
        self.0.range(range)
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

    /// Insert a new remote event at a specific index.
    fn insert(&mut self, event_index: usize, event_meta: EventMeta) {
        // If there is an associated `timeline_item_index`, shift all the
        // `timeline_item_index` that come after this one.
        if let Some(new_timeline_item_index) = event_meta.timeline_item_index {
            self.increment_all_timeline_item_index_after(new_timeline_item_index);
        }

        // Insert the event.
        self.0.insert(event_index, event_meta)
    }

    /// Remove one remote event at a specific index, and return it if it exists.
    fn remove(&mut self, event_index: usize) -> Option<EventMeta> {
        // Remove the event.
        let event_meta = self.0.remove(event_index)?;

        // If there is an associated `timeline_item_index`, shift all the
        // `timeline_item_index` that come after this one.
        if let Some(removed_timeline_item_index) = event_meta.timeline_item_index {
            self.decrement_all_timeline_item_index_after(removed_timeline_item_index);
        }

        Some(event_meta)
    }

    /// Return a reference to the last remote event if it exists.
    #[cfg(test)]
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

    /// Get an immutable reference to a specific remote event by its ID.
    pub fn get_by_event_id(&self, event_id: &EventId) -> Option<&EventMeta> {
        self.0.iter().rev().find(|event_meta| event_meta.event_id == event_id)
    }

    /// Get the position of an event in the events array by its ID.
    pub fn position_by_event_id(&self, event_id: &EventId) -> Option<usize> {
        // Reverse the iterator to start looking at the end. Since this will give us the
        // "reverse" position, reverse the index after finding the event.
        self.0
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, event_meta)| (event_meta.event_id == event_id).then_some(i))
    }

    /// Shift to the right all timeline item indexes that are equal to or
    /// greater than `new_timeline_item_index`.
    fn increment_all_timeline_item_index_after(&mut self, new_timeline_item_index: usize) {
        // Traverse items from back to front because:
        // - if `new_timeline_item_index` is 0, we need to shift all items anyways, so
        //   all items must be traversed,
        // - otherwise, it's unlikely we want to traverse all items: the item has been
        //   either inserted or pushed back, so there is no need to traverse the first
        //   items; we can also break the iteration as soon as all timeline item index
        //   after `new_timeline_item_index` has been updated.
        for event_meta in self.0.iter_mut().rev() {
            if let Some(timeline_item_index) = event_meta.timeline_item_index.as_mut() {
                if *timeline_item_index >= new_timeline_item_index {
                    *timeline_item_index += 1;
                } else {
                    // Items are ordered.
                    break;
                }
            }
        }
    }

    /// Shift to the left all timeline item indexes that are greater than
    /// `removed_wtimeline_item_index`.
    fn decrement_all_timeline_item_index_after(&mut self, removed_timeline_item_index: usize) {
        // Traverse items from back to front because:
        // - if `new_timeline_item_index` is 0, we need to shift all items anyways, so
        //   all items must be traversed,
        // - otherwise, it's unlikely we want to traverse all items: the item has been
        //   either inserted or pushed back, so there is no need to traverse the first
        //   items; we can also break the iteration as soon as all timeline item index
        //   after `new_timeline_item_index` has been updated.
        for event_meta in self.0.iter_mut().rev() {
            if let Some(timeline_item_index) = event_meta.timeline_item_index.as_mut() {
                if *timeline_item_index > removed_timeline_item_index {
                    *timeline_item_index -= 1;
                } else {
                    // Items are ordered.
                    break;
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

        if let Some(event_index) = event_index
            && let Some(event_meta) = self.0.get_mut(event_index)
        {
            event_meta.timeline_item_index = Some(new_timeline_item_index);
        }
    }

    /// Notify that a timeline item has been removed at
    /// `new_timeline_item_index`.
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

#[cfg(test)]
mod all_remote_events_tests {
    use assert_matches::assert_matches;
    use ruma::event_id;

    use super::{AllRemoteEvents, EventMeta};

    fn event_meta(event_id: &str, timeline_item_index: Option<usize>) -> EventMeta {
        EventMeta {
            event_id: event_id.parse().unwrap(),
            thread_root_id: None,
            timeline_item_index,
            visible: false,
            can_show_read_receipts: false,
        }
    }

    macro_rules! assert_events {
        ( $events:ident, [ $( ( $event_id:literal, $timeline_item_index:expr ) ),* $(,)? ] ) => {
            let mut iter = $events .iter();

            $(
                assert_matches!(iter.next(), Some(EventMeta { event_id, timeline_item_index, .. }) => {
                    assert_eq!(event_id.as_str(), $event_id );
                    assert_eq!(*timeline_item_index, $timeline_item_index );
                });
            )*

            assert!(iter.next().is_none(), "Not all events have been asserted");
        }
    }

    #[test]
    fn test_range() {
        let mut events = AllRemoteEvents::default();

        // Push some events.
        events.push_back(event_meta("$ev0", None));
        events.push_back(event_meta("$ev1", None));
        events.push_back(event_meta("$ev2", None));

        assert_eq!(events.iter().count(), 3);

        // Test a few combinations.
        assert_eq!(events.range(..).count(), 3);
        assert_eq!(events.range(1..).count(), 2);
        assert_eq!(events.range(0..=1).count(), 2);

        // Iterate on some of them.
        let mut some_events = events.range(1..);

        assert_matches!(some_events.next(), Some(EventMeta { event_id, .. }) => {
            assert_eq!(event_id.as_str(), "$ev1");
        });
        assert_matches!(some_events.next(), Some(EventMeta { event_id, .. }) => {
            assert_eq!(event_id.as_str(), "$ev2");
        });
        assert!(some_events.next().is_none());
    }

    #[test]
    fn test_clear() {
        let mut events = AllRemoteEvents::default();

        // Push some events.
        events.push_back(event_meta("$ev0", None));
        events.push_back(event_meta("$ev1", None));
        events.push_back(event_meta("$ev2", None));

        assert_eq!(events.iter().count(), 3);

        // And clear them!
        events.clear();

        assert_eq!(events.iter().count(), 0);
    }

    #[test]
    fn test_push_front() {
        let mut events = AllRemoteEvents::default();

        // Push front on an empty set, nothing particular.
        events.push_front(event_meta("$ev0", Some(1)));

        // Push front with no `timeline_item_index`.
        events.push_front(event_meta("$ev1", None));

        // Push front with a `timeline_item_index`.
        events.push_front(event_meta("$ev2", Some(0)));

        // Push front with the same `timeline_item_index`.
        events.push_front(event_meta("$ev3", Some(0)));

        assert_events!(
            events,
            [
                // `timeline_item_index` is untouched
                ("$ev3", Some(0)),
                // `timeline_item_index` has been shifted once
                ("$ev2", Some(1)),
                // no `timeline_item_index`
                ("$ev1", None),
                // `timeline_item_index` has been shifted twice
                ("$ev0", Some(3)),
            ]
        );
    }

    #[test]
    fn test_push_back() {
        let mut events = AllRemoteEvents::default();

        // Push back on an empty set, nothing particular.
        events.push_back(event_meta("$ev0", Some(0)));

        // Push back with no `timeline_item_index`.
        events.push_back(event_meta("$ev1", None));

        // Push back with a `timeline_item_index`.
        events.push_back(event_meta("$ev2", Some(1)));

        // Push back with a `timeline_item_index` pointing to a timeline item that is
        // not the last one. Is it possible in practise? Normally not, but let's test
        // it anyway.
        events.push_back(event_meta("$ev3", Some(1)));

        assert_events!(
            events,
            [
                // `timeline_item_index` is untouched
                ("$ev0", Some(0)),
                // no `timeline_item_index`
                ("$ev1", None),
                // `timeline_item_index` has been shifted once
                ("$ev2", Some(2)),
                // `timeline_item_index` is untouched
                ("$ev3", Some(1)),
            ]
        );
    }

    #[test]
    fn test_insert() {
        let mut events = AllRemoteEvents::default();

        // Insert on an empty set, nothing particular.
        events.insert(0, event_meta("$ev0", Some(0)));

        // Insert at the end with no `timeline_item_index`.
        events.insert(1, event_meta("$ev1", None));

        // Insert at the end with a `timeline_item_index`.
        events.insert(2, event_meta("$ev2", Some(1)));

        // Insert at the start, with a `timeline_item_index`.
        events.insert(0, event_meta("$ev3", Some(0)));

        assert_events!(
            events,
            [
                // `timeline_item_index` is untouched
                ("$ev3", Some(0)),
                // `timeline_item_index` has been shifted once
                ("$ev0", Some(1)),
                // no `timeline_item_index`
                ("$ev1", None),
                // `timeline_item_index` has been shifted once
                ("$ev2", Some(2)),
            ]
        );
    }

    #[test]
    fn test_remove() {
        let mut events = AllRemoteEvents::default();

        // Push some events.
        events.push_back(event_meta("$ev0", Some(0)));
        events.push_back(event_meta("$ev1", Some(1)));
        events.push_back(event_meta("$ev2", None));
        events.push_back(event_meta("$ev3", Some(2)));

        // Assert initial state.
        assert_events!(
            events,
            [("$ev0", Some(0)), ("$ev1", Some(1)), ("$ev2", None), ("$ev3", Some(2))]
        );

        // Remove two events.
        events.remove(2); // $ev2 has no `timeline_item_index`
        events.remove(1); // $ev1 has a `timeline_item_index`

        assert_events!(
            events,
            [
                ("$ev0", Some(0)),
                // `timeline_item_index` has shifted once
                ("$ev3", Some(1)),
            ]
        );
    }

    #[test]
    fn test_last() {
        let mut events = AllRemoteEvents::default();

        assert!(events.last().is_none());
        assert!(events.last_index().is_none());

        // Push some events.
        events.push_back(event_meta("$ev0", Some(0)));
        events.push_back(event_meta("$ev1", Some(1)));

        assert_matches!(events.last(), Some(EventMeta { event_id, .. }) => {
            assert_eq!(event_id.as_str(), "$ev1");
        });
        assert_eq!(events.last_index(), Some(1));
    }

    #[test]
    fn test_get_by_event_by_mut() {
        let mut events = AllRemoteEvents::default();

        // Push some events.
        events.push_back(event_meta("$ev0", Some(0)));
        events.push_back(event_meta("$ev1", Some(1)));

        assert!(events.get_by_event_id_mut(event_id!("$ev0")).is_some());
        assert!(events.get_by_event_id_mut(event_id!("$ev42")).is_none());
    }

    #[test]
    fn test_timeline_item_has_been_inserted_at() {
        let mut events = AllRemoteEvents::default();

        // Push some events.
        events.push_back(event_meta("$ev0", Some(0)));
        events.push_back(event_meta("$ev1", Some(1)));
        events.push_back(event_meta("$ev2", None));
        events.push_back(event_meta("$ev3", None));
        events.push_back(event_meta("$ev4", Some(2)));
        events.push_back(event_meta("$ev5", Some(3)));
        events.push_back(event_meta("$ev6", None));

        // A timeline item has been inserted at index 2, and maps to no event.
        events.timeline_item_has_been_inserted_at(2, None);

        assert_events!(
            events,
            [
                ("$ev0", Some(0)),
                ("$ev1", Some(1)),
                ("$ev2", None),
                ("$ev3", None),
                // `timeline_item_index` is shifted once
                ("$ev4", Some(3)),
                // `timeline_item_index` is shifted once
                ("$ev5", Some(4)),
                ("$ev6", None),
            ]
        );

        // A timeline item has been inserted at the back, and maps to `$ev6`.
        events.timeline_item_has_been_inserted_at(5, Some(6));

        assert_events!(
            events,
            [
                ("$ev0", Some(0)),
                ("$ev1", Some(1)),
                ("$ev2", None),
                ("$ev3", None),
                ("$ev4", Some(3)),
                ("$ev5", Some(4)),
                // `timeline_item_index` has been updated
                ("$ev6", Some(5)),
            ]
        );
    }

    #[test]
    fn test_timeline_item_has_been_removed_at() {
        let mut events = AllRemoteEvents::default();

        // Push some events.
        events.push_back(event_meta("$ev0", Some(0)));
        events.push_back(event_meta("$ev1", Some(1)));
        events.push_back(event_meta("$ev2", None));
        events.push_back(event_meta("$ev3", None));
        events.push_back(event_meta("$ev4", Some(3)));
        events.push_back(event_meta("$ev5", Some(4)));
        events.push_back(event_meta("$ev6", None));

        // A timeline item has been removed at index 2, which maps to no event.
        events.timeline_item_has_been_removed_at(2);

        assert_events!(
            events,
            [
                ("$ev0", Some(0)),
                ("$ev1", Some(1)),
                ("$ev2", None),
                ("$ev3", None),
                // `timeline_item_index` is shifted once
                ("$ev4", Some(2)),
                // `timeline_item_index` is shifted once
                ("$ev5", Some(3)),
                ("$ev6", None),
            ]
        );

        // A timeline item has been removed at index 2, which maps to `$ev4`.
        events.timeline_item_has_been_removed_at(2);

        assert_events!(
            events,
            [
                ("$ev0", Some(0)),
                ("$ev1", Some(1)),
                ("$ev2", None),
                ("$ev3", None),
                // `timeline_item_index` has been updated
                ("$ev4", None),
                // `timeline_item_index` has shifted once
                ("$ev5", Some(2)),
                ("$ev6", None),
            ]
        );

        // A timeline item has been removed at index 0, which maps to `$ev0`.
        events.timeline_item_has_been_removed_at(0);

        assert_events!(
            events,
            [
                // `timeline_item_index` has been updated
                ("$ev0", None),
                // `timeline_item_index` has shifted once
                ("$ev1", Some(0)),
                ("$ev2", None),
                ("$ev3", None),
                ("$ev4", None),
                // `timeline_item_index` has shifted once
                ("$ev5", Some(1)),
                ("$ev6", None),
            ]
        );
    }
}
