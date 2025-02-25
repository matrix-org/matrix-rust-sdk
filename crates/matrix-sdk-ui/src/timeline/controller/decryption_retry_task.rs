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
    traits::{Decryptor, RoomDataProvider},
    EncryptedMessage, TimelineItem,
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
        settings: TimelineSettings,
        room_data_provider: P,
    ) -> Self {
        // We will send decryption requests down this channel to the long-running task
        let (sender, receiver) = mpsc::channel(CHANNEL_BUFFER_SIZE);

        // Spawn the long-running task, providing the receiver so we can listen for
        // decryption requests
        let handle = matrix_sdk::executor::spawn(decryption_task(
            state.clone(),
            settings,
            room_data_provider,
            receiver,
        ));

        // Keep hold of the sender so we can send off decryption requests to the task.
        Self { sender, _task_handle: Arc::new(handle) }
    }

    /// Use the supplied decryptor to attempt redecryption of the events
    /// associated with the supplied session IDs.
    pub(crate) async fn decrypt(&self, decryptor: D, session_ids: Option<BTreeSet<String>>) {
        let res = self.sender.send(DecryptionRetryRequest { decryptor, session_ids }).await;

        if let Err(error) = res {
            error!("Failed to send decryption retry request: {}", error);
        }
    }
}

/// The information sent across the channel to the long-running task requesting
/// that the supplied set of sessions be retried.
struct DecryptionRetryRequest<D: Decryptor> {
    decryptor: D,
    session_ids: Option<BTreeSet<String>>,
}

/// Long-running task that waits for decryption requests to come through the
/// supplied channel `receiver` and act on them. Stops when the channel is
/// closed, i.e. when the sender side is dropped.
async fn decryption_task<D: Decryptor>(
    state: Arc<RwLock<TimelineState>>,
    settings: TimelineSettings,
    room_data_provider: impl RoomDataProvider,
    mut receiver: Receiver<DecryptionRetryRequest<D>>,
) {
    debug!("Decryption task starting.");
    while let Some(request) = receiver.recv().await {
        let retry_indices = retry_indices(state.clone(), &request.session_ids).await;
        if !retry_indices.is_empty() {
            debug!("Retrying decryption");
            decrypt_by_index(
                state.clone(),
                settings.clone(),
                room_data_provider.clone(),
                request.decryptor,
                &request.session_ids,
                retry_indices,
            )
            .await
        }
    }
    debug!("Decryption task stopping.");
}

async fn retry_indices(
    state: Arc<RwLock<TimelineState>>,
    session_ids: &Option<BTreeSet<String>>,
) -> Vec<usize> {
    let state = state.read_owned().await;

    let should_retry = |session_id: &str| {
        if let Some(session_ids) = &session_ids {
            session_ids.contains(session_id)
        } else {
            true
        }
    };

    state
        .items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| match item.as_event()?.content().as_unable_to_decrypt()? {
            EncryptedMessage::MegolmV1AesSha2 { session_id, .. } if should_retry(session_id) => {
                Some(idx)
            }
            EncryptedMessage::MegolmV1AesSha2 { .. }
            | EncryptedMessage::OlmV1Curve25519AesSha2 { .. }
            | EncryptedMessage::Unknown => None,
        })
        .collect()
}

/// Attempt decryption of the events encrypted with the session IDs in the
/// supplied decryption `request`.
async fn decrypt_by_index<D: Decryptor>(
    state: Arc<RwLock<TimelineState>>,
    settings: TimelineSettings,
    room_data_provider: impl RoomDataProvider,
    decryptor: D,
    session_ids: &Option<BTreeSet<String>>,
    retry_indices: Vec<usize>,
) {
    let should_retry = move |session_id: &str| {
        if let Some(session_ids) = &session_ids {
            session_ids.contains(session_id)
        } else {
            true
        }
    };

    let mut state = state.clone().write_owned().await;

    let push_rules_context = room_data_provider.push_rules_and_context().await;
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

            match decryptor.decrypt_event_impl(original_json).await {
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

    state
        .retry_event_decryption(
            retry_one,
            retry_indices,
            push_rules_context,
            &room_data_provider,
            &settings,
        )
        .await;
}
