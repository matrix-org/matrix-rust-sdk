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
use indexmap::IndexMap;
use matrix_sdk::deserialized_responses::SyncTimelineEvent;
use matrix_sdk_base::deserialized_responses::TimelineEvent;
#[cfg(test)]
use ruma::events::receipt::ReceiptEventContent;
use ruma::{
    events::{
        relation::Annotation, room::redaction::RoomRedactionEventContent,
        AnyMessageLikeEventContent, AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent,
    },
    push::Action,
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
    RoomVersionId, UserId,
};
use tracing::{debug, error, instrument, trace, warn};

use super::{HandleManyEventsResult, ReactionState, TimelineInnerSettings};
use crate::{
    events::SyncTimelineEventWithoutContent,
    timeline::{
        event_handler::{
            DayDividerAdjuster, Flow, HandleEventResult, TimelineEventContext,
            TimelineEventHandler, TimelineEventKind, TimelineItemPosition,
        },
        event_item::EventItemIdentifier,
        polls::PollPendingEvents,
        reactions::{ReactionToggleResult, Reactions},
        read_receipts::ReadReceipts,
        traits::RoomDataProvider,
        util::{rfind_event_by_id, rfind_event_item, RelativePosition},
        AnnotationKey, Error as TimelineError, Profile, ReactionSenderData, TimelineItem,
        TimelineItemKind,
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
    Back {
        /// Did the event come from the cache?
        from_cache: bool,
    },
}

#[derive(Debug)]
pub(in crate::timeline) struct TimelineInnerState {
    pub items: ObservableVector<Arc<TimelineItem>>,
    pub meta: TimelineInnerMetadata,
}

impl TimelineInnerState {
    pub(super) fn new(
        room_version: RoomVersionId,
        unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,
    ) -> Self {
        Self {
            // Upstream default capacity is currently 16, which is making
            // sliding-sync tests with 20 events lag. This should still be
            // small enough.
            items: ObservableVector::with_capacity(32),
            meta: TimelineInnerMetadata::new(room_version, unable_to_decrypt_hook),
        }
    }

    /// Add the given events at the given end of the timeline.
    #[tracing::instrument(skip(self, events, room_data_provider, settings))]
    pub(super) async fn add_events_at<P: RoomDataProvider>(
        &mut self,
        events: Vec<impl Into<SyncTimelineEvent>>,
        position: TimelineEnd,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) -> HandleManyEventsResult {
        if events.is_empty() {
            return Default::default();
        }

        let mut txn = self.transaction();
        let handle_many_res =
            txn.add_events_at(events, position, room_data_provider, settings).await;
        txn.commit();

        handle_many_res
    }

    #[instrument(skip_all)]
    pub(super) async fn handle_sync_events<P: RoomDataProvider>(
        &mut self,
        events: Vec<SyncTimelineEvent>,
        account_data: Vec<Raw<AnyRoomAccountDataEvent>>,
        ephemeral: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) {
        let mut txn = self.transaction();

        txn.add_events_at(
            events,
            TimelineEnd::Back { from_cache: false },
            room_data_provider,
            settings,
        )
        .await;

        trace!("Handling account data");
        for raw_event in account_data {
            match raw_event.deserialize() {
                Ok(AnyRoomAccountDataEvent::FullyRead(ev)) => {
                    txn.set_fully_read_event(ev.content.event_id);
                }
                Ok(_) => {}
                Err(e) => {
                    let event_type = raw_event.get_field::<String>("type").ok().flatten();
                    warn!(event_type, "Failed to deserialize account data: {e}");
                }
            }
        }

        if !ephemeral.is_empty() {
            trace!("Handling ephemeral room events");
            let own_user_id = room_data_provider.own_user_id();
            for raw_event in ephemeral {
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
        }

        txn.commit();
    }

    /// Handle the creation of a new local event.
    pub(super) fn handle_local_event(
        &mut self,
        own_user_id: OwnedUserId,
        own_profile: Option<Profile>,
        txn_id: OwnedTransactionId,
        content: AnyMessageLikeEventContent,
    ) {
        let ctx = TimelineEventContext {
            sender: own_user_id,
            sender_profile: own_profile,
            timestamp: MilliSecondsSinceUnixEpoch::now(),
            is_own_event: true,
            // FIXME: Should we supply something here for encrypted rooms?
            encryption_info: None,
            read_receipts: Default::default(),
            // An event sent by ourself is never matched against push rules.
            is_highlighted: false,
            flow: Flow::Local { txn_id },
        };

        let mut txn = self.transaction();
        TimelineEventHandler::new(&mut txn, ctx)
            .handle_event(TimelineEventKind::Message { content, relations: Default::default() });

        txn.maybe_adjust_day_dividers();

        txn.commit();
    }

    /// Handle the local redaction of an event.
    #[instrument(skip_all)]
    pub(super) fn handle_local_redaction(
        &mut self,
        own_user_id: OwnedUserId,
        own_profile: Option<Profile>,
        txn_id: OwnedTransactionId,
        to_redact: EventItemIdentifier,
        content: RoomRedactionEventContent,
    ) {
        let ctx = TimelineEventContext {
            sender: own_user_id,
            sender_profile: own_profile,
            timestamp: MilliSecondsSinceUnixEpoch::now(),
            is_own_event: true,
            // FIXME: Should we supply something here for encrypted rooms?
            encryption_info: None,
            read_receipts: Default::default(),
            // An event sent by ourself is never matched against push rules.
            is_highlighted: false,
            flow: Flow::Local { txn_id: txn_id.clone() },
        };

        let mut txn = self.transaction();
        let timeline_event_handler = TimelineEventHandler::new(&mut txn, ctx);

        match to_redact {
            EventItemIdentifier::TransactionId(txn_id) => {
                timeline_event_handler.handle_event(TimelineEventKind::LocalRedaction {
                    redacts: txn_id,
                    content: content.clone(),
                });
            }
            EventItemIdentifier::EventId(event_id) => {
                timeline_event_handler
                    .handle_event(TimelineEventKind::Redaction { redacts: event_id, content });
            }
        }

        txn.maybe_adjust_day_dividers();

        txn.commit();
    }

    #[cfg(feature = "e2e-encryption")]
    pub(super) async fn retry_event_decryption<P: RoomDataProvider, Fut>(
        &mut self,
        retry_one: impl Fn(Arc<TimelineItem>) -> Fut,
        retry_indices: Vec<usize>,
        push_rules_context: Option<(ruma::push::Ruleset, ruma::push::PushConditionRoomCtx)>,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) where
        Fut: Future<Output = Option<TimelineEvent>>,
    {
        let mut txn = self.transaction();

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
                )
                .await;

            // If the UTD was removed rather than updated, offset all
            // subsequent loop iterations.
            if handle_one_res.item_removed {
                offset += 1;
            }
        }

        txn.maybe_adjust_day_dividers();

        txn.commit();
    }

    pub(super) fn update_timeline_reaction(
        &mut self,
        own_user_id: &UserId,
        annotation: &Annotation,
        result: &ReactionToggleResult,
    ) -> Result<(), TimelineError> {
        if matches!(result, ReactionToggleResult::RedactSuccess) {
            // We did a successful redaction, so no need to update the item
            // because the reaction is already gone.
            return Ok(());
        }

        let (remote_echo_to_add, local_echo_to_remove) = match result {
            ReactionToggleResult::AddSuccess { event_id, txn_id } => (Some(event_id), Some(txn_id)),
            ReactionToggleResult::AddFailure { txn_id } => (None, Some(txn_id)),
            ReactionToggleResult::RedactSuccess => (None, None),
            ReactionToggleResult::RedactFailure { event_id } => (Some(event_id), None),
        };

        let related = rfind_event_item(&self.items, |it| {
            it.event_id().is_some_and(|it| it == annotation.event_id)
        });

        let Some((idx, related)) = related else {
            // Event isn't found at all.
            warn!("Timeline item not found, can't update reaction ID");
            return Err(TimelineError::FailedToToggleReaction);
        };
        let Some(remote_related) = related.as_remote() else {
            error!("inconsistent state: reaction received on a non-remote event item");
            return Err(TimelineError::FailedToToggleReaction);
        };
        // Note: remote event is not synced yet, so we're adding an item
        // with the local timestamp.
        let reaction_sender_data = ReactionSenderData {
            sender_id: own_user_id.to_owned(),
            timestamp: MilliSecondsSinceUnixEpoch::now(),
        };

        let new_reactions = {
            let mut reactions = remote_related.reactions.clone();
            let reaction_group = reactions.entry(annotation.key.clone()).or_default();

            // Remove the local echo from the related event.
            if let Some(txn_id) = local_echo_to_remove {
                let id = EventItemIdentifier::TransactionId(txn_id.clone());
                if reaction_group.0.swap_remove(&id).is_none() {
                    warn!(
                        "Tried to remove reaction by transaction ID, but didn't \
                         find matching reaction in the related event's reactions"
                    );
                }
            }

            // Add the remote echo to the related event
            if let Some(event_id) = remote_echo_to_add {
                reaction_group.0.insert(
                    EventItemIdentifier::EventId(event_id.clone()),
                    reaction_sender_data.clone(),
                );
            };

            if reaction_group.0.is_empty() {
                reactions.swap_remove(&annotation.key);
            }

            reactions
        };
        let new_related = related.with_kind(remote_related.with_reactions(new_reactions));

        // Update the reactions stored in the timeline state
        {
            // Remove the local echo from reaction_map
            // (should the local echo already be up-to-date after event handling?)
            if let Some(txn_id) = local_echo_to_remove {
                let id = EventItemIdentifier::TransactionId(txn_id.clone());
                if self.meta.reactions.map.remove(&id).is_none() {
                    warn!(
                        "Tried to remove reaction by transaction ID, but didn't \
                     find matching reaction in the reaction map"
                    );
                }
            }
            // Add the remote echo to the reaction_map
            if let Some(event_id) = remote_echo_to_add {
                self.meta.reactions.map.insert(
                    EventItemIdentifier::EventId(event_id.clone()),
                    (reaction_sender_data, annotation.clone()),
                );
            }
        }

        let item = TimelineItem::new(new_related, related.internal_id);
        self.items.set(idx, item);

        Ok(())
    }

    pub(super) fn set_fully_read_event(&mut self, fully_read_event_id: OwnedEventId) {
        let mut txn = self.transaction();
        txn.set_fully_read_event(fully_read_event_id);
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

    fn transaction(&mut self) -> TimelineInnerStateTransaction<'_> {
        let items = self.items.transaction();
        let meta = self.meta.clone();
        TimelineInnerStateTransaction { items, previous_meta: &mut self.meta, meta }
    }
}

