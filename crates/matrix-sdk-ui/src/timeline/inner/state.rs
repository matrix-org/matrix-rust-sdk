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
    collections::{HashMap, VecDeque},
    future::Future,
    mem::{self, ManuallyDrop},
    ops::{Deref, DerefMut},
    sync::Arc,
};

use eyeball_im::{ObservableVector, ObservableVectorTransaction, ObservableVectorTransactionEntry};
use imbl::Vector;
use indexmap::IndexMap;
use matrix_sdk::{deserialized_responses::SyncTimelineEvent, sync::Timeline};
use matrix_sdk_base::{deserialized_responses::TimelineEvent, sync::JoinedRoom};
#[cfg(test)]
use ruma::events::receipt::ReceiptEventContent;
use ruma::{
    events::{
        receipt::{Receipt, ReceiptType},
        relation::Annotation,
        room::redaction::RoomRedactionEventContent,
        AnyMessageLikeEventContent, AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent,
    },
    push::Action,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
    RoomVersionId, UserId,
};
use tracing::{debug, error, instrument, trace, warn};

use super::{HandleManyEventsResult, ReactionState, TimelineInnerSettings};
use crate::{
    events::SyncTimelineEventWithoutContent,
    timeline::{
        event_handler::{
            Flow, HandleEventResult, TimelineEventContext, TimelineEventHandler, TimelineEventKind,
            TimelineItemPosition,
        },
        event_item::EventItemIdentifier,
        item::timeline_item,
        polls::PollPendingEvents,
        reactions::{ReactionToggleResult, Reactions},
        traits::RoomDataProvider,
        util::{
            find_read_marker, rfind_event_by_id, rfind_event_item, timestamp_to_date,
            RelativePosition,
        },
        AnnotationKey, Error as TimelineError, Profile, ReactionSenderData, TimelineItem,
        TimelineItemKind, VirtualTimelineItem,
    },
};

#[derive(Debug)]
pub(in crate::timeline) struct TimelineInnerState {
    pub items: ObservableVector<Arc<TimelineItem>>,
    pub meta: TimelineInnerMetadata,
}

impl TimelineInnerState {
    pub(super) fn new(room_version: RoomVersionId) -> Self {
        Self {
            // Upstream default capacity is currently 16, which is making
            // sliding-sync tests with 20 events lag. This should still be
            // small enough.
            items: ObservableVector::with_capacity(32),
            meta: TimelineInnerMetadata::new(room_version),
        }
    }

    pub(super) fn back_pagination_token(&self) -> Option<&str> {
        let (_, token) = self.meta.back_pagination_tokens.last()?;
        Some(token)
    }

    #[tracing::instrument(skip_all)]
    pub(super) async fn add_initial_events<P: RoomDataProvider>(
        &mut self,
        events: Vector<SyncTimelineEvent>,
        mut back_pagination_token: Option<String>,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) {
        debug!("Adding {} initial events", events.len());

        let mut txn = self.transaction();
        for event in events {
            txn.handle_remote_event(
                event,
                TimelineItemPosition::End { from_cache: true },
                // back pagination token, if any, is added to the first event
                back_pagination_token.take(),
                room_data_provider,
                settings,
            )
            .await;
        }
        txn.commit();
    }

    pub(super) async fn handle_sync_timeline<P: RoomDataProvider>(
        &mut self,
        timeline: Timeline,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) {
        let mut txn = self.transaction();
        txn.handle_sync_timeline(timeline, room_data_provider, settings).await;
        txn.commit();
    }

    #[instrument(skip_all)]
    pub(super) async fn handle_joined_room_update<P: RoomDataProvider>(
        &mut self,
        update: JoinedRoom,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) {
        let mut txn = self.transaction();
        txn.handle_sync_timeline(update.timeline, room_data_provider, settings).await;

        trace!("Handling account data");
        for raw_event in update.account_data {
            match raw_event.deserialize() {
                Ok(AnyRoomAccountDataEvent::FullyRead(ev)) => {
                    txn.set_fully_read_event(ev.content.event_id);
                }
                Ok(_) => {}
                Err(e) => {
                    warn!("Failed to deserialize account data: {e}");
                }
            }
        }

        if !update.ephemeral.is_empty() {
            trace!("Handling ephemeral room events");
            let own_user_id = room_data_provider.own_user_id();
            for raw_event in update.ephemeral {
                match raw_event.deserialize() {
                    Ok(AnySyncEphemeralRoomEvent::Receipt(ev)) => {
                        txn.handle_explicit_read_receipts(ev.content, own_user_id);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("Failed to deserialize ephemeral event: {e}");
                    }
                }
            }
        }

        txn.commit();
    }

