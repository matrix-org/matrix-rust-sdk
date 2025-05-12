// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use std::collections::{HashMap, HashSet};

use eyeball_im::VectorDiff;
use itertools::Itertools as _;
use matrix_sdk::deserialized_responses::TimelineEvent;
use ruma::{push::Action, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId};
use tracing::{debug, instrument, warn};

use super::{
    super::{
        controller::{FullEventMeta, ObservableItemsTransactionEntry},
        date_dividers::DateDividerAdjuster,
        event_handler::{
            Flow, HandleEventResult, TimelineEventContext, TimelineEventHandler, TimelineEventKind,
            TimelineItemPosition,
        },
        event_item::RemoteEventOrigin,
        traits::RoomDataProvider,
    },
    ObservableItems, ObservableItemsTransaction, TimelineFocusKind, TimelineMetadata,
    TimelineSettings,
};
use crate::{events::SyncTimelineEventWithoutContent, timeline::VirtualTimelineItem};

pub(in crate::timeline) struct TimelineStateTransaction<'a> {
    /// A vector transaction over the items themselves. Holds temporary state
    /// until committed.
    pub items: ObservableItemsTransaction<'a>,

    /// Number of items when the transaction has been created/has started.
    number_of_items_when_transaction_started: usize,

    /// A clone of the previous meta, that we're operating on during the
    /// transaction, and that will be committed to the previous meta location in
    /// [`Self::commit`].
    pub meta: TimelineMetadata,

    /// Pointer to the previous meta, only used during [`Self::commit`].
    previous_meta: &'a mut TimelineMetadata,

    /// The kind of focus of this timeline.
    pub(super) timeline_focus: TimelineFocusKind,
}

impl<'a> TimelineStateTransaction<'a> {
    /// Create a new [`TimelineStateTransaction`].
    pub(super) fn new(
        items: &'a mut ObservableItems,
        meta: &'a mut TimelineMetadata,
        timeline_focus: TimelineFocusKind,
    ) -> Self {
        let previous_meta = meta;
        let meta = previous_meta.clone();
        let items = items.transaction();

        Self {
            number_of_items_when_transaction_started: items.len(),
            items,
            previous_meta,
            meta,
            timeline_focus,
        }
    }

    /// Handle updates on events as [`VectorDiff`]s.
    pub(super) async fn handle_remote_events_with_diffs<RoomData>(
        &mut self,
        diffs: Vec<VectorDiff<TimelineEvent>>,
        origin: RemoteEventOrigin,
        room_data_provider: &RoomData,
        settings: &TimelineSettings,
    ) where
        RoomData: RoomDataProvider,
    {
        let mut date_divider_adjuster =
            DateDividerAdjuster::new(settings.date_divider_mode.clone());

        for diff in diffs {
            match diff {
                VectorDiff::Append { values: events } => {
                    for event in events {
                        self.handle_remote_event(
                            event,
                            TimelineItemPosition::End { origin },
                            room_data_provider,
                            settings,
                            &mut date_divider_adjuster,
                        )
                        .await;
                    }
                }

                VectorDiff::PushFront { value: event } => {
                    self.handle_remote_event(
                        event,
                        TimelineItemPosition::Start { origin },
                        room_data_provider,
                        settings,
                        &mut date_divider_adjuster,
                    )
                    .await;
                }

                VectorDiff::PushBack { value: event } => {
                    self.handle_remote_event(
                        event,
                        TimelineItemPosition::End { origin },
                        room_data_provider,
                        settings,
                        &mut date_divider_adjuster,
                    )
                    .await;
                }

                VectorDiff::Insert { index: event_index, value: event } => {
                    self.handle_remote_event(
                        event,
                        TimelineItemPosition::At { event_index, origin },
                        room_data_provider,
                        settings,
                        &mut date_divider_adjuster,
                    )
                    .await;
                }

                VectorDiff::Set { index: event_index, value: event } => {
                    if let Some(timeline_item_index) = self
                        .items
                        .all_remote_events()
                        .get(event_index)
                        .and_then(|meta| meta.timeline_item_index)
                    {
                        self.handle_remote_event(
                            event,
                            TimelineItemPosition::UpdateAt { timeline_item_index },
                            room_data_provider,
                            settings,
                            &mut date_divider_adjuster,
                        )
                        .await;
                    } else {
                        warn!(event_index, "Set update dropped because there wasn't any attached timeline item index.");
                    }
                }

                VectorDiff::Remove { index: event_index } => {
                    self.remove_timeline_item(event_index, &mut date_divider_adjuster);
                }

                VectorDiff::Clear => {
                    self.clear();
                }

                v => unimplemented!("{v:?}"),
            }
        }

        self.adjust_date_dividers(date_divider_adjuster);
        self.check_invariants();
    }

