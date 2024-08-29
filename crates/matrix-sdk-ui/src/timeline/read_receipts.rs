// Copyright 2023 Kévin Commaille
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
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use eyeball_im::ObservableVectorTransaction;
use indexmap::IndexMap;
use ruma::{
    events::receipt::{Receipt, ReceiptEventContent, ReceiptThread, ReceiptType},
    EventId, OwnedEventId, OwnedUserId, UserId,
};
use tracing::{debug, error, warn};

use super::{
    controller::{
        EventMeta, FullEventMeta, TimelineMetadata, TimelineState, TimelineStateTransaction,
    },
    traits::RoomDataProvider,
    util::{rfind_event_by_id, RelativePosition},
    TimelineItem,
};

/// In-memory caches for read receipts.
#[derive(Clone, Debug, Default)]
pub(super) struct ReadReceipts {
    /// Map of public read receipts on events.
    ///
    /// Event ID => User ID => Read receipt of the user.
    by_event: HashMap<OwnedEventId, IndexMap<OwnedUserId, Receipt>>,

    /// In-memory cache of all latest read receipts by user.
    ///
    /// User ID => Receipt type => Read receipt of the user of the given
    /// type.
    latest_by_user: HashMap<OwnedUserId, HashMap<ReceiptType, (OwnedEventId, Receipt)>>,
}

impl ReadReceipts {
    /// Empty the caches.
    pub(super) fn clear(&mut self) {
        self.by_event.clear();
        self.latest_by_user.clear();
    }

    /// Read the latest read receipt of the given type for the given user, from
    /// the in-memory cache.
    fn get_latest(
        &self,
        user_id: &UserId,
        receipt_type: &ReceiptType,
    ) -> Option<&(OwnedEventId, Receipt)> {
        self.latest_by_user.get(user_id).and_then(|map| map.get(receipt_type))
    }

    /// Insert or update in the local cache the latest read receipt for the
    /// given user.
    fn upsert_latest(
        &mut self,
        user_id: OwnedUserId,
        receipt_type: ReceiptType,
        read_receipt: (OwnedEventId, Receipt),
    ) {
        self.latest_by_user.entry(user_id).or_default().insert(receipt_type, read_receipt);
    }

    /// Update the timeline items with the given read receipt if it is more
    /// recent than the current one.
    ///
    /// In the process, if applicable, this method updates the inner maps to use
    /// the new receipt. If `is_own_user_id` is `false`, it also updates the
    /// receipts on the corresponding timeline items.
    ///
    /// Currently this method only works reliably if the timeline was started
    /// from the end of the timeline.
    fn maybe_update_read_receipt(
        &mut self,
        new_receipt: FullReceipt<'_>,
        is_own_user_id: bool,
        all_events: &VecDeque<EventMeta>,
        timeline_items: &mut ObservableVectorTransaction<'_, Arc<TimelineItem>>,
    ) {
        // Get old receipt.
        let old_receipt = self.get_latest(new_receipt.user_id, &new_receipt.receipt_type);
        if old_receipt
            .as_ref()
            .is_some_and(|(old_receipt_event_id, _)| old_receipt_event_id == new_receipt.event_id)
        {
            // The receipt has not changed so there is nothing to do.
            return;
        }
        let old_event_id = old_receipt.map(|(event_id, _)| event_id);

        // Find receipts positions.
        let mut old_receipt_pos = None;
        let mut old_item_event_id = None;
        let mut new_receipt_pos = None;
        let mut new_item_event_id = None;
        for (pos, event) in all_events.iter().rev().enumerate() {
            if old_event_id == Some(&event.event_id) {
                old_receipt_pos = Some(pos);
            }
            // The receipt should appear on the first event that is visible.
            if old_receipt_pos.is_some() && old_item_event_id.is_none() && event.visible {
                old_item_event_id = Some(event.event_id.clone());
            }

            if new_receipt.event_id == event.event_id {
                new_receipt_pos = Some(pos);
            }
            // The receipt should appear on the first event that is visible.
            if new_receipt_pos.is_some() && new_item_event_id.is_none() && event.visible {
                new_item_event_id = Some(event.event_id.clone());
            }

            if old_item_event_id.is_some() && new_item_event_id.is_some() {
                // We have everything we need, stop.
                break;
            }
        }

        // Check if the old receipt is more recent than the new receipt.
        if let Some(old_receipt_pos) = old_receipt_pos {
            let Some(new_receipt_pos) = new_receipt_pos else {
                // The old receipt is more recent since we can't find the new receipt in the
                // timeline and we supposedly have all events since the end of the timeline.
                return;
            };

            if old_receipt_pos < new_receipt_pos {
                // The old receipt is more recent than the new one.
                return;
            }
        }

        // The new receipt is deemed more recent from now on because:
        // - If old_receipt_pos is Some, we already checked all the cases where it
        //   wouldn't be more recent.
        // - If both old_receipt_pos and new_receipt_pos are None, they are both
        //   explicit read receipts so the server should only send us a more recent
        //   receipt.
        // - If old_receipt_pos is None and new_receipt_pos is Some, the new receipt is
        //   more recent because it has a place in the timeline.

        if !is_own_user_id {
            // Remove the old receipt from the old event.
            if let Some(old_event_id) = old_event_id.cloned() {
                self.remove_event_receipt_for_user(&old_event_id, new_receipt.user_id);
            }
            // Add the new receipt to the new event.
            self.add_event_receipt_for_user(
                new_receipt.event_id.to_owned(),
                new_receipt.user_id.to_owned(),
                new_receipt.receipt.clone(),
            );
        }

        // Update the receipt of the user.
        self.upsert_latest(
            new_receipt.user_id.to_owned(),
            new_receipt.receipt_type,
            (new_receipt.event_id.to_owned(), new_receipt.receipt.clone()),
        );

        if is_own_user_id || new_item_event_id == old_item_event_id {
            // The receipt did not change in the timeline.
            return;
        }

        let timeline_update = ReadReceiptTimelineUpdate {
            old_event_id: old_item_event_id,
            new_event_id: new_item_event_id,
        };
        timeline_update.apply(
            timeline_items,
            new_receipt.user_id.to_owned(),
            new_receipt.receipt.clone(),
        );
    }

