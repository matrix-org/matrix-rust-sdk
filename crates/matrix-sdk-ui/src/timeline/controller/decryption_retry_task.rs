// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use std::{collections::BTreeSet, sync::Arc};

use imbl::Vector;
use itertools::{Either, Itertools as _};
use matrix_sdk::{
    deserialized_responses::TimelineEventKind as SdkTimelineEventKind, executor::JoinHandle,
};
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    RwLock,
};
use tracing::{debug, error, field, info, info_span, Instrument as _};

use crate::timeline::{
    controller::{TimelineSettings, TimelineState},
    event_item::EventTimelineItemKind,
    traits::{Decryptor, RoomDataProvider},
    EncryptedMessage, EventTimelineItem, TimelineItem, TimelineItemKind,
};

/// Holds a long-running task that is used to retry decryption of items in the
/// timeline when new information about a session is received.
///
/// Creating an instance with [`DecryptionRetryTask::new`] creates the async
/// task, and a channel that is used to communicate with it.
///
/// The underlying async task will stop soon after the [`DecryptionRetryTask`]
/// is dropped, because it waits for the channel to close, which happens when we
/// drop the sending side.
#[derive(Clone, Debug)]
pub struct DecryptionRetryTask<D: Decryptor> {
    /// The sending side of the channel that we have open to the long-running
    /// async task. Every time we want to retry decrypting some events, we
    /// send a [`DecryptionRetryRequest`] along this channel. Users of this
    /// struct call [`DecryptionRetryTask::decrypt`] to do this.
    sender: Sender<DecryptionRetryRequest<D>>,

    /// The join handle of the task. We don't actually use this, since the task
    /// will end soon after we are dropped, because when `sender` is dropped the
    /// task will see that the channel closed, but we hold on to the handle to
    /// indicate that we own the task.
    _task_handle: Arc<JoinHandle<()>>,
}

/// How many concurrent retry requests we will queue before blocking when
/// attempting to queue another. We don't normally expect more than one or two
/// will be queued at a time, so blocking should be a rare occurrence.
const CHANNEL_BUFFER_SIZE: usize = 100;

impl<D: Decryptor> DecryptionRetryTask<D> {
    pub(crate) fn new<P: RoomDataProvider>(
        state: Arc<RwLock<TimelineState>>,
        room_data_provider: P,
    ) -> Self {
        // We will send decryption requests down this channel to the long-running task
        let (sender, receiver) = mpsc::channel(CHANNEL_BUFFER_SIZE);

        // Spawn the long-running task, providing the receiver so we can listen for
        // decryption requests
        let handle =
            matrix_sdk::executor::spawn(decryption_task(state, room_data_provider, receiver));

        // Keep hold of the sender so we can send off decryption requests to the task.
        Self { sender, _task_handle: Arc::new(handle) }
    }

    /// Use the supplied decryptor to attempt redecryption of the events
    /// associated with the supplied session IDs.
    pub(crate) async fn decrypt(
        &self,
        decryptor: D,
        session_ids: Option<BTreeSet<String>>,
        settings: TimelineSettings,
    ) {
        let res =
            self.sender.send(DecryptionRetryRequest { decryptor, session_ids, settings }).await;

        if let Err(error) = res {
            error!("Failed to send decryption retry request: {error}");
        }
    }
}

/// The information sent across the channel to the long-running task requesting
/// that the supplied set of sessions be retried.
struct DecryptionRetryRequest<D: Decryptor> {
    decryptor: D,
    session_ids: Option<BTreeSet<String>>,
    settings: TimelineSettings,
}

