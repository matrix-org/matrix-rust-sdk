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

use std::{collections::VecDeque, future::Future, sync::Arc};

use eyeball_im::{ObservableVector, ObservableVectorTransaction, ObservableVectorTransactionEntry};
use matrix_sdk::{deserialized_responses::SyncTimelineEvent, send_queue::SendHandle};
use matrix_sdk_base::deserialized_responses::TimelineEvent;
#[cfg(test)]
use ruma::events::receipt::ReceiptEventContent;
use ruma::{
    events::AnySyncEphemeralRoomEvent, push::Action, serde::Raw, EventId,
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId, RoomVersionId,
    UserId,
};
use tracing::{debug, instrument, trace, warn};

use super::{HandleManyEventsResult, TimelineFocusKind, TimelineSettings};
use crate::{
    events::SyncTimelineEventWithoutContent,
    timeline::{
        day_dividers::DayDividerAdjuster,
        event_handler::{
            Flow, HandleEventResult, TimelineEventContext, TimelineEventHandler, TimelineEventKind,
            TimelineItemPosition,
        },
        event_item::RemoteEventOrigin,
        polls::PollPendingEvents,
        reactions::Reactions,
        read_receipts::ReadReceipts,
        traits::RoomDataProvider,
        util::{rfind_event_by_id, RelativePosition},
        Profile, TimelineItem, TimelineItemKind,
    },
    unable_to_decrypt_hook::UtdHookManager,
};

/// Which end of the timeline should an event be added to?
///
/// This is a simplification of `TimelineItemPosition` which doesn't contain the
/// `Update` variant, when adding a bunch of events at the same time.
#[derive(Debug)]
pub(crate) enum TimelineEnd {
    /// Event should be prepended to the front of the timeline.
    Front,
    /// Event should appended to the back of the timeline.
    Back,
}

#[derive(Debug)]
pub(in crate::timeline) struct TimelineState {
    pub items: ObservableVector<Arc<TimelineItem>>,
    pub meta: TimelineMetadata,

    /// The kind of focus of this timeline.
    timeline_focus: TimelineFocusKind,
}

impl TimelineState {
    pub(super) fn new(
        timeline_focus: TimelineFocusKind,
        room_version: RoomVersionId,
        internal_id_prefix: Option<String>,
        unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,
        is_room_encrypted: bool,
    ) -> Self {
        Self {
            // Upstream default capacity is currently 16, which is making
            // sliding-sync tests with 20 events lag. This should still be
            // small enough.
            items: ObservableVector::with_capacity(32),
            meta: TimelineMetadata::new(
                room_version,
                internal_id_prefix,
                unable_to_decrypt_hook,
                is_room_encrypted,
            ),
            timeline_focus,
        }
    }

