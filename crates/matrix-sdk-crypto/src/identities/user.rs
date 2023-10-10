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
    collections::HashMap,
    ops::Deref,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use as_variant::as_variant;
use ruma::{
    api::client::keys::upload_signatures::v3::Request as SignatureUploadRequest,
    events::{
        key::verification::VerificationMethod, room::message::KeyVerificationRequestEventContent,
    },
    DeviceId, EventId, OwnedDeviceId, OwnedUserId, RoomId, UserId,
};
use serde::{Deserialize, Serialize};
use tracing::error;

use super::{atomic_bool_deserializer, atomic_bool_serializer};
use crate::{
    error::SignatureError,
    store::{Changes, IdentityChanges, Store},
    types::{MasterPubkey, SelfSigningPubkey, UserSigningPubkey},
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
        as_variant!(self, Self::Own)
    }

    /// Destructure the enum into an `UserIdentity` if it's of the correct
    /// type.
    pub fn other(self) -> Option<UserIdentity> {
        as_variant!(self, Self::Other)
    }

    /// Get the ID of the user this identity belongs to.
    pub fn user_id(&self) -> &UserId {
        match self {
            UserIdentities::Own(u) => u.user_id(),
            UserIdentities::Other(u) => u.user_id(),
        }
    }

    pub(crate) fn new(
        store: Store,
        identity: ReadOnlyUserIdentities,
        verification_machine: VerificationMachine,
        own_identity: Option<ReadOnlyOwnUserIdentity>,
    ) -> Self {
        match identity {
            ReadOnlyUserIdentities::Own(i) => {
                Self::Own(OwnUserIdentity { inner: i, verification_machine, store })
            }
            ReadOnlyUserIdentities::Other(i) => {
                Self::Other(UserIdentity { inner: i, own_identity, verification_machine })
            }
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
/// This is the user identity of a user that is our own. Other users will
/// only contain a master key and a self signing key, meaning that only device
/// signatures can be checked with this identity.
///
/// This struct wraps a read-only version of the struct and allows verifications
/// to be requested to verify our own device with the user identity.
#[derive(Debug, Clone)]
pub struct OwnUserIdentity {
    pub(crate) inner: ReadOnlyOwnUserIdentity,
    pub(crate) verification_machine: VerificationMachine,
    store: Store,
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
            identities: IdentityChanges {
                changed: vec![self.inner.clone().into()],
                new: vec![],
                unchanged: vec![],
            },
            ..Default::default()
        };

        if let Err(e) = self.verification_machine.store.save_changes(changes).await {
            error!(error = ?e, "Couldn't store our own user identity after marking it as verified");
        }

        let cache = self.store.cache().await?;
        let account = cache.account().await?;
        account.sign_master_key(self.master_key.clone()).await
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
            let mut device_keys = self.store.cache().await?.account().await?.device_keys().await;
            device_keys.signatures = signatures;

            self.inner.self_signing_key().verify_device_keys(&device_keys).is_ok()
        } else {
            false
        })
    }

    async fn request_verification_helper(
        &self,
        methods: Option<Vec<VerificationMethod>>,
    ) -> Result<(VerificationRequest, OutgoingVerificationRequest), CryptoStoreError> {
        let all_devices = self.verification_machine.store.get_user_devices(self.user_id()).await?;
        let devices = self
            .inner
            .filter_devices_to_request(all_devices, self.verification_machine.own_device_id());

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
    pub fn is_verified(&self) -> bool {
        self.own_identity.as_ref().is_some_and(|o| o.is_identity_signed(&self.inner).is_ok())
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
                .store
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
        as_variant!(self, Self::Own)
    }

    pub(crate) fn into_own(self) -> Option<ReadOnlyOwnUserIdentity> {
        as_variant!(self, Self::Own)
    }

    /// Destructure the enum into an `UserIdentity` if it's of the correct
    /// type.
    pub fn other(&self) -> Option<&ReadOnlyUserIdentity> {
        as_variant!(self, Self::Other)
    }
}