    fn check_invariants(&self) {
        self.check_no_duplicate_read_receipts();
        self.check_no_unused_unique_ids();
    }

    fn check_no_duplicate_read_receipts(&self) {
        let mut by_user_id = HashMap::new();
        let mut duplicates = HashSet::new();

        for item in self.items.iter_remotes_region().filter_map(|(_, item)| item.as_event()) {
            if let Some(event_id) = item.event_id() {
                for (user_id, _read_receipt) in item.read_receipts() {
                    if let Some(prev_event_id) = by_user_id.insert(user_id, event_id) {
                        duplicates.insert((user_id.clone(), prev_event_id, event_id));
                    }
                }
            }
        }

        if !duplicates.is_empty() {
            #[cfg(any(debug_assertions, test))]
            panic!("duplicate read receipts in this timeline:{:?}\n{:?}", duplicates, self.items);

            #[cfg(not(any(debug_assertions, test)))]
            tracing::error!(
                "duplicate read receipts in this timeline:{:?}\n{:?}",
                duplicates,
                self.items
            );
        }
    }

    fn check_no_unused_unique_ids(&self) {
        let duplicates = self
            .items
            .iter_all_regions()
            .duplicates_by(|(_nth, item)| item.unique_id())
            .map(|(_nth, item)| item.unique_id())
            .collect::<Vec<_>>();

        if !duplicates.is_empty() {
            #[cfg(any(debug_assertions, test))]
            panic!("duplicate unique ids in this timeline:{:?}\n{:?}", duplicates, self.items);

            #[cfg(not(any(debug_assertions, test)))]
            tracing::error!(
                "duplicate unique ids in this timeline:{:?}\n{:?}",
                duplicates,
                self.items
            );
        }
    }

