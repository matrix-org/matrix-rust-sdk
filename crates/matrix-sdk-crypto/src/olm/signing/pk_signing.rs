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

use std::collections::BTreeMap;

use ruma::{encryption::KeyUsage, DeviceKeyAlgorithm, DeviceKeyId, OwnedUserId};
use serde::{Deserialize, Serialize};
use serde_json::{Error as JsonError, Value};
use thiserror::Error;
use vodozemac::{Ed25519PublicKey, Ed25519SecretKey, Ed25519Signature, KeyError};

use crate::{
    error::SignatureError,
    identities::{MasterPubkey, SelfSigningPubkey, UserSigningPubkey},
    olm::utility::SignJson,
    types::{CrossSigningKey, DeviceKeys, Signatures},
    utilities::{encode, DecodeError},
    ReadOnlyUserIdentity,
};

/// Error type reporting failures in the signing operations.
#[derive(Debug, Error)]
pub enum SigningError {
    /// Error decoding the base64 encoded pickle data.
    #[error(transparent)]
    Decode(#[from] DecodeError),

    /// Error decrypting the pickled signing seed
    #[error("Error decrypting the pickled signing seed")]
    Decryption(String),

    /// Error deserializing the pickle data.
    #[error(transparent)]
    Json(#[from] JsonError),
}

#[derive(Serialize, Deserialize)]
pub struct Signing {
    inner: Ed25519SecretKey,
    public_key: Ed25519PublicKey,
}

impl std::fmt::Debug for Signing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signing").field("public_key", &self.public_key.to_base64()).finish()
    }
}

impl PartialEq for Signing {
    fn eq(&self, other: &Signing) -> bool {
        self.public_key == other.public_key
    }
}

impl SignJson for Signing {
    fn sign_json(&self, value: Value) -> Result<Ed25519Signature, SignatureError> {
        self.inner.sign_json(value)
    }
}

#[derive(PartialEq, Debug)]
pub struct MasterSigning {
    pub inner: Signing,
    pub public_key: MasterPubkey,
}

#[derive(Deserialize, Serialize)]
#[allow(missing_debug_implementations)]
pub struct PickledMasterSigning {
    pickle: PickledSigning,
    public_key: CrossSigningKey,
}

#[derive(Deserialize, Serialize)]
#[allow(missing_debug_implementations)]
pub struct PickledUserSigning {
    pickle: PickledSigning,
    public_key: CrossSigningKey,
}

#[derive(Deserialize, Serialize)]
#[allow(missing_debug_implementations)]
pub struct PickledSelfSigning {
    pickle: PickledSigning,
    public_key: CrossSigningKey,
}

impl MasterSigning {
    pub fn pickle(&self) -> PickledMasterSigning {
        let pickle = self.inner.pickle();
        let public_key = self.public_key.as_ref().clone();
        PickledMasterSigning { pickle, public_key }
    }

    pub fn export_seed(&self) -> String {
        encode(self.inner.as_bytes())
    }

    pub fn from_base64(user_id: OwnedUserId, key: &str) -> Result<Self, KeyError> {
        let inner = Signing::from_base64(key)?;
        let public_key = inner.cross_signing_key(user_id, KeyUsage::Master).into();

        Ok(Self { inner, public_key })
    }

    pub fn from_pickle(pickle: PickledMasterSigning) -> Result<Self, SigningError> {
        let inner = Signing::from_pickle(pickle.pickle)?;

        Ok(Self { inner, public_key: pickle.public_key.into() })
    }

    pub fn sign(&self, message: &str) -> Ed25519Signature {
        self.inner.sign(message)
    }

    pub fn sign_subkey(&self, subkey: &mut CrossSigningKey) {
        let json_subkey = serde_json::to_value(&subkey).expect("Can't serialize cross signing key");
        let signature = self.inner.sign_json(json_subkey).expect("Can't sign cross signing keys");

        subkey.signatures.add_signature(
            self.public_key.user_id().to_owned(),
            DeviceKeyId::from_parts(
                DeviceKeyAlgorithm::Ed25519,
                self.inner.public_key.to_base64().as_str().into(),
            ),
            signature,
        );
    }
}

impl UserSigning {
    pub fn pickle(&self) -> PickledUserSigning {
        let pickle = self.inner.pickle();
        let public_key = self.public_key.as_ref().clone();
        PickledUserSigning { pickle, public_key }
    }

    pub fn export_seed(&self) -> String {
        encode(self.inner.as_bytes())
    }

    pub fn from_base64(user_id: OwnedUserId, key: &str) -> Result<Self, KeyError> {
        let inner = Signing::from_base64(key)?;
        let public_key = inner.cross_signing_key(user_id, KeyUsage::UserSigning).into();

        Ok(Self { inner, public_key })
    }

