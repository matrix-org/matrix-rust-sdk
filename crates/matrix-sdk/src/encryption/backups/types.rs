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

use matrix_sdk_base::crypto::{store::RoomKeyCounts, RoomKeyImportResult};
use tokio::sync::broadcast;

use crate::utils::ChannelObservable;
#[cfg(doc)]
use crate::{
    encryption::{backups::Backups, secret_storage::SecretStore},
    Client,
};

/// The states the upload task can be in.
///
/// You can listen on the state of the upload task using the
/// [`Backups::wait_for_steady_state()`] method.
///
/// [`Backups::wait_for_steady_state()`]: crate::encryption::backups::Backups::wait_for_steady_state
#[derive(Clone, Debug)]
pub enum UploadState {
    /// The task is iddle, waiting for new room keys to arrive to try to upload
    /// them.
    Idle,
    /// The task has awoken and is checking if new room keys need to be
    /// uploaded.
    CheckingIfUploadNeeded(RoomKeyCounts),
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
}

const DEFAULT_BACKUP_UPLOAD_DELAY: Duration = Duration::from_millis(100);

impl Default for BackupClientState {
    fn default() -> Self {
        Self {
            upload_delay: RwLock::new(DEFAULT_BACKUP_UPLOAD_DELAY).into(),
            upload_progress: ChannelObservable::new(UploadState::Idle),
            global_state: Default::default(),
            room_keys_broadcaster: broadcast::Sender::new(100),
        }
    }
}

/// The states the backup support for the current [`Client`] can be in.
///
/// Backups can be either enabled automatically if we receive a valid backup
/// recovery key[[1]] or the [`Client`] can create a new backup themselves.
///
/// The [`Client`] can also delete and disable a currently active backup.
///
/// Backups will be enabled automatically if we receive the backup recovery key
/// either from:
///
/// * Another device using `m.secret.send`[[2]], this usually happens after
///   interactive
/// verification has been done.
/// * Secret storage[[3]], this can be done with the
///   [`SecretStore::import_secrets()`] method.
///
/// [1]: https://spec.matrix.org/v1.8/client-server-api/#recovery-key
/// [2]: https://spec.matrix.org/v1.8/client-server-api/#sharing
/// [3]: https://spec.matrix.org/v1.8/client-server-api/#secret-storage
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackupState {
    /// Backups are not active locally and we don't know if they exist on the
    /// server.
    ///
    /// The reason why we don't know if a backup might exist is that we don't
    /// get notified by the server about the creation or deletion of
    /// backups. If we want to know the current state of things we need to poll
    /// the server, this can be done with the
    /// [`Backups::exists_on_server()`] method.
    #[default]
    Unknown,
    /// A new backup is being created by this [`Client`], this state will be
    /// entered if you call the [`Backups::create()`] method.
    Creating,
    /// An existing backup is being enabled to be used by this [`Client`]. We
    /// will enter this state if we have received a backup recovery key.
    Enabling,
    /// An existing backup will be enabled to be used by this [`Client`] after
    /// the client has been restored. This state happens every time a
    /// [`Client`] is restored and we previously have enabled a backup.
    Resuming,
    /// Backups are enabled and room keys are actively being backed up.
    Enabled,
    /// Room keys are currently being downloaded. This state will only happen
    /// after a `Enabling` state. The [`Client`] will attempt to download
    /// all room keys from the backup before transitioning into the
    /// `Enabled` state.
    Downloading,
    /// The backups are being disabled and deleted from the server. This state
    /// will happen when you call the [`Backups::disable()`] method, after
    /// backups have been disabled we're going to transition into the
    /// `Unknown` state.
    Disabling,
}
