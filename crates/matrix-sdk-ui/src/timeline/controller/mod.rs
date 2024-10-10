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

use std::{collections::BTreeSet, fmt, sync::Arc};

use as_variant::as_variant;
use eyeball_im::{ObservableVectorEntry, VectorDiff};
use eyeball_im_util::vector::VectorObserverExt;
use futures_core::Stream;
use imbl::Vector;
#[cfg(test)]
use matrix_sdk::crypto::OlmMachine;
use matrix_sdk::{
    deserialized_responses::SyncTimelineEvent,
    event_cache::{paginator::Paginator, RoomEventCache},
    send_queue::{
        LocalEcho, LocalEchoContent, RoomSendQueueUpdate, SendHandle, SendReactionHandle,
    },
    Result, Room,
};
use ruma::{
    api::client::receipt::create_receipt::v3::ReceiptType as SendReceiptType,
    events::{
        poll::unstable_start::UnstablePollStartEventContent,
        reaction::ReactionEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        relation::Annotation,
        room::message::{MessageType, Relation},
        AnyMessageLikeEventContent, AnySyncEphemeralRoomEvent, AnySyncMessageLikeEvent,
        AnySyncTimelineEvent, MessageLikeEventType,
    },
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, RoomVersionId,
    TransactionId, UserId,
};
#[cfg(test)]
use ruma::{events::receipt::ReceiptEventContent, RoomId};
use tokio::sync::{RwLock, RwLockWriteGuard};
use tracing::{
    debug, error, field, field::debug, info, info_span, instrument, trace, warn, Instrument as _,
};

pub(super) use self::state::{
    EventMeta, FullEventMeta, PendingEdit, PendingEditKind, TimelineEnd, TimelineMetadata,
    TimelineState, TimelineStateTransaction,
};
use super::{
    event_handler::TimelineEventKind,
    event_item::{ReactionStatus, RemoteEventOrigin},
    traits::{Decryptor, RoomDataProvider},
    util::{rfind_event_by_id, rfind_event_item, RelativePosition},
    Error, EventSendState, EventTimelineItem, InReplyToDetails, Message, PaginationError, Profile,
    ReactionInfo, RepliedToEvent, TimelineDetails, TimelineEventItemId, TimelineFocus,
    TimelineItem, TimelineItemContent, TimelineItemKind,
};
use crate::{
    timeline::{
        day_dividers::DayDividerAdjuster,
        event_item::EventTimelineItemKind,
        pinned_events_loader::{PinnedEventsLoader, PinnedEventsLoaderError},
        reactions::FullReactionKey,
        util::rfind_event_by_uid,
        TimelineEventFilterFn,
    },
    unable_to_decrypt_hook::UtdHookManager,
};

mod state;

/// Data associated to the current timeline focus.
#[derive(Debug)]
enum TimelineFocusData<P: RoomDataProvider> {
    /// The timeline receives live events from the sync.
    Live,

    /// The timeline is focused on a single event, and it can expand in one
    /// direction or another.
    Event {
        /// The event id we've started to focus on.
        event_id: OwnedEventId,
        /// The paginator instance.
        paginator: Paginator<P>,
        /// Number of context events to request for the first request.
        num_context_events: u16,
    },

    PinnedEvents {
        loader: PinnedEventsLoader,
    },
}

#[derive(Clone, Debug)]
pub(super) struct TimelineController<P: RoomDataProvider = Room> {
    /// Inner mutable state.
    state: Arc<RwLock<TimelineState>>,

    /// Inner mutable focus state.
    focus: Arc<RwLock<TimelineFocusData<P>>>,

    /// A [`RoomDataProvider`] implementation, providing data.
    ///
    /// Useful for testing only; in the real world, it's just a [`Room`].
    pub(crate) room_data_provider: P,

    /// Settings applied to this timeline.
    settings: TimelineSettings,
}

#[derive(Clone)]
pub(super) struct TimelineSettings {
    /// Should the read receipts and read markers be handled?
    pub(super) track_read_receipts: bool,
    /// Event filter that controls what's rendered as a timeline item (and thus
    /// what can carry read receipts).
    pub(super) event_filter: Arc<TimelineEventFilterFn>,
    /// Are unparsable events added as timeline items of their own kind?
    pub(super) add_failed_to_parse: bool,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for TimelineSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TimelineSettings")
            .field("track_read_receipts", &self.track_read_receipts)
            .field("add_failed_to_parse", &self.add_failed_to_parse)
            .finish_non_exhaustive()
    }
}

