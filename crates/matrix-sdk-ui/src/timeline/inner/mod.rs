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

use as_variant::as_variant;
use eyeball_im::{ObservableVectorEntry, VectorDiff};
use eyeball_im_util::vector::VectorObserverExt;
use futures_core::Stream;
use imbl::Vector;
use itertools::Itertools;
#[cfg(all(test, feature = "e2e-encryption"))]
use matrix_sdk::crypto::OlmMachine;
use matrix_sdk::{deserialized_responses::SyncTimelineEvent, Error, Result, Room};
#[cfg(test)]
use ruma::events::receipt::ReceiptEventContent;
#[cfg(all(test, feature = "e2e-encryption"))]
use ruma::RoomId;
use ruma::{
    api::client::receipt::create_receipt::v3::ReceiptType as SendReceiptType,
    events::{
        fully_read::FullyReadEvent,
        poll::unstable_start::UnstablePollStartEventContent,
        reaction::ReactionEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        relation::Annotation,
        room::message::{MessageType, Relation},
        AnyMessageLikeEventContent, AnySyncEphemeralRoomEvent, AnySyncMessageLikeEvent,
        AnySyncTimelineEvent, MessageLikeEventType,
    },
    serde::Raw,
    EventId, OwnedEventId, OwnedTransactionId, RoomVersionId, TransactionId, UserId,
};
use tokio::sync::{RwLock, RwLockWriteGuard};
use tracing::{debug, error, field::debug, info, instrument, trace, warn};
#[cfg(feature = "e2e-encryption")]
use tracing::{field, info_span, Instrument as _};

#[cfg(feature = "e2e-encryption")]
use super::traits::Decryptor;
use super::{
    event_item::EventItemIdentifier,
    reactions::ReactionToggleResult,
    traits::RoomDataProvider,
    util::{rfind_event_by_id, rfind_event_item, RelativePosition},
    AnnotationKey, EventSendState, EventTimelineItem, InReplyToDetails, Message, Profile,
    RepliedToEvent, TimelineDetails, TimelineItem, TimelineItemContent, TimelineItemKind,
};
use crate::{
    timeline::{day_dividers::DayDividerAdjuster, TimelineEventFilterFn},
    unable_to_decrypt_hook::UtdHookManager,
};

mod state;

pub(super) use self::state::{
    EventMeta, FullEventMeta, TimelineEnd, TimelineInnerMetadata, TimelineInnerState,
    TimelineInnerStateTransaction,
};

#[derive(Clone, Debug)]
pub(super) struct TimelineInner<P: RoomDataProvider = Room> {
    state: Arc<RwLock<TimelineInnerState>>,
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
    /// Should the read receipts and read markers be handled?
    pub(super) track_read_receipts: bool,
    /// Event filter that controls what's rendered as a timeline item (and thus
    /// what can carry read receipts).
    pub(super) event_filter: Arc<TimelineEventFilterFn>,
    /// Are unparsable events added as timeline items of their own kind?
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
            event_filter: Arc::new(default_event_filter),
            add_failed_to_parse: true,
        }
    }
}

