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

use std::{
    collections::{btree_map::Iter, BTreeMap},
    convert::TryFrom,
    ops::Deref,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use ruma::{
    api::client::keys::upload_signatures::v3::Request as SignatureUploadRequest,
    encryption::{CrossSigningKey, DeviceKeys, KeyUsage},
    events::{
        key::verification::VerificationMethod, room::message::KeyVerificationRequestEventContent,
    },
    DeviceId, DeviceKeyId, EventId, RoomId, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::to_value;
use tracing::error;

use super::{atomic_bool_deserializer, atomic_bool_serializer};
use crate::{
    error::SignatureError,
    olm::Utility,
    store::{Changes, IdentityChanges},
    verification::VerificationMachine,
    CryptoStoreError, OutgoingVerificationRequest, ReadOnlyDevice, VerificationRequest,
};

/// Enum over the different user identity types we can have.
#[derive(Debug, Clone)]
pub enum UserIdentities {
    /// Our own user identity.
    Own(OwnUserIdentity),
    /// An identity belonging to another user.
    Other(UserIdentity),
}

impl UserIdentities {
    /// Destructure the enum into an `OwnUserIdentity` if it's of the correct
    /// type.
    pub fn own(self) -> Option<OwnUserIdentity> {
        match self {
            Self::Own(i) => Some(i),
            _ => None,
        }
    }

    /// Destructure the enum into an `UserIdentity` if it's of the correct
    /// type.
    pub fn other(self) -> Option<UserIdentity> {
        match self {
            Self::Other(i) => Some(i),
            _ => None,
        }
    }
}

impl From<OwnUserIdentity> for UserIdentities {
    fn from(i: OwnUserIdentity) -> Self {
        Self::Own(i)
    }
}

impl From<UserIdentity> for UserIdentities {
    fn from(i: UserIdentity) -> Self {
        Self::Other(i)
    }
}

/// Struct representing a cross signing identity of a user.
///
/// This is the user identity of a user that isn't our own. Other users will
/// only contain a master key and a self signing key, meaning that only device
/// signatures can be checked with this identity.
///
/// This struct wraps a read-only version of the struct and allows verifications
/// to be requested to verify our own device with the user identity.
#[derive(Debug, Clone)]
pub struct OwnUserIdentity {
    pub(crate) inner: ReadOnlyOwnUserIdentity,
    pub(crate) verification_machine: VerificationMachine,
}

impl Deref for OwnUserIdentity {
    type Target = ReadOnlyOwnUserIdentity;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl OwnUserIdentity {
    /// Mark our user identity as verified.
    ///
    /// This will mark the identity locally as verified and sign it with our own
    /// device.
    ///
    /// Returns a signature upload request that needs to be sent out.
    pub async fn verify(&self) -> Result<SignatureUploadRequest, SignatureError> {
        self.mark_as_verified();

        let changes = Changes {
            identities: IdentityChanges { changed: vec![self.inner.clone().into()], new: vec![] },
            ..Default::default()
        };

        if let Err(e) = self.verification_machine.store.save_changes(changes).await {
            error!(error =? e, "Couldn't store our own user identity after marking it as verified");
        }

        self.verification_machine.store.account.sign_master_key(self.master_key.clone()).await
    }

    /// Send a verification request to our other devices.
    pub async fn request_verification(
        &self,
    ) -> Result<(VerificationRequest, OutgoingVerificationRequest), CryptoStoreError> {
        self.request_verification_helper(None).await
    }

    /// Send a verification request to our other devices while specifying our
    /// supported methods.
    ///
    /// # Arguments
    ///
    /// * `methods` - The verification methods that we're supporting.
    pub async fn request_verification_with_methods(
        &self,
        methods: Vec<VerificationMethod>,
    ) -> Result<(VerificationRequest, OutgoingVerificationRequest), CryptoStoreError> {
        self.request_verification_helper(Some(methods)).await
    }

    /// Does our user identity trust our own device, i.e. have we signed  our
    /// own device keys with our self-signing key.
    pub async fn trusts_our_own_device(&self) -> Result<bool, CryptoStoreError> {
        Ok(if let Some(signatures) = self.verification_machine.store.device_signatures().await? {
            let mut device_keys = self.verification_machine.store.account.device_keys().await;
            device_keys.signatures = signatures;

            self.inner.self_signing_key().verify_device_keys(device_keys).is_ok()
        } else {
            false
        })
    }

    async fn request_verification_helper(
        &self,
        methods: Option<Vec<VerificationMethod>>,
    ) -> Result<(VerificationRequest, OutgoingVerificationRequest), CryptoStoreError> {
        let devices: Vec<Box<DeviceId>> = self
            .verification_machine
            .store
            .get_user_devices(self.user_id())
            .await?
            .into_iter()
            .map(|(d, _)| d)
            .filter(|d| &**d != self.verification_machine.own_device_id())
            .collect();

        Ok(self
            .verification_machine
            .request_to_device_verification(self.user_id(), devices, methods)
            .await)
    }
}

/// Struct representing a cross signing identity of a user.
///
/// This is the user identity of a user that isn't our own. Other users will
/// only contain a master key and a self signing key, meaning that only device
/// signatures can be checked with this identity.
///
/// This struct wraps a read-only version of the struct and allows verifications
/// to be requested to verify our own device with the user identity.
#[derive(Debug, Clone)]
pub struct UserIdentity {
    pub(crate) inner: ReadOnlyUserIdentity,
    pub(crate) own_identity: Option<ReadOnlyOwnUserIdentity>,
    pub(crate) verification_machine: VerificationMachine,
}

impl Deref for UserIdentity {
    type Target = ReadOnlyUserIdentity;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl UserIdentity {
    /// Is this user identity verified.
    pub fn verified(&self) -> bool {
        self.own_identity
            .as_ref()
            .map(|o| o.is_identity_signed(&self.inner).is_ok())
            .unwrap_or(false)
    }

    /// Manually verify this user.
    ///
    /// This method will attempt to sign the user identity using our private
    /// cross signing key.
    ///
    /// This method fails if we don't have the private part of our user-signing
    /// key.
    ///
    /// Returns a request that needs to be sent out for the user to be marked
    /// as verified.
    pub async fn verify(&self) -> Result<SignatureUploadRequest, SignatureError> {
        if self.user_id() != self.verification_machine.own_user_id() {
            Ok(self
                .verification_machine
                .private_identity
                .lock()
                .await
                .sign_user(&self.inner)
                .await?)
        } else {
            Err(SignatureError::UserIdMismatch)
        }
    }

    /// Create a `VerificationRequest` object after the verification request
    /// content has been sent out.
    pub async fn request_verification(
        &self,
        room_id: &RoomId,
        request_event_id: &EventId,
        methods: Option<Vec<VerificationMethod>>,
    ) -> VerificationRequest {
        self.verification_machine
            .request_verification(&self.inner, room_id, request_event_id, methods)
            .await
    }

    /// Send a verification request to the given user.
    ///
    /// The returned content needs to be sent out into a DM room with the given
    /// user.
    ///
    /// After the content has been sent out a `VerificationRequest` can be
    /// started with the [`request_verification()`] method.
    ///
    /// [`request_verification()`]: #method.request_verification
    pub async fn verification_request_content(
        &self,
        methods: Option<Vec<VerificationMethod>>,
    ) -> KeyVerificationRequestEventContent {
        VerificationRequest::request(
            self.verification_machine.own_user_id(),
            self.verification_machine.own_device_id(),
            self.user_id(),
            methods,
        )
    }
}

/// Wrapper for a cross signing key marking it as the master key.
///
/// Master keys are used to sign other cross signing keys, the self signing and
/// user signing keys of an user will be signed by their master key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MasterPubkey(Arc<CrossSigningKey>);

/// Wrapper for a cross signing key marking it as a self signing key.
///
/// Self signing keys are used to sign the user's own devices.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfSigningPubkey(Arc<CrossSigningKey>);

/// Wrapper for a cross signing key marking it as a user signing key.
///
/// User signing keys are used to sign the master keys of other users.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSigningPubkey(Arc<CrossSigningKey>);

impl PartialEq for MasterPubkey {
    fn eq(&self, other: &MasterPubkey) -> bool {
        self.0.user_id == other.0.user_id && self.0.keys == other.0.keys
    }
}

impl PartialEq for SelfSigningPubkey {
    fn eq(&self, other: &SelfSigningPubkey) -> bool {
        self.0.user_id == other.0.user_id && self.0.keys == other.0.keys
    }
}

impl PartialEq for UserSigningPubkey {
    fn eq(&self, other: &UserSigningPubkey) -> bool {
        self.0.user_id == other.0.user_id && self.0.keys == other.0.keys
    }
}

impl From<CrossSigningKey> for MasterPubkey {
    fn from(key: CrossSigningKey) -> Self {
        Self(Arc::new(key))
    }
}

impl From<CrossSigningKey> for SelfSigningPubkey {
    fn from(key: CrossSigningKey) -> Self {
        Self(Arc::new(key))
    }
}

impl From<CrossSigningKey> for UserSigningPubkey {
    fn from(key: CrossSigningKey) -> Self {
        Self(Arc::new(key))
    }
}

#[allow(clippy::from_over_into)]
impl Into<CrossSigningKey> for MasterPubkey {
    fn into(self) -> CrossSigningKey {
        self.0.as_ref().clone()
    }
}

#[allow(clippy::from_over_into)]
impl Into<CrossSigningKey> for UserSigningPubkey {
    fn into(self) -> CrossSigningKey {
        self.0.as_ref().clone()
    }
}

#[allow(clippy::from_over_into)]
impl Into<CrossSigningKey> for SelfSigningPubkey {
    fn into(self) -> CrossSigningKey {
        self.0.as_ref().clone()
    }
}

impl AsRef<CrossSigningKey> for MasterPubkey {
    fn as_ref(&self) -> &CrossSigningKey {
        &self.0
    }
}

impl AsRef<CrossSigningKey> for SelfSigningPubkey {
    fn as_ref(&self) -> &CrossSigningKey {
        &self.0
    }
}

impl AsRef<CrossSigningKey> for UserSigningPubkey {
    fn as_ref(&self) -> &CrossSigningKey {
        &self.0
    }
}

impl From<&CrossSigningKey> for MasterPubkey {
    fn from(key: &CrossSigningKey) -> Self {
        Self(Arc::new(key.clone()))
    }
}

impl From<&CrossSigningKey> for SelfSigningPubkey {
    fn from(key: &CrossSigningKey) -> Self {
        Self(Arc::new(key.clone()))
    }
}

impl From<&CrossSigningKey> for UserSigningPubkey {
    fn from(key: &CrossSigningKey) -> Self {
        Self(Arc::new(key.clone()))
    }
}

impl<'a> From<&'a SelfSigningPubkey> for CrossSigningSubKeys<'a> {
    fn from(key: &'a SelfSigningPubkey) -> Self {
        CrossSigningSubKeys::SelfSigning(key)
    }
}

impl<'a> From<&'a UserSigningPubkey> for CrossSigningSubKeys<'a> {
    fn from(key: &'a UserSigningPubkey) -> Self {
        CrossSigningSubKeys::UserSigning(key)
    }
}

/// Enum over the cross signing sub-keys.
pub(crate) enum CrossSigningSubKeys<'a> {
    /// The self signing subkey.
    SelfSigning(&'a SelfSigningPubkey),
    /// The user signing subkey.
    UserSigning(&'a UserSigningPubkey),
}

impl<'a> CrossSigningSubKeys<'a> {
    /// Get the id of the user that owns this cross signing subkey.
    fn user_id(&self) -> &UserId {
        match self {
            CrossSigningSubKeys::SelfSigning(key) => &key.0.user_id,
            CrossSigningSubKeys::UserSigning(key) => &key.0.user_id,
        }
    }

    /// Get the `CrossSigningKey` from an sub-keys enum
    pub(crate) fn cross_signing_key(&self) -> &CrossSigningKey {
        match self {
            CrossSigningSubKeys::SelfSigning(key) => &key.0,
            CrossSigningSubKeys::UserSigning(key) => &key.0,
        }
    }
}

impl MasterPubkey {
    /// Get the user id of the master key's owner.
    pub fn user_id(&self) -> &UserId {
        &self.0.user_id
    }

    /// Get the keys map of containing the master keys.
    pub fn keys(&self) -> &BTreeMap<String, String> {
        &self.0.keys
    }

    /// Get the list of `KeyUsage` that is set for this key.
    pub fn usage(&self) -> &[KeyUsage] {
        &self.0.usage
    }

    /// Get the signatures map of this cross signing key.
    pub fn signatures(&self) -> &BTreeMap<Box<UserId>, BTreeMap<String, String>> {
        &self.0.signatures
    }

    /// Get the master key with the given key id.
    ///
    /// # Arguments
    ///
    /// * `key_id` - The id of the key that should be fetched.
    pub fn get_key(&self, key_id: &DeviceKeyId) -> Option<&str> {
        self.0.keys.get(key_id.as_str()).map(|k| k.as_str())
    }

    /// Get the first available master key.
    ///
    /// There's usually only a single master key so this will usually fetch the
    /// only key.
    pub fn get_first_key(&self) -> Option<&str> {
        self.0.keys.values().map(|k| k.as_str()).next()
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
        let (key_id, key) = self.0.keys.iter().next().ok_or(SignatureError::MissingSigningKey)?;

        let key_id = <&DeviceKeyId>::try_from(key_id.as_str())?;

        // FIXME `KeyUsage is missing PartialEq.
        // if self.0.usage.contains(&KeyUsage::Master) {
        //     return Err(SignatureError::MissingSigningKey);
        // }
        let subkey: CrossSigningSubKeys = subkey.into();

        if self.0.user_id != subkey.user_id() {
            return Err(SignatureError::UserIdMismatch);
        }

        let utility = Utility::new();
        utility.verify_json(
            &self.0.user_id,
            key_id,
            key,
            &mut to_value(subkey.cross_signing_key()).map_err(|_| SignatureError::NotAnObject)?,
        )
    }
}

impl<'a> IntoIterator for &'a MasterPubkey {
    type Item = (&'a String, &'a String);
    type IntoIter = Iter<'a, String, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.keys().iter()
    }
}

impl UserSigningPubkey {
    /// Get the user id of the user signing key's owner.
    pub fn user_id(&self) -> &UserId {
        &self.0.user_id
    }

    /// Get the keys map of containing the user signing keys.
    pub fn keys(&self) -> &BTreeMap<String, String> {
        &self.0.keys
    }

    /// Check if the given master key is signed by this user signing key.
    ///
    /// # Arguments
    ///
    /// * `master_key` - The master key that should be checked for a valid
    /// signature.
    ///
    /// Returns an empty result if the signature check succeeded, otherwise a
    /// SignatureError indicating why the check failed.
    pub(crate) fn verify_master_key(
        &self,
        master_key: &MasterPubkey,
    ) -> Result<(), SignatureError> {
        let (key_id, key) = self.0.keys.iter().next().ok_or(SignatureError::MissingSigningKey)?;

        // TODO check that the usage is OK.

        let utility = Utility::new();
        utility.verify_json(
            &self.0.user_id,
            key_id.as_str().try_into()?,
            key,
            &mut to_value(&*master_key.0).map_err(|_| SignatureError::NotAnObject)?,
        )
    }
}

impl<'a> IntoIterator for &'a UserSigningPubkey {
    type Item = (&'a String, &'a String);
    type IntoIter = Iter<'a, String, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.keys().iter()
    }
}

impl SelfSigningPubkey {
    /// Get the user id of the self signing key's owner.
    pub fn user_id(&self) -> &UserId {
        &self.0.user_id
    }

    /// Get the keys map of containing the self signing keys.
    pub fn keys(&self) -> &BTreeMap<String, String> {
        &self.0.keys
    }

    pub(crate) fn verify_device_keys(&self, device_keys: DeviceKeys) -> Result<(), SignatureError> {
        let (key_id, key) = self.0.keys.iter().next().ok_or(SignatureError::MissingSigningKey)?;
        // TODO check that the usage is OK.

        let mut device = to_value(device_keys)?;

        let utility = Utility::new();

        utility.verify_json(&self.0.user_id, key_id.as_str().try_into()?, key, &mut device)
    }

    /// Check if the given device is signed by this self signing key.
    ///
    /// # Arguments
    ///
    /// * `device` - The device that should be checked for a valid signature.
    ///
    /// Returns an empty result if the signature check succeeded, otherwise a
    /// SignatureError indicating why the check failed.
    pub(crate) fn verify_device(&self, device: &ReadOnlyDevice) -> Result<(), SignatureError> {
        self.verify_device_keys(device.as_device_keys().to_owned())
    }
}

impl<'a> IntoIterator for &'a SelfSigningPubkey {
    type Item = (&'a String, &'a String);
    type IntoIter = Iter<'a, String, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.keys().iter()
    }
}

