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

mod pk_signing;

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use matrix_sdk_common::locks::Mutex;
use pk_signing::{MasterSigning, PickledSignings, SelfSigning, Signing, SigningError, UserSigning};
use ruma::{
    api::client::r0::keys::{upload_signatures::Request as SignatureUploadRequest, KeyUsage},
    encryption::DeviceKeys,
    DeviceKeyAlgorithm, DeviceKeyId, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::Error as JsonError;

use crate::{
    error::SignatureError, identities::MasterPubkey, requests::UploadSigningKeysRequest,
    OwnUserIdentity, ReadOnlyAccount, ReadOnlyDevice, UserIdentity,
};

/// Private cross signing identity.
///
/// This object holds the private and public ed25519 key triplet that is used
/// for cross signing.
///
/// The object might be completely empty or have only some of the key pairs
/// available.
///
/// It can be used to sign devices or other identities.
#[derive(Clone, Debug)]
pub struct PrivateCrossSigningIdentity {
    user_id: Arc<UserId>,
    shared: Arc<AtomicBool>,
    pub(crate) master_key: Arc<Mutex<Option<MasterSigning>>>,
    pub(crate) user_signing_key: Arc<Mutex<Option<UserSigning>>>,
    pub(crate) self_signing_key: Arc<Mutex<Option<SelfSigning>>>,
}

/// The pickled version of a `PrivateCrossSigningIdentity`.
///
/// Can be used to store the identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickledCrossSigningIdentity {
    /// The user id of the identity owner.
    pub user_id: UserId,
    /// Have the public keys of the identity been shared.
    pub shared: bool,
    /// The encrypted pickle of the identity.
    pub pickle: String,
}

impl PrivateCrossSigningIdentity {
    /// Get the user id that this identity belongs to.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// Is the identity empty.
    ///
    /// An empty identity doesn't contain any private keys.
    ///
    /// It is usual for the identity not to contain the master key since the
    /// master key is only needed to sign the subkeys.
    ///
    /// An empty identity indicates that either no identity was created for this
    /// use or that another device created it and hasn't shared it yet with us.
    pub async fn is_empty(&self) -> bool {
        let has_master = self.master_key.lock().await.is_some();
        let has_user = self.user_signing_key.lock().await.is_some();
        let has_self = self.self_signing_key.lock().await.is_some();

        !(has_master && has_user && has_self)
    }

    /// Can we sign our own devices, i.e. do we have a self signing key.
    pub async fn can_sign_devices(&self) -> bool {
        self.self_signing_key.lock().await.is_some()
    }

    /// Do we have the master key.
    pub async fn has_master_key(&self) -> bool {
        self.master_key.lock().await.is_some()
    }

    /// Get the public part of the master key, if we have one.
    pub async fn master_public_key(&self) -> Option<MasterPubkey> {
        self.master_key.lock().await.as_ref().map(|m| m.public_key.to_owned())
    }

