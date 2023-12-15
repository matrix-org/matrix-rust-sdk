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

use std::{collections::BTreeMap, sync::Weak, time::Duration};

use futures_util::future::join_all;
use matrix_sdk_common::failures_cache::FailuresCache;
use ruma::OwnedRoomId;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tracing::{trace, warn};

use super::ClientInner;
use crate::{
    encryption::backups::UploadState,
    executor::{spawn, JoinHandle},
    Client,
};

#[derive(Default)]
pub(crate) struct ClientTasks {
    #[cfg(feature = "e2e-encryption")]
    pub(crate) upload_room_keys: Option<BackupUploadingTask>,
    #[cfg(feature = "e2e-encryption")]
    pub(crate) download_room_keys: Option<BackupDownloadTask>,
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
    pub(crate) fn new(client: Weak<ClientInner>) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();

        let join_handle = spawn(async move {
            Self::listen(client, receiver).await;
        });

        Self { sender, join_handle }
    }

    pub(crate) fn trigger_upload(&self) {
        let _ = self.sender.send(());
    }

    pub(crate) async fn listen(client: Weak<ClientInner>, mut receiver: UnboundedReceiver<()>) {
        while receiver.recv().await.is_some() {
            if let Some(client) = client.upgrade() {
                let client = Client { inner: client };

                if let Err(e) = client.encryption().backups().backup_room_keys().await {
                    client.inner.backup_state.upload_progress.set(UploadState::Error);
                    warn!("Error backing up room keys {e:?}");
                }

                client.inner.backup_state.upload_progress.set(UploadState::Idle);
            } else {
                trace!("Client got dropped, shutting down the task");
                break;
            }
        }
    }
}

pub type RoomKeyInfo = (OwnedRoomId, String);
pub type TaskQueue = BTreeMap<RoomKeyInfo, JoinHandle<()>>;

pub(crate) struct BackupDownloadTask {
    sender: mpsc::UnboundedSender<RoomKeyInfo>,
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
    const DOWNLOAD_DELAY_MILLIS: u64 = 100;

    pub(crate) fn new(client: Weak<ClientInner>) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();

        let join_handle = spawn(async move {
            Self::listen(
                client,
                receiver,
                FailuresCache::with_settings(Duration::from_secs(60 * 60 * 24), 60),
            )
            .await;
        });

        Self { sender, join_handle }
    }

    pub(crate) fn trigger_download(&self, room_key_info: RoomKeyInfo) {
        let _ = self.sender.send(room_key_info);
    }

    pub(crate) async fn download(
        client: Client,
        room_key_info: RoomKeyInfo,
        failures_cache: FailuresCache<RoomKeyInfo>,
    ) {
        // Wait a bit, perhaps the room key will arrive in the meantime.
        tokio::time::sleep(Duration::from_millis(Self::DOWNLOAD_DELAY_MILLIS)).await;

        if let Some(machine) = client.olm_machine().await.as_ref() {
            let (room_id, session_id) = &room_key_info;

            if !machine.is_room_key_available(room_id, session_id).await.unwrap() {
                match client.encryption().backups().download_room_key(room_id, session_id).await {
                    Ok(_) => failures_cache.remove(std::iter::once(&room_key_info)),
                    Err(_) => failures_cache.insert(room_key_info),
                }
            }
        }
    }

    pub(crate) async fn prune_tasks(task_queue: &mut TaskQueue) {
        let mut handles = Vec::with_capacity(task_queue.len());

        while let Some((_, handle)) = task_queue.pop_first() {
            handles.push(handle);
        }

        join_all(handles).await;
    }

    pub(crate) async fn listen(
        client: Weak<ClientInner>,
        mut receiver: UnboundedReceiver<RoomKeyInfo>,
        failures_cache: FailuresCache<RoomKeyInfo>,
    ) {
        let mut task_queue = TaskQueue::new();

        while let Some(room_key_info) = receiver.recv().await {
            trace!(?room_key_info, "Got a request to download a room key from the backup");

            if task_queue.len() >= 10 {
                Self::prune_tasks(&mut task_queue).await
            }

            if let Some(client) = client.upgrade() {
                let client = Client { inner: client };
                let backups = client.encryption().backups();

                let already_tried = failures_cache.contains(&room_key_info);
                let task_exists = task_queue.contains_key(&room_key_info);

                if !already_tried && !task_exists && backups.are_enabled().await {
                    let task = spawn(Self::download(
                        client,
                        room_key_info.to_owned(),
                        failures_cache.to_owned(),
                    ));

                    task_queue.insert(room_key_info, task);
                }
            } else {
                trace!("Client got dropped, shutting down the task");
                break;
            }
        }
    }
}
