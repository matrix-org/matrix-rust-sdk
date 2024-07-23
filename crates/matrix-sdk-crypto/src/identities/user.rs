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
        Arc, RwLock,
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
use serde_json::Value;
use tracing::error;

use super::{atomic_bool_deserializer, atomic_bool_serializer};
use crate::{
    error::SignatureError,
    store::{Changes, IdentityChanges, Store},
    types::{MasterPubkey, SelfSigningPubkey, UserSigningPubkey},
    verification::VerificationMachine,
    CryptoStoreError, DeviceData, OutgoingVerificationRequest, VerificationRequest,
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
        identity: UserIdentityData,
        verification_machine: VerificationMachine,
        own_identity: Option<OwnUserIdentityData>,
    ) -> Self {
        match identity {
            UserIdentityData::Own(i) => {
                Self::Own(OwnUserIdentity { inner: i, verification_machine, store })
            }
            UserIdentityData::Other(i) => {
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
/// This struct wraps the [`OwnUserIdentityData`] type and allows a verification
/// to be requested to verify our own device with the user identity.
#[derive(Debug, Clone)]
pub struct OwnUserIdentity {
    pub(crate) inner: OwnUserIdentityData,
    pub(crate) verification_machine: VerificationMachine,
    store: Store,
}

impl Deref for OwnUserIdentity {
    type Target = OwnUserIdentityData;

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
        account.sign_master_key(&self.master_key)
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

    /// Does our user identity trust our own device, i.e. have we signed our
    /// own device keys with our self-signing key.
    pub async fn trusts_our_own_device(&self) -> Result<bool, CryptoStoreError> {
        Ok(if let Some(signatures) = self.verification_machine.store.device_signatures().await? {
            let mut device_keys = self.store.cache().await?.account().await?.device_keys();
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

        Ok(self.verification_machine.request_to_device_verification(
            self.user_id(),
            devices,
            methods,
        ))
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
    pub(crate) inner: OtherUserIdentityData,
    pub(crate) own_identity: Option<OwnUserIdentityData>,
    pub(crate) verification_machine: VerificationMachine,
}

impl Deref for UserIdentity {
    type Target = OtherUserIdentityData;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl UserIdentity {
    /// Is this user identity verified.
    pub fn is_verified(&self) -> bool {
        self.own_identity.as_ref().is_some_and(|own_identity| {
            // The identity of another user is verified iff our own identity is verified and
            // if our own identity has signed the other user's identity.
            own_identity.is_verified() && own_identity.is_identity_signed(&self.inner).is_ok()
        })
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
    pub fn request_verification(
        &self,
        room_id: &RoomId,
        request_event_id: &EventId,
        methods: Option<Vec<VerificationMethod>>,
    ) -> VerificationRequest {
        self.verification_machine.request_verification(
            &self.inner,
            room_id,
            request_event_id,
            methods,
        )
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
    pub fn verification_request_content(
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

    /// Pin the current identity (Master public key).
    pub async fn pin_current_master_key(&self) -> Result<(), CryptoStoreError> {
        self.inner.pin();
        let to_save = UserIdentityData::Other(self.inner.clone());
        let changes = Changes {
            identities: IdentityChanges { changed: vec![to_save], ..Default::default() },
            ..Default::default()
        };
        self.verification_machine.store.inner().save_changes(changes).await?;
        Ok(())
    }

    /// Returns true if the identity is not verified and did change since the
    /// last time we pinned it.
    ///
    /// An identity mismatch is detected when there is a trust problem with the
    /// user identity. There is an identity mismatch if the current identity
    /// is not verified and there is a pinning violation. An identity
    /// mismatch must be reported to the user, and can be resolved by:
    /// - Verifying the new identity (see
    ///   [`UserIdentity::request_verification`])
    /// - Or by updating the pinned key (see
    ///   [`UserIdentity::pin_current_master_key`]).
    pub fn has_identity_mismatch(&self) -> bool {
        // First check if the current identity is verified.
        if self.is_verified() {
            return false;
        }
        // If not we can check the pinned identity. Verification always have
        // higher priority than pinning.
        self.inner.has_pin_violation()
    }
}

/// Enum over the different user identity types we can have.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserIdentityData {
    /// Our own user identity.
    Own(OwnUserIdentityData),
    /// Identities of other users.
    Other(OtherUserIdentityData),
}

impl From<OwnUserIdentityData> for UserIdentityData {
    fn from(identity: OwnUserIdentityData) -> Self {
        UserIdentityData::Own(identity)
    }
}

impl From<OtherUserIdentityData> for UserIdentityData {
    fn from(identity: OtherUserIdentityData) -> Self {
        UserIdentityData::Other(identity)
    }
}

impl UserIdentityData {
    /// The unique user id of this identity.
    pub fn user_id(&self) -> &UserId {
        match self {
            UserIdentityData::Own(i) => i.user_id(),
            UserIdentityData::Other(i) => i.user_id(),
        }
    }

    /// Get the master key of the identity.
    pub fn master_key(&self) -> &MasterPubkey {
        match self {
            UserIdentityData::Own(i) => i.master_key(),
            UserIdentityData::Other(i) => i.master_key(),
        }
    }

    /// Get the [`SelfSigningPubkey`] key of the identity.
    pub fn self_signing_key(&self) -> &SelfSigningPubkey {
        match self {
            UserIdentityData::Own(i) => &i.self_signing_key,
            UserIdentityData::Other(i) => &i.self_signing_key,
        }
    }

    /// Get the user-signing key of the identity, this is only present for our
    /// own user identity..
    pub fn user_signing_key(&self) -> Option<&UserSigningPubkey> {
        match self {
            UserIdentityData::Own(i) => Some(&i.user_signing_key),
            UserIdentityData::Other(_) => None,
        }
    }

    /// Convert the enum into a reference [`OwnUserIdentityData`] if it's of
    /// the correct type.
    pub fn own(&self) -> Option<&OwnUserIdentityData> {
        as_variant!(self, Self::Own)
    }

    /// Convert the enum into an [`OwnUserIdentityData`] if it's of the correct
    /// type.
    pub(crate) fn into_own(self) -> Option<OwnUserIdentityData> {
        as_variant!(self, Self::Own)
    }

    /// Convert the enum into a reference to [`OtherUserIdentityData`] if
    /// it's of the correct type.
    pub fn other(&self) -> Option<&OtherUserIdentityData> {
        as_variant!(self, Self::Other)
    }
}

/// Struct representing a cross signing identity of a user.
///
/// This is the user identity of a user that isn't our own. Other users will
/// only contain a master key and a self signing key, meaning that only device
/// signatures can be checked with this identity.
///
/// This struct also contains the currently pinned master key for that user.
/// The first time a cryptographic identity is seen for a given user, it
/// will be associated to that user (i.e. pinned). Future interactions
/// will expect this user crypto identity to stay the same,
/// this will help prevent some MITM attacks.
/// In case of identity change, it will be possible to pin the new identity
/// is the user wants.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(try_from = "OtherUserIdentityDataSerializer", into = "OtherUserIdentityDataSerializer")]
pub struct OtherUserIdentityData {
    user_id: OwnedUserId,
    pub(crate) master_key: Arc<MasterPubkey>,
    self_signing_key: Arc<SelfSigningPubkey>,
    pinned_master_key: Arc<RwLock<MasterPubkey>>,
}

/// Intermediate struct to help serialize OtherUserIdentityData and support
/// versioning and migration.
/// Version v1 is adding support for identity pinning (`pinned_master_key`), as
/// part of migration we just pin the currently known master key.
#[derive(Deserialize, Serialize)]
struct OtherUserIdentityDataSerializer {
    version: Option<String>,
    #[serde(flatten)]
    other: Value,
}

#[derive(Debug, Deserialize, Serialize)]
struct OtherUserIdentityDataSerializerV0 {
    user_id: OwnedUserId,
    master_key: MasterPubkey,
    self_signing_key: SelfSigningPubkey,
}

#[derive(Debug, Deserialize, Serialize)]
struct OtherUserIdentityDataSerializerV1 {
    user_id: OwnedUserId,
    master_key: MasterPubkey,
    self_signing_key: SelfSigningPubkey,
    pinned_master_key: MasterPubkey,
}

impl TryFrom<OtherUserIdentityDataSerializer> for OtherUserIdentityData {
    type Error = serde_json::Error;
    fn try_from(
        value: OtherUserIdentityDataSerializer,
    ) -> Result<OtherUserIdentityData, Self::Error> {
        match value.version {
            None => {
                // Old format, migrate the pinned identity
                let v0: OtherUserIdentityDataSerializerV0 = serde_json::from_value(value.other)?;
                Ok(OtherUserIdentityData {
                    user_id: v0.user_id,
                    master_key: Arc::new(v0.master_key.clone()),
                    self_signing_key: Arc::new(v0.self_signing_key),
                    // We migrate by pinning the current master key
                    pinned_master_key: Arc::new(RwLock::new(v0.master_key)),
                })
            }
            Some(v) if v == "1" => {
                let v1: OtherUserIdentityDataSerializerV1 = serde_json::from_value(value.other)?;
                Ok(OtherUserIdentityData {
                    user_id: v1.user_id,
                    master_key: Arc::new(v1.master_key.clone()),
                    self_signing_key: Arc::new(v1.self_signing_key),
                    pinned_master_key: Arc::new(RwLock::new(v1.pinned_master_key)),
                })
            }
            _ => Err(serde::de::Error::custom(format!("Unsupported Version {:?}", value.version))),
        }
    }
}

impl From<OtherUserIdentityData> for OtherUserIdentityDataSerializer {
    fn from(value: OtherUserIdentityData) -> Self {
        let v1 = OtherUserIdentityDataSerializerV1 {
            user_id: value.user_id.clone(),
            master_key: value.master_key().to_owned(),
            self_signing_key: value.self_signing_key().to_owned(),
            pinned_master_key: value.pinned_master_key.read().unwrap().clone(),
        };
        OtherUserIdentityDataSerializer {
            version: Some("1".to_owned()),
            other: serde_json::to_value(v1).unwrap(),
        }
    }
}

impl PartialEq for OtherUserIdentityData {
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

impl OtherUserIdentityData {
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

        Ok(Self {
            user_id: master_key.user_id().into(),
            master_key: master_key.clone().into(),
            self_signing_key: self_signing_key.into(),
            pinned_master_key: RwLock::new(master_key).into(),
        })
    }

    #[cfg(test)]
    pub(crate) async fn from_private(identity: &crate::olm::PrivateCrossSigningIdentity) -> Self {
        let master_key = identity.master_key.lock().await.as_ref().unwrap().public_key().clone();
        let self_signing_key =
            identity.self_signing_key.lock().await.as_ref().unwrap().public_key().clone().into();

        Self {
            user_id: identity.user_id().into(),
            master_key: Arc::new(master_key.clone()),
            self_signing_key,
            pinned_master_key: Arc::new(RwLock::new(master_key.clone())),
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

    /// Pin the current identity
    pub(crate) fn pin(&self) {
        let mut m = self.pinned_master_key.write().unwrap();
        *m = self.master_key.as_ref().clone()
    }

    /// Returns true if the identity has changed since we last pinned it.
    ///
    /// Key pinning acts as a trust on first use mechanism, the first time an
    /// identity is known for a user it will be pinned.
    /// For future interaction with a user, the identity is expected to be the
    /// one that was pinned. In case of identity change the UI client should
    /// receive reports of pinning violation and decide to act accordingly;
    /// that is accept and pin the new identity, perform a verification or
    /// stop communications.
    pub(crate) fn has_pin_violation(&self) -> bool {
        let pinned_master_key = self.pinned_master_key.read().unwrap();
        pinned_master_key.get_first_key() != self.master_key().get_first_key()
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

        // We update the identity with the new master and self signing key, but we keep
        // the previous pinned master key.
        // This identity will have a pin violation until the new master key is pinned
        // (see [`has_pin_violation`]).
        let pinned_master_key = self.pinned_master_key.read().unwrap().clone();

        let new = Self {
            user_id: master_key.user_id().into(),
            master_key: master_key.clone().into(),
            self_signing_key: self_signing_key.into(),
            pinned_master_key: RwLock::new(pinned_master_key).into(),
        };
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
    pub(crate) fn is_device_signed(&self, device: &DeviceData) -> Result<(), SignatureError> {
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
pub struct OwnUserIdentityData {
    user_id: OwnedUserId,
    master_key: Arc<MasterPubkey>,
    self_signing_key: Arc<SelfSigningPubkey>,
    user_signing_key: Arc<UserSigningPubkey>,
    #[serde(
        serialize_with = "atomic_bool_serializer",
        deserialize_with = "atomic_bool_deserializer"
    )]
    verified: Arc<AtomicBool>,
}

impl PartialEq for OwnUserIdentityData {
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

impl OwnUserIdentityData {
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
            master_key: master_key.into(),
            self_signing_key: self_signing_key.into(),
            user_signing_key: user_signing_key.into(),
            verified: Arc::new(AtomicBool::new(false)),
        })
    }

    #[cfg(test)]
    pub(crate) async fn from_private(identity: &crate::olm::PrivateCrossSigningIdentity) -> Self {
        let master_key = identity.master_key.lock().await.as_ref().unwrap().public_key().clone();
        let self_signing_key =
            identity.self_signing_key.lock().await.as_ref().unwrap().public_key().clone();
        let user_signing_key =
            identity.user_signing_key.lock().await.as_ref().unwrap().public_key().clone();

        Self {
            user_id: identity.user_id().into(),
            master_key: master_key.into(),
            self_signing_key: self_signing_key.into(),
            user_signing_key: user_signing_key.into(),
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
    /// * `identity` - The identity of another user that we want to check if it
    ///   has been signed.
    ///
    /// Returns an empty result if the signature check succeeded, otherwise a
    /// SignatureError indicating why the check failed.
    pub(crate) fn is_identity_signed(
        &self,
        identity: &OtherUserIdentityData,
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
    pub(crate) fn is_device_signed(&self, device: &DeviceData) -> Result<(), SignatureError> {
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

        self.self_signing_key = self_signing_key.into();
        self.user_signing_key = user_signing_key.into();

        if self.master_key.as_ref() != &master_key {
            self.verified.store(false, Ordering::SeqCst);
        }

        self.master_key = master_key.into();

        Ok(old != *self)
    }

    fn filter_devices_to_request(
        &self,
        devices: HashMap<OwnedDeviceId, DeviceData>,
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

/// Testing Facilities
#[cfg(any(test, feature = "testing"))]
#[allow(dead_code)]
pub(crate) mod testing {
    use ruma::{api::client::keys::get_keys::v3::Response as KeyQueryResponse, user_id};

    use super::{OtherUserIdentityData, OwnUserIdentityData};
    #[cfg(test)]
    use crate::{identities::manager::testing::other_user_id, olm::PrivateCrossSigningIdentity};
    use crate::{
        identities::{
            manager::testing::{other_key_query, own_key_query},
            DeviceData,
        },
        types::CrossSigningKey,
    };

    /// Generate test devices from KeyQueryResponse
    pub fn device(response: &KeyQueryResponse) -> (DeviceData, DeviceData) {
        let mut devices = response.device_keys.values().next().unwrap().values();
        let first =
            DeviceData::try_from(&devices.next().unwrap().deserialize_as().unwrap()).unwrap();
        let second =
            DeviceData::try_from(&devices.next().unwrap().deserialize_as().unwrap()).unwrap();
        (first, second)
    }

    /// Generate [`OwnUserIdentityData`] from a [`KeyQueryResponse`] for testing
    pub fn own_identity(response: &KeyQueryResponse) -> OwnUserIdentityData {
        let user_id = user_id!("@example:localhost");

        let master_key: CrossSigningKey =
            response.master_keys.get(user_id).unwrap().deserialize_as().unwrap();
        let user_signing: CrossSigningKey =
            response.user_signing_keys.get(user_id).unwrap().deserialize_as().unwrap();
        let self_signing: CrossSigningKey =
            response.self_signing_keys.get(user_id).unwrap().deserialize_as().unwrap();

        OwnUserIdentityData::new(
            master_key.try_into().unwrap(),
            self_signing.try_into().unwrap(),
            user_signing.try_into().unwrap(),
        )
        .unwrap()
    }

    /// Generate default own identity for tests
    pub fn get_own_identity() -> OwnUserIdentityData {
        own_identity(&own_key_query())
    }

    /// Generate default other "own" identity for tests
    #[cfg(test)]
    pub async fn get_other_own_identity() -> OwnUserIdentityData {
        let private_identity = PrivateCrossSigningIdentity::new(other_user_id().into());
        OwnUserIdentityData::from_private(&private_identity).await
    }

    /// Generate default other identify for tests
    pub fn get_other_identity() -> OtherUserIdentityData {
        let user_id = user_id!("@example2:localhost");
        let response = other_key_query();

        let master_key: CrossSigningKey =
            response.master_keys.get(user_id).unwrap().deserialize_as().unwrap();
        let self_signing: CrossSigningKey =
            response.self_signing_keys.get(user_id).unwrap().deserialize_as().unwrap();

        OtherUserIdentityData::new(master_key.try_into().unwrap(), self_signing.try_into().unwrap())
            .unwrap()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{collections::HashMap, sync::Arc};

    use assert_matches::assert_matches;
    use matrix_sdk_test::{async_test, response_from_file, test_json};
    use ruma::{
        api::{client::keys::get_keys::v3::Response as KeyQueryResponse, IncomingResponse},
        device_id, user_id, TransactionId,
    };
    use serde_json::{json, Value};
    use tokio::sync::Mutex;

    use super::{
        testing::{device, get_other_identity, get_own_identity},
        OwnUserIdentityData, UserIdentityData,
    };
    use crate::{
        identities::{
            manager::testing::own_key_query,
            user::{OtherUserIdentityDataSerializer, OtherUserIdentityDataSerializerV1},
            Device,
        },
        olm::{Account, PrivateCrossSigningIdentity},
        store::{CryptoStoreWrapper, MemoryStore},
        types::{CrossSigningKey, MasterPubkey, SelfSigningPubkey, Signatures, UserSigningPubkey},
        verification::VerificationMachine,
        OlmMachine, OtherUserIdentityData,
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

        OwnUserIdentityData::new(
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

        let identity = OwnUserIdentityData::new(
            master_key.clone().try_into().unwrap(),
            self_signing.clone().try_into().unwrap(),
            user_signing.clone().try_into().unwrap(),
        )
        .unwrap();

        let mut master_key_updated_signature = master_key;
        master_key_updated_signature.signatures = Signatures::new();

        let updated_identity = OwnUserIdentityData::new(
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
    fn deserialization_migration_test() {
        let serialized_value = json!({
                "user_id":"@example2:localhost",
                "master_key":{
                   "user_id":"@example2:localhost",
                   "usage":[
                      "master"
                   ],
                   "keys":{
                      "ed25519:kC/HmRYw4HNqUp/i4BkwYENrf+hd9tvdB7A1YOf5+Do":"kC/HmRYw4HNqUp/i4BkwYENrf+hd9tvdB7A1YOf5+Do"
                   },
                   "signatures":{
                      "@example2:localhost":{
                         "ed25519:SKISMLNIMH":"KdUZqzt8VScGNtufuQ8lOf25byYLWIhmUYpPENdmM8nsldexD7vj+Sxoo7PknnTX/BL9h2N7uBq0JuykjunCAw"
                      }
                   }
                },
                "self_signing_key":{
                   "user_id":"@example2:localhost",
                   "usage":[
                      "self_signing"
                   ],
                   "keys":{
                      "ed25519:ZtFrSkJ1qB8Jph/ql9Eo/lKpIYCzwvKAKXfkaS4XZNc":"ZtFrSkJ1qB8Jph/ql9Eo/lKpIYCzwvKAKXfkaS4XZNc"
                   },
                   "signatures":{
                      "@example2:localhost":{
                         "ed25519:kC/HmRYw4HNqUp/i4BkwYENrf+hd9tvdB7A1YOf5+Do":"W/O8BnmiUETPpH02mwYaBgvvgF/atXnusmpSTJZeUSH/vHg66xiZOhveQDG4cwaW8iMa+t9N4h1DWnRoHB4mCQ"
                      }
                   }
                }
        });
        let migrated: OtherUserIdentityData = serde_json::from_value(serialized_value).unwrap();

        let pinned_master_key = migrated.pinned_master_key.read().unwrap();
        assert_eq!(*pinned_master_key, migrated.master_key().clone());

        // Serialize back
        let value = serde_json::to_value(migrated.clone()).unwrap();

        // Should be serialized with latest version
        let _: OtherUserIdentityDataSerializerV1 =
            serde_json::from_value(value.clone()).expect("Should deserialize as version 1");

        let with_serializer: OtherUserIdentityDataSerializer =
            serde_json::from_value(value).unwrap();
        assert_eq!("1", with_serializer.version.unwrap());
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
            device_owner_identity: Some(UserIdentityData::Own(identity.clone())),
        };

        let second = Device {
            inner: second,
            verification_machine,
            own_identity: Some(identity.clone()),
            device_owner_identity: Some(UserIdentityData::Own(identity.clone())),
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

    #[async_test]
    async fn resolve_identity_mismatch_with_verification() {
        use test_json::keys_query_sets::IdentityChangeDataSet as DataSet;

        let my_user_id = user_id!("@me:localhost");
        let machine = OlmMachine::new(my_user_id, device_id!("ABCDEFGH")).await;
        machine.bootstrap_cross_signing(false).await.unwrap();

        let my_id = machine.get_identity(my_user_id, None).await.unwrap().unwrap().own().unwrap();
        let usk_key_id = my_id.inner.user_signing_key().keys().iter().next().unwrap().0;

        println!("USK ID: {}", usk_key_id);
        let keys_query = DataSet::key_query_with_identity_a();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        // Simulate an identity hange
        let keys_query = DataSet::key_query_with_identity_b();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let other_user_id = DataSet::user_id();

        let other_identity =
            machine.get_identity(other_user_id, None).await.unwrap().unwrap().other().unwrap();

        // There should be an identity mismatch
        assert!(other_identity.has_identity_mismatch());

        // Manually verify for the purpose of this test
        let sig_upload = other_identity.verify().await.unwrap();

        let raw_extracted =
            sig_upload.signed_keys.get(other_user_id).unwrap().iter().next().unwrap().1.get();

        let new_signature: CrossSigningKey = serde_json::from_str(raw_extracted).unwrap();

        let mut msk_to_update: CrossSigningKey =
            serde_json::from_value(DataSet::msk_b().get("@bob:localhost").unwrap().clone())
                .unwrap();

        msk_to_update.signatures.add_signature(
            my_user_id.to_owned(),
            usk_key_id.to_owned(),
            new_signature.signatures.get_signature(my_user_id, usk_key_id).unwrap(),
        );

        // we want to update bob device keys with the new signature
        let data = json!({
                "device_keys": {}, // For the purpose of this test we don't need devices here
                "failures": {},
                "master_keys": {
                    DataSet::user_id(): msk_to_update
        ,
                },
                "self_signing_keys": DataSet::ssk_b(),
            });

        let kq_response = KeyQueryResponse::try_from_http_response(response_from_file(&data))
            .expect("Can't parse the `/keys/upload` response");
        machine.mark_request_as_sent(&TransactionId::new(), &kq_response).await.unwrap();

        // There should not be an identity mismatch anymore
        let other_identity =
            machine.get_identity(other_user_id, None).await.unwrap().unwrap().other().unwrap();
        assert!(!other_identity.has_identity_mismatch());
        // But there is still a pin violation
        assert!(other_identity.inner.has_pin_violation());
    }
}