    /// Create a new empty identity.
    pub(crate) fn empty(user_id: UserId) -> Self {
        Self {
            user_id: Arc::new(user_id),
            shared: Arc::new(AtomicBool::new(false)),
            master_key: Arc::new(Mutex::new(None)),
            self_signing_key: Arc::new(Mutex::new(None)),
            user_signing_key: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) async fn to_public_identity(&self) -> Result<OwnUserIdentity, SignatureError> {
        let master = self
            .master_key
            .lock()
            .await
            .as_ref()
            .ok_or(SignatureError::MissingSigningKey)?
            .public_key
            .clone();
        let self_signing = self
            .self_signing_key
            .lock()
            .await
            .as_ref()
            .ok_or(SignatureError::MissingSigningKey)?
            .public_key
            .clone();
        let user_signing = self
            .user_signing_key
            .lock()
            .await
            .as_ref()
            .ok_or(SignatureError::MissingSigningKey)?
            .public_key
            .clone();
        let identity = OwnUserIdentity::new(master, self_signing, user_signing)?;
        identity.mark_as_verified();

        Ok(identity)
    }

    /// Sign the given public user identity with this private identity.
    pub(crate) async fn sign_user(
        &self,
        user_identity: &UserIdentity,
    ) -> Result<SignatureUploadRequest, SignatureError> {
        let signed_keys = self
            .user_signing_key
            .lock()
            .await
            .as_ref()
            .ok_or(SignatureError::MissingSigningKey)?
            .sign_user(user_identity)
            .await?;

        Ok(SignatureUploadRequest::new(signed_keys))
    }

    /// Sign the given device keys with this identity.
    pub(crate) async fn sign_device(
        &self,
        device: &ReadOnlyDevice,
    ) -> Result<SignatureUploadRequest, SignatureError> {
        let mut device_keys = device.as_device_keys();
        device_keys.signatures.clear();
        self.sign_device_keys(&mut device_keys).await
    }

    /// Sign an Olm account with this private identity.
    pub(crate) async fn sign_account(
        &self,
        account: &ReadOnlyAccount,
    ) -> Result<SignatureUploadRequest, SignatureError> {
        let mut device_keys = account.unsigned_device_keys();
        self.sign_device_keys(&mut device_keys).await
    }

    pub(crate) async fn sign_device_keys(
        &self,
        mut device_keys: &mut DeviceKeys,
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
            .insert(device_keys.device_id.to_string(), serde_json::to_value(device_keys)?);

        Ok(SignatureUploadRequest::new(signed_keys))
    }

    /// Create a new identity for the given Olm Account.
    ///
    /// Returns the new identity, the upload signing keys request and a
    /// signature upload request that contains the signature of the account
    /// signed by the self signing key.
    ///
    /// # Arguments
    ///
    /// * `account` - The Olm account that is creating the new identity. The
    /// account will sign the master key and the self signing key will sign the
    /// account.
    pub(crate) async fn new_with_account(
        account: &ReadOnlyAccount,
    ) -> (Self, UploadSigningKeysRequest, SignatureUploadRequest) {
        let master = Signing::new();

        let mut public_key =
            master.cross_signing_key(account.user_id().to_owned(), KeyUsage::Master);
        let signature = account
            .sign_json(
                serde_json::to_value(&public_key)
                    .expect("Can't convert own public master key to json"),
            )
            .await;

        public_key
            .signatures
            .entry(account.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, account.device_id())
                    .to_string(),
                signature,
            );

        let master = MasterSigning { inner: master, public_key: public_key.into() };

        let identity = Self::new_helper(account.user_id(), master).await;
        let signature_request = identity
            .sign_account(account)
            .await
            .expect("Can't sign own device with new cross signign keys");

        let request = identity.as_upload_request().await;

        (identity, request, signature_request)
    }

    async fn new_helper(user_id: &UserId, master: MasterSigning) -> Self {
        let user = Signing::new();
        let mut public_key = user.cross_signing_key(user_id.to_owned(), KeyUsage::UserSigning);
        master.sign_subkey(&mut public_key).await;

        let user = UserSigning { inner: user, public_key: public_key.into() };

        let self_signing = Signing::new();
        let mut public_key =
            self_signing.cross_signing_key(user_id.to_owned(), KeyUsage::SelfSigning);
        master.sign_subkey(&mut public_key).await;

        let self_signing = SelfSigning { inner: self_signing, public_key: public_key.into() };

        Self {
            user_id: Arc::new(user_id.to_owned()),
            shared: Arc::new(AtomicBool::new(false)),
            master_key: Arc::new(Mutex::new(Some(master))),
            self_signing_key: Arc::new(Mutex::new(Some(self_signing))),
            user_signing_key: Arc::new(Mutex::new(Some(user))),
        }
    }

    /// Create a new cross signing identity without signing the device that
    /// created it.
    #[cfg(test)]
    pub(crate) async fn new(user_id: UserId) -> Self {
        let master = Signing::new();

        let public_key = master.cross_signing_key(user_id.clone(), KeyUsage::Master);
        let master = MasterSigning { inner: master, public_key: public_key.into() };

        Self::new_helper(&user_id, master).await
    }

    /// Mark the identity as shared.
    pub fn mark_as_shared(&self) {
        self.shared.store(true, Ordering::SeqCst)
    }

    /// Has the identity been shared.
    ///
    /// A shared identity here means that the public keys of the identity have
    /// been uploaded to the server.
    pub fn shared(&self) -> bool {
        self.shared.load(Ordering::SeqCst)
    }