/// Long-running task that waits for decryption requests to come through the
/// supplied channel `receiver` and act on them. Stops when the channel is
/// closed, i.e. when the sender side is dropped.
async fn decryption_task<D: Decryptor>(
    state: Arc<RwLock<TimelineState>>,
    room_data_provider: impl RoomDataProvider,
    mut receiver: Receiver<DecryptionRetryRequest<D>>,
) {
    debug!("Decryption task starting.");

    while let Some(request) = receiver.recv().await {
        let should_retry = |session_id: &str| {
            if let Some(session_ids) = &request.session_ids {
                session_ids.contains(session_id)
            } else {
                true
            }
        };

        // Find the indices of events that are in the supplied sessions, distinguishing
        // between UTDs which we need to decrypt, and already-decrypted events where we
        // only need to re-fetch encryption info.
        let mut state = state.write().await;
        let (retry_decryption_indices, retry_info_indices) =
            compute_event_indices_to_retry_decryption(&state.items, should_retry);

        // Retry fetching encryption info for events that are already decrypted
        if !retry_info_indices.is_empty() {
            debug!("Retrying fetching encryption info");
            retry_fetch_encryption_info(&mut state, retry_info_indices, &room_data_provider).await;
        }

        // Retry decrypting any unable-to-decrypt messages
        if !retry_decryption_indices.is_empty() {
            debug!("Retrying decryption");
            decrypt_by_index(
                &mut state,
                &request.settings,
                &room_data_provider,
                request.decryptor,
                should_retry,
                retry_decryption_indices,
            )
            .await
        }
    }

    debug!("Decryption task stopping.");
}

/// Decide which events should be retried, either for re-decryption, or, if they
/// are already decrypted, for re-checking their encryption info.
///
/// Returns a tuple `(retry_decryption_indices, retry_info_indices)` where
/// `retry_decryption_indices` is a list of the indices of UTDs to try
/// decrypting, and retry_info_indices is a list of the indices of
/// already-decrypted events whose encryption info we can re-fetch.
fn compute_event_indices_to_retry_decryption(
    items: &Vector<Arc<TimelineItem>>,
    should_retry: impl Fn(&str) -> bool,
) -> (Vec<usize>, Vec<usize>) {
    use Either::{Left, Right};

    // We retry an event if its session ID should be retried
    let should_retry_event = |event: &EventTimelineItem| {
        let session_id = if let Some(encrypted_message) = event.content().as_unable_to_decrypt() {
            // UTDs carry their session ID inside the content
            encrypted_message.session_id()
        } else {
            // Non-UTDs only have a session ID if they are remote and have it in the
            // EncryptionInfo
            event.as_remote().and_then(|remote| remote.encryption_info.as_ref()?.session_id())
        };

        if let Some(session_id) = session_id {
            // Should we retry this session ID?
            should_retry(session_id)
        } else {
            // No session ID: don't retry this event
            false
        }
    };

    items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            item.as_event().filter(|e| should_retry_event(e)).map(|event| (idx, event))
        })
        // Break the result into 2 lists: (utds, decrypted)
        .partition_map(
            |(idx, event)| {
                if event.content().is_unable_to_decrypt() {
                    Left(idx)
                } else {
                    Right(idx)
                }
            },
        )
}

/// Try to fetch [`EncryptionInfo`] for the events with the supplied
/// indices, and update them where we succeed.
pub(super) async fn retry_fetch_encryption_info<P: RoomDataProvider>(
    state: &mut TimelineState,
    retry_indices: Vec<usize>,
    room_data_provider: &P,
) {
    for idx in retry_indices {
        let old_item = state.items.get(idx);
        if let Some(new_item) = make_replacement_for(room_data_provider, old_item).await {
            state.items.replace(idx, new_item);
        }
    }
}

/// Create a replacement TimelineItem for the supplied one, with new
/// [`EncryptionInfo`] from the supplied `room_data_provider`. Returns None if
/// the supplied item is not a remote event, or if it doesn't have a session ID.
async fn make_replacement_for<P: RoomDataProvider>(
    room_data_provider: &P,
    item: Option<&Arc<TimelineItem>>,
) -> Option<Arc<TimelineItem>> {
    let item = item?;
    let event = item.as_event()?;
    let remote = event.as_remote()?;
    let session_id = remote.encryption_info.as_ref()?.session_id()?;

    let new_encryption_info =
        room_data_provider.get_encryption_info(session_id, &event.sender).await;
    let mut new_remote = remote.clone();
    new_remote.encryption_info = new_encryption_info;
    let new_item = item.with_kind(TimelineItemKind::Event(
        event.with_kind(EventTimelineItemKind::Remote(new_remote)),
    ));

    Some(new_item)
}

