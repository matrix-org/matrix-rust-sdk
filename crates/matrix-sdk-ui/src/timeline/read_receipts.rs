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
    collections::{HashMap, VecDeque},
    sync::Arc,
};

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
        EventMeta, FullEventMeta, TimelineInnerMetadata, TimelineInnerState,
        TimelineInnerStateTransaction,
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
    pub users_read_receipts: HashMap<OwnedUserId, HashMap<ReceiptType, UserTimelineReceipt>>,
}

impl ReadReceipts {
    /// Remove all data.
    pub(super) fn clear(&mut self) {
        self.events_read_receipts.clear();
        self.users_read_receipts.clear();
    }

    /// Update the timeline items with the given read receipt if it is more
    /// recent than the current one.
    ///
    /// In the process,if applicable, this method updates the
    /// `users_read_receipts` map to use the new receipt. If
    /// `update_old_item` is `true`, it also removes the corresponding
    /// receipt from its old item.
    ///
    /// Returns the event ID of the timeline item on which the receipt should
    /// appear, if the receipt was updated.
    ///
    /// Currently this method only works reliably if the timeline was started
    /// from the end of the timeline.
    fn maybe_update_read_receipt(
        &mut self,
        new_receipt: FullReceipt<'_>,
        is_own_user: bool,
        all_events: &VecDeque<EventMeta>,
    ) -> ReadReceiptTimelineUpdate {
        // Get old receipt.
        let old_receipt = self
            .users_read_receipts
            .get(new_receipt.user_id)
            .and_then(|receipts| receipts.get(&new_receipt.receipt_type));
        if old_receipt.is_some_and(|old_receipt| old_receipt.event_id == new_receipt.event_id) {
            // The receipt has not changed so there is nothing to do.
            return ReadReceiptTimelineUpdate::default();
        }
        let old_event_id = old_receipt.map(|r| &r.event_id);
        let old_item_event_id = old_receipt.and_then(|r| r.timeline_event_id.clone());

        // Find receipts positions.
        let mut old_receipt_pos = None;
        let mut new_receipt_pos = None;
        let mut new_item_event_id = None;
        for (pos, event) in all_events.iter().rev().enumerate() {
            if old_event_id == Some(&event.event_id) {
                old_receipt_pos = Some(pos);
            }

            if new_receipt.event_id == event.event_id {
                new_receipt_pos = Some(pos);
            }
            // The receipt should appear on the first event that is visible.
            if new_receipt_pos.is_some() && new_item_event_id.is_none() && event.visible {
                new_item_event_id = Some(event.event_id.clone());
            }

            if new_receipt_pos.is_some()
                && new_item_event_id.is_some()
                && (old_receipt.is_none() || old_receipt_pos.is_some())
            {
                // We have everything we need, stop.
                break;
            }
        }

        // Check if the old receipt is more recent than the new receipt.
        if let Some(old_receipt_pos) = old_receipt_pos {
            let Some(new_receipt_pos) = new_receipt_pos else {
                // The old receipt is more recent since we can't find the new receipt in the
                // timeline and we supposedly have all events since the end of the timeline.
                return ReadReceiptTimelineUpdate::default();
            };

            if old_receipt_pos < new_receipt_pos {
                // The old receipt is more recent than the new one.
                return ReadReceiptTimelineUpdate::default();
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

        if !is_own_user {
            // Remove the old receipt from the old event.
            if let Some(old_event_id) = old_event_id {
                let is_empty =
                    if let Some(event_receipts) = self.events_read_receipts.get_mut(old_event_id) {
                        event_receipts.remove(new_receipt.user_id);
                        event_receipts.is_empty()
                    } else {
                        false
                    };
                // Remove the entry if the map is empty.
                if is_empty {
                    self.events_read_receipts.remove(old_event_id);
                }
            }

            // Add the new receipt to the new event.
            self.events_read_receipts
                .entry(new_receipt.event_id.to_owned())
                .or_default()
                .insert(new_receipt.user_id.to_owned(), new_receipt.receipt.clone());
        }

        // Update the receipt of the user.
        self.users_read_receipts.entry(new_receipt.user_id.to_owned()).or_default().insert(
            new_receipt.receipt_type,
            UserTimelineReceipt {
                event_id: new_receipt.event_id.to_owned(),
                receipt: new_receipt.receipt.clone(),
                timeline_event_id: new_item_event_id.clone(),
            },
        );

        if is_own_user || (new_item_event_id.is_some() && new_item_event_id == old_item_event_id) {
            // The receipt did not change in the timeline.
            return ReadReceiptTimelineUpdate::default();
        }

        ReadReceiptTimelineUpdate {
            old_event_id: old_item_event_id,
            new_event_id: new_item_event_id,
        }
    }

    /// Get the user read receipts for the given event.
    ///
    /// This loads all the receipts on the event and the receipts on the
    /// following events that were discarded.
    #[tracing::instrument(ret)]
    pub(super) fn read_receipts_for_event(
        &self,
        event_id: &EventId,
        all_events: &VecDeque<EventMeta>,
        at_end: bool,
    ) -> IndexMap<OwnedUserId, Receipt> {
        let mut all_receipts = self.events_read_receipts.get(event_id).cloned().unwrap_or_default();

        if at_end {
            // No need to search for extra receipts, there are no events after.
            return all_receipts;
        }

        // Iterate past the event.
        let events_iter = all_events.iter().skip_while(|meta| meta.event_id != event_id).skip(1);

        for hidden_event_meta in events_iter.take_while(|meta| !meta.visible) {
            tracing::debug!("hidden event: {hidden_event_meta:?}");
            let Some(event_receipts) = self.events_read_receipts.get(&hidden_event_meta.event_id)
            else {
                continue;
            };

            all_receipts.extend(event_receipts.iter().map(|(k, v)| (k.clone(), v.clone())));
        }

        all_receipts
    }

    /// Get the unthreaded receipt of the given type for the given user in the
    /// timeline.
    pub(super) async fn user_receipt(
        &self,
        user_id: &UserId,
        receipt_type: ReceiptType,
        room: &Room,
    ) -> Option<(OwnedEventId, Receipt)> {
        if let Some(user_receipt) =
            self.users_read_receipts.get(user_id).and_then(|user_map| user_map.get(&receipt_type))
        {
            return Some((user_receipt.event_id.clone(), user_receipt.receipt.clone()));
        }

        room.user_receipt(receipt_type.clone(), ReceiptThread::Unthreaded, user_id)
            .await
            .unwrap_or_else(|e| {
                error!("Could not get user read receipt of type {receipt_type:?}: {e}");
                None
            })
    }

    /// Set the timeline item on which a user receipt appears.
    pub(super) fn set_user_receipt_timeline_item(
        &mut self,
        user_id: &UserId,
        timeline_event_id: OwnedEventId,
    ) {
        if let Some(user_receipt) = self
            .users_read_receipts
            .get_mut(user_id)
            .and_then(|user_map| user_map.get_mut(&ReceiptType::Read))
        {
            user_receipt.timeline_event_id = Some(timeline_event_id);
        } else {
            error!(%user_id, "inconsistent state: user receipt for timeline item not found");
        }
    }
}

/// The read receipt of a user in the timeline.
#[derive(Clone, Debug)]
pub(super) struct UserTimelineReceipt {
    /// The real event ID of the receipt.
    pub event_id: OwnedEventId,
    /// The content of the receipt.
    pub receipt: Receipt,
    /// The ID of the event on which the receipt appears in the timeline.
    pub timeline_event_id: Option<OwnedEventId>,
}

struct FullReceipt<'a> {
    event_id: &'a EventId,
    user_id: &'a UserId,
    receipt_type: ReceiptType,
    receipt: &'a Receipt,
}

/// A read receipt update in the timeline.
#[derive(Clone, Debug, Default)]
pub(super) struct ReadReceiptTimelineUpdate {
    /// The old event that had the receipt of the user, if any.
    old_event_id: Option<OwnedEventId>,
    /// The new event that has the receipt of the user, if any.
    new_event_id: Option<OwnedEventId>,
}

impl ReadReceiptTimelineUpdate {
    /// Remove the old receipt from the corresponding timeline item.
    fn remove_old_receipt(
        &self,
        items: &mut ObservableVectorTransaction<Arc<TimelineItem>>,
        user_id: &UserId,
    ) {
        let Some(event_id) = &self.old_event_id else {
            // Nothing to do.
            return;
        };

        let Some((receipt_pos, event_item)) = rfind_event_by_id(items, event_id) else {
            error!(%event_id, %user_id, "inconsistent state: old event item for read receipt was not found");
            return;
        };

        let event_item_id = event_item.internal_id;
        let mut event_item = event_item.clone();

        if let Some(remote_event_item) = event_item.as_remote_mut() {
            if remote_event_item.read_receipts.remove(user_id).is_none() {
                error!(
                    %event_id, %user_id,
                    "inconsistent state: old event item for user's read \
                     receipt doesn't have a receipt for the user"
                );
            }
            items.set(receipt_pos, timeline_item(event_item, event_item_id));
        } else {
            warn!("received a read receipt for a local item, this should not be possible");
        }
    }

