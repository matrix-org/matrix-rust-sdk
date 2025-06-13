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
use matrix_sdk::deserialized_responses::{
    ThreadSummaryStatus, TimelineEvent, TimelineEventKind, UnsignedEventLocation,
};
use ruma::{
    events::AnySyncTimelineEvent, push::Action, serde::Raw, EventId, MilliSecondsSinceUnixEpoch,
    OwnedEventId, OwnedTransactionId, OwnedUserId, UserId,
};
use tracing::{debug, instrument, warn};

use super::{
    super::{
        controller::ObservableItemsTransactionEntry,
        date_dividers::DateDividerAdjuster,
        event_handler::{Flow, TimelineEventContext, TimelineEventHandler, TimelineItemPosition},
        event_item::RemoteEventOrigin,
        traits::RoomDataProvider,
    },
    metadata::EventMeta,
    ObservableItems, ObservableItemsTransaction, TimelineFocusKind, TimelineMetadata,
    TimelineSettings,
};
use crate::timeline::{
    event_handler::{FailedToParseEvent, RemovedItem, TimelineAction},
    EmbeddedEvent, ThreadSummary, TimelineDetails, VirtualTimelineItem,
};

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

    async fn handle_remote_aggregation<RoomData>(
        &mut self,
        event: TimelineEvent,
        position: TimelineItemPosition,
        room_data_provider: &RoomData,
        date_divider_adjuster: &mut DateDividerAdjuster,
    ) where
        RoomData: RoomDataProvider,
    {
        let deserialized = match event.raw().deserialize() {
            Ok(deserialized) => deserialized,
            Err(err) => {
                warn!("Failed to deserialize timeline event: {err}");
                return;
            }
        };

        let sender = deserialized.sender().to_owned();
        let timestamp = deserialized.origin_server_ts();
        let event_id = deserialized.event_id().to_owned();
        let txn_id = deserialized.transaction_id().map(ToOwned::to_owned);

        if let Some(action @ TimelineAction::HandleAggregation { .. }) = TimelineAction::from_event(
            deserialized,
            event.raw(),
            room_data_provider,
            None,
            &self.meta,
            None,
            None,
            None,
        )
        .await
        {
            let encryption_info = event.kind.encryption_info().cloned();

            let sender_profile = room_data_provider.profile_from_user_id(&sender).await;

            let ctx = TimelineEventContext {
                sender,
                sender_profile,
                timestamp,
                // These are not used when handling an aggregation.
                read_receipts: Default::default(),
                is_highlighted: false,
                flow: Flow::Remote {
                    event_id: event_id.clone(),
                    raw_event: event.raw().clone(),
                    encryption_info,
                    txn_id,
                    position,
                },
                // This field is not used when handling an aggregation.
                should_add_new_items: false,
            };

            TimelineEventHandler::new(self, ctx).handle_event(date_divider_adjuster, action).await;
        }
    }

    /// Handle a set of live remote aggregations on events as [`VectorDiff`]s.
    ///
    /// This is like `handle_remote_events`, with two key differences:
    /// - it only applies to aggregated events, not all the sync events.
    /// - it will also not add the events to the `all_remote_events` array
    ///   itself.
    pub(super) async fn handle_remote_aggregations<RoomData>(
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
                        self.handle_remote_aggregation(
                            event,
                            TimelineItemPosition::End { origin },
                            room_data_provider,
                            &mut date_divider_adjuster,
                        )
                        .await;
                    }
                }

                VectorDiff::PushFront { value: event } => {
                    self.handle_remote_aggregation(
                        event,
                        TimelineItemPosition::Start { origin },
                        room_data_provider,
                        &mut date_divider_adjuster,
                    )
                    .await;
                }

                VectorDiff::PushBack { value: event } => {
                    self.handle_remote_aggregation(
                        event,
                        TimelineItemPosition::End { origin },
                        room_data_provider,
                        &mut date_divider_adjuster,
                    )
                    .await;
                }

                VectorDiff::Insert { index: event_index, value: event } => {
                    self.handle_remote_aggregation(
                        event,
                        TimelineItemPosition::At { event_index, origin },
                        room_data_provider,
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
                        self.handle_remote_aggregation(
                            event,
                            TimelineItemPosition::UpdateAt { timeline_item_index },
                            room_data_provider,
                            &mut date_divider_adjuster,
                        )
                        .await;
                    } else {
                        warn!(event_index, "Set update dropped because there wasn't any attached timeline item index.");
                    }
                }

                VectorDiff::Remove { .. } | VectorDiff::Clear => {
                    // Do nothing. An aggregated redaction comes with a
                    // redaction event, or as a redacted event in the first
                    // place.
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
            panic!("duplicate read receipts in this timeline: {duplicates:?}\n{:?}", self.items);

            #[cfg(not(any(debug_assertions, test)))]
            tracing::error!(
                ?duplicates,
                items = ?self.items,
                "duplicate read receipts in this timeline",
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
            panic!("duplicate unique ids in this timeline: {duplicates:?}\n{:?}", self.items);

            #[cfg(not(any(debug_assertions, test)))]
            tracing::error!(
                ?duplicates,
                items = ?self.items,
                "duplicate unique ids in this timeline",
            );
        }
    }

    /// Whether the event should be added to the timeline as a new item.
    fn should_add_event_item<P: RoomDataProvider>(
        &self,
        room_data_provider: &P,
        settings: &TimelineSettings,
        event: &AnySyncTimelineEvent,
        thread_root: Option<&EventId>,
        position: TimelineItemPosition,
    ) -> bool {
        let room_version = room_data_provider.room_version();
        if !(settings.event_filter)(event, &room_version) {
            // The user filtered out the event.
            return false;
        }

        match &self.timeline_focus {
            TimelineFocusKind::PinnedEvents => {
                // Only add pinned events for the pinned events timeline.
                room_data_provider.is_pinned_event(event.event_id())
            }

            TimelineFocusKind::Event { hide_threaded_events } => {
                // If the timeline's filtering out in-thread events, don't add items for
                // threaded events.
                if thread_root.is_some() && *hide_threaded_events {
                    return false;
                }

                // Retrieve the origin of the event.
                let origin = match position {
                    TimelineItemPosition::End { origin }
                    | TimelineItemPosition::Start { origin }
                    | TimelineItemPosition::At { origin, .. } => origin,

                    TimelineItemPosition::UpdateAt { timeline_item_index: idx } => self
                        .items
                        .get(idx)
                        .and_then(|item| item.as_event()?.as_remote())
                        .map_or(RemoteEventOrigin::Unknown, |item| item.origin),
                };

                match origin {
                    // Never add any item to a focused timeline when the item comes from sync.
                    RemoteEventOrigin::Sync | RemoteEventOrigin::Unknown => false,
                    RemoteEventOrigin::Cache | RemoteEventOrigin::Pagination => true,
                }
            }

            TimelineFocusKind::Live { hide_threaded_events } => {
                // If the timeline's filtering out in-thread events, don't add items for
                // threaded events.
                thread_root.is_none() || !hide_threaded_events
            }

            TimelineFocusKind::Thread { root_event_id } => {
                // Add new items only for the thread root and the thread replies.
                event.event_id() == root_event_id
                    || thread_root.as_ref().is_some_and(|r| r == root_event_id)
            }
        }
    }

    /// After a deserialization error, adds a failed-to-parse item to the
    /// timeline if configured to do so, or logs the error (and optionally
    /// save metadata) if not.
    async fn maybe_add_error_item(
        &mut self,
        position: TimelineItemPosition,
        room_data_provider: &impl RoomDataProvider,
        raw: &Raw<AnySyncTimelineEvent>,
        deserialization_error: serde_json::Error,
        settings: &TimelineSettings,
    ) -> Option<(
        OwnedEventId,
        OwnedUserId,
        MilliSecondsSinceUnixEpoch,
        Option<OwnedTransactionId>,
        Option<TimelineAction>,
        bool,
    )> {
        let state_key: Option<String> = raw.get_field("state_key").ok().flatten();

        // A state event is an event that has a state key. Note that the two branches
        // differ because the inferred return type for `get_field` is different
        // in each case.
        //
        // If this was a state event but it didn't include a state_key, we'll assume it
        // was a msg-like, because we can't do much more.
        let event_type = if let Some(state_key) = state_key {
            raw.get_field("type")
                .ok()
                .flatten()
                .map(|event_type| FailedToParseEvent::State { event_type, state_key })
        } else {
            raw.get_field("type").ok().flatten().map(FailedToParseEvent::MsgLike)
        };

        let event_id: Option<OwnedEventId> = raw.get_field("event_id").ok().flatten();
        let Some(event_id) = event_id else {
            // If the event doesn't even have an event ID, we can't do anything with it.
            warn!(
                ?event_type,
                "Failed to deserialize timeline event (with no ID): {deserialization_error}"
            );
            return None;
        };

        let sender: Option<OwnedUserId> = raw.get_field("sender").ok().flatten();
        let origin_server_ts: Option<MilliSecondsSinceUnixEpoch> =
            raw.get_field("origin_server_ts").ok().flatten();

        match (sender, origin_server_ts, event_type) {
            (Some(sender), Some(origin_server_ts), Some(event_type))
                if settings.add_failed_to_parse =>
            {
                // We have sufficient information to show an item in the timeline, and we've
                // been requested to show it, let's do it.
                #[derive(serde::Deserialize)]
                struct Unsigned {
                    transaction_id: Option<OwnedTransactionId>,
                }

                let transaction_id: Option<OwnedTransactionId> = raw
                    .get_field::<Unsigned>("unsigned")
                    .ok()
                    .flatten()
                    .and_then(|unsigned| unsigned.transaction_id);

                // The event can be partially deserialized, and it is allowed to be added to
                // the timeline.
                Some((
                    event_id,
                    sender,
                    origin_server_ts,
                    transaction_id,
                    Some(TimelineAction::failed_to_parse(event_type, deserialization_error)),
                    true,
                ))
            }

            (sender, origin_server_ts, event_type) => {
                // We either lack information for rendering an item, or we've been requested not
                // to show it. Save it into the metadata and return.
                warn!(
                    ?event_type,
                    ?event_id,
                    "Failed to deserialize timeline event: {deserialization_error}"
                );

                // Remember the event before returning prematurely.
                // See [`ObservableItems::all_remote_events`].
                self.add_or_update_remote_event(
                    EventMeta::new(event_id, false),
                    sender.as_deref(),
                    origin_server_ts,
                    position,
                    room_data_provider,
                    settings,
                )
                .await;
                None
            }
        }
    }

    // Attempt to load a thread's latest reply as an embedded timeline item, either
    // using the event cache or the storage.
    async fn fetch_latest_thread_reply(
        &mut self,
        event_id: &EventId,
        room_data_provider: &impl RoomDataProvider,
    ) -> Option<Box<EmbeddedEvent>> {
        let event = room_data_provider
            .load_event(event_id)
            .await
            .inspect_err(|err| {
                warn!("Failed to load thread latest event: {err}");
            })
            .ok()?;

        EmbeddedEvent::try_from_timeline_event(event, room_data_provider, &self.meta)
            .await
            .inspect_err(|err| {
                warn!("Failed to extract thread latest event into a timeline item content: {err}");
            })
            .ok()
            .flatten()
            .map(Box::new)
    }

    /// Handle a remote event.
    ///
    /// Returns whether an item has been removed from the timeline.
    pub(super) async fn handle_remote_event<P: RoomDataProvider>(
        &mut self,
        event: TimelineEvent,
        position: TimelineItemPosition,
        room_data_provider: &P,
        settings: &TimelineSettings,
        date_divider_adjuster: &mut DateDividerAdjuster,
    ) -> RemovedItem {
        let is_highlighted =
            event.push_actions().is_some_and(|actions| actions.iter().any(Action::is_highlight));

        let thread_summary = if let ThreadSummaryStatus::Some(summary) = event.thread_summary {
            let latest_reply_item = if let Some(latest_reply) = summary.latest_reply {
                self.fetch_latest_thread_reply(&latest_reply, room_data_provider).await
            } else {
                None
            };
            Some(ThreadSummary {
                latest_event: TimelineDetails::from_initial_value(latest_reply_item),
                num_replies: summary.num_replies,
            })
        } else {
            None
        };

        let encryption_info = event.kind.encryption_info().cloned();

        let bundled_edit_encryption_info = event.kind.unsigned_encryption_map().and_then(|map| {
            map.get(&UnsignedEventLocation::RelationsReplace)?.encryption_info().cloned()
        });

        let (raw, utd_info) = match event.kind {
            TimelineEventKind::UnableToDecrypt { utd_info, event } => (event, Some(utd_info)),
            _ => (event.kind.into_raw(), None),
        };

        let (event_id, sender, timestamp, txn_id, timeline_action, should_add) = match raw
            .deserialize()
        {
            // Classical path: the event is valid, can be deserialized, everything is alright.
            Ok(event) => {
                let (in_reply_to, thread_root) = self.meta.process_event_relations(
                    &event,
                    &raw,
                    bundled_edit_encryption_info,
                    &self.items,
                    &self.timeline_focus,
                );

                let should_add = self.should_add_event_item(
                    room_data_provider,
                    settings,
                    &event,
                    thread_root.as_deref(),
                    position,
                );

                (
                    event.event_id().to_owned(),
                    event.sender().to_owned(),
                    event.origin_server_ts(),
                    event.transaction_id().map(ToOwned::to_owned),
                    TimelineAction::from_event(
                        event,
                        &raw,
                        room_data_provider,
                        utd_info,
                        &self.meta,
                        in_reply_to,
                        thread_root,
                        thread_summary,
                    )
                    .await,
                    should_add,
                )
            }

            // The event seems invalid…
            Err(e) => {
                if let Some(tuple) =
                    self.maybe_add_error_item(position, room_data_provider, &raw, e, settings).await
                {
                    tuple
                } else {
                    return false;
                }
            }
        };

        // Remember the event.
        // See [`ObservableItems::all_remote_events`].
        self.add_or_update_remote_event(
            EventMeta::new(event_id.clone(), should_add),
            Some(&sender),
            Some(timestamp),
            position,
            room_data_provider,
            settings,
        )
        .await;

        // Handle the event to create or update a timeline item.
        if let Some(timeline_action) = timeline_action {
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
                is_highlighted,
                flow: Flow::Remote {
                    event_id: event_id.clone(),
                    raw_event: raw,
                    encryption_info,
                    txn_id,
                    position,
                },
                should_add_new_items: should_add,
            };

            TimelineEventHandler::new(self, ctx)
                .handle_event(date_divider_adjuster, timeline_action)
                .await
        } else {
            // No item has been removed from the timeline.
            false
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
        event_meta: EventMeta,
        sender: Option<&UserId>,
        timestamp: Option<MilliSecondsSinceUnixEpoch>,
        position: TimelineItemPosition,
        room_data_provider: &P,
        settings: &TimelineSettings,
    ) {
        let event_id = event_meta.event_id.clone();

        match position {
            TimelineItemPosition::Start { .. } => self.items.push_front_remote_event(event_meta),

            TimelineItemPosition::End { .. } => {
                self.items.push_back_remote_event(event_meta);
            }

            TimelineItemPosition::At { event_index, .. } => {
                self.items.insert_remote_event(event_index, event_meta);
            }

            TimelineItemPosition::UpdateAt { .. } => {
                if let Some(event) =
                    self.items.get_remote_event_by_event_id_mut(&event_meta.event_id)
                {
                    if event.visible != event_meta.visible {
                        event.visible = event_meta.visible;

                        if settings.track_read_receipts {
                            // Since the event's visibility changed, we need to update the read
                            // receipts of the previous visible event.
                            self.maybe_update_read_receipts_of_prev_event(&event_meta.event_id);
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
            self.load_read_receipts_for_event(&event_id, room_data_provider).await;

            self.maybe_add_implicit_read_receipt(&event_id, sender, timestamp);
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
