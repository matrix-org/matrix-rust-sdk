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

use std::{collections::HashMap, sync::Arc};

use eyeball_im::ObservableVector;
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
            update_read_marker, Flow, HandleEventResult, TimelineEventContext,
            TimelineEventHandler, TimelineEventKind, TimelineItemPosition,
        },
        event_item::EventItemIdentifier,
        item::timeline_item,
        reactions::{ReactionToggleResult, Reactions},
        rfind_event_item,
        traits::RoomDataProvider,
        AnnotationKey, Error as TimelineError, Profile, ReactionSenderData, TimelineItem,
    },
};

#[derive(Debug)]
pub(in crate::timeline) struct TimelineInnerState {
    pub items: ObservableVector<Arc<TimelineItem>>,
    pub next_internal_id: u64,
    pub reactions: Reactions,
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

impl TimelineInnerState {
    pub(super) fn new(room_version: RoomVersionId) -> Self {
        Self {
            // Upstream default capacity is currently 16, which is making
            // sliding-sync tests with 20 events lag. This should still be
            // small enough.
            items: ObservableVector::with_capacity(32),
            next_internal_id: Default::default(),
            reactions: Default::default(),
            fully_read_event: Default::default(),
            event_should_update_fully_read_marker: Default::default(),
            users_read_receipts: Default::default(),
            reaction_state: Default::default(),
            in_flight_reaction: Default::default(),
            room_version,
        }
    }

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
                (
                    event.event_id().to_owned(),
                    event.sender().to_owned(),
                    event.origin_server_ts(),
                    event.transaction_id().map(ToOwned::to_owned),
                    event.into(),
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
        let encryption_info = event.encryption_info;
        let sender_profile = room_data_provider.profile(&sender).await;
        let read_receipts = if settings.track_read_receipts {
            self.load_read_receipts_for_event(&event_id, room_data_provider).await
        } else {
            Default::default()
        };
        let is_highlighted = event.push_actions.iter().any(Action::is_highlight);
        let ctx = TimelineEventContext {
            sender,
            sender_profile,
            timestamp,
            is_own_event,
            encryption_info,
            read_receipts,
            is_highlighted,
        };
        let flow = Flow::Remote { event_id, raw_event: raw, txn_id, position, should_add };

        TimelineEventHandler::new(ctx, flow, self, settings.track_read_receipts)
            .handle_event(event_kind)
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
        };

        let flow = Flow::Local { txn_id };
        let kind = TimelineEventKind::Message { content, relations: Default::default() };

        TimelineEventHandler::new(ctx, flow, self, settings.track_read_receipts).handle_event(kind);
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
        let flow = Flow::Local { txn_id: txn_id.clone() };
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
        };

        match to_redact {
            EventItemIdentifier::TransactionId(txn_id) => {
                let kind =
                    TimelineEventKind::LocalRedaction { redacts: txn_id, content: content.clone() };

                TimelineEventHandler::new(ctx, flow, self, settings.track_read_receipts)
                    .handle_event(kind);
            }
            EventItemIdentifier::EventId(event_id) => {
                let kind = TimelineEventKind::Redaction { redacts: event_id, content };

                TimelineEventHandler::new(ctx, flow, self, settings.track_read_receipts)
                    .handle_event(kind);
            }
        }
    }

    pub(super) fn clear(&mut self) {
        self.items.clear();
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

        update_read_marker(
            &mut self.items,
            self.fully_read_event.as_deref(),
            &mut self.event_should_update_fully_read_marker,
        );
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
                if self.reactions.map.remove(&id).is_none() {
                    warn!(
                        "Tried to remove reaction by transaction ID, but didn't \
                     find matching reaction in the reaction map"
                    );
                }
            }
            // Add the remote echo to the reaction_map
            if let Some(event_id) = remote_echo_to_add {
                self.reactions.map.insert(
                    EventItemIdentifier::EventId(event_id.clone()),
                    (reaction_sender_data, annotation.clone()),
                );
            }
        }

        self.items.set(idx, timeline_item(new_related, related.internal_id));

        Ok(())
    }
}