impl Default for TimelineSettings {
    fn default() -> Self {
        Self {
            track_read_receipts: false,
            event_filter: Arc::new(default_event_filter),
            add_failed_to_parse: true,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum TimelineFocusKind {
    Live,
    Event,
    PinnedEvents,
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
                        | AnyMessageLikeEventContent::CallNotify(_)
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

impl<P: RoomDataProvider> TimelineController<P> {
    pub(super) fn new(
        room_data_provider: P,
        focus: TimelineFocus,
        internal_id_prefix: Option<String>,
        unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,
        is_room_encrypted: Option<bool>,
    ) -> Self {
        let (focus_data, focus_kind) = match focus {
            TimelineFocus::Live => (TimelineFocusData::Live, TimelineFocusKind::Live),

            TimelineFocus::Event { target, num_context_events } => {
                let paginator = Paginator::new(room_data_provider.clone());
                (
                    TimelineFocusData::Event { paginator, event_id: target, num_context_events },
                    TimelineFocusKind::Event,
                )
            }

            TimelineFocus::PinnedEvents { max_events_to_load, max_concurrent_requests } => (
                TimelineFocusData::PinnedEvents {
                    loader: PinnedEventsLoader::new(
                        Arc::new(room_data_provider.clone()),
                        max_events_to_load as usize,
                        max_concurrent_requests as usize,
                    ),
                },
                TimelineFocusKind::PinnedEvents,
            ),
        };

        let state = TimelineState::new(
            focus_kind,
            room_data_provider.own_user_id().to_owned(),
            room_data_provider.room_version(),
            internal_id_prefix,
            unable_to_decrypt_hook,
            is_room_encrypted,
        );

        Self {
            state: Arc::new(RwLock::new(state)),
            focus: Arc::new(RwLock::new(focus_data)),
            room_data_provider,
            settings: Default::default(),
        }
    }

    /// Initializes the configured focus with appropriate data.
    ///
    /// Should be called only once after creation of the [`TimelineInner`], with
    /// all its fields set.
    ///
    /// Returns whether there were any events added to the timeline.
    pub(super) async fn init_focus(
        &self,
        room_event_cache: &RoomEventCache,
    ) -> Result<bool, Error> {
        let focus_guard = self.focus.read().await;

        match &*focus_guard {
            TimelineFocusData::Live => {
                // Retrieve the cached events, and add them to the timeline.
                let (events, _) =
                    room_event_cache.subscribe().await.map_err(Error::EventCacheError)?;

                let has_events = !events.is_empty();

                self.replace_with_initial_remote_events(events, RemoteEventOrigin::Cache).await;

                Ok(has_events)
            }

            TimelineFocusData::Event { event_id, paginator, num_context_events } => {
                // Start a /context request, and append the results (in order) to the timeline.
                let start_from_result = paginator
                    .start_from(event_id, (*num_context_events).into())
                    .await
                    .map_err(PaginationError::Paginator)?;

                drop(focus_guard);

                let has_events = !start_from_result.events.is_empty();

                self.replace_with_initial_remote_events(
                    start_from_result.events.into_iter().map(Into::into).collect(),
                    RemoteEventOrigin::Pagination,
                )
                .await;

                Ok(has_events)
            }

            TimelineFocusData::PinnedEvents { loader } => {
                let loaded_events = loader.load_events().await.map_err(Error::PinnedEventsError)?;

                drop(focus_guard);

                let has_events = !loaded_events.is_empty();

                self.replace_with_initial_remote_events(
                    loaded_events,
                    RemoteEventOrigin::Pagination,
                )
                .await;

                Ok(has_events)
            }
        }
    }

    /// Listens to encryption state changes for the room in
    /// [`matrix_sdk_base::RoomInfo`] and applies the new value to the
    /// existing timeline items. This will then cause a refresh of those
    /// timeline items.
    pub async fn handle_encryption_state_changes(&self) {
        let mut room_info = self.room_data_provider.room_info();

        while let Some(info) = room_info.next().await {
            let changed = {
                let state = self.state.read().await;
                let mut old_is_room_encrypted = state.meta.is_room_encrypted.write().unwrap();
                let is_encrypted_now = info.is_encrypted();

                if *old_is_room_encrypted != Some(is_encrypted_now) {
                    *old_is_room_encrypted = Some(is_encrypted_now);
                    true
                } else {
                    false
                }
            };

            if changed {
                let mut state = self.state.write().await;
                state.update_all_events_is_room_encrypted();
            }
        }
    }

    pub(crate) async fn reload_pinned_events(
        &self,
    ) -> Result<Vec<SyncTimelineEvent>, PinnedEventsLoaderError> {
        let focus_guard = self.focus.read().await;

        if let TimelineFocusData::PinnedEvents { loader } = &*focus_guard {
            loader.load_events().await
        } else {
            Err(PinnedEventsLoaderError::TimelineFocusNotPinnedEvents)
        }
    }

    /// Run a backward pagination (in focused mode) and append the results to
    /// the timeline.
    ///
    /// Returns whether we hit the start of the timeline.
    pub(super) async fn focused_paginate_backwards(
        &self,
        num_events: u16,
    ) -> Result<bool, PaginationError> {
        let pagination = match &*self.focus.read().await {
            TimelineFocusData::Live | TimelineFocusData::PinnedEvents { .. } => {
                return Err(PaginationError::NotEventFocusMode)
            }
            TimelineFocusData::Event { paginator, .. } => paginator
                .paginate_backward(num_events.into())
                .await
                .map_err(PaginationError::Paginator)?,
        };

        self.add_events_at(pagination.events, TimelineEnd::Front, RemoteEventOrigin::Pagination)
            .await;

        Ok(pagination.hit_end_of_timeline)
    }

    /// Run a forward pagination (in focused mode) and append the results to
    /// the timeline.
    ///
    /// Returns whether we hit the end of the timeline.
    pub(super) async fn focused_paginate_forwards(
        &self,
        num_events: u16,
    ) -> Result<bool, PaginationError> {
        let pagination = match &*self.focus.read().await {
            TimelineFocusData::Live | TimelineFocusData::PinnedEvents { .. } => {
                return Err(PaginationError::NotEventFocusMode)
            }
            TimelineFocusData::Event { paginator, .. } => paginator
                .paginate_forward(num_events.into())
                .await
                .map_err(PaginationError::Paginator)?,
        };

        self.add_events_at(pagination.events, TimelineEnd::Back, RemoteEventOrigin::Pagination)
            .await;

        Ok(pagination.hit_end_of_timeline)
    }

    /// Is this timeline receiving events from sync (aka has a live focus)?
    pub(super) async fn is_live(&self) -> bool {
        matches!(&*self.focus.read().await, TimelineFocusData::Live)
    }

    pub(super) fn with_settings(mut self, settings: TimelineSettings) -> Self {
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
    ) -> (Vector<Arc<TimelineItem>>, impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Send) {
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

    /// Toggle a reaction locally.
    ///
    /// Returns true if the reaction was added, false if it was removed.
    #[instrument(skip_all)]
    pub(super) async fn toggle_reaction_local(
        &self,
        unique_id: &str,
        key: &str,
    ) -> Result<bool, Error> {
        let mut state = self.state.write().await;

        let Some((item_pos, item)) = rfind_event_by_uid(&state.items, unique_id) else {
            warn!("Timeline item not found, can't add reaction");
            return Err(Error::FailedToToggleReaction);
        };

        let user_id = self.room_data_provider.own_user_id();
        let prev_status = item
            .reactions()
            .get(key)
            .and_then(|group| group.get(user_id))
            .map(|reaction_info| reaction_info.status.clone());

        let Some(prev_status) = prev_status else {
            match &item.inner.kind {
                EventTimelineItemKind::Local(local) => {
                    if let Some(send_handle) = local.send_handle.clone() {
                        if send_handle
                            .react(key.to_owned())
                            .await
                            .map_err(|err| Error::SendQueueError(err.into()))?
                            .is_some()
                        {
                            trace!("adding a reaction to a local echo");
                            return Ok(true);
                        }

                        warn!("couldn't toggle reaction for local echo");
                        return Ok(false);
                    }

                    warn!("missing send handle for local echo; is this a test?");
                    return Ok(false);
                }

                EventTimelineItemKind::Remote(remote) => {
                    // Add a reaction through the room data provider.
                    // No need to reflect the effect locally, since the local echo handling will
                    // take care of it.
                    trace!("adding a reaction to a remote echo");
                    let annotation = Annotation::new(remote.event_id.to_owned(), key.to_owned());
                    self.room_data_provider
                        .send(ReactionEventContent::from(annotation).into())
                        .await?;
                    return Ok(true);
                }
            }
        };

        trace!("removing a previous reaction");
        match prev_status {
            ReactionStatus::LocalToLocal(send_reaction_handle) => {
                if let Some(handle) = send_reaction_handle {
                    if !handle.abort().await.map_err(|err| Error::SendQueueError(err.into()))? {
                        // Impossible state: the reaction has moved from local to echo under our
                        // feet, but the timeline was supposed to be locked!
                        warn!("unexpectedly unable to abort sending of local reaction");
                    }
                } else {
                    warn!("no send reaction handle (this should only happen in testing contexts)");
                }
            }

            ReactionStatus::LocalToRemote(send_handle) => {
                // No need to reflect the change ourselves, since handling the discard of the
                // local echo will take care of it.
                trace!("aborting send of the previous reaction that was a local echo");
                if let Some(handle) = send_handle {
                    if !handle.abort().await.map_err(|err| Error::SendQueueError(err.into()))? {
                        // Impossible state: the reaction has moved from local to echo under our
                        // feet, but the timeline was supposed to be locked!
                        warn!("unexpectedly unable to abort sending of local reaction");
                    }
                } else {
                    warn!("no send handle (this should only happen in testing contexts)");
                }
            }

            ReactionStatus::RemoteToRemote(event_id) => {
                // Assume the redaction will work; we'll re-add the reaction if it didn't.
                let Some(annotated_event_id) =
                    item.as_remote().map(|event_item| event_item.event_id.clone())
                else {
                    warn!("remote reaction to remote event, but the associated item isn't remote");
                    return Ok(false);
                };

                let mut reactions = item.reactions().clone();
                let reaction_info = reactions.remove_reaction(user_id, key);

                if reaction_info.is_some() {
                    let new_item = item.with_reactions(reactions);
                    state.items.set(item_pos, new_item);
                } else {
                    warn!("reaction is missing on the item, not removing it locally, but sending redaction.");
                }

                // Release the lock before running the request.
                drop(state);

                trace!("sending redact for a previous reaction");
                if let Err(err) = self.room_data_provider.redact(&event_id, None, None).await {
                    if let Some(reaction_info) = reaction_info {
                        debug!("sending redact failed, adding the reaction back to the list");

                        let mut state = self.state.write().await;
                        if let Some((item_pos, item)) =
                            rfind_event_by_id(&state.items, &annotated_event_id)
                        {
                            // Re-add the reaction to the mapping.
                            let mut reactions = item.reactions().clone();
                            reactions
                                .entry(key.to_owned())
                                .or_default()
                                .insert(user_id.to_owned(), reaction_info);
                            let new_item = item.with_reactions(reactions);
                            state.items.set(item_pos, new_item);
                        } else {
                            warn!("couldn't find item to re-add reaction anymore; maybe it's been redacted?");
                        }
                    }

                    return Err(err);
                }
            }
        }

        Ok(false)
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
        origin: RemoteEventOrigin,
    ) -> HandleManyEventsResult {
        if events.is_empty() {
            return Default::default();
        }

        let mut state = self.state.write().await;
        state
            .add_remote_events_at(
                events,
                position,
                origin,
                &self.room_data_provider,
                &self.settings,
            )
            .await
    }

    pub(super) async fn clear(&self) {
        self.state.write().await.clear();
    }

    /// Replaces the content of the current timeline with initial events.
    ///
    /// Also sets up read receipts and the read marker for a live timeline of a
    /// room.
    ///
    /// This is all done with a single lock guard, since we don't want the state
    /// to be modified between the clear and re-insertion of new events.
    pub(super) async fn replace_with_initial_remote_events(
        &self,
        events: Vec<SyncTimelineEvent>,
        origin: RemoteEventOrigin,
    ) {
        let mut state = self.state.write().await;

        let track_read_markers = self.settings.track_read_receipts;
        if track_read_markers {
            state.populate_initial_user_receipt(&self.room_data_provider, ReceiptType::Read).await;
            state
                .populate_initial_user_receipt(&self.room_data_provider, ReceiptType::ReadPrivate)
                .await;
        }

        // Replace the events if either the current event list or the new one aren't
        // empty.
        // Previously we just had to check the new one wasn't empty because
        // we did a clear operation before so the current one would always be empty, but
        // now we may want to replace a populated timeline with an empty one.
        if !state.items.is_empty() || !events.is_empty() {
            state
                .replace_with_remote_events(
                    events,
                    TimelineEnd::Back,
                    origin,
                    &self.room_data_provider,
                    &self.settings,
                )
                .await;
        }

        if track_read_markers {
            if let Some(fully_read_event_id) =
                self.room_data_provider.load_fully_read_marker().await
            {
                state.handle_fully_read_marker(fully_read_event_id);
            }
        }
    }

    pub(super) async fn handle_fully_read_marker(&self, fully_read_event_id: OwnedEventId) {
        self.state.write().await.handle_fully_read_marker(fully_read_event_id);
    }

    pub(super) async fn handle_ephemeral_events(
        &self,
        events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
    ) {
        let mut state = self.state.write().await;
        state.handle_ephemeral_events(events, &self.room_data_provider).await;
    }

    /// Creates the local echo for an event we're sending.
    #[instrument(skip_all)]
    pub(super) async fn handle_local_event(
        &self,
        txn_id: OwnedTransactionId,
        content: TimelineEventKind,
        send_handle: Option<SendHandle>,
    ) {
        let sender = self.room_data_provider.own_user_id().to_owned();
        let profile = self.room_data_provider.profile_from_user_id(&sender).await;

        // Only add new items if the timeline is live.
        let should_add_new_items = self.is_live().await;

        let mut state = self.state.write().await;
        state
            .handle_local_event(sender, profile, should_add_new_items, txn_id, send_handle, content)
            .await;
    }

    /// Update the send state of a local event represented by a transaction ID.
    ///
    /// If the corresponding local timeline item is missing, a warning is
    /// raised.
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
            // Event isn't found as a proper item. Try to find it as a reaction?
            if let Some((event_id, reaction_key)) = new_event_id.zip(
                txn.meta.reactions.map.get(&TimelineEventItemId::TransactionId(txn_id.to_owned())),
            ) {
                match &reaction_key.item {
                    TimelineEventItemId::TransactionId(_) => {
                        error!("unexpected remote reaction to local echo")
                    }
                    TimelineEventItemId::EventId(reacted_to_event_id) => {
                        if let Some((item_pos, event_item)) =
                            rfind_event_by_id(&txn.items, reacted_to_event_id)
                        {
                            let mut reactions = event_item.reactions().clone();
                            if let Some(entry) = reactions
                                .get_mut(&reaction_key.key)
                                .and_then(|by_user| by_user.get_mut(&reaction_key.sender))
                            {
                                trace!("updated reaction status to sent");
                                entry.status = ReactionStatus::RemoteToRemote(event_id.to_owned());
                                txn.items.set(item_pos, event_item.with_reactions(reactions));
                                txn.commit();
                                return;
                            }
                        }
                    }
                }
            }

            warn!("Timeline item not found, can't update send state");
            return;
        };

        let Some(local_item) = item.as_local() else {
            warn!("We looked for a local item, but it transitioned to remote??");
            return;
        };

        // The event was already marked as sent, that's a broken state, let's
        // emit an error but also override to the given sent state.
        if let EventSendState::Sent { event_id: existing_event_id } = &local_item.send_state {
            error!(?existing_event_id, ?new_event_id, "Local echo already marked as sent");
        }

        // If the event had local reactions, upgrade the mapping from reaction to
        // events, to indicate that the event is now remote.
        if let Some(new_event_id) = new_event_id {
            let reactions = item.reactions();
            for (_key, by_user) in reactions.iter() {
                for (_user_id, info) in by_user.iter() {
                    if let ReactionStatus::LocalToLocal(Some(reaction_handle)) = &info.status {
                        let reaction_txn_id = reaction_handle.transaction_id().to_owned();
                        if let Some(found) = txn
                            .meta
                            .reactions
                            .map
                            .get_mut(&TimelineEventItemId::TransactionId(reaction_txn_id))
                        {
                            found.item = TimelineEventItemId::EventId(new_event_id.to_owned());
                        }
                    }
                }
            }
        }

        let new_item = item.with_inner_kind(local_item.with_send_state(send_state));
        txn.items.set(idx, new_item);

        txn.commit();
    }

    pub(super) async fn discard_local_echo(&self, txn_id: &TransactionId) -> bool {
        let mut state = self.state.write().await;

        if let Some((idx, _)) =
            rfind_event_item(&state.items, |it| it.transaction_id() == Some(txn_id))
        {
            let mut txn = state.transaction();

            txn.items.remove(idx);

            // A read marker or a day divider may have been inserted before the local echo.
            // Ensure both are up to date.
            let mut adjuster = DayDividerAdjuster::default();
            adjuster.run(&mut txn.items, &mut txn.meta);

            txn.meta.update_read_marker(&mut txn.items);

            txn.commit();

            debug!("Discarded local echo");
            return true;
        }

        // Look if this was a local reaction echo.
        if let Some(full_key) =
            state.meta.reactions.map.remove(&TimelineEventItemId::TransactionId(txn_id.to_owned()))
        {
            let item = match &full_key.item {
                TimelineEventItemId::TransactionId(txn_id) => {
                    rfind_event_item(&state.items, |item| item.transaction_id() == Some(txn_id))
                }
                TimelineEventItemId::EventId(event_id) => rfind_event_by_id(&state.items, event_id),
            };

            let Some((idx, item)) = item else {
                warn!("missing reacted-to item for a reaction");
                return false;
            };

            let mut reactions = item.reactions().clone();
            if reactions.remove_reaction(&full_key.sender, &full_key.key).is_some() {
                let updated_item = item.with_reactions(reactions);
                state.items.set(idx, updated_item);
            } else {
                warn!(
                    "missing reaction {} for sender {} on timeline item",
                    full_key.key, full_key.sender
                );
            }

            return true;
        }

        debug!("Can't find local echo to discard");
        false
    }

    pub(super) async fn replace_local_echo(
        &self,
        txn_id: &TransactionId,
        content: AnyMessageLikeEventContent,
    ) -> bool {
        let AnyMessageLikeEventContent::RoomMessage(content) = content else {
            // Ideally, we'd support replacing local echoes for a reaction, etc., but
            // handling RoomMessage should be sufficient in most cases. Worst
            // case, the local echo will be sent Soon™ and we'll get another chance at
            // editing the event then.
            warn!("Replacing a local echo for a non-RoomMessage-like event NYI");
            return false;
        };

        let mut state = self.state.write().await;
        let mut txn = state.transaction();

        let Some((idx, prev_item)) =
            rfind_event_item(&txn.items, |it| it.transaction_id() == Some(txn_id))
        else {
            debug!("Can't find local echo to replace");
            return false;
        };

        // Reuse the previous local echo's state, but reset the send state to not sent
        // (per API contract).
        let ti_kind = {
            let Some(prev_local_item) = prev_item.as_local() else {
                warn!("We looked for a local item, but it transitioned as remote??");
                return false;
            };
            prev_local_item.with_send_state(EventSendState::NotSentYet)
        };

        // Replace the local-related state (kind) and the content state.
        let new_item = TimelineItem::new(
            prev_item
                .with_kind(ti_kind)
                .with_content(TimelineItemContent::message(content, None, &txn.items), None),
            prev_item.internal_id.to_owned(),
        );

        txn.items.set(idx, new_item);

        // This doesn't change the original sending time, so there's no need to adjust
        // day dividers.

        txn.commit();

        debug!("Replaced local echo");
        true
    }

    #[instrument(skip(self, room), fields(room_id = ?room.room_id()))]
    pub(super) async fn retry_event_decryption(
        &self,
        room: &Room,
        session_ids: Option<BTreeSet<String>>,
    ) {
        self.retry_event_decryption_inner(room.to_owned(), session_ids).await
    }

    #[cfg(test)]
    pub(super) async fn retry_event_decryption_test(
        &self,
        room_id: &RoomId,
        olm_machine: OlmMachine,
        session_ids: Option<BTreeSet<String>>,
    ) {
        self.retry_event_decryption_inner((olm_machine, room_id.to_owned()), session_ids).await
    }

    async fn retry_event_decryption_inner(
        &self,
        decryptor: impl Decryptor,
        session_ids: Option<BTreeSet<String>>,
    ) {
        use matrix_sdk::crypto::types::events::UtdCause;

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

                            let cause = UtdCause::determine(Some(original_json));

                            // Notify observers that we managed to eventually decrypt an event.
                            if let Some(hook) = unable_to_decrypt_hook {
                                hook.on_late_decrypt(&remote_event.event_id, cause).await;
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

    pub(super) async fn set_sender_profiles_error(&self, error: Arc<matrix_sdk::Error>) {
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

    /// Subscribe to changes in the read receipts of our own user.
    pub async fn subscribe_own_user_read_receipts_changed(&self) -> impl Stream<Item = ()> {
        self.state.read().await.meta.read_receipts.subscribe_own_user_read_receipts_changed()
    }

    /// Handle a room send update that's a new local echo.
    pub(crate) async fn handle_local_echo(&self, echo: LocalEcho) {
        match echo.content {
            LocalEchoContent::Event { serialized_event, send_handle, is_wedged } => {
                let content = match serialized_event.deserialize() {
                    Ok(d) => d,
                    Err(err) => {
                        warn!("error deserializing local echo: {err}");
                        return;
                    }
                };

                self.handle_local_event(
                    echo.transaction_id.clone(),
                    TimelineEventKind::Message { content, relations: Default::default() },
                    Some(send_handle),
                )
                .await;

                if is_wedged {
                    self.update_event_send_state(
                        &echo.transaction_id,
                        EventSendState::SendingFailed {
                            // Put a dummy error in this case, since we're not persisting the errors
                            // that occurred in previous sessions.
                            error: Arc::new(matrix_sdk::Error::UnknownError(Box::new(
                                MissingLocalEchoFailError,
                            ))),
                            is_recoverable: false,
                        },
                    )
                    .await;
                }
            }

            LocalEchoContent::React { key, send_handle, applies_to } => {
                self.handle_local_reaction(key, send_handle, applies_to).await;
            }
        }
    }

    /// Adds a reaction (local echo) to a local echo.
    #[instrument(skip(self, send_handle))]
    async fn handle_local_reaction(
        &self,
        reaction_key: String,
        send_handle: SendReactionHandle,
        applies_to: OwnedTransactionId,
    ) {
        let mut state = self.state.write().await;

        let Some((item_pos, item)) =
            rfind_event_item(&state.items, |item| item.transaction_id() == Some(&applies_to))
        else {
            warn!("Local item not found anymore.");
            return;
        };

        let user_id = self.room_data_provider.own_user_id();

        let reaction_txn_id = send_handle.transaction_id().to_owned();
        let reaction_info = ReactionInfo {
            timestamp: MilliSecondsSinceUnixEpoch::now(),
            status: ReactionStatus::LocalToLocal(Some(send_handle)),
        };

        let mut reactions = item.reactions().clone();
        let by_user = reactions.entry(reaction_key.clone()).or_default();
        by_user.insert(user_id.to_owned(), reaction_info);

        trace!("Adding local reaction to local echo");
        let new_item = item.with_reactions(reactions);
        state.items.set(item_pos, new_item);

        // Add it to the reaction map, so we can discard it later if needs be.
        state.meta.reactions.map.insert(
            TimelineEventItemId::TransactionId(reaction_txn_id),
            FullReactionKey {
                item: TimelineEventItemId::TransactionId(applies_to),
                key: reaction_key,
                sender: user_id.to_owned(),
            },
        );
    }

    /// Handle a single room send queue update.
    pub(crate) async fn handle_room_send_queue_update(&self, update: RoomSendQueueUpdate) {
        match update {
            RoomSendQueueUpdate::NewLocalEvent(echo) => {
                self.handle_local_echo(echo).await;
            }

            RoomSendQueueUpdate::CancelledLocalEvent { transaction_id } => {
                if !self.discard_local_echo(&transaction_id).await {
                    warn!("couldn't find the local echo to discard");
                }
            }

            RoomSendQueueUpdate::ReplacedLocalEvent { transaction_id, new_content } => {
                let content = match new_content.deserialize() {
                    Ok(d) => d,
                    Err(err) => {
                        warn!("error deserializing local echo (upon edit): {err}");
                        return;
                    }
                };

                if !self.replace_local_echo(&transaction_id, content).await {
                    warn!("couldn't find the local echo to replace");
                }
            }

            RoomSendQueueUpdate::SendError { transaction_id, error, is_recoverable } => {
                self.update_event_send_state(
                    &transaction_id,
                    EventSendState::SendingFailed { error, is_recoverable },
                )
                .await;
            }

            RoomSendQueueUpdate::RetryEvent { transaction_id } => {
                self.update_event_send_state(&transaction_id, EventSendState::NotSentYet).await;
            }

            RoomSendQueueUpdate::SentEvent { transaction_id, event_id } => {
                self.update_event_send_state(&transaction_id, EventSendState::Sent { event_id })
                    .await;
            }
        }
    }
}

impl TimelineController {
    pub(super) fn room(&self) -> &Room {
        &self.room_data_provider
    }

    /// Given an event identifier, will fetch the details for the event it's
    /// replying to, if applicable.
    #[instrument(skip(self))]
    pub(super) async fn fetch_in_reply_to_details(&self, event_id: &EventId) -> Result<(), Error> {
        let state = self.state.write().await;
        let (index, item) = rfind_event_by_id(&state.items, event_id)
            .ok_or(Error::EventNotInTimeline(TimelineEventItemId::EventId(event_id.to_owned())))?;
        let remote_item = item
            .as_remote()
            .ok_or(Error::EventNotInTimeline(TimelineEventItemId::EventId(event_id.to_owned())))?
            .clone();

        let TimelineItemContent::Message(message) = item.content().clone() else {
            debug!("Event is not a message");
            return Ok(());
        };
        let Some(in_reply_to) = message.in_reply_to() else {
            debug!("Event is not a reply");
            return Ok(());
        };
        if let TimelineDetails::Pending = &in_reply_to.event {
            debug!("Replied-to event is already being fetched");
            return Ok(());
        }
        if let TimelineDetails::Ready(_) = &in_reply_to.event {
            debug!("Replied-to event has already been fetched");
            return Ok(());
        }

        let internal_id = item.internal_id.to_owned();
        let item = item.clone();
        let event = fetch_replied_to_event(
            state,
            index,
            &item,
            internal_id,
            &message,
            &in_reply_to.event_id,
            self.room(),
        )
        .await?;

        // We need to be sure to have the latest position of the event as it might have
        // changed while waiting for the request.
        let mut state = self.state.write().await;
        let (index, item) = rfind_event_by_id(&state.items, &remote_item.event_id)
            .ok_or(Error::EventNotInTimeline(TimelineEventItemId::EventId(event_id.to_owned())))?;

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

        // Now that we've received the content of the replied-to event, replace the
        // replied-to content in the item with it.
        trace!("Updating in-reply-to details");
        let internal_id = item.internal_id.to_owned();
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
                if let Some(prev_event_id) = self.room_data_provider.load_fully_read_marker().await
                {
                    if let Some(relative_pos) =
                        state.meta.compare_events_positions(&prev_event_id, event_id)
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

#[derive(Debug, Error)]
#[error("local echo failed to send in a previous session")]
struct MissingLocalEchoFailError;

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
    mut state: RwLockWriteGuard<'_, TimelineState>,
    index: usize,
    item: &EventTimelineItem,
    internal_id: String,
    message: &Message,
    in_reply_to: &EventId,
    room: &Room,
) -> Result<TimelineDetails<Box<RepliedToEvent>>, Error> {
    if let Some((_, item)) = rfind_event_by_id(&state.items, in_reply_to) {
        let details = TimelineDetails::Ready(Box::new(RepliedToEvent {
            content: item.content.clone(),
            sender: item.sender().to_owned(),
            sender_profile: item.sender_profile().clone(),
        }));

        trace!("Found replied-to event locally");
        return Ok(details);
    };

    // Replace the item with a new timeline item that has the fetching status of the
    // replied-to event to pending.
    trace!("Setting in-reply-to details to pending");
    let reply = message.with_in_reply_to(InReplyToDetails {
        event_id: in_reply_to.to_owned(),
        event: TimelineDetails::Pending,
    });
    let event_item = item.with_content(TimelineItemContent::Message(reply), None);

    let new_timeline_item = TimelineItem::new(event_item, internal_id);
    state.items.set(index, new_timeline_item);

    // Don't hold the state lock while the network request is made
    drop(state);

    trace!("Fetching replied-to event");
    let res = match room.event(in_reply_to, None).await {
        Ok(timeline_event) => TimelineDetails::Ready(Box::new(
            RepliedToEvent::try_from_timeline_event(timeline_event, room).await?,
        )),
        Err(e) => TimelineDetails::Error(Arc::new(e)),
    };
    Ok(res)
}
