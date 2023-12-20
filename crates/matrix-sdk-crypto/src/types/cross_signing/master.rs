// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use std::{collections::btree_map::Iter, sync::Arc};

use ruma::{encryption::KeyUsage, DeviceKeyId, OwnedDeviceKeyId, UserId};
use serde::{Deserialize, Serialize};
use vodozemac::Ed25519PublicKey;

use super::{CrossSigningKey, CrossSigningSubKeys, SigningKey};
use crate::{
    olm::VerifyJson,
    types::{Signatures, SigningKeys},
    SignatureError,
};

/// Wrapper for a cross signing key marking it as the master key.
///
/// Master keys are used to sign other cross signing keys, the self signing and
/// user signing keys of an user will be signed by their master key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "CrossSigningKey")]
pub struct MasterPubkey(pub(super) Arc<CrossSigningKey>);

impl MasterPubkey {
    /// Get the user id of the master key's owner.
    pub fn user_id(&self) -> &UserId {
        &self.0.user_id
    }

    /// Get the keys map of containing the master keys.
    pub fn keys(&self) -> &SigningKeys<OwnedDeviceKeyId> {
        &self.0.keys
    }

    /// Get the list of `KeyUsage` that is set for this key.
    pub fn usage(&self) -> &[KeyUsage] {
        &self.0.usage
    }

    /// Get the signatures map of this cross signing key.
    pub fn signatures(&self) -> &Signatures {
        &self.0.signatures
    }

    /// Get the master key with the given key id.
    ///
    /// # Arguments
    ///
    /// * `key_id` - The id of the key that should be fetched.
    pub fn get_key(&self, key_id: &DeviceKeyId) -> Option<&SigningKey> {
        self.0.keys.get(key_id)
    }

    /// Get the first available master key.
    ///
    /// There's usually only a single master key so this will usually fetch the
    /// only key.
    pub fn get_first_key(&self) -> Option<Ed25519PublicKey> {
        self.0.get_first_key_and_id().map(|(_, k)| k)
    }

    /// Check if the given JSON is signed by this master key.
    ///
    /// This method should only be used if an object's signature needs to be
    /// checked multiple times, and you'd like to avoid performing the
    /// canonicalization step each time.
    ///
    /// **Note**: Use this method with caution, the `canonical_json` needs to be
    /// correctly canonicalized and make sure that the object you are checking
    /// the signature for is allowed to be signed by a master key.
    pub(crate) fn has_signed_raw(
        &self,
        signatures: &Signatures,
        canonical_json: &str,
    ) -> Result<(), SignatureError> {
        if let Some((key_id, key)) = self.0.get_first_key_and_id() {
            key.verify_canonicalized_json(&self.0.user_id, key_id, signatures, canonical_json)
        } else {
            Err(SignatureError::UnsupportedAlgorithm)
        }
    }

    /// Check if the given cross signing sub-key is signed by the master key.
    ///
    /// # Arguments
    ///
    /// * `subkey` - The subkey that should be checked for a valid signature.
    ///
    /// Returns an empty result if the signature check succeeded, otherwise a
    /// SignatureError indicating why the check failed.
    pub(crate) fn verify_subkey<'a>(
        &self,
        subkey: impl Into<CrossSigningSubKeys<'a>>,
    ) -> Result<(), SignatureError> {
        let subkey: CrossSigningSubKeys<'_> = subkey.into();

        if self.0.user_id != subkey.user_id() {
            return Err(SignatureError::UserIdMismatch);
        }

        if let Some((key_id, key)) = self.0.get_first_key_and_id() {
            key.verify_json(&self.0.user_id, key_id, subkey.cross_signing_key())
        } else {
            Err(SignatureError::UnsupportedAlgorithm)
        }
    }
}

impl<'a> IntoIterator for &'a MasterPubkey {
    type Item = (&'a OwnedDeviceKeyId, &'a SigningKey);
    type IntoIter = Iter<'a, OwnedDeviceKeyId, SigningKey>;

    fn into_iter(self) -> Self::IntoIter {
        self.keys().iter()
    }
}

impl AsRef<CrossSigningKey> for MasterPubkey {
    fn as_ref(&self) -> &CrossSigningKey {
        &self.0
    }
}

impl TryFrom<CrossSigningKey> for MasterPubkey {
    type Error = serde_json::Error;

    fn try_from(key: CrossSigningKey) -> Result<Self, Self::Error> {
        if key.usage.contains(&KeyUsage::Master) && key.usage.len() == 1 {
            Ok(Self(key.into()))
        } else {
            Err(serde::de::Error::custom(format!(
                "Expected cross signing key usage {} was not found",
                KeyUsage::Master
            )))
        }
    }
}
