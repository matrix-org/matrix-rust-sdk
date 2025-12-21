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

use matrix_sdk_base::crypto::store::types::RoomKeyCounts;
use ruma::events::macros::EventContent;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

#[cfg(doc)]
use crate::encryption::{
    backups::Backups,
    recovery::{Recovery, futures::Enable},
};

/// Result type alias for the [`Recovery`] subsystem.
pub type Result<A, E = RecoveryError> = std::result::Result<A, E>;

/// Error type for the [`Recovery`] subsystem.
#[derive(Debug, Error)]
pub enum RecoveryError {
    /// A backup already exists on the homeserver, the recovery subsystem does
    /// not allow backups to be overwritten, disable recovery first.
    #[error(
        "A backup already exists on the homeserver and the method does not allow to overwrite it"
    )]
    BackupExistsOnServer,

    /// A typical SDK error.
    #[error(transparent)]
    Sdk(#[from] crate::Error),

    /// Error in the secret storage subsystem.
    #[error(transparent)]
    SecretStorage(#[from] crate::encryption::secret_storage::SecretStorageError),
}

/// Enum describing the states the [`Recovery::enable()`] method can be in.
#[derive(Debug, Default, Clone, Zeroize, ZeroizeOnDrop)]
pub enum EnableProgress {
    /// The client is just starting the process of enabling recovery, this is
    /// the initial state.
    #[default]
    Starting,
    /// The client is creating a new server-side key backup.
    CreatingBackup,
    /// The client is creating a new recovery key and uploading all the locally
    /// cached secrets to the homeserver.
    CreatingRecoveryKey,
    /// The client is currently backing up room keys to the server-side key
    /// backup. This state may be emitted multiple times until all room keys
    /// have been backed up.
    #[zeroize(skip)]
    BackingUp(RoomKeyCounts),
    /// The client encountered an error while trying to upload all room keys to
    /// the server-side key backup.
    ///
    /// Not all room keys may have been backed up, the client will try to back
    /// them up again at a later point. If you'd like to wait for the backup
    /// to finish again you can use the [`Backups::wait_for_steady_state()`]
    /// method.
    RoomKeyUploadError,
    /// Recovery has been successfully enabled, this is the final state.
    Done {
        /// The newly created recovery key.
        // TODO: Can I remove this from here? It seems a bit dumb.
        recovery_key: String,
    },
}

/// The states the recovery subsystem can be in.
///
/// You can listen on the state of the recovery mechanism using the
/// [`Recovery::state_stream()`] method.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RecoveryState {
    /// We didn't yet inform ourselves about the state of things.
    #[default]
    Unknown,
    /// Secret storage is set up and we have all the secrets locally.
    Enabled,
    /// No default secret storage key exists or it is disabled explicitly using
    /// the account data event.
    Disabled,
    /// Secret storage is set up but we're missing some secrets.
    Incomplete,
}

/// A hack to allow the `m.secret_storage.default_key` event to be "deleted".
///
/// This allows us to set the `m.secret_storage.default_key` event to an empty
/// JSON object, which means that the event will be invalid.
#[derive(Clone, Debug, Default, Deserialize, Serialize, EventContent)]
#[ruma_event(type = "m.secret_storage.default_key", kind = GlobalAccountData)]
pub(super) struct SecretStorageDisabledContent {}

/// A custom global account data event which tells us that a new backup should
/// not be automatically created.
///
/// This event is defined in [MSC4287].
///
/// [MSC4287]: https://github.com/matrix-org/matrix-spec-proposals/pull/4287
#[derive(Clone, Debug, Default, Deserialize, Serialize, EventContent)]
#[ruma_event(type = "m.org.matrix.custom.backup_disabled", kind = GlobalAccountData)]
pub(super) struct BackupDisabledContent {
    pub disabled: bool,
}
