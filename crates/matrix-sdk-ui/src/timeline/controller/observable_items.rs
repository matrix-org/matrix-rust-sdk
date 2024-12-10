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
    ops::{Deref, RangeBounds},
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
        self.items.for_each(|entry| {
            f(ObservableItemsTransactionEntry { entry, all_remote_events: self.all_remote_events })
        })
    }

    /// Commit this transaction, persisting the changes and notifying
    /// subscribers.
    pub fn commit(self) {
        self.items.commit()
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
    /// Replace the timeline item by `timeline_item`.
    pub fn replace(this: &mut Self, timeline_item: Arc<TimelineItem>) -> Arc<TimelineItem> {
        ObservableVectorTransactionEntry::set(&mut this.entry, timeline_item)
    }

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
        events::room::message::{MessageType, TextMessageEventContent},
        owned_user_id, MilliSecondsSinceUnixEpoch,
    };
    use stream_assert::assert_next_matches;

    use super::*;
    use crate::timeline::{
        controller::{EventTimelineItemKind, RemoteEventOrigin},
        event_item::RemoteEventTimelineItem,
        EventTimelineItem, Message, TimelineDetails, TimelineItemContent, TimelineUniqueId,
    };

    fn item(event_id: &str) -> Arc<TimelineItem> {
        TimelineItem::new(
            EventTimelineItem::new(
                owned_user_id!("@ivan:mnt.io"),
                TimelineDetails::Unavailable,
                MilliSecondsSinceUnixEpoch(0u32.into()),
                TimelineItemContent::Message(Message {
                    msgtype: MessageType::Text(TextMessageEventContent::plain("hello")),
                    in_reply_to: None,
                    thread_root: None,
                    edited: false,
                    mentions: None,
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
                Default::default(),
                false,
            ),
            TimelineUniqueId(format!("__id_{event_id}")),
        )
    }

    fn read_marker() -> Arc<TimelineItem> {
        TimelineItem::read_marker()
    }

    fn event_meta(event_id: &str) -> EventMeta {
        EventMeta { event_id: event_id.parse().unwrap(), timeline_item_index: None, visible: false }
    }

    macro_rules! assert_event_id {
        ( $timeline_item:expr, $event_id:literal $( , $message:expr )? $(,)? ) => {
            assert_eq!($timeline_item.as_event().unwrap().event_id().unwrap().as_str(), $event_id $( , $message)? );
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
    /// describes by `range`.
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
        EventMeta { event_id: event_id.parse().unwrap(), timeline_item_index, visible: false }
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
