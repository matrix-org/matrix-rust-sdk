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

#![allow(dead_code, missing_docs)]

mod pk_signing;

use matrix_sdk_common::encryption::DeviceKeys;
use serde::{Deserialize, Serialize};
use serde_json::Error as JsonError;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use matrix_sdk_common::{
    api::r0::keys::{upload_signatures::Request as SignatureUploadRequest, KeyUsage},
    identifiers::UserId,
    locks::Mutex,
};

use crate::{error::SignatureError, requests::UploadSigningKeysRequest};

use crate::ReadOnlyAccount;

use pk_signing::{MasterSigning, PickledSignings, SelfSigning, Signing, SigningError, UserSigning};

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
    pub user_id: UserId,
    pub shared: bool,
    pub pickle: String,
}

impl PrivateCrossSigningIdentity {
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    pub async fn is_empty(&self) -> bool {
        let has_master = self.master_key.lock().await.is_some();
        let has_user = self.user_signing_key.lock().await.is_some();
        let has_self = self.self_signing_key.lock().await.is_some();

        !(has_master && has_user && has_self)
    }

    pub(crate) fn empty(user_id: UserId) -> Self {
        Self {
            user_id: Arc::new(user_id),
            shared: Arc::new(AtomicBool::new(false)),
            master_key: Arc::new(Mutex::new(None)),
            self_signing_key: Arc::new(Mutex::new(None)),
            user_signing_key: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) async fn sign_device(
        &self,
        mut device_keys: DeviceKeys,
    ) -> Result<SignatureUploadRequest, SignatureError> {
        self.self_signing_key
            .lock()
            .await
            .as_ref()
            .ok_or(SignatureError::MissingSigningKey)?
            .sign_device(&mut device_keys)
            .await?;

        let mut signed_keys = BTreeMap::new();
        signed_keys
            .entry((&*self.user_id).to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(
                device_keys.device_id.to_string(),
                serde_json::to_value(device_keys)?,
            );

        Ok(SignatureUploadRequest { signed_keys })
    }

    pub(crate) async fn new_with_account(
        account: &ReadOnlyAccount,
    ) -> (Self, SignatureUploadRequest) {
        let master = Signing::new();

        let mut public_key =
            master.cross_signing_key(account.user_id().to_owned(), KeyUsage::Master);
        let signature = account
            .sign_json(
                serde_json::to_value(&public_key)
                    .expect("Can't convert own public master key to json"),
            )
            .await
            .expect("Can't sign own public master key");
        public_key
            .signatures
            .entry(account.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(format!("ed25519:{}", account.device_id()), signature);

        let master = MasterSigning {
            inner: master,
            public_key: public_key.into(),
        };

        let identity = Self::new_helper(account.user_id(), master).await;
        let device_keys = account.unsigned_device_keys();
        let request = identity
            .sign_device(device_keys)
            .await
            .expect("Can't sign own device with new cross signign keys");

        (identity, request)
    }

    async fn new_helper(user_id: &UserId, master: MasterSigning) -> Self {
        let user = Signing::new();
        let mut public_key = user.cross_signing_key(user_id.to_owned(), KeyUsage::UserSigning);
        master.sign_subkey(&mut public_key).await;

        let user = UserSigning {
            inner: user,
            public_key: public_key.into(),
        };

        let self_signing = Signing::new();
        let mut public_key =
            self_signing.cross_signing_key(user_id.to_owned(), KeyUsage::SelfSigning);
        master.sign_subkey(&mut public_key).await;

        let self_signing = SelfSigning {
            inner: self_signing,
            public_key: public_key.into(),
        };

        Self {
            user_id: Arc::new(user_id.to_owned()),
            shared: Arc::new(AtomicBool::new(false)),
            master_key: Arc::new(Mutex::new(Some(master))),
            self_signing_key: Arc::new(Mutex::new(Some(self_signing))),
            user_signing_key: Arc::new(Mutex::new(Some(user))),
        }
    }

    pub(crate) async fn new(user_id: UserId) -> Self {
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

    pub fn mark_as_shared(&self) {
        self.shared.store(true, Ordering::SeqCst)
    }

    pub fn shared(&self) -> bool {
        self.shared.load(Ordering::SeqCst)
    }

    pub async fn pickle(
        &self,
        pickle_key: &[u8],
    ) -> Result<PickledCrossSigningIdentity, JsonError> {
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

        let pickle = PickledSignings {
            master_key,
            user_signing_key,
            self_signing_key,
        };

        let pickle = serde_json::to_string(&pickle)?;

        Ok(PickledCrossSigningIdentity {
            user_id: self.user_id.as_ref().to_owned(),
            shared: self.shared(),
            pickle,
        })
    }

    /// Restore the private cross signing identity from a pickle.
    ///
    /// # Panic
    ///
    /// Panics if the pickle_key isn't 32 bytes long.
    pub async fn from_pickle(
        pickle: PickledCrossSigningIdentity,
        pickle_key: &[u8],
    ) -> Result<Self, SigningError> {
        let signings: PickledSignings = serde_json::from_str(&pickle.pickle)?;

        let master = if let Some(m) = signings.master_key {
            Some(MasterSigning::from_pickle(m, pickle_key)?)
        } else {
            None
        };

        let self_signing = if let Some(s) = signings.self_signing_key {
            Some(SelfSigning::from_pickle(s, pickle_key)?)
        } else {
            None
        };

        let user_signing = if let Some(u) = signings.user_signing_key {
            Some(UserSigning::from_pickle(u, pickle_key)?)
        } else {
            None
        };

        Ok(Self {
            user_id: Arc::new(pickle.user_id),
            shared: Arc::new(AtomicBool::from(pickle.shared)),
            master_key: Arc::new(Mutex::new(master)),
            self_signing_key: Arc::new(Mutex::new(self_signing)),
            user_signing_key: Arc::new(Mutex::new(user_signing)),
        })
    }

    pub(crate) async fn as_upload_request(&self) -> UploadSigningKeysRequest {
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

        UploadSigningKeysRequest {
            master_key,
            user_signing_key,
            self_signing_key,
        }
    }
}

#[cfg(test)]
mod test {
    use crate::olm::ReadOnlyAccount;

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

        let unpickled = Signing::from_pickle(pickled, pickle_key()).unwrap();

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

        let pickled = identity.pickle(pickle_key()).await.unwrap();

        let unpickled = PrivateCrossSigningIdentity::from_pickle(pickled, pickle_key())
            .await
            .unwrap();

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

    #[async_test]
    async fn private_identity_signed_by_accound() {
        let account = ReadOnlyAccount::new(&user_id(), "DEVICEID".into());
        let (identity, _) = PrivateCrossSigningIdentity::new_with_account(&account).await;
        let master = identity.master_key.lock().await;
        let master = master.as_ref().unwrap();

        assert!(!master.public_key.signatures().is_empty());
    }
}