/// The default event filter for
/// [`crate::timeline::TimelineBuilder::event_filter`].
///
/// It filters out events that are not rendered by the timeline, including but
/// not limited to: reactions, edits, redactions on existing messages.
///
/// If you have a custom filter, it may be best to chain yours with this one if
/// you do not want to run into situations where a read receipt is not visible
/// because it's living on an event that doesn't have a matching timeline item.
pub fn default_event_filter(event: &AnySyncTimelineEvent, room_version: &RoomVersionId) -> bool {
    match event {
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(ev)) => {
            if ev.redacts(room_version).is_some() {
                // This is a redaction of an existing message, we'll only update the previous
                // message and not render a new entry.
                false
            } else {
                // This is a redacted entry, that we'll show only if the redacted entity wasn't
                // a reaction.
                ev.event_type() != MessageLikeEventType::Reaction
            }
        }

        AnySyncTimelineEvent::MessageLike(msg) => {
            match msg.original_content() {
                None => {
                    // This is a redacted entry, that we'll show only if the redacted entity wasn't
                    // a reaction.
                    msg.event_type() != MessageLikeEventType::Reaction
                }

                Some(original_content) => {
                    match original_content {
                        AnyMessageLikeEventContent::RoomMessage(content) => {
                            if content
                                .relates_to
                                .as_ref()
                                .is_some_and(|rel| matches!(rel, Relation::Replacement(_)))
                            {
                                // Edits aren't visible by default.
                                return false;
                            }

                            matches!(
                                content.msgtype,
                                MessageType::Audio(_)
                                    | MessageType::Emote(_)
                                    | MessageType::File(_)
                                    | MessageType::Image(_)
                                    | MessageType::Location(_)
                                    | MessageType::Notice(_)
                                    | MessageType::ServerNotice(_)
                                    | MessageType::Text(_)
                                    | MessageType::Video(_)
                                    | MessageType::VerificationRequest(_)
                            )
                        }

                        AnyMessageLikeEventContent::Sticker(_)
                        | AnyMessageLikeEventContent::UnstablePollStart(
                            UnstablePollStartEventContent::New(_),
                        )
                        | AnyMessageLikeEventContent::CallInvite(_)
                        | AnyMessageLikeEventContent::RoomEncrypted(_) => true,

                        _ => false,
                    }
                }
            }
        }

        AnySyncTimelineEvent::State(_) => {
            // All the state events may get displayed by default.
            true
        }
    }
}

