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

use futures_util::pin_mut;
use imbl::Vector;
use itertools::{Either, Itertools as _};
use matrix_sdk::{
    event_cache::RedecryptorReport,
    executor::{JoinHandle, spawn},
};
use tokio_stream::StreamExt as _;

use crate::timeline::{TimelineController, TimelineItem};

/// All the drop handles for the tasks used for crypto, namely message
/// re-decryption, in the timeline.
#[derive(Debug)]
pub(in crate::timeline) struct CryptoDropHandles {
    redecryption_report_join_handle: JoinHandle<()>,
    encryption_changes_handle: JoinHandle<()>,
}

impl Drop for CryptoDropHandles {
    fn drop(&mut self) {
        self.redecryption_report_join_handle.abort();
        self.encryption_changes_handle.abort();
    }
}

/// Decide which events should be retried, either for re-decryption, or, if they
/// are already decrypted, for re-checking their encryption info.
///
/// Returns two sets of session IDs, one for the UTDs and one for the events
/// that have an encryption info that might need to be updated.
pub(super) fn compute_redecryption_candidates(
    timeline_items: &Vector<Arc<TimelineItem>>,
) -> (BTreeSet<String>, BTreeSet<String>) {
    timeline_items
        .iter()
        .filter_map(|event| {
            event.as_event().and_then(|e| {
                let session_id = e.encryption_info().and_then(|info| info.session_id());

                let session_id = if let Some(session_id) = session_id {
                    Some(session_id)
                } else {
                    event.as_event().and_then(|e| {
                        e.content.as_unable_to_decrypt().and_then(|utd| utd.session_id())
                    })
                };

                session_id.map(|id| id.to_owned()).zip(Some(e))
            })
        })
        .partition_map(|(session_id, event)| {
            if event.content.is_unable_to_decrypt() {
                Either::Left(session_id)
            } else {
                Either::Right(session_id)
            }
        })
}

async fn redecryption_report_task(timeline_controller: TimelineController) {
    let client = timeline_controller.room().client();
    let stream = client.event_cache().subscribe_to_decryption_reports();

    pin_mut!(stream);

    while let Some(report) = stream.next().await {
        match report {
            Ok(RedecryptorReport::ResolvedUtds { events, .. }) => {
                let state = timeline_controller.state.read().await;

                if let Some(utd_hook) = &state.meta.unable_to_decrypt_hook {
                    for event_id in events {
                        utd_hook.on_late_decrypt(&event_id).await;
                    }
                }
            }
            Ok(RedecryptorReport::Lagging | RedecryptorReport::BackupAvailable) | Err(_) => {
                // Since the event cache keeps all the events we are keeping
                // cached in the timeline in memory as well,
                // R2D2 will handle the redecryption of these events when any of
                // those reports come in.
            }
        }
    }
}

/// Spawn all the crypto-related tasks that are used to handle re-decryption of
/// messages.
pub(in crate::timeline) async fn spawn_crypto_tasks(
    controller: TimelineController,
) -> CryptoDropHandles {
    let redecryption_report_join_handle = spawn(redecryption_report_task(controller.clone()));

    CryptoDropHandles {
        redecryption_report_join_handle,
        encryption_changes_handle: spawn(async move {
            controller.handle_encryption_state_changes().await
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc, time::SystemTime};

    use imbl::vector;
    use matrix_sdk::deserialized_responses::{AlgorithmInfo, EncryptionInfo, VerificationState};
    use matrix_sdk_base::crypto::types::events::UtdCause;
    use ruma::{
        MilliSecondsSinceUnixEpoch, OwnedTransactionId,
        events::room::{
            encrypted::{
                EncryptedEventScheme, MegolmV1AesSha2Content, MegolmV1AesSha2ContentInit,
                RoomEncryptedEventContent,
            },
            message::RoomMessageEventContent,
        },
        owned_device_id, owned_event_id, owned_user_id,
    };

    use crate::timeline::{
        EncryptedMessage, EventSendState, EventTimelineItem, MsgLikeContent,
        ReactionsByKeyBySender, TimelineDetails, TimelineItem, TimelineItemContent,
        TimelineItemKind, TimelineUniqueId, VirtualTimelineItem,
        controller::decryption_retry_task::compute_redecryption_candidates,
        event_item::{
            EventTimelineItemKind, LocalEventTimelineItem, RemoteEventOrigin,
            RemoteEventTimelineItem,
        },
    };

    #[test]
    fn test_non_events_are_not_retried() {
        // Given a timeline with only non-events
        let timeline = vector![TimelineItem::read_marker(), date_divider()];
        // When we ask what to retry
        let answer = compute_redecryption_candidates(&timeline);
        // Then we retry nothing
        assert!(answer.0.is_empty());
        assert!(answer.1.is_empty());
    }

    #[test]
    fn test_non_remote_events_are_not_retried() {
        // Given a timeline with only local events
        let timeline = vector![local_event()];
        // When we ask what to retry
        let answer = compute_redecryption_candidates(&timeline);
        // Then we retry nothing
        assert!(answer.0.is_empty());
        assert!(answer.1.is_empty());
    }

    #[test]
    fn test_utds_are_retried() {
        // Given a timeline with a UTD
        let timeline = vector![utd_event("session1")];
        // When we ask what to retry
        let answer = compute_redecryption_candidates(&timeline);
        // Then we retry decrypting it, and don't refetch any encryption info
        assert_eq!(answer.0.first().map(|s| s.as_str()), Some("session1"));
        assert!(answer.1.is_empty());
    }

    #[test]
    fn test_remote_decrypted_info_is_refetched() {
        // Given a timeline with a decrypted event
        let timeline = vector![decrypted_event("session1")];
        // When we ask what to retry
        let answer = compute_redecryption_candidates(&timeline);
        // Then we don't need to decrypt anything, but we do refetch the encryption info
        assert!(answer.0.is_empty());
        assert_eq!(answer.1.first().map(|s| s.as_str()), Some("session1"));
    }

    #[test]
    fn test_only_required_sessions_are_retried() {
        // Given we want to retry everything in session1 only

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
        let answer = compute_redecryption_candidates(&timeline);

        // Then we re-decrypt the UTDs, and refetch the decrypted events' info
        assert!(answer.0.contains("session1"));
        assert!(answer.0.contains("session2"));
        assert!(answer.1.contains("session1"));
        assert!(answer.1.contains("session2"));
    }

    fn date_divider() -> Arc<TimelineItem> {
        TimelineItem::new(
            TimelineItemKind::Virtual(VirtualTimelineItem::DateDivider(timestamp())),
            TimelineUniqueId("datething".to_owned()),
        )
    }

    fn local_event() -> Arc<TimelineItem> {
        let event_kind = EventTimelineItemKind::Local(LocalEventTimelineItem {
            send_state: EventSendState::NotSentYet { progress: None },
            transaction_id: OwnedTransactionId::from("trans"),
            send_handle: None,
        });

        TimelineItem::new(
            TimelineItemKind::Event(EventTimelineItem::new(
                owned_user_id!("@u:s.to"),
                TimelineDetails::Pending,
                None,
                None,
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
                None,
                None,
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
                forwarder: None,
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
                None,
                None,
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
