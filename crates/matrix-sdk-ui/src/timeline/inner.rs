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

#[cfg(feature = "e2e-encryption")]
use std::collections::BTreeSet;
use std::{collections::HashMap, fmt, sync::Arc};

use eyeball_im::{ObservableVector, VectorSubscriber};
#[cfg(any(test, feature = "testing"))]
use eyeball_im_util::{FilterMapVectorSubscriber, VectorExt};
use imbl::Vector;
use indexmap::{IndexMap, IndexSet};
use itertools::Itertools;
#[cfg(all(test, feature = "e2e-encryption"))]
use matrix_sdk::crypto::OlmMachine;
use matrix_sdk::{
    deserialized_responses::{SyncTimelineEvent, TimelineEvent},
    room,
    sync::{JoinedRoom, Timeline},
    Error, Result,
};
#[cfg(test)]
use ruma::events::receipt::ReceiptEventContent;
#[cfg(all(test, feature = "e2e-encryption"))]
use ruma::RoomId;
use ruma::{
    api::client::receipt::create_receipt::v3::ReceiptType as SendReceiptType,
    events::{
        fully_read::FullyReadEvent,
        reaction::ReactionEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        relation::Annotation,
        room::redaction::RoomRedactionEventContent,
        AnyMessageLikeEventContent, AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent,
        AnySyncTimelineEvent,
    },
    push::Action,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
    TransactionId, UserId,
};
use tokio::sync::{Mutex, MutexGuard};
use tracing::{debug, error, field::debug, info, instrument, trace, warn};
#[cfg(feature = "e2e-encryption")]
use tracing::{field, info_span, Instrument as _};

#[cfg(feature = "e2e-encryption")]
use super::traits::Decryptor;
use super::{
    compare_events_positions,
    event_handler::{
        update_read_marker, Flow, HandleEventResult, TimelineEventHandler, TimelineEventKind,
        TimelineEventMetadata, TimelineItemPosition,
    },
    event_item::{EventItemIdentifier, ReactionSenderData},
    reactions::ReactionToggleResult,
    rfind_event_by_id, rfind_event_item, timeline_item,
    traits::RoomDataProvider,
    AnnotationKey, EventSendState, EventTimelineItem, InReplyToDetails, Message, Profile,
    RelativePosition, RepliedToEvent, TimelineDetails, TimelineItem, TimelineItemContent,
    TimelineItemKind,
};
use crate::{events::SyncTimelineEventWithoutContent, timeline::new_timeline_item};

#[derive(Clone, Debug)]
pub(super) struct TimelineInner<P: RoomDataProvider = room::Common> {
    state: Arc<Mutex<TimelineInnerState>>,
    room_data_provider: P,
    settings: TimelineInnerSettings,
}

#[derive(Debug, Default)]
pub(super) struct TimelineInnerState {
    pub(super) items: ObservableVector<Arc<TimelineItem>>,
    pub(super) next_internal_id: u64,
    /// Reaction event / txn ID => sender and reaction data.
    pub(super) reaction_map: HashMap<EventItemIdentifier, (ReactionSenderData, Annotation)>,
    /// ID of event that is not in the timeline yet => List of reaction event
    /// IDs.
    pub(super) pending_reactions: HashMap<OwnedEventId, IndexSet<OwnedEventId>>,
    pub(super) fully_read_event: Option<OwnedEventId>,
    /// Whether the fully-read marker item should try to be updated when an
    /// event is added.
    /// This is currently `true` in two cases:
    /// - The fully-read marker points to an event that is not in the timeline,
    /// - The fully-read marker item would be the last item in the timeline.
    pub(super) event_should_update_fully_read_marker: bool,
    /// User ID => Receipt type => Read receipt of the user of the given type.
    pub(super) users_read_receipts:
        HashMap<OwnedUserId, HashMap<ReceiptType, (OwnedEventId, Receipt)>>,
    /// the local reaction request state that is queued next
    pub(super) reaction_state: IndexMap<AnnotationKey, ReactionState>,
    /// the in flight reaction request state that is ongoing
    pub(super) in_flight_reaction: IndexMap<AnnotationKey, ReactionState>,
}

#[derive(Debug, Clone)]
pub(super) enum ReactionAction {
    /// Request already in progress so allow that one to resolve
    None,

    /// Send this reaction to the server
    SendRemote(OwnedTransactionId),

    /// Redact this reaction from the server
    RedactRemote(OwnedEventId),
}

#[derive(Debug, Clone)]
pub(super) enum ReactionState {
    Redacting(Option<OwnedEventId>),
    Sending(OwnedTransactionId),
}

#[derive(Clone)]
pub(super) struct TimelineInnerSettings {
    pub(super) track_read_receipts: bool,
    pub(super) event_filter: Arc<TimelineEventFilterFn>,
    pub(super) add_failed_to_parse: bool,
}

