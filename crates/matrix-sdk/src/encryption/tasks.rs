// Copyright 2023-2024 The Matrix.org Foundation C.I.C.
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

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use matrix_sdk_common::failures_cache::FailuresCache;
use ruma::{
    events::room::encrypted::{EncryptedEventScheme, OriginalSyncRoomEncryptedEvent},
    serde::Raw,
    OwnedEventId, OwnedRoomId,
};
use tokio::sync::{
    mpsc::{self, UnboundedReceiver},
    Mutex,
};
use tracing::{debug, trace, warn};

use crate::{
    client::WeakClient,
    encryption::backups::UploadState,
    executor::{spawn, JoinHandle},
    Client,
};

/// A cache of room keys we already downloaded.
type DownloadCache = FailuresCache<RoomKeyInfo>;

#[derive(Default)]
pub(crate) struct ClientTasks {
    #[cfg(feature = "e2e-encryption")]
    pub(crate) upload_room_keys: Option<BackupUploadingTask>,
    #[cfg(feature = "e2e-encryption")]
    pub(crate) download_room_keys: Option<BackupDownloadTask>,
    #[cfg(feature = "e2e-encryption")]
    pub(crate) update_recovery_state_after_backup: Option<JoinHandle<()>>,
    pub(crate) setup_e2ee: Option<JoinHandle<()>>,
}

#[cfg(feature = "e2e-encryption")]
pub(crate) struct BackupUploadingTask {
    sender: mpsc::UnboundedSender<()>,
    #[allow(dead_code)]
    join_handle: JoinHandle<()>,
}

#[cfg(feature = "e2e-encryption")]
impl Drop for BackupUploadingTask {
    fn drop(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        self.join_handle.abort();
    }
}

#[cfg(feature = "e2e-encryption")]
impl BackupUploadingTask {
    pub(crate) fn new(client: WeakClient) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();

        let join_handle = spawn(async move {
            Self::listen(client, receiver).await;
        });

        Self { sender, join_handle }
    }

    pub(crate) fn trigger_upload(&self) {
        let _ = self.sender.send(());
    }

    pub(crate) async fn listen(client: WeakClient, mut receiver: UnboundedReceiver<()>) {
        while receiver.recv().await.is_some() {
            if let Some(client) = client.get() {
                let upload_progress = &client.inner.e2ee.backup_state.upload_progress;

                if let Err(e) = client.encryption().backups().backup_room_keys().await {
                    upload_progress.set(UploadState::Error);
                    warn!("Error backing up room keys {e:?}");
                    // Note: it's expected we're not `continue`ing here, because
                    // *every* single state update
                    // is propagated to the caller.
                }

                upload_progress.set(UploadState::Idle);
            } else {
                trace!("Client got dropped, shutting down the task");
                break;
            }
        }
    }
}

/// Information about a request for a backup download for an undecryptable
/// event.
#[derive(Debug)]
struct RoomKeyDownloadRequest {
    /// The room in which the event was sent.
    room_id: OwnedRoomId,

    /// The ID of the event we could not decrypt.
    event_id: OwnedEventId,

    /// The event we could not decrypt.
    event: Raw<OriginalSyncRoomEncryptedEvent>,

    /// The unique ID of the room key that the event was encrypted with.
    megolm_session_id: String,
}

impl RoomKeyDownloadRequest {
    pub fn to_room_key_info(&self) -> RoomKeyInfo {
        (self.room_id.clone(), self.megolm_session_id.clone())
    }
}

pub type RoomKeyInfo = (OwnedRoomId, String);

pub(crate) struct BackupDownloadTask {
    sender: mpsc::UnboundedSender<RoomKeyDownloadRequest>,
    #[allow(dead_code)]
    join_handle: JoinHandle<()>,
}

#[cfg(feature = "e2e-encryption")]
impl Drop for BackupDownloadTask {
    fn drop(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        self.join_handle.abort();
    }
}

impl BackupDownloadTask {
    #[cfg(not(test))]
    const DOWNLOAD_DELAY_MILLIS: u64 = 100;

    pub(crate) fn new(client: WeakClient) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();

        let join_handle = spawn(async move {
            Self::listen(client, receiver).await;
        });