/// Struct representing a cross signing identity of a user.
///
/// This is the user identity of a user that isn't our own. Other users will
/// only contain a master key and a self signing key, meaning that only device
/// signatures can be checked with this identity.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReadOnlyUserIdentity {
    user_id: OwnedUserId,
    pub(crate) master_key: MasterPubkey,
    self_signing_key: SelfSigningPubkey,
}

impl PartialEq for ReadOnlyUserIdentity {
    /// The `PartialEq` implementation compares several attributes, including
    /// the user ID, key material, usage, and, notably, the signatures of
    /// the master key.
    ///
    /// This approach contrasts with the `PartialEq` implementation of the
    /// [`MasterPubkey`], and [`SelfSigningPubkey`] types,
    /// where the signatures are disregarded. This distinction arises from our
    /// treatment of identity as the combined representation of cross-signing
    /// keys and the associated verification state.
    ///
    /// The verification state of an identity depends on the signatures of the
    /// master key, requiring their inclusion in our `PartialEq` implementation.
    fn eq(&self, other: &Self) -> bool {
        self.user_id == other.user_id
            && self.master_key == other.master_key
            && self.self_signing_key == other.self_signing_key
            && self.master_key.signatures() == other.master_key.signatures()
    }
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

        Ok(Self { user_id: master_key.user_id().into(), master_key, self_signing_key })
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
    /// Otherwise, returns `true` if there was a change to the identity and
    /// `false` if the identity is unchanged.
    pub(crate) fn update(
        &mut self,
        master_key: MasterPubkey,
        self_signing_key: SelfSigningPubkey,
    ) -> Result<bool, SignatureError> {
        master_key.verify_subkey(&self_signing_key)?;

        let new = Self::new(master_key, self_signing_key)?;
        let changed = new != *self;

        *self = new;
        Ok(changed)
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
    user_id: OwnedUserId,
    master_key: MasterPubkey,
    self_signing_key: SelfSigningPubkey,
    user_signing_key: UserSigningPubkey,
    #[serde(
        serialize_with = "atomic_bool_serializer",
        deserialize_with = "atomic_bool_deserializer"
    )]
    verified: Arc<AtomicBool>,
}