    /// Handle a remote event.
    ///
    /// Returns the number of timeline updates that were made.
    pub(super) async fn handle_remote_event<P: RoomDataProvider>(
        &mut self,
        event: TimelineEvent,
        position: TimelineItemPosition,
        room_data_provider: &P,
        settings: &TimelineSettings,
        date_divider_adjuster: &mut DateDividerAdjuster,
    ) -> HandleEventResult {
        let TimelineEvent { push_actions, kind } = event;
        let encryption_info = kind.encryption_info().cloned();

        let (raw, utd_info) = match kind {
            matrix_sdk::deserialized_responses::TimelineEventKind::UnableToDecrypt {
                utd_info,
                event,
            } => (event, Some(utd_info)),
            _ => (kind.into_raw(), None),
        };

        let (event_id, sender, timestamp, txn_id, event_kind, should_add) = match raw.deserialize()
        {
            // Classical path: the event is valid, can be deserialized, everything is alright.
            Ok(event) => {
                let event_id = event.event_id().to_owned();
                let room_version = room_data_provider.room_version();

                let mut should_add = (settings.event_filter)(&event, &room_version);

                if should_add {
                    // Retrieve the origin of the event.
                    let origin = match position {
                        TimelineItemPosition::End { origin }
                        | TimelineItemPosition::Start { origin }
                        | TimelineItemPosition::At { origin, .. } => origin,

                        TimelineItemPosition::UpdateAt { timeline_item_index: idx } => self
                            .items
                            .get(idx)
                            .and_then(|item| item.as_event())
                            .and_then(|item| item.as_remote())
                            .map_or(RemoteEventOrigin::Unknown, |item| item.origin),
                    };

                    // If the event should be added according to the general event filter, use a
                    // second filter to decide whether it should be added depending on the timeline
                    // focus and events origin, if needed
                    match self.timeline_focus {
                        TimelineFocusKind::PinnedEvents => {
                            // Only add pinned events for the pinned events timeline
                            should_add = room_data_provider.is_pinned_event(&event_id);
                        }
                        TimelineFocusKind::Live => {
                            match origin {
                                RemoteEventOrigin::Sync | RemoteEventOrigin::Unknown => {
                                    // Always add new items to a live timeline receiving items from
                                    // sync.
                                    should_add = true;
                                }
                                RemoteEventOrigin::Cache | RemoteEventOrigin::Pagination => {
                                    // Forward the previous decision to add it.
                                }
                            }
                        }
                        TimelineFocusKind::Event => {
                            match origin {
                                RemoteEventOrigin::Sync | RemoteEventOrigin::Unknown => {
                                    // Never add any item to a focused timeline when the item comes
                                    // down from the sync.
                                    should_add = false;
                                }
                                RemoteEventOrigin::Cache | RemoteEventOrigin::Pagination => {
                                    // Forward the previous decision to add it.
                                }
                            }
                        }
                    }
                }

                (
                    event_id,
                    event.sender().to_owned(),
                    event.origin_server_ts(),
                    event.transaction_id().map(ToOwned::to_owned),
                    TimelineEventKind::from_event(
                        event,
                        &raw,
                        room_data_provider,
                        utd_info,
                        &mut self.meta,
                    )
                    .await,
                    should_add,
                )
            }

            // The event seems invalid…
            Err(e) => match raw.deserialize_as::<SyncTimelineEventWithoutContent>() {
                // The event can be partially deserialized, and it is allowed to be added to the
                // timeline.
                Ok(event) if settings.add_failed_to_parse => (
                    event.event_id().to_owned(),
                    event.sender().to_owned(),
                    event.origin_server_ts(),
                    event.transaction_id().map(ToOwned::to_owned),
                    Some(TimelineEventKind::failed_to_parse(event, e)),
                    true,
                ),

                // The event can be partially deserialized, but it is NOT allowed to be added to
                // the timeline.
                Ok(event) => {
                    let event_type = event.event_type();
                    let event_id = event.event_id();
                    warn!(%event_type, %event_id, "Failed to deserialize timeline event: {e}");

                    let event_meta = FullEventMeta {
                        event_id,
                        sender: Some(event.sender()),
                        timestamp: Some(event.origin_server_ts()),
                        visible: false,
                    };

                    // Remember the event before returning prematurely.
                    // See [`ObservableItems::all_remote_events`].
                    self.add_or_update_remote_event(
                        event_meta,
                        position,
                        room_data_provider,
                        settings,
                    )
                    .await;

                    return HandleEventResult::default();
                }

                // The event can NOT be partially deserialized, it seems really broken.
                Err(e) => {
                    let event_type: Option<String> = raw.get_field("type").ok().flatten();
                    let event_id: Option<String> = raw.get_field("event_id").ok().flatten();
                    warn!(
                        event_type,
                        event_id, "Failed to deserialize timeline event even without content: {e}"
                    );

                    let event_id = event_id.and_then(|s| EventId::parse(s).ok());

                    if let Some(event_id) = &event_id {
                        let sender: Option<OwnedUserId> = raw.get_field("sender").ok().flatten();
                        let timestamp: Option<MilliSecondsSinceUnixEpoch> =
                            raw.get_field("origin_server_ts").ok().flatten();

                        let event_meta = FullEventMeta {
                            event_id,
                            sender: sender.as_deref(),
                            timestamp,
                            visible: false,
                        };

                        // Remember the event before returning prematurely.
                        // See [`ObservableItems::all_remote_events`].
                        self.add_or_update_remote_event(
                            event_meta,
                            position,
                            room_data_provider,
                            settings,
                        )
                        .await;
                    }

                    return HandleEventResult::default();
                }
            },
        };

        let event_meta = FullEventMeta {
            event_id: &event_id,
            sender: Some(&sender),
            timestamp: Some(timestamp),
            visible: should_add,
        };

        // Remember the event.
        // See [`ObservableItems::all_remote_events`].
        self.add_or_update_remote_event(event_meta, position, room_data_provider, settings).await;

        let sender_profile = room_data_provider.profile_from_user_id(&sender).await;
        let ctx = TimelineEventContext {
            sender,
            sender_profile,
            timestamp,
            read_receipts: if settings.track_read_receipts && should_add {
                self.meta.read_receipts.compute_event_receipts(
                    &event_id,
                    &mut self.items,
                    matches!(position, TimelineItemPosition::End { .. }),
                )
            } else {
                Default::default()
            },
            is_highlighted: push_actions
                .as_ref()
                .is_some_and(|actions| actions.iter().any(Action::is_highlight)),
            flow: Flow::Remote {
                event_id: event_id.clone(),
                raw_event: raw,
                encryption_info,
                txn_id,
                position,
            },
            should_add_new_items: should_add,
        };

        // Handle the event to create or update a timeline item.
        if let Some(event_kind) = event_kind {
            TimelineEventHandler::new(self, ctx)
                .handle_event(date_divider_adjuster, event_kind)
                .await
        } else {
            HandleEventResult::default()
        }
    }