impl fmt::Debug for TimelineInnerSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TimelineInnerSettings")
            .field("track_read_receipts", &self.track_read_receipts)
            .field("add_failed_to_parse", &self.add_failed_to_parse)
            .finish_non_exhaustive()
    }
}

impl Default for TimelineInnerSettings {
    fn default() -> Self {
        Self {
            track_read_receipts: false,
            event_filter: Arc::new(|_| true),
            add_failed_to_parse: true,
        }
    }
}

pub(super) type TimelineEventFilterFn = dyn Fn(&AnySyncTimelineEvent) -> bool + Send + Sync;

impl<P: RoomDataProvider> TimelineInner<P> {
    pub(super) fn new(room_data_provider: P) -> Self {
        let state = TimelineInnerState {
            // Upstream default capacity is currently 16, which is making
            // sliding-sync tests with 20 events lag. This should still be
            // small enough.
            items: ObservableVector::with_capacity(32),
            ..Default::default()
        };
        Self {
            state: Arc::new(Mutex::new(state)),
            room_data_provider,
            settings: TimelineInnerSettings::default(),
        }
    }

    pub(super) fn with_settings(mut self, settings: TimelineInnerSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Get a copy of the current items in the list.
    ///
    /// Cheap because `im::Vector` is cheap to clone.
    pub(super) async fn items(&self) -> Vector<Arc<TimelineItem>> {
        self.state.lock().await.items.clone()
    }

    pub(super) async fn subscribe(
        &self,
    ) -> (Vector<Arc<TimelineItem>>, VectorSubscriber<Arc<TimelineItem>>) {
        trace!("Creating timeline items signal");
        let state = self.state.lock().await;
        // auto-deref to the inner vector's clone method
        let items = state.items.clone();
        let stream = state.items.subscribe();
        (items, stream)
    }

    #[cfg(any(test, feature = "testing"))]
    pub(super) async fn subscribe_filter_map<U, F>(
        &self,
        f: F,
    ) -> (Vector<U>, FilterMapVectorSubscriber<Arc<TimelineItem>, F>)
    where
        U: Clone,
        F: Fn(Arc<TimelineItem>) -> Option<U>,
    {
        trace!("Creating timeline items signal");
        let state = self.state.lock().await;
        state.items.subscribe_filter_map(f)
    }

    pub(super) async fn toggle_reaction_local(
        &self,
        annotation: &Annotation,
    ) -> Result<ReactionAction, super::Error> {
        let mut state = self.state.lock().await;

        let user_id = self.room_data_provider.own_user_id();

        let related_event = {
            let items = state.items.clone();
            let (_, item) = rfind_event_by_id(&items, &annotation.event_id)
                .ok_or(super::Error::FailedToToggleReaction)?;
            item.to_owned()
        };

        let (to_redact_local, to_redact_remote) = {
            let reactions = related_event.reactions();

            let user_reactions =
                reactions.get(&annotation.key).map(|group| group.by_sender(user_id));

            user_reactions
                .map(|reactions| {
                    let reactions = reactions.collect_vec();
                    let local = reactions.iter().find_map(|(txid, _event_id)| *txid);
                    let remote = reactions.iter().find_map(|(_txid, event_id)| *event_id);
                    (local, remote)
                })
                .unwrap_or((None, None))
        };

        let sender = self.room_data_provider.own_user_id().to_owned();
        let sender_profile = self.room_data_provider.profile(&sender).await;
        let reaction_state = match (to_redact_local, to_redact_remote) {
            (None, None) => {
                // No record of the reaction, create a local echo

                let in_flight = state.in_flight_reaction.get::<AnnotationKey>(&annotation.into());
                let txn_id = match in_flight {
                    Some(ReactionState::Sending(txn_id)) => {
                        // Use the transaction ID as the in flight request
                        txn_id.clone()
                    }
                    _ => TransactionId::new(),
                };

                let event_content = AnyMessageLikeEventContent::Reaction(
                    ReactionEventContent::from(annotation.clone()),
                );
                self.handle_local_event_internal(
                    &mut state,
                    sender,
                    sender_profile,
                    txn_id.clone(),
                    event_content.clone(),
                )
                .await;
                ReactionState::Sending(txn_id)
            }
            (to_redact_local, to_redact_remote) => {
                // The reaction exists, redact local echo and/or remote echo
                let no_reason = RoomRedactionEventContent::default();
                let to_redact = if let Some(to_redact_local) = to_redact_local {
                    EventItemIdentifier::TransactionId(to_redact_local.clone())
                } else if let Some(to_redact_remote) = to_redact_remote {
                    EventItemIdentifier::EventId(to_redact_remote.clone())
                } else {
                    error!("Transaction id and event id are both missing");
                    return Err(super::Error::FailedToToggleReaction);
                };
                self.handle_local_redaction_internal(
                    &mut state,
                    sender,
                    sender_profile,
                    TransactionId::new(),
                    to_redact,
                    no_reason.clone(),
                )
                .await;

                // Remember the remote echo to redact on the homeserver
                ReactionState::Redacting(to_redact_remote.cloned())
            }
        };

        state.reaction_state.insert(annotation.into(), reaction_state.clone());

        // Check the action to perform depending on any in flight request
        let in_flight = state.in_flight_reaction.get::<AnnotationKey>(&annotation.into());
        let result = match in_flight {
            Some(_) => {
                // There is an in-flight request
                // This local reaction should not initiate a new request
                ReactionAction::None
            }

            None => {
                // There is no in-flight request
                match reaction_state.clone() {
                    ReactionState::Redacting(event_id) => match event_id {
                        Some(event_id) => ReactionAction::RedactRemote(event_id),
                        // If we just redacted a local echo, no need to send a remote request
                        None => ReactionAction::None,
                    },
                    ReactionState::Sending(txn_id) => ReactionAction::SendRemote(txn_id),
                }
            }
        };

        match result {
            ReactionAction::None => {}
            ReactionAction::SendRemote(_) | ReactionAction::RedactRemote(_) => {
                // Remember the new in flight request
                state.in_flight_reaction.insert(annotation.into(), reaction_state);
            }
        };

        Ok(result)
    }

    pub(super) async fn set_initial_user_receipt(
        &mut self,
        receipt_type: ReceiptType,
        receipt: (OwnedEventId, Receipt),
    ) {
        let own_user_id = self.room_data_provider.own_user_id().to_owned();
        self.state
            .lock()
            .await
            .users_read_receipts
            .entry(own_user_id)
            .or_default()
            .insert(receipt_type, receipt);
    }

    #[tracing::instrument(skip_all)]
    pub(super) async fn add_initial_events(&mut self, events: Vector<SyncTimelineEvent>) {
        if events.is_empty() {
            return;
        }

        debug!("Adding {} initial events", events.len());

        let mut state = self.state.lock().await;
        for event in events {
            state
                .handle_remote_event(
                    event,
                    TimelineItemPosition::End { from_cache: true },
                    &self.room_data_provider,
                    &self.settings,
                )
                .await;
        }
    }

    pub(super) async fn clear(&self) {
        trace!("Clearing timeline");
        self.state.lock().await.clear();
    }

    pub(super) async fn handle_joined_room_update(&self, update: JoinedRoom) {
        let mut state = self.state.lock().await;
        state.handle_sync_timeline(update.timeline, &self.room_data_provider, &self.settings).await;

        for raw_event in update.account_data {
            match raw_event.deserialize() {
                Ok(AnyRoomAccountDataEvent::FullyRead(ev)) => {
                    state.set_fully_read_event(ev.content.event_id)
                }
                Ok(_) => {}
                Err(e) => {
                    warn!("Failed to deserialize account data: {e}");
                }
            }
        }

        if !update.ephemeral.is_empty() {
            let own_user_id = self.room_data_provider.own_user_id();
            for raw_event in update.ephemeral {
                match raw_event.deserialize() {
                    Ok(AnySyncEphemeralRoomEvent::Receipt(ev)) => {
                        state.handle_explicit_read_receipts(ev.content, own_user_id);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("Failed to deserialize ephemeral event: {e}");
                    }
                }
            }
        }
    }

    pub(super) async fn handle_sync_timeline(&self, timeline: Timeline) {
        self.state
            .lock()
            .await
            .handle_sync_timeline(timeline, &self.room_data_provider, &self.settings)
            .await;
    }

    #[cfg(test)]
    pub(super) async fn handle_live_event(&self, event: SyncTimelineEvent) {
        self.state
            .lock()
            .await
            .handle_live_event(event, &self.room_data_provider, &self.settings)
            .await;
    }

    /// Handle the creation of a new local event.
    #[instrument(skip_all)]
    pub(super) async fn handle_local_event(
        &self,
        txn_id: OwnedTransactionId,
        content: AnyMessageLikeEventContent,
    ) {
        let sender = self.room_data_provider.own_user_id().to_owned();
        let sender_profile = self.room_data_provider.profile(&sender).await;

        let mut state = self.state.lock().await;
        self.handle_local_event_internal(&mut state, sender, sender_profile, txn_id, content).await;
    }

    /// Handle the creation of a new local event.
    #[instrument(skip_all)]
    async fn handle_local_event_internal(
        &self,
        state: &mut TimelineInnerState,
        own_user_id: OwnedUserId,
        own_profile: Option<Profile>,
        txn_id: OwnedTransactionId,
        content: AnyMessageLikeEventContent,
    ) {
        let event_meta = TimelineEventMetadata {
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

        TimelineEventHandler::new(event_meta, flow, state, self.settings.track_read_receipts)
            .handle_event(kind);
    }

    /// Handle the creation of a new local event.
    #[instrument(skip_all)]
    pub(super) async fn handle_local_redaction(
        &self,
        txn_id: OwnedTransactionId,
        to_redact: EventItemIdentifier,
        content: RoomRedactionEventContent,
    ) {
        let sender = self.room_data_provider.own_user_id().to_owned();
        let profile = self.room_data_provider.profile(&sender).await;

        let mut state = self.state.lock().await;
        self.handle_local_redaction_internal(
            &mut state, sender, profile, txn_id, to_redact, content,
        )
        .await;
    }

    /// Handle the local redaction of an event.
    #[instrument(skip_all)]
    async fn handle_local_redaction_internal(
        &self,
        state: &mut TimelineInnerState,
        own_user_id: OwnedUserId,
        own_profile: Option<Profile>,
        txn_id: OwnedTransactionId,
        to_redact: EventItemIdentifier,
        content: RoomRedactionEventContent,
    ) {
        let flow = Flow::Local { txn_id: txn_id.clone() };
        let event_meta = TimelineEventMetadata {
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

                TimelineEventHandler::new(
                    event_meta,
                    flow,
                    state,
                    self.settings.track_read_receipts,
                )
                .handle_event(kind);
            }
            EventItemIdentifier::EventId(event_id) => {
                let kind = TimelineEventKind::Redaction { redacts: event_id, content };

                TimelineEventHandler::new(
                    event_meta,
                    flow,
                    state,
                    self.settings.track_read_receipts,
                )
                .handle_event(kind);
            }
        }
    }

    /// Update the send state of a local event represented by a transaction ID.
    ///
    /// If no local event is found, a warning is raised.
    #[instrument(skip_all, fields(txn_id))]
    pub(super) async fn update_event_send_state(
        &self,
        txn_id: &TransactionId,
        send_state: EventSendState,
    ) {
        let mut state = self.state.lock().await;

        let new_event_id: Option<&EventId> = match &send_state {
            EventSendState::Sent { event_id } => Some(event_id),
            _ => None,
        };

        // The local echoes are always at the end of the timeline, we must first make
        // sure the remote echo hasn't showed up yet.
        if rfind_event_item(&state.items, |it| {
            new_event_id.is_some() && it.event_id() == new_event_id && it.as_remote().is_some()
        })
        .is_some()
        {
            // Remote echo already received. This is very unlikely.
            trace!("Remote echo received before send-event response");

            let local_echo =
                rfind_event_item(&state.items, |it| it.transaction_id() == Some(txn_id));

            // If there's both the remote echo and a local echo, that means the
            // remote echo was received before the response *and* contained no
            // transaction ID (and thus duplicated the local echo).
            if let Some((idx, _)) = local_echo {
                warn!("Message echo got duplicated, removing the local one");
                state.items.remove(idx);

                if idx == 0 {
                    error!("Inconsistent state: Local echo was not preceded by day divider");
                    return;
                }

                if idx == state.items.len() && state.items[idx - 1].is_day_divider() {
                    // The day divider may have been added for this local echo, remove it and let
                    // the next message decide whether it's required or not.
                    state.items.remove(idx - 1);
                }
            }

            return;
        }

        // Look for the local event by the transaction ID or event ID.
        let result = rfind_event_item(&state.items, |it| {
            it.transaction_id() == Some(txn_id)
                || new_event_id.is_some()
                    && it.event_id() == new_event_id
                    && it.as_local().is_some()
        });

        let Some((idx, item)) = result else {
            // Event isn't found at all.
            warn!("Timeline item not found, can't add event ID");
            return;
        };

        let Some(local_item) = item.as_local() else {
            warn!("We looked for a local item, but it transitioned to remote??");
            return;
        };

        // The event was already marked as sent, that's a broken state, let's
        // emit an error but also override to the given sent state.
        if let EventSendState::Sent { event_id: existing_event_id } = &local_item.send_state {
            let new_event_id = new_event_id.map(debug);
            error!(?existing_event_id, ?new_event_id, "Local echo already marked as sent");
        }

        let is_error = matches!(send_state, EventSendState::SendingFailed { .. });

        let new_item = item.with_inner_kind(local_item.with_send_state(send_state));
        state.items.set(idx, new_item);

        if is_error {
            // When there is an error, sending further messages is paused. This
            // should be reflected in the timeline, so we set all other pending
            // events to cancelled.
            let num_items = state.items.len();
            for idx in 0..num_items {
                let item = state.items[idx].clone();
                let Some(event_item) = item.as_event() else { continue };
                let Some(local_item) = event_item.as_local() else { continue };
                if matches!(&local_item.send_state, EventSendState::NotSentYet) {
                    let new_event_item =
                        event_item.with_kind(local_item.with_send_state(EventSendState::Cancelled));
                    state.items.set(idx, item.with_kind(new_event_item));
                }
            }
        }
    }

    /// Reconcile the timeline with the result of a request to toggle a
    /// reaction.
    ///
    /// Checks and finalises any state that tracks ongoing requests and decides
    /// whether further requests are required to handle any new local echos.
    pub(super) async fn resolve_reaction_response(
        &self,
        annotation: &Annotation,
        result: &ReactionToggleResult,
    ) -> Result<ReactionAction, super::Error> {
        let mut state = self.state.lock().await;
        let user_id = self.room_data_provider.own_user_id();
        let annotation_key: AnnotationKey = annotation.into();

        let reaction_state = state
            .reaction_state
            .get(&AnnotationKey::from(annotation))
            .expect("Reaction state should be set before sending the reaction");

        let follow_up_action = match (result, reaction_state) {
            (ReactionToggleResult::AddSuccess { event_id, .. }, ReactionState::Redacting(_)) => {
                // A reaction was added successfully but we've been requested to undo it
                state
                    .in_flight_reaction
                    .insert(annotation_key, ReactionState::Redacting(Some(event_id.to_owned())));
                ReactionAction::RedactRemote(event_id.to_owned())
            }
            (ReactionToggleResult::RedactSuccess { .. }, ReactionState::Sending(txn_id)) => {
                // A reaction was was redacted successfully but we've been requested to undo it
                let txn_id = txn_id.to_owned();
                state
                    .in_flight_reaction
                    .insert(annotation_key, ReactionState::Sending(txn_id.clone()));
                ReactionAction::SendRemote(txn_id)
            }
            _ => {
                // We're done, so also update the timeline
                state.in_flight_reaction.remove(&annotation_key);
                state.reaction_state.remove(&annotation_key);
                update_timeline_reaction(&mut state, user_id, annotation, result)?;

                ReactionAction::None
            }
        };

        Ok(follow_up_action)
    }

    pub(super) async fn prepare_retry(
        &self,
        txn_id: &TransactionId,
    ) -> Option<TimelineItemContent> {
        let mut state = self.state.lock().await;

        let (idx, item) = rfind_event_item(&state.items, |it| it.transaction_id() == Some(txn_id))?;
        let local_item = item.as_local()?;

        match &local_item.send_state {
            EventSendState::NotSentYet => {
                warn!("Attempted to retry the sending of an item that is already pending");
                return None;
            }
            EventSendState::Sent { .. } => {
                warn!("Attempted to retry the sending of an item that has already succeeded");
                return None;
            }
            EventSendState::SendingFailed { .. } | EventSendState::Cancelled => {}
        }

        let new_item = item.with_inner_kind(local_item.with_send_state(EventSendState::NotSentYet));
        let content = item.content.clone();
        state.items.set(idx, new_item);

        Some(content)
    }

    pub(super) async fn discard_local_echo(&self, txn_id: &TransactionId) -> bool {
        let mut state = self.state.lock().await;
        if let Some((idx, _)) =
            rfind_event_item(&state.items, |it| it.transaction_id() == Some(txn_id))
        {
            state.items.remove(idx);
            true
        } else {
            false
        }
    }

    /// Handle a back-paginated event.
    ///
    /// Returns the number of timeline updates that were made.
    #[instrument(skip_all)]
    pub(super) async fn handle_back_paginated_event(
        &self,
        event: TimelineEvent,
    ) -> HandleEventResult {
        self.state
            .lock()
            .await
            .handle_remote_event(
                event.into(),
                TimelineItemPosition::Start,
                &self.room_data_provider,
                &self.settings,
            )
            .await
    }

    pub(super) async fn set_fully_read_event(&self, fully_read_event_id: OwnedEventId) {
        self.state.lock().await.set_fully_read_event(fully_read_event_id)
    }

    #[cfg(feature = "e2e-encryption")]
    #[instrument(skip(self, room), fields(room_id = ?room.room_id()))]
    pub(super) async fn retry_event_decryption(
        &self,
        room: &room::Common,
        session_ids: Option<BTreeSet<String>>,
    ) {
        self.retry_event_decryption_inner(room.to_owned(), session_ids).await
    }

    #[cfg(all(test, feature = "e2e-encryption"))]
    pub(super) async fn retry_event_decryption_test(
        &self,
        room_id: &RoomId,
        olm_machine: OlmMachine,
        session_ids: Option<BTreeSet<String>>,
    ) {
        self.retry_event_decryption_inner((olm_machine, room_id.to_owned()), session_ids).await
    }

    #[cfg(feature = "e2e-encryption")]
    async fn retry_event_decryption_inner(
        &self,
        decryptor: impl Decryptor,
        session_ids: Option<BTreeSet<String>>,
    ) {
        use super::EncryptedMessage;

        let mut state = self.state.clone().lock_owned().await;

        let should_retry = move |session_id: &str| {
            if let Some(session_ids) = &session_ids {
                session_ids.contains(session_id)
            } else {
                true
            }
        };

        let retry_indices: Vec<_> = state
            .items
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| match item.as_event()?.content().as_unable_to_decrypt()? {
                EncryptedMessage::MegolmV1AesSha2 { session_id, .. }
                    if should_retry(session_id) =>
                {
                    Some(idx)
                }
                EncryptedMessage::MegolmV1AesSha2 { .. }
                | EncryptedMessage::OlmV1Curve25519AesSha2 { .. }
                | EncryptedMessage::Unknown => None,
            })
            .collect();

        if retry_indices.is_empty() {
            return;
        }

        debug!("Retrying decryption");

        let settings = self.settings.clone();
        let room_data_provider = self.room_data_provider.clone();
        let push_rules_context = room_data_provider.push_rules_and_context().await;

        matrix_sdk::executor::spawn(async move {
            let retry_one = |item: Arc<TimelineItem>| {
                let decryptor = decryptor.clone();
                let should_retry = &should_retry;
                async move {
                    let event_item = item.as_event()?;

                    let session_id = match event_item.content().as_unable_to_decrypt()? {
                        EncryptedMessage::MegolmV1AesSha2 { session_id, .. }
                            if should_retry(session_id) =>
                        {
                            session_id
                        }
                        EncryptedMessage::MegolmV1AesSha2 { .. }
                        | EncryptedMessage::OlmV1Curve25519AesSha2 { .. }
                        | EncryptedMessage::Unknown => return None,
                    };

                    tracing::Span::current().record("session_id", session_id);

                    let Some(remote_event) = event_item.as_remote() else {
                        error!("Key for unable-to-decrypt timeline item is not an event ID");
                        return None;
                    };

                    tracing::Span::current().record("event_id", debug(&remote_event.event_id));

                    match decryptor.decrypt_event_impl(&remote_event.original_json).await {
                        Ok(event) => {
                            trace!(
                                "Successfully decrypted event that previously failed to decrypt"
                            );
                            Some(event)
                        }
                        Err(e) => {
                            info!("Failed to decrypt event after receiving room key: {e}");
                            None
                        }
                    }
                }
                .instrument(info_span!(
                    "retry_one",
                    session_id = field::Empty,
                    event_id = field::Empty
                ))
            };

            // Loop through all the indices, in order so we don't decrypt edits
            // before the event being edited, if both were UTD. Keep track of
            // index change as UTDs are removed instead of updated.
            let mut offset = 0;
            for idx in retry_indices {
                let idx = idx - offset;
                let Some(mut event) = retry_one(state.items[idx].clone()).await else {
                    continue;
                };

                event.push_actions = push_rules_context
                    .as_ref()
                    .map(|(push_rules, push_context)| {
                        push_rules.get_actions(&event.event, push_context).to_owned()
                    })
                    .unwrap_or_default();

                let result = state
                    .handle_remote_event(
                        event.into(),
                        TimelineItemPosition::Update(idx),
                        &room_data_provider,
                        &settings,
                    )
                    .await;

                // If the UTD was removed rather than updated, offset all
                // subsequent loop iterations.
                if result.item_removed {
                    offset += 1;
                }
            }
        });
    }