/// Enum over the different user identity types we can have.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReadOnlyUserIdentities {
    /// Our own user identity.
    Own(ReadOnlyOwnUserIdentity),
    /// Identities of other users.
    Other(ReadOnlyUserIdentity),
}

impl From<ReadOnlyOwnUserIdentity> for ReadOnlyUserIdentities {
    fn from(identity: ReadOnlyOwnUserIdentity) -> Self {
        ReadOnlyUserIdentities::Own(identity)
    }
}

impl From<ReadOnlyUserIdentity> for ReadOnlyUserIdentities {
    fn from(identity: ReadOnlyUserIdentity) -> Self {
        ReadOnlyUserIdentities::Other(identity)
    }
}

impl ReadOnlyUserIdentities {
    /// The unique user id of this identity.
    pub fn user_id(&self) -> &UserId {
        match self {
            ReadOnlyUserIdentities::Own(i) => i.user_id(),
            ReadOnlyUserIdentities::Other(i) => i.user_id(),
        }
    }

    /// Get the master key of the identity.
    pub fn master_key(&self) -> &MasterPubkey {
        match self {
            ReadOnlyUserIdentities::Own(i) => i.master_key(),
            ReadOnlyUserIdentities::Other(i) => i.master_key(),
        }
    }

    /// Get the self-signing key of the identity.
    pub fn self_signing_key(&self) -> &SelfSigningPubkey {
        match self {
            ReadOnlyUserIdentities::Own(i) => &i.self_signing_key,
            ReadOnlyUserIdentities::Other(i) => &i.self_signing_key,
        }
    }

