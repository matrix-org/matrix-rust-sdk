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
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use async_trait::async_trait;
use futures_signals::signal_vec::{MutableVec, MutableVecLockRef, SignalVec};
use indexmap::IndexSet;
use matrix_sdk_base::{
    crypto::OlmMachine,
    deserialized_responses::{EncryptionInfo, SyncTimelineEvent, TimelineEvent},
    locks::Mutex,
};
use ruma::{
    events::{
        fully_read::FullyReadEvent, relation::Annotation, AnyMessageLikeEventContent,
        AnySyncTimelineEvent,
    },
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId, RoomId,
    TransactionId, UserId,
};
use tracing::{debug, error, field::debug, info, warn};
#[cfg(feature = "e2e-encryption")]
use tracing::{instrument, trace};

use super::{
    event_handler::{
        update_read_marker, Flow, HandleEventResult, TimelineEventHandler, TimelineEventKind,
        TimelineEventMetadata, TimelineItemPosition,
    },
    rfind_event_by_id, rfind_event_item, EventSendState, EventTimelineItem, InReplyToDetails,
    Message, Profile, RepliedToEvent, TimelineDetails, TimelineItem, TimelineItemContent,
};
use crate::{
    events::SyncTimelineEventWithoutContent,
    room::{self, timeline::event_item::RemoteEventTimelineItem},
    Error, Result,
};

#[derive(Debug)]
pub(super) struct TimelineInner<P: ProfileProvider = room::Common> {
    items: MutableVec<Arc<TimelineItem>>,
    metadata: Mutex<TimelineInnerMetadata>,
    profile_provider: P,
}

/// Non-signalling parts of `TimelineInner`.
#[derive(Debug, Default)]
pub(super) struct TimelineInnerMetadata {
    /// Reaction event / txn ID => sender and reaction data.
    pub(super) reaction_map:
        HashMap<(Option<OwnedTransactionId>, Option<OwnedEventId>), (OwnedUserId, Annotation)>,
    /// ID of event that is not in the timeline yet => List of reaction event
    /// IDs.
    pub(super) pending_reactions: HashMap<OwnedEventId, IndexSet<OwnedEventId>>,
    pub(super) fully_read_event: Option<OwnedEventId>,
    /// Whether the event that the fully-ready event _refers to_ is part of the
    /// timeline.
    pub(super) fully_read_event_in_timeline: bool,
}

impl<P: ProfileProvider> TimelineInner<P> {
    pub(super) fn new(profile_provider: P) -> Self {
        Self { items: Default::default(), metadata: Default::default(), profile_provider }
    }