    /// Add the new receipt to the corresponding timeline item.
    fn add_new_receipt(
        self,
        items: &mut ObservableVectorTransaction<Arc<TimelineItem>>,
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

        let event_item_id = event_item.internal_id;
        let mut event_item = event_item.clone();

        if let Some(remote_event_item) = event_item.as_remote_mut() {
            remote_event_item.read_receipts.insert(user_id, receipt);
            items.set(receipt_pos, timeline_item(event_item, event_item_id));
        } else {
            warn!("received a read receipt for a local item, this should not be possible");
        }
    }

    /// Apply this update to the timeline.
    fn apply(
        self,
        items: &mut ObservableVectorTransaction<Arc<TimelineItem>>,
        user_id: OwnedUserId,
        receipt: Receipt,
    ) {
        self.remove_old_receipt(items, &user_id);
        self.add_new_receipt(items, user_id, receipt);
    }
}

impl TimelineInnerStateTransaction<'_> {
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
                    // For now we only care about receipts in the main thread.
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

                    self.meta
                        .read_receipts
                        .maybe_update_read_receipt(
                            full_receipt,
                            is_own_user_id,
                            &self.meta.all_events,
                        )
                        .apply(&mut self.items, user_id, receipt);
                }
            }
        }
    }

    /// Load the read receipts from the room data provider for the given event
    /// ID.
    ///
    /// Populates the read receipts maps.
    pub(super) async fn load_read_receipts_for_event<P: RoomDataProvider>(
        &mut self,
        event_id: OwnedEventId,
        room_data_provider: &P,
    ) {
        let read_receipts = room_data_provider.read_receipts_for_event(&event_id).await;

        // Since they are explicit read receipts, we need to check if they are
        // superseded by implicit read receipts.
        let own_user_id = room_data_provider.own_user_id();
        for (user_id, receipt) in read_receipts {
            let full_receipt = FullReceipt {
                event_id: &event_id,
                user_id: &user_id,
                receipt_type: ReceiptType::Read,
                receipt: &receipt,
            };
            self.meta
                .read_receipts
                .maybe_update_read_receipt(
                    full_receipt,
                    user_id == own_user_id,
                    &self.meta.all_events,
                )
                .apply(&mut self.items, user_id, receipt);
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
    pub(super) fn maybe_add_implicit_read_receipt(&mut self, event_metadata: FullEventMeta<'_>) {
        let FullEventMeta { event_id, sender, is_own_event, timestamp, .. } = event_metadata;

        let (Some(user_id), Some(timestamp)) = (sender, timestamp) else {
            // We cannot add a read receipt if we do not know the user or the timestamp.
            return;
        };

        let receipt = Receipt::new(timestamp);
        let new_receipt =
            FullReceipt { event_id, user_id, receipt_type: ReceiptType::Read, receipt: &receipt };

        self.meta
            .read_receipts
            .maybe_update_read_receipt(new_receipt, is_own_event, &self.meta.all_events)
            .apply(&mut self.items, user_id.to_owned(), receipt);
    }

    /// Update the read receipts on the event with the given event ID and the
    /// previous visible event because of a visibility change.
    pub(super) fn maybe_update_read_receipts_of_prev_event(&mut self, event_id: &EventId) {
        // Find the previous visible event, if there is one.
        let Some((prev_item_pos, prev_event_item)) = self
            .all_events
            .iter()
            .rev()
            .skip_while(|meta| meta.event_id != event_id)
            .skip(1)
            .find(|meta| meta.visible)
            .and_then(|meta| rfind_event_by_id(&self.items, &meta.event_id))
        else {
            return;
        };

        let prev_event_item_id = prev_event_item.internal_id;
        let mut prev_event_item = prev_event_item.clone();

        if let Some(remote_prev_event_item) = prev_event_item.as_remote_mut() {
            let read_receipts = self.read_receipts.read_receipts_for_event(
                &remote_prev_event_item.event_id,
                &self.all_events,
                false,
            );

            // If the count did not change, the receipts did not change either.
            if read_receipts.len() != remote_prev_event_item.read_receipts.len() {
                for user_id in read_receipts.keys() {
                    self.read_receipts.set_user_receipt_timeline_item(
                        user_id,
                        remote_prev_event_item.event_id.clone(),
                    );
                }

                remote_prev_event_item.read_receipts = read_receipts;
                self.items.set(prev_item_pos, timeline_item(prev_event_item, prev_event_item_id));
            }
        } else {
            warn!("loading read receipts for a local item, this should not be possible");
        }
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
        let public_read_receipt =
            self.read_receipts.user_receipt(user_id, ReceiptType::Read, room).await;
        let private_read_receipt =
            self.read_receipts.user_receipt(user_id, ReceiptType::ReadPrivate, room).await;

        match choose_priv_or_pub_receipt(
            &self.meta,
            private_read_receipt.as_ref().map(|(ev_id, r)| (ev_id, r)),
            public_read_receipt.as_ref().map(|(ev_id, r)| (ev_id, r)),
        )? {
            PrivOrPub::Priv => private_read_receipt,
            PrivOrPub::Pub => public_read_receipt,
        }
    }

    pub(super) fn latest_user_read_receipt_timeline_event_id(
        &self,
        user_id: &UserId,
    ) -> Option<OwnedEventId> {
        let public_read_receipt = self
            .read_receipts
            .users_read_receipts
            .get(user_id)
            .and_then(|m| m.get(&ReceiptType::Read))
            .and_then(|r| r.timeline_event_id.as_ref().map(|ev_id| (ev_id, &r.receipt)));
        let private_read_receipt = self
            .read_receipts
            .users_read_receipts
            .get(user_id)
            .and_then(|m| m.get(&ReceiptType::ReadPrivate))
            .and_then(|r| r.timeline_event_id.as_ref().map(|ev_id| (ev_id, &r.receipt)));

        match choose_priv_or_pub_receipt(&self.meta, private_read_receipt, public_read_receipt)? {
            PrivOrPub::Priv => private_read_receipt.map(|(event_id, _)| event_id.clone()),
            PrivOrPub::Pub => public_read_receipt.map(|(event_id, _)| event_id.clone()),
        }
    }
}

/// Determine whether the private or public read receipt is more recent.
fn choose_priv_or_pub_receipt(
    meta: &TimelineInnerMetadata,
    private: Option<(&OwnedEventId, &Receipt)>,
    public: Option<(&OwnedEventId, &Receipt)>,
) -> Option<PrivOrPub> {
    // If we only have one, return it.
    let Some((pub_event_id, pub_receipt)) = public else {
        if private.is_some() {
            return Some(PrivOrPub::Priv);
        } else {
            return None;
        }
    };
    let Some((priv_event_id, priv_receipt)) = private else {
        return Some(PrivOrPub::Pub);
    };

    // Compare by position in the timeline.
    if let Some(relative_pos) = meta.compare_events_positions(pub_event_id, priv_event_id) {
        if relative_pos == RelativePosition::After {
            return Some(PrivOrPub::Priv);
        }

        return Some(PrivOrPub::Pub);
    }

    // Compare by timestamp.
    if let Some((pub_ts, priv_ts)) = pub_receipt.ts.zip(priv_receipt.ts) {
        if priv_ts > pub_ts {
            return Some(PrivOrPub::Priv);
        }

        return Some(PrivOrPub::Pub);
    }

    // As a fallback, let's assume that a private read receipt should be more recent
    // than a public read receipt, otherwise there's no point in the private read
    // receipt.
    Some(PrivOrPub::Priv)
}

/// Whether to use the public or private receipt.
#[derive(Clone, Copy, Debug)]
enum PrivOrPub {
    /// The private receipt should be used.
    Priv,
    /// The public receipt should be used.
    Pub,
}
