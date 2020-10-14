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

#![allow(dead_code)]

use aes_gcm::{
    aead::{generic_array::GenericArray, Aead, NewAead},
    Aes256Gcm,
};
use base64::{decode_config, encode_config, DecodeError, URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use zeroize::Zeroizing;

use olm_rs::{errors::OlmUtilityError, pk::OlmPkSigning, utility::OlmUtility};

use matrix_sdk_common::{
    api::r0::keys::{upload_signing_keys::Request as UploadRequest, CrossSigningKey, KeyUsage},
    identifiers::UserId,
    locks::Mutex,
};

use crate::identities::{MasterPubkey, SelfSigningPubkey, UserSigningPubkey};

fn encode<T: AsRef<[u8]>>(input: T) -> String {
    encode_config(input, URL_SAFE_NO_PAD)
}

fn decode<T: AsRef<[u8]>>(input: T) -> Result<Vec<u8>, DecodeError> {
    decode_config(input, URL_SAFE_NO_PAD)
}

#[derive(Clone)]
pub struct Signing {
    inner: Arc<Mutex<OlmPkSigning>>,
    seed: Arc<Zeroizing<Vec<u8>>>,
    public_key: PublicSigningKey,
}

impl std::fmt::Debug for Signing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signing")
            .field("public_key", &self.public_key.as_str())
            .finish()
    }
}

impl PartialEq for Signing {
    fn eq(&self, other: &Signing) -> bool {
        self.seed == other.seed
    }
}

