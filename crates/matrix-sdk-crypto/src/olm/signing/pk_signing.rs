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

use std::{collections::BTreeMap, convert::TryInto, sync::Arc};

use ruma::{
    encryption::KeyUsage, serde::CanonicalJsonValue, DeviceId, DeviceKeyAlgorithm, DeviceKeyId,
    UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::{Error as JsonError, Value};
use thiserror::Error;
use vodozemac::{Ed25519PublicKey, Ed25519SecretKey, Ed25519Signature, KeyError};

use crate::{
    error::SignatureError,
    identities::{MasterPubkey, SelfSigningPubkey, UserSigningPubkey},
    types::{CrossSigningKey, CrossSigningKeySignatures, DeviceKeys},
    utilities::{encode, DecodeError},
    ReadOnlyUserIdentity,
};

/// Error type reporting failures in the Signign operations.
#[derive(Debug, Error)]
pub enum SigningError {
    /// Error decoding the base64 encoded pickle data.
    #[error(transparent)]
    Decode(#[from] DecodeError),

    /// Error decrypting the pickled signing seed
    #[error("Error decrypting the pickled signign seed")]
    Decryption(String),

    /// Error deserializing the pickle data.
    #[error(transparent)]
    Json(#[from] JsonError),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Signing {
    inner: Arc<Ed25519SecretKey>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InnerPickle {
    version: u8,
    nonce: String,
    ciphertext: String,
}

#[derive(Clone, PartialEq, Debug)]
pub struct MasterSigning {
    pub inner: Signing,
    pub public_key: MasterPubkey,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PickledMasterSigning {
    pickle: PickledSigning,
    public_key: CrossSigningKey,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PickledUserSigning {
    pickle: PickledSigning,
    public_key: CrossSigningKey,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PickledSelfSigning {
    pickle: PickledSigning,
    public_key: CrossSigningKey,
}

impl PickledSigning {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl MasterSigning {
    pub async fn pickle(&self) -> PickledMasterSigning {
        let pickle = self.inner.pickle().await;
        let public_key = self.public_key.as_ref().clone();
        PickledMasterSigning { pickle, public_key }
    }

    pub fn export_seed(&self) -> String {
        encode(self.inner.as_bytes())
    }

    pub fn from_base64(user_id: Box<UserId>, key: &str) -> Result<Self, KeyError> {
        let inner = Signing::from_base64(key)?;
        let public_key = inner.cross_signing_key(user_id, KeyUsage::Master).into();

        Ok(Self { inner, public_key })
    }

    pub fn from_pickle(pickle: PickledMasterSigning) -> Result<Self, SigningError> {
        let inner = Signing::from_pickle(pickle.pickle)?;

        Ok(Self { inner, public_key: pickle.public_key.into() })
    }

    pub async fn sign(&self, message: &str) -> Ed25519Signature {
        self.inner.sign(message).await
    }

    pub async fn sign_subkey(&self, subkey: &mut CrossSigningKey) {
        let json_subkey = serde_json::to_value(&subkey).expect("Can't serialize cross signing key");
        let signature =
            self.inner.sign_json(json_subkey).await.expect("Can't sign cross signing keys");

        subkey
            .signatures
            .entry(self.public_key.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(
                DeviceKeyId::from_parts(
                    DeviceKeyAlgorithm::Ed25519,
                    &Box::<DeviceId>::from(self.inner.public_key.to_base64()),
                ),
                signature.to_base64(),
            );
    }
}

impl UserSigning {
    pub async fn pickle(&self) -> PickledUserSigning {
        let pickle = self.inner.pickle().await;
        let public_key = self.public_key.as_ref().clone();
        PickledUserSigning { pickle, public_key }
    }

    pub fn export_seed(&self) -> String {
        encode(self.inner.as_bytes())
    }

    pub fn from_base64(user_id: Box<UserId>, key: &str) -> Result<Self, KeyError> {
        let inner = Signing::from_base64(key)?;
        let public_key = inner.cross_signing_key(user_id, KeyUsage::UserSigning).into();

        Ok(Self { inner, public_key })
    }

    pub async fn sign_user(
        &self,
        user: &ReadOnlyUserIdentity,
    ) -> Result<CrossSigningKey, SignatureError> {
        let signatures = self.sign_user_helper(user).await?;
        let mut master_key = user.master_key().as_ref().clone();

        master_key.signatures = signatures;

        Ok(master_key)
    }

    pub async fn sign_user_helper(
        &self,
        user: &ReadOnlyUserIdentity,
    ) -> Result<CrossSigningKeySignatures, SignatureError> {
        let user_master: &CrossSigningKey = user.master_key().as_ref();
        let signature = self.inner.sign_json(serde_json::to_value(user_master)?).await?;

        let mut signatures = BTreeMap::new();

        signatures
            .entry(self.public_key.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(
                DeviceKeyId::from_parts(
                    DeviceKeyAlgorithm::Ed25519,
                    &Box::<DeviceId>::from(self.inner.public_key.to_base64()),
                ),
                signature.to_base64(),
            );

        Ok(signatures)
    }

    pub fn from_pickle(pickle: PickledUserSigning) -> Result<Self, SigningError> {
        let inner = Signing::from_pickle(pickle.pickle)?;

        Ok(Self { inner, public_key: pickle.public_key.into() })
    }
}

impl SelfSigning {
    pub async fn pickle(&self) -> PickledSelfSigning {
        let pickle = self.inner.pickle().await;
        let public_key = self.public_key.as_ref().clone();
        PickledSelfSigning { pickle, public_key }
    }

    pub fn export_seed(&self) -> String {
        encode(self.inner.as_bytes())
    }

    pub fn from_base64(user_id: Box<UserId>, key: &str) -> Result<Self, KeyError> {
        let inner = Signing::from_base64(key)?;
        let public_key = inner.cross_signing_key(user_id, KeyUsage::SelfSigning).into();

        Ok(Self { inner, public_key })
    }

    pub async fn sign_device_helper(
        &self,
        value: Value,
    ) -> Result<Ed25519Signature, SignatureError> {
        self.inner.sign_json(value).await
    }

    pub async fn sign_device(&self, device_keys: &mut DeviceKeys) -> Result<(), SignatureError> {
        let signature = self.sign_device_helper(serde_json::to_value(&device_keys)?).await?;

        device_keys
            .signatures
            .entry(self.public_key.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(
                DeviceKeyId::from_parts(
                    DeviceKeyAlgorithm::Ed25519,
                    &Box::<DeviceId>::from(self.inner.public_key.to_base64()),
                ),
                signature.to_base64(),
            );

        Ok(())
    }

    pub fn from_pickle(pickle: PickledSelfSigning) -> Result<Self, SigningError> {
        let inner = Signing::from_pickle(pickle.pickle)?;

        Ok(Self { inner, public_key: pickle.public_key.into() })
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct SelfSigning {
    pub inner: Signing,
    pub public_key: SelfSigningPubkey,
}

#[derive(Clone, PartialEq, Debug)]
pub struct UserSigning {
    pub inner: Signing,
    pub public_key: UserSigningPubkey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickledSignings {
    pub master_key: Option<PickledMasterSigning>,
    pub user_signing_key: Option<PickledUserSigning>,
    pub self_signing_key: Option<PickledSelfSigning>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickledSigning(String);

impl Signing {
    pub fn new() -> Self {
        let secret_key = Ed25519SecretKey::new();
        Self::new_helper(secret_key)
    }

    fn new_helper(secret_key: Ed25519SecretKey) -> Self {
        let public_key = secret_key.public_key();

        Signing { inner: Arc::new(secret_key), public_key }
    }

    pub fn from_base64(key: &str) -> Result<Self, KeyError> {
        let key = Ed25519SecretKey::from_base64(key)?;
        Ok(Self::new_helper(key))
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.inner.as_bytes()
    }

    pub fn from_pickle(pickle: PickledSigning) -> Result<Self, SigningError> {
        let secret_key: Ed25519SecretKey = serde_json::from_str(pickle.as_str())?;
        Ok(Self::new_helper(secret_key))
    }

    pub async fn pickle(&self) -> PickledSigning {
        PickledSigning(serde_json::to_string(&self.inner).expect("Can't serialize signing key"))
    }

    pub fn public_key(&self) -> Ed25519PublicKey {
        self.public_key
    }

    pub fn cross_signing_key(&self, user_id: Box<UserId>, usage: KeyUsage) -> CrossSigningKey {
        let keys = BTreeMap::from([(
            DeviceKeyId::from_parts(
                DeviceKeyAlgorithm::Ed25519,
                &Box::<DeviceId>::from(self.public_key().to_base64()),
            ),
            self.inner.public_key().into(),
        )]);

        CrossSigningKey::new(user_id, vec![usage], keys, BTreeMap::new())
    }

    #[cfg(test)]
    pub async fn verify(
        &self,
        message: &str,
        signature: &Ed25519Signature,
    ) -> Result<(), SignatureError> {
        Ok(self.public_key.verify(message.as_bytes(), signature)?)
    }

    pub async fn sign_json(&self, mut json: Value) -> Result<Ed25519Signature, SignatureError> {
        let json_object = json.as_object_mut().ok_or(SignatureError::NotAnObject)?;
        let _ = json_object.remove("signatures");
        let _ = json_object.remove("unsigned");

        let canonical_json: CanonicalJsonValue =
            json.try_into().expect("Can't canonicalize the json value");

        Ok(self.sign(&canonical_json.to_string()).await)
    }

    pub async fn sign(&self, message: &str) -> Ed25519Signature {
        self.inner.sign(message.as_bytes())
    }
}