    /// Returns the cached receipts by user for a given `event_id`.
    fn get_event_receipts(&self, event_id: &EventId) -> Option<&IndexMap<OwnedUserId, Receipt>> {
        self.by_event.get(event_id)
    }

    /// Mark the given event as seen by the user with the given receipt.
    fn add_event_receipt_for_user(
        &mut self,
        event_id: OwnedEventId,
        user_id: OwnedUserId,
        receipt: Receipt,
    ) {
        self.by_event.entry(event_id).or_default().insert(user_id, receipt);
    }

    /// Unmark the given event as seen by the user.
    fn remove_event_receipt_for_user(&mut self, event_id: &EventId, user_id: &UserId) {
        if let Some(map) = self.by_event.get_mut(event_id) {
            map.swap_remove(user_id);
            // Remove the entire map if this was the last entry.
            if map.is_empty() {
                self.by_event.remove(event_id);
            }
        }
    }

    /// Get the read receipts by user for the given event.
    ///
    /// This includes all the receipts on the event as well as all the receipts
    /// on the following events that are filtered out (not visible).
    pub(super) fn compute_event_receipts(
        &self,
        event_id: &EventId,
        all_events: &VecDeque<EventMeta>,
        at_end: bool,
    ) -> IndexMap<OwnedUserId, Receipt> {
        let mut all_receipts = self.get_event_receipts(event_id).cloned().unwrap_or_default();

        if at_end {
            // No need to search for extra receipts, there are no events after.
            return all_receipts;
        }

        // Find the event.
        let events_iter = all_events.iter().skip_while(|meta| meta.event_id != event_id);

        // Get past the event
        let events_iter = events_iter.skip(1);

        // Include receipts from all the following non-visible events.
        for hidden_event_meta in events_iter.take_while(|meta| !meta.visible) {
            if let Some(event_receipts) = self.get_event_receipts(&hidden_event_meta.event_id) {
                all_receipts.extend(event_receipts.clone());
            }
        }

        all_receipts
    }
}

struct FullReceipt<'a> {
    event_id: &'a EventId,
    user_id: &'a UserId,
    receipt_type: ReceiptType,
    receipt: &'a Receipt,
}

/// A read receipt update in the timeline.
#[derive(Clone, Debug, Default)]
struct ReadReceiptTimelineUpdate {
    /// The old event that had the receipt of the user, if any.
    old_event_id: Option<OwnedEventId>,
    /// The new event that has the receipt of the user, if any.
    new_event_id: Option<OwnedEventId>,
}

