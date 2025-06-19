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

use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use matrix_sdk_base::crypto::{RoomKeyImportResult, store::types::RoomKeyCounts};
use tokio::sync::broadcast;

use crate::utils::ChannelObservable;
#[cfg(doc)]
use crate::{
    Client,
    encryption::{backups::Backups, secret_storage::SecretStore},
};

/// The states the upload task can be in.
///
/// You can listen on the state of the upload task using the
/// [`Backups::wait_for_steady_state()`] method.
///
/// [`Backups::wait_for_steady_state()`]: crate::encryption::backups::Backups::wait_for_steady_state
#[derive(Clone, Debug)]
pub enum UploadState {
    /// The task is idle, waiting for new room keys to arrive to try to upload
    /// them.
    Idle,
    /// The task is currently uploading room keys to the homeserver.
    Uploading(RoomKeyCounts),
    /// There was an error while trying to upload room keys, the task will go
    /// back to the `Idle` state and try again later.
    Error,
    /// All room keys have been successfully uploaded, the task will now go back
    /// to the `Idle` state.
    Done,
}

pub(crate) struct BackupClientState {
    pub(super) upload_delay: Arc<RwLock<Duration>>,
    pub(crate) upload_progress: ChannelObservable<UploadState>,
    pub(super) global_state: ChannelObservable<BackupState>,
    pub(super) room_keys_broadcaster: broadcast::Sender<RoomKeyImportResult>,

    /// Whether a key storage backup exists on the server, as far as we know.
    ///
    /// This is `None` if we have not asked the server yet, and `Some`
    /// otherwise. This value is not always up-to-date: if the backup status
    /// on the server was changed by some other client, we will have a old
    /// value.
    pub(super) backup_exists_on_server: RwLock<Option<bool>>,
}

impl BackupClientState {
    /// Update the cached value indicating whether a key storage backup exists
    /// on the server
    pub(crate) fn set_backup_exists_on_server(&self, exists_on_server: bool) {
        *self.backup_exists_on_server.write().unwrap() = Some(exists_on_server);
    }

    /// Ask whether the key storage backup exists on the server. Returns `None`
    /// if we haven't checked. Note that this value will be out-of-date if
    /// some other client changed the state since the last time we checked.
    pub(crate) fn backup_exists_on_server(&self) -> Option<bool> {
        *self.backup_exists_on_server.read().unwrap()
    }

    /// Clear out the cached value indicating whether a key storage backup
    /// exists on the server, meaning that the code in
    /// [`super::Backups`] will repopulate it when needed
    /// with an up-to-date value.
    pub(crate) fn clear_backup_exists_on_server(&self) {
        *self.backup_exists_on_server.write().unwrap() = None;
    }
}

const DEFAULT_BACKUP_UPLOAD_DELAY: Duration = Duration::from_millis(100);

impl Default for BackupClientState {
    fn default() -> Self {
        Self {
            upload_delay: RwLock::new(DEFAULT_BACKUP_UPLOAD_DELAY).into(),
            upload_progress: ChannelObservable::new(UploadState::Idle),
            global_state: Default::default(),
            room_keys_broadcaster: broadcast::Sender::new(100),
            backup_exists_on_server: RwLock::new(None),
        }
    }
}

/// The possible states of the [`Client`]'s room key backup mechanism.
///
/// A local backup instance can be created either by receiving a valid backup
/// recovery key [[1]] or by having the [`Client`] create a new backup itself.
///
/// The [`Client`] can also delete and disable a currently active backup.
///
/// Backups will be enabled automatically if we receive the backup recovery key
/// either from:
///
/// * Another device using `m.secret.send`[[2]], which usually happens after
///   completing interactive verification.
/// * Secret storage[[3]], which is done by calling the
///   [`SecretStore::import_secrets()`] method.
///
/// [1]: https://spec.matrix.org/v1.8/client-server-api/#recovery-key
/// [2]: https://spec.matrix.org/v1.8/client-server-api/#sharing
/// [3]: https://spec.matrix.org/v1.8/client-server-api/#secret-storage
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackupState {
    /// There is no locally active backup and we don't know whether there backup
    /// exists on the server.
    ///
    /// The reason we don't know whether a server-side backup exists is that we
    /// don't get notified by the server about the creation and deletion of
    /// backups. If we want to know the current state, we need to poll the
    /// server, which is done using the [`Backups::fetch_exists_on_server()`]
    /// method.
    #[default]
    Unknown,
    /// A new backup is being created by this [`Client`]. This state will be
    /// entered if you call the [`Backups::create()`] method.
    Creating,
    /// An existing backup is being enabled for use by this [`Client`]. We
    /// will enter this state if we have received a backup recovery key.
    Enabling,
    /// An existing backup will be enabled to be used by this [`Client`] after
    /// the client has been restored. This state happens every time a
    /// [`Client`] is restored after we'd previously enabled a backup.
    Resuming,
    /// The backup is enabled and room keys are actively being backed up.
    Enabled,
    /// Room keys are currently being downloaded. This state will only happen
    /// after an `Enabling` state. The [`Client`] will attempt to download
    /// all room keys from the backup before transitioning into the
    /// `Enabled` state.
    Downloading,
    /// The backup is being disabled and deleted from the server. This state
    /// will happen when you call the [`Backups::disable()`] method. After it
    /// has been disabled, we're going to transition into the `Unknown` state.
    Disabling,
}