    /// Get the user-signing key of the identity, this is only present for our
    /// own user identity..
    pub fn user_signing_key(&self) -> Option<&UserSigningPubkey> {
        match self {
            ReadOnlyUserIdentities::Own(i) => Some(&i.user_signing_key),
            ReadOnlyUserIdentities::Other(_) => None,
        }
    }

    /// Destructure the enum into an `ReadOnlyOwnUserIdentity` if it's of the
    /// correct type.
    pub fn own(&self) -> Option<&ReadOnlyOwnUserIdentity> {
        match self {
            ReadOnlyUserIdentities::Own(i) => Some(i),
            _ => None,
        }
    }

    pub(crate) fn into_own(self) -> Option<ReadOnlyOwnUserIdentity> {
        match self {
            ReadOnlyUserIdentities::Own(i) => Some(i),
            _ => None,
        }
    }

    /// Destructure the enum into an `UserIdentity` if it's of the correct
    /// type.
    pub fn other(&self) -> Option<&ReadOnlyUserIdentity> {
        match self {
            ReadOnlyUserIdentities::Other(i) => Some(i),
            _ => None,
        }
    }
}

impl PartialEq for ReadOnlyUserIdentities {
    fn eq(&self, other: &ReadOnlyUserIdentities) -> bool {
        self.user_id() == other.user_id()
    }
}

