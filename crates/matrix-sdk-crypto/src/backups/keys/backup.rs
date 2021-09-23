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

use olm_rs::pk::OlmPkEncryption;
use ruma::{
    api::client::r0::backup::{BackupAlgorithm, KeyBackupData, KeyBackupDataInit, SessionDataInit},
    DeviceKeyId, UserId,
};
use zeroize::Zeroizing;

use super::recovery::DecodeError;
use crate::olm::InboundGroupSession;

#[derive(Debug)]
struct InnerBackupKey {
    key: [u8; MegolmV1BackupKey::KEY_SIZE],
    signatures: BTreeMap<UserId, BTreeMap<DeviceKeyId, String>>,
    version: Mutex<Option<String>>,
}

#[derive(Debug, Clone)]
pub struct MegolmV1BackupKey {
    inner: Arc<InnerBackupKey>,
}

impl MegolmV1BackupKey {
    const KEY_SIZE: usize = 32;

    pub(super) fn new(public_key: &str, version: Option<String>) -> Self {
        let key = Self::from_base64(public_key).expect("Invalid backup key");

        if let Some(version) = version {
            key.set_version(version)
        }

        key
    }

    /// TODO
    pub fn from_base64(public_key: &str) -> Result<Self, DecodeError> {
        let mut key = [0u8; Self::KEY_SIZE];
        let decoded_key = crate::utilities::decode(public_key)?;

        if decoded_key.len() != Self::KEY_SIZE {
            Err(DecodeError::Length(Self::KEY_SIZE, decoded_key.len()))
        } else {
            key.copy_from_slice(&decoded_key);

            let inner =
                InnerBackupKey { key, signatures: Default::default(), version: Mutex::new(None) };

            Ok(MegolmV1BackupKey { inner: inner.into() })
        }
    }

    /// TODO
    pub fn backup_version(&self) -> Option<String> {
        self.inner.version.lock().unwrap().clone()
    }

    /// TODO
    pub fn set_version(&self, version: String) {
        *self.inner.version.lock().unwrap() = Some(version);
    }

    pub(crate) async fn encrypt(&self, session: InboundGroupSession) -> KeyBackupData {
        let pk = OlmPkEncryption::new(&self.encoded_key());

        // It's ok to truncate here, there's a semantic difference only between
        // 0 and 1+ anyways.
        let forwarded_count = (session.forwarding_key_chain().len() as u32).into();
        let first_message_index = session.first_known_index().into();

        // Convert our key to the backup representation.
        let key = session.to_backup().await;

        // The key gets zeroized in `BackedUpRoomKey` but we're creating a copy
        // here that won't, so let's wrap it up in a `Zeroizing` struct.
        let key =
            Zeroizing::new(serde_json::to_string(&key).expect("Can't serialize exported room key"));

        let message = pk.encrypt(&key);

        let session_data = SessionDataInit {
            ephemeral: message.ephemeral_key,
            ciphertext: message.ciphertext,
            mac: message.mac,
        }
        .into();

        KeyBackupDataInit {
            first_message_index,
            forwarded_count,
            // TODO is this actually used anywhere? seems to be completely
            // useless and requires us to get the Device out of the store?
            is_verified: false,
            session_data,
        }
        .into()
    }

    pub fn encoded_key(&self) -> String {
        crate::utilities::encode(&self.inner.key)
    }

    pub fn signatures(&self) -> BTreeMap<UserId, BTreeMap<DeviceKeyId, String>> {
        self.inner.signatures.to_owned()
    }

    pub fn to_backup_algorithm(&self) -> BackupAlgorithm {
        BackupAlgorithm::MegolmBackupV1Curve25519AesSha2 {
            public_key: self.encoded_key(),
            signatures: self.inner.signatures.to_owned(),
        }
    }
}
