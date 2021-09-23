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

use aes_gcm::{
    aead::{generic_array::GenericArray, Aead, NewAead},
    Aes256Gcm,
};
use getrandom::getrandom;
use matrix_sdk_common::locks::Mutex;
use olm_rs::pk::OlmPkSigning;
#[cfg(test)]
use olm_rs::{errors::OlmUtilityError, utility::OlmUtility};
use ruma::{
    encryption::{CrossSigningKey, CrossSigningKeySignatures, DeviceKeys, KeyUsage},
    serde::CanonicalJsonValue,
    DeviceKeyAlgorithm, DeviceKeyId, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Error as JsonError, Value};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::{
    error::SignatureError,
    identities::{MasterPubkey, SelfSigningPubkey, UserSigningPubkey},
    utilities::{decode_url_safe, encode, encode_url_safe, DecodeError},
    ReadOnlyUserIdentity,
};

const NONCE_SIZE: usize = 12;

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

#[derive(Clone)]
pub struct Signing {
    inner: Arc<Mutex<OlmPkSigning>>,
    seed: Arc<Zeroizing<Vec<u8>>>,
    public_key: PublicSigningKey,
}

impl std::fmt::Debug for Signing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signing").field("public_key", &self.public_key.as_str()).finish()
    }
}

impl PartialEq for Signing {
    fn eq(&self, other: &Signing) -> bool {
        self.seed == other.seed
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicSigningKey(Arc<str>);

impl PublicSigningKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[allow(clippy::inherent_to_string)]
    fn to_string(&self) -> String {
        self.0.to_string()
    }
}

impl MasterSigning {
    pub async fn pickle(&self, pickle_key: &[u8]) -> PickledMasterSigning {
        let pickle = self.inner.pickle(pickle_key).await;
        let public_key = self.public_key.clone().into();
        PickledMasterSigning { pickle, public_key }
    }

    pub fn export_seed(&self) -> String {
        encode(self.inner.seed.as_slice())
    }

    pub fn from_seed(user_id: UserId, seed: Vec<u8>) -> Self {
        let inner = Signing::from_seed(seed);
        let public_key = inner.cross_signing_key(user_id, KeyUsage::Master).into();

        Self { inner, public_key }
    }

    pub fn from_pickle(
        pickle: PickledMasterSigning,
        pickle_key: &[u8],
    ) -> Result<Self, SigningError> {
        let inner = Signing::from_pickle(pickle.pickle, pickle_key)?;

        Ok(Self { inner, public_key: pickle.public_key.into() })
    }

    pub async fn sign(&self, message: &str) -> String {
        self.inner.sign(message).await.0
    }

    pub async fn sign_subkey<'a>(&self, subkey: &mut CrossSigningKey) {
        let subkey_without_signatures = json!({
            "user_id": subkey.user_id.clone(),
            "keys": subkey.keys.clone(),
            "usage": subkey.usage.clone(),
        });

        let message = serde_json::to_string(&subkey_without_signatures)
            .expect("Can't serialize cross signing subkey");
        let signature = self.sign(&message).await;

        subkey
            .signatures
            .entry(self.public_key.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(
                DeviceKeyId::from_parts(
                    DeviceKeyAlgorithm::Ed25519,
                    self.inner.public_key().as_str().into(),
                )
                .to_string(),
                signature,
            );
    }
}

impl UserSigning {
    pub async fn pickle(&self, pickle_key: &[u8]) -> PickledUserSigning {
        let pickle = self.inner.pickle(pickle_key).await;
        let public_key = self.public_key.clone().into();
        PickledUserSigning { pickle, public_key }
    }

    pub fn export_seed(&self) -> String {
        encode(self.inner.seed.as_slice())
    }

    pub fn from_seed(user_id: UserId, seed: Vec<u8>) -> Self {
        let inner = Signing::from_seed(seed);
        let public_key = inner.cross_signing_key(user_id, KeyUsage::UserSigning).into();

        Self { inner, public_key }
    }

    pub async fn sign_user(
        &self,
        user: &ReadOnlyUserIdentity,
    ) -> Result<CrossSigningKey, SignatureError> {
        let signature = self.sign_user_helper(user).await?;
        let mut master_key: CrossSigningKey = user.master_key().to_owned().into();

        master_key.signatures.extend(signature);

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
                    self.inner.public_key.as_str().into(),
                )
                .to_string(),
                signature.0,
            );

        Ok(signatures)
    }

    pub fn from_pickle(
        pickle: PickledUserSigning,
        pickle_key: &[u8],
    ) -> Result<Self, SigningError> {
        let inner = Signing::from_pickle(pickle.pickle, pickle_key)?;

        Ok(Self { inner, public_key: pickle.public_key.into() })
    }
}

impl SelfSigning {
    pub async fn pickle(&self, pickle_key: &[u8]) -> PickledSelfSigning {
        let pickle = self.inner.pickle(pickle_key).await;
        let public_key = self.public_key.clone().into();
        PickledSelfSigning { pickle, public_key }
    }