/// Struct representing a cross signing identity of a user.
///
/// This is the user identity of a user that isn't our own. Other users will
/// only contain a master key and a self signing key, meaning that only device
/// signatures can be checked with this identity.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReadOnlyUserIdentity {
    user_id: Arc<UserId>,
    pub(crate) master_key: MasterPubkey,
    self_signing_key: SelfSigningPubkey,
}

impl ReadOnlyUserIdentity {
    /// Create a new user identity with the given master and self signing key.
    ///
    /// # Arguments
    ///
    /// * `master_key` - The master key of the user identity.
    ///
    /// * `self signing key` - The self signing key of user identity.
    ///
    /// Returns a `SignatureError` if the self signing key fails to be correctly
    /// verified by the given master key.
    pub(crate) fn new(
        master_key: MasterPubkey,
        self_signing_key: SelfSigningPubkey,
    ) -> Result<Self, SignatureError> {
        master_key.verify_subkey(&self_signing_key)?;

        Ok(Self { user_id: master_key.0.user_id.clone().into(), master_key, self_signing_key })
    }

    #[cfg(test)]
    pub(crate) async fn from_private(identity: &crate::olm::PrivateCrossSigningIdentity) -> Self {
        let master_key = identity.master_key.lock().await.as_ref().unwrap().public_key.clone();
        let self_signing_key =
            identity.self_signing_key.lock().await.as_ref().unwrap().public_key.clone();

        Self { user_id: identity.user_id().into(), master_key, self_signing_key }
    }