    pub(super) async fn set_sender_profiles_pending(&self) {
        self.set_non_ready_sender_profiles(TimelineDetails::Pending).await;
    }

    pub(super) async fn set_sender_profiles_error(&self, error: Arc<Error>) {
        self.set_non_ready_sender_profiles(TimelineDetails::Error(error)).await;
    }

    async fn set_non_ready_sender_profiles(&self, profile_state: TimelineDetails<Profile>) {
        let mut state = self.state.lock().await;
        for idx in 0..state.items.len() {
            let item = state.items[idx].clone();
            let Some(event_item) = item.as_event() else { continue };
            if !matches!(event_item.sender_profile(), TimelineDetails::Ready(_)) {
                let item = item.with_kind(TimelineItemKind::Event(
                    event_item.with_sender_profile(profile_state.clone()),
                ));
                state.items.set(idx, item);
            }
        }
    }

    pub(super) async fn update_sender_profiles(&self) {
        trace!("Updating sender profiles");

        let mut state = self.state.lock().await;
        let num_items = state.items.len();

        for idx in 0..num_items {
            let sender = match state.items[idx].as_event() {
                Some(event_item) => event_item.sender().to_owned(),
                None => continue,
            };
            let maybe_profile = self.room_data_provider.profile(&sender).await;

            assert_eq!(state.items.len(), num_items);

            let item = state.items[idx].clone();
            let event_item = item.as_event().unwrap();
            match maybe_profile {
                Some(profile) => {
                    if !event_item.sender_profile().contains(&profile) {
                        let updated_item =
                            event_item.with_sender_profile(TimelineDetails::Ready(profile));
                        state.items.set(idx, item.with_kind(updated_item));
                    }
                }
                None => {
                    if !event_item.sender_profile().is_unavailable() {
                        let updated_item =
                            event_item.with_sender_profile(TimelineDetails::Unavailable);
                        state.items.set(idx, item.with_kind(updated_item));
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) async fn handle_read_receipts(&self, receipt_event_content: ReceiptEventContent) {
        let own_user_id = self.room_data_provider.own_user_id();
        self.state.lock().await.handle_explicit_read_receipts(receipt_event_content, own_user_id);
    }
}

impl TimelineInner {
    pub(super) fn room(&self) -> &room::Common {
        &self.room_data_provider
    }

    /// Get the current fully-read event.
    pub(super) async fn fully_read_event(&self) -> Option<FullyReadEvent> {
        match self.room().account_data_static().await {
            Ok(Some(fully_read)) => match fully_read.deserialize() {
                Ok(fully_read) => Some(fully_read),
                Err(e) => {
                    error!("Failed to deserialize fully-read account data: {e}");
                    None
                }
            },
            Err(e) => {
                error!("Failed to get fully-read account data from the store: {e}");
                None
            }
            _ => None,
        }
    }

    /// Load the current fully-read event in this inner timeline.
    pub(super) async fn load_fully_read_event(&self) {
        if let Some(fully_read) = self.fully_read_event().await {
            self.set_fully_read_event(fully_read.content.event_id).await;
        }
    }

    #[instrument(skip(self))]
    pub(super) async fn fetch_in_reply_to_details(
        &self,
        event_id: &EventId,
    ) -> Result<(), super::Error> {
        let state = self.state.lock().await;
        let (index, item) = rfind_event_by_id(&state.items, event_id)
            .ok_or(super::Error::RemoteEventNotInTimeline)?;
        let remote_item = item.as_remote().ok_or(super::Error::RemoteEventNotInTimeline)?.clone();

        let TimelineItemContent::Message(message) = item.content().clone() else {
            info!("Event is not a message");
            return Ok(());
        };
        let Some(in_reply_to) = message.in_reply_to() else {
            info!("Event is not a reply");
            return Ok(());
        };
        if let TimelineDetails::Ready(_) = &in_reply_to.event {
            info!("Replied-to event has already been fetched");
            return Ok(());
        }

        let item = item.clone();
        let event = fetch_replied_to_event(
            state,
            index,
            &item,
            &message,
            &in_reply_to.event_id,
            self.room(),
        )
        .await?;

        // We need to be sure to have the latest position of the event as it might have
        // changed while waiting for the request.
        let mut state = self.state.lock().await;
        let (index, item) = rfind_event_by_id(&state.items, &remote_item.event_id)
            .ok_or(super::Error::RemoteEventNotInTimeline)?;

        // Check the state of the event again, it might have been redacted while
        // the request was in-flight.
        let TimelineItemContent::Message(message) = item.content().clone() else {
            info!("Event is no longer a message (redacted?)");
            return Ok(());
        };
        let Some(in_reply_to) = message.in_reply_to() else {
            warn!("Event no longer has a reply (bug?)");
            return Ok(());
        };

        trace!("Updating in-reply-to details");
        let internal_id = item.internal_id;
        let mut item = item.clone();
        item.set_content(TimelineItemContent::Message(
            message.with_in_reply_to(InReplyToDetails {
                event_id: in_reply_to.event_id.clone(),
                event,
            }),
        ));
        state.items.set(index, timeline_item(item, internal_id));

        Ok(())
    }

    /// Get the latest read receipt for the given user.
    ///
    /// Useful to get the latest read receipt, whether it's private or public.
    pub(super) async fn latest_user_read_receipt(
        &self,
        user_id: &UserId,
    ) -> Option<(OwnedEventId, Receipt)> {
        let state = self.state.lock().await;
        let room = self.room();

        state.latest_user_read_receipt(user_id, room).await
    }

    /// Check whether the given receipt should be sent.
    ///
    /// Returns `false` if the given receipt is older than the current one.
    pub(super) async fn should_send_receipt(
        &self,
        receipt_type: &SendReceiptType,
        thread: &ReceiptThread,
        event_id: &EventId,
    ) -> bool {
        // We don't support threaded receipts yet.
        if *thread != ReceiptThread::Unthreaded {
            return true;
        }

        let own_user_id = self.room().own_user_id();
        let state = self.state.lock().await;
        let room = self.room();

        match receipt_type {
            SendReceiptType::Read => {
                if let Some((old_pub_read, _)) =
                    state.user_receipt(own_user_id, ReceiptType::Read, room).await
                {
                    if let Some(relative_pos) =
                        compare_events_positions(&old_pub_read, event_id, &state.items)
                    {
                        return relative_pos == RelativePosition::After;
                    }
                }
            }
            // Implicit read receipts are saved as public read receipts, so get the latest. It also
            // doesn't make sense to have a private read receipt behind a public one.
            SendReceiptType::ReadPrivate => {
                if let Some((old_priv_read, _)) =
                    state.latest_user_read_receipt(own_user_id, room).await
                {
                    if let Some(relative_pos) =
                        compare_events_positions(&old_priv_read, event_id, &state.items)
                    {
                        return relative_pos == RelativePosition::After;
                    }
                }
            }
            SendReceiptType::FullyRead => {
                if let Some(old_fully_read) = self.fully_read_event().await {
                    if let Some(relative_pos) = compare_events_positions(
                        &old_fully_read.content.event_id,
                        event_id,
                        &state.items,
                    ) {
                        return relative_pos == RelativePosition::After;
                    }
                }
            }
            _ => {}
        }

        // Let the server handle unknown receipts.
        true
    }
}

impl TimelineInnerState {
    #[instrument(skip_all)]
    pub(super) async fn handle_sync_timeline<P: RoomDataProvider>(
        &mut self,
        timeline: Timeline,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) {
        if timeline.limited {
            debug!("Got limited sync response, resetting timeline");
            self.clear();
        }

        for event in timeline.events {
            self.handle_live_event(event, room_data_provider, settings).await;
        }
    }

    /// Handle a live remote event.
    ///
    /// Shorthand for `handle_remote_event` with a `position` of
    /// `TimelineItemPosition::End { from_cache: false }`.
    async fn handle_live_event<P: RoomDataProvider>(
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
    async fn handle_remote_event<P: RoomDataProvider>(
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
        let event_meta = TimelineEventMetadata {
            sender,
            sender_profile,
            timestamp,
            is_own_event,
            encryption_info,
            read_receipts,
            is_highlighted,
        };
        let flow = Flow::Remote { event_id, raw_event: raw, txn_id, position, should_add };

        TimelineEventHandler::new(event_meta, flow, self, settings.track_read_receipts)
            .handle_event(event_kind)
    }

    pub(super) fn clear(&mut self) {
        self.items.clear();
        self.reaction_map.clear();
        self.fully_read_event = None;
        self.event_should_update_fully_read_marker = false;
    }

    #[instrument(skip_all)]
    fn set_fully_read_event(&mut self, fully_read_event_id: OwnedEventId) {
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
}

async fn fetch_replied_to_event(
    mut state: MutexGuard<'_, TimelineInnerState>,
    index: usize,
    item: &EventTimelineItem,
    message: &Message,
    in_reply_to: &EventId,
    room: &room::Common,
) -> Result<TimelineDetails<Box<RepliedToEvent>>, super::Error> {
    if let Some((_, item)) = rfind_event_by_id(&state.items, in_reply_to) {
        let details = match item.content() {
            TimelineItemContent::Message(message) => {
                TimelineDetails::Ready(Box::new(RepliedToEvent {
                    message: message.clone(),
                    sender: item.sender().to_owned(),
                    sender_profile: item.sender_profile().clone(),
                }))
            }
            _ => return Err(super::Error::UnsupportedEvent),
        };

        debug!("Found replied-to event locally");
        return Ok(details);
    };

    trace!("Setting in-reply-to details to pending");
    let reply = message.with_in_reply_to(InReplyToDetails {
        event_id: in_reply_to.to_owned(),
        event: TimelineDetails::Pending,
    });
    let event_item = item.with_content(TimelineItemContent::Message(reply), None);

    let state_ref = &mut *state;
    state_ref.items.set(index, new_timeline_item(event_item, &mut state_ref.next_internal_id));

    // Don't hold the state lock while the network request is made
    drop(state);

    trace!("Fetching replied-to event");
    let res = match room.event(in_reply_to).await {
        Ok(timeline_event) => TimelineDetails::Ready(Box::new(
            RepliedToEvent::try_from_timeline_event(timeline_event, room).await?,
        )),
        Err(e) => TimelineDetails::Error(Arc::new(e)),
    };
    Ok(res)
}

fn update_timeline_reaction(
    state: &mut TimelineInnerState,
    own_user_id: &UserId,
    annotation: &Annotation,
    result: &ReactionToggleResult,
) -> Result<(), super::Error> {
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

    let related = rfind_event_item(&state.items, |it| {
        it.event_id().is_some_and(|it| it == annotation.event_id)
    });

    let Some((idx, related)) = related else {
        // Event isn't found at all.
        warn!("Timeline item not found, can't update reaction ID");
        return Err(super::Error::FailedToToggleReaction);
    };
    let Some(remote_related) = related.as_remote() else {
        error!("inconsistent state: reaction received on a non-remote event item");
        return Err(super::Error::FailedToToggleReaction);
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
            if state.reaction_map.remove(&id).is_none() {
                warn!(
                    "Tried to remove reaction by transaction ID, but didn't \
                     find matching reaction in the reaction map"
                );
            }
        }
        // Add the remote echo to the reaction_map
        if let Some(event_id) = remote_echo_to_add {
            state.reaction_map.insert(
                EventItemIdentifier::EventId(event_id.clone()),
                (reaction_sender_data, annotation.clone()),
            );
        }
    }

    state.items.set(idx, timeline_item(new_related, related.internal_id));

    Ok(())
}