    #[instrument(skip_all)]
    pub(super) async fn handle_back_paginated_events<P: RoomDataProvider>(
        &mut self,
        events: Vec<TimelineEvent>,
        mut back_pagination_token: Option<String>,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) -> Option<HandleManyEventsResult> {
        let mut txn = self.transaction();

        let num_events = events.len();
        let mut total = HandleManyEventsResult::default();
        for (i, event) in events.into_iter().enumerate() {
            // back pagination token is used for the last event in the chunk
            // because back-paginated events have reverse order from sync events
            let token = if i == num_events - 1 { back_pagination_token.take() } else { None };
            let res = txn
                .handle_remote_event(
                    event.into(),
                    TimelineItemPosition::Start,
                    token,
                    room_data_provider,
                    settings,
                )
                .await;

            total.items_added = total.items_added.checked_add(res.item_added as u16)?;
            total.items_updated = total.items_updated.checked_add(res.items_updated)?;
        }

        txn.commit();

        Some(total)
    }

    #[cfg(test)]
    pub(super) async fn handle_live_event<P: RoomDataProvider>(
        &mut self,
        event: SyncTimelineEvent,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) {
        let mut txn = self.transaction();
        txn.handle_live_event(event, None, room_data_provider, settings).await;
        txn.commit();
    }

    /// Handle the creation of a new local event.
    pub(super) fn handle_local_event(
        &mut self,
        own_user_id: OwnedUserId,
        own_profile: Option<Profile>,
        txn_id: OwnedTransactionId,
        content: AnyMessageLikeEventContent,
        settings: &TimelineInnerSettings,
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
        TimelineEventHandler::new(&mut txn, ctx, settings.track_read_receipts)
            .handle_event(TimelineEventKind::Message { content, relations: Default::default() });
        txn.commit();
    }

    /// Handle the local redaction of an event.
    pub(super) fn handle_local_redaction(
        &mut self,
        own_user_id: OwnedUserId,
        own_profile: Option<Profile>,
        txn_id: OwnedTransactionId,
        to_redact: EventItemIdentifier,
        content: RoomRedactionEventContent,
        settings: &TimelineInnerSettings,
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
        let timeline_event_handler =
            TimelineEventHandler::new(&mut txn, ctx, settings.track_read_receipts);

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

            let result = txn
                .handle_remote_event(
                    event.into(),
                    TimelineItemPosition::Update(idx),
                    None,
                    room_data_provider,
                    settings,
                )
                .await;

            // If the UTD was removed rather than updated, offset all
            // subsequent loop iterations.
            if result.item_removed {
                offset += 1;
            }
        }

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
                if reaction_group.0.remove(&id).is_none() {
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
                reactions.remove(&annotation.key);
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

        let item = timeline_item(new_related, related.internal_id);
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
        let items = ManuallyDrop::new(self.items.transaction());
        TimelineInnerStateTransaction { items, meta: &mut self.meta }
    }
}

impl Deref for TimelineInnerState {
    type Target = TimelineInnerMetadata;

    fn deref(&self) -> &Self::Target {
        &self.meta
    }
}

impl DerefMut for TimelineInnerState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.meta
    }
}

pub(in crate::timeline) struct TimelineInnerStateTransaction<'a> {
    pub items: ManuallyDrop<ObservableVectorTransaction<'a, Arc<TimelineItem>>>,
    pub meta: &'a mut TimelineInnerMetadata,
}