impl ReadReceiptTimelineUpdate {
    /// Remove the old receipt from the corresponding timeline item.
    fn remove_old_receipt(
        &self,
        items: &mut ObservableVectorTransaction<'_, Arc<TimelineItem>>,
        user_id: &UserId,
    ) {
        let Some(event_id) = &self.old_event_id else {
            // Nothing to do.
            return;
        };

        let Some((receipt_pos, event_item)) = rfind_event_by_id(items, event_id) else {
            debug!(%event_id, %user_id, "inconsistent state: old event item for read receipt was not found");
            return;
        };

        let event_item_id = event_item.internal_id.to_owned();
        let mut event_item = event_item.clone();

        if let Some(remote_event_item) = event_item.as_remote_mut() {
            if remote_event_item.read_receipts.swap_remove(user_id).is_none() {
                debug!(
                    %event_id, %user_id,
                    "inconsistent state: old event item for user's read \
                     receipt doesn't have a receipt for the user"
                );
            }
            items.set(receipt_pos, TimelineItem::new(event_item, event_item_id));
        } else {
            warn!("received a read receipt for a local item, this should not be possible");
        }
    }

    /// Add the new receipt to the corresponding timeline item.
    fn add_new_receipt(
        self,
        items: &mut ObservableVectorTransaction<'_, Arc<TimelineItem>>,
        user_id: OwnedUserId,
        receipt: Receipt,
    ) {
        let Some(event_id) = self.new_event_id else {
            // Nothing to do.
            return;
        };

        let Some((receipt_pos, event_item)) = rfind_event_by_id(items, &event_id) else {
            // This can happen for new timeline items, the receipts will be loaded directly
            // during construction of the item.
            return;
        };

        let event_item_id = event_item.internal_id.to_owned();
        let mut event_item = event_item.clone();

        if let Some(remote_event_item) = event_item.as_remote_mut() {
            remote_event_item.read_receipts.insert(user_id, receipt);
            items.set(receipt_pos, TimelineItem::new(event_item, event_item_id));
        } else {
            warn!("received a read receipt for a local item, this should not be possible");
        }
    }

    /// Apply this update to the timeline.
    fn apply(
        self,
        items: &mut ObservableVectorTransaction<'_, Arc<TimelineItem>>,
        user_id: OwnedUserId,
        receipt: Receipt,
    ) {
        self.remove_old_receipt(items, &user_id);
        self.add_new_receipt(items, user_id, receipt);
    }
}

