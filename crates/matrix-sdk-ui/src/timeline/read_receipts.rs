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

use std::{collections::HashMap, sync::Arc};

use eyeball_im::ObservableVectorTransaction;
use indexmap::IndexMap;
use matrix_sdk::Room;
use ruma::{
    events::receipt::{Receipt, ReceiptEventContent, ReceiptThread, ReceiptType},
    EventId, OwnedEventId, OwnedUserId, UserId,
};
use tracing::{error, warn};

use super::{
    inner::{
        FullEventMeta, TimelineInnerMetadata, TimelineInnerState, TimelineInnerStateTransaction,
    },
    item::timeline_item,
    traits::RoomDataProvider,
    util::{rfind_event_by_id, RelativePosition},
    TimelineItem,
};

#[derive(Clone, Debug, Default)]
pub(super) struct ReadReceipts {
    /// Map of public read receipts on events.
    ///
    /// Event ID => User ID => Read receipt of the user.
    pub events_read_receipts: HashMap<OwnedEventId, IndexMap<OwnedUserId, Receipt>>,
    /// Map of all user read receipts.
    ///
    /// User ID => Receipt type => Read receipt of the user of the given
    /// type.
    pub users_read_receipts: HashMap<OwnedUserId, HashMap<ReceiptType, (OwnedEventId, Receipt)>>,
}

impl ReadReceipts {
    /// Remove all data.
    pub(super) fn clear(&mut self) {
        self.users_read_receipts.clear();
    }

    /// Update the timeline items with the given read receipt if it is more
    /// recent than the current one.
    ///
    /// In the process, this method removes the corresponding receipt from its
    /// old item, if applicable, and updates the `users_read_receipts` map
    /// to use the new receipt.
    ///
    /// Returns true if the read receipt was saved.
    ///
    /// Currently this method only works reliably if the timeline was started
    /// from the end of the timeline.
    fn maybe_update_read_receipt(
        &mut self,
        receipt: FullReceipt<'_>,
        new_item_pos: Option<usize>,
        is_own_user_id: bool,
        timeline_items: &mut ObservableVectorTransaction<'_, Arc<TimelineItem>>,
    ) -> bool {
        let old_event_id = self
            .users_read_receipts
            .get(receipt.user_id)
            .and_then(|receipts| receipts.get(&receipt.receipt_type))
            .map(|(event_id, _)| event_id);
        if old_event_id.is_some_and(|id| id == receipt.event_id) {
            // Nothing to do.
            return false;
        }

        let old_item_and_pos = old_event_id.and_then(|e| rfind_event_by_id(timeline_items, e));
        if let Some((old_receipt_pos, old_event_item)) = old_item_and_pos {
            let Some(new_receipt_pos) = new_item_pos else {
                // The old receipt is likely more recent since we can't find the
                // event of the new receipt in the timeline. Even if it isn't, we
                // wouldn't know where to put it.
                return false;
            };

            if old_receipt_pos > new_receipt_pos {
                // The old receipt is more recent than the new one.
                return false;
            }

            if !is_own_user_id {
                // Remove the read receipt for this user from the old event item.
                let old_event_item_id = old_event_item.internal_id;
                let mut old_event_item = old_event_item.clone();
                if let Some(old_remote_event_item) = old_event_item.as_remote_mut() {
                    if !old_remote_event_item.remove_read_receipt(receipt.user_id) {
                        error!(
                            "inconsistent state: old event item for user's read \
                         receipt doesn't have a receipt for the user"
                        );
                    }
                    timeline_items
                        .set(old_receipt_pos, timeline_item(old_event_item, old_event_item_id));
                } else {
                    warn!("received a read receipt for a local item, this should not be possible");
                }
            }
        }

        // The new receipt is deemed more recent from now on because:
        // - If old_receipt_item is Some, we already checked all the cases where it
        //   wouldn't be more recent.
        // - If both old_receipt_item and new_receipt_item are None, they are both
        //   explicit read receipts so the server should only send us a more recent
        //   receipt.
        // - If old_receipt_item is None and new_receipt_item is Some, the new receipt
        //   is likely more recent because it has a place in the timeline.

        // Remove the old receipt from the old event.
        if let Some(old_event_id) = old_event_id {
            if let Some(event_receipts) = self.events_read_receipts.get_mut(old_event_id) {
                event_receipts.remove(receipt.user_id);

                // Remove the entry if the map is empty.
                if event_receipts.is_empty() {
                    self.events_read_receipts.remove(old_event_id);
                }
            };
        }

        // Add the new receipt to the new event.
        self.events_read_receipts
            .entry(receipt.event_id.to_owned())
            .or_default()
            .insert(receipt.user_id.to_owned(), receipt.receipt.clone());

        // Update the receipt of the user.
        self.users_read_receipts
            .entry(receipt.user_id.to_owned())
            .or_default()
            .insert(receipt.receipt_type, (receipt.event_id.to_owned(), receipt.receipt.clone()));

        true
    }

    /// Get the user read receipts for the given event.
    pub(super) fn read_receipts_for_event(
        &self,
        event_id: &EventId,
    ) -> IndexMap<OwnedUserId, Receipt> {
        self.events_read_receipts.get(event_id).cloned().unwrap_or_default()
    }
}

