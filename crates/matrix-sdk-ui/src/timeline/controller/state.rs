// Copyright 2023 The Matrix.org Foundation C.I.C.
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
    future::Future,
    num::NonZeroUsize,
    sync::{Arc, RwLock},
};

use eyeball_im::VectorDiff;
use itertools::Itertools as _;
use matrix_sdk::{
    deserialized_responses::SyncTimelineEvent, ring_buffer::RingBuffer, send_queue::SendHandle,
};
use matrix_sdk_base::deserialized_responses::TimelineEvent;
#[cfg(test)]
use ruma::events::receipt::ReceiptEventContent;
use ruma::{
    events::{
        poll::{
            unstable_response::UnstablePollResponseEventContent,
            unstable_start::NewUnstablePollStartEventContentWithoutRelation,
        },
        relation::Replacement,
        room::message::RoomMessageEventContentWithoutRelation,
        AnySyncEphemeralRoomEvent, AnySyncTimelineEvent,
    },
    push::Action,
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
    RoomVersionId, UserId,
};
use tracing::{debug, instrument, trace, warn};

use super::{
    observable_items::{
        AllRemoteEvents, ObservableItems, ObservableItemsTransaction,
        ObservableItemsTransactionEntry,
    },
    DateDividerMode, HandleManyEventsResult, TimelineFocusKind, TimelineSettings,
};
use crate::{
    events::SyncTimelineEventWithoutContent,
    timeline::{
        date_dividers::DateDividerAdjuster,
        event_handler::{
            Flow, HandleEventResult, TimelineEventContext, TimelineEventHandler, TimelineEventKind,
            TimelineItemPosition,
        },
        event_item::{PollState, RemoteEventOrigin, ResponseData},
        item::TimelineUniqueId,
        reactions::Reactions,
        read_receipts::ReadReceipts,
        traits::RoomDataProvider,
        util::{rfind_event_by_id, RelativePosition},
        Profile, TimelineItem, TimelineItemKind,
    },
    unable_to_decrypt_hook::UtdHookManager,
};

/// This is a simplification of [`TimelineItemPosition`] which doesn't contain
/// the [`TimelineItemPosition::UpdateDecrypted`] variant, because it is used
/// only for **new** items.
#[derive(Debug)]
pub(crate) enum TimelineNewItemPosition {
    /// One or more items are prepended to the timeline (i.e. they're the
    /// oldest).
    Start { origin: RemoteEventOrigin },

    /// One or more items are appended to the timeline (i.e. they're the most
    /// recent).
    End { origin: RemoteEventOrigin },
}

impl From<TimelineNewItemPosition> for TimelineItemPosition {
    fn from(value: TimelineNewItemPosition) -> Self {
        match value {
            TimelineNewItemPosition::Start { origin } => Self::Start { origin },
            TimelineNewItemPosition::End { origin } => Self::End { origin },
        }
    }
}

#[derive(Debug)]
pub(in crate::timeline) struct TimelineState {
    pub items: ObservableItems,
    pub meta: TimelineMetadata,

    /// The kind of focus of this timeline.
    timeline_focus: TimelineFocusKind,
}

impl TimelineState {
    pub(super) fn new(
        timeline_focus: TimelineFocusKind,
        own_user_id: OwnedUserId,
        room_version: RoomVersionId,
        internal_id_prefix: Option<String>,
        unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,
        is_room_encrypted: Option<bool>,
    ) -> Self {
        Self {
            items: ObservableItems::new(),
            meta: TimelineMetadata::new(
                own_user_id,
                room_version,
                internal_id_prefix,
                unable_to_decrypt_hook,
                is_room_encrypted,
            ),
            timeline_focus,
        }
    }

    /// Add the given remote events at the given end of the timeline.
    ///
    /// Note: when the `position` is [`TimelineEnd::Front`], prepended events
    /// should be ordered in *reverse* topological order, that is, `events[0]`
    /// is the most recent.
    #[tracing::instrument(skip(self, events, room_data_provider, settings))]
    pub(super) async fn add_remote_events_at<Events, RoomData>(
        &mut self,
        events: Events,
        position: TimelineNewItemPosition,
        room_data_provider: &RoomData,
        settings: &TimelineSettings,
    ) -> HandleManyEventsResult
    where
        Events: IntoIterator + ExactSizeIterator,
        <Events as IntoIterator>::Item: Into<SyncTimelineEvent>,
        RoomData: RoomDataProvider,
    {
        if events.len() == 0 {
            return Default::default();
        }

        let mut txn = self.transaction();
        let handle_many_res =
            txn.add_remote_events_at(events, position, room_data_provider, settings).await;
        txn.commit();

        handle_many_res
    }

