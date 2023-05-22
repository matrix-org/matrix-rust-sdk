// Copyright 2021 The Matrix.org Foundation C.I.C.
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
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use ruma::{
    api::client::backup::{KeyBackupData, KeyBackupDataInit, SessionDataInit},
    serde::Base64,
    OwnedDeviceKeyId, OwnedUserId,
};
use vodozemac::Curve25519PublicKey;
use zeroize::Zeroizing;

use super::{compat::PkEncryption, recovery::DecodeError};
use crate::olm::InboundGroupSession;

#[derive(Debug)]
struct InnerBackupKey {
    key: Curve25519PublicKey,
    signatures: BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceKeyId, String>>,
    version: Mutex<Option<String>>,
}

/// The public part of a backup key.
#[derive(Clone)]
pub struct MegolmV1BackupKey {
    inner: Arc<InnerBackupKey>,
}

impl std::fmt::Debug for MegolmV1BackupKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MegolmV1BackupKey")
            .field("key", &self.to_base64())
            .field("version", &self.backup_version())
            .finish()
    }
}

impl MegolmV1BackupKey {
    pub(super) fn new(key: Curve25519PublicKey, version: Option<String>) -> Self {
        Self {
            inner: InnerBackupKey {
                key,
                signatures: Default::default(),
                version: Mutex::new(version),
            }
            .into(),
        }
    }

    /// Get the full name of the backup algorithm this backup key supports.
    pub fn backup_algorithm(&self) -> &str {
        "m.megolm_backup.v1.curve25519-aes-sha2"
    }

    /// Get all the signatures of this `MegolmV1BackupKey`.
    pub fn signatures(&self) -> BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceKeyId, String>> {
        self.inner.signatures.to_owned()
    }

    /// Try to create a new `MegolmV1BackupKey` from a base 64 encoded string.
    pub fn from_base64(public_key: &str) -> Result<Self, DecodeError> {
        let key = Curve25519PublicKey::from_base64(public_key)?;

        let inner =
            InnerBackupKey { key, signatures: Default::default(), version: Mutex::new(None) };

        Ok(MegolmV1BackupKey { inner: inner.into() })
    }

    /// Convert the [`MegolmV1BackupKey`] to a base 64 encoded string.
    pub fn to_base64(&self) -> String {
        self.inner.key.to_base64()
    }

    /// Get the backup version that this key is used with, if any.
    pub fn backup_version(&self) -> Option<String> {
        self.inner.version.lock().unwrap().clone()
    }

    /// Set the backup version that this `MegolmV1BackupKey` will be used with.
    ///
    /// The key won't be able to encrypt room keys unless a version has been
    /// set.
    pub fn set_version(&self, version: String) {
        *self.inner.version.lock().unwrap() = Some(version);
    }

    pub(crate) async fn encrypt(&self, session: InboundGroupSession) -> KeyBackupData {
        let pk = PkEncryption::from_key(self.inner.key);

        // The forwarding chains don't mean much, we only care whether we received the
        // session directly from the creator of the session or not.
        let forwarded_count = (session.has_been_imported() as u8).into();
        let first_message_index = session.first_known_index().into();

        // Convert our key to the backup representation.
        let key = session.to_backup().await;

        // The key gets zeroized in `BackedUpRoomKey` but we're creating a copy
        // here that won't, so let's wrap it up in a `Zeroizing` struct.
        let key =
            Zeroizing::new(serde_json::to_vec(&key).expect("Can't serialize exported room key"));

        let message = pk.encrypt(&key);

        let session_data = SessionDataInit {
            ephemeral: Base64::new(message.ephemeral_key.to_vec()),
            ciphertext: Base64::new(message.ciphertext),
            mac: Base64::new(message.mac),
        }
        .into();

        KeyBackupDataInit {
            first_message_index,
            forwarded_count,
            // TODO: is this actually used anywhere? seems to be completely
            // useless and requires us to get the Device out of the store?
            // Also should this be checked at the time of the backup or at the
            // time of the room key receival?
            is_verified: false,
            session_data,
        }
        .into()
    }
}