struct FullReceipt<'a> {
    event_id: &'a EventId,
    user_id: &'a UserId,
    receipt_type: ReceiptType,
    receipt: &'a Receipt,
}

impl TimelineInnerStateTransaction<'_> {
    /// Update the new item pointed to by the user's read receipt.
    fn add_read_receipt(
        &mut self,
        receipt_item_pos: Option<usize>,
        user_id: OwnedUserId,
        receipt: Receipt,
    ) {
        let Some(pos) = receipt_item_pos else { return };
        let timeline_item = self.items[pos].clone();
        let Some(mut event_item) = timeline_item.as_event().cloned() else { return };

        event_item.as_remote_mut().unwrap().add_read_receipt(user_id, receipt);
        self.items.set(pos, timeline_item.with_kind(event_item));
    }

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
                    if receipt.thread != ReceiptThread::Unthreaded {
                        continue;
                    }

                    let receipt_item_pos =
                        rfind_event_by_id(&self.items, &event_id).map(|(pos, _)| pos);
                    let is_own_user_id = user_id == own_user_id;
                    let full_receipt = FullReceipt {
                        event_id: &event_id,
                        user_id: &user_id,
                        receipt_type: receipt_type.clone(),
                        receipt: &receipt,
                    };

                    let read_receipt_updated = self.meta.read_receipts.maybe_update_read_receipt(
                        full_receipt,
                        receipt_item_pos,
                        is_own_user_id,
                        &mut self.items,
                    );

                    if read_receipt_updated && !is_own_user_id {
                        self.add_read_receipt(receipt_item_pos, user_id, receipt);
                    }
                }
            }
        }
    }

    /// Load the read receipts from the store for the given event ID.
    ///
    /// Populates the read receipts maps.
    pub(super) async fn load_read_receipts_for_event<P: RoomDataProvider>(
        &mut self,
        event_id: &EventId,
        room_data_provider: &P,
    ) {
        let read_receipts = room_data_provider.read_receipts_for_event(event_id).await;

        // Filter out receipts for our own user.
        let own_user_id = room_data_provider.own_user_id();
        let read_receipts: IndexMap<OwnedUserId, Receipt> =
            read_receipts.into_iter().filter(|(user_id, _)| user_id != own_user_id).collect();

        // Since they are explicit read receipts, we need to check if they are
        // superseded by implicit read receipts.
        let own_user_id = room_data_provider.own_user_id();
        for (user_id, receipt) in read_receipts {
            let full_receipt = FullReceipt {
                event_id,
                user_id: &user_id,
                receipt_type: ReceiptType::Read,
                receipt: &receipt,
            };
            self.meta.read_receipts.maybe_update_read_receipt(
                full_receipt,
                None,
                user_id == own_user_id,
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
            None,
            is_own_event,
            &mut self.items,
        );
    }
}

impl TimelineInnerState {
    /// Get the latest read receipt for the given user.
    ///
    /// Useful to get the latest read receipt, whether it's private or public.
    pub(super) async fn latest_user_read_receipt(
        &self,
        user_id: &UserId,
        room: &Room,
    ) -> Option<(OwnedEventId, Receipt)> {
        let public_read_receipt = self.user_receipt(user_id, ReceiptType::Read, room).await;
        let private_read_receipt = self.user_receipt(user_id, ReceiptType::ReadPrivate, room).await;

        // If we only have one, return it.
        let Some((pub_event_id, pub_receipt)) = &public_read_receipt else {
            return private_read_receipt;
        };
        let Some((priv_event_id, priv_receipt)) = &private_read_receipt else {
            return public_read_receipt;
        };

        // Compare by position in the timeline.
        if let Some(relative_pos) = self.meta.compare_events_positions(pub_event_id, priv_event_id)
        {
            if relative_pos == RelativePosition::After {
                return private_read_receipt;
            }

            return public_read_receipt;
        }

        // Compare by timestamp.
        if let Some((pub_ts, priv_ts)) = pub_receipt.ts.zip(priv_receipt.ts) {
            if priv_ts > pub_ts {
                return private_read_receipt;
            }

            return public_read_receipt;
        }

        // As a fallback, let's assume that a private read receipt should be more recent
        // than a public read receipt, otherwise there's no point in the private read
        // receipt.
        private_read_receipt
    }
}

impl TimelineInnerMetadata {
    /// Get the unthreaded receipt of the given type for the given user in the
    /// timeline.
    pub(super) async fn user_receipt(
        &self,
        user_id: &UserId,
        receipt_type: ReceiptType,
        room: &Room,
    ) -> Option<(OwnedEventId, Receipt)> {
        if let Some(receipt) = self
            .read_receipts
            .users_read_receipts
            .get(user_id)
            .and_then(|user_map| user_map.get(&receipt_type))
            .cloned()
        {
            return Some(receipt);
        }

        room.user_receipt(receipt_type.clone(), ReceiptThread::Unthreaded, user_id)
            .await
            .unwrap_or_else(|e| {
                error!("Could not get user read receipt of type {receipt_type:?}: {e}");
                None
            })
    }
}
