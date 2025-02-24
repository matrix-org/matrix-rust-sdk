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

use matrix_sdk::deserialized_responses::TimelineEventKind as SdkTimelineEventKind;
use tokio::sync::RwLock;
use tracing::{debug, error, field, info, info_span, Instrument as _};

use crate::timeline::{
    controller::{TimelineSettings, TimelineState},
    traits::{Decryptor, RoomDataProvider},
    EncryptedMessage, TimelineItem,
};

/// A long-running task spawned and owned by the TimelineController, and used to
/// retry decryption of items in the timeline when new information about a
/// session is received.
pub struct DecryptionRetryTask<P: RoomDataProvider> {
    state: Arc<RwLock<TimelineState>>,
    settings: TimelineSettings,
    room_data_provider: P,
}

impl<P: RoomDataProvider> DecryptionRetryTask<P> {
    pub(crate) fn new(
        state: Arc<RwLock<TimelineState>>,
        settings: TimelineSettings,
        room_data_provider: P,
    ) -> Self {
        Self { state, settings, room_data_provider }
    }

    pub(crate) async fn decrypt(
        &self,
        decryptor: impl Decryptor,
        session_ids: Option<BTreeSet<String>>,
    ) {
        let state = self.state.clone().read_owned().await;

        let should_retry = |session_id: &str| {
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

        drop(state);

        debug!("Retrying decryption");

        self.decrypt_by_index(decryptor, session_ids, retry_indices).await;
    }

    async fn decrypt_by_index(
        &self,
        decryptor: impl Decryptor,
        session_ids: Option<BTreeSet<String>>,
        retry_indices: Vec<usize>,
    ) {
        let should_retry = move |session_id: &str| {
            if let Some(session_ids) = &session_ids {
                session_ids.contains(session_id)
            } else {
                true
            }
        };

        let mut state = self.state.clone().write_owned().await;

        let settings = self.settings.clone();
        let room_data_provider = self.room_data_provider.clone();
        let push_rules_context = self.room_data_provider.push_rules_and_context().await;
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

                    tracing::Span::current()
                        .record("event_id", field::debug(&remote_event.event_id));

                    let Some(original_json) = &remote_event.original_json else {
                        error!("UTD item must contain original JSON");
                        return None;
                    };

                    match decryptor.decrypt_event_impl(original_json).await {
                        Ok(event) => {
                            if let SdkTimelineEventKind::UnableToDecrypt { utd_info, .. } =
                                event.kind
                            {
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
}
