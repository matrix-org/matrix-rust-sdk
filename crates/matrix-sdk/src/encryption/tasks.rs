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

use futures_core::Stream;
use futures_util::{StreamExt, pin_mut};
#[cfg(feature = "experimental-encrypted-state-events")]
use matrix_sdk_base::crypto::types::events::room::encrypted::{
    EncryptedEvent, RoomEventEncryptionScheme,
};
use matrix_sdk_base::{
    InviteAcceptanceDetails, RoomState, crypto::store::types::RoomKeyBundleInfo,
};
use matrix_sdk_common::failures_cache::FailuresCache;
#[cfg(not(feature = "experimental-encrypted-state-events"))]
use ruma::events::room::encrypted::{EncryptedEventScheme, OriginalSyncRoomEncryptedEvent};
#[cfg(feature = "experimental-encrypted-state-events")]
use ruma::serde::JsonCastable;
use ruma::{OwnedEventId, OwnedRoomId, serde::Raw};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, instrument, trace, warn};

use crate::{
    Client, Room,
    client::WeakClient,
    encryption::backups::UploadState,
    executor::{JoinHandle, spawn},
    room::shared_room_history,
};

/// A cache of room keys we already downloaded.
type DownloadCache = FailuresCache<RoomKeyInfo>;

#[derive(Default)]
pub(crate) struct ClientTasks {
    pub(crate) upload_room_keys: Option<BackupUploadingTask>,
    pub(crate) download_room_keys: Option<BackupDownloadTask>,
    pub(crate) update_recovery_state_after_backup: Option<JoinHandle<()>>,
    pub(crate) receive_historic_room_key_bundles: Option<BundleReceiverTask>,
    pub(crate) setup_e2ee: Option<JoinHandle<()>>,
}

pub(crate) struct BackupUploadingTask {
    sender: mpsc::UnboundedSender<()>,
    #[allow(dead_code)]
    join_handle: JoinHandle<()>,
}

