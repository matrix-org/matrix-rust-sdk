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

use std::sync::Weak;

use tokio::sync::mpsc::{self, UnboundedReceiver};
use tracing::{trace, warn};

use super::ClientInner;
use crate::{
    encryption::backups::UploadState,
    executor::{spawn, JoinHandle},
    Client,
};

pub(crate) struct ClientTasks {
    #[cfg(feature = "e2e-encryption")]
    pub(crate) upload_room_keys: BackupUploadingTask,
}

impl ClientTasks {
    pub(crate) fn new(client: Weak<ClientInner>) -> Self {
        Self {
            #[cfg(feature = "e2e-encryption")]
            upload_room_keys: BackupUploadingTask::new(client),
        }
    }
}

#[cfg(feature = "e2e-encryption")]
pub(crate) struct BackupUploadingTask {
    sender: mpsc::UnboundedSender<()>,
    join_handle: JoinHandle<()>,
}

#[cfg(feature = "e2e-encryption")]
impl Drop for BackupUploadingTask {
    fn drop(&mut self) {
        self.join_handle.abort();
    }
}

#[cfg(feature = "e2e-encryption")]
impl BackupUploadingTask {
    pub fn new(client: Weak<ClientInner>) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();

        let join_handle = spawn(async move {
            Self::listen(client, receiver).await;
        });

        Self { sender, join_handle }
    }

    pub fn trigger_upload(&self) {
        let _ = self.sender.send(());
    }

    pub async fn listen(client: Weak<ClientInner>, mut receiver: UnboundedReceiver<()>) {
        while receiver.recv().await.is_some() {
            if let Some(client) = client.upgrade() {
                let client = Client { inner: client };

                if let Err(e) = client.encryption().backups().backup_room_keys().await {
                    client.inner.backups_state.upload_progress.set(UploadState::Error);
                    warn!("Error backing up room keys {e:?}");
                }

                client.inner.backups_state.upload_progress.set(UploadState::Idle);
            } else {
                trace!("Client got dropped, shutting down the task");
                break;
            }
        }
    }
}