#[derive(Clone, PartialEq, Debug)]
struct MasterSigning {
    inner: Signing,
    public_key: MasterPubkey,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PickledMasterSigning {
    pickle: PickledSigning,
    public_key: CrossSigningKey,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PickledUserSigning {
    pickle: PickledSigning,
    public_key: CrossSigningKey,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PickledSelfSigning {
    pickle: PickledSigning,
    public_key: CrossSigningKey,
}

impl MasterSigning {
    async fn pickle(&self, pickle_key: &[u8]) -> PickledMasterSigning {
        let pickle = self.inner.pickle(pickle_key).await;
        let public_key = self.public_key.clone().into();
        PickledMasterSigning { pickle, public_key }
    }

    fn from_pickle(pickle: PickledMasterSigning, pickle_key: &[u8]) -> Self {
        let inner = Signing::from_pickle(pickle.pickle, pickle_key);

        Self {
            inner,
            public_key: pickle.public_key.into(),
        }
    }

    async fn sign_subkey<'a>(&self, subkey: &mut CrossSigningKey) {
        // TODO create a borrowed version of a cross singing key.
        let subkey_wihtout_signatures = CrossSigningKey {
            user_id: subkey.user_id.clone(),
            keys: subkey.keys.clone(),
            usage: subkey.usage.clone(),
            signatures: BTreeMap::new(),
        };

        let message = cjson::to_string(&subkey_wihtout_signatures)
            .expect("Can't serialize cross signing subkey");
        let signature = self.inner.sign(&message).await;

        subkey
            .signatures
            .entry(self.public_key.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(
                format!("ed25519:{}", self.inner.public_key().as_str()),
                signature.0,
            );
    }
}

impl UserSigning {
    async fn pickle(&self, pickle_key: &[u8]) -> PickledUserSigning {
        let pickle = self.inner.pickle(pickle_key).await;
        let public_key = self.public_key.clone().into();
        PickledUserSigning { pickle, public_key }
    }

    fn from_pickle(pickle: PickledUserSigning, pickle_key: &[u8]) -> Self {
        let inner = Signing::from_pickle(pickle.pickle, pickle_key);

        Self {
            inner,
            public_key: pickle.public_key.into(),
        }
    }
}

impl SelfSigning {
    async fn pickle(&self, pickle_key: &[u8]) -> PickledSelfSigning {
        let pickle = self.inner.pickle(pickle_key).await;
        let public_key = self.public_key.clone().into();
        PickledSelfSigning { pickle, public_key }
    }

    fn from_pickle(pickle: PickledSelfSigning, pickle_key: &[u8]) -> Self {
        let inner = Signing::from_pickle(pickle.pickle, pickle_key);

        Self {
            inner,
            public_key: pickle.public_key.into(),
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
struct SelfSigning {
    inner: Signing,
    public_key: SelfSigningPubkey,
}

#[derive(Clone, PartialEq, Debug)]
struct UserSigning {
    inner: Signing,
    public_key: UserSigningPubkey,
}

#[derive(Clone, Debug)]
pub struct PrivateCrossSigningIdentity {
    user_id: Arc<UserId>,
    shared: Arc<AtomicBool>,
    master_key: Arc<Mutex<Option<MasterSigning>>>,
    user_signing_key: Arc<Mutex<Option<UserSigning>>>,
    self_signing_key: Arc<Mutex<Option<SelfSigning>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickledCrossSigningIdentity {
    user_id: UserId,
    shared: bool,

    master_key: Option<PickledMasterSigning>,
    user_signing_key: Option<PickledUserSigning>,
    self_signing_key: Option<PickledSelfSigning>,
}

#[derive(Debug, Clone)]
pub struct Signature(String);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PickledSigning(String);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InnerPickle {
    version: u8,
    nonce: String,
    ciphertext: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PublicSigningKey(Arc<String>);

impl Signature {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl PickledSigning {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl PublicSigningKey {
    fn as_str(&self) -> &str {
        &self.0
    }

    #[allow(clippy::inherent_to_string)]
    fn to_string(&self) -> String {
        self.0.to_string()
    }
}

impl Signing {
    fn new() -> Self {
        let seed = OlmPkSigning::generate_seed();
        Self::from_seed(seed)
    }

    fn from_seed(seed: Vec<u8>) -> Self {
        let inner = OlmPkSigning::new(seed.clone()).expect("Unable to create pk signing object");
        let public_key = PublicSigningKey(Arc::new(inner.public_key().to_owned()));

        Signing {
            inner: Arc::new(Mutex::new(inner)),
            seed: Arc::new(Zeroizing::from(seed)),
            public_key,
        }
    }

    fn from_pickle(pickle: PickledSigning, pickle_key: &[u8]) -> Self {
        let pickled: InnerPickle = serde_json::from_str(pickle.as_str()).unwrap();

        let key = GenericArray::from_slice(pickle_key);
        let cipher = Aes256Gcm::new(key);

        let nonce = decode(pickled.nonce).unwrap();
        let nonce = GenericArray::from_slice(&nonce);
        let ciphertext = &decode(pickled.ciphertext).unwrap();

        let seed = cipher
            .decrypt(&nonce, ciphertext.as_slice())
            .expect("Can't decrypt pickle");

        Self::from_seed(seed)
    }

    async fn pickle(&self, pickle_key: &[u8]) -> PickledSigning {
        let key = GenericArray::from_slice(pickle_key);
        let cipher = Aes256Gcm::new(key);
        let nonce = GenericArray::from_slice(b"unique nonce");
        let ciphertext = cipher
            .encrypt(nonce, self.seed.as_slice())
            .expect("Can't encrypt signing pickle");

        let ciphertext = encode(ciphertext);

        let pickle = InnerPickle {
            version: 1,
            nonce: encode(nonce.as_slice()),
            ciphertext,
        };

        PickledSigning(serde_json::to_string(&pickle).expect("Can't encode pickled signing"))
    }

    fn public_key(&self) -> &PublicSigningKey {
        &self.public_key
    }

    fn cross_signing_key(&self, user_id: UserId, usage: KeyUsage) -> CrossSigningKey {
        let mut keys = BTreeMap::new();

        keys.insert(
            format!("ed25519:{}", self.public_key().as_str()),
            self.public_key().to_string(),
        );

        CrossSigningKey {
            user_id,
            usage: vec![usage],
            keys,
            signatures: BTreeMap::new(),
        }
    }

    async fn verify(&self, message: &str, signature: &Signature) -> Result<bool, OlmUtilityError> {
        let utility = OlmUtility::new();
        utility.ed25519_verify(self.public_key.as_str(), message, signature.as_str())
    }

    async fn sign(&self, message: &str) -> Signature {
        Signature(self.inner.lock().await.sign(message))
    }
}

impl PrivateCrossSigningIdentity {
    async fn new(user_id: UserId) -> Self {
        let master = Signing::new();

        let public_key = master.cross_signing_key(user_id.clone(), KeyUsage::Master);
        let master = MasterSigning {
            inner: master,
            public_key: public_key.into(),
        };

        let user = Signing::new();
        let mut public_key = user.cross_signing_key(user_id.clone(), KeyUsage::UserSigning);
        master.sign_subkey(&mut public_key).await;

        let user = UserSigning {
            inner: user,
            public_key: public_key.into(),
        };

        let self_signing = Signing::new();
        let mut public_key = self_signing.cross_signing_key(user_id.clone(), KeyUsage::SelfSigning);
        master.sign_subkey(&mut public_key).await;

        let self_signing = SelfSigning {
            inner: self_signing,
            public_key: public_key.into(),
        };

        Self {
            user_id: Arc::new(user_id),
            shared: Arc::new(AtomicBool::new(false)),
            master_key: Arc::new(Mutex::new(Some(master))),
            self_signing_key: Arc::new(Mutex::new(Some(self_signing))),
            user_signing_key: Arc::new(Mutex::new(Some(user))),
        }
    }

    fn mark_as_shared(&self) {
        self.shared.store(true, Ordering::SeqCst)
    }

    pub fn shared(&self) -> bool {
        self.shared.load(Ordering::SeqCst)
    }

    async fn pickle(&self, pickle_key: &[u8]) -> PickledCrossSigningIdentity {
        let master_key = if let Some(m) = self.master_key.lock().await.as_ref() {
            Some(m.pickle(pickle_key).await)
        } else {
            None
        };

        let self_signing_key = if let Some(m) = self.self_signing_key.lock().await.as_ref() {
            Some(m.pickle(pickle_key).await)
        } else {
            None
        };

        let user_signing_key = if let Some(m) = self.user_signing_key.lock().await.as_ref() {
            Some(m.pickle(pickle_key).await)
        } else {
            None
        };

        PickledCrossSigningIdentity {
            user_id: self.user_id.as_ref().to_owned(),
            shared: self.shared(),
            master_key,
            self_signing_key,
            user_signing_key,
        }
    }

    async fn from_pickle(pickle: PickledCrossSigningIdentity, pickle_key: &[u8]) -> Self {
        let master = if let Some(m) = pickle.master_key {
            Some(MasterSigning::from_pickle(m, pickle_key))
        } else {
            None
        };

        let self_signing = if let Some(s) = pickle.self_signing_key {
            Some(SelfSigning::from_pickle(s, pickle_key))
        } else {
            None
        };

        let user_signing = if let Some(u) = pickle.user_signing_key {
            Some(UserSigning::from_pickle(u, pickle_key))
        } else {
            None
        };

        Self {
            user_id: Arc::new(pickle.user_id),
            shared: Arc::new(AtomicBool::from(pickle.shared)),
            master_key: Arc::new(Mutex::new(master)),
            self_signing_key: Arc::new(Mutex::new(self_signing)),
            user_signing_key: Arc::new(Mutex::new(user_signing)),
        }
    }

    async fn as_upload_request(&self) -> UploadRequest<'_> {
        let master_key = self
            .master_key
            .lock()
            .await
            .as_ref()
            .cloned()
            .map(|k| k.public_key.into());

        let user_signing_key = self
            .user_signing_key
            .lock()
            .await
            .as_ref()
            .cloned()
            .map(|k| k.public_key.into());

        let self_signing_key = self
            .self_signing_key
            .lock()
            .await
            .as_ref()
            .cloned()
            .map(|k| k.public_key.into());

        UploadRequest {
            auth: None,
            master_key,
            user_signing_key,
            self_signing_key,
        }
    }
}

#[cfg(test)]
mod test {
    use super::{PrivateCrossSigningIdentity, Signing};

    use matrix_sdk_common::identifiers::{user_id, UserId};
    use matrix_sdk_test::async_test;

    fn user_id() -> UserId {
        user_id!("@example:localhost")
    }

    fn pickle_key() -> &'static [u8] {
        &[0u8; 32]
    }

    #[test]
    fn signing_creation() {
        let signing = Signing::new();
        assert!(!signing.public_key().as_str().is_empty());
    }

    #[async_test]
    async fn signature_verification() {
        let signing = Signing::new();

        let message = "Hello world";

        let signature = signing.sign(message).await;
        assert!(signing.verify(message, &signature).await.is_ok());
    }

    #[async_test]
    async fn pickling_signing() {
        let signing = Signing::new();
        let pickled = signing.pickle(pickle_key()).await;

        let unpickled = Signing::from_pickle(pickled, pickle_key());

        assert_eq!(signing.public_key(), unpickled.public_key());
    }

    #[async_test]
    async fn private_identity_creation() {
        let identity = PrivateCrossSigningIdentity::new(user_id()).await;

        let master_key = identity.master_key.lock().await;
        let master_key = master_key.as_ref().unwrap();

        assert!(master_key
            .public_key
            .verify_subkey(
                &identity
                    .self_signing_key
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .public_key,
            )
            .is_ok());

        assert!(master_key
            .public_key
            .verify_subkey(
                &identity
                    .user_signing_key
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .public_key,
            )
            .is_ok());
    }

    #[async_test]
    async fn identity_pickling() {
        let identity = PrivateCrossSigningIdentity::new(user_id()).await;

        let pickled = identity.pickle(pickle_key()).await;

        let unpickled = PrivateCrossSigningIdentity::from_pickle(pickled, pickle_key()).await;

        assert_eq!(identity.user_id, unpickled.user_id);
        assert_eq!(
            &*identity.master_key.lock().await,
            &*unpickled.master_key.lock().await
        );
        assert_eq!(
            &*identity.user_signing_key.lock().await,
            &*unpickled.user_signing_key.lock().await
        );
        assert_eq!(
            &*identity.self_signing_key.lock().await,
            &*unpickled.self_signing_key.lock().await
        );
    }
}
