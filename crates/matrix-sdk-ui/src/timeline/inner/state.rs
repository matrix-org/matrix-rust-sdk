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
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use eyeball_im::{ObservableVector, ObservableVectorTransaction, ObservableVectorTransactionEntry};
use indexmap::IndexMap;
use matrix_sdk::{deserialized_responses::SyncTimelineEvent, sync::Timeline};
use ruma::{
    events::{
        receipt::{Receipt, ReceiptType},
        relation::Annotation,
        room::redaction::RoomRedactionEventContent,
        AnyMessageLikeEventContent,
    },
    push::Action,
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId, RoomVersionId,
    UserId,
};
use tracing::{debug, error, instrument, trace, warn};

use super::{ReactionState, TimelineInnerSettings};
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
        util::{find_read_marker, rfind_event_by_id, rfind_event_item, timestamp_to_date},
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

        TimelineEventHandler::new(&mut self.transaction(), ctx, settings.track_read_receipts)
            .handle_event(TimelineEventKind::Message { content, relations: Default::default() });
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

        let mut state = self.transaction();
        let timeline_event_handler =
            TimelineEventHandler::new(&mut state, ctx, settings.track_read_receipts);

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

    pub fn transaction(&mut self) -> TimelineInnerStateTransaction<'_> {
        TimelineInnerStateTransaction {
            items: ManuallyDrop::new(self.items.transaction()),
            meta: &mut self.meta,
        }
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
    pub async fn handle_sync_timeline<P: RoomDataProvider>(
        &mut self,
        timeline: Timeline,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) {
        if timeline.limited {
            debug!("Got limited sync response, resetting timeline");
            self.clear();
        }

        let num_events = timeline.events.len();
        for (i, event) in timeline.events.into_iter().enumerate() {
            trace!("Handling event {i} out of {num_events}");
            self.handle_live_event(event, room_data_provider, settings).await;
        }
    }

    /// Handle a live remote event.
    ///
    /// Shorthand for `handle_remote_event` with a `position` of
    /// `TimelineItemPosition::End { from_cache: false }`.
    pub(super) async fn handle_live_event<P: RoomDataProvider>(
        &mut self,
        event: SyncTimelineEvent,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) -> HandleEventResult {
        self.handle_remote_event(
            event,
            TimelineItemPosition::End { from_cache: false },
            room_data_provider,
            settings,
        )
        .await
    }

    /// Handle a remote event.
    ///
    /// Returns the number of timeline updates that were made.
    pub(super) async fn handle_remote_event<P: RoomDataProvider>(
        &mut self,
        event: SyncTimelineEvent,
        position: TimelineItemPosition,
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
                    return HandleEventResult::default();
                }
                Err(e) => {
                    let event_type: Option<String> = raw.get_field("type").ok().flatten();
                    let event_id: Option<String> = raw.get_field("event_id").ok().flatten();
                    warn!(event_type, event_id, "Failed to deserialize timeline event: {e}");
                    return HandleEventResult::default();
                }
            },
        };

        let is_own_event = sender == room_data_provider.own_user_id();
        let sender_profile = room_data_provider.profile(&sender).await;
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

    pub(super) fn clear(&mut self) {
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

        self.reactions.clear();
        self.fully_read_event = None;
        self.event_should_update_fully_read_marker = false;
    }

    #[instrument(skip_all)]
    pub(super) fn set_fully_read_event(&mut self, fully_read_event_id: OwnedEventId) {
        // A similar event has been handled already. We can ignore it.
        if self.fully_read_event.as_ref().is_some_and(|id| *id == fully_read_event_id) {
            return;
        }

        self.fully_read_event = Some(fully_read_event_id);
        self.meta.update_read_marker(&mut self.items);
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

impl Drop for TimelineInnerStateTransaction<'_> {
    fn drop(&mut self) {
        // Safety: self.items is not touched again
        let txn = unsafe { ManuallyDrop::take(&mut self.items) };
        txn.commit();
    }
}

#[derive(Debug)]
pub(in crate::timeline) struct TimelineInnerMetadata {
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
}

impl TimelineInnerMetadata {
    fn new(room_version: RoomVersionId) -> TimelineInnerMetadata {
        Self {
            next_internal_id: Default::default(),
            reactions: Default::default(),
            poll_pending_events: Default::default(),
            fully_read_event: Default::default(),
            event_should_update_fully_read_marker: Default::default(),
            users_read_receipts: Default::default(),
            reaction_state: Default::default(),
            in_flight_reaction: Default::default(),
            room_version,
        }
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