        Self { sender, join_handle }
    }

    /// Trigger a backup download for the keys for the given event.
    ///
    /// Does nothing unless the event is encrypted using `m.megolm.v1.aes-sha2`.
    /// Otherwise, tells the listener task to set off a task to do a backup
    /// download, unless there is one already running.
    pub(crate) fn trigger_download_for_utd_event(
        &self,
        room_id: OwnedRoomId,
        event: Raw<OriginalSyncRoomEncryptedEvent>,
    ) {
        if let Ok(deserialized_event) = event.deserialize() {
            if let EncryptedEventScheme::MegolmV1AesSha2(c) = deserialized_event.content.scheme {
                let _ = self.sender.send(RoomKeyDownloadRequest {
                    room_id,
                    event_id: deserialized_event.event_id,
                    event,
                    megolm_session_id: c.session_id,
                });
            }
        }
    }

    /// Listen for incoming [`RoomKeyDownloadRequest`]s and process them.
    ///
    /// This will keep running until either the request channel is closed, or
    /// all other references to `Client` are dropped.
    ///
    /// # Arguments
    ///
    /// * `receiver` - The source of incoming [`RoomKeyDownloadRequest`]s.
    async fn listen(client: WeakClient, mut receiver: UnboundedReceiver<RoomKeyDownloadRequest>) {
        let state = Arc::new(Mutex::new(BackupDownloadTaskListenerState::new(client)));

        while let Some(room_key_download_request) = receiver.recv().await {
            let mut state_guard = state.lock().await;

            if state_guard.client.strong_count() == 0 {
                trace!("Client got dropped, shutting down the task");
                break;
            }

            // Check that we don't already have a task to process this event, and fire one
            // off else if not.
            let event_id = &room_key_download_request.event_id;
            if !state_guard.active_tasks.contains_key(event_id) {
                let event_id = event_id.to_owned();
                let task =
                    spawn(Self::handle_download_request(state.clone(), room_key_download_request));
                state_guard.active_tasks.insert(event_id, task);
            }
        }
    }

    /// Handle a request to download a room key for a given event.
    ///
    /// Sleeps for a while to see if the key turns up; then checks if we still
    /// want to do a download, and does the download if so.
    async fn handle_download_request(
        state: Arc<Mutex<BackupDownloadTaskListenerState>>,
        download_request: RoomKeyDownloadRequest,
    ) {
        // Wait a bit, perhaps the room key will arrive in the meantime.
        #[cfg(not(test))]
        tokio::time::sleep(Duration::from_millis(Self::DOWNLOAD_DELAY_MILLIS)).await;

        // Now take the lock, and check that we still want to do a download. If we do,
        // keep hold of a strong reference to the `Client`.
        let client = {
            let mut state = state.lock().await;

            let Some(client) = state.client.get() else {
                // The client was dropped while we were sleeping. We should just bail out;
                // the main BackupDownloadTask loop will bail out too.
                return;
            };

            // Check that we still want to do a download.
            if !state.should_download(&client, &download_request).await {
                // We decided against doing a download. Mark the job done for this event before
                // dropping the lock.
                state.active_tasks.remove(&download_request.event_id);
                return;
            }

            // Before we drop the lock, indicate to other tasks that may be considering this
            // room key, that we're going to go ahead and do a download.
            state.downloaded_room_keys.insert(download_request.to_room_key_info());

            client
        };

        // Do the download without holding the lock.
        let result = client
            .encryption()
            .backups()
            .download_room_key(&download_request.room_id, &download_request.megolm_session_id)
            .await;

        // Then take the lock again to update the state.
        {
            let mut state = state.lock().await;
            let room_key_info = download_request.to_room_key_info();

            match result {
                Ok(true) => {
                    // We successfully downloaded the room key. We can clear any record of previous
                    // backoffs from the failures cache, because we won't be needing them again.
                    state.failures_cache.remove(std::iter::once(&room_key_info))
                }
                Ok(false) => {
                    // We did not find a valid backup decryption key or backup version, we did not
                    // even attempt to download the room key.
                    state.downloaded_room_keys.remove(std::iter::once(&room_key_info));
                }
                Err(_) => {
                    // We were unable to download the room key. Update the failure cache so that we
                    // back off from more requests, and also remove the entry from the list of
                    // room keys that we are downloading.
                    state.downloaded_room_keys.remove(std::iter::once(&room_key_info));
                    state.failures_cache.insert(room_key_info);
                }
            }

            state.active_tasks.remove(&download_request.event_id);
        }
    }
}

/// The state for an active [`BackupDownloadTask`].
struct BackupDownloadTaskListenerState {
    /// Reference to the `Client`, which will be used to fire off the download
    /// requests.
    client: WeakClient,

    /// A record of backup download attempts that have recently failed.
    failures_cache: FailuresCache<RoomKeyInfo>,

    /// Map from event ID to download task
    active_tasks: BTreeMap<OwnedEventId, JoinHandle<()>>,

    /// A list of room keys that we have already downloaded, or are about to
    /// download.
    ///
    /// The idea here is that once we've (successfully) downloaded a room key
    /// from the backup, there's not much point trying again even if we get
    /// another UTD event that uses the same room key.
    downloaded_room_keys: DownloadCache,
}

impl BackupDownloadTaskListenerState {
    /// Prepare a new `BackupDownloadTaskListenerState`.
    ///
    /// # Arguments
    ///
    /// * `client` - A reference to the `Client`, which is used to fire off the
    ///   backup download request.
    pub fn new(client: WeakClient) -> Self {
        Self {
            client,
            failures_cache: FailuresCache::with_settings(Duration::from_secs(60 * 60 * 24), 60),
            active_tasks: Default::default(),
            downloaded_room_keys: DownloadCache::with_settings(
                Duration::from_secs(60 * 60 * 24),
                60,
            ),
        }
    }