impl<P: RoomDataProvider> TimelineInner<P> {
    pub(super) fn new(
        room_data_provider: P,
        unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,
    ) -> Self {
        let state =
            TimelineInnerState::new(room_data_provider.room_version(), unable_to_decrypt_hook);
        Self {
            state: Arc::new(RwLock::new(state)),
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
        (state.items.clone(), state.items.subscribe().into_stream())
    }

    pub(super) async fn subscribe_batched(
        &self,
    ) -> (Vector<Arc<TimelineItem>>, impl Stream<Item = Vec<VectorDiff<Arc<TimelineItem>>>>) {
        trace!("Creating timeline items signal");
        let state = self.state.read().await;
        (state.items.clone(), state.items.subscribe().into_batched_stream())
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
        self.state.read().await.items.subscribe().filter_map(f)
    }

    #[instrument(skip_all)]
    pub(super) async fn toggle_reaction_local(
        &self,
        annotation: &Annotation,
    ) -> Result<ReactionAction, super::Error> {
        let mut state = self.state.write().await;

        let user_id = self.room_data_provider.own_user_id();

        let related_event = {
            let items = state.items.clone();
            let Some((_, item)) = rfind_event_by_id(&items, &annotation.event_id) else {
                warn!("Timeline item not found, can't update reaction ID");
                return Err(super::Error::FailedToToggleReaction);
            };
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
        let sender_profile = self.room_data_provider.profile_from_user_id(&sender).await;
        let reaction_state = match (to_redact_local, to_redact_remote) {
            (None, None) => {
                // No previous record of the reaction, create a local echo.

                let in_flight =
                    state.meta.in_flight_reaction.get::<AnnotationKey>(&annotation.into());
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
                );
                ReactionState::Sending(txn_id)
            }

            (to_redact_local, to_redact_remote) => {
                // The reaction exists, redact local echo and/or remote echo

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
                );

                // Remember the remote echo to redact on the homeserver
                ReactionState::Redacting(to_redact_remote.cloned())
            }
        };

        state.meta.reaction_state.insert(annotation.into(), reaction_state.clone());

        // Check the action to perform depending on any in flight request
        let in_flight = state.meta.in_flight_reaction.get::<AnnotationKey>(&annotation.into());
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
                state.meta.in_flight_reaction.insert(annotation.into(), reaction_state);
            }
        };

        Ok(result)
    }

    /// Populates our own latest read receipt in the in-memory by-user read
    /// receipt cache.
    pub(super) async fn populate_initial_user_receipt(&mut self, receipt_type: ReceiptType) {
        let own_user_id = self.room_data_provider.own_user_id().to_owned();

        let mut read_receipt = self
            .room_data_provider
            .load_user_receipt(receipt_type.clone(), ReceiptThread::Unthreaded, &own_user_id)
            .await;

        // Fallback to the one in the main thread.
        if read_receipt.is_none() {
            read_receipt = self
                .room_data_provider
                .load_user_receipt(receipt_type.clone(), ReceiptThread::Main, &own_user_id)
                .await;
        }

        if let Some(read_receipt) = read_receipt {
            self.state.write().await.meta.read_receipts.upsert_latest(
                own_user_id,
                receipt_type,
                read_receipt,
            );
        }
    }

    /// Handle a list of events at the given end of the timeline.
    ///
    /// Note: when the `position` is [`TimelineEnd::Front`], prepended events
    /// should be ordered in *reverse* topological order, that is, `events[0]`
    /// is the most recent.
    ///
    /// Returns the number of timeline updates that were made.
    pub(super) async fn add_events_at(
        &self,
        events: Vec<impl Into<SyncTimelineEvent>>,
        position: TimelineEnd,
    ) -> HandleManyEventsResult {
        if events.is_empty() {
            return Default::default();
        }

        let mut state = self.state.write().await;
        state.add_events_at(events, position, &self.room_data_provider, &self.settings).await
    }

    pub(super) async fn clear(&self) {
        self.state.write().await.clear();
    }

    pub(super) async fn handle_fully_read_marker(&self, fully_read_event_id: OwnedEventId) {
        self.state.write().await.handle_fully_read_marker(fully_read_event_id);
    }

    pub(super) async fn handle_sync_events(
        &self,
        events: Vec<SyncTimelineEvent>,
        ephemeral: Vec<Raw<AnySyncEphemeralRoomEvent>>,
    ) {
        let mut state = self.state.write().await;
        state.handle_sync_events(events, ephemeral, &self.room_data_provider, &self.settings).await;
    }

    #[cfg(test)]
    pub(super) async fn handle_live_event(&self, event: SyncTimelineEvent) {
        let mut state = self.state.write().await;
        state
            .add_events_at(
                vec![event],
                TimelineEnd::Back { from_cache: false },
                &self.room_data_provider,
                &self.settings,
            )
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
        let profile = self.room_data_provider.profile_from_user_id(&sender).await;

        let mut state = self.state.write().await;
        state.handle_local_event(sender, profile, txn_id, content);
    }

    /// Handle the redaction of a local event.
    #[cfg(test)]
    pub(super) async fn handle_local_redaction(
        &self,
        txn_id: OwnedTransactionId,
        to_redact: EventItemIdentifier,
    ) {
        let sender = self.room_data_provider.own_user_id().to_owned();
        let profile = self.room_data_provider.profile_from_user_id(&sender).await;

        let mut state = self.state.write().await;
        state.handle_local_redaction(sender, profile, txn_id, to_redact);
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
        let mut txn = state.transaction();

        let new_event_id: Option<&EventId> =
            as_variant!(&send_state, EventSendState::Sent { event_id } => event_id);

        // The local echoes are always at the end of the timeline, we must first make
        // sure the remote echo hasn't showed up yet.
        if rfind_event_item(&txn.items, |it| {
            new_event_id.is_some() && it.event_id() == new_event_id && it.as_remote().is_some()
        })
        .is_some()
        {
            // Remote echo already received. This is very unlikely.
            trace!("Remote echo received before send-event response");

            let local_echo = rfind_event_item(&txn.items, |it| it.transaction_id() == Some(txn_id));

            // If there's both the remote echo and a local echo, that means the
            // remote echo was received before the response *and* contained no
            // transaction ID (and thus duplicated the local echo).
            if let Some((idx, _)) = local_echo {
                warn!("Message echo got duplicated, removing the local one");
                txn.items.remove(idx);

                // Adjust the day dividers, if needs be.
                let mut adjuster = DayDividerAdjuster::default();
                adjuster.run(&mut txn.items, &mut txn.meta);
            }

            txn.commit();
            return;
        }

        // Look for the local event by the transaction ID or event ID.
        let result = rfind_event_item(&txn.items, |it| {
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
        txn.items.set(idx, new_item);

        if is_error {
            // When there is an error, sending further messages is paused. This
            // should be reflected in the timeline, so we set all other pending
            // events to cancelled.
            let items = &mut txn.items;
            let num_items = items.len();
            for idx in 0..num_items {
                let item = &items[idx];
                let Some(event_item) = item.as_event() else { continue };
                let Some(local_item) = event_item.as_local() else { continue };
                if matches!(&local_item.send_state, EventSendState::NotSentYet) {
                    let new_event_item =
                        event_item.with_kind(local_item.with_send_state(EventSendState::Cancelled));
                    items.set(idx, item.with_kind(new_event_item));
                }
            }
        }

        txn.commit();
    }

    /// Reconcile the timeline with the result of a request to toggle a
    /// reaction.
    ///
    /// Checks and finalises any state that tracks ongoing requests and decides
    /// whether further requests are required to handle any new local echos.
    #[instrument(skip_all)]
    pub(super) async fn resolve_reaction_response(
        &self,
        annotation: &Annotation,
        result: &ReactionToggleResult,
    ) -> Result<ReactionAction, super::Error> {
        let mut state = self.state.write().await;
        let user_id = self.room_data_provider.own_user_id();
        let annotation_key: AnnotationKey = annotation.into();

        let reaction_state = state
            .meta
            .reaction_state
            .get(&AnnotationKey::from(annotation))
            .expect("Reaction state should be set before sending the reaction");

        let follow_up_action = match (result, reaction_state) {
            (ReactionToggleResult::AddSuccess { event_id, .. }, ReactionState::Redacting(_)) => {
                // A reaction was added successfully but we've been requested to undo it
                state
                    .meta
                    .in_flight_reaction
                    .insert(annotation_key, ReactionState::Redacting(Some(event_id.to_owned())));
                ReactionAction::RedactRemote(event_id.to_owned())
            }
            (ReactionToggleResult::RedactSuccess { .. }, ReactionState::Sending(txn_id)) => {
                // A reaction was was redacted successfully but we've been requested to undo it
                let txn_id = txn_id.to_owned();
                state
                    .meta
                    .in_flight_reaction
                    .insert(annotation_key, ReactionState::Sending(txn_id.clone()));
                ReactionAction::SendRemote(txn_id)
            }
            _ => {
                // We're done, so also update the timeline
                state.meta.in_flight_reaction.swap_remove(&annotation_key);
                state.meta.reaction_state.swap_remove(&annotation_key);
                state.update_timeline_reaction(user_id, annotation, result)?;

                ReactionAction::None
            }
        };

        if matches!(
            result,
            ReactionToggleResult::AddFailure { .. } | ReactionToggleResult::RedactFailure { .. }
        ) {
            return Err(super::Error::FailedToToggleReaction);
        }

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

        let mut txn = state.items.transaction();
        txn.remove(idx);
        txn.push_back(new_item);
        txn.commit();

        Some(content)
    }

    pub(super) async fn discard_local_echo(&self, txn_id: &TransactionId) -> bool {
        let mut state = self.state.write().await;

        if let Some((idx, _)) =
            rfind_event_item(&state.items, |it| it.transaction_id() == Some(txn_id))
        {
            state.items.remove(idx);
            debug!("Discarded local echo");
            true
        } else {
            debug!("Can't find local echo to discard");
            false
        }
    }

    pub(super) async fn set_fully_read_event(&self, fully_read_event_id: OwnedEventId) {
        self.state.write().await.set_fully_read_event(fully_read_event_id);
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
        let unable_to_decrypt_hook = state.meta.unable_to_decrypt_hook.clone();

        matrix_sdk::executor::spawn(async move {
            let retry_one = |item: Arc<TimelineItem>| {
                let decryptor = decryptor.clone();
                let should_retry = &should_retry;
                let unable_to_decrypt_hook = unable_to_decrypt_hook.clone();
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

                            // Notify observers that we managed to eventually decrypt an event.
                            if let Some(hook) = unable_to_decrypt_hook {
                                hook.on_late_decrypt(&remote_event.event_id);
                            }

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

            state
                .retry_event_decryption(
                    retry_one,
                    retry_indices,
                    push_rules_context,
                    &room_data_provider,
                    &settings,
                )
                .await;
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

    pub(super) async fn update_missing_sender_profiles(&self) {
        trace!("Updating missing sender profiles");

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

            match self.room_data_provider.profile_from_user_id(event_item.sender()).await {
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

        trace!("Done updating missing sender profiles");
    }

    /// Update the profiles of the given senders, even if they are ready.
    pub(super) async fn force_update_sender_profiles(&self, sender_ids: &BTreeSet<&UserId>) {
        trace!("Forcing update of sender profiles: {sender_ids:?}");

        let mut state = self.state.write().await;
        let mut entries = state.items.entries();
        while let Some(mut entry) = entries.next() {
            let Some(event_item) = entry.as_event() else { continue };
            if !sender_ids.contains(event_item.sender()) {
                continue;
            }

            let event_id = event_item.event_id().map(debug);
            let transaction_id = event_item.transaction_id().map(debug);

            match self.room_data_provider.profile_from_user_id(event_item.sender()).await {
                Some(profile) => {
                    if matches!(event_item.sender_profile(), TimelineDetails::Ready(old_profile) if *old_profile == profile)
                    {
                        debug!(event_id, transaction_id, "Profile already up-to-date");
                    } else {
                        trace!(event_id, transaction_id, "Updating profile");
                        let updated_item =
                            event_item.with_sender_profile(TimelineDetails::Ready(profile));
                        let new_item = entry.with_kind(updated_item);
                        ObservableVectorEntry::set(&mut entry, new_item);
                    }
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

        trace!("Done forcing update of sender profiles");
    }

    #[cfg(test)]
    pub(super) async fn handle_read_receipts(&self, receipt_event_content: ReceiptEventContent) {
        let own_user_id = self.room_data_provider.own_user_id();
        self.state.write().await.handle_read_receipts(receipt_event_content, own_user_id);
    }

    /// Get the latest read receipt for the given user.
    ///
    /// Useful to get the latest read receipt, whether it's private or public.
    pub(super) async fn latest_user_read_receipt(
        &self,
        user_id: &UserId,
    ) -> Option<(OwnedEventId, Receipt)> {
        self.state.read().await.latest_user_read_receipt(user_id, &self.room_data_provider).await
    }

    /// Get the ID of the timeline event with the latest read receipt for the
    /// given user.
    pub(super) async fn latest_user_read_receipt_timeline_event_id(
        &self,
        user_id: &UserId,
    ) -> Option<OwnedEventId> {
        self.state.read().await.latest_user_read_receipt_timeline_event_id(user_id)
    }
}

impl TimelineInner {
    pub(super) fn room(&self) -> &Room {
        &self.room_data_provider
    }

    /// Get the current fully-read event, from storage.
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

    /// Load the current fully-read event in this inner timeline from storage.
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
        state.items.set(index, TimelineItem::new(item, internal_id));

        Ok(())
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
                    state.meta.user_receipt(own_user_id, ReceiptType::Read, room).await
                {
                    trace!(%old_pub_read, "found a previous public receipt");
                    if let Some(relative_pos) =
                        state.meta.compare_events_positions(&old_pub_read, event_id)
                    {
                        trace!("event referred to new receipt is {relative_pos:?} the previous receipt");
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
                    trace!(%old_priv_read, "found a previous private receipt");
                    if let Some(relative_pos) =
                        state.meta.compare_events_positions(&old_priv_read, event_id)
                    {
                        trace!("event referred to new receipt is {relative_pos:?} the previous receipt");
                        return relative_pos == RelativePosition::After;
                    }
                }
            }
            SendReceiptType::FullyRead => {
                if let Some(old_fully_read) = self.fully_read_event().await {
                    if let Some(relative_pos) = state
                        .meta
                        .compare_events_positions(&old_fully_read.content.event_id, event_id)
                    {
                        return relative_pos == RelativePosition::After;
                    }
                }
            }
            _ => {}
        }

        // Let the server handle unknown receipts.
        true
    }

    /// Returns the latest event identifier, even if it's not visible, or if
    /// it's folded into another timeline item.
    pub(crate) async fn latest_event_id(&self) -> Option<OwnedEventId> {
        let state = self.state.read().await;
        state.meta.all_events.back().map(|event_meta| &event_meta.event_id).cloned()
    }
}

#[derive(Debug, Default)]
pub(super) struct HandleManyEventsResult {
    /// The number of items that were added to the timeline.
    ///
    /// Note one can't assume anything about the position at which those were
    /// added.
    pub items_added: u64,

    /// The number of items that were updated in the timeline.
    pub items_updated: u64,
}

async fn fetch_replied_to_event(
    mut state: RwLockWriteGuard<'_, TimelineInnerState>,
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

    let new_timeline_item = state.meta.new_timeline_item(event_item);
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