    pub(super) fn items(&self) -> MutableVecLockRef<'_, Arc<TimelineItem>> {
        self.items.lock_ref()
    }

    pub(super) fn items_signal(&self) -> impl SignalVec<Item = Arc<TimelineItem>> {
        self.items.signal_vec_cloned()
    }

    pub(super) async fn add_initial_events(&mut self, events: Vec<SyncTimelineEvent>) {
        if events.is_empty() {
            return;
        }

        debug!("Adding {} initial events", events.len());

        let timeline_meta = self.metadata.get_mut();

        for event in events {
            handle_remote_event(
                event.event,
                event.encryption_info,
                TimelineItemPosition::End,
                &self.items,
                timeline_meta,
                &self.profile_provider,
            )
            .await;
        }
    }

    #[cfg(feature = "experimental-sliding-sync")]
    pub(super) async fn clear(&self) {
        let mut timeline_meta = self.metadata.lock().await;
        let mut timeline_items = self.items.lock_mut();

        timeline_meta.reaction_map.clear();
        timeline_meta.fully_read_event = None;
        timeline_meta.fully_read_event_in_timeline = false;

        timeline_items.clear();
    }

    pub(super) async fn handle_live_event(
        &self,
        raw: Raw<AnySyncTimelineEvent>,
        encryption_info: Option<EncryptionInfo>,
    ) {
        let mut timeline_meta = self.metadata.lock().await;
        handle_remote_event(
            raw,
            encryption_info,
            TimelineItemPosition::End,
            &self.items,
            &mut timeline_meta,
            &self.profile_provider,
        )
        .await;
    }

    /// Handle the creation of a new local event.
    pub(super) async fn handle_local_event(
        &self,
        txn_id: OwnedTransactionId,
        content: AnyMessageLikeEventContent,
    ) {
        let sender = self.profile_provider.own_user_id().to_owned();
        let sender_profile = self.profile_provider.profile(&sender).await;
        let event_meta = TimelineEventMetadata {
            sender,
            sender_profile,
            is_own_event: true,
            relations: Default::default(),
            // FIXME: Should we supply something here for encrypted rooms?
            encryption_info: None,
        };

        let flow = Flow::Local { txn_id, timestamp: MilliSecondsSinceUnixEpoch::now() };
        let kind = TimelineEventKind::Message { content };

        let mut timeline_meta = self.metadata.lock().await;
        let mut timeline_items = self.items.lock_mut();
        TimelineEventHandler::new(event_meta, flow, &mut timeline_items, &mut timeline_meta)
            .handle_event(kind);
    }

    /// Update the send state of a local event represented by a transaction ID.
    ///
    /// If no local event is found, a warning is raised.
    pub(super) fn update_event_send_state(
        &self,
        txn_id: &TransactionId,
        send_state: EventSendState,
    ) {
        let mut lock = self.items.lock_mut();

        let new_event_id: Option<&EventId> = match &send_state {
            EventSendState::Sent { event_id } => Some(event_id),
            _ => None,
        };

        // Look for the local event by the transaction ID or event ID.
        let result = rfind_event_item(&lock, |it| {
            it.transaction_id() == Some(txn_id)
                || new_event_id.is_some() && it.event_id() == new_event_id
        });

        let Some((idx, item)) = result else {
            // Event isn't found at all.
            warn!(?txn_id, "Timeline item not found, can't add event ID");
            return;
        };

        let EventTimelineItem::Local(item) = item else {
            // Remote echo already received. This is very unlikely.
            trace!(?txn_id, "Remote echo received before send-event response");
            return;
        };

        // The event was already marked as sent, that's a broken state, let's
        // emit an error but also override to the given sent state.
        if let EventSendState::Sent { event_id: existing_event_id } = &item.send_state {
            let new_event_id = new_event_id.map(debug);
            error!(?existing_event_id, ?new_event_id, ?txn_id, "Local echo already marked as sent");
        }

        let new_item = TimelineItem::Event(item.with_send_state(send_state).into());
        lock.set_cloned(idx, Arc::new(new_item));
    }

    /// Handle a back-paginated event.
    ///
    /// Returns the number of timeline updates that were made.
    pub(super) async fn handle_back_paginated_event(
        &self,
        event: TimelineEvent,
    ) -> HandleEventResult {
        let mut metadata_lock = self.metadata.lock().await;
        handle_remote_event(
            event.event.cast(),
            event.encryption_info,
            TimelineItemPosition::Start,
            &self.items,
            &mut metadata_lock,
            &self.profile_provider,
        )
        .await
    }

    #[instrument(skip_all)]
    pub(super) fn add_loading_indicator(&self) {
        let mut lock = self.items.lock_mut();
        if lock.first().map_or(false, |item| item.is_loading_indicator()) {
            warn!("There is already a loading indicator");
            return;
        }

        lock.insert_cloned(0, Arc::new(TimelineItem::loading_indicator()));
    }

    #[instrument(skip(self))]
    pub(super) fn remove_loading_indicator(&self, more_messages: bool) {
        let mut lock = self.items.lock_mut();
        if !lock.first().map_or(false, |item| item.is_loading_indicator()) {
            warn!("There is no loading indicator");
            return;
        }

        if more_messages {
            lock.remove(0);
        } else {
            lock.set_cloned(0, Arc::new(TimelineItem::timeline_start()))
        }
    }

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

    pub(super) async fn set_fully_read_event(&self, fully_read_event_id: OwnedEventId) {
        let mut metadata_lock = self.metadata.lock().await;

        // A similar event has been handled already. We can ignore it.
        if metadata_lock.fully_read_event.as_ref().map_or(false, |id| *id == fully_read_event_id) {
            return;
        }

        metadata_lock.fully_read_event = Some(fully_read_event_id);

        let mut items_lock = self.items.lock_mut();
        let metadata = &mut *metadata_lock;
        update_read_marker(
            &mut items_lock,
            metadata.fully_read_event.as_deref(),
            &mut metadata.fully_read_event_in_timeline,
        );
    }

    /// Collect events and their metadata that are unable-to-decrypt (UTD)
    /// events in the timeline.
    fn collect_utds(
        &self,
        session_ids: Option<BTreeSet<&str>>,
    ) -> Vec<(usize, OwnedEventId, String, Raw<AnySyncTimelineEvent>)> {
        use super::EncryptedMessage;

        let should_retry = |session_id: &str| {
            let session_ids = &session_ids;

            if let Some(session_ids) = session_ids {
                session_ids.contains(session_id)
            } else {
                true
            }
        };

        self.items
            .lock_ref()
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| {
                let event_item = &item.as_event()?;
                let utd = event_item.content().as_unable_to_decrypt()?;

                match utd {
                    EncryptedMessage::MegolmV1AesSha2 { session_id, .. }
                        if should_retry(session_id) =>
                    {
                        let EventTimelineItem::Remote(RemoteEventTimelineItem { event_id, raw, .. }) = event_item else {
                            error!("Key for unable-to-decrypt timeline item is not an event ID");
                            return None;
                        };

                        Some((
                            idx,
                            event_id.to_owned(),
                            session_id.to_owned(),
                            raw.clone(),
                        ))
                    }
                    EncryptedMessage::MegolmV1AesSha2 { .. }
                    | EncryptedMessage::OlmV1Curve25519AesSha2 { .. }
                    | EncryptedMessage::Unknown => None,
                }
            })
            .collect()
    }

    #[cfg(feature = "e2e-encryption")]
    #[instrument(skip(self, olm_machine))]
    pub(super) async fn retry_event_decryption(
        &self,
        room_id: &RoomId,
        olm_machine: &OlmMachine,
        session_ids: Option<BTreeSet<&str>>,
    ) {
        debug!("Retrying decryption");

        let utds_for_session = self.collect_utds(session_ids);

        if utds_for_session.is_empty() {
            trace!("Found no events to retry decryption for");
            return;
        }

        let mut metadata_lock = self.metadata.lock().await;
        for (idx, event_id, session_id, utd) in utds_for_session {
            let event = match olm_machine.decrypt_room_event(utd.cast_ref(), room_id).await {
                Ok(ev) => ev,
                Err(e) => {
                    info!(
                        ?event_id,
                        ?session_id,
                        "Failed to decrypt event after receiving room key: {e}"
                    );
                    continue;
                }
            };

            trace!(
                ?event_id,
                ?session_id,
                "Successfully decrypted event that previously failed to decrypt"
            );

            handle_remote_event(
                event.event.cast(),
                event.encryption_info,
                TimelineItemPosition::Update(idx),
                &self.items,
                &mut metadata_lock,
                &self.profile_provider,
            )
            .await;
        }
    }

    pub(super) fn set_sender_profiles_pending(&self) {
        self.set_non_ready_sender_profiles(TimelineDetails::Pending);
    }

    pub(super) fn set_sender_profiles_error(&self, error: Arc<Error>) {
        self.set_non_ready_sender_profiles(TimelineDetails::Error(error));
    }

    fn set_non_ready_sender_profiles(&self, state: TimelineDetails<Profile>) {
        let mut timeline_items = self.items.lock_mut();
        for idx in 0..timeline_items.len() {
            let Some(event_item) = timeline_items[idx].as_event() else { continue };
            if !matches!(event_item.sender_profile(), TimelineDetails::Ready(_)) {
                timeline_items.set_cloned(
                    idx,
                    Arc::new(TimelineItem::Event(event_item.with_sender_profile(state.clone()))),
                );
            }
        }
    }

    pub(super) async fn update_sender_profiles(&self) {
        // Can't lock the timeline items across .await points without making the
        // resulting future `!Send`. As a (brittle) hack around that, lock the
        // timeline items in each loop iteration but keep a lock of the metadata
        // so no event handler runs in parallel and assert the number of items
        // doesn't change between iterations.
        let _guard = self.metadata.lock().await;
        let num_items = self.items().len();

        for idx in 0..num_items {
            let sender = match self.items()[idx].as_event() {
                Some(event_item) => event_item.sender().to_owned(),
                None => continue,
            };
            let maybe_profile = self.profile_provider.profile(&sender).await;

            let mut timeline_items = self.items.lock_mut();
            assert_eq!(timeline_items.len(), num_items);

            let event_item = timeline_items[idx].as_event().unwrap();
            match maybe_profile {
                Some(profile) => {
                    if !event_item.sender_profile().contains(&profile) {
                        let updated_item =
                            event_item.with_sender_profile(TimelineDetails::Ready(profile));
                        timeline_items.set_cloned(idx, Arc::new(TimelineItem::Event(updated_item)));
                    }
                }
                None => {
                    if !event_item.sender_profile().is_unavailable() {
                        let updated_item =
                            event_item.with_sender_profile(TimelineDetails::Unavailable);
                        timeline_items.set_cloned(idx, Arc::new(TimelineItem::Event(updated_item)));
                    }
                }
            }
        }
    }

    fn update_event_item(&self, index: usize, event_item: EventTimelineItem) {
        self.items.lock_mut().set_cloned(index, Arc::new(TimelineItem::Event(event_item)))
    }
}

