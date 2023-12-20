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

use ruma::{encryption::KeyUsage, DeviceKeyAlgorithm, DeviceKeyId, OwnedUserId};
use serde::{Deserialize, Serialize};
use serde_json::{Error as JsonError, Value};
use thiserror::Error;
use vodozemac::{DecodeError, Ed25519PublicKey, Ed25519SecretKey, Ed25519Signature, KeyError};
use zeroize::Zeroize;

use crate::{
    error::SignatureError,
    olm::utility::SignJson,
    types::{
        CrossSigningKey, DeviceKeys, MasterPubkey, SelfSigningPubkey, Signatures, SigningKeys,
        UserSigningPubkey,
    },
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

#[cfg(not(tarpaulin_include))]
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
    public_key: MasterPubkey,
}

#[derive(Deserialize, Serialize)]
#[allow(missing_debug_implementations)]
pub struct PickledUserSigning {
    pickle: PickledSigning,
    public_key: UserSigningPubkey,
}

#[derive(Deserialize, Serialize)]
#[allow(missing_debug_implementations)]
pub struct PickledSelfSigning {
    pickle: PickledSigning,
    public_key: SelfSigningPubkey,
}

impl MasterSigning {
    pub fn new(user_id: OwnedUserId) -> Self {
        let inner = Signing::new();
        let public_key = inner
            .cross_signing_key(user_id, KeyUsage::Master)
            .try_into()
            .expect("A freshly created master key can be converted into a MasterPubkey");
        let mut key = Self { inner, public_key };
        let mut cross_signing_key = key.public_key.as_ref().to_owned();

        key.sign_subkey(&mut cross_signing_key);
        key.public_key = cross_signing_key
            .try_into()
            .expect("A freshly signed master key can be converted into a MasterPubkey");

        key
    }

    pub fn pickle(&self) -> PickledMasterSigning {
        let pickle = self.inner.pickle();
        let public_key = self.public_key.clone();
        PickledMasterSigning { pickle, public_key }
    }

    pub fn export_seed(&self) -> String {
        self.inner.to_base64()
    }

    pub fn from_base64(user_id: OwnedUserId, key: &str) -> Result<Self, KeyError> {
        let inner = Signing::from_base64(key)?;
        let public_key = inner
            .cross_signing_key(user_id, KeyUsage::Master)
            .try_into()
            .expect("A master key can always be imported from a base64 encoded value");

        Ok(Self { inner, public_key })
    }

    pub fn from_pickle(pickle: PickledMasterSigning) -> Result<Self, SigningError> {
        let inner = Signing::from_pickle(pickle.pickle)?;
        let public_key = pickle.public_key;

        Ok(Self { inner, public_key })
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
        let public_key = self.public_key.clone();
        PickledUserSigning { pickle, public_key }
    }

    pub fn export_seed(&self) -> String {
        self.inner.to_base64()
    }

    pub fn from_base64(user_id: OwnedUserId, key: &str) -> Result<Self, KeyError> {
        let inner = Signing::from_base64(key)?;
        let public_key = inner
            .cross_signing_key(user_id, KeyUsage::UserSigning)
            .try_into()
            .expect("A user-signing key can always be imported from a base64 encoded value");

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

        Ok(Self { inner, public_key: pickle.public_key })
    }
}

impl SelfSigning {
    pub fn pickle(&self) -> PickledSelfSigning {
        let pickle = self.inner.pickle();
        let public_key = self.public_key.clone();
        PickledSelfSigning { pickle, public_key }
    }

    pub fn export_seed(&self) -> String {
        self.inner.to_base64()
    }

    pub fn from_base64(user_id: OwnedUserId, key: &str) -> Result<Self, KeyError> {
        let inner = Signing::from_base64(key)?;
        let public_key = inner
            .cross_signing_key(user_id, KeyUsage::SelfSigning)
            .try_into()
            .expect("A self-signing key can always be imported from a base64 encoded value");

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

        Ok(Self { inner, public_key: pickle.public_key })
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

    pub fn from_pickle(pickle: PickledSigning) -> Result<Self, SigningError> {
        Ok(Self::new_helper(pickle.0))
    }

    pub fn to_base64(&self) -> String {
        self.inner.to_base64()
    }

    pub fn pickle(&self) -> PickledSigning {
        let mut bytes = self.inner.to_bytes();
        let ret = PickledSigning(Ed25519SecretKey::from_slice(&bytes));

        bytes.zeroize();

        ret
    }

    pub fn public_key(&self) -> Ed25519PublicKey {
        self.public_key
    }

    pub fn cross_signing_key(&self, user_id: OwnedUserId, usage: KeyUsage) -> CrossSigningKey {
        let keys = SigningKeys::from([(
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
