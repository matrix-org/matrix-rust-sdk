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

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Struct collecting data that is important to migrate sessions to the rust-sdk
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct SessionMigrationData {
    /// The user id that the data belongs to.
    pub user_id: String,
    /// The device id that the data belongs to.
    pub device_id: String,
    /// The Curve25519 public key of the Account that owns this data.
    pub curve25519_key: String,
    /// The Ed25519 public key of the Account that owns this data.
    pub ed25519_key: String,
    /// The list of pickleds Olm Sessions.
    pub sessions: Vec<PickledSession>,
    /// The list of pickled Megolm inbound group sessions.
    pub inbound_group_sessions: Vec<PickledInboundGroupSession>,
    /// The Olm pickle key that was used to pickle all the Olm objects.
    pub pickle_key: Vec<u8>,
}

/// A pickled version of an `Account`.
///
/// Holds all the information that needs to be stored in a database to restore
/// an account.
#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PickledAccount {
    /// The user id of the account owner.
    pub user_id: String,
    /// The device ID of the account owner.
    pub device_id: String,
    /// The libolm pickle of the Olm account.
    pub pickle: String,
    /// Was the account shared.
    pub shared: bool,
    /// The number of uploaded one-time keys we have on the server.
    pub uploaded_signed_key_count: i64,
}

/// A pickled version of a `Session`.
///
/// Holds all the information that needs to be stored in a database to restore
/// a Session.
#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PickledSession {
    /// The libolm pickle string holding the Olm Session.
    pub pickle: String,
    /// The curve25519 key of the other user that we share this session with.
    pub sender_key: String,
    /// Was the session created using a fallback key.
    pub created_using_fallback_key: bool,
    /// The Unix timestamp when the session was created.
    pub creation_time: String,
    /// The Unix timestamp when the session was last used.
    pub last_use_time: String,
}

/// A pickled version of an `InboundGroupSession`.
///
/// Holds all the information that needs to be stored in a database to restore
/// an InboundGroupSession.
#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PickledInboundGroupSession {
    /// The liblolm pickle string holding the InboundGroupSession.
    pub pickle: String,
    /// The public curve25519 key of the account that sent us the session
    pub sender_key: String,
    /// The public ed25519 key of the account that sent us the session.
    pub signing_key: HashMap<String, String>,
    /// The id of the room that the session is used in.
    pub room_id: String,
    /// The list of claimed ed25519 that forwarded us this key. Will be empty if
    /// we directly received this session.
    pub forwarding_chains: Vec<String>,
    /// Flag remembering if the session was directly sent to us by the sender
    /// or if it was imported.
    pub imported: bool,
    /// Flag remembering if the session has been backed up.
    pub backed_up: bool,
}

/// Error type for the migration process.
#[derive(Debug, thiserror::Error)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error))]
pub enum MigrationError {
    /// Generic catch all error variant.
    #[error("error migrating database: {error_message}")]
    Generic {
        /// The error message
        error_message: String,
    },
}

impl From<anyhow::Error> for MigrationError {
    fn from(e: anyhow::Error) -> MigrationError {
        MigrationError::Generic { error_message: e.to_string() }
    }
}