    /// Handle updates on events as [`VectorDiff`]s.
    pub(super) async fn handle_remote_events_with_diffs<RoomData>(
        &mut self,
        diffs: Vec<VectorDiff<SyncTimelineEvent>>,
        origin: RemoteEventOrigin,
        room_data: &RoomData,
        settings: &TimelineSettings,
    ) where
        RoomData: RoomDataProvider,
    {
        if diffs.is_empty() {
            return Default::default();
        }

        let mut transaction = self.transaction();
        transaction.handle_remote_events_with_diffs(diffs, origin, room_data, settings).await;
        transaction.commit();
    }

    /// Marks the given event as fully read, using the read marker received from
    /// sync.
    pub(super) fn handle_fully_read_marker(&mut self, fully_read_event_id: OwnedEventId) {
        let mut txn = self.transaction();
        txn.set_fully_read_event(fully_read_event_id);
        txn.commit();
    }

    #[instrument(skip_all)]
    pub(super) async fn handle_ephemeral_events<P: RoomDataProvider>(
        &mut self,
        events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        room_data_provider: &P,
    ) {
        if events.is_empty() {
            return;
        }

        let mut txn = self.transaction();

        trace!("Handling ephemeral room events");
        let own_user_id = room_data_provider.own_user_id();
        for raw_event in events {
            match raw_event.deserialize() {
                Ok(AnySyncEphemeralRoomEvent::Receipt(ev)) => {
                    txn.handle_explicit_read_receipts(ev.content, own_user_id);
                }
                Ok(_) => {}
                Err(e) => {
                    let event_type = raw_event.get_field::<String>("type").ok().flatten();
                    warn!(event_type, "Failed to deserialize ephemeral event: {e}");
                }
            }
        }

        txn.commit();
    }

    /// Adds a local echo (for an event) to the timeline.
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all)]
    pub(super) async fn handle_local_event(
        &mut self,
        own_user_id: OwnedUserId,
        own_profile: Option<Profile>,
        should_add_new_items: bool,
        date_divider_mode: DateDividerMode,
        txn_id: OwnedTransactionId,
        send_handle: Option<SendHandle>,
        content: TimelineEventKind,
    ) {
        let ctx = TimelineEventContext {
            sender: own_user_id,
            sender_profile: own_profile,
            timestamp: MilliSecondsSinceUnixEpoch::now(),
            is_own_event: true,
            read_receipts: Default::default(),
            // An event sent by ourselves is never matched against push rules.
            is_highlighted: false,
            flow: Flow::Local { txn_id, send_handle },
            should_add_new_items,
        };

        let mut txn = self.transaction();

        let mut date_divider_adjuster = DateDividerAdjuster::new(date_divider_mode);

        TimelineEventHandler::new(&mut txn, ctx)
            .handle_event(&mut date_divider_adjuster, content)
            .await;

        txn.adjust_date_dividers(date_divider_adjuster);

        txn.commit();
    }

    pub(super) async fn retry_event_decryption<P: RoomDataProvider, Fut>(
        &mut self,
        retry_one: impl Fn(Arc<TimelineItem>) -> Fut,
        retry_indices: Vec<usize>,
        push_rules_context: Option<(ruma::push::Ruleset, ruma::push::PushConditionRoomCtx)>,
        room_data_provider: &P,
        settings: &TimelineSettings,
    ) where
        Fut: Future<Output = Option<TimelineEvent>>,
    {
        let mut txn = self.transaction();

        let mut date_divider_adjuster =
            DateDividerAdjuster::new(settings.date_divider_mode.clone());

        // Loop through all the indices, in order so we don't decrypt edits
        // before the event being edited, if both were UTD. Keep track of
        // index change as UTDs are removed instead of updated.
        let mut offset = 0;
        for idx in retry_indices {
            let idx = idx - offset;
            let Some(mut event) = retry_one(txn.items[idx].clone()).await else {
                continue;
            };

            event.push_actions = push_rules_context.as_ref().map(|(push_rules, push_context)| {
                push_rules.get_actions(event.raw(), push_context).to_owned()
            });

            let handle_one_res = txn
                .handle_remote_event(
                    event.into(),
                    TimelineItemPosition::UpdateDecrypted { timeline_item_index: idx },
                    room_data_provider,
                    settings,
                    &mut date_divider_adjuster,
                )
                .await;

            // If the UTD was removed rather than updated, offset all
            // subsequent loop iterations.
            if handle_one_res.item_removed {
                offset += 1;
            }
        }

        txn.adjust_date_dividers(date_divider_adjuster);

        txn.commit();
    }

    #[cfg(test)]
    pub(super) fn handle_read_receipts(
        &mut self,
        receipt_event_content: ReceiptEventContent,
        own_user_id: &UserId,
    ) {
        let mut txn = self.transaction();
        txn.handle_explicit_read_receipts(receipt_event_content, own_user_id);
        txn.commit();
    }

    pub(super) fn clear(&mut self) {
        let mut txn = self.transaction();
        txn.clear();
        txn.commit();
    }

    /// Replaces the existing events in the timeline with the given remote ones.
    ///
    /// Note: when the `position` is [`TimelineEnd::Front`], prepended events
    /// should be ordered in *reverse* topological order, that is, `events[0]`
    /// is the most recent.
    pub(super) async fn replace_with_remote_events<Events, RoomData>(
        &mut self,
        events: Events,
        position: TimelineNewItemPosition,
        room_data_provider: &RoomData,
        settings: &TimelineSettings,
    ) -> HandleManyEventsResult
    where
        Events: IntoIterator,
        Events::Item: Into<SyncTimelineEvent>,
        RoomData: RoomDataProvider,
    {
        let mut txn = self.transaction();
        txn.clear();
        let result = txn.add_remote_events_at(events, position, room_data_provider, settings).await;
        txn.commit();
        result
    }

    pub(super) fn update_all_events_is_room_encrypted(&mut self) {
        let is_room_encrypted = *self.meta.is_room_encrypted.read().unwrap();

        // When this transaction finishes, all items in the timeline will be emitted
        // again with the updated encryption value
        let mut txn = self.transaction();
        txn.update_all_events_is_room_encrypted(is_room_encrypted);
        txn.commit();
    }

    pub(super) fn transaction(&mut self) -> TimelineStateTransaction<'_> {
        let items = self.items.transaction();
        let meta = self.meta.clone();

        TimelineStateTransaction {
            items,
            previous_meta: &mut self.meta,
            meta,
            timeline_focus: self.timeline_focus,
        }
    }
}