impl Drop for BackupUploadingTask {
    fn drop(&mut self) {
        #[cfg(not(target_family = "wasm"))]
        self.join_handle.abort();
    }
}

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

    pub(crate) async fn listen(client: WeakClient, mut receiver: mpsc::UnboundedReceiver<()>) {
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
    #[cfg(not(feature = "experimental-encrypted-state-events"))]
    event: Raw<OriginalSyncRoomEncryptedEvent>,

    /// The event we could not decrypt.
    #[cfg(feature = "experimental-encrypted-state-events")]
    event: Raw<EncryptedEvent>,

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

impl Drop for BackupDownloadTask {
    fn drop(&mut self) {
        #[cfg(not(target_family = "wasm"))]
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
    #[cfg(not(feature = "experimental-encrypted-state-events"))]
    pub(crate) fn trigger_download_for_utd_event(
        &self,
        room_id: OwnedRoomId,
        event: Raw<OriginalSyncRoomEncryptedEvent>,
    ) {
        if let Ok(deserialized_event) = event.deserialize()
            && let EncryptedEventScheme::MegolmV1AesSha2(c) = deserialized_event.content.scheme
        {
            let _ = self.sender.send(RoomKeyDownloadRequest {
                room_id,
                event_id: deserialized_event.event_id,
                event,
                megolm_session_id: c.session_id,
            });
        }
    }

    /// Trigger a backup download for the keys for the given event.
    ///
    /// Does nothing unless the event is encrypted using `m.megolm.v1.aes-sha2`.
    /// Otherwise, tells the listener task to set off a task to do a backup
    /// download, unless there is one already running.
    #[cfg(feature = "experimental-encrypted-state-events")]
    pub(crate) fn trigger_download_for_utd_event<T: JsonCastable<EncryptedEvent>>(
        &self,
        room_id: OwnedRoomId,
        event: Raw<T>,
    ) {
        if let Ok(deserialized_event) = event.deserialize_as::<EncryptedEvent>() {
            if let RoomEventEncryptionScheme::MegolmV1AesSha2(c) = deserialized_event.content.scheme
            {
                let _ = self.sender.send(RoomKeyDownloadRequest {
                    room_id,
                    event_id: deserialized_event.event_id,
                    event: event.cast(),
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
    async fn listen(
        client: WeakClient,
        mut receiver: mpsc::UnboundedReceiver<RoomKeyDownloadRequest>,
    ) {
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
        crate::sleep::sleep(Duration::from_millis(Self::DOWNLOAD_DELAY_MILLIS)).await;

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
            .is_room_key_available(
                #[cfg(not(feature = "experimental-encrypted-state-events"))]
                download_request.event.cast_ref(),
                #[cfg(feature = "experimental-encrypted-state-events")]
                &download_request.event,
                &download_request.room_id,
            )
            .await
            .unwrap_or(false)
        {
            debug!(
                ?download_request,
                "Not performing backup download because key became available while we were sleeping"
            );
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
        }

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

pub(crate) struct BundleReceiverTask {
    _handle: JoinHandle<()>,
}

impl BundleReceiverTask {
    pub async fn new(client: &Client) -> Self {
        let stream = client.encryption().historic_room_key_stream().await.expect("E2EE tasks should only be initialized once we have logged in and have access to an OlmMachine");
        let weak_client = WeakClient::from_client(client);
        let handle = spawn(Self::listen_task(weak_client, stream));

        Self { _handle: handle }
    }

    async fn listen_task(client: WeakClient, stream: impl Stream<Item = RoomKeyBundleInfo>) {
        pin_mut!(stream);

        // TODO: Listening to this stream is not enough for iOS due to the NSE killing
        // our OlmMachine and thus also this stream. We need to add an event handler
        // that will listen for the bundle event. To be able to add an event handler,
        // we'll have to implement the bundle event in Ruma.
        while let Some(bundle_info) = stream.next().await {
            let Some(client) = client.get() else {
                // The client was dropped while we were waiting on the stream. Let's end the
                // loop, since this means that the application has shut down.
                break;
            };

            let Some(room) = client.get_room(&bundle_info.room_id) else {
                warn!(room_id = %bundle_info.room_id, "Received a historic room key bundle for an unknown room");
                continue;
            };

            Self::handle_bundle(&room, &bundle_info).await;
        }
    }

    #[instrument(skip(room), fields(room_id = %room.room_id()))]
    async fn handle_bundle(room: &Room, bundle_info: &RoomKeyBundleInfo) {
        if Self::should_accept_bundle(room, bundle_info) {
            info!("Accepting a late key bundle.");

            if let Err(e) =
                shared_room_history::maybe_accept_key_bundle(room, &bundle_info.sender).await
            {
                warn!("Couldn't accept a late room key bundle {e:?}");
            }
        } else {
            info!("Refusing to accept a historic room key bundle.");
        }
    }

    fn should_accept_bundle(room: &Room, bundle_info: &RoomKeyBundleInfo) -> bool {
        // We accept historic room key bundles up to one day after we have accepted an
        // invite.
        const DAY: Duration = Duration::from_secs(24 * 60 * 60);

        // If we don't have any invite acceptance details, then this client wasn't the
        // one that accepted the invite.
        let Some(InviteAcceptanceDetails { invite_accepted_at, inviter }) =
            room.invite_acceptance_details()
        else {
            return false;
        };

        let state = room.state();
        let elapsed_since_join = invite_accepted_at.to_system_time().and_then(|t| t.elapsed().ok());
        let bundle_sender = &bundle_info.sender;

        match (state, elapsed_since_join) {
            (RoomState::Joined, Some(elapsed_since_join)) => {
                elapsed_since_join < DAY && bundle_sender == &inviter
            }
            (RoomState::Joined, None) => false,
            (RoomState::Left | RoomState::Invited | RoomState::Knocked | RoomState::Banned, _) => {
                false
            }
        }
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod test {
    use matrix_sdk_test::{
        InvitedRoomBuilder, JoinedRoomBuilder, async_test, event_factory::EventFactory,
    };
    #[cfg(not(feature = "experimental-encrypted-state-events"))]
    use ruma::events::room::encrypted::OriginalSyncRoomEncryptedEvent;
    use ruma::{event_id, room_id, user_id};
    use serde_json::json;
    use vodozemac::Curve25519PublicKey;
    use wiremock::MockServer;

    use super::*;
    use crate::test_utils::{logged_in_client, mocks::MatrixMockServer};

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

        #[cfg(not(feature = "experimental-encrypted-state-events"))]
        let event: Raw<OriginalSyncRoomEncryptedEvent> =
            serde_json::from_value(event_content).expect("");

        #[cfg(feature = "experimental-encrypted-state-events")]
        let event: Raw<EncryptedEvent> = serde_json::from_value(event_content).expect("");

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

    /// Test that ensures that we only accept a bundle if a certain set of
    /// conditions is met.
    #[async_test]
    async fn test_should_accept_bundle() {
        let server = MatrixMockServer::new().await;

        let alice_user_id = user_id!("@alice:localhost");
        let bob_user_id = user_id!("@bob:localhost");
        let joined_room_id = room_id!("!joined:localhost");
        let invited_rom_id = room_id!("!invited:localhost");

        let client = server
            .client_builder()
            .logged_in_with_token("ABCD".to_owned(), alice_user_id.into(), "DEVICEID".into())
            .build()
            .await;

        let event_factory = EventFactory::new().room(invited_rom_id);
        let bob_member_event = event_factory.member(bob_user_id);
        let alice_member_event = event_factory.member(bob_user_id).invited(alice_user_id);

        server
            .mock_sync()
            .ok_and_run(&client, |builder| {
                builder.add_joined_room(JoinedRoomBuilder::new(joined_room_id)).add_invited_room(
                    InvitedRoomBuilder::new(invited_rom_id)
                        .add_state_event(bob_member_event)
                        .add_state_event(alice_member_event),
                );
            })
            .await;

        let room =
            client.get_room(joined_room_id).expect("We should have access to our joined room now");

        assert!(
            room.invite_acceptance_details().is_none(),
            "We shouldn't have any invite acceptance details if we didn't join the room on this Client"
        );

        let bundle_info = RoomKeyBundleInfo {
            sender: bob_user_id.to_owned(),
            sender_key: Curve25519PublicKey::from_bytes([0u8; 32]),
            room_id: joined_room_id.to_owned(),
        };

        assert!(
            !BundleReceiverTask::should_accept_bundle(&room, &bundle_info),
            "We should not accept a bundle if we did not join the room from this Client"
        );

        let invited_room =
            client.get_room(invited_rom_id).expect("We should have access to our invited room now");

        assert!(
            !BundleReceiverTask::should_accept_bundle(&invited_room, &bundle_info),
            "We should not accept a bundle if we didn't join the room."
        );

        server.mock_room_join(invited_rom_id).ok().mock_once().mount().await;

        let room = client
            .join_room_by_id(invited_rom_id)
            .await
            .expect("We should be able to join the invited room");

        let details = room
            .invite_acceptance_details()
            .expect("We should have stored the invite acceptance details");
        assert_eq!(details.inviter, bob_user_id, "We should have recorded that Bob has invited us");

        assert!(
            BundleReceiverTask::should_accept_bundle(&room, &bundle_info),
            "We should accept a bundle if we just joined the room and did so from this very Client object"
        );
    }
}
