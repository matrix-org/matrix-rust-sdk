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
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use as_variant::as_variant;
use matrix_sdk_common::locks::RwLock;
use ruma::{
    DeviceId, EventId, OwnedDeviceId, OwnedUserId, RoomId, UserId,
    api::client::keys::upload_signatures::v3::Request as SignatureUploadRequest,
    events::{key::verification::VerificationMethod, room::message::MessageType},
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tracing::{error, info};

use crate::{
    CryptoStoreError, DeviceData, VerificationRequest,
    error::SignatureError,
    store::{
        Store,
        types::{Changes, IdentityChanges},
    },
    types::{
        MasterPubkey, SelfSigningPubkey, UserSigningPubkey, requests::OutgoingVerificationRequest,
    },
    verification::VerificationMachine,
};

/// Enum over the different user identity types we can have.
#[derive(Debug, Clone)]
pub enum UserIdentity {
    /// Our own user identity.
    Own(OwnUserIdentity),
    /// An identity belonging to another user.
    Other(OtherUserIdentity),
}

impl UserIdentity {
    /// Destructure the enum into an [`OwnUserIdentity`] if it's of the correct
    /// type.
    pub fn own(self) -> Option<OwnUserIdentity> {
        as_variant!(self, Self::Own)
    }

    /// Destructure the enum into an [`OtherUserIdentity`] if it's of the
    /// correct type.
    pub fn other(self) -> Option<OtherUserIdentity> {
        as_variant!(self, Self::Other)
    }

    /// Get the ID of the user this identity belongs to.
    pub fn user_id(&self) -> &UserId {
        match self {
            UserIdentity::Own(u) => u.user_id(),
            UserIdentity::Other(u) => u.user_id(),
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
                Self::Other(OtherUserIdentity { inner: i, own_identity, verification_machine })
            }
        }
    }

    /// Check if this user identity is verified.
    ///
    /// For our own identity, this means either that we have checked the public
    /// keys in the identity against the private keys; or that the identity
    /// has been manually marked as verified via
    /// [`OwnUserIdentity::verify`].
    ///
    /// For another user's identity, it means that we have verified our own
    /// identity as above, *and* that the other user's identity has been signed
    /// by our own user-signing key.
    pub fn is_verified(&self) -> bool {
        match self {
            UserIdentity::Own(u) => u.is_verified(),
            UserIdentity::Other(u) => u.is_verified(),
        }
    }

    /// True if we verified this identity at some point in the past.
    ///
    /// To reset this latch back to `false`, one must call
    /// [`UserIdentity::withdraw_verification()`].
    pub fn was_previously_verified(&self) -> bool {
        match self {
            UserIdentity::Own(u) => u.was_previously_verified(),
            UserIdentity::Other(u) => u.was_previously_verified(),
        }
    }

    /// Reset the flag that records that the identity has been verified, thus
    /// clearing [`UserIdentity::was_previously_verified`] and
    /// [`UserIdentity::has_verification_violation`].
    pub async fn withdraw_verification(&self) -> Result<(), CryptoStoreError> {
        match self {
            UserIdentity::Own(u) => u.withdraw_verification().await,
            UserIdentity::Other(u) => u.withdraw_verification().await,
        }
    }

    /// Remember this identity, ensuring it does not result in a pin violation.
    ///
    /// When we first see a user, we assume their cryptographic identity has not
    /// been tampered with by the homeserver or another entity with
    /// man-in-the-middle capabilities. We remember this identity and call this
    /// action "pinning".
    ///
    /// If the identity presented for the user changes later on, the newly
    /// presented identity is considered to be in "pin violation". This
    /// method explicitly accepts the new identity, allowing it to replace
    /// the previously pinned one and bringing it out of pin violation.
    ///
    /// UIs should display a warning to the user when encountering an identity
    /// which is not verified and is in pin violation. See
    /// [`OtherUserIdentity::identity_needs_user_approval`].
    pub async fn pin(&self) -> Result<(), CryptoStoreError> {
        match self {
            UserIdentity::Own(_) => {
                // Nothing to be done for our own identity: we already
                // consider it trusted in this sense.
                Ok(())
            }
            UserIdentity::Other(u) => u.pin_current_master_key().await,
        }
    }

    /// Was this identity previously verified, and is no longer?
    pub fn has_verification_violation(&self) -> bool {
        match self {
            UserIdentity::Own(u) => u.has_verification_violation(),
            UserIdentity::Other(u) => u.has_verification_violation(),
        }
    }
}

impl From<OwnUserIdentity> for UserIdentity {
    fn from(i: OwnUserIdentity) -> Self {
        Self::Own(i)
    }
}