    /// Get the user id of this identity.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// Get the public master key of the identity.
    pub fn master_key(&self) -> &MasterPubkey {
        &self.master_key
    }

    /// Get the public self-signing key of the identity.
    pub fn self_signing_key(&self) -> &SelfSigningPubkey {
        &self.self_signing_key
    }

    /// Update the identity with a new master key and self signing key.
    ///
    /// # Arguments
    ///
    /// * `master_key` - The new master key of the user identity.
    ///
    /// * `self_signing_key` - The new self signing key of user identity.
    ///
    /// Returns a `SignatureError` if we failed to update the identity.
    pub(crate) fn update(
        &mut self,
        master_key: MasterPubkey,
        self_signing_key: SelfSigningPubkey,
    ) -> Result<(), SignatureError> {
        master_key.verify_subkey(&self_signing_key)?;

        self.master_key = master_key;
        self.self_signing_key = self_signing_key;

        Ok(())
    }

    /// Check if the given device has been signed by this identity.
    ///
    /// The user_id of the user identity and the user_id of the device need to
    /// match for the signature check to succeed as we don't trust users to sign
    /// devices of other users.
    ///
    /// # Arguments
    ///
    /// * `device` - The device that should be checked for a valid signature.
    ///
    /// Returns an empty result if the signature check succeeded, otherwise a
    /// SignatureError indicating why the check failed.
    pub(crate) fn is_device_signed(&self, device: &ReadOnlyDevice) -> Result<(), SignatureError> {
        if self.user_id() != device.user_id() {
            return Err(SignatureError::UserIdMismatch);
        }

        self.self_signing_key.verify_device(device)
    }
}