impl TimelineInnerStateTransaction<'_> {
    async fn handle_sync_timeline<P: RoomDataProvider>(
        &mut self,
        mut timeline: Timeline,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) {
        if timeline.limited {
            debug!("Got limited sync response, resetting timeline");
            self.clear();
        }

        let num_events = timeline.events.len();
        for (i, event) in timeline.events.into_iter().enumerate() {
            trace!("Handling event {} out of {num_events}", i + 1);
            self.handle_live_event(event, timeline.prev_batch.take(), room_data_provider, settings)
                .await;
        }
    }

    /// Handle a live remote event.
    ///
    /// Shorthand for `handle_remote_event` with a `position` of
    /// `TimelineItemPosition::End { from_cache: false }`.
    async fn handle_live_event<P: RoomDataProvider>(
        &mut self,
        event: SyncTimelineEvent,
        back_pagination_token: Option<String>,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) -> HandleEventResult {
        self.handle_remote_event(
            event,
            TimelineItemPosition::End { from_cache: false },
            back_pagination_token,
            room_data_provider,
            settings,
        )
        .await
    }

    /// Handle a remote event.
    ///
    /// Returns the number of timeline updates that were made.
    async fn handle_remote_event<P: RoomDataProvider>(
        &mut self,
        event: SyncTimelineEvent,
        position: TimelineItemPosition,
        back_pagination_token: Option<String>,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) -> HandleEventResult {
        let should_add_event = &*settings.event_filter;
        let raw = event.event;
        let (event_id, sender, timestamp, txn_id, event_kind, should_add) = match raw.deserialize()
        {
            Ok(event) => {
                let should_add = should_add_event(&event);
                let room_version = room_data_provider.room_version();
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

                    self.add_event(event_id.to_owned(), false, position);

                    return HandleEventResult::default();
                }
                Err(e) => {
                    let event_type: Option<String> = raw.get_field("type").ok().flatten();
                    let event_id: Option<String> = raw.get_field("event_id").ok().flatten();
                    warn!(event_type, event_id, "Failed to deserialize timeline event: {e}");

                    if let Some(Ok(event_id)) = event_id.map(EventId::parse) {
                        self.add_event(event_id.to_owned(), false, position);
                    }

                    return HandleEventResult::default();
                }
            },
        };

        if let Some(token) = back_pagination_token {
            trace!(token, ?event_id, "Adding back-pagination token");
            self.meta.back_pagination_tokens.push((event_id.clone(), token));
        }

        self.add_event(event_id.clone(), should_add, position);

        let is_own_event = sender == room_data_provider.own_user_id();
        let sender_profile = room_data_provider.profile_from_user_id(&sender).await;
        let ctx = TimelineEventContext {
            sender,
            sender_profile,
            timestamp,
            is_own_event,
            encryption_info: event.encryption_info,
            read_receipts: if settings.track_read_receipts {
                self.load_read_receipts_for_event(&event_id, room_data_provider).await
            } else {
                Default::default()
            },
            is_highlighted: event.push_actions.iter().any(Action::is_highlight),
            flow: Flow::Remote { event_id, raw_event: raw, txn_id, position, should_add },
        };

        TimelineEventHandler::new(self, ctx, settings.track_read_receipts).handle_event(event_kind)
    }

    fn clear(&mut self) {
        trace!("Clearing timeline");

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

        self.all_events.clear();
        self.reactions.clear();
        self.fully_read_event = None;
        self.event_should_update_fully_read_marker = false;
        self.back_pagination_tokens.clear();
    }

    #[instrument(skip_all)]
    fn set_fully_read_event(&mut self, fully_read_event_id: OwnedEventId) {
        // A similar event has been handled already. We can ignore it.
        if self.fully_read_event.as_ref().is_some_and(|id| *id == fully_read_event_id) {
            return;
        }

        self.fully_read_event = Some(fully_read_event_id);
        self.meta.update_read_marker(&mut self.items);
    }

    fn commit(mut self) {
        let Self {
            items,
            // meta is just a reference, does not any dropping
            meta: _,
        } = &mut self;

        // Safety: self is forgotten to avoid double free from drop
        let items = unsafe { ManuallyDrop::take(items) };
        mem::forget(self);

        items.commit();
    }
}

impl Drop for TimelineInnerStateTransaction<'_> {
    fn drop(&mut self) {
        warn!("timeline state transaction cancelled");
        // Safety: self.items is not touched anymore, the only other place
        // dropping is Self::commit which makes sure to skip this Drop impl.
        unsafe {
            ManuallyDrop::drop(&mut self.items);
        }
    }
}

impl Deref for TimelineInnerStateTransaction<'_> {
    type Target = TimelineInnerMetadata;

    fn deref(&self) -> &Self::Target {
        self.meta
    }
}

impl DerefMut for TimelineInnerStateTransaction<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.meta
    }
}

#[derive(Debug)]
pub(in crate::timeline) struct TimelineInnerMetadata {
    /// List of all the events as received in the timeline, even the ones that
    /// are discarded in the timeline items.
    all_events: VecDeque<EventMeta>,
    next_internal_id: u64,
    pub reactions: Reactions,
    pub poll_pending_events: PollPendingEvents,
    pub fully_read_event: Option<OwnedEventId>,
    /// Whether the fully-read marker item should try to be updated when an
    /// event is added.
    /// This is currently `true` in two cases:
    /// - The fully-read marker points to an event that is not in the timeline,
    /// - The fully-read marker item would be the last item in the timeline.
    pub event_should_update_fully_read_marker: bool,
    /// User ID => Receipt type => Read receipt of the user of the given
    /// type.
    pub users_read_receipts: HashMap<OwnedUserId, HashMap<ReceiptType, (OwnedEventId, Receipt)>>,
    /// the local reaction request state that is queued next
    pub reaction_state: IndexMap<AnnotationKey, ReactionState>,
    /// the in flight reaction request state that is ongoing
    pub in_flight_reaction: IndexMap<AnnotationKey, ReactionState>,
    pub room_version: RoomVersionId,