impl TimelineStateTransaction<'_> {
    pub(super) fn handle_explicit_read_receipts(
        &mut self,
        receipt_event_content: ReceiptEventContent,
        own_user_id: &UserId,
    ) {
        for (event_id, receipt_types) in receipt_event_content.0 {
            for (receipt_type, receipts) in receipt_types {
                // We only care about read receipts here.
                if !matches!(receipt_type, ReceiptType::Read | ReceiptType::ReadPrivate) {
                    continue;
                }

                for (user_id, receipt) in receipts {
                    if !matches!(receipt.thread, ReceiptThread::Unthreaded | ReceiptThread::Main) {
                        continue;
                    }

                    let is_own_user_id = user_id == own_user_id;
                    let full_receipt = FullReceipt {
                        event_id: &event_id,
                        user_id: &user_id,
                        receipt_type: receipt_type.clone(),
                        receipt: &receipt,
                    };

                    self.meta.read_receipts.maybe_update_read_receipt(
                        full_receipt,
                        is_own_user_id,
                        &self.meta.all_events,
                        &mut self.items,
                    );
                }
            }
        }
    }

    /// Load the read receipts from the store for the given event ID.
    ///
    /// Populates the read receipts in-memory caches.
    pub(super) async fn load_read_receipts_for_event<P: RoomDataProvider>(
        &mut self,
        event_id: &EventId,
        room_data_provider: &P,
    ) {
        let read_receipts = room_data_provider.load_event_receipts(event_id).await;

        // Filter out receipts for our own user.
        let own_user_id = room_data_provider.own_user_id();
        let read_receipts = read_receipts.into_iter().filter(|(user_id, _)| user_id != own_user_id);

        // Since they are explicit read receipts, we need to check if they are
        // superseded by implicit read receipts.
        for (user_id, receipt) in read_receipts {
            let full_receipt = FullReceipt {
                event_id,
                user_id: &user_id,
                receipt_type: ReceiptType::Read,
                receipt: &receipt,
            };

            self.meta.read_receipts.maybe_update_read_receipt(
                full_receipt,
                user_id == own_user_id,
                &self.meta.all_events,
                &mut self.items,
            );
        }
    }

    /// Add an implicit read receipt to the given event item, if it is more
    /// recent than the current read receipt for the sender of the event.
    ///
    /// According to the spec, read receipts should not point to events sent by
    /// our own user, but these events are used to reset the notification
    /// count, so we need to handle them locally too. For that we create an
    /// "implicit" read receipt, compared to the "explicit" ones sent by the
    /// client.
    pub(super) fn maybe_add_implicit_read_receipt(&mut self, event_meta: FullEventMeta<'_>) {
        let FullEventMeta { event_id, sender, is_own_event, timestamp, .. } = event_meta;

        let (Some(user_id), Some(timestamp)) = (sender, timestamp) else {
            // We cannot add a read receipt if we do not know the user or the timestamp.
            return;
        };

        let receipt = Receipt::new(timestamp);
        let full_receipt =
            FullReceipt { event_id, user_id, receipt_type: ReceiptType::Read, receipt: &receipt };

        self.meta.read_receipts.maybe_update_read_receipt(
            full_receipt,
            is_own_event,
            &self.meta.all_events,
            &mut self.items,
        );
    }

    /// Update the read receipts on the event with the given event ID and the
    /// previous visible event because of a visibility change.
    pub(super) fn maybe_update_read_receipts_of_prev_event(&mut self, event_id: &EventId) {
        // Find the previous visible event, if there is one.
        let Some(prev_event_meta) = self
            .meta
            .all_events
            .iter()
            .rev()
            // Find the event item.
            .skip_while(|meta| meta.event_id != event_id)
            // Go past the event item.
            .skip(1)
            // Find the first visible item.
            .find(|meta| meta.visible)
        else {
            return;
        };

        let Some((prev_item_pos, prev_event_item)) =
            rfind_event_by_id(&self.items, &prev_event_meta.event_id)
        else {
            error!("inconsistent state: timeline item of visible event was not found");
            return;
        };

        let prev_event_item_id = prev_event_item.internal_id.to_owned();
        let mut prev_event_item = prev_event_item.clone();

        let Some(remote_prev_event_item) = prev_event_item.as_remote_mut() else {
            warn!("loading read receipts for a local item, this should not be possible");
            return;
        };

        let read_receipts = self.meta.read_receipts.compute_event_receipts(
            &remote_prev_event_item.event_id,
            &self.meta.all_events,
            false,
        );

        // If the count did not change, the receipts did not change either.
        if read_receipts.len() == remote_prev_event_item.read_receipts.len() {
            return;
        }

        remote_prev_event_item.read_receipts = read_receipts;
        self.items.set(prev_item_pos, TimelineItem::new(prev_event_item, prev_event_item_id));
    }
}

impl TimelineState {
    /// Populates our own latest read receipt in the in-memory by-user read
    /// receipt cache.
    pub(super) async fn populate_initial_user_receipt<P: RoomDataProvider>(
        &mut self,
        room_data_provider: &P,
        receipt_type: ReceiptType,
    ) {
        let own_user_id = room_data_provider.own_user_id().to_owned();

        let mut read_receipt = room_data_provider
            .load_user_receipt(receipt_type.clone(), ReceiptThread::Unthreaded, &own_user_id)
            .await;

        // Fallback to the one in the main thread.
        if read_receipt.is_none() {
            read_receipt = room_data_provider
                .load_user_receipt(receipt_type.clone(), ReceiptThread::Main, &own_user_id)
                .await;
        }

        if let Some(read_receipt) = read_receipt {
            self.meta.read_receipts.upsert_latest(own_user_id, receipt_type, read_receipt);
        }
    }

    /// Get the latest read receipt for the given user.
    ///
    /// Useful to get the latest read receipt, whether it's private or public.
    pub(super) async fn latest_user_read_receipt<P: RoomDataProvider>(
        &self,
        user_id: &UserId,
        room_data_provider: &P,
    ) -> Option<(OwnedEventId, Receipt)> {
        let public_read_receipt =
            self.meta.user_receipt(user_id, ReceiptType::Read, room_data_provider).await;
        let private_read_receipt =
            self.meta.user_receipt(user_id, ReceiptType::ReadPrivate, room_data_provider).await;

        // Let's assume that a private read receipt should be more recent than a public
        // read receipt, otherwise there's no point in the private read receipt,
        // and use it as default.
        match self
            .meta
            .compare_optional_receipts(public_read_receipt.as_ref(), private_read_receipt.as_ref())
        {
            Ordering::Greater => public_read_receipt,
            Ordering::Less => private_read_receipt,
            _ => unreachable!(),
        }
    }