/// Struct representing a cross signing identity of our own user.
///
/// This is the user identity of our own user. This user identity will contain a
/// master key, self signing key as well as a user signing key.
///
/// This identity can verify other identities as well as devices belonging to
/// the identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadOnlyOwnUserIdentity {
    user_id: Arc<UserId>,
    master_key: MasterPubkey,
    self_signing_key: SelfSigningPubkey,
    user_signing_key: UserSigningPubkey,
    #[serde(
        serialize_with = "atomic_bool_serializer",
        deserialize_with = "atomic_bool_deserializer"
    )]
    verified: Arc<AtomicBool>,
}

impl ReadOnlyOwnUserIdentity {
    /// Create a new own user identity with the given master, self signing, and
    /// user signing key.
    ///
    /// # Arguments
    ///
    /// * `master_key` - The master key of the user identity.
    ///
    /// * `self_signing_key` - The self signing key of user identity.
    ///
    /// * `user_signing_key` - The user signing key of user identity.
    ///
    /// Returns a `SignatureError` if the self signing key fails to be correctly
    /// verified by the given master key.
    pub(crate) fn new(
        master_key: MasterPubkey,
        self_signing_key: SelfSigningPubkey,
        user_signing_key: UserSigningPubkey,
    ) -> Result<Self, SignatureError> {
        master_key.verify_subkey(&self_signing_key)?;
        master_key.verify_subkey(&user_signing_key)?;

        Ok(Self {
            user_id: master_key.0.user_id.clone().into(),
            master_key,
            self_signing_key,
            user_signing_key,
            verified: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Get the user id of this identity.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// Get the public master key of the identity.
    pub fn master_key(&self) -> &MasterPubkey {
        &self.master_key
    }

    /// Get the public self-signing key of the identity.
    pub fn self_signing_key(&self) -> &SelfSigningPubkey {
        &self.self_signing_key
    }

    /// Get the public user-signing key of the identity.
    pub fn user_signing_key(&self) -> &UserSigningPubkey {
        &self.user_signing_key
    }

    /// Check if the given identity has been signed by this identity.
    ///
    /// # Arguments
    ///
    /// * `identity` - The identity of another user that we want to check if
    /// it's has been signed.
    ///
    /// Returns an empty result if the signature check succeeded, otherwise a
    /// SignatureError indicating why the check failed.
    pub(crate) fn is_identity_signed(
        &self,
        identity: &ReadOnlyUserIdentity,
    ) -> Result<(), SignatureError> {
        self.user_signing_key.verify_master_key(&identity.master_key)
    }

    /// Check if the given device has been signed by this identity.
    ///
    /// Only devices of our own user should be checked with this method, if a
    /// device of a different user is given the signature check will always fail
    /// even if a valid signature exists.
    ///
    /// # Arguments
    ///
    /// * `device` - The device that should be checked for a valid signature.
    ///
    /// Returns an empty result if the signature check succeeded, otherwise a
    /// SignatureError indicating why the check failed.
    pub(crate) fn is_device_signed(&self, device: &ReadOnlyDevice) -> Result<(), SignatureError> {
        if self.user_id() != device.user_id() {
            return Err(SignatureError::UserIdMismatch);
        }

        self.self_signing_key.verify_device(device)
    }

    /// Mark our identity as verified.
    pub fn mark_as_verified(&self) {
        self.verified.store(true, Ordering::SeqCst)
    }

    /// Check if our identity is verified.
    pub fn is_verified(&self) -> bool {
        self.verified.load(Ordering::SeqCst)
    }

    /// Update the identity with a new master key and self signing key.
    ///
    /// Note: This will reset the verification state if the master keys differ.
    ///
    /// # Arguments
    ///
    /// * `master_key` - The new master key of the user identity.
    ///
    /// * `self_signing_key` - The new self signing key of user identity.
    ///
    /// * `user_signing_key` - The new user signing key of user identity.
    ///
    /// Returns a `SignatureError` if we failed to update the identity.
    pub(crate) fn update(
        &mut self,
        master_key: MasterPubkey,
        self_signing_key: SelfSigningPubkey,
        user_signing_key: UserSigningPubkey,
    ) -> Result<(), SignatureError> {
        master_key.verify_subkey(&self_signing_key)?;
        master_key.verify_subkey(&user_signing_key)?;

        self.self_signing_key = self_signing_key;
        self.user_signing_key = user_signing_key;

        if self.master_key != master_key {
            self.verified.store(false, Ordering::SeqCst)
        }

        self.master_key = master_key;

        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod test {
    use std::{convert::TryFrom, sync::Arc};

    use matrix_sdk_common::locks::Mutex;
    use matrix_sdk_test::async_test;
    use ruma::{api::client::keys::get_keys::v3::Response as KeyQueryResponse, user_id};

    use super::{ReadOnlyOwnUserIdentity, ReadOnlyUserIdentities, ReadOnlyUserIdentity};
    use crate::{
        identities::{
            manager::test::{other_key_query, own_key_query},
            Device, ReadOnlyDevice,
        },
        olm::{PrivateCrossSigningIdentity, ReadOnlyAccount},
        store::MemoryStore,
        verification::VerificationMachine,
    };

    fn device(response: &KeyQueryResponse) -> (ReadOnlyDevice, ReadOnlyDevice) {
        let mut devices = response.device_keys.values().next().unwrap().values();
        let first =
            ReadOnlyDevice::try_from(&devices.next().unwrap().deserialize().unwrap()).unwrap();
        let second =
            ReadOnlyDevice::try_from(&devices.next().unwrap().deserialize().unwrap()).unwrap();
        (first, second)
    }

    fn own_identity(response: &KeyQueryResponse) -> ReadOnlyOwnUserIdentity {
        let user_id = user_id!("@example:localhost");

        let master_key = response.master_keys.get(user_id).unwrap().deserialize().unwrap();
        let user_signing = response.user_signing_keys.get(user_id).unwrap().deserialize().unwrap();
        let self_signing = response.self_signing_keys.get(user_id).unwrap().deserialize().unwrap();

        ReadOnlyOwnUserIdentity::new(master_key.into(), self_signing.into(), user_signing.into())
            .unwrap()
    }

    pub(crate) fn get_own_identity() -> ReadOnlyOwnUserIdentity {
        own_identity(&own_key_query())
    }

    pub(crate) fn get_other_identity() -> ReadOnlyUserIdentity {
        let user_id = user_id!("@example2:localhost");
        let response = other_key_query();

        let master_key = response.master_keys.get(user_id).unwrap().deserialize().unwrap();
        let self_signing = response.self_signing_keys.get(user_id).unwrap().deserialize().unwrap();

        ReadOnlyUserIdentity::new(master_key.into(), self_signing.into()).unwrap()
    }

    #[test]
    fn own_identity_create() {
        let user_id = user_id!("@example:localhost");
        let response = own_key_query();

        let master_key = response.master_keys.get(user_id).unwrap().deserialize().unwrap();
        let user_signing = response.user_signing_keys.get(user_id).unwrap().deserialize().unwrap();
        let self_signing = response.self_signing_keys.get(user_id).unwrap().deserialize().unwrap();

        ReadOnlyOwnUserIdentity::new(master_key.into(), self_signing.into(), user_signing.into())
            .unwrap();
    }

    #[test]
    fn other_identity_create() {
        get_other_identity();
    }

    #[test]
    fn own_identity_check_signatures() {
        let response = own_key_query();
        let identity = get_own_identity();
        let (first, second) = device(&response);

        assert!(identity.is_device_signed(&first).is_err());
        assert!(identity.is_device_signed(&second).is_ok());

        let private_identity =
            Arc::new(Mutex::new(PrivateCrossSigningIdentity::empty(second.user_id().to_owned())));
        let verification_machine = VerificationMachine::new(
            ReadOnlyAccount::new(second.user_id(), second.device_id()),
            private_identity,
            Arc::new(MemoryStore::new()),
        );

        let first = Device {
            inner: first,
            verification_machine: verification_machine.clone(),
            own_identity: Some(identity.clone()),
            device_owner_identity: Some(ReadOnlyUserIdentities::Own(identity.clone())),
        };

        let second = Device {
            inner: second,
            verification_machine,
            own_identity: Some(identity.clone()),
            device_owner_identity: Some(ReadOnlyUserIdentities::Own(identity.clone())),
        };

        assert!(!second.is_locally_trusted());
        assert!(!second.is_cross_signing_trusted());

        assert!(!first.is_locally_trusted());
        assert!(!first.is_cross_signing_trusted());

        identity.mark_as_verified();
        assert!(second.verified());
        assert!(!first.verified());
    }

    #[async_test]
    async fn own_device_with_private_identity() {
        let response = own_key_query();
        let (_, device) = device(&response);

        let account = ReadOnlyAccount::new(device.user_id(), device.device_id());
        let (identity, _, _) = PrivateCrossSigningIdentity::new_with_account(&account).await;

        let id = Arc::new(Mutex::new(identity.clone()));

        let verification_machine = VerificationMachine::new(
            ReadOnlyAccount::new(device.user_id(), device.device_id()),
            id.clone(),
            Arc::new(MemoryStore::new()),
        );

        let public_identity = identity.to_public_identity().await.unwrap();

        let mut device = Device {
            inner: device,
            verification_machine: verification_machine.clone(),
            own_identity: Some(public_identity.clone()),
            device_owner_identity: Some(public_identity.clone().into()),
        };

        assert!(!device.verified());

        let mut device_keys = device.as_device_keys().to_owned();

        identity.sign_device_keys(&mut device_keys).await.unwrap();
        device.inner.update_device(&device_keys).expect("Couldn't update newly signed device keys");
        assert!(device.verified());
    }
}