pub(in crate::timeline) struct TimelineStateTransaction<'a> {
    /// A vector transaction over the items themselves. Holds temporary state
    /// until committed.
    pub items: ObservableItemsTransaction<'a>,

    /// A clone of the previous meta, that we're operating on during the
    /// transaction, and that will be committed to the previous meta location in
    /// [`Self::commit`].
    pub meta: TimelineMetadata,

    /// Pointer to the previous meta, only used during [`Self::commit`].
    previous_meta: &'a mut TimelineMetadata,

    /// The kind of focus of this timeline.
    timeline_focus: TimelineFocusKind,
}

impl TimelineStateTransaction<'_> {
    /// Add the given remote events at the given end of the timeline.
    ///
    /// Note: when the `position` is [`TimelineEnd::Front`], prepended events
    /// should be ordered in *reverse* topological order, that is, `events[0]`
    /// is the most recent.
    #[tracing::instrument(skip(self, events, room_data_provider, settings))]
    pub(super) async fn add_remote_events_at<Events, RoomData>(
        &mut self,
        events: Events,
        position: TimelineNewItemPosition,
        room_data_provider: &RoomData,
        settings: &TimelineSettings,
    ) -> HandleManyEventsResult
    where
        Events: IntoIterator,
        Events::Item: Into<SyncTimelineEvent>,
        RoomData: RoomDataProvider,
    {
        let mut total = HandleManyEventsResult::default();

        let position = position.into();

        let mut date_divider_adjuster =
            DateDividerAdjuster::new(settings.date_divider_mode.clone());

        // Implementation note: when `position` is `TimelineEnd::Front`, events are in
        // the reverse topological order. Prepending them one by one in the order they
        // appear in the vector will thus result in the correct order.
        //
        // For instance, if the new events are : [C, B, A], where C is the most recent
        // and A is the oldest: we prepend C, then prepend B, then prepend A,
        // resulting in [A, B, C, (previous events)], which is what we want.

        for event in events {
            let handle_one_res = self
                .handle_remote_event(
                    event.into(),
                    position,
                    room_data_provider,
                    settings,
                    &mut date_divider_adjuster,
                )
                .await;

            total.items_added += handle_one_res.item_added as u64;
            total.items_updated += handle_one_res.items_updated as u64;
        }

        self.adjust_date_dividers(date_divider_adjuster);

        self.check_no_unused_unique_ids();
        total
    }

    /// Handle updates on events as [`VectorDiff`]s.
    pub(super) async fn handle_remote_events_with_diffs<RoomData>(
        &mut self,
        diffs: Vec<VectorDiff<SyncTimelineEvent>>,
        origin: RemoteEventOrigin,
        room_data_provider: &RoomData,
        settings: &TimelineSettings,
    ) where
        RoomData: RoomDataProvider,
    {
        let mut day_divider_adjuster = DayDividerAdjuster::default();

        for diff in diffs {
            match diff {
                VectorDiff::Append { values: events } => {
                    for event in events {
                        self.handle_remote_event(
                            event,
                            TimelineItemPosition::End { origin },
                            room_data_provider,
                            settings,
                            &mut day_divider_adjuster,
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
                        &mut day_divider_adjuster,
                    )
                    .await;
                }

                v => unimplemented!("{v:?}"),
            }
        }
    }

    fn check_no_unused_unique_ids(&self) {
        let duplicates = self
            .items
            .iter()
            .duplicates_by(|item| item.unique_id())
            .map(|item| item.unique_id())
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
    async fn handle_remote_event<P: RoomDataProvider>(
        &mut self,
        event: SyncTimelineEvent,
        position: TimelineItemPosition,
        room_data_provider: &P,
        settings: &TimelineSettings,
        date_divider_adjuster: &mut DateDividerAdjuster,
    ) -> HandleEventResult {
        let SyncTimelineEvent { push_actions, kind } = event;
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
                        | TimelineItemPosition::Start { origin } => origin,

                        TimelineItemPosition::UpdateDecrypted { timeline_item_index: idx } => self
                            .items
                            .get(idx)
                            .and_then(|item| item.as_event())
                            .and_then(|item| item.as_remote())
                            .map_or(RemoteEventOrigin::Unknown, |item| item.origin),
                    };

                    match origin {
                        RemoteEventOrigin::Sync | RemoteEventOrigin::Unknown => {
                            should_add = match self.timeline_focus {
                                TimelineFocusKind::PinnedEvents => {
                                    // Only insert timeline items for pinned events, if the event
                                    // came from the sync.
                                    room_data_provider.is_pinned_event(&event_id)
                                }

                                TimelineFocusKind::Live => {
                                    // Always add new items to a live timeline receiving items from
                                    // sync.
                                    true
                                }

                                TimelineFocusKind::Event => {
                                    // Never add any item to a focused timeline when the item comes
                                    // down from the sync.
                                    false
                                }
                            };
                        }

                        RemoteEventOrigin::Pagination | RemoteEventOrigin::Cache => {
                            // Forward the previous decision to add it.
                        }
                    }
                }

                (
                    event_id,
                    event.sender().to_owned(),
                    event.origin_server_ts(),
                    event.transaction_id().map(ToOwned::to_owned),
                    TimelineEventKind::from_event(event, &raw, room_data_provider, utd_info).await,
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
                    TimelineEventKind::failed_to_parse(event, e),
                    true,
                ),

                // The event can be partially deserialized, but it is NOT allowed to be added to
                // the timeline.
                Ok(event) => {
                    let event_type = event.event_type();
                    let event_id = event.event_id();
                    warn!(%event_type, %event_id, "Failed to deserialize timeline event: {e}");

                    let is_own_event = event.sender() == room_data_provider.own_user_id();
                    let event_meta = FullEventMeta {
                        event_id,
                        sender: Some(event.sender()),
                        is_own_event,
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
                        let is_own_event =
                            sender.as_ref().is_some_and(|s| s == room_data_provider.own_user_id());
                        let timestamp: Option<MilliSecondsSinceUnixEpoch> =
                            raw.get_field("origin_server_ts").ok().flatten();

                        let event_meta = FullEventMeta {
                            event_id,
                            sender: sender.as_deref(),
                            is_own_event,
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

        let is_own_event = sender == room_data_provider.own_user_id();

        let event_meta = FullEventMeta {
            event_id: &event_id,
            sender: Some(&sender),
            is_own_event,
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
            is_own_event,
            read_receipts: if settings.track_read_receipts && should_add {
                self.meta.read_receipts.compute_event_receipts(
                    &event_id,
                    self.items.all_remote_events(),
                    matches!(position, TimelineItemPosition::End { .. }),
                )
            } else {
                Default::default()
            },
            is_highlighted: push_actions.iter().any(Action::is_highlight),
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
        TimelineEventHandler::new(self, ctx).handle_event(date_divider_adjuster, event_kind).await
    }

    fn clear(&mut self) {
        let has_local_echoes = self.items.iter().any(|item| item.is_local_echo());

        // By first checking if there are any local echoes first, we do a bit
        // more work in case some are found, but it should be worth it because
        // there will often not be any, and only emitting a single
        // `VectorDiff::Clear` should be much more efficient to process for
        // subscribers.
        if has_local_echoes {
            // Remove all remote events and the read marker
            self.items.for_each(|entry| {
                if entry.is_remote_event() || entry.is_read_marker() {
                    ObservableItemsTransactionEntry::remove(entry);
                }
            });

            // Remove stray date dividers
            let mut idx = 0;
            while idx < self.items.len() {
                if self.items[idx].is_date_divider()
                    && self.items.get(idx + 1).map_or(true, |item| item.is_date_divider())
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
    fn set_fully_read_event(&mut self, fully_read_event_id: OwnedEventId) {
        // A similar event has been handled already. We can ignore it.
        if self.meta.fully_read_event.as_ref().is_some_and(|id| *id == fully_read_event_id) {
            return;
        }

        self.meta.fully_read_event = Some(fully_read_event_id);
        self.meta.update_read_marker(&mut self.items);
    }

    pub(super) fn commit(self) {
        let Self { items, previous_meta, meta, .. } = self;

        // Replace the pointer to the previous meta with the new one.
        *previous_meta = meta;

        items.commit();
    }

    /// Add or update a remote  event in the
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
        // Detect if an event already exists in [`ObservableItems::all_remote_events`].
        //
        // Returns its position, in this case.
        fn event_already_exists(
            new_event_id: &EventId,
            all_remote_events: &AllRemoteEvents,
        ) -> Option<usize> {
            all_remote_events.iter().position(|EventMeta { event_id, .. }| event_id == new_event_id)
        }

        match position {
            TimelineItemPosition::Start { .. } => {
                if let Some(pos) =
                    event_already_exists(event_meta.event_id, self.items.all_remote_events())
                {
                    self.items.remove_remote_event(pos);
                }

                self.items.push_front_remote_event(event_meta.base_meta())
            }

            TimelineItemPosition::End { .. } => {
                if let Some(pos) =
                    event_already_exists(event_meta.event_id, self.items.all_remote_events())
                {
                    self.items.remove_remote_event(pos);
                }

                self.items.push_back_remote_event(event_meta.base_meta());
            }

            TimelineItemPosition::UpdateDecrypted { .. } => {
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
                TimelineItemPosition::Start { .. } | TimelineItemPosition::End { .. }
            )
        {
            self.load_read_receipts_for_event(event_meta.event_id, room_data_provider).await;

            self.maybe_add_implicit_read_receipt(event_meta);
        }
    }

    fn adjust_date_dividers(&mut self, mut adjuster: DateDividerAdjuster) {
        adjuster.run(&mut self.items, &mut self.meta);
    }

    /// This method replaces the `is_room_encrypted` value for all timeline
    /// items to its updated version and creates a `VectorDiff::Set` operation
    /// for each item which will be added to this transaction.
    fn update_all_events_is_room_encrypted(&mut self, is_encrypted: Option<bool>) {
        for idx in 0..self.items.len() {
            let item = &self.items[idx];

            if let Some(event) = item.as_event() {
                let mut cloned_event = event.clone();
                cloned_event.is_room_encrypted = is_encrypted;

                // Replace the existing item with a new version with the right encryption flag
                let item = item.with_kind(cloned_event);
                self.items.replace(idx, item);
            }
        }
    }
}

/// Cache holding poll response and end events handled before their poll start
/// event has been handled.
#[derive(Clone, Debug, Default)]
pub(in crate::timeline) struct PendingPollEvents {
    /// Responses to a poll (identified by the poll's start event id).
    responses: HashMap<OwnedEventId, Vec<ResponseData>>,

    /// Mapping of a poll (identified by its start event's id) to its end date.
    end_dates: HashMap<OwnedEventId, MilliSecondsSinceUnixEpoch>,
}

impl PendingPollEvents {
    pub(crate) fn add_response(
        &mut self,
        start_event_id: &EventId,
        sender: &UserId,
        timestamp: MilliSecondsSinceUnixEpoch,
        content: &UnstablePollResponseEventContent,
    ) {
        self.responses.entry(start_event_id.to_owned()).or_default().push(ResponseData {
            sender: sender.to_owned(),
            timestamp,
            answers: content.poll_response.answers.clone(),
        });
    }

    pub(crate) fn clear(&mut self) {
        self.end_dates.clear();
        self.responses.clear();
    }

    /// Mark a poll as finished by inserting its poll date.
    pub(crate) fn mark_as_ended(
        &mut self,
        start_event_id: &EventId,
        timestamp: MilliSecondsSinceUnixEpoch,
    ) {
        self.end_dates.insert(start_event_id.to_owned(), timestamp);
    }

    /// Dumps all response and end events present in the cache that belong to
    /// the given start_event_id into the given poll_state.
    pub(crate) fn apply_pending(&mut self, start_event_id: &EventId, poll_state: &mut PollState) {
        if let Some(pending_responses) = self.responses.remove(start_event_id) {
            poll_state.response_data.extend(pending_responses);
        }
        if let Some(pending_end) = self.end_dates.remove(start_event_id) {
            poll_state.end_event_timestamp = Some(pending_end);
        }
    }
}

#[derive(Clone)]
pub(in crate::timeline) enum PendingEditKind {
    RoomMessage(Replacement<RoomMessageEventContentWithoutRelation>),
    Poll(Replacement<NewUnstablePollStartEventContentWithoutRelation>),
}

#[derive(Clone)]
pub(in crate::timeline) struct PendingEdit {
    pub kind: PendingEditKind,
    pub event_json: Raw<AnySyncTimelineEvent>,
}

impl PendingEdit {
    pub fn edited_event(&self) -> &EventId {
        match &self.kind {
            PendingEditKind::RoomMessage(Replacement { event_id, .. })
            | PendingEditKind::Poll(Replacement { event_id, .. }) => event_id,
        }
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for PendingEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            PendingEditKind::RoomMessage(_) => {
                f.debug_struct("RoomMessage").finish_non_exhaustive()
            }
            PendingEditKind::Poll(_) => f.debug_struct("Poll").finish_non_exhaustive(),
        }
    }
}

#[derive(Clone, Debug)]
pub(in crate::timeline) struct TimelineMetadata {
    // **** CONSTANT FIELDS ****
    /// An optional prefix for internal IDs, defined during construction of the
    /// timeline.
    ///
    /// This value is constant over the lifetime of the metadata.
    internal_id_prefix: Option<String>,

    /// The hook to call whenever we run into a unable-to-decrypt event.
    ///
    /// This value is constant over the lifetime of the metadata.
    pub(crate) unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,

    /// A boolean indicating whether the room the timeline is attached to is
    /// actually encrypted or not.
    pub(crate) is_room_encrypted: Arc<RwLock<Option<bool>>>,

    /// Matrix room version of the timeline's room, or a sensible default.
    ///
    /// This value is constant over the lifetime of the metadata.
    pub room_version: RoomVersionId,

    /// The own [`OwnedUserId`] of the client who opened the timeline.
    own_user_id: OwnedUserId,

    // **** DYNAMIC FIELDS ****
    /// The next internal identifier for timeline items, used for both local and
    /// remote echoes.
    ///
    /// This is never cleared, but always incremented, to avoid issues with
    /// reusing a stale internal id across timeline clears. We don't expect
    /// we can hit `u64::max_value()` realistically, but if this would
    /// happen, we do a wrapping addition when incrementing this
    /// id; the previous 0 value would have disappeared a long time ago, unless
    /// the device has terabytes of RAM.
    next_internal_id: u64,

    /// State helping matching reactions to their associated events, and
    /// stashing pending reactions.
    pub reactions: Reactions,

    /// Associated poll events received before their original poll start event.
    pub pending_poll_events: PendingPollEvents,

    /// Edit events received before the related event they're editing.
    pub pending_edits: RingBuffer<PendingEdit>,

    /// Identifier of the fully-read event, helping knowing where to introduce
    /// the read marker.
    pub fully_read_event: Option<OwnedEventId>,

    /// Whether we have a fully read-marker item in the timeline, that's up to
    /// date with the room's read marker.
    ///
    /// This is false when:
    /// - The fully-read marker points to an event that is not in the timeline,
    /// - The fully-read marker item would be the last item in the timeline.
    pub has_up_to_date_read_marker_item: bool,

    /// Read receipts related state.
    ///
    /// TODO: move this over to the event cache (see also #3058).
    pub read_receipts: ReadReceipts,
}

/// Maximum number of stash pending edits.
/// SAFETY: 32 is not 0.
const MAX_NUM_STASHED_PENDING_EDITS: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(32) };

impl TimelineMetadata {
    pub(crate) fn new(
        own_user_id: OwnedUserId,
        room_version: RoomVersionId,
        internal_id_prefix: Option<String>,
        unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,
        is_room_encrypted: Option<bool>,
    ) -> Self {
        Self {
            own_user_id,
            next_internal_id: Default::default(),
            reactions: Default::default(),
            pending_poll_events: Default::default(),
            pending_edits: RingBuffer::new(MAX_NUM_STASHED_PENDING_EDITS),
            fully_read_event: Default::default(),
            // It doesn't make sense to set this to false until we fill the `fully_read_event`
            // field, otherwise we'll keep on exiting early in `Self::update_read_marker`.
            has_up_to_date_read_marker_item: true,
            read_receipts: Default::default(),
            room_version,
            unable_to_decrypt_hook,
            internal_id_prefix,
            is_room_encrypted: Arc::new(RwLock::new(is_room_encrypted)),
        }
    }

    pub(crate) fn clear(&mut self) {
        // Note: we don't clear the next internal id to avoid bad cases of stale unique
        // ids across timeline clears.
        self.reactions.clear();
        self.pending_poll_events.clear();
        self.pending_edits.clear();
        self.fully_read_event = None;
        // We forgot about the fully read marker right above, so wait for a new one
        // before attempting to update it for each new timeline item.
        self.has_up_to_date_read_marker_item = true;
        self.read_receipts.clear();
    }

    /// Get the relative positions of two events in the timeline.
    ///
    /// This method assumes that all events since the end of the timeline are
    /// known.
    ///
    /// Returns `None` if none of the two events could be found in the timeline.
    pub fn compare_events_positions(
        &self,
        event_a: &EventId,
        event_b: &EventId,
        all_remote_events: &AllRemoteEvents,
    ) -> Option<RelativePosition> {
        if event_a == event_b {
            return Some(RelativePosition::Same);
        }

        // We can make early returns here because we know all events since the end of
        // the timeline, so the first event encountered is the oldest one.
        for event_meta in all_remote_events.iter().rev() {
            if event_meta.event_id == event_a {
                return Some(RelativePosition::Before);
            }
            if event_meta.event_id == event_b {
                return Some(RelativePosition::After);
            }
        }

        None
    }

    /// Returns the next internal id for a timeline item (and increment our
    /// internal counter).
    fn next_internal_id(&mut self) -> TimelineUniqueId {
        let val = self.next_internal_id;
        self.next_internal_id = self.next_internal_id.wrapping_add(1);
        let prefix = self.internal_id_prefix.as_deref().unwrap_or("");
        TimelineUniqueId(format!("{prefix}{val}"))
    }

    /// Returns a new timeline item with a fresh internal id.
    pub fn new_timeline_item(&mut self, kind: impl Into<TimelineItemKind>) -> Arc<TimelineItem> {
        TimelineItem::new(kind, self.next_internal_id())
    }

    /// Try to update the read marker item in the timeline.
    pub(crate) fn update_read_marker(&mut self, items: &mut ObservableItemsTransaction<'_>) {
        let Some(fully_read_event) = &self.fully_read_event else { return };
        trace!(?fully_read_event, "Updating read marker");

        let read_marker_idx = items.iter().rposition(|item| item.is_read_marker());

        let mut fully_read_event_idx =
            rfind_event_by_id(items, fully_read_event).map(|(idx, _)| idx);

        if let Some(i) = &mut fully_read_event_idx {
            // The item at position `i` is the first item that's fully read, we're about to
            // insert a read marker just after it.
            //
            // Do another forward pass to skip all the events we've sent too.

            // Find the position of the first element…
            let next = items
                .iter()
                .enumerate()
                // …strictly *after* the fully read event…
                .skip(*i + 1)
                // …that's not virtual and not sent by us…
                .find(|(_, item)| {
                    item.as_event().is_some_and(|event| event.sender() != self.own_user_id)
                })
                .map(|(i, _)| i);

            if let Some(next) = next {
                // `next` point to the first item that's not sent by us, so the *previous* of
                // next is the right place where to insert the fully read marker.
                *i = next.wrapping_sub(1);
            } else {
                // There's no event after the read marker that's not sent by us, i.e. the full
                // timeline has been read: the fully read marker goes to the end.
                *i = items.len().wrapping_sub(1);
            }
        }

        match (read_marker_idx, fully_read_event_idx) {
            (None, None) => {
                // We didn't have a previous read marker, and we didn't find the fully-read
                // event in the timeline items. Don't do anything, and retry on
                // the next event we add.
                self.has_up_to_date_read_marker_item = false;
            }

            (None, Some(idx)) => {
                // Only insert the read marker if it is not at the end of the timeline.
                if idx + 1 < items.len() {
                    let idx = idx + 1;
                    items.insert(idx, TimelineItem::read_marker(), None);
                    self.has_up_to_date_read_marker_item = true;
                } else {
                    // The next event might require a read marker to be inserted at the current
                    // end.
                    self.has_up_to_date_read_marker_item = false;
                }
            }

            (Some(_), None) => {
                // We didn't find the timeline item containing the event referred to by the read
                // marker. Retry next time we get a new event.
                self.has_up_to_date_read_marker_item = false;
            }

            (Some(from), Some(to)) => {
                if from >= to {
                    // The read marker can't move backwards.
                    if from + 1 == items.len() {
                        // The read marker has nothing after it. An item disappeared; remove it.
                        items.remove(from);
                    }
                    self.has_up_to_date_read_marker_item = true;
                    return;
                }

                let prev_len = items.len();
                let read_marker = items.remove(from);

                // Only insert the read marker if it is not at the end of the timeline.
                if to + 1 < prev_len {
                    // Since the fully-read event's index was shifted to the left
                    // by one position by the remove call above, insert the fully-
                    // read marker at its previous position, rather than that + 1
                    items.insert(to, read_marker, None);
                    self.has_up_to_date_read_marker_item = true;
                } else {
                    self.has_up_to_date_read_marker_item = false;
                }
            }
        }
    }
}

/// Full metadata about an event.
///
/// Only used to group function parameters.
pub(crate) struct FullEventMeta<'a> {
    /// The ID of the event.
    pub event_id: &'a EventId,
    /// Whether the event is among the timeline items.
    pub visible: bool,
    /// The sender of the event.
    pub sender: Option<&'a UserId>,
    /// Whether this event was sent by our own user.
    pub is_own_event: bool,
    /// The timestamp of the event.
    pub timestamp: Option<MilliSecondsSinceUnixEpoch>,
}

impl FullEventMeta<'_> {
    fn base_meta(&self) -> EventMeta {
        EventMeta {
            event_id: self.event_id.to_owned(),
            visible: self.visible,
            timeline_item_index: None,
        }
    }
}

/// Metadata about an event that needs to be kept in memory.
#[derive(Debug, Clone)]
pub(crate) struct EventMeta {
    /// The ID of the event.
    pub event_id: OwnedEventId,

    /// Whether the event is among the timeline items.
    pub visible: bool,

    /// Foundation for the mapping between remote events to timeline items.
    ///
    /// Let's explain it. The events represent the first set and are stored in
    /// [`ObservableItems::all_remote_events`], and the timeline
    /// items represent the second set and are stored in
    /// [`ObservableItems::items`].
    ///
    /// Each event is mapped to at most one timeline item:
    ///
    /// - `None` if the event isn't rendered in the timeline (e.g. some state
    ///   events, or malformed events) or is rendered as a timeline item that
    ///   attaches to or groups with another item, like reactions,
    /// - `Some(_)` if the event is rendered in the timeline.
    ///
    /// This is neither a surjection nor an injection. Every timeline item may
    /// not be attached to an event, for example with a virtual timeline item.
    /// We can formulate other rules:
    ///
    /// - a timeline item that doesn't _move_ and that is represented by an
    ///   event has a mapping to an event,
    /// - a virtual timeline item has no mapping to an event.
    ///
    /// Imagine the following remote events:
    ///
    /// | index | remote events |
    /// +-------+---------------+
    /// | 0     | `$ev0`        |
    /// | 1     | `$ev1`        |
    /// | 2     | `$ev2`        |
    /// | 3     | `$ev3`        |
    /// | 4     | `$ev4`        |
    /// | 5     | `$ev5`        |
    ///
    /// Once rendered in a timeline, it for example produces:
    ///
    /// | index | item              | related items        |
    /// +-------+-------------------+----------------------+
    /// | 0     | content of `$ev0` |                      |
    /// | 1     | content of `$ev2` | reaction with `$ev4` |
    /// | 2     | day divider       |                      |
    /// | 3     | content of `$ev3` |                      |
    /// | 4     | content of `$ev5` |                      |
    ///
    /// Note the day divider that is a virtual item. Also note `$ev4` which is
    /// a reaction to `$ev2`. Finally note that `$ev1` is not rendered in
    /// the timeline.
    ///
    /// The mapping between remote event index to timeline item index will look
    /// like this:
    ///
    /// | remote event index | timeline item index | comment                                    |
    /// +--------------------+---------------------+--------------------------------------------+
    /// | 0                  | `Some(0)`           | `$ev0` is rendered as the #0 timeline item |
    /// | 1                  | `None`              | `$ev1` isn't rendered in the timeline      |
    /// | 2                  | `Some(1)`           | `$ev2` is rendered as the #1 timeline item |
    /// | 3                  | `Some(3)`           | `$ev3` is rendered as the #3 timeline item |
    /// | 4                  | `None`              | `$ev4` is a reaction to item #1            |
    /// | 5                  | `Some(4)`           | `$ev5` is rendered as the #4 timeline item |
    ///
    /// Note that the #2 timeline item (the day divider) doesn't map to any
    /// remote event, but if it moves, it has an impact on this mapping.
    pub timeline_item_index: Option<usize>,
}