    /// Get the ID of the visible timeline event with the latest read receipt
    /// for the given user.
    pub(super) fn latest_user_read_receipt_timeline_event_id(
        &self,
        user_id: &UserId,
    ) -> Option<OwnedEventId> {
        // We only need to use the local map, since receipts for known events are
        // already loaded from the store.
        let public_read_receipt = self.meta.read_receipts.get_latest(user_id, &ReceiptType::Read);
        let private_read_receipt =
            self.meta.read_receipts.get_latest(user_id, &ReceiptType::ReadPrivate);

        // Let's assume that a private read receipt should be more recent than a public
        // read receipt, otherwise there's no point in the private read receipt,
        // and use it as default.
        let (latest_receipt_id, _) =
            match self.meta.compare_optional_receipts(public_read_receipt, private_read_receipt) {
                Ordering::Greater => public_read_receipt?,
                Ordering::Less => private_read_receipt?,
                _ => unreachable!(),
            };

        // Find the corresponding visible event.
        self.meta
            .all_events
            .iter()
            .rev()
            .skip_while(|ev| ev.event_id != *latest_receipt_id)
            .find(|ev| ev.visible)
            .map(|ev| ev.event_id.clone())
    }
}

impl TimelineMetadata {
    /// Get the latest receipt of the given type for the given user in the
    /// timeline.
    ///
    /// This will attempt to read the latest user receipt for a user from the
    /// cache, or load it from the storage if missing from the cache.
    pub(super) async fn user_receipt<P: RoomDataProvider>(
        &self,
        user_id: &UserId,
        receipt_type: ReceiptType,
        room_data_provider: &P,
    ) -> Option<(OwnedEventId, Receipt)> {
        if let Some(receipt) = self.read_receipts.get_latest(user_id, &receipt_type) {
            // Since it is in the timeline, it should be the most recent.
            return Some(receipt.clone());
        }

        let unthreaded_read_receipt = room_data_provider
            .load_user_receipt(receipt_type.clone(), ReceiptThread::Unthreaded, user_id)
            .await;

        let main_thread_read_receipt = room_data_provider
            .load_user_receipt(receipt_type.clone(), ReceiptThread::Main, user_id)
            .await;

        // Let's use the unthreaded read receipt as default, since it's the one we
        // should be using.
        match self.compare_optional_receipts(
            main_thread_read_receipt.as_ref(),
            unthreaded_read_receipt.as_ref(),
        ) {
            Ordering::Greater => main_thread_read_receipt,
            Ordering::Less => unthreaded_read_receipt,
            _ => unreachable!(),
        }
    }

    /// Compares two optional receipts to know which one is more recent.
    ///
    /// Returns `Ordering::Greater` if the left-hand side is more recent than
    /// the right-hand side, and `Ordering::Less` if it is older. If it's
    /// not possible to know which one is the more recent, defaults to
    /// `Ordering::Less`, making the right-hand side the default.
    fn compare_optional_receipts(
        &self,
        lhs: Option<&(OwnedEventId, Receipt)>,
        rhs_or_default: Option<&(OwnedEventId, Receipt)>,
    ) -> Ordering {
        // If we only have one, use it.
        let Some((lhs_event_id, lhs_receipt)) = lhs else {
            return Ordering::Less;
        };
        let Some((rhs_event_id, rhs_receipt)) = rhs_or_default else {
            return Ordering::Greater;
        };

        // Compare by position in the timeline.
        if let Some(relative_pos) = self.compare_events_positions(lhs_event_id, rhs_event_id) {
            if relative_pos == RelativePosition::Before {
                return Ordering::Greater;
            }

            return Ordering::Less;
        }

        // Compare by timestamp.
        if let Some((lhs_ts, rhs_ts)) = lhs_receipt.ts.zip(rhs_receipt.ts) {
            if lhs_ts > rhs_ts {
                return Ordering::Greater;
            }

            return Ordering::Less;
        }

        Ordering::Less
    }
}
