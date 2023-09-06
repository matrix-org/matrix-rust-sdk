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
use std::{fmt, sync::Arc};

use async_rx::StreamExt as _;
use eyeball_im::{ObservableVectorEntry, VectorDiff};
use eyeball_im_util::vector;
use futures_core::Stream;
use imbl::Vector;
use itertools::Itertools;
#[cfg(all(test, feature = "e2e-encryption"))]
use matrix_sdk::crypto::OlmMachine;
use matrix_sdk::{
    deserialized_responses::{SyncTimelineEvent, TimelineEvent},
    sync::{JoinedRoom, Timeline},
    Error, Result, Room,
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
    EventId, OwnedEventId, OwnedTransactionId, TransactionId, UserId,
};
use tracing::{debug, error, field::debug, info, instrument, trace, warn};
#[cfg(feature = "e2e-encryption")]
use tracing::{field, info_span, Instrument as _};

#[cfg(feature = "e2e-encryption")]
use super::traits::Decryptor;
use super::{
    event_handler::TimelineItemPosition,
    event_item::EventItemIdentifier,
    item::timeline_item,
    reactions::ReactionToggleResult,
    traits::RoomDataProvider,
    util::{compare_events_positions, rfind_event_by_id, rfind_event_item, RelativePosition},
    AnnotationKey, EventSendState, EventTimelineItem, InReplyToDetails, Message, Profile,
    RepliedToEvent, TimelineDetails, TimelineItem, TimelineItemContent, TimelineItemKind,
};

mod state;

pub(super) use self::state::TimelineInnerState;
use self::state::{TimelineInnerStateLock, TimelineInnerStateWriteGuard};

#[derive(Clone, Debug)]
pub(super) struct TimelineInner<P: RoomDataProvider = Room> {
    state: TimelineInnerStateLock,
    room_data_provider: P,
    settings: TimelineInnerSettings,
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

#[cfg(not(tarpaulin_include))]
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
        let state = TimelineInnerState::new(room_data_provider.room_version());
        Self {
            state: TimelineInnerStateLock::new(state),
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
        self.state.read().await.items.clone()
    }

    pub(super) async fn subscribe(
        &self,
    ) -> (Vector<Arc<TimelineItem>>, impl Stream<Item = VectorDiff<Arc<TimelineItem>>>) {
        trace!("Creating timeline items signal");
        let state = self.state.read().await;
        // auto-deref to the inner vector's clone method
        let items = state.items.clone();
        let stream = state.items.subscribe().into_stream();
        (items, stream)
    }

    pub(super) async fn subscribe_batched(
        &self,
    ) -> (Vector<Arc<TimelineItem>>, impl Stream<Item = Vec<VectorDiff<Arc<TimelineItem>>>>) {
        let (items, stream) = self.subscribe().await;
        let stream = stream.batch_with(self.state.subscribe_lock_release());
        (items, stream)
    }

    pub(super) async fn subscribe_filter_map<U, F>(
        &self,
        f: F,
    ) -> (Vector<U>, impl Stream<Item = VectorDiff<U>>)
    where
        U: Clone,
        F: Fn(Arc<TimelineItem>) -> Option<U>,
    {
        trace!("Creating timeline items signal");
        let state = self.state.read().await;
        vector::FilterMap::new(state.items.clone(), state.items.subscribe().into_stream(), f)
    }

    pub(super) async fn toggle_reaction_local(
        &self,
        annotation: &Annotation,
    ) -> Result<ReactionAction, super::Error> {
        let mut state = self.state.write().await;

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
                state.handle_local_event(
                    sender,
                    sender_profile,
                    txn_id.clone(),
                    event_content.clone(),
                    &self.settings,
                );
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
                state.handle_local_redaction(
                    sender,
                    sender_profile,
                    TransactionId::new(),
                    to_redact,
                    no_reason.clone(),
                    &self.settings,
                );

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
            .write()
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

        let mut state = self.state.write().await;
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
        self.state.write().await.clear();
    }

    #[instrument(skip_all)]
    pub(super) async fn handle_joined_room_update(&self, update: JoinedRoom) {
        let mut state = self.state.write().await;
        state.handle_sync_timeline(update.timeline, &self.room_data_provider, &self.settings).await;

        trace!("Handling account data");
        for raw_event in update.account_data {
            match raw_event.deserialize() {
                Ok(AnyRoomAccountDataEvent::FullyRead(ev)) => {
                    state.set_fully_read_event(ev.content.event_id);
                }
                Ok(_) => {}
                Err(e) => {
                    warn!("Failed to deserialize account data: {e}");
                }
            }
        }

        if !update.ephemeral.is_empty() {
            trace!("Handling ephemeral room events");
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
            .write()
            .await
            .handle_sync_timeline(timeline, &self.room_data_provider, &self.settings)
            .await;
    }

    #[cfg(test)]
    pub(super) async fn handle_live_event(&self, event: SyncTimelineEvent) {
        self.state
            .write()
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
        let profile = self.room_data_provider.profile(&sender).await;

        let mut state = self.state.write().await;
        state.handle_local_event(sender, profile, txn_id, content, &self.settings);
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

        let mut state = self.state.write().await;
        state.handle_local_redaction(sender, profile, txn_id, to_redact, content, &self.settings);
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
        let mut state = self.state.write().await;

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
        let mut state = self.state.write().await;
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
                state.update_timeline_reaction(user_id, annotation, result)?;

                ReactionAction::None
            }
        };