    /// Store the cross signing identity as a pickle.
    ///
    /// # Arguments
    ///
    /// * `pickle_key` - The key that should be used to encrypt the signing
    /// object, must be 32 bytes long.
    ///
    /// # Panics
    ///
    /// This will panic if the provided pickle key isn't 32 bytes long.
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

        let pickle = PickledSignings { master_key, user_signing_key, self_signing_key };

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

    /// Get the upload request that is needed to share the public keys of this
    /// identity.
    pub(crate) async fn as_upload_request(&self) -> UploadSigningKeysRequest {
        let master_key =
            self.master_key.lock().await.as_ref().cloned().map(|k| k.public_key.into());

        let user_signing_key =
            self.user_signing_key.lock().await.as_ref().cloned().map(|k| k.public_key.into());

        let self_signing_key =
            self.self_signing_key.lock().await.as_ref().cloned().map(|k| k.public_key.into());

        UploadSigningKeysRequest { master_key, self_signing_key, user_signing_key }
    }
}

#[cfg(test)]
mod test {
    use std::{collections::BTreeMap, sync::Arc};

    use matrix_sdk_test::async_test;
    use ruma::{api::client::r0::keys::CrossSigningKey, user_id, UserId};

    use super::{PrivateCrossSigningIdentity, Signing};
    use crate::{
        identities::{ReadOnlyDevice, UserIdentity},
        olm::ReadOnlyAccount,
    };

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
            .verify_subkey(&identity.self_signing_key.lock().await.as_ref().unwrap().public_key,)
            .is_ok());

        assert!(master_key
            .public_key
            .verify_subkey(&identity.user_signing_key.lock().await.as_ref().unwrap().public_key,)
            .is_ok());
    }

    #[async_test]
    async fn identity_pickling() {
        let identity = PrivateCrossSigningIdentity::new(user_id()).await;

        let pickled = identity.pickle(pickle_key()).await.unwrap();

        let unpickled =
            PrivateCrossSigningIdentity::from_pickle(pickled, pickle_key()).await.unwrap();

        assert_eq!(identity.user_id, unpickled.user_id);
        assert_eq!(&*identity.master_key.lock().await, &*unpickled.master_key.lock().await);
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
        let (identity, _, _) = PrivateCrossSigningIdentity::new_with_account(&account).await;
        let master = identity.master_key.lock().await;
        let master = master.as_ref().unwrap();

        assert!(!master.public_key.signatures().is_empty());
    }

    #[async_test]
    async fn sign_device() {
        let account = ReadOnlyAccount::new(&user_id(), "DEVICEID".into());
        let (identity, _, _) = PrivateCrossSigningIdentity::new_with_account(&account).await;

        let mut device = ReadOnlyDevice::from_account(&account).await;
        let self_signing = identity.self_signing_key.lock().await;
        let self_signing = self_signing.as_ref().unwrap();

        let mut device_keys = device.as_device_keys();
        self_signing.sign_device(&mut device_keys).await.unwrap();
        device.signatures = Arc::new(device_keys.signatures);

        let public_key = &self_signing.public_key;
        public_key.verify_device(&device).unwrap()
    }

    #[async_test]
    async fn sign_user_identity() {
        let account = ReadOnlyAccount::new(&user_id(), "DEVICEID".into());
        let (identity, _, _) = PrivateCrossSigningIdentity::new_with_account(&account).await;

        let bob_account = ReadOnlyAccount::new(&user_id!("@bob:localhost"), "DEVICEID".into());
        let (bob_private, _, _) = PrivateCrossSigningIdentity::new_with_account(&bob_account).await;
        let mut bob_public = UserIdentity::from_private(&bob_private).await;

        let user_signing = identity.user_signing_key.lock().await;
        let user_signing = user_signing.as_ref().unwrap();

        let signatures = user_signing.sign_user(&bob_public).await.unwrap();

        let (key_id, signature) = signatures
            .iter()
            .next()
            .unwrap()
            .1
            .iter()
            .next()
            .map(|(k, s)| (k.to_string(), serde_json::from_value(s.to_owned()).unwrap()))
            .unwrap();

        let mut master: CrossSigningKey = bob_public.master_key.as_ref().clone();
        master
            .signatures
            .entry(identity.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(key_id, signature);

        bob_public.master_key = master.into();

        user_signing.public_key.verify_master_key(bob_public.master_key()).unwrap();
    }
}
