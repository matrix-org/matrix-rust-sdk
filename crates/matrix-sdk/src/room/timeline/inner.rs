use std::{collections::BTreeSet, sync::Arc};

use futures_signals::signal_vec::{MutableVec, MutableVecLockMut};
use matrix_sdk_base::{
    crypto::OlmMachine,
    deserialized_responses::{EncryptionInfo, SyncTimelineEvent, TimelineEvent},
    locks::Mutex,
};
use ruma::{
    events::{fully_read::FullyReadEvent, AnyMessageLikeEventContent, AnySyncTimelineEvent},
    serde::Raw,
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, RoomId, TransactionId, UserId,
};
use tracing::{error, info, warn};

use super::{
    event_handler::{
        update_read_marker, Flow, TimelineEventHandler, TimelineEventKind, TimelineEventMetadata,
        TimelineItemPosition,
    },
    find_event_by_txn_id, TimelineInnerMetadata, TimelineItem, TimelineKey,
};
use crate::events::SyncTimelineEventWithoutContent;

#[derive(Debug, Default)]
pub(super) struct TimelineInner {
    pub(super) items: MutableVec<Arc<TimelineItem>>,
    pub(super) metadata: Mutex<TimelineInnerMetadata>,
}

impl TimelineInner {
    pub(super) fn add_initial_events(
        &mut self,
        events: Vec<SyncTimelineEvent>,
        own_user_id: &UserId,
    ) {
        let timeline_meta = self.metadata.get_mut();
        let timeline_items = &mut self.items.lock_mut();

        for event in events {
            handle_remote_event(
                event.event,
                own_user_id,
                event.encryption_info,
                TimelineItemPosition::End,
                timeline_items,
                timeline_meta,
            );
        }
    }

    pub(super) async fn handle_live_event(
        &self,
        raw: Raw<AnySyncTimelineEvent>,
        encryption_info: Option<EncryptionInfo>,
        own_user_id: &UserId,
    ) {
        let mut timeline_meta = self.metadata.lock().await;
        handle_remote_event(
            raw,
            own_user_id,
            encryption_info,
            TimelineItemPosition::End,
            &mut self.items.lock_mut(),
            &mut timeline_meta,
        );
    }

    pub(super) async fn handle_local_event(
        &self,
        txn_id: OwnedTransactionId,
        content: AnyMessageLikeEventContent,
        own_user_id: &UserId,
    ) {
        let event_meta = TimelineEventMetadata {
            sender: own_user_id.to_owned(),
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

    pub(super) async fn handle_back_paginated_event(
        &self,
        event: TimelineEvent,
        own_user_id: &UserId,
    ) {
        let mut metadata_lock = self.metadata.lock().await;
        handle_remote_event(
            event.event.cast(),
            own_user_id,
            event.encryption_info,
            TimelineItemPosition::Start,
            &mut self.items.lock_mut(),
            &mut metadata_lock,
        );
    }

    pub(super) fn add_event_id(&self, txn_id: &TransactionId, event_id: OwnedEventId) {
        let mut lock = self.items.lock_mut();
        if let Some((idx, item)) = find_event_by_txn_id(&lock, txn_id) {
            if item.event_id.as_ref().map_or(false, |ev_id| *ev_id != event_id) {
                error!("remote echo and send-event response disagree on the event ID");
            }

            lock.set_cloned(idx, Arc::new(TimelineItem::Event(item.with_event_id(Some(event_id)))));
        } else {
            warn!(%txn_id, "Timeline item not found, can't add event ID");
        }
    }

    pub(super) async fn handle_fully_read(&self, raw: Raw<FullyReadEvent>) {
        let fully_read_event = match raw.deserialize() {
            Ok(ev) => ev.content.event_id,
            Err(error) => {
                error!(?error, "Failed to deserialize `m.fully_read` account data");
                return;
            }
        };

        self.set_fully_read_event(fully_read_event).await;
    }

    pub(super) async fn set_fully_read_event(&self, fully_read_event_id: OwnedEventId) {
        let mut metadata_lock = self.metadata.lock().await;

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

    #[cfg(feature = "e2e-encryption")]
    pub(super) async fn retry_event_decryption(
        &self,
        room_id: &RoomId,
        olm_machine: &OlmMachine,
        session_ids: BTreeSet<&str>,
        own_user_id: &UserId,
    ) {
        use super::EncryptedMessage;

        let utds_for_session: Vec<_> = self
            .items
            .lock_ref()
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| {
                let event_item = &item.as_event()?;
                let utd = event_item.content.as_unable_to_decrypt()?;

                match utd {
                    EncryptedMessage::MegolmV1AesSha2 { session_id, .. }
                        if session_ids.contains(session_id.as_str()) =>
                    {
                        let TimelineKey::EventId(event_id) = &event_item.key else {
                            error!("Key for unable-to-decrypt timeline item is not an event ID");
                            return None;
                        };
                        let Some(raw) = event_item.raw.clone() else {
                            error!("No raw event in unable-to-decrypt timeline item");
                            return None;
                        };

                        Some((idx, event_id.to_owned(), session_id.to_owned(), raw))
                    }
                    EncryptedMessage::MegolmV1AesSha2 { .. }
                    | EncryptedMessage::OlmV1Curve25519AesSha2 { .. }
                    | EncryptedMessage::Unknown => None,
                }
            })
            .collect();

        if utds_for_session.is_empty() {
            return;
        }

        let mut metadata_lock = self.metadata.lock().await;
        for (idx, event_id, session_id, utd) in utds_for_session.iter().rev() {
            let event = match olm_machine.decrypt_room_event(utd.cast_ref(), room_id).await {
                Ok(ev) => ev,
                Err(e) => {
                    info!(
                        %event_id, %session_id,
                        "Failed to decrypt event after receiving room key: {e}"
                    );
                    continue;
                }
            };

            // Because metadata is always locked before we attempt to lock the
            // items, this will never be contended.
            // Because there is an `.await` in this loop, we have to re-lock
            // this mutex every iteration because holding it across `.await`
            // makes the future `!Send`, which makes it not event-handler-safe.
            let mut items_lock = self.items.lock_mut();
            handle_remote_event(
                event.event.cast(),
                own_user_id,
                event.encryption_info,
                TimelineItemPosition::Update(*idx),
                &mut items_lock,
                &mut metadata_lock,
            );
        }
    }
}

fn handle_remote_event(
    raw: Raw<AnySyncTimelineEvent>,
    own_user_id: &UserId,
    encryption_info: Option<EncryptionInfo>,
    position: TimelineItemPosition,
    timeline_items: &mut MutableVecLockMut<'_, Arc<TimelineItem>>,
    timeline_meta: &mut TimelineInnerMetadata,
) {
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
                    return;
                }
            },
        };

    let is_own_event = sender == own_user_id;
    let event_meta = TimelineEventMetadata { sender, is_own_event, relations, encryption_info };
    let flow = Flow::Remote { event_id, origin_server_ts, raw_event: raw, txn_id, position };

    TimelineEventHandler::new(event_meta, flow, timeline_items, timeline_meta)
        .handle_event(event_kind)
}