    /// Check if we should set off a download for the given request.
    ///
    /// Checks if:
    ///  * we already have the key,
    ///  * we have already downloaded this room key, or are about to do so, or
    ///  * we've backed off from trying to download this room key.
    ///
    /// If any of the above are true, returns `false`. Otherwise, returns
    /// `true`.
    pub async fn should_download(
        &self,
        client: &Client,
        download_request: &RoomKeyDownloadRequest,
    ) -> bool {
        // Check that the Client has an OlmMachine
        let machine_guard = client.olm_machine().await;
        let Some(machine) = machine_guard.as_ref() else {
            return false;
        };

        // If backups aren't enabled, there's no point in trying to download a room key.
        if !client.encryption().backups().are_enabled().await {
            debug!(
                ?download_request,
                "Not performing backup download because backups are not enabled"
            );

            return false;
        }

        // Check if the keys for this message have arrived in the meantime.
        // If we get a StoreError doing the lookup, we assume the keys haven't arrived
        // (though if the store is returning errors, probably something else is
        // going to go wrong very soon).
        if machine
            .is_room_key_available(download_request.event.cast_ref(), &download_request.room_id)
            .await
            .unwrap_or(false)
        {
            debug!(?download_request, "Not performing backup download because key became available while we were sleeping");
            return false;
        }

        // Check if we already downloaded this room key, or another task is in the
        // process of doing so.
        let room_key_info = download_request.to_room_key_info();
        if self.downloaded_room_keys.contains(&room_key_info) {
            debug!(
                ?download_request,
                "Not performing backup download because this room key has already been downloaded recently"
            );
            return false;
        };

        // Check if we're backing off from attempts to download this room key
        if self.failures_cache.contains(&room_key_info) {
            debug!(
                ?download_request,
                "Not performing backup download because this room key failed to download recently"
            );
            return false;
        }

        debug!(?download_request, "Performing backup download");
        true
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod test {
    use matrix_sdk_test::async_test;
    use ruma::{event_id, room_id};
    use serde_json::json;
    use wiremock::MockServer;

    use super::*;
    use crate::test_utils::logged_in_client;

    // Test that, if backups are not enabled, we don't incorrectly mark a room key
    // as downloaded.
    #[async_test]
    async fn test_disabled_backup_does_not_mark_room_key_as_downloaded() {
        let room_id = room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost");
        let event_id = event_id!("$JbFHtZpEJiH8uaajZjPLz0QUZc1xtBR9rPGBOjF6WFM");
        let session_id = "session_id";

        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        let weak_client = WeakClient::from_client(&client);

        let event_content = json!({
            "event_id": event_id,
            "origin_server_ts": 1698579035927u64,
            "sender": "@example2:morpheus.localhost",
            "type": "m.room.encrypted",
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2",
                "ciphertext": "AwgAEpABhetEzzZzyYrxtEVUtlJnZtJcURBlQUQJ9irVeklCTs06LwgTMQj61PMUS4Vy\
                               YOX+PD67+hhU40/8olOww+Ud0m2afjMjC3wFX+4fFfSkoWPVHEmRVucfcdSF1RSB4EmK\
                               PIP4eo1X6x8kCIMewBvxl2sI9j4VNvDvAN7M3zkLJfFLOFHbBviI4FN7hSFHFeM739Zg\
                               iwxEs3hIkUXEiAfrobzaMEM/zY7SDrTdyffZndgJo7CZOVhoV6vuaOhmAy4X2t4UnbuV\
                               JGJjKfV57NAhp8W+9oT7ugwO",
                "device_id": "KIUVQQSDTM",
                "sender_key": "LvryVyoCjdONdBCi2vvoSbI34yTOx7YrCFACUEKoXnc",
                "session_id": "64H7XKokIx0ASkYDHZKlT5zd/Zccz/cQspPNdvnNULA"
            }
        });

        let event: Raw<OriginalSyncRoomEncryptedEvent> =
            serde_json::from_value(event_content).expect("");

        let state = Arc::new(Mutex::new(BackupDownloadTaskListenerState::new(weak_client)));
        let download_request = RoomKeyDownloadRequest {
            room_id: room_id.into(),
            megolm_session_id: session_id.to_owned(),
            event,
            event_id: event_id.into(),
        };

        assert!(
            !client.encryption().backups().are_enabled().await,
            "Backups should not be enabled."
        );

        BackupDownloadTask::handle_download_request(state.clone(), download_request).await;

        {
            let state = state.lock().await;
            assert!(
                !state.downloaded_room_keys.contains(&(room_id.to_owned(), session_id.to_owned())),
                "Backups are not enabled, we should not mark any room keys as downloaded."
            )
        }
    }
}