    /// Remove one timeline item by its `event_index`.
    fn remove_timeline_item(
        &mut self,
        event_index: usize,
        day_divider_adjuster: &mut DateDividerAdjuster,
    ) {
        day_divider_adjuster.mark_used();

        // We need to be careful here.
        //
        // We must first remove the timeline item, which will update the mapping between
        // remote events and timeline items. Removing the timeline item will “unlink”
        // this mapping as the remote event will be updated to map to nothing. Only
        // after that, we can remove the remote event. Doing this in the other order
        // will update the mapping twice, and will result in a corrupted state.

        // Remove the timeline item first.
        if let Some(event_meta) = self.items.all_remote_events().get(event_index) {
            // Fetch the `timeline_item_index` associated to the remote event.
            if let Some(timeline_item_index) = event_meta.timeline_item_index {
                let _ = self.items.remove(timeline_item_index);
            }

            // Now we can remove the remote event.
            self.items.remove_remote_event(event_index);
        }
    }

    pub(super) fn clear(&mut self) {
        // By first checking if there are any local echoes first, we do a bit
        // more work in case some are found, but it should be worth it because
        // there will often not be any, and only emitting a single
        // `VectorDiff::Clear` should be much more efficient to process for
        // subscribers.
        if self.items.has_local() {
            // Remove all remote events and virtual items that aren't date dividers.
            self.items.for_each(|entry| {
                if entry.is_remote_event()
                    || entry.as_virtual().is_some_and(|vitem| match vitem {
                        VirtualTimelineItem::DateDivider(_) => false,
                        VirtualTimelineItem::ReadMarker | VirtualTimelineItem::TimelineStart => {
                            true
                        }
                    })
                {
                    ObservableItemsTransactionEntry::remove(entry);
                }
            });

            // Remove stray date dividers
            let mut idx = 0;
            while idx < self.items.len() {
                if self.items[idx].is_date_divider()
                    && self.items.get(idx + 1).is_none_or(|item| item.is_date_divider())
                {
                    self.items.remove(idx);
                    // don't increment idx because all elements have shifted
                } else {
                    idx += 1;
                }
            }
        } else {
            self.items.clear();
        }

        self.meta.clear();

        debug!(remaining_items = self.items.len(), "Timeline cleared");
    }