    /// Back-pagination tokens, reversed in order compared to the associated
    /// timeline items (to allow efficient pushing and popping).
    ///
    /// Private because it's not needed by `TimelineEventHandler`.
    back_pagination_tokens: Vec<(OwnedEventId, String)>,
}

impl TimelineInnerMetadata {
    fn new(room_version: RoomVersionId) -> TimelineInnerMetadata {
        Self {
            all_events: Default::default(),
            next_internal_id: Default::default(),
            reactions: Default::default(),
            poll_pending_events: Default::default(),
            fully_read_event: Default::default(),
            event_should_update_fully_read_marker: Default::default(),
            users_read_receipts: Default::default(),
            reaction_state: Default::default(),
            in_flight_reaction: Default::default(),
            room_version,
            back_pagination_tokens: Vec::new(),
        }
    }

    fn add_event(&mut self, event_id: OwnedEventId, visible: bool, position: TimelineItemPosition) {
        let meta = EventMeta { event_id, visible };

        match position {
            TimelineItemPosition::Start => self.all_events.push_front(meta),
            TimelineItemPosition::End { .. } => {
                // Handle duplicated event.
                if let Some(pos) =
                    self.all_events.iter().position(|ev| ev.event_id == meta.event_id)
                {
                    self.all_events.remove(pos);
                }

                self.all_events.push_back(meta);
            }
            #[cfg(feature = "e2e-encryption")]
            TimelineItemPosition::Update(_) => {
                if let Some(event) =
                    self.all_events.iter_mut().find(|e| e.event_id == meta.event_id)
                {
                    event.visible = visible;
                }
            }
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

    pub fn next_internal_id(&mut self) -> u64 {
        let val = self.next_internal_id;
        self.next_internal_id += 1;
        val
    }

    pub fn new_timeline_item(&mut self, kind: impl Into<TimelineItemKind>) -> Arc<TimelineItem> {
        timeline_item(kind, self.next_internal_id())
    }

    /// Returns a new day divider item for the new timestamp if it is on a
    /// different day than the old timestamp
    pub fn maybe_create_day_divider_from_timestamps(
        &mut self,
        old_ts: MilliSecondsSinceUnixEpoch,
        new_ts: MilliSecondsSinceUnixEpoch,
    ) -> Option<Arc<TimelineItem>> {
        (timestamp_to_date(old_ts) != timestamp_to_date(new_ts))
            .then(|| self.new_timeline_item(VirtualTimelineItem::DayDivider(new_ts)))
    }

    pub(crate) fn update_read_marker(
        &mut self,
        items: &mut ObservableVectorTransaction<'_, Arc<TimelineItem>>,
    ) {
        let Some(fully_read_event) = &self.fully_read_event else { return };
        trace!(?fully_read_event, "Updating read marker");

        let read_marker_idx = find_read_marker(items);
        let fully_read_event_idx = rfind_event_by_id(items, fully_read_event).map(|(idx, _)| idx);

        match (read_marker_idx, fully_read_event_idx) {
            (None, None) => {
                self.event_should_update_fully_read_marker = true;
            }
            (None, Some(idx)) => {
                // We don't want to insert the read marker if it is at the end of the timeline.
                if idx + 1 < items.len() {
                    self.event_should_update_fully_read_marker = false;
                    items.insert(idx + 1, TimelineItem::read_marker());
                } else {
                    self.event_should_update_fully_read_marker = true;
                }
            }
            (Some(_), None) => {
                // Keep the current position of the read marker, hopefully we
                // should have a new position later.
                self.event_should_update_fully_read_marker = true;
            }
            (Some(from), Some(to)) => {
                self.event_should_update_fully_read_marker = false;

                // The read marker can't move backwards.
                if from < to {
                    let item = items.remove(from);

                    // We don't want to re-insert the read marker if it is at the end of the
                    // timeline.
                    if to < items.len() {
                        // Since the fully-read event's index was shifted to the left
                        // by one position by the remove call above, insert the fully-
                        // read marker at its previous position, rather than that + 1
                        items.insert(to, item);
                    } else {
                        self.event_should_update_fully_read_marker = true;
                    }
                }
            }
        }
    }
}

/// Metadata about an event.
#[derive(Debug, Clone)]
struct EventMeta {
    /// The ID of the event.
    event_id: OwnedEventId,
    /// Whether the event is among the timeline items.
    visible: bool,
}