pub(in crate::timeline) struct TimelineInnerStateTransaction<'a> {
    /// A vector transaction over the items themselves. Holds temporary state
    /// until committed.
    pub items: ObservableVectorTransaction<'a, Arc<TimelineItem>>,

    /// A clone of the previous meta, that we're operating on during the
    /// transaction, and that will be committed to the previous meta location in
    /// [`Self::commit`].
    pub meta: TimelineInnerMetadata,

    /// Pointer to the previous meta, only used during [`Self::commit`].
    previous_meta: &'a mut TimelineInnerMetadata,
}

impl TimelineInnerStateTransaction<'_> {
    /// Add the given events at the given end of the timeline.
    #[tracing::instrument(skip(self, events, room_data_provider, settings))]
    pub(super) async fn add_events_at<P: RoomDataProvider>(
        &mut self,
        events: Vec<impl Into<SyncTimelineEvent>>,
        position: TimelineEnd,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) -> HandleManyEventsResult {
        let mut total = HandleManyEventsResult::default();

        let position = match position {
            TimelineEnd::Front => TimelineItemPosition::Start,
            TimelineEnd::Back { from_cache } => TimelineItemPosition::End { from_cache },
        };

        for event in events {
            let handle_one_res = self
                .handle_remote_event(event.into(), position, room_data_provider, settings)
                .await;

            total.items_added += handle_one_res.item_added as u64;
            total.items_updated += handle_one_res.items_updated as u64;
        }

        self.maybe_adjust_day_dividers();

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
        settings: &TimelineInnerSettings,
    ) -> HandleEventResult {
        let raw = event.event;
        let (event_id, sender, timestamp, txn_id, event_kind, should_add) = match raw.deserialize()
        {
            Ok(event) => {
                let room_version = room_data_provider.room_version();
                let should_add = (settings.event_filter)(&event, &room_version);
                (
                    event.event_id().to_owned(),
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
                    self.add_event(event_meta, position, room_data_provider, settings).await;

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
                        self.add_event(event_meta, position, room_data_provider, settings).await;
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
        self.add_event(event_meta, position, room_data_provider, settings).await;

        let sender_profile = room_data_provider.profile_from_user_id(&sender).await;
        let ctx = TimelineEventContext {
            sender,
            sender_profile,
            timestamp,
            is_own_event,
            encryption_info: event.encryption_info,
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
                raw_event: raw,
                txn_id,
                position,
                should_add,
            },
        };

        TimelineEventHandler::new(self, ctx).handle_event(event_kind)
    }

    fn clear(&mut self) {
        // By first checking if there are any local echoes first, we do a bit
        // more work in case some are found, but it should be worth it because
        // there will often not be any, and only emitting a single
        // `VectorDiff::Clear` should be much more efficient to process for
        // subscribers.
        if self.items.iter().any(|item| item.is_local_echo()) {
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

        self.meta.all_events.clear();
        self.meta.read_receipts.clear();
        self.meta.reactions.clear();
        self.meta.fully_read_event = None;
        // We forgot about the fully read marker right above, so wait for a new one
        // before attempting to update it for each new timeline item.
        self.meta.has_up_to_date_read_marker_item = true;

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

    fn commit(self) {
        let Self { items, previous_meta, meta } = self;

        // Replace the pointer to the previous meta with the new one.
        *previous_meta = meta;

        items.commit();
    }

    async fn add_event<P: RoomDataProvider>(
        &mut self,
        event_meta: FullEventMeta<'_>,
        position: TimelineItemPosition,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) {
        match position {
            TimelineItemPosition::Start => self.meta.all_events.push_front(event_meta.base_meta()),
            TimelineItemPosition::End { .. } => {
                // Handle duplicated event.
                if let Some(pos) =
                    self.meta.all_events.iter().position(|ev| ev.event_id == event_meta.event_id)
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
            && matches!(position, TimelineItemPosition::Start | TimelineItemPosition::End { .. })
        {
            self.load_read_receipts_for_event(event_meta.event_id, room_data_provider).await;

            self.maybe_add_implicit_read_receipt(event_meta);
        }
    }

    /// Inserts/removes any day dividers that might be missing or superfluous,
    /// according to the events we just handled.
    fn maybe_adjust_day_dividers(&mut self) {
        let mut adjuster = DayDividerAdjuster { items: &mut self.items, meta: &mut self.meta };

        adjuster.maybe_adjust_day_dividers();
    }
}

#[derive(Clone, Debug)]
pub(in crate::timeline) struct TimelineInnerMetadata {
    /// List of all the events as received in the timeline, even the ones that
    /// are discarded in the timeline items.
    pub all_events: VecDeque<EventMeta>,

    /// The next internal identifier for timeline items, used for both local and
    /// remote echoes.
    next_internal_id: u64,

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

    /// The local reaction request state that is queued next.
    pub reaction_state: IndexMap<AnnotationKey, ReactionState>,
    /// The in-flight reaction request state that is ongoing.
    pub in_flight_reaction: IndexMap<AnnotationKey, ReactionState>,

    /// The hook to call whenever we run into a unable-to-decrypt event.
    pub(crate) unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,

    /// Matrix room version of the timeline's room, or a sensible default.
    pub room_version: RoomVersionId,
}

impl TimelineInnerMetadata {
    fn new(
        room_version: RoomVersionId,
        unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,
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
            reaction_state: Default::default(),
            in_flight_reaction: Default::default(),
            room_version,
            unable_to_decrypt_hook,
        }
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
    pub fn next_internal_id(&mut self) -> u64 {
        let val = self.next_internal_id;
        self.next_internal_id += 1;
        val
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
                    // The read marker can't move backwards. Keep the current one.
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