    #[instrument(skip_all)]
    pub(super) fn set_fully_read_event(&mut self, fully_read_event_id: OwnedEventId) {
        // A similar event has been handled already. We can ignore it.
        if self.meta.fully_read_event.as_ref().is_some_and(|id| *id == fully_read_event_id) {
            return;
        }

        self.meta.fully_read_event = Some(fully_read_event_id);
        self.meta.update_read_marker(&mut self.items);
    }

    pub(super) fn commit(self) {
        // Update the `subscriber_skip_count` value.
        let previous_number_of_items = self.number_of_items_when_transaction_started;
        let next_number_of_items = self.items.len();

        if previous_number_of_items != next_number_of_items {
            let count = self
                .meta
                .subscriber_skip_count
                .compute_next(previous_number_of_items, next_number_of_items);
            self.meta.subscriber_skip_count.update(count, &self.timeline_focus);
        }

        // Replace the pointer to the previous meta with the new one.
        *self.previous_meta = self.meta;

        self.items.commit();
    }

    /// Add or update a remote event in the
    /// [`ObservableItems::all_remote_events`] collection.
    ///
    /// This method also adjusts read receipt if needed.
    async fn add_or_update_remote_event<P: RoomDataProvider>(
        &mut self,
        event_meta: FullEventMeta<'_>,
        position: TimelineItemPosition,
        room_data_provider: &P,
        settings: &TimelineSettings,
    ) {
        match position {
            TimelineItemPosition::Start { .. } => {
                self.items.push_front_remote_event(event_meta.base_meta())
            }

            TimelineItemPosition::End { .. } => {
                self.items.push_back_remote_event(event_meta.base_meta());
            }

            TimelineItemPosition::At { event_index, .. } => {
                self.items.insert_remote_event(event_index, event_meta.base_meta());
            }

            TimelineItemPosition::UpdateAt { .. } => {
                if let Some(event) =
                    self.items.get_remote_event_by_event_id_mut(event_meta.event_id)
                {
                    if event.visible != event_meta.visible {
                        event.visible = event_meta.visible;

                        if settings.track_read_receipts {
                            // Since the event's visibility changed, we need to update the read
                            // receipts of the previous visible event.
                            self.maybe_update_read_receipts_of_prev_event(event_meta.event_id);
                        }
                    }
                }
            }
        }

        if settings.track_read_receipts
            && matches!(
                position,
                TimelineItemPosition::Start { .. }
                    | TimelineItemPosition::End { .. }
                    | TimelineItemPosition::At { .. }
            )
        {
            self.load_read_receipts_for_event(event_meta.event_id, room_data_provider).await;

            self.maybe_add_implicit_read_receipt(event_meta);
        }
    }

    pub(super) fn adjust_date_dividers(&mut self, mut adjuster: DateDividerAdjuster) {
        adjuster.run(&mut self.items, &mut self.meta);
    }

    /// This method replaces the `is_room_encrypted` value for all timeline
    /// items to its updated version and creates a `VectorDiff::Set` operation
    /// for each item which will be added to this transaction.
    pub(super) fn mark_all_events_as_encrypted(&mut self) {
        for idx in 0..self.items.len() {
            let item = &self.items[idx];

            if let Some(event) = item.as_event() {
                if event.is_room_encrypted {
                    continue;
                }

                let mut cloned_event = event.clone();
                cloned_event.is_room_encrypted = true;

                // Replace the existing item with a new version with the right encryption flag
                let item = item.with_kind(cloned_event);
                self.items.replace(idx, item);
            }
        }
    }
}
