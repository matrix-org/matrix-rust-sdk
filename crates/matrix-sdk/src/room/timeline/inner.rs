use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use async_trait::async_trait;
use futures_signals::signal_vec::{MutableVec, MutableVecLockRef, SignalVec};
#[cfg(any(test, feature = "experimental-sliding-sync"))]
use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
use matrix_sdk_base::{
    crypto::OlmMachine,
    deserialized_responses::{EncryptionInfo, TimelineEvent},
    locks::Mutex,
};
use ruma::{
    api::client::message::send_message_event::v3::Response as SendMessageEventResponse,
    events::{
        fully_read::FullyReadEvent, relation::Annotation, AnyMessageLikeEventContent,
        AnySyncTimelineEvent,
    },
    serde::Raw,
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId, RoomId,
    TransactionId, UserId,
};
use tracing::{debug, error, info, warn};
#[cfg(feature = "e2e-encryption")]
use tracing::{instrument, trace};

use super::{
    event_handler::{
        update_read_marker, Flow, HandleEventResult, TimelineEventHandler, TimelineEventKind,
        TimelineEventMetadata, TimelineItemPosition,
    },
    rfind_event_item, EventTimelineItem, Profile, TimelineItem,
};
use crate::{
    events::SyncTimelineEventWithoutContent,
    room::{self, timeline::event_item::RemoteEventTimelineItem},
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
    // Reaction event / txn ID => sender and reaction data
    pub(super) reaction_map:
        HashMap<(Option<OwnedTransactionId>, Option<OwnedEventId>), (OwnedUserId, Annotation)>,
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

    #[cfg(any(test, feature = "experimental-sliding-sync"))]
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

    /// Handle the response returned by the server when a local event has been
    /// sent.
    pub(super) fn handle_local_event_send_response(
        &self,
        txn_id: &TransactionId,
        response: crate::error::Result<SendMessageEventResponse>,
    ) -> crate::error::Result<()> {
        match response {
            Ok(response) => {
                self.update_event_id_of_local_event(txn_id, Some(response.event_id));

                Ok(())
            }
            Err(error) => {
                self.update_event_id_of_local_event(txn_id, None);

                Err(error)
            }
        }
    }

    /// Update the event ID of a local event represented by a transaction ID.
    ///
    /// If the event ID is `None`, it means there is no event ID returned by the
    /// server, so the sending has failed. If the event ID is `Some(_)`, it
    /// means the sending has been successful.
    ///
    /// If no local event is found, a warning is raised.
    pub(super) fn update_event_id_of_local_event(
        &self,
        txn_id: &TransactionId,
        event_id: Option<OwnedEventId>,
    ) {
        let mut lock = self.items.lock_mut();

        // Look for the local event by the transaction ID or event ID.
        let result = rfind_event_item(&lock, |it| {
            it.transaction_id() == Some(txn_id)
                || event_id.is_some() && it.event_id() == event_id.as_deref()
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

        // An event ID already exists, that's a broken state, let's emit an
        // error but also override to the given event ID.
        if let Some(existing_event_id) = &item.event_id {
            error!(
                ?existing_event_id, new_event_id = ?event_id, ?txn_id,
                "Local echo already has an event ID"
            );
        }

        lock.set_cloned(idx, Arc::new(TimelineItem::Event(item.with_event_id(event_id).into())));
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
            Err(error) => {
                error!(?error, "Failed to deserialize `m.fully_read` account data");
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
        for (idx, event_id, session_id, utd) in utds_for_session.iter().rev() {
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
                TimelineItemPosition::Update(*idx),
                &self.items,
                &mut metadata_lock,
                &self.profile_provider,
            )
            .await;
        }
    }
}

impl TimelineInner {
    pub(super) fn room(&self) -> &room::Common {
        &self.profile_provider
    }
}

#[async_trait]
pub(super) trait ProfileProvider {
    fn own_user_id(&self) -> &UserId;
    async fn profile(&self, user_id: &UserId) -> Profile;
}

#[async_trait]
impl ProfileProvider for room::Common {
    fn own_user_id(&self) -> &UserId {
        (**self).own_user_id()
    }

    async fn profile(&self, user_id: &UserId) -> Profile {
        match self.get_member_no_sync(user_id).await {
            Ok(Some(member)) => Profile {
                display_name: member.display_name().map(ToOwned::to_owned),
                display_name_ambiguous: member.name_ambiguous(),
                avatar_url: member.avatar_url().map(ToOwned::to_owned),
            },
            Ok(None) => {
                Profile { display_name: None, display_name_ambiguous: false, avatar_url: None }
            }
            Err(e) => {
                error!(%user_id, "Failed to getch room member information: {e}");
                Profile { display_name: None, display_name_ambiguous: false, avatar_url: None }
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
                    warn!("Failed to deserialize timeline event: {e}");
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
