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
use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use eyeball_im::{ObservableVector, VectorSubscriber};
use imbl::Vector;
use indexmap::{IndexMap, IndexSet};
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_base::crypto::OlmMachine;
use matrix_sdk_base::deserialized_responses::{EncryptionInfo, SyncTimelineEvent, TimelineEvent};
#[cfg(feature = "e2e-encryption")]
use ruma::RoomId;
use ruma::{
    api::client::receipt::create_receipt::v3::ReceiptType as SendReceiptType,
    events::{
        fully_read::FullyReadEvent,
        receipt::{Receipt, ReceiptEventContent, ReceiptThread, ReceiptType},
        relation::Annotation,
        AnyMessageLikeEventContent, AnySyncTimelineEvent,
    },
    push::{Action, PushConditionRoomCtx, Ruleset},
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
    TransactionId, UserId,
};
use tokio::sync::{Mutex, MutexGuard};
use tracing::{debug, error, field::debug, instrument, trace, warn};
#[cfg(feature = "e2e-encryption")]
use tracing::{field, info, info_span, Instrument as _};

use super::{
    compare_events_positions,
    event_handler::{
        update_read_marker, Flow, HandleEventResult, TimelineEventHandler, TimelineEventKind,
        TimelineEventMetadata, TimelineItemPosition,
    },
    read_receipts::{
        handle_explicit_read_receipts, latest_user_read_receipt, load_read_receipts_for_event,
        user_receipt,
    },
    rfind_event_by_id, rfind_event_item, EventSendState, EventTimelineItem, InReplyToDetails,
    Message, Profile, RelativePosition, RepliedToEvent, TimelineDetails, TimelineItem,
    TimelineItemContent,
};
use crate::{events::SyncTimelineEventWithoutContent, room, Error, Result};

#[derive(Debug)]
pub(super) struct TimelineInner<P: RoomDataProvider = room::Common> {
    state: Mutex<TimelineInnerState>,
    room_data_provider: P,
    track_read_receipts: bool,
}

#[derive(Debug, Default)]
pub(super) struct TimelineInnerState {
    pub(super) items: ObservableVector<Arc<TimelineItem>>,
    /// Reaction event / txn ID => sender and reaction data.
    pub(super) reaction_map:
        HashMap<(Option<OwnedTransactionId>, Option<OwnedEventId>), (OwnedUserId, Annotation)>,
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
}

impl<P: RoomDataProvider> TimelineInner<P> {
    pub(super) fn new(room_data_provider: P) -> Self {
        let state = TimelineInnerState {
            // Upstream default capacity is currently 16, which is making
            // sliding-sync tests with 20 events lag. This should still be
            // small enough.
            items: ObservableVector::with_capacity(32),
            ..Default::default()
        };
        Self { state: Mutex::new(state), room_data_provider, track_read_receipts: false }
    }