    /// Add the given remove events at the given end of the timeline.
    ///
    /// Note: when the `position` is [`TimelineEnd::Front`], prepended events
    /// should be ordered in *reverse* topological order, that is, `events[0]`
    /// is the most recent.
    #[tracing::instrument(skip(self, events, room_data_provider, settings))]
    pub(super) async fn add_remote_events_at<P: RoomDataProvider>(
        &mut self,
        events: Vec<impl Into<SyncTimelineEvent>>,
        position: TimelineEnd,
        origin: RemoteEventOrigin,
        room_data_provider: &P,
        settings: &TimelineSettings,
    ) -> HandleManyEventsResult {
        if events.is_empty() {
            return Default::default();
        }

        let mut txn = self.transaction();
        let handle_many_res =
            txn.add_remote_events_at(events, position, origin, room_data_provider, settings).await;
        txn.commit();

        handle_many_res
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
    #[instrument(skip_all)]
    pub(super) async fn handle_local_event(
        &mut self,
        own_user_id: OwnedUserId,
        own_profile: Option<Profile>,
        should_add_new_items: bool,
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

        let mut day_divider_adjuster = DayDividerAdjuster::default();

        TimelineEventHandler::new(&mut txn, ctx)
            .handle_event(
                &mut day_divider_adjuster,
                content,
                // Local events are never UTD, so no need to pass in a raw_event - this is only
                // used to determine the type of UTD if there is one.
                None,
            )
            .await;

        txn.adjust_day_dividers(day_divider_adjuster);

        txn.commit();
    }

    #[cfg(feature = "e2e-encryption")]
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

        let mut day_divider_adjuster = DayDividerAdjuster::default();

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
                push_rules.get_actions(&event.event, push_context).to_owned()
            });

            let handle_one_res = txn
                .handle_remote_event(
                    event.into(),
                    TimelineItemPosition::Update(idx),
                    room_data_provider,
                    settings,
                    &mut day_divider_adjuster,
                )
                .await;

            // If the UTD was removed rather than updated, offset all
            // subsequent loop iterations.
            if handle_one_res.item_removed {
                offset += 1;
            }
        }

        txn.adjust_day_dividers(day_divider_adjuster);

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
    pub items: ObservableVectorTransaction<'a, Arc<TimelineItem>>,

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
    pub(super) async fn add_remote_events_at<P: RoomDataProvider>(
        &mut self,
        events: Vec<impl Into<SyncTimelineEvent>>,
        position: TimelineEnd,
        origin: RemoteEventOrigin,
        room_data_provider: &P,
        settings: &TimelineSettings,
    ) -> HandleManyEventsResult {
        let mut total = HandleManyEventsResult::default();

        let position = match position {
            TimelineEnd::Front => TimelineItemPosition::Start { origin },
            TimelineEnd::Back => TimelineItemPosition::End { origin },
        };

        let mut day_divider_adjuster = DayDividerAdjuster::default();

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
                    &mut day_divider_adjuster,
                )
                .await;

            total.items_added += handle_one_res.item_added as u64;
            total.items_updated += handle_one_res.items_updated as u64;
        }

        self.adjust_day_dividers(day_divider_adjuster);

        total
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
        day_divider_adjuster: &mut DayDividerAdjuster,
    ) -> HandleEventResult {
        let raw = event.event;
        let (event_id, sender, timestamp, txn_id, event_kind, should_add) = match raw.deserialize()
        {
            Ok(event) => {
                let event_id = event.event_id().to_owned();
                let room_version = room_data_provider.room_version();

                let mut should_add = (settings.event_filter)(&event, &room_version);

                if should_add {
                    // Retrieve the origin of the event.
                    let origin = match position {
                        TimelineItemPosition::End { origin }
                        | TimelineItemPosition::Start { origin } => origin,

                        TimelineItemPosition::Update(idx) => self
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
                    TimelineEventKind::from_event(event, &room_version),
                    should_add,
                )
            }

            Err(e) => match raw.deserialize_as::<SyncTimelineEventWithoutContent>() {
                Ok(event) if settings.add_failed_to_parse => (
                    event.event_id().to_owned(),
                    event.sender().to_owned(),
                    event.origin_server_ts(),
                    event.transaction_id().map(ToOwned::to_owned),
                    TimelineEventKind::failed_to_parse(event, e),
                    true,
                ),

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
                    let _event_added_or_updated = self
                        .add_or_update_event(event_meta, position, room_data_provider, settings)
                        .await;

                    return HandleEventResult::default();
                }

                Err(e) => {
                    let event_type: Option<String> = raw.get_field("type").ok().flatten();
                    let event_id: Option<String> = raw.get_field("event_id").ok().flatten();
                    warn!(event_type, event_id, "Failed to deserialize timeline event: {e}");

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
                        let _event_added_or_updated = self
                            .add_or_update_event(event_meta, position, room_data_provider, settings)
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

        let event_added_or_updated =
            self.add_or_update_event(event_meta, position, room_data_provider, settings).await;

        // If the event has not been added or updated, it's because it's a duplicated
        // event. Let's return early.
        if !event_added_or_updated {
            return HandleEventResult::default();
        }

        let sender_profile = room_data_provider.profile_from_user_id(&sender).await;
        let ctx = TimelineEventContext {
            sender,
            sender_profile,
            timestamp,
            is_own_event,
            read_receipts: if settings.track_read_receipts && should_add {
                self.meta.read_receipts.compute_event_receipts(
                    &event_id,
                    &self.meta.all_events,
                    matches!(position, TimelineItemPosition::End { .. }),
                )
            } else {
                Default::default()
            },
            is_highlighted: event.push_actions.iter().any(Action::is_highlight),
            flow: Flow::Remote {
                event_id: event_id.clone(),
                raw_event: raw.clone(),
                encryption_info: event.encryption_info,
                txn_id,
                position,
            },
            should_add_new_items: should_add,
        };

        TimelineEventHandler::new(self, ctx)
            .handle_event(day_divider_adjuster, event_kind, Some(&raw))
            .await
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
                    ObservableVectorTransactionEntry::remove(entry);
                }
            });

            // Remove stray day dividers
            let mut idx = 0;
            while idx < self.items.len() {
                if self.items[idx].is_day_divider()
                    && self.items.get(idx + 1).map_or(true, |item| item.is_day_divider())
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

        // Only clear the internal counter if there are no local echoes. Otherwise, we
        // might end up reusing the same internal id for a local echo and
        // another item.
        let reset_internal_id = !has_local_echoes;
        self.meta.clear(reset_internal_id);

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

    /// Add or update an event in the [`TimelineMetadata::all_events`]
    /// collection.
    ///
    /// This method also adjusts read receipt if needed.
    ///
    /// It returns `true` if the event has been added or updated, `false`
    /// otherwise. The latter happens if the event already exists, i.e. if
    /// an existing event is requested to be added.
    async fn add_or_update_event<P: RoomDataProvider>(
        &mut self,
        event_meta: FullEventMeta<'_>,
        position: TimelineItemPosition,
        room_data_provider: &P,
        settings: &TimelineSettings,
    ) -> bool {
        // Detect if an event already exists in [`TimelineMetadata::all_events`].
        //
        // Returns its position, in this case.
        fn event_already_exists(
            new_event_id: &EventId,
            all_events: &VecDeque<EventMeta>,
        ) -> Option<usize> {
            all_events.iter().position(|EventMeta { event_id, .. }| event_id == new_event_id)
        }

        match position {
            TimelineItemPosition::Start { .. } => {
                if let Some(pos) = event_already_exists(event_meta.event_id, &self.meta.all_events)
                {
                    self.meta.all_events.remove(pos);
                }

                self.meta.all_events.push_front(event_meta.base_meta())
            }

            TimelineItemPosition::End { .. } => {
                if let Some(pos) = event_already_exists(event_meta.event_id, &self.meta.all_events)
                {
                    self.meta.all_events.remove(pos);
                }

                self.meta.all_events.push_back(event_meta.base_meta());
            }

            #[cfg(feature = "e2e-encryption")]
            TimelineItemPosition::Update(_) => {
                if let Some(event) =
                    self.meta.all_events.iter_mut().find(|e| e.event_id == event_meta.event_id)
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

        true
    }

    fn adjust_day_dividers(&mut self, mut adjuster: DayDividerAdjuster) {
        adjuster.run(&mut self.items, &mut self.meta);
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

    pub(crate) is_room_encrypted: bool,

    /// Matrix room version of the timeline's room, or a sensible default.
    ///
    /// This value is constant over the lifetime of the metadata.
    pub room_version: RoomVersionId,

    // **** DYNAMIC FIELDS ****
    /// The next internal identifier for timeline items, used for both local and
    /// remote echoes.
    next_internal_id: u64,

    /// List of all the events as received in the timeline, even the ones that
    /// are discarded in the timeline items.
    pub all_events: VecDeque<EventMeta>,

    pub reactions: Reactions,
    pub poll_pending_events: PollPendingEvents,
    pub fully_read_event: Option<OwnedEventId>,

    /// Whether we have a fully read-marker item in the timeline, that's up to
    /// date with the room's read marker.
    ///
    /// This is false when:
    /// - The fully-read marker points to an event that is not in the timeline,
    /// - The fully-read marker item would be the last item in the timeline.
    pub has_up_to_date_read_marker_item: bool,

    pub read_receipts: ReadReceipts,
}

impl TimelineMetadata {
    pub(crate) fn new(
        room_version: RoomVersionId,
        internal_id_prefix: Option<String>,
        unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,
        is_room_encrypted: bool,
    ) -> Self {
        Self {
            all_events: Default::default(),
            next_internal_id: Default::default(),
            reactions: Default::default(),
            poll_pending_events: Default::default(),
            fully_read_event: Default::default(),
            // It doesn't make sense to set this to false until we fill the `fully_read_event`
            // field, otherwise we'll keep on exiting early in `Self::update_read_marker`.
            has_up_to_date_read_marker_item: true,
            read_receipts: Default::default(),
            room_version,
            unable_to_decrypt_hook,
            internal_id_prefix,
            is_room_encrypted,
        }
    }

    pub(crate) fn clear(&mut self, reset_internal_id: bool) {
        if reset_internal_id {
            self.next_internal_id = 0;
        }
        self.all_events.clear();
        self.reactions.clear();
        self.poll_pending_events.clear();
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
    ) -> Option<RelativePosition> {
        if event_a == event_b {
            return Some(RelativePosition::Same);
        }

        // We can make early returns here because we know all events since the end of
        // the timeline, so the first event encountered is the oldest one.
        for meta in self.all_events.iter().rev() {
            if meta.event_id == event_a {
                return Some(RelativePosition::Before);
            }
            if meta.event_id == event_b {
                return Some(RelativePosition::After);
            }
        }

        None
    }

    /// Returns the next internal id for a timeline item (and increment our
    /// internal counter).
    fn next_internal_id(&mut self) -> String {
        let val = self.next_internal_id;
        self.next_internal_id += 1;
        let prefix = self.internal_id_prefix.as_deref().unwrap_or("");
        format!("{prefix}{val}")
    }

    /// Returns a new timeline item with a fresh internal id.
    pub fn new_timeline_item(&mut self, kind: impl Into<TimelineItemKind>) -> Arc<TimelineItem> {
        TimelineItem::new(kind, self.next_internal_id())
    }

    /// Try to update the read marker item in the timeline.
    pub(crate) fn update_read_marker(
        &mut self,
        items: &mut ObservableVectorTransaction<'_, Arc<TimelineItem>>,
    ) {
        let Some(fully_read_event) = &self.fully_read_event else { return };
        trace!(?fully_read_event, "Updating read marker");

        let read_marker_idx = items.iter().rposition(|item| item.is_read_marker());
        let fully_read_event_idx = rfind_event_by_id(items, fully_read_event).map(|(idx, _)| idx);

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
                    items.insert(idx + 1, TimelineItem::read_marker());
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
                    items.insert(to, read_marker);
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

impl<'a> FullEventMeta<'a> {
    fn base_meta(&self) -> EventMeta {
        EventMeta { event_id: self.event_id.to_owned(), visible: self.visible }
    }
}

/// Metadata about an event that needs to be kept in memory.
#[derive(Debug, Clone)]
pub(crate) struct EventMeta {
    /// The ID of the event.
    pub event_id: OwnedEventId,
    /// Whether the event is among the timeline items.
    pub visible: bool,
}