/// Attempt decryption of the events encrypted with the session IDs in the
/// supplied decryption `request`.
async fn decrypt_by_index<D: Decryptor>(
    state: &mut TimelineState,
    settings: &TimelineSettings,
    room_data_provider: &impl RoomDataProvider,
    decryptor: D,
    should_retry: impl Fn(&str) -> bool,
    retry_indices: Vec<usize>,
) {
    let push_ctx = room_data_provider.push_context().await;
    let push_ctx = push_ctx.as_ref();
    let unable_to_decrypt_hook = state.meta.unable_to_decrypt_hook.clone();

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

            tracing::Span::current().record("event_id", field::debug(&remote_event.event_id));

            let Some(original_json) = &remote_event.original_json else {
                error!("UTD item must contain original JSON");
                return None;
            };

            match decryptor.decrypt_event_impl(original_json, push_ctx).await {
                Ok(event) => {
                    if let SdkTimelineEventKind::UnableToDecrypt { utd_info, .. } = event.kind {
                        info!(
                            "Failed to decrypt event after receiving room key: {:?}",
                            utd_info.reason
                        );
                        None
                    } else {
                        // Notify observers that we managed to eventually decrypt an event.
                        if let Some(hook) = unable_to_decrypt_hook {
                            hook.on_late_decrypt(&remote_event.event_id).await;
                        }

                        Some(event)
                    }
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

    state.retry_event_decryption(retry_one, retry_indices, room_data_provider, settings).await;
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc, time::SystemTime};

    use imbl::vector;
    use matrix_sdk::{
        crypto::types::events::UtdCause,
        deserialized_responses::{AlgorithmInfo, EncryptionInfo, VerificationState},
    };
    use ruma::{
        events::room::{
            encrypted::{
                EncryptedEventScheme, MegolmV1AesSha2Content, MegolmV1AesSha2ContentInit,
                RoomEncryptedEventContent,
            },
            message::RoomMessageEventContent,
        },
        owned_device_id, owned_event_id, owned_user_id, MilliSecondsSinceUnixEpoch,
        OwnedTransactionId,
    };

    use crate::timeline::{
        controller::decryption_retry_task::compute_event_indices_to_retry_decryption,
        event_item::{
            EventTimelineItemKind, LocalEventTimelineItem, RemoteEventOrigin,
            RemoteEventTimelineItem,
        },
        EncryptedMessage, EventSendState, EventTimelineItem, MsgLikeContent,
        ReactionsByKeyBySender, TimelineDetails, TimelineItem, TimelineItemContent,
        TimelineItemKind, TimelineUniqueId, VirtualTimelineItem,
    };

    #[test]
    fn test_non_events_are_not_retried() {
        // Given a timeline with only non-events
        let timeline = vector![TimelineItem::read_marker(), date_divider()];
        // When we ask what to retry
        let answer = compute_event_indices_to_retry_decryption(&timeline, always_retry);
        // Then we retry nothing
        assert!(answer.0.is_empty());
        assert!(answer.1.is_empty());
    }

    #[test]
    fn test_non_remote_events_are_not_retried() {
        // Given a timeline with only local events
        let timeline = vector![local_event()];
        // When we ask what to retry
        let answer = compute_event_indices_to_retry_decryption(&timeline, always_retry);
        // Then we retry nothing
        assert!(answer.0.is_empty());
        assert!(answer.1.is_empty());
    }

    #[test]
    fn test_utds_are_retried() {
        // Given a timeline with a UTD
        let timeline = vector![utd_event("session1")];
        // When we ask what to retry
        let answer = compute_event_indices_to_retry_decryption(&timeline, always_retry);
        // Then we retry decrypting it, and don't refetch any encryption info
        assert_eq!(answer.0, vec![0]);
        assert!(answer.1.is_empty());
    }

    #[test]
    fn test_remote_decrypted_info_is_refetched() {
        // Given a timeline with a decrypted event
        let timeline = vector![decrypted_event("session1")];
        // When we ask what to retry
        let answer = compute_event_indices_to_retry_decryption(&timeline, always_retry);
        // Then we don't need to decrypt anything, but we do refetch the encryption info
        assert!(answer.0.is_empty());
        assert_eq!(answer.1, vec![0]);
    }

    #[test]
    fn test_only_required_sessions_are_retried() {
        // Given we want to retry everything in session1 only

        fn retry(s: &str) -> bool {
            s == "session1"
        }

        // And we have a timeline containing non-events, local events, UTDs and
        // decrypted events
        let timeline = vector![
            TimelineItem::read_marker(),
            utd_event("session1"),
            utd_event("session1"),
            date_divider(),
            utd_event("session2"),
            decrypted_event("session1"),
            decrypted_event("session1"),
            decrypted_event("session2"),
            local_event(),
        ];

        // When we ask what to retry
        let answer = compute_event_indices_to_retry_decryption(&timeline, retry);

        // Then we re-decrypt the UTDs, and refetch the decrypted events' info
        assert_eq!(answer.0, vec![1, 2]);
        assert_eq!(answer.1, vec![5, 6]);
    }

    fn always_retry(_: &str) -> bool {
        true
    }

    fn date_divider() -> Arc<TimelineItem> {
        TimelineItem::new(
            TimelineItemKind::Virtual(VirtualTimelineItem::DateDivider(timestamp())),
            TimelineUniqueId("datething".to_owned()),
        )
    }

    fn local_event() -> Arc<TimelineItem> {
        let event_kind = EventTimelineItemKind::Local(LocalEventTimelineItem {
            send_state: EventSendState::NotSentYet,
            transaction_id: OwnedTransactionId::from("trans"),
            send_handle: None,
        });

        TimelineItem::new(
            TimelineItemKind::Event(EventTimelineItem::new(
                owned_user_id!("@u:s.to"),
                TimelineDetails::Pending,
                timestamp(),
                TimelineItemContent::MsgLike(MsgLikeContent::redacted()),
                event_kind,
                true,
            )),
            TimelineUniqueId("local".to_owned()),
        )
    }

    fn utd_event(session_id: &str) -> Arc<TimelineItem> {
        let event_kind = EventTimelineItemKind::Remote(RemoteEventTimelineItem {
            event_id: owned_event_id!("$local"),
            transaction_id: None,
            read_receipts: Default::default(),
            is_own: false,
            is_highlighted: false,
            encryption_info: None,
            original_json: None,
            latest_edit_json: None,
            origin: RemoteEventOrigin::Sync,
        });

        TimelineItem::new(
            TimelineItemKind::Event(EventTimelineItem::new(
                owned_user_id!("@u:s.to"),
                TimelineDetails::Pending,
                timestamp(),
                TimelineItemContent::MsgLike(MsgLikeContent::unable_to_decrypt(
                    EncryptedMessage::from_content(
                        RoomEncryptedEventContent::new(
                            EncryptedEventScheme::MegolmV1AesSha2(MegolmV1AesSha2Content::from(
                                MegolmV1AesSha2ContentInit {
                                    ciphertext: "cyf".to_owned(),
                                    sender_key: "sendk".to_owned(),
                                    device_id: owned_device_id!("DEV"),
                                    session_id: session_id.to_owned(),
                                },
                            )),
                            None,
                        ),
                        UtdCause::Unknown,
                    ),
                )),
                event_kind,
                true,
            )),
            TimelineUniqueId("local".to_owned()),
        )
    }

    fn decrypted_event(session_id: &str) -> Arc<TimelineItem> {
        let event_kind = EventTimelineItemKind::Remote(RemoteEventTimelineItem {
            event_id: owned_event_id!("$local"),
            transaction_id: None,
            read_receipts: Default::default(),
            is_own: false,
            is_highlighted: false,
            encryption_info: Some(Arc::new(EncryptionInfo {
                sender: owned_user_id!("@u:s.co"),
                sender_device: None,
                algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
                    curve25519_key: "".to_owned(),
                    sender_claimed_keys: BTreeMap::new(),
                    session_id: Some(session_id.to_owned()),
                },
                verification_state: VerificationState::Verified,
            })),
            original_json: None,
            latest_edit_json: None,
            origin: RemoteEventOrigin::Sync,
        });

        let content = RoomMessageEventContent::text_plain("hi");

        TimelineItem::new(
            TimelineItemKind::Event(EventTimelineItem::new(
                owned_user_id!("@u:s.to"),
                TimelineDetails::Pending,
                timestamp(),
                TimelineItemContent::message(
                    content.msgtype,
                    content.mentions,
                    ReactionsByKeyBySender::default(),
                    None,
                    None,
                    None,
                ),
                event_kind,
                true,
            )),
            TimelineUniqueId("local".to_owned()),
        )
    }

    fn timestamp() -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch::from_system_time(SystemTime::UNIX_EPOCH).unwrap()
    }
}