impl TimelineInner {
    pub(super) fn room(&self) -> &room::Common {
        &self.profile_provider
    }

    pub(super) async fn fetch_in_reply_to_details(
        &self,
        index: usize,
        mut item: RemoteEventTimelineItem,
    ) -> Result<RemoteEventTimelineItem> {
        let TimelineItemContent::Message(message) = item.content.clone() else {
            return Ok(item);
        };
        let Some(in_reply_to) = message.in_reply_to() else {
            return Ok(item);
        };

        let details =
            self.fetch_replied_to_event(index, &item, &message, &in_reply_to.event_id).await;

        // We need to be sure to have the latest position of the event as it might have
        // changed while waiting for the request.
        let (index, _) = rfind_event_by_id(&self.items(), &item.event_id)
            .ok_or(super::Error::RemoteEventNotInTimeline)?;

        item = item.with_content(TimelineItemContent::Message(message.with_in_reply_to(
            InReplyToDetails { event_id: in_reply_to.event_id.clone(), details },
        )));
        self.update_event_item(index, item.clone().into());

        Ok(item)
    }

    async fn fetch_replied_to_event(
        &self,
        index: usize,
        item: &RemoteEventTimelineItem,
        message: &Message,
        in_reply_to: &EventId,
    ) -> TimelineDetails<Box<RepliedToEvent>> {
        if let Some((_, item)) = rfind_event_by_id(&self.items(), in_reply_to) {
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

        self.update_event_item(
            index,
            item.with_content(TimelineItemContent::Message(message.with_in_reply_to(
                InReplyToDetails {
                    event_id: in_reply_to.to_owned(),
                    details: TimelineDetails::Pending,
                },
            )))
            .into(),
        );

        match self.room().event(in_reply_to).await {
            Ok(timeline_event) => {
                match RepliedToEvent::try_from_timeline_event(
                    timeline_event,
                    &self.profile_provider,
                )
                .await
                {
                    Ok(event) => TimelineDetails::Ready(Box::new(event)),
                    Err(e) => TimelineDetails::Error(Arc::new(e)),
                }
            }
            Err(e) => TimelineDetails::Error(Arc::new(e)),
        }
    }
}

#[async_trait]
pub(super) trait ProfileProvider {
    fn own_user_id(&self) -> &UserId;
    async fn profile(&self, user_id: &UserId) -> Option<Profile>;
}

#[async_trait]
impl ProfileProvider for room::Common {
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
}

/// Handle a remote event.
///
/// Returns the number of timeline updates that were made.
async fn handle_remote_event<P: ProfileProvider>(
    raw: Raw<AnySyncTimelineEvent>,
    encryption_info: Option<EncryptionInfo>,
    position: TimelineItemPosition,
    // MutableVecLock can't be held across `.await`s in `Send` futures, so we
    // can't lock it ahead of time like `timeline_meta`.
    timeline_items: &MutableVec<Arc<TimelineItem>>,
    timeline_meta: &mut TimelineInnerMetadata,
    profile_provider: &P,
) -> HandleEventResult {
    let (event_id, sender, origin_server_ts, txn_id, relations, event_kind) =
        match raw.deserialize() {
            Ok(event) => (
                event.event_id().to_owned(),
                event.sender().to_owned(),
                event.origin_server_ts(),
                event.transaction_id().map(ToOwned::to_owned),
                event.relations().to_owned(),
                event.into(),
            ),
            Err(e) => match raw.deserialize_as::<SyncTimelineEventWithoutContent>() {
                Ok(event) => (
                    event.event_id().to_owned(),
                    event.sender().to_owned(),
                    event.origin_server_ts(),
                    event.transaction_id().map(ToOwned::to_owned),
                    event.relations().to_owned(),
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

    let is_own_event = sender == profile_provider.own_user_id();
    let sender_profile = profile_provider.profile(&sender).await;
    let event_meta =
        TimelineEventMetadata { sender, sender_profile, is_own_event, relations, encryption_info };
    let flow = Flow::Remote { event_id, origin_server_ts, raw_event: raw, txn_id, position };

    let mut timeline_items = timeline_items.lock_mut();
    TimelineEventHandler::new(event_meta, flow, &mut timeline_items, timeline_meta)
        .handle_event(event_kind)
}