    pub fn export_seed(&self) -> String {
        encode(self.inner.seed.as_slice())
    }

    pub fn from_seed(user_id: UserId, seed: Vec<u8>) -> Self {
        let inner = Signing::from_seed(seed);
        let public_key = inner.cross_signing_key(user_id, KeyUsage::SelfSigning).into();

        Self { inner, public_key }
    }

    pub async fn sign_device_helper(&self, value: Value) -> Result<Signature, SignatureError> {
        self.inner.sign_json(value).await
    }

    pub async fn sign_device(&self, device_keys: &mut DeviceKeys) -> Result<(), SignatureError> {
        // Create a copy of the device keys containing only fields that will
        // get signed.
        let json_device = json!({
            "user_id": device_keys.user_id,
            "device_id": device_keys.device_id,
            "algorithms": device_keys.algorithms,
            "keys": device_keys.keys,
        });

        let signature = self.sign_device_helper(json_device).await?;

        device_keys
            .signatures
            .entry(self.public_key.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(
                DeviceKeyId::from_parts(
                    DeviceKeyAlgorithm::Ed25519,
                    self.inner.public_key.as_str().into(),
                ),
                signature.0,
            );

        Ok(())
    }

    pub fn from_pickle(
        pickle: PickledSelfSigning,
        pickle_key: &[u8],
    ) -> Result<Self, SigningError> {
        let inner = Signing::from_pickle(pickle.pickle, pickle_key)?;

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

#[derive(Debug, Clone)]
pub struct Signature(String);

impl std::fmt::Display for Signature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickledSigning(String);

impl Signing {
    pub fn new() -> Self {
        let seed = OlmPkSigning::generate_seed();
        Self::from_seed(seed)
    }

    pub fn from_seed(seed: Vec<u8>) -> Self {
        let inner = OlmPkSigning::new(&seed).expect("Unable to create pk signing object");
        let public_key = PublicSigningKey(inner.public_key().into());

        Signing {
            inner: Arc::new(Mutex::new(inner)),
            seed: Arc::new(Zeroizing::from(seed)),
            public_key,
        }
    }

    pub fn from_pickle(pickle: PickledSigning, pickle_key: &[u8]) -> Result<Self, SigningError> {
        let pickled: InnerPickle = serde_json::from_str(pickle.as_str())?;

        let key = GenericArray::from_slice(pickle_key);
        let cipher = Aes256Gcm::new(key);

        let nonce = decode_url_safe(pickled.nonce)?;
        let nonce = GenericArray::from_slice(&nonce);
        let ciphertext = &decode_url_safe(pickled.ciphertext)?;

        let seed = cipher
            .decrypt(nonce, ciphertext.as_slice())
            .map_err(|e| SigningError::Decryption(e.to_string()))?;

        Ok(Self::from_seed(seed))
    }

    pub async fn pickle(&self, pickle_key: &[u8]) -> PickledSigning {
        let key = GenericArray::from_slice(pickle_key);
        let cipher = Aes256Gcm::new(key);

        let mut nonce = vec![0u8; NONCE_SIZE];
        getrandom(&mut nonce).expect("Can't generate nonce to pickle the signing object");
        let nonce = GenericArray::from_slice(nonce.as_slice());

        let ciphertext =
            cipher.encrypt(nonce, self.seed.as_slice()).expect("Can't encrypt signing pickle");

        let ciphertext = encode_url_safe(ciphertext);

        let pickle =
            InnerPickle { version: 1, nonce: encode_url_safe(nonce.as_slice()), ciphertext };

        PickledSigning(serde_json::to_string(&pickle).expect("Can't encode pickled signing"))
    }

    pub fn public_key(&self) -> &PublicSigningKey {
        &self.public_key
    }

    pub fn cross_signing_key(&self, user_id: UserId, usage: KeyUsage) -> CrossSigningKey {
        let mut keys = BTreeMap::new();

        keys.insert(
            DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.public_key().as_str().into())
                .to_string(),
            self.public_key().to_string(),
        );

        CrossSigningKey::new(user_id, vec![usage], keys, BTreeMap::new())
    }

    #[cfg(test)]
    pub async fn verify(
        &self,
        message: &str,
        signature: &Signature,
    ) -> Result<bool, OlmUtilityError> {
        let utility = OlmUtility::new();
        utility.ed25519_verify(self.public_key.as_str(), message, signature.to_string())
    }

    pub async fn sign_json(&self, mut json: Value) -> Result<Signature, SignatureError> {
        let json_object = json.as_object_mut().ok_or(SignatureError::NotAnObject)?;
        let _ = json_object.remove("signatures");
        let canonical_json: CanonicalJsonValue =
            json.try_into().expect("Can't canonicalize the json value");
        Ok(self.sign(&canonical_json.to_string()).await)
    }

    pub async fn sign(&self, message: &str) -> Signature {
        Signature(self.inner.lock().await.sign(message))
    }
}