    pub fn sign_user(
        &self,
        user: &ReadOnlyUserIdentity,
    ) -> Result<CrossSigningKey, SignatureError> {
        let signatures = self.sign_user_helper(user)?;
        let mut master_key = user.master_key().as_ref().clone();

        master_key.signatures = signatures;

        Ok(master_key)
    }

    pub fn sign_user_helper(
        &self,
        user: &ReadOnlyUserIdentity,
    ) -> Result<Signatures, SignatureError> {
        let user_master: &CrossSigningKey = user.master_key().as_ref();
        let signature = self.inner.sign_json(serde_json::to_value(user_master)?)?;

        let mut signatures = Signatures::new();

        signatures.add_signature(
            self.public_key.user_id().to_owned(),
            DeviceKeyId::from_parts(
                DeviceKeyAlgorithm::Ed25519,
                self.inner.public_key.to_base64().as_str().into(),
            ),
            signature,
        );

        Ok(signatures)
    }

    pub fn from_pickle(pickle: PickledUserSigning) -> Result<Self, SigningError> {
        let inner = Signing::from_pickle(pickle.pickle)?;

        Ok(Self { inner, public_key: pickle.public_key.into() })
    }
}

impl SelfSigning {
    pub fn pickle(&self) -> PickledSelfSigning {
        let pickle = self.inner.pickle();
        let public_key = self.public_key.as_ref().clone();
        PickledSelfSigning { pickle, public_key }
    }

    pub fn export_seed(&self) -> String {
        encode(self.inner.as_bytes())
    }

    pub fn from_base64(user_id: OwnedUserId, key: &str) -> Result<Self, KeyError> {
        let inner = Signing::from_base64(key)?;
        let public_key = inner.cross_signing_key(user_id, KeyUsage::SelfSigning).into();

        Ok(Self { inner, public_key })
    }

    pub fn sign_device(&self, device_keys: &mut DeviceKeys) -> Result<(), SignatureError> {
        let serialized = serde_json::to_value(&device_keys)?;
        let signature = self.inner.sign_json(serialized)?;

        device_keys.signatures.add_signature(
            self.public_key.user_id().to_owned(),
            DeviceKeyId::from_parts(
                DeviceKeyAlgorithm::Ed25519,
                self.inner.public_key.to_base64().as_str().into(),
            ),
            signature,
        );

        Ok(())
    }

    pub fn from_pickle(pickle: PickledSelfSigning) -> Result<Self, SigningError> {
        let inner = Signing::from_pickle(pickle.pickle)?;

        Ok(Self { inner, public_key: pickle.public_key.into() })
    }
}

#[derive(PartialEq, Debug)]
pub struct SelfSigning {
    pub inner: Signing,
    pub public_key: SelfSigningPubkey,
}

#[derive(PartialEq, Debug)]
pub struct UserSigning {
    pub inner: Signing,
    pub public_key: UserSigningPubkey,
}

#[derive(Serialize, Deserialize)]
#[allow(missing_debug_implementations)]
pub struct PickledSignings {
    pub master_key: Option<PickledMasterSigning>,
    pub user_signing_key: Option<PickledUserSigning>,
    pub self_signing_key: Option<PickledSelfSigning>,
}

#[derive(Serialize, Deserialize)]
pub struct PickledSigning(Ed25519SecretKey);

impl Signing {
    pub fn new() -> Self {
        let secret_key = Ed25519SecretKey::new();
        Self::new_helper(secret_key)
    }

    fn new_helper(secret_key: Ed25519SecretKey) -> Self {
        let public_key = secret_key.public_key();

        Signing { inner: secret_key, public_key }
    }

    pub fn from_base64(key: &str) -> Result<Self, KeyError> {
        let key = Ed25519SecretKey::from_base64(key)?;
        Ok(Self::new_helper(key))
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.inner.as_bytes()
    }

    pub fn from_pickle(pickle: PickledSigning) -> Result<Self, SigningError> {
        Ok(Self::new_helper(pickle.0))
    }

    pub fn pickle(&self) -> PickledSigning {
        PickledSigning(
            Ed25519SecretKey::from_slice(self.inner.as_bytes())
                .expect("Copying the private key should work"),
        )
    }

    pub fn public_key(&self) -> Ed25519PublicKey {
        self.public_key
    }

    pub fn cross_signing_key(&self, user_id: OwnedUserId, usage: KeyUsage) -> CrossSigningKey {
        let keys = BTreeMap::from([(
            DeviceKeyId::from_parts(
                DeviceKeyAlgorithm::Ed25519,
                self.public_key().to_base64().as_str().into(),
            ),
            self.inner.public_key().into(),
        )]);

        CrossSigningKey::new(user_id, vec![usage], keys, Default::default())
    }

    pub fn sign(&self, message: &str) -> Ed25519Signature {
        self.inner.sign(message.as_bytes())
    }
}