impl PartialEq for ReadOnlyOwnUserIdentity {
    /// The `PartialEq` implementation compares several attributes, including
    /// the user ID, key material, usage, and, notably, the signatures of
    /// the master key.
    ///
    /// This approach contrasts with the `PartialEq` implementation of the
    /// [`MasterPubkey`], [`SelfSigningPubkey`] and [`UserSigningPubkey`] types,
    /// where the signatures are disregarded. This distinction arises from our
    /// treatment of identity as the combined representation of cross-signing
    /// keys and the associated verification state.
    ///
    /// The verification state of an identity depends on the signatures of the
    /// master key, requiring their inclusion in our `PartialEq` implementation.
    fn eq(&self, other: &Self) -> bool {
        self.user_id == other.user_id
            && self.master_key == other.master_key
            && self.self_signing_key == other.self_signing_key
            && self.user_signing_key == other.user_signing_key
            && self.is_verified() == other.is_verified()
            && self.master_key.signatures() == other.master_key.signatures()
    }
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
            user_id: master_key.user_id().into(),
            master_key,
            self_signing_key,
            user_signing_key,
            verified: Arc::new(AtomicBool::new(false)),
        })
    }

    #[cfg(test)]
    pub(crate) async fn from_private(identity: &crate::olm::PrivateCrossSigningIdentity) -> Self {
        let master_key = identity.master_key.lock().await.as_ref().unwrap().public_key.clone();
        let self_signing_key =
            identity.self_signing_key.lock().await.as_ref().unwrap().public_key.clone();
        let user_signing_key =
            identity.user_signing_key.lock().await.as_ref().unwrap().public_key.clone();

        Self {
            user_id: identity.user_id().into(),
            master_key,
            self_signing_key,
            user_signing_key,
            verified: Arc::new(AtomicBool::new(false)),
        }
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

    #[cfg(test)]
    pub fn mark_as_unverified(&self) {
        self.verified.store(false, Ordering::SeqCst)
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
    /// Otherwise, returns `true` if there was a change to the identity and
    /// `false` if the identity is unchanged.
    pub(crate) fn update(
        &mut self,
        master_key: MasterPubkey,
        self_signing_key: SelfSigningPubkey,
        user_signing_key: UserSigningPubkey,
    ) -> Result<bool, SignatureError> {
        master_key.verify_subkey(&self_signing_key)?;
        master_key.verify_subkey(&user_signing_key)?;

        let old = self.clone();

        self.self_signing_key = self_signing_key;
        self.user_signing_key = user_signing_key;

        if self.master_key != master_key {
            self.verified.store(false, Ordering::SeqCst);
        }

        self.master_key = master_key;

        Ok(old != *self)
    }

    fn filter_devices_to_request(
        &self,
        devices: HashMap<OwnedDeviceId, ReadOnlyDevice>,
        own_device_id: &DeviceId,
    ) -> Vec<OwnedDeviceId> {
        devices
            .into_iter()
            .filter_map(|(device_id, device)| {
                (device_id != own_device_id && self.is_device_signed(&device).is_ok())
                    .then_some(device_id)
            })
            .collect()
    }
}

#[cfg(any(test, feature = "testing"))]
pub(crate) mod testing {
    //! Testing Facilities
    #![allow(dead_code)]
    use ruma::{api::client::keys::get_keys::v3::Response as KeyQueryResponse, user_id};

    use super::{ReadOnlyOwnUserIdentity, ReadOnlyUserIdentity};
    #[cfg(test)]
    use crate::{identities::manager::testing::other_user_id, olm::PrivateCrossSigningIdentity};
    use crate::{
        identities::{
            manager::testing::{other_key_query, own_key_query},
            ReadOnlyDevice,
        },
        types::CrossSigningKey,
    };

    /// Generate test devices from KeyQueryResponse
    pub fn device(response: &KeyQueryResponse) -> (ReadOnlyDevice, ReadOnlyDevice) {
        let mut devices = response.device_keys.values().next().unwrap().values();
        let first =
            ReadOnlyDevice::try_from(&devices.next().unwrap().deserialize_as().unwrap()).unwrap();
        let second =
            ReadOnlyDevice::try_from(&devices.next().unwrap().deserialize_as().unwrap()).unwrap();
        (first, second)
    }

    /// Generate ReadOnlyOwnUserIdentity from KeyQueryResponse for testing
    pub fn own_identity(response: &KeyQueryResponse) -> ReadOnlyOwnUserIdentity {
        let user_id = user_id!("@example:localhost");

        let master_key: CrossSigningKey =
            response.master_keys.get(user_id).unwrap().deserialize_as().unwrap();
        let user_signing: CrossSigningKey =
            response.user_signing_keys.get(user_id).unwrap().deserialize_as().unwrap();
        let self_signing: CrossSigningKey =
            response.self_signing_keys.get(user_id).unwrap().deserialize_as().unwrap();

        ReadOnlyOwnUserIdentity::new(
            master_key.try_into().unwrap(),
            self_signing.try_into().unwrap(),
            user_signing.try_into().unwrap(),
        )
        .unwrap()
    }

    /// Generate default own identity for tests
    pub fn get_own_identity() -> ReadOnlyOwnUserIdentity {
        own_identity(&own_key_query())
    }

    /// Generate default other "own" identity for tests
    #[cfg(test)]
    pub async fn get_other_own_identity() -> ReadOnlyOwnUserIdentity {
        let private_identity = PrivateCrossSigningIdentity::new(other_user_id().into()).await;
        ReadOnlyOwnUserIdentity::from_private(&private_identity).await
    }

    /// Generate default other identify for tests
    pub fn get_other_identity() -> ReadOnlyUserIdentity {
        let user_id = user_id!("@example2:localhost");
        let response = other_key_query();

        let master_key: CrossSigningKey =
            response.master_keys.get(user_id).unwrap().deserialize_as().unwrap();
        let self_signing: CrossSigningKey =
            response.self_signing_keys.get(user_id).unwrap().deserialize_as().unwrap();

        ReadOnlyUserIdentity::new(master_key.try_into().unwrap(), self_signing.try_into().unwrap())
            .unwrap()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{collections::HashMap, sync::Arc};

    use assert_matches::assert_matches;
    use matrix_sdk_test::async_test;
    use ruma::{device_id, user_id};
    use serde_json::{json, Value};
    use tokio::sync::Mutex;

    use super::{
        testing::{device, get_other_identity, get_own_identity},
        ReadOnlyOwnUserIdentity, ReadOnlyUserIdentities,
    };
    use crate::{
        identities::{manager::testing::own_key_query, Device},
        olm::{Account, PrivateCrossSigningIdentity},
        store::{CryptoStoreWrapper, MemoryStore},
        types::{CrossSigningKey, MasterPubkey, SelfSigningPubkey, Signatures, UserSigningPubkey},
        verification::VerificationMachine,
    };

    #[test]
    fn own_identity_create() {
        let user_id = user_id!("@example:localhost");
        let response = own_key_query();

        let master_key: CrossSigningKey =
            response.master_keys.get(user_id).unwrap().deserialize_as().unwrap();
        let user_signing: CrossSigningKey =
            response.user_signing_keys.get(user_id).unwrap().deserialize_as().unwrap();
        let self_signing: CrossSigningKey =
            response.self_signing_keys.get(user_id).unwrap().deserialize_as().unwrap();

        ReadOnlyOwnUserIdentity::new(
            master_key.try_into().unwrap(),
            self_signing.try_into().unwrap(),
            user_signing.try_into().unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn own_identity_partial_equality() {
        let user_id = user_id!("@example:localhost");
        let response = own_key_query();

        let master_key: CrossSigningKey =
            response.master_keys.get(user_id).unwrap().deserialize_as().unwrap();
        let user_signing: CrossSigningKey =
            response.user_signing_keys.get(user_id).unwrap().deserialize_as().unwrap();
        let self_signing: CrossSigningKey =
            response.self_signing_keys.get(user_id).unwrap().deserialize_as().unwrap();

        let identity = ReadOnlyOwnUserIdentity::new(
            master_key.clone().try_into().unwrap(),
            self_signing.clone().try_into().unwrap(),
            user_signing.clone().try_into().unwrap(),
        )
        .unwrap();

        let mut master_key_updated_signature = master_key.clone();
        master_key_updated_signature.signatures = Signatures::new();

        let updated_identity = ReadOnlyOwnUserIdentity::new(
            master_key_updated_signature.try_into().unwrap(),
            self_signing.try_into().unwrap(),
            user_signing.try_into().unwrap(),
        )
        .unwrap();

        assert_ne!(identity, updated_identity);
        assert_eq!(identity.master_key(), updated_identity.master_key());
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

        identity.is_device_signed(&first).unwrap_err();
        identity.is_device_signed(&second).unwrap();

        let private_identity =
            Arc::new(Mutex::new(PrivateCrossSigningIdentity::empty(second.user_id())));
        let verification_machine = VerificationMachine::new(
            Account::with_device_id(second.user_id(), second.device_id()).static_data,
            private_identity,
            Arc::new(CryptoStoreWrapper::new(second.user_id(), MemoryStore::new())),
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
        assert!(second.is_verified());
        assert!(!first.is_verified());
    }

    #[async_test]
    async fn own_device_with_private_identity() {
        let response = own_key_query();
        let (_, device) = device(&response);

        let account = Account::with_device_id(device.user_id(), device.device_id());
        let (identity, _, _) = PrivateCrossSigningIdentity::with_account(&account).await;

        let id = Arc::new(Mutex::new(identity.clone()));

        let verification_machine = VerificationMachine::new(
            Account::with_device_id(device.user_id(), device.device_id()).static_data,
            id.clone(),
            Arc::new(CryptoStoreWrapper::new(device.user_id(), MemoryStore::new())),
        );

        let public_identity = identity.to_public_identity().await.unwrap();

        let mut device = Device {
            inner: device,
            verification_machine: verification_machine.clone(),
            own_identity: Some(public_identity.clone()),
            device_owner_identity: Some(public_identity.clone().into()),
        };

        assert!(!device.is_verified());

        let mut device_keys = device.as_device_keys().to_owned();

        identity.sign_device_keys(&mut device_keys).await.unwrap();
        device.inner.update_device(&device_keys).expect("Couldn't update newly signed device keys");
        assert!(device.is_verified());
    }

    /// Test that `CrossSigningKey` instances without a correct `usage` cannot
    /// be deserialized into high-level structs representing the MSK, SSK
    /// and USK.
    #[test]
    fn cannot_instantiate_keys_with_incorrect_usage() {
        let user_id = user_id!("@example:localhost");
        let response = own_key_query();

        let master_key = response.master_keys.get(user_id).unwrap();
        let mut master_key_json: Value = master_key.deserialize_as().unwrap();
        let self_signing_key = response.self_signing_keys.get(user_id).unwrap();
        let mut self_signing_key_json: Value = self_signing_key.deserialize_as().unwrap();
        let user_signing_key = response.user_signing_keys.get(user_id).unwrap();
        let mut user_signing_key_json: Value = user_signing_key.deserialize_as().unwrap();

        // Delete the usages.
        let usage = master_key_json.get_mut("usage").unwrap();
        *usage = json!([]);
        let usage = self_signing_key_json.get_mut("usage").unwrap();
        *usage = json!([]);
        let usage = user_signing_key_json.get_mut("usage").unwrap();
        *usage = json!([]);

        // It should now be impossible to deserialize the keys into their corresponding
        // high-level cross-signing key structs.
        assert_matches!(serde_json::from_value::<MasterPubkey>(master_key_json.clone()), Err(_));
        assert_matches!(
            serde_json::from_value::<SelfSigningPubkey>(self_signing_key_json.clone()),
            Err(_)
        );
        assert_matches!(
            serde_json::from_value::<UserSigningPubkey>(user_signing_key_json.clone()),
            Err(_)
        );

        // Add additional usages.
        let usage = master_key_json.get_mut("usage").unwrap();
        *usage = json!(["master", "user_signing"]);
        let usage = self_signing_key_json.get_mut("usage").unwrap();
        *usage = json!(["self_signing", "user_signing"]);
        let usage = user_signing_key_json.get_mut("usage").unwrap();
        *usage = json!(["user_signing", "self_signing"]);

        // It should still be impossible to deserialize the keys into their
        // corresponding high-level cross-signing key structs.
        assert_matches!(serde_json::from_value::<MasterPubkey>(master_key_json.clone()), Err(_));
        assert_matches!(
            serde_json::from_value::<SelfSigningPubkey>(self_signing_key_json.clone()),
            Err(_)
        );
        assert_matches!(
            serde_json::from_value::<UserSigningPubkey>(user_signing_key_json.clone()),
            Err(_)
        );
    }

    #[test]
    fn filter_devices_to_request() {
        let response = own_key_query();
        let identity = get_own_identity();
        let (first, second) = device(&response);

        let second_device_id = second.device_id().to_owned();
        let unknown_device_id = device_id!("UNKNOWN");

        let devices = HashMap::from([
            (first.device_id().to_owned(), first),
            (second.device_id().to_owned(), second),
        ]);

        // Own device and devices not verified are filtered out.
        assert_eq!(identity.filter_devices_to_request(devices.clone(), &second_device_id).len(), 0);
        // Signed devices that are not our own are kept.
        assert_eq!(
            identity.filter_devices_to_request(devices, unknown_device_id),
            [second_device_id]
        );
    }
}