        Ok(follow_up_action)
    }

    pub(super) async fn prepare_retry(
        &self,
        txn_id: &TransactionId,
    ) -> Option<TimelineItemContent> {
        let mut state = self.state.write().await;

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
        state.items.remove(idx);
        state.items.push_back(new_item);

        Some(content)
    }

    pub(super) async fn discard_local_echo(&self, txn_id: &TransactionId) -> bool {
        let mut state = self.state.write().await;

        if let Some((idx, _)) =
            rfind_event_item(&state.items, |it| it.transaction_id() == Some(txn_id))
        {
            state.items.remove(idx);
            true
        } else {
            false
        }
    }

    /// Handle a list of back-paginated events.
    ///
    /// Returns the number of timeline updates that were made. Short-circuits
    /// and returns `None` if the number of items added or updated exceeds
    /// `u16::MAX`, which should practically never happen.
    #[instrument(skip_all)]
    pub(super) async fn handle_back_paginated_events(
        &self,
        events: Vec<TimelineEvent>,
    ) -> Option<HandleManyEventsResult> {
        let mut state = self.state.write().await;

        let mut total = HandleManyEventsResult::default();
        for event in events {
            let res = state
                .handle_remote_event(
                    event.into(),
                    TimelineItemPosition::Start,
                    &self.room_data_provider,
                    &self.settings,
                )
                .await;

            total.items_added = total.items_added.checked_add(res.item_added as u16)?;
            total.items_updated = total.items_updated.checked_add(res.items_updated)?;
        }

        Some(total)
    }

    pub(super) async fn set_fully_read_event(&self, fully_read_event_id: OwnedEventId) {
        self.state.write().await.set_fully_read_event(fully_read_event_id)
    }

    #[cfg(feature = "e2e-encryption")]
    #[instrument(skip(self, room), fields(room_id = ?room.room_id()))]
    pub(super) async fn retry_event_decryption(
        &self,
        room: &Room,
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

        let mut state = self.state.clone().write_owned().await;

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

                    let Some(original_json) = &remote_event.original_json else {
                        error!("UTD item must contain original JSON");
                        return None;
                    };

                    match decryptor.decrypt_event_impl(original_json).await {
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

                event.push_actions =
                    push_rules_context.as_ref().map(|(push_rules, push_context)| {
                        push_rules.get_actions(&event.event, push_context).to_owned()
                    });

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
        self.state.write().await.items.for_each(|mut entry| {
            let Some(event_item) = entry.as_event() else { return };
            if !matches!(event_item.sender_profile(), TimelineDetails::Ready(_)) {
                let new_item = entry.with_kind(TimelineItemKind::Event(
                    event_item.with_sender_profile(profile_state.clone()),
                ));
                ObservableVectorEntry::set(&mut entry, new_item);
            }
        });
    }

    pub(super) async fn update_sender_profiles(&self) {
        trace!("Updating sender profiles");

        let mut state = self.state.write().await;
        let mut entries = state.items.entries();
        while let Some(mut entry) = entries.next() {
            let Some(event_item) = entry.as_event() else { continue };
            let event_id = event_item.event_id().map(debug);
            let transaction_id = event_item.transaction_id().map(debug);

            if event_item.sender_profile().is_ready() {
                trace!(event_id, transaction_id, "Profile already set");
                continue;
            }

            match self.room_data_provider.profile(event_item.sender()).await {
                Some(profile) => {
                    trace!(event_id, transaction_id, "Adding profile");
                    let updated_item =
                        event_item.with_sender_profile(TimelineDetails::Ready(profile));
                    let new_item = entry.with_kind(updated_item);
                    ObservableVectorEntry::set(&mut entry, new_item);
                }
                None => {
                    if !event_item.sender_profile().is_unavailable() {
                        trace!(event_id, transaction_id, "Marking profile unavailable");
                        let updated_item =
                            event_item.with_sender_profile(TimelineDetails::Unavailable);
                        let new_item = entry.with_kind(updated_item);
                        ObservableVectorEntry::set(&mut entry, new_item);
                    } else {
                        debug!(event_id, transaction_id, "Profile already marked unavailable");
                    }
                }
            }
        }

        trace!("Done updating sender profiles");
    }

    #[cfg(test)]
    pub(super) async fn handle_read_receipts(&self, receipt_event_content: ReceiptEventContent) {
        let own_user_id = self.room_data_provider.own_user_id();
        self.state.write().await.handle_explicit_read_receipts(receipt_event_content, own_user_id);
    }
}

impl TimelineInner {
    pub(super) fn room(&self) -> &Room {
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
        let state = self.state.write().await;
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
        if let TimelineDetails::Pending = &in_reply_to.event {
            info!("Replied-to event is already being fetched");
            return Ok(());
        }
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
        let mut state = self.state.write().await;
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
        let state = self.state.read().await;
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
        let state = self.state.read().await;
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

#[derive(Default)]
pub(super) struct HandleManyEventsResult {
    pub items_added: u16,
    pub items_updated: u16,
}

async fn fetch_replied_to_event(
    mut state: TimelineInnerStateWriteGuard<'_>,
    index: usize,
    item: &EventTimelineItem,
    message: &Message,
    in_reply_to: &EventId,
    room: &Room,
) -> Result<TimelineDetails<Box<RepliedToEvent>>, super::Error> {
    if let Some((_, item)) = rfind_event_by_id(&state.items, in_reply_to) {
        let details = TimelineDetails::Ready(Box::new(RepliedToEvent {
            content: item.content.clone(),
            sender: item.sender().to_owned(),
            sender_profile: item.sender_profile().clone(),
        }));

        debug!("Found replied-to event locally");
        return Ok(details);
    };

    trace!("Setting in-reply-to details to pending");
    let reply = message.with_in_reply_to(InReplyToDetails {
        event_id: in_reply_to.to_owned(),
        event: TimelineDetails::Pending,
    });
    let event_item = item.with_content(TimelineItemContent::Message(reply), None);

    let new_timeline_item = state.new_timeline_item(event_item);
    state.items.set(index, new_timeline_item);

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