    pub(super) fn with_read_receipt_tracking(mut self, track_read_receipts: bool) -> Self {
        self.track_read_receipts = track_read_receipts;
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

    pub(super) fn set_initial_user_receipt(
        &mut self,
        receipt_type: ReceiptType,
        receipt: (OwnedEventId, Receipt),
    ) {
        let own_user_id = self.room_data_provider.own_user_id().to_owned();
        self.state
            .get_mut()
            .users_read_receipts
            .entry(own_user_id)
            .or_default()
            .insert(receipt_type, receipt);
    }

    pub(super) async fn add_initial_events(&mut self, events: Vector<SyncTimelineEvent>) {
        if events.is_empty() {
            return;
        }

        debug!("Adding {} initial events", events.len());

        let state = self.state.get_mut();

        for event in events {
            handle_remote_event(
                event.event,
                event.encryption_info,
                event.push_actions,
                TimelineItemPosition::End { from_cache: true },
                state,
                &self.room_data_provider,
                self.track_read_receipts,
            )
            .await;
        }
    }

    #[cfg(feature = "experimental-sliding-sync")]
    pub(super) async fn clear(&self) {
        trace!("Clearing timeline");

        let mut state = self.state.lock().await;
        state.items.clear();
        state.reaction_map.clear();
        state.fully_read_event = None;
        state.event_should_update_fully_read_marker = false;
    }

    #[instrument(skip_all)]
    pub(super) async fn handle_live_event(
        &self,
        raw: Raw<AnySyncTimelineEvent>,
        encryption_info: Option<EncryptionInfo>,
        push_actions: Vec<Action>,
    ) {
        let mut state = self.state.lock().await;
        handle_remote_event(
            raw,
            encryption_info,
            push_actions,
            TimelineItemPosition::End { from_cache: false },
            &mut state,
            &self.room_data_provider,
            self.track_read_receipts,
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
        let sender_profile = self.room_data_provider.profile(&sender).await;
        let event_meta = TimelineEventMetadata {
            sender,
            sender_profile,
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

        let mut state = self.state.lock().await;
        TimelineEventHandler::new(event_meta, flow, &mut state, self.track_read_receipts)
            .handle_event(kind);
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

        // Look for the local event by the transaction ID or event ID.
        let result = rfind_event_item(&state.items, |it| {
            it.transaction_id() == Some(txn_id)
                || new_event_id.is_some() && it.event_id() == new_event_id
        });

        let Some((idx, item)) = result else {
            // Event isn't found at all.
            warn!("Timeline item not found, can't add event ID");
            return;
        };

        let Some(local_item) = item.as_local() else {
            // Remote echo already received. This is very unlikely.
            trace!("Remote echo received before send-event response");
            return;
        };

        // The event was already marked as sent, that's a broken state, let's
        // emit an error but also override to the given sent state.
        if let EventSendState::Sent { event_id: existing_event_id } = &local_item.send_state {
            let new_event_id = new_event_id.map(debug);
            error!(?existing_event_id, ?new_event_id, "Local echo already marked as sent");
        }

        let new_item = TimelineItem::Event(item.with_kind(local_item.with_send_state(send_state)));
        state.items.set(idx, Arc::new(new_item));
    }

    /// Handle a back-paginated event.
    ///
    /// Returns the number of timeline updates that were made.
    #[instrument(skip_all)]
    pub(super) async fn handle_back_paginated_event(
        &self,
        event: TimelineEvent,
    ) -> HandleEventResult {
        let mut state = self.state.lock().await;
        handle_remote_event(
            event.event.cast(),
            event.encryption_info,
            event.push_actions,
            TimelineItemPosition::Start,
            &mut state,
            &self.room_data_provider,
            self.track_read_receipts,
        )
        .await
    }

    #[instrument(skip_all)]
    pub(super) async fn add_loading_indicator(&self) {
        let mut state = self.state.lock().await;

        if state.items.front().map_or(false, |item| item.is_loading_indicator()) {
            warn!("There is already a loading indicator");
            return;
        }

        state.items.push_front(Arc::new(TimelineItem::loading_indicator()));
    }

    #[instrument(skip(self))]
    pub(super) async fn remove_loading_indicator(&self, more_messages: bool) {
        let mut state = self.state.lock().await;

        if !state.items.front().map_or(false, |item| item.is_loading_indicator()) {
            warn!("There is no loading indicator");
            return;
        }

        if more_messages {
            state.items.pop_front();
        } else {
            state.items.set(0, Arc::new(TimelineItem::timeline_start()));
        }
    }

    #[instrument(skip_all)]
    pub(super) async fn handle_fully_read(&self, raw: Raw<FullyReadEvent>) {
        let fully_read_event_id = match raw.deserialize() {
            Ok(ev) => ev.content.event_id,
            Err(e) => {
                error!("Failed to deserialize fully-read account data: {e}");
                return;
            }
        };

        self.set_fully_read_event(fully_read_event_id).await;
    }

    #[instrument(skip_all)]
    pub(super) async fn set_fully_read_event(&self, fully_read_event_id: OwnedEventId) {
        let mut state = self.state.lock().await;

        // A similar event has been handled already. We can ignore it.
        if state.fully_read_event.as_ref().map_or(false, |id| *id == fully_read_event_id) {
            return;
        }

        state.fully_read_event = Some(fully_read_event_id);

        let state = &mut *state;
        update_read_marker(
            &mut state.items,
            state.fully_read_event.as_deref(),
            &mut state.event_should_update_fully_read_marker,
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[instrument(skip(self, olm_machine))]
    pub(super) async fn retry_event_decryption(
        &self,
        room_id: &RoomId,
        olm_machine: &OlmMachine,
        session_ids: Option<BTreeSet<&str>>,
    ) {
        use super::EncryptedMessage;

        trace!("Retrying decryption");

        let push_rules_context = self.room_data_provider.push_rules_and_context().await;

        let should_retry = |session_id: &str| {
            if let Some(session_ids) = &session_ids {
                session_ids.contains(session_id)
            } else {
                true
            }
        };

        let retry_one = |item: Arc<TimelineItem>| {
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

                let raw = remote_event.original_json.cast_ref();
                match olm_machine.decrypt_room_event(raw, room_id).await {
                    Ok(event) => {
                        trace!("Successfully decrypted event that previously failed to decrypt");
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

        let mut state = self.state.lock().await;

        // We loop through all the items in the timeline, if we successfully
        // decrypt a UTD item we either replace it or remove it and update
        // another one.
        let mut idx = 0;
        while let Some(item) = state.items.get(idx) {
            let Some(event) = retry_one(item.clone()).await else {
                idx += 1;
                continue;
            };

            let push_actions = push_rules_context
                .as_ref()
                .map(|(push_rules, push_context)| {
                    push_rules.get_actions(&event.event, push_context).to_owned()
                })
                .unwrap_or_default();

            let result = handle_remote_event(
                event.event.cast(),
                event.encryption_info,
                push_actions,
                TimelineItemPosition::Update(idx),
                &mut state,
                &self.room_data_provider,
                self.track_read_receipts,
            )
            .await;

            // If the UTD was removed rather than updated, run the loop again
            // with the same index.
            if !result.item_removed {
                idx += 1;
            }
        }
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
            let Some(event_item) = state.items[idx].as_event() else { continue };
            if !matches!(event_item.sender_profile(), TimelineDetails::Ready(_)) {
                let item = Arc::new(TimelineItem::Event(
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

            let event_item = state.items[idx].as_event().unwrap();
            match maybe_profile {
                Some(profile) => {
                    if !event_item.sender_profile().contains(&profile) {
                        let updated_item =
                            event_item.with_sender_profile(TimelineDetails::Ready(profile));
                        state.items.set(idx, Arc::new(TimelineItem::Event(updated_item)));
                    }
                }
                None => {
                    if !event_item.sender_profile().is_unavailable() {
                        let updated_item =
                            event_item.with_sender_profile(TimelineDetails::Unavailable);
                        state.items.set(idx, Arc::new(TimelineItem::Event(updated_item)));
                    }
                }
            }
        }
    }

    pub(super) async fn handle_read_receipts(&self, receipt_event_content: ReceiptEventContent) {
        let mut state = self.state.lock().await;
        let own_user_id = self.room_data_provider.own_user_id();

        handle_explicit_read_receipts(receipt_event_content, own_user_id, &mut state)
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

    pub(super) async fn fetch_in_reply_to_details(&self, event_id: &EventId) -> Result<()> {
        let state = self.state.lock().await;
        let (index, item) = rfind_event_by_id(&state.items, event_id)
            .ok_or(super::Error::RemoteEventNotInTimeline)?;
        let remote_item = item.as_remote().ok_or(super::Error::RemoteEventNotInTimeline)?.clone();

        let TimelineItemContent::Message(message) = item.content().clone() else {
            return Ok(());
        };
        let Some(in_reply_to) = message.in_reply_to() else {
            return Ok(());
        };

        let item = item.clone();
        let event = fetch_replied_to_event(
            state,
            index,
            &item,
            &message,
            &in_reply_to.event_id,
            self.room(),
        )
        .await;

        // We need to be sure to have the latest position of the event as it might have
        // changed while waiting for the request.
        let mut state = self.state.lock().await;
        let (index, item) = rfind_event_by_id(&state.items, &remote_item.event_id)
            .ok_or(super::Error::RemoteEventNotInTimeline)?;

        // Check the state of the event again, it might have been redacted while
        // the request was in-flight.
        let TimelineItemContent::Message(message) = item.content().clone() else {
            return Ok(());
        };
        let Some(in_reply_to) = message.in_reply_to() else {
            return Ok(());
        };

        let mut item = item.clone();
        item.set_content(TimelineItemContent::Message(
            message.with_in_reply_to(InReplyToDetails {
                event_id: in_reply_to.event_id.clone(),
                event,
            }),
        ));
        state.items.set(index, Arc::new(item.into()));

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

        latest_user_read_receipt(user_id, &state, room).await
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
                    user_receipt(own_user_id, ReceiptType::Read, &state, room).await
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
                    latest_user_read_receipt(own_user_id, &state, room).await
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

async fn fetch_replied_to_event(
    mut state: MutexGuard<'_, TimelineInnerState>,
    index: usize,
    item: &EventTimelineItem,
    message: &Message,
    in_reply_to: &EventId,
    room: &room::Common,
) -> TimelineDetails<Box<RepliedToEvent>> {
    if let Some((_, item)) = rfind_event_by_id(&state.items, in_reply_to) {
        let details = match item.content() {
            TimelineItemContent::Message(message) => {
                TimelineDetails::Ready(Box::new(RepliedToEvent {
                    message: message.clone(),
                    sender: item.sender().to_owned(),
                    sender_profile: item.sender_profile().clone(),
                }))
            }
            _ => TimelineDetails::Error(Arc::new(super::Error::UnsupportedEvent.into())),
        };

        return details;
    };

    let reply = message.with_in_reply_to(InReplyToDetails {
        event_id: in_reply_to.to_owned(),
        event: TimelineDetails::Pending,
    });
    let event_item = item.apply_edit(TimelineItemContent::Message(reply), None);
    state.items.set(index, Arc::new(event_item.into()));

    // Don't hold the state lock while the network request is made
    drop(state);

    match room.event(in_reply_to).await {
        Ok(timeline_event) => {
            match RepliedToEvent::try_from_timeline_event(timeline_event, room).await {
                Ok(event) => TimelineDetails::Ready(Box::new(event)),
                Err(e) => TimelineDetails::Error(Arc::new(e)),
            }
        }
        Err(e) => TimelineDetails::Error(Arc::new(e)),
    }
}

#[async_trait]
pub(super) trait RoomDataProvider {
    fn own_user_id(&self) -> &UserId;
    async fn profile(&self, user_id: &UserId) -> Option<Profile>;
    async fn read_receipts_for_event(&self, event_id: &EventId) -> IndexMap<OwnedUserId, Receipt>;
    async fn push_rules_and_context(&self) -> Option<(Ruleset, PushConditionRoomCtx)>;
}

#[async_trait]
impl RoomDataProvider for room::Common {
    fn own_user_id(&self) -> &UserId {
        (**self).own_user_id()
    }

    async fn profile(&self, user_id: &UserId) -> Option<Profile> {
        match self.get_member_no_sync(user_id).await {
            Ok(Some(member)) => Some(Profile {
                display_name: member.display_name().map(ToOwned::to_owned),
                display_name_ambiguous: member.name_ambiguous(),
                avatar_url: member.avatar_url().map(ToOwned::to_owned),
            }),
            Ok(None) if self.are_members_synced() => Some(Profile {
                display_name: None,
                display_name_ambiguous: false,
                avatar_url: None,
            }),
            Ok(None) => None,
            Err(e) => {
                error!(%user_id, "Failed to getch room member information: {e}");
                None
            }
        }
    }

    async fn read_receipts_for_event(&self, event_id: &EventId) -> IndexMap<OwnedUserId, Receipt> {
        match self.event_receipts(ReceiptType::Read, ReceiptThread::Unthreaded, event_id).await {
            Ok(receipts) => receipts.into_iter().collect(),
            Err(e) => {
                error!(?event_id, "Failed to get read receipts for event: {e}");
                IndexMap::new()
            }
        }
    }

    async fn push_rules_and_context(&self) -> Option<(Ruleset, PushConditionRoomCtx)> {
        match self.push_context().await {
            Ok(Some(push_context)) => match self.client().account().push_rules().await {
                Ok(push_rules) => Some((push_rules, push_context)),
                Err(e) => {
                    error!("Could not get push rules: {e}");
                    None
                }
            },
            Ok(None) => {
                debug!("Could not aggregate push context");
                None
            }
            Err(e) => {
                error!("Could not get push context: {e}");
                None
            }
        }
    }
}

/// Handle a remote event.
///
/// Returns the number of timeline updates that were made.
async fn handle_remote_event<P: RoomDataProvider>(
    raw: Raw<AnySyncTimelineEvent>,
    encryption_info: Option<EncryptionInfo>,
    push_actions: Vec<Action>,
    position: TimelineItemPosition,
    timeline_state: &mut TimelineInnerState,
    room_data_provider: &P,
    track_read_receipts: bool,
) -> HandleEventResult {
    let (event_id, sender, timestamp, txn_id, event_kind) = match raw.deserialize() {
        Ok(event) => (
            event.event_id().to_owned(),
            event.sender().to_owned(),
            event.origin_server_ts(),
            event.transaction_id().map(ToOwned::to_owned),
            event.into(),
        ),
        Err(e) => match raw.deserialize_as::<SyncTimelineEventWithoutContent>() {
            Ok(event) => (
                event.event_id().to_owned(),
                event.sender().to_owned(),
                event.origin_server_ts(),
                event.transaction_id().map(ToOwned::to_owned),
                TimelineEventKind::failed_to_parse(event, e),
            ),
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
    let read_receipts = if track_read_receipts {
        load_read_receipts_for_event(&event_id, timeline_state, room_data_provider).await
    } else {
        Default::default()
    };
    let is_highlighted = push_actions.iter().any(Action::is_highlight);
    let event_meta = TimelineEventMetadata {
        sender,
        sender_profile,
        timestamp,
        is_own_event,
        encryption_info,
        read_receipts,
        is_highlighted,
    };
    let flow = Flow::Remote { event_id, raw_event: raw, txn_id, position };

    TimelineEventHandler::new(event_meta, flow, timeline_state, track_read_receipts)
        .handle_event(event_kind)
}