impl From<OtherUserIdentity> for UserIdentity {
    fn from(i: OtherUserIdentity) -> Self {
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

impl DerefMut for OwnUserIdentity {
    fn deref_mut(&mut self) -> &mut <Self as Deref>::Target {
        &mut self.inner
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

    /// Remove the requirement for this identity to be verified.
    pub async fn withdraw_verification(&self) -> Result<(), CryptoStoreError> {
        self.inner.withdraw_verification();
        let to_save = UserIdentityData::Own(self.inner.clone());
        let changes = Changes {
            identities: IdentityChanges { changed: vec![to_save], ..Default::default() },
            ..Default::default()
        };
        self.verification_machine.store.inner().save_changes(changes).await?;
        Ok(())
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
pub struct OtherUserIdentity {
    pub(crate) inner: OtherUserIdentityData,
    pub(crate) own_identity: Option<OwnUserIdentityData>,
    pub(crate) verification_machine: VerificationMachine,
}

impl Deref for OtherUserIdentity {
    type Target = OtherUserIdentityData;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for OtherUserIdentity {
    fn deref_mut(&mut self) -> &mut <Self as Deref>::Target {
        &mut self.inner
    }
}

impl OtherUserIdentity {
    /// Is this user identity verified.
    pub fn is_verified(&self) -> bool {
        self.own_identity
            .as_ref()
            .is_some_and(|own_identity| own_identity.is_identity_verified(&self.inner))
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

    /// Create a [`VerificationRequest`] object after the verification request
    /// content returned by [`OtherUserIdentity::verification_request_content`]
    /// has been sent out.
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

    /// Create a verification request to send to the given user.
    ///
    /// The returned content needs to be sent out into a DM room with the user.
    ///
    /// After the content has been sent out a [`VerificationRequest`] can be
    /// started with the [`OtherUserIdentity::request_verification()`] method.
    pub fn verification_request_content(
        &self,
        methods: Option<Vec<VerificationMethod>>,
    ) -> MessageType {
        MessageType::VerificationRequest(VerificationRequest::request(
            self.verification_machine.own_user_id(),
            self.verification_machine.own_device_id(),
            self.user_id(),
            methods,
        ))
    }

    /// Pin the current identity (public part of the master signing key).
    pub async fn pin_current_master_key(&self) -> Result<(), CryptoStoreError> {
        info!(master_key = ?self.master_key.get_first_key(), "Pinning current identity for user '{}'", self.user_id());
        self.inner.pin();
        let to_save = UserIdentityData::Other(self.inner.clone());
        let changes = Changes {
            identities: IdentityChanges { changed: vec![to_save], ..Default::default() },
            ..Default::default()
        };
        self.verification_machine.store.inner().save_changes(changes).await?;
        Ok(())
    }

    /// Has the identity changed in a way that requires approval from the user?
    ///
    /// A user identity needs approval if it changed after the crypto machine
    /// has already observed ("pinned") a different identity for that user,
    /// unless it is an explicitly verified identity (using for example
    /// interactive verification).
    ///
    /// This situation can be resolved by:
    ///
    /// - Verifying the new identity with
    ///   [`OtherUserIdentity::request_verification`], or:
    /// - Updating the pin to the new identity with
    ///   [`OtherUserIdentity::pin_current_master_key`].
    pub fn identity_needs_user_approval(&self) -> bool {
        // First check if the current identity is verified.
        if self.is_verified() {
            return false;
        }
        // If not we can check the pinned identity. Verification always have
        // higher priority than pinning.
        self.inner.has_pin_violation()
    }

    /// Remove the requirement for this identity to be verified.
    pub async fn withdraw_verification(&self) -> Result<(), CryptoStoreError> {
        info!(
            master_key = ?self.master_key.get_first_key(),
            user = ?self.user_id(),
            "Withdrawing verification status and pinning current identity"
        );
        self.inner.withdraw_verification();
        let to_save = UserIdentityData::Other(self.inner.clone());
        let changes = Changes {
            identities: IdentityChanges { changed: vec![to_save], ..Default::default() },
            ..Default::default()
        };
        self.verification_machine.store.inner().save_changes(changes).await?;
        Ok(())
    }

    /// Test helper that marks that an identity has been previously verified and
    /// persist the change in the store.
    #[cfg(test)]
    pub async fn mark_as_previously_verified(&self) -> Result<(), CryptoStoreError> {
        self.inner.mark_as_previously_verified();

        let to_save = UserIdentityData::Other(self.inner.clone());
        let changes = Changes {
            identities: IdentityChanges { changed: vec![to_save], ..Default::default() },
            ..Default::default()
        };

        self.verification_machine.store.inner().save_changes(changes).await?;

        Ok(())
    }

    /// Was this identity verified since initial observation and is not anymore?
    ///
    /// Such a violation should be reported to the local user by the
    /// application, and resolved by
    ///
    /// - Verifying the new identity with
    ///   [`OtherUserIdentity::request_verification`]
    /// - Or by withdrawing the verification requirement
    ///   [`OtherUserIdentity::withdraw_verification`].
    pub fn has_verification_violation(&self) -> bool {
        if !self.inner.was_previously_verified() {
            // If that identity has never been verified it cannot be in violation.
            return false;
        }

        !self.is_verified()
    }
}

/// Enum over the different user identity types we can have.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserIdentityData {
    /// Our own user identity.
    Own(OwnUserIdentityData),
    /// The identity of another user.
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

    /// True if we verified our own identity at some point in the past.
    ///
    /// To reset this latch back to `false`, one must call
    /// [`UserIdentity::withdraw_verification()`].
    pub fn was_previously_verified(&self) -> bool {
        match self {
            UserIdentityData::Own(i) => i.was_previously_verified(),
            UserIdentityData::Other(i) => i.was_previously_verified(),
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
/// This struct also contains the currently pinned user identity (public master
/// key) for that user and a local flag that serves as a latch to remember if an
/// identity was verified once.
///
/// The first time a cryptographic user identity is seen for a given user, it
/// will be associated with that user ("pinned"). Future interactions
/// will expect this identity to stay the same, to avoid MITM attacks from the
/// homeserver.
///
/// The user can explicitly pin the new identity to allow for legitimate
/// identity changes (for example, in case of key material or device loss).
///
/// As soon as the cryptographic identity is verified (i.e. signed by our own
/// trusted identity), a flag is set to remember it (`previously_verified`).
/// Future interactions will expect this user to stay verified, in case of
/// violation the user should be notified with a blocking warning when sending a
/// message.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(try_from = "OtherUserIdentityDataSerializer", into = "OtherUserIdentityDataSerializer")]
pub struct OtherUserIdentityData {
    user_id: OwnedUserId,
    pub(crate) master_key: Arc<MasterPubkey>,
    self_signing_key: Arc<SelfSigningPubkey>,
    pinned_master_key: Arc<RwLock<MasterPubkey>>,
    /// This tracks whether this olm machine has already seen this user as
    /// verified. To use it in the future to detect cases where the user has
    /// become unverified for any reason. This can be reset using
    /// [`OtherUserIdentityData::withdraw_verification()`].
    previously_verified: Arc<AtomicBool>,
}

/// Intermediate struct to help serialize OtherUserIdentityData and support
/// versioning and migration.
///
/// Version v1 is adding support for identity pinning (`pinned_master_key`), as
/// part of migration we just pin the currently known public master key.
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

#[derive(Debug, Deserialize, Serialize)]
struct OtherUserIdentityDataSerializerV2 {
    user_id: OwnedUserId,
    master_key: MasterPubkey,
    self_signing_key: SelfSigningPubkey,
    pinned_master_key: MasterPubkey,
    previously_verified: bool,
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
                    previously_verified: Arc::new(false.into()),
                })
            }
            Some(v) if v == "1" => {
                let v1: OtherUserIdentityDataSerializerV1 = serde_json::from_value(value.other)?;
                Ok(OtherUserIdentityData {
                    user_id: v1.user_id,
                    master_key: Arc::new(v1.master_key.clone()),
                    self_signing_key: Arc::new(v1.self_signing_key),
                    pinned_master_key: Arc::new(RwLock::new(v1.pinned_master_key)),
                    // Put it to false. There will be a migration to mark all users as dirty, so we
                    // will receive an update for the identity that will correctly set up the value.
                    previously_verified: Arc::new(false.into()),
                })
            }
            Some(v) if v == "2" => {
                let v2: OtherUserIdentityDataSerializerV2 = serde_json::from_value(value.other)?;
                Ok(OtherUserIdentityData {
                    user_id: v2.user_id,
                    master_key: Arc::new(v2.master_key.clone()),
                    self_signing_key: Arc::new(v2.self_signing_key),
                    pinned_master_key: Arc::new(RwLock::new(v2.pinned_master_key)),
                    previously_verified: Arc::new(v2.previously_verified.into()),
                })
            }
            _ => Err(serde::de::Error::custom(format!("Unsupported Version {:?}", value.version))),
        }
    }
}

impl From<OtherUserIdentityData> for OtherUserIdentityDataSerializer {
    fn from(value: OtherUserIdentityData) -> Self {
        let v2 = OtherUserIdentityDataSerializerV2 {
            user_id: value.user_id.clone(),
            master_key: value.master_key().to_owned(),
            self_signing_key: value.self_signing_key().to_owned(),
            pinned_master_key: value.pinned_master_key.read().clone(),
            previously_verified: value.previously_verified.load(Ordering::SeqCst),
        };
        OtherUserIdentityDataSerializer {
            version: Some("2".to_owned()),
            other: serde_json::to_value(v2).unwrap(),
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
            previously_verified: Arc::new(false.into()),
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
            previously_verified: Arc::new(false.into()),
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

    /// Remember this identity, ensuring it does not result in a pin violation.
    ///
    /// When we first see a user, we assume their cryptographic identity has not
    /// been tampered with by the homeserver or another entity with
    /// man-in-the-middle capabilities. We remember this identity and call this
    /// action "pinning".
    ///
    /// If the identity presented for the user changes later on, the newly
    /// presented identity is considered to be in "pin violation". This
    /// method explicitly accepts the new identity, allowing it to replace
    /// the previously pinned one and bringing it out of pin violation.
    ///
    /// UIs should display a warning to the user when encountering an identity
    /// which is not verified and is in pin violation. See
    /// [`OtherUserIdentity::identity_needs_user_approval`].
    pub(crate) fn pin(&self) {
        let mut m = self.pinned_master_key.write();
        *m = self.master_key.as_ref().clone()
    }

    /// Remember that this identity used to be verified at some point.
    pub(crate) fn mark_as_previously_verified(&self) {
        self.previously_verified.store(true, Ordering::SeqCst)
    }

    /// True if we verified this identity (with any own identity, at any
    /// point).
    ///
    /// To set this latch back to false, call
    /// [`OtherUserIdentityData::withdraw_verification()`].
    pub fn was_previously_verified(&self) -> bool {
        self.previously_verified.load(Ordering::SeqCst)
    }

    /// Remove the requirement for this identity to be verified.
    ///
    /// If an identity was previously verified and is not anymore it will be
    /// reported to the user. In order to remove this notice users have to
    /// verify again or to withdraw the verification requirement.
    pub fn withdraw_verification(&self) {
        // We also pin when we withdraw, since withdrawing implicitly acknowledges
        // the identity change
        self.pin();
        self.previously_verified.store(false, Ordering::SeqCst)
    }

    /// Returns true if the identity has changed since we last pinned it.
    ///
    /// Key pinning acts as a trust on first use mechanism: the first time an
    /// identity is known for a user it will be pinned.
    ///
    /// For future interaction with a user, the identity is expected to be the
    /// one that was pinned. In case of identity change the UI client should
    /// receive reports of pinning violation and decide to act accordingly:
    /// accept and pin the new identity, perform a verification, or
    /// stop communications.
    pub(crate) fn has_pin_violation(&self) -> bool {
        let pinned_master_key = self.pinned_master_key.read();
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
    /// * `maybe_verified_own_user_signing_key` - Our own user_signing_key if it
    ///   is verified to check the identity trust status after update.
    ///
    /// Returns a `SignatureError` if we failed to update the identity.
    /// Otherwise, returns `true` if there was a change to the identity and
    /// `false` if the identity is unchanged.
    pub(crate) fn update(
        &mut self,
        master_key: MasterPubkey,
        self_signing_key: SelfSigningPubkey,
        maybe_verified_own_user_signing_key: Option<&UserSigningPubkey>,
    ) -> Result<bool, SignatureError> {
        master_key.verify_subkey(&self_signing_key)?;

        // We update the identity with the new master and self signing key, but we keep
        // the previous pinned master key.
        // This identity will have a pin violation until the new master key is pinned
        // (see `has_pin_violation()`).
        let pinned_master_key = self.pinned_master_key.read().clone();

        // Check if the new master_key is signed by our own **verified**
        // user_signing_key. If the identity was verified we remember it.
        let updated_is_verified =
            maybe_verified_own_user_signing_key.is_some_and(|own_user_signing_key| {
                own_user_signing_key.verify_master_key(&master_key).is_ok()
            });

        let new = Self {
            user_id: master_key.user_id().into(),
            master_key: master_key.clone().into(),
            self_signing_key: self_signing_key.into(),
            pinned_master_key: RwLock::new(pinned_master_key).into(),
            previously_verified: Arc::new(
                (self.was_previously_verified() || updated_is_verified).into(),
            ),
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
    /// Returns `true` if the signature check succeeded, otherwise `false`.
    pub(crate) fn is_device_signed(&self, device: &DeviceData) -> bool {
        self.user_id() == device.user_id() && self.self_signing_key.verify_device(device).is_ok()
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
    #[serde(deserialize_with = "deserialize_own_user_identity_data_verified")]
    verified: Arc<RwLock<OwnUserIdentityVerifiedState>>,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum OwnUserIdentityVerifiedState {
    /// We have never verified our own identity
    #[default]
    NeverVerified,

    /// We previously verified this identity, but it has changed.
    #[serde(alias = "PreviouslyVerifiedButNoLonger")]
    VerificationViolation,

    /// We have verified the current identity.
    Verified,
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
            && *self.verified.read() == *other.verified.read()
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
            verified: Default::default(),
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
            verified: Default::default(),
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

    /// Check if the given user identity has been verified.
    ///
    /// The identity of another user is verified iff our own identity is
    /// verified and if our own identity has signed the other user's
    /// identity.
    ///
    /// # Arguments
    ///
    /// * `identity` - The identity of another user which we want to check has
    ///   been verified.
    pub fn is_identity_verified(&self, identity: &OtherUserIdentityData) -> bool {
        self.is_verified() && self.is_identity_signed(identity)
    }

    /// Check if the given identity has been signed by this identity.
    ///
    /// Note that, normally, you'll also want to check that the
    /// `OwnUserIdentityData` has been verified; for that,
    /// [`Self::is_identity_verified`] is more appropriate.
    ///
    /// # Arguments
    ///
    /// * `identity` - The identity of another user that we want to check if it
    ///   has been signed.
    ///
    /// Returns `true` if the signature check succeeded, otherwise `false`.
    pub(crate) fn is_identity_signed(&self, identity: &OtherUserIdentityData) -> bool {
        self.user_signing_key.verify_master_key(&identity.master_key).is_ok()
    }

    /// Check if the given device has been signed by this identity.
    ///
    /// Only devices of our own user should be checked with this method. If a
    /// device of a different user is given, the signature check will always
    /// fail even if a valid signature exists.
    ///
    /// # Arguments
    ///
    /// * `device` - The device that should be checked for a valid signature.
    ///
    /// Returns `true` if the signature check succeeded, otherwise `false`.
    pub(crate) fn is_device_signed(&self, device: &DeviceData) -> bool {
        self.user_id() == device.user_id() && self.self_signing_key.verify_device(device).is_ok()
    }

    /// Mark our identity as verified.
    pub fn mark_as_verified(&self) {
        *self.verified.write() = OwnUserIdentityVerifiedState::Verified;
    }

    /// Mark our identity as unverified.
    pub(crate) fn mark_as_unverified(&self) {
        let mut guard = self.verified.write();
        if *guard == OwnUserIdentityVerifiedState::Verified {
            *guard = OwnUserIdentityVerifiedState::VerificationViolation;
        }
    }

    /// Check if our identity is verified.
    pub fn is_verified(&self) -> bool {
        *self.verified.read() == OwnUserIdentityVerifiedState::Verified
    }

    /// True if we verified our own identity at some point in the past.
    ///
    /// To reset this latch back to `false`, one must call
    /// [`OwnUserIdentityData::withdraw_verification()`].
    pub fn was_previously_verified(&self) -> bool {
        matches!(
            *self.verified.read(),
            OwnUserIdentityVerifiedState::Verified
                | OwnUserIdentityVerifiedState::VerificationViolation
        )
    }

    /// Remove the requirement for this identity to be verified.
    ///
    /// If an identity was previously verified and is not any more it will be
    /// reported to the user. In order to remove this notice users have to
    /// verify again or to withdraw the verification requirement.
    pub fn withdraw_verification(&self) {
        let mut guard = self.verified.write();
        if *guard == OwnUserIdentityVerifiedState::VerificationViolation {
            *guard = OwnUserIdentityVerifiedState::NeverVerified;
        }
    }

    /// Was this identity previously verified, and is no longer?
    ///
    /// Such a violation should be reported to the local user by the
    /// application, and resolved by
    ///
    /// - Verifying the new identity with
    ///   [`OwnUserIdentity::request_verification`]
    /// - Or by withdrawing the verification requirement
    ///   [`OwnUserIdentity::withdraw_verification`].
    pub fn has_verification_violation(&self) -> bool {
        *self.verified.read() == OwnUserIdentityVerifiedState::VerificationViolation
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
            self.mark_as_unverified()
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
                (device_id != own_device_id && self.is_device_signed(&device)).then_some(device_id)
            })
            .collect()
    }
}

/// Custom deserializer for [`OwnUserIdentityData::verified`].
///
/// This used to be a bool, so we need to handle that.
fn deserialize_own_user_identity_data_verified<'de, D>(
    de: D,
) -> Result<Arc<RwLock<OwnUserIdentityVerifiedState>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum VerifiedStateOrBool {
        VerifiedState(OwnUserIdentityVerifiedState),
        Bool(bool),
    }

    let verified_state = match VerifiedStateOrBool::deserialize(de)? {
        VerifiedStateOrBool::Bool(true) => OwnUserIdentityVerifiedState::Verified,
        VerifiedStateOrBool::Bool(false) => OwnUserIdentityVerifiedState::NeverVerified,
        VerifiedStateOrBool::VerifiedState(x) => x,
    };

    Ok(Arc::new(RwLock::new(verified_state)))
}

/// Testing Facilities
#[cfg(any(test, feature = "testing"))]
#[allow(dead_code)]
pub(crate) mod testing {
    use matrix_sdk_test::ruma_response_from_json;
    use ruma::{
        UserId,
        api::client::keys::{
            get_keys::v3::Response as KeyQueryResponse,
            upload_signatures::v3::Request as SignatureUploadRequest,
        },
        user_id,
    };
    use serde_json::json;

    use super::{OtherUserIdentityData, OwnUserIdentity, OwnUserIdentityData};
    #[cfg(test)]
    use crate::{identities::manager::testing::other_user_id, olm::PrivateCrossSigningIdentity};
    use crate::{
        identities::{
            DeviceData,
            manager::testing::{other_key_query, own_key_query},
        },
        store::Store,
        types::CrossSigningKey,
        verification::VerificationMachine,
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

    pub fn own_identity_wrapped(
        inner: OwnUserIdentityData,
        verification_machine: VerificationMachine,
        store: Store,
    ) -> OwnUserIdentity {
        OwnUserIdentity { inner, verification_machine, store }
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

    /// When we want to test identities that are verified, we need to simulate
    /// the verification process. This function supports that by simulating
    /// what happens when a successful verification dance happens and
    /// providing the /keys/query response we would get when that happened.
    ///
    /// signature_upload_request will be the result of calling
    /// [`super::OtherUserIdentity::verify`].
    ///
    /// # Example
    ///
    /// ```ignore
    /// let signature_upload_request = their_identity.verify().await.unwrap();
    ///
    /// let msk_json = json!({
    ///     "their_user_id": {
    ///         "keys": { "ed25519:blah": "blah" }
    ///         "signatures": {
    ///             "their_user_id": { "ed25519:blah": "blah", ... }
    ///         }
    ///         "usage": [ "master" ],
    ///         "user_id": "their_user_id"
    ///     }
    /// });
    ///
    /// let ssk_json = json!({
    ///     "their_user_id": {
    ///         "keys": { "ed25519:blah": "blah" },
    ///         "signatures": {
    ///             "their_user_id": { "ed25519:blah": "blah" }
    ///         },
    ///         "usage": [ "self_signing" ],
    ///         "user_id": "their_user_id"
    ///     }
    /// })
    ///
    /// let response = simulate_key_query_response_for_verification(
    ///     signature_upload_request,
    ///     my_identity,
    ///     my_user_id,
    ///     their_user_id,
    ///     msk_json,
    ///     ssk_json
    /// ).await;
    ///
    /// olm_machine
    ///     .mark_request_as_sent(
    ///         &TransactionId::new(),
    ///         crate::IncomingResponse::KeysQuery(&kq_response),
    ///     )
    ///     .await
    ///     .unwrap();
    /// ```
    pub fn simulate_key_query_response_for_verification(
        signature_upload_request: SignatureUploadRequest,
        my_identity: OwnUserIdentity,
        my_user_id: &UserId,
        their_user_id: &UserId,
        msk_json: serde_json::Value,
        ssk_json: serde_json::Value,
    ) -> KeyQueryResponse {
        // Find the signed key inside the SignatureUploadRequest
        let cross_signing_key: CrossSigningKey = serde_json::from_str(
            signature_upload_request
                .signed_keys
                .get(their_user_id)
                .expect("Signature upload request should contain a key for their user ID")
                .iter()
                .next()
                .expect("There should be a key in the signature upload request")
                .1
                .get(),
        )
        .expect("Should not fail to deserialize the key");

        // Find their master key that we want to update inside their msk JSON
        let mut their_msk: CrossSigningKey = serde_json::from_value(
            msk_json.get(their_user_id.as_str()).expect("msk should contain their user ID").clone(),
        )
        .expect("Should not fail to deserialize msk");

        // Find our own user signing key
        let my_user_signing_key_id = my_identity
            .user_signing_key()
            .keys()
            .iter()
            .next()
            .expect("There should be a user signing key")
            .0;

        // Add the signature from the SignatureUploadRequest to their master key, under
        // our user ID
        their_msk.signatures.add_signature(
            my_user_id.to_owned(),
            my_user_signing_key_id.to_owned(),
            cross_signing_key
                .signatures
                .get_signature(my_user_id, my_user_signing_key_id)
                .expect("There should be a signature for our user"),
        );

        // Create a JSON response as if the verification has happened
        ruma_response_from_json(&json!({
            "device_keys": {}, // Don't need devices here, even though they would exist
            "failures": {},
            "master_keys": {
                their_user_id: their_msk,
            },
            "self_signing_keys": ssk_json,
        }))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{collections::HashMap, sync::Arc};

    use assert_matches::assert_matches;
    use matrix_sdk_test::{async_test, test_json};
    use ruma::{TransactionId, device_id, user_id};
    use serde_json::{Value, json};
    use tokio::sync::Mutex;

    use super::{
        OtherUserIdentityDataSerializerV2, OwnUserIdentityData, OwnUserIdentityVerifiedState,
        UserIdentityData,
        testing::{device, get_other_identity, get_own_identity},
    };
    use crate::{
        CrossSigningKeyExport, OlmMachine, OtherUserIdentity, OtherUserIdentityData,
        identities::{
            Device,
            manager::testing::own_key_query,
            user::{
                OtherUserIdentityDataSerializer,
                testing::simulate_key_query_response_for_verification,
            },
        },
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

        let pinned_master_key = migrated.pinned_master_key.read();
        assert_eq!(*pinned_master_key, migrated.master_key().clone());

        // Serialize back
        let value = serde_json::to_value(migrated.clone()).unwrap();

        // Should be serialized with latest version
        let _: OtherUserIdentityDataSerializerV2 =
            serde_json::from_value(value.clone()).expect("Should deserialize as version 2");

        let with_serializer: OtherUserIdentityDataSerializer =
            serde_json::from_value(value).unwrap();
        assert_eq!("2", with_serializer.version.unwrap());
    }

    /// [`OwnUserIdentityData::verified`] was previously an AtomicBool. Check
    /// that we can deserialize boolean values.
    #[test]
    fn test_deserialize_own_user_identity_bool_verified() {
        let mut json = own_user_identity_data();

        // Set `"verified": false`
        *json.get_mut("verified").unwrap() = false.into();
        let id: OwnUserIdentityData = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(*id.verified.read(), OwnUserIdentityVerifiedState::NeverVerified);

        // Tweak the json to have `"verified": true`, and repeat
        *json.get_mut("verified").unwrap() = true.into();
        let id: OwnUserIdentityData = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(*id.verified.read(), OwnUserIdentityVerifiedState::Verified);
    }

    #[test]
    fn test_own_user_identity_verified_state_verification_violation_deserializes() {
        // Given data containing verified: VerificationViolation
        let mut json = own_user_identity_data();
        *json.get_mut("verified").unwrap() = "VerificationViolation".into();

        // When we deserialize
        let id: OwnUserIdentityData = serde_json::from_value(json.clone()).unwrap();

        // Then the value is correctly populated
        assert_eq!(*id.verified.read(), OwnUserIdentityVerifiedState::VerificationViolation);
    }

    #[test]
    fn test_own_user_identity_verified_state_previously_verified_deserializes() {
        // Given data containing verified: PreviouslyVerifiedButNoLonger
        let mut json = own_user_identity_data();
        *json.get_mut("verified").unwrap() = "PreviouslyVerifiedButNoLonger".into();

        // When we deserialize
        let id: OwnUserIdentityData = serde_json::from_value(json.clone()).unwrap();

        // Then the old value is re-interpreted as VerificationViolation
        assert_eq!(*id.verified.read(), OwnUserIdentityVerifiedState::VerificationViolation);
    }

    #[test]
    fn own_identity_check_signatures() {
        let response = own_key_query();
        let identity = get_own_identity();
        let (first, second) = device(&response);

        assert!(!identity.is_device_signed(&first));
        assert!(identity.is_device_signed(&second));

        let account = Account::with_device_id(second.user_id(), second.device_id());
        let verification_machine = get_verification_machine(&account);

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
    async fn test_own_device_with_private_identity() {
        let response = own_key_query();
        let (_, device) = device(&response);

        let account = Account::with_device_id(device.user_id(), device.device_id());
        let verification_machine = get_verification_machine(&account);
        let public_identity = verification_machine.get_own_user_identity_data().await.unwrap();

        let mut device = Device {
            inner: device,
            verification_machine: verification_machine.clone(),
            own_identity: Some(public_identity.clone()),
            device_owner_identity: Some(public_identity.clone().into()),
        };

        assert!(!device.is_verified());

        let mut device_keys = device.as_device_keys().to_owned();

        let identity = verification_machine.store.private_identity.lock().await;
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
    async fn test_resolve_identity_pin_violation_with_verification() {
        use test_json::keys_query_sets::IdentityChangeDataSet as DataSet;

        let my_user_id = user_id!("@me:localhost");
        let machine = OlmMachine::new(my_user_id, device_id!("ABCDEFGH")).await;
        machine.bootstrap_cross_signing(false).await.unwrap();

        let my_id = machine.get_identity(my_user_id, None).await.unwrap().unwrap().own().unwrap();

        let keys_query = DataSet::key_query_with_identity_a();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        // Simulate an identity change
        let keys_query = DataSet::key_query_with_identity_b();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let other_user_id = DataSet::user_id();

        let other_identity =
            machine.get_identity(other_user_id, None).await.unwrap().unwrap().other().unwrap();

        // The identity should need user approval now
        assert!(other_identity.identity_needs_user_approval());

        // Manually verify for the purpose of this test
        let sig_upload = other_identity.verify().await.unwrap();

        let kq_response = simulate_key_query_response_for_verification(
            sig_upload,
            my_id,
            my_user_id,
            other_user_id,
            DataSet::master_signing_keys_b(),
            DataSet::self_signing_keys_b(),
        );
        machine.mark_request_as_sent(&TransactionId::new(), &kq_response).await.unwrap();

        // The identity should not need any user approval now
        let other_identity =
            machine.get_identity(other_user_id, None).await.unwrap().unwrap().other().unwrap();
        assert!(!other_identity.identity_needs_user_approval());
        // But there is still a pin violation
        assert!(other_identity.inner.has_pin_violation());
    }

    #[async_test]
    async fn test_resolve_identity_pin_violation_with_withdraw_verification() {
        use test_json::keys_query_sets::IdentityChangeDataSet as DataSet;

        let my_user_id = user_id!("@me:localhost");
        let machine = OlmMachine::new(my_user_id, device_id!("ABCDEFGH")).await;
        machine.bootstrap_cross_signing(false).await.unwrap();

        let keys_query = DataSet::key_query_with_identity_a();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        // Simulate an identity change
        let keys_query = DataSet::key_query_with_identity_b();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let other_user_id = DataSet::user_id();

        let other_identity =
            machine.get_identity(other_user_id, None).await.unwrap().unwrap().other().unwrap();

        // For testing purpose mark it as previously verified
        other_identity.mark_as_previously_verified().await.unwrap();

        // The identity should need user approval now
        assert!(other_identity.identity_needs_user_approval());

        // We withdraw verification
        other_identity.withdraw_verification().await.unwrap();

        // The identity should not need any user approval now
        let other_identity =
            machine.get_identity(other_user_id, None).await.unwrap().unwrap().other().unwrap();
        assert!(!other_identity.identity_needs_user_approval());
        // And should not have a pin violation
        assert!(!other_identity.inner.has_pin_violation());
    }

    #[async_test]
    async fn test_resolve_identity_verification_violation_with_withdraw() {
        use test_json::keys_query_sets::VerificationViolationTestData as DataSet;

        let machine = OlmMachine::new(DataSet::own_id(), device_id!("LOCAL")).await;

        let keys_query = DataSet::own_keys_query_response_1();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        machine
            .import_cross_signing_keys(CrossSigningKeyExport {
                master_key: DataSet::MASTER_KEY_PRIVATE_EXPORT.to_owned().into(),
                self_signing_key: DataSet::SELF_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
                user_signing_key: DataSet::USER_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
            })
            .await
            .unwrap();

        let keys_query = DataSet::bob_keys_query_response_rotated();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let bob_identity =
            machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap().other().unwrap();

        // For testing purpose mark it as previously verified
        bob_identity.mark_as_previously_verified().await.unwrap();

        assert!(bob_identity.has_verification_violation());

        // withdraw
        bob_identity.withdraw_verification().await.unwrap();

        let bob_identity =
            machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap().other().unwrap();

        assert!(!bob_identity.has_verification_violation());
    }

    #[async_test]
    async fn test_reset_own_keys_creates_verification_violation() {
        use test_json::keys_query_sets::VerificationViolationTestData as DataSet;

        let machine = OlmMachine::new(DataSet::own_id(), device_id!("LOCAL")).await;

        let keys_query = DataSet::own_keys_query_response_1();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        machine
            .import_cross_signing_keys(CrossSigningKeyExport {
                master_key: DataSet::MASTER_KEY_PRIVATE_EXPORT.to_owned().into(),
                self_signing_key: DataSet::SELF_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
                user_signing_key: DataSet::USER_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
            })
            .await
            .unwrap();

        let keys_query = DataSet::bob_keys_query_response_signed();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let bob_identity =
            machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap().other().unwrap();

        // For testing purpose mark it as previously verified
        bob_identity.mark_as_previously_verified().await.unwrap();

        assert!(!bob_identity.has_verification_violation());

        let _ = machine.bootstrap_cross_signing(true).await.unwrap();

        let bob_identity =
            machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap().other().unwrap();

        assert!(bob_identity.has_verification_violation());
    }

    /// Test that receiving new public keys for our own identity causes a
    /// verification violation on our own identity.
    #[async_test]
    async fn test_own_keys_update_creates_own_identity_verification_violation() {
        use test_json::keys_query_sets::VerificationViolationTestData as DataSet;

        let machine = OlmMachine::new(DataSet::own_id(), device_id!("LOCAL")).await;

        // Start with our own identity verified
        let own_keys = DataSet::own_keys_query_response_1();
        machine.mark_request_as_sent(&TransactionId::new(), &own_keys).await.unwrap();

        machine
            .import_cross_signing_keys(CrossSigningKeyExport {
                master_key: DataSet::MASTER_KEY_PRIVATE_EXPORT.to_owned().into(),
                self_signing_key: DataSet::SELF_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
                user_signing_key: DataSet::USER_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
            })
            .await
            .unwrap();

        // Double-check that we have a verified identity
        let own_identity = machine.get_identity(DataSet::own_id(), None).await.unwrap().unwrap();
        assert!(own_identity.is_verified());
        assert!(own_identity.was_previously_verified());
        assert!(!own_identity.has_verification_violation());

        // Now, we receive a *different* set of public keys
        let own_keys = DataSet::own_keys_query_response_2();
        machine.mark_request_as_sent(&TransactionId::new(), &own_keys).await.unwrap();

        // That should give an identity that is no longer verified, with a verification
        // violation.
        let own_identity = machine.get_identity(DataSet::own_id(), None).await.unwrap().unwrap();
        assert!(!own_identity.is_verified());
        assert!(own_identity.was_previously_verified());
        assert!(own_identity.has_verification_violation());

        // Now check that we can withdraw verification for our own identity, and that it
        // becomes valid again.
        own_identity.withdraw_verification().await.unwrap();

        assert!(!own_identity.is_verified());
        assert!(!own_identity.was_previously_verified());
        assert!(!own_identity.has_verification_violation());
    }

    fn own_user_identity_data() -> Value {
        json!({
            "user_id": "@example:localhost",
            "master_key": {
                "user_id":"@example:localhost",
                "usage":["master"],
                "keys":{"ed25519:rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0":"rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0"},
            },
            "self_signing_key": {
                "user_id":"@example:localhost",
                "usage":["self_signing"],
                "keys":{"ed25519:0C8lCBxrvrv/O7BQfsKnkYogHZX3zAgw3RfJuyiq210":"0C8lCBxrvrv/O7BQfsKnkYogHZX3zAgw3RfJuyiq210"}
            },
            "user_signing_key": {
                "user_id":"@example:localhost",
                "usage":["user_signing"],
                "keys":{"ed25519:DU9z4gBFKFKCk7a13sW9wjT0Iyg7Hqv5f0BPM7DEhPo":"DU9z4gBFKFKCk7a13sW9wjT0Iyg7Hqv5f0BPM7DEhPo"}
            },
            "verified": false
        })
    }

    #[async_test]
    async fn test_other_user_identity_verification_request_content() {
        let other_user_identity = other_user_identity().await;
        let verification_request_content = other_user_identity.verification_request_content(None);
        let mut verification_request_content_json =
            serde_json::to_value(verification_request_content)
                .expect("Could not serialize verification request content");

        // Remove the body which is a pain to match
        let verification_request_content_object = verification_request_content_json
            .as_object_mut()
            .expect("serialized verification request was not an object");
        verification_request_content_object.remove("body").expect("No `body` in message content");

        // The methods are variable too
        let methods = verification_request_content_object
            .remove("methods")
            .expect("No `methods` in message content");
        let methods = methods.as_array().expect("`methods` was not an array");
        assert!(methods.contains(&json!("m.sas.v1")));
        assert!(methods.contains(&json!("m.reciprocate.v1")));

        assert_eq!(
            verification_request_content_json,
            json!({
                "msgtype": "m.key.verification.request",
                "from_device": "DEV123",
                "to": other_user_identity.user_id().to_string(),
            })
        );
    }

    /// Create an [`OtherUserIdentity`] for use in tests
    async fn other_user_identity() -> OtherUserIdentity {
        use ruma::owned_device_id;

        let other_user_identity_data = get_other_identity();

        let account =
            Account::with_device_id(user_id!("@own_user:localhost"), &owned_device_id!("DEV123"));

        let verification_machine = get_verification_machine(&account);
        let own_identity_data = verification_machine.get_own_user_identity_data().await.unwrap();

        OtherUserIdentity {
            inner: other_user_identity_data,
            own_identity: Some(own_identity_data),
            verification_machine,
        }
    }

    /**
     * Create a minimal [`VerificationMachine`] for the given account,
     * backed by a [`MemoryStore`].
     *
     * Creates a new private user identity for the account.
     */
    fn get_verification_machine(account: &Account) -> VerificationMachine {
        let private_identity = PrivateCrossSigningIdentity::for_account(account);
        VerificationMachine::new(
            account.static_data().clone(),
            Arc::new(Mutex::new(private_identity)),
            Arc::new(CryptoStoreWrapper::new(
                account.user_id(),
                account.device_id(),
                MemoryStore::new(),
            )),
        )
    }
}
