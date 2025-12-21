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
    collections::{BTreeMap, HashMap},
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use matrix_sdk_common::locks::RwLock;
use ruma::{
    DeviceId, DeviceKeyAlgorithm, DeviceKeyId, MilliSecondsSinceUnixEpoch, OwnedDeviceId,
    OwnedDeviceKeyId, UInt, UserId,
    api::client::keys::upload_signatures::v3::Request as SignatureUploadRequest,
    events::{AnyToDeviceEventContent, key::verification::VerificationMethod},
    serde::Raw,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{instrument, trace};
use vodozemac::{Curve25519PublicKey, Ed25519PublicKey, olm::SessionConfig};

use super::{atomic_bool_deserializer, atomic_bool_serializer};
#[cfg(any(test, feature = "testing", doc))]
use crate::OlmMachine;
use crate::{
    Account, Sas, VerificationRequest,
    error::{MismatchedIdentityKeysError, OlmError, OlmResult, SignatureError},
    identities::{OwnUserIdentityData, UserIdentityData},
    olm::{InboundGroupSession, OutboundGroupSession, Session, ShareInfo, VerifyJson},
    session_manager::{CollectStrategy, withheld_code_for_device_for_share_strategy},
    store::{
        CryptoStoreWrapper, Result as StoreResult,
        caches::SequenceNumber,
        types::{Changes, DeviceChanges},
    },
    types::{
        DeviceKey, DeviceKeys, EventEncryptionAlgorithm, Signatures, SignedKey,
        events::{
            EventType, forwarded_room_key::ForwardedRoomKeyContent,
            room::encrypted::ToDeviceEncryptedEventContent,
        },
        requests::{OutgoingVerificationRequest, ToDeviceRequest},
    },
    verification::VerificationMachine,
};

pub enum MaybeEncryptedRoomKey {
    Encrypted {
        // `Box` the session to reduce the size of `Encrypted`.
        used_session: Box<Session>,
        // `Box` the session to reduce the size of `Encrypted`.
        share_info: Box<ShareInfo>,
        message: Raw<AnyToDeviceEventContent>,
    },
    /// We could not encrypt a message to this device because there is no active
    /// Olm session.
    MissingSession,
}

/// A read-only version of a `Device`.
#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceData {
    #[serde(alias = "inner")]
    pub(crate) device_keys: Arc<DeviceKeys>,
    #[serde(
        serialize_with = "atomic_bool_serializer",
        deserialize_with = "atomic_bool_deserializer"
    )]
    deleted: Arc<AtomicBool>,
    trust_state: Arc<RwLock<LocalTrust>>,
    /// Flag remembering if we successfully sent an `m.no_olm` withheld code to
    /// this device.
    #[serde(
        default,
        serialize_with = "atomic_bool_serializer",
        deserialize_with = "atomic_bool_deserializer"
    )]
    withheld_code_sent: Arc<AtomicBool>,
    /// First time this device was seen in milliseconds since epoch.
    /// Default to epoch for migration purpose.
    #[serde(default = "default_timestamp")]
    first_time_seen_ts: MilliSecondsSinceUnixEpoch,
    /// The number of times the device has tried to unwedge Olm sessions with
    /// us.
    #[serde(default)]
    pub(crate) olm_wedging_index: SequenceNumber,
}

fn default_timestamp() -> MilliSecondsSinceUnixEpoch {
    MilliSecondsSinceUnixEpoch(UInt::default())
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for DeviceData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceData")
            .field("user_id", &self.user_id())
            .field("device_id", &self.device_id())
            .field("display_name", &self.display_name())
            .field("keys", self.keys())
            .field("deleted", &self.deleted.load(Ordering::SeqCst))
            .field("trust_state", &self.trust_state)
            .field("withheld_code_sent", &self.withheld_code_sent)
            .finish()
    }
}

/// A device represents a E2EE capable client of an user.
#[derive(Clone)]
pub struct Device {
    pub(crate) inner: DeviceData,
    pub(crate) verification_machine: VerificationMachine,
    pub(crate) own_identity: Option<OwnUserIdentityData>,
    pub(crate) device_owner_identity: Option<UserIdentityData>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device").field("device", &self.inner).finish()
    }
}

impl Deref for Device {
    type Target = DeviceData;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Device {
    /// Start a interactive verification with this `Device`
    ///
    /// Returns a `Sas` object and a to-device request that needs to be sent
    /// out.
    ///
    /// This method has been deprecated in the spec and the
    /// [`request_verification()`] method should be used instead.
    ///
    /// [`request_verification()`]: #method.request_verification
    pub async fn start_verification(&self) -> StoreResult<(Sas, ToDeviceRequest)> {
        let (sas, request) = self.verification_machine.start_sas(self.inner.clone()).await?;

        if let OutgoingVerificationRequest::ToDevice(r) = request {
            Ok((sas, r))
        } else {
            panic!("Invalid verification request type");
        }
    }

    /// Is this our own device?
    pub fn is_our_own_device(&self) -> bool {
        let own_ed25519_key = self.verification_machine.store.account.identity_keys.ed25519;
        let own_curve25519_key = self.verification_machine.store.account.identity_keys.curve25519;

        self.user_id() == self.verification_machine.own_user_id()
            && self.device_id() == self.verification_machine.own_device_id()
            && self.ed25519_key().is_some_and(|k| k == own_ed25519_key)
            && self.curve25519_key().is_some_and(|k| k == own_curve25519_key)
    }

    /// Does the given `InboundGroupSession` belong to this device?
    ///
    /// An `InboundGroupSession` is exchanged between devices as an Olm
    /// encrypted `m.room_key` event. This method determines if this `Device`
    /// can be confirmed as the creator and owner of the `m.room_key`.
    pub fn is_owner_of_session(
        &self,
        session: &InboundGroupSession,
    ) -> Result<bool, MismatchedIdentityKeysError> {
        if session.has_been_imported() {
            // An imported room key means that we did not receive the room key as a
            // `m.room_key` event when the room key was initially exchanged.
            //
            // This could mean a couple of things:
            //      1. We received the room key as a `m.forwarded_room_key`.
            //      2. We imported the room key through a file export.
            //      3. We imported the room key through a backup.
            //
            // To be certain that a `Device` is the owner of a room key we need to have a
            // proof that the `Curve25519` key of this `Device` was used to
            // initially exchange the room key. This proof is provided by the Olm decryption
            // step, see below for further clarification.
            //
            // Each of the above room key methods that receive room keys do not contain this
            // proof and we received only a claim that the room key is tied to a
            // `Curve25519` key.
            //
            // Since there's no way to verify that the claim is true, we say that we don't
            // know that the room key belongs to this device.
            Ok(false)
        } else if let Some(key) =
            session.signing_keys().get(&DeviceKeyAlgorithm::Ed25519).and_then(|k| k.ed25519())
        {
            // Room keys are received as an `m.room.encrypted` to-device message using the
            // `m.olm` algorithm. Upon decryption of the `m.room.encrypted` to-device
            // message, the decrypted content will contain also an `Ed25519` public key[1].
            //
            // The inclusion of this key means that the `Curve25519` key of the `Device` and
            // Olm `Session`, established using the DH authentication of the
            // double ratchet, "binds" the `Ed25519` key of the `Device`. In other words, it
            // prevents an attack in which Mallory publishes Bob's public `Curve25519` key
            // as her own, and subsequently forwards an Olm message she received from Bob to
            // Alice, claiming that she, Mallory, originated the Olm message (leading Alice
            // to believe that Mallory also sent the messages in the subsequent Megolm
            // session).
            //
            // On the other hand, the `Ed25519` key binds the `Curve25519` key
            // using a signature which is uploaded to the server as
            // `device_keys` and downloaded by us using a `/keys/query` request.
            //
            // A `Device` is considered to be the owner of a room key iff:
            //     1. The `Curve25519` key that was used to establish the Olm `Session` that
            //        was used to decrypt the to-device message is binding the `Ed25519` key
            //        of this `Device` via the content of the to-device message, and:
            //     2. The `Ed25519` key of this device has signed a `device_keys` object
            //        that contains the `Curve25519` key from step 1.
            //
            // We don't need to check the signature of the `Device` here, since we don't
            // accept a `Device` unless it has a valid `Ed25519` signature.
            //
            // We do check that the `Curve25519` that was used to decrypt the event carrying
            // the `m.room_key` and the `Ed25519` key that was part of the
            // decrypted content matches the keys found in this `Device`.
            //
            // ```text
            //                                              ┌───────────────────────┐
            //                                              │ EncryptedToDeviceEvent│
            //                                              └───────────────────────┘
            //                                                         │
            //    ┌──────────────────────────────────┐                 │
            //    │              Device              │                 ▼
            //    ├──────────────────────────────────┤        ┌──────────────────┐
            //    │            Device Keys           │        │      Session     │
            //    ├────────────────┬─────────────────┤        ├──────────────────┤
            //    │   Ed25519 Key  │  Curve25519 Key │◄──────►│  Curve25519 Key  │
            //    └────────────────┴─────────────────┘        └──────────────────┘
            //            ▲                                            │
            //            │                                            │
            //            │                                            │ Decrypt
            //            │                                            │
            //            │                                            ▼
            //            │                                 ┌───────────────────────┐
            //            │                                 │  DecryptedOlmV1Event  │
            //            │                                 ├───────────────────────┤
            //            │                                 │         keys          │
            //            │                                 ├───────────────────────┤
            //            └────────────────────────────────►│       Ed25519 Key     │
            //                                              └───────────────────────┘
            // ```
            //
            // [1]: https://spec.matrix.org/v1.5/client-server-api/#molmv1curve25519-aes-sha2
            let ed25519_comparison = self.ed25519_key().map(|k| k == key);
            let curve25519_comparison = self.curve25519_key().map(|k| k == session.sender_key());

            match (ed25519_comparison, curve25519_comparison) {
                // If we have any of the keys but they don't turn out to match, refuse to decrypt
                // instead.
                (_, Some(false)) | (Some(false), _) => Err(MismatchedIdentityKeysError {
                    key_ed25519: key.into(),
                    device_ed25519: self.ed25519_key().map(Into::into),
                    key_curve25519: session.sender_key().into(),
                    device_curve25519: self.curve25519_key().map(Into::into),
                }),
                // If both keys match, we have ourselves an owner.
                (Some(true), Some(true)) => Ok(true),
                // In the remaining cases, the device is missing at least one of the required
                // identity keys, so we default to a negative answer.
                _ => Ok(false),
            }
        } else {
            Ok(false)
        }
    }

    /// Is this device cross signed by its owner?
    pub fn is_cross_signed_by_owner(&self) -> bool {
        self.device_owner_identity
            .as_ref()
            .is_some_and(|owner_identity| self.inner.is_cross_signed_by_owner(owner_identity))
    }

    /// Is the device owner verified by us?
    pub fn is_device_owner_verified(&self) -> bool {
        self.device_owner_identity.as_ref().is_some_and(|id| match id {
            UserIdentityData::Own(own_identity) => own_identity.is_verified(),
            UserIdentityData::Other(other_identity) => {
                self.own_identity.as_ref().is_some_and(|oi| oi.is_identity_verified(other_identity))
            }
        })
    }

    /// Request an interactive verification with this `Device`.
    ///
    /// Returns a `VerificationRequest` object and a to-device request that
    /// needs to be sent out.
    pub fn request_verification(&self) -> (VerificationRequest, OutgoingVerificationRequest) {
        self.request_verification_helper(None)
    }

    /// Request an interactive verification with this `Device`.
    ///
    /// Returns a `VerificationRequest` object and a to-device request that
    /// needs to be sent out.
    ///
    /// # Arguments
    ///
    /// * `methods` - The verification methods that we want to support.
    pub fn request_verification_with_methods(
        &self,
        methods: Vec<VerificationMethod>,
    ) -> (VerificationRequest, OutgoingVerificationRequest) {
        self.request_verification_helper(Some(methods))
    }

    fn request_verification_helper(
        &self,
        methods: Option<Vec<VerificationMethod>>,
    ) -> (VerificationRequest, OutgoingVerificationRequest) {
        self.verification_machine.request_to_device_verification(
            self.user_id(),
            vec![self.device_id().to_owned()],
            methods,
        )
    }

    /// Get the most recently created session that belongs to this device.
    pub(crate) async fn get_most_recent_session(&self) -> OlmResult<Option<Session>> {
        self.inner.get_most_recent_session(self.verification_machine.store.inner()).await
    }

    /// Is this device considered to be verified.
    ///
    /// This method returns true if either [`is_locally_trusted()`] returns true
    /// or if [`is_cross_signing_trusted()`] returns true.
    ///
    /// [`is_locally_trusted()`]: #method.is_locally_trusted
    /// [`is_cross_signing_trusted()`]: #method.is_cross_signing_trusted
    pub fn is_verified(&self) -> bool {
        self.inner.is_verified(&self.own_identity, &self.device_owner_identity)
    }

    /// Is this device considered to be verified using cross signing.
    pub fn is_cross_signing_trusted(&self) -> bool {
        self.inner.is_cross_signing_trusted(&self.own_identity, &self.device_owner_identity)
    }

    /// Manually verify this device.
    ///
    /// This method will attempt to sign the device using our private cross
    /// signing key.
    ///
    /// This method will always fail if the device belongs to someone else, we
    /// can only sign our own devices.
    ///
    /// It can also fail if we don't have the private part of our self-signing
    /// key.
    ///
    /// Returns a request that needs to be sent out for the device to be marked
    /// as verified.
    pub async fn verify(&self) -> Result<SignatureUploadRequest, SignatureError> {
        if self.user_id() == self.verification_machine.own_user_id() {
            Ok(self
                .verification_machine
                .store
                .private_identity
                .lock()
                .await
                .sign_device(&self.inner)
                .await?)
        } else {
            Err(SignatureError::UserIdMismatch)
        }
    }

    /// Set the local trust state of the device to the given state.
    ///
    /// This won't affect any cross signing trust state, this only sets a flag
    /// marking to have the given trust state.
    ///
    /// # Arguments
    ///
    /// * `trust_state` - The new trust state that should be set for the device.
    pub async fn set_local_trust(&self, trust_state: LocalTrust) -> StoreResult<()> {
        self.inner.set_trust_state(trust_state);

        let changes = Changes {
            devices: DeviceChanges { changed: vec![self.inner.clone()], ..Default::default() },
            ..Default::default()
        };

        self.verification_machine.store.save_changes(changes).await
    }

    /// Encrypt the given content for this `Device`.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the event that should be encrypted.
    pub(crate) async fn encrypt(
        &self,
        event_type: &str,
        content: impl Serialize,
    ) -> OlmResult<(Session, Raw<ToDeviceEncryptedEventContent>)> {
        self.inner.encrypt(self.verification_machine.store.inner(), event_type, content).await
    }

    /// Encrypt the given inbound group session as a forwarded room key for this
    /// device.
    pub async fn encrypt_room_key_for_forwarding(
        &self,
        session: InboundGroupSession,
        message_index: Option<u32>,
    ) -> OlmResult<(Session, Raw<ToDeviceEncryptedEventContent>)> {
        let content: ForwardedRoomKeyContent = {
            let export = if let Some(index) = message_index {
                session.export_at_index(index).await
            } else {
                session.export().await
            };

            export.try_into()?
        };

        let event_type = content.event_type().to_owned();

        self.encrypt(&event_type, content).await
    }

    /// Encrypt an event for this device.
    ///
    /// Beware that the 1-to-1 session must be established prior to this
    /// call by using the [`OlmMachine::get_missing_sessions`] method.
    ///
    /// Notable limitation: The caller is responsible for sending the encrypted
    /// event to the target device, this encryption method supports out-of-order
    /// messages to a certain extent (2000 messages), if multiple messages are
    /// encrypted using this method they should be sent in the same order as
    /// they are encrypted.
    ///
    /// *Note*: To instead encrypt an event meant for a room use the
    /// [`OlmMachine::encrypt_room_event()`] method instead.
    ///
    /// # Arguments
    /// * `event_type` - The type of the event to be sent.
    /// * `content` - The content of the event to be sent. This should be a type
    ///   that implements the `Serialize` trait.
    /// * `share_strategy` - The share strategy to use to determine whether we
    ///   should encrypt to the device.
    ///
    /// # Returns
    ///
    /// The encrypted raw content to be shared with your preferred transport
    /// layer (usually to-device), [`OlmError::MissingSession`] if there is
    /// no established session with the device.
    pub async fn encrypt_event_raw(
        &self,
        event_type: &str,
        content: &Value,
        share_strategy: CollectStrategy,
    ) -> OlmResult<Raw<ToDeviceEncryptedEventContent>> {
        if let Some(withheld_code) = withheld_code_for_device_for_share_strategy(
            &self.inner,
            share_strategy,
            &self.own_identity,
            &self.device_owner_identity,
        )
        .await?
        {
            return Err(OlmError::Withheld(withheld_code));
        }

        let (used_session, raw_encrypted) = self.encrypt(event_type, content).await?;

        // Persist the used session
        self.verification_machine
            .store
            .save_changes(Changes { sessions: vec![used_session], ..Default::default() })
            .await?;

        Ok(raw_encrypted)
    }

    /// True if this device is an [MSC3814](https://github.com/matrix-org/matrix-spec-proposals/pull/3814) dehydrated device.
    pub fn is_dehydrated(&self) -> bool {
        self.inner.is_dehydrated()
    }
}

/// A read only view over all devices belonging to a user.
#[derive(Debug)]
pub struct UserDevices {
    pub(crate) inner: HashMap<OwnedDeviceId, DeviceData>,
    pub(crate) verification_machine: VerificationMachine,
    pub(crate) own_identity: Option<OwnUserIdentityData>,
    pub(crate) device_owner_identity: Option<UserIdentityData>,
}

impl UserDevices {
    /// Get the specific device with the given device ID.
    pub fn get(&self, device_id: &DeviceId) -> Option<Device> {
        self.inner.get(device_id).map(|d| Device {
            inner: d.clone(),
            verification_machine: self.verification_machine.clone(),
            own_identity: self.own_identity.clone(),
            device_owner_identity: self.device_owner_identity.clone(),
        })
    }

    fn own_user_id(&self) -> &UserId {
        self.verification_machine.own_user_id()
    }

    fn own_device_id(&self) -> &DeviceId {
        self.verification_machine.own_device_id()
    }

    /// Returns true if there is at least one devices of this user that is
    /// considered to be verified, false otherwise.
    ///
    /// This won't consider your own device as verified, as your own device is
    /// always implicitly verified.
    pub fn is_any_verified(&self) -> bool {
        self.inner
            .values()
            .filter(|d| {
                !(d.user_id() == self.own_user_id() && d.device_id() == self.own_device_id())
            })
            .any(|d| d.is_verified(&self.own_identity, &self.device_owner_identity))
    }

    /// Iterator over all the device ids of the user devices.
    pub fn keys(&self) -> impl Iterator<Item = &DeviceId> {
        self.inner.keys().map(Deref::deref)
    }

    /// Iterator over all the devices of the user devices.
    pub fn devices(&self) -> impl Iterator<Item = Device> + '_ {
        self.inner.values().map(move |d| Device {
            inner: d.clone(),
            verification_machine: self.verification_machine.clone(),
            own_identity: self.own_identity.clone(),
            device_owner_identity: self.device_owner_identity.clone(),
        })
    }
}

/// The local trust state of a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum LocalTrust {
    /// The device has been verified and is trusted.
    Verified = 0,
    /// The device been blacklisted from communicating.
    BlackListed = 1,
    /// The trust state of the device is being ignored.
    Ignored = 2,
    /// The trust state is unset.
    Unset = 3,
}

impl From<i64> for LocalTrust {
    fn from(state: i64) -> Self {
        match state {
            0 => LocalTrust::Verified,
            1 => LocalTrust::BlackListed,
            2 => LocalTrust::Ignored,
            3 => LocalTrust::Unset,
            _ => LocalTrust::Unset,
        }
    }
}

impl DeviceData {
    /// Create a new Device, this constructor skips signature verification of
    /// the keys, `TryFrom` should be used for completely new devices we
    /// receive.
    pub fn new(device_keys: DeviceKeys, trust_state: LocalTrust) -> Self {
        Self {
            device_keys: device_keys.into(),
            trust_state: Arc::new(RwLock::new(trust_state)),
            deleted: Arc::new(AtomicBool::new(false)),
            withheld_code_sent: Arc::new(AtomicBool::new(false)),
            first_time_seen_ts: MilliSecondsSinceUnixEpoch::now(),
            olm_wedging_index: Default::default(),
        }
    }

    /// The user id of the device owner.
    pub fn user_id(&self) -> &UserId {
        &self.device_keys.user_id
    }

    /// The unique ID of the device.
    pub fn device_id(&self) -> &DeviceId {
        &self.device_keys.device_id
    }

    /// Get the human readable name of the device.
    pub fn display_name(&self) -> Option<&str> {
        self.device_keys.unsigned.device_display_name.as_deref()
    }

    /// Get the key of the given key algorithm belonging to this device.
    pub fn get_key(&self, algorithm: DeviceKeyAlgorithm) -> Option<&DeviceKey> {
        self.device_keys.get_key(algorithm)
    }

    /// Get the Curve25519 key of the given device.
    pub fn curve25519_key(&self) -> Option<Curve25519PublicKey> {
        self.device_keys.curve25519_key()
    }

    /// Get the Ed25519 key of the given device.
    pub fn ed25519_key(&self) -> Option<Ed25519PublicKey> {
        self.device_keys.ed25519_key()
    }

    /// Get a map containing all the device keys.
    pub fn keys(&self) -> &BTreeMap<OwnedDeviceKeyId, DeviceKey> {
        &self.device_keys.keys
    }

    /// Get a map containing all the device signatures.
    pub fn signatures(&self) -> &Signatures {
        &self.device_keys.signatures
    }

    /// Get the trust state of the device.
    pub fn local_trust_state(&self) -> LocalTrust {
        *self.trust_state.read()
    }

    /// Is the device locally marked as trusted.
    pub fn is_locally_trusted(&self) -> bool {
        self.local_trust_state() == LocalTrust::Verified
    }

    /// Is the device locally marked as blacklisted.
    ///
    /// Blacklisted devices won't receive any group sessions.
    pub fn is_blacklisted(&self) -> bool {
        self.local_trust_state() == LocalTrust::BlackListed
    }

    /// Set the trust state of the device to the given state.
    ///
    /// Note: This should only done in the crypto store where the trust state
    /// can be stored.
    pub(crate) fn set_trust_state(&self, state: LocalTrust) {
        *self.trust_state.write() = state;
    }

    pub(crate) fn mark_withheld_code_as_sent(&self) {
        self.withheld_code_sent.store(true, Ordering::Relaxed)
    }

    /// Returns true if the `m.no_olm` withheld code was already sent to this
    /// device.
    pub fn was_withheld_code_sent(&self) -> bool {
        self.withheld_code_sent.load(Ordering::Relaxed)
    }

    /// Get the list of algorithms this device supports.
    pub fn algorithms(&self) -> &[EventEncryptionAlgorithm] {
        &self.device_keys.algorithms
    }

    /// Does this device support any of our known Olm encryption algorithms.
    pub fn supports_olm(&self) -> bool {
        #[cfg(feature = "experimental-algorithms")]
        {
            self.algorithms().contains(&EventEncryptionAlgorithm::OlmV1Curve25519AesSha2)
                || self.algorithms().contains(&EventEncryptionAlgorithm::OlmV2Curve25519AesSha2)
        }

        #[cfg(not(feature = "experimental-algorithms"))]
        {
            self.algorithms().contains(&EventEncryptionAlgorithm::OlmV1Curve25519AesSha2)
        }
    }

    /// Find and return the most recently created Olm [`Session`] we are sharing
    /// with this device.
    pub(crate) async fn get_most_recent_session(
        &self,
        store: &CryptoStoreWrapper,
    ) -> OlmResult<Option<Session>> {
        if let Some(sender_key) = self.curve25519_key() {
            if let Some(sessions) = store.get_sessions(&sender_key.to_base64()).await? {
                let mut sessions = sessions.lock().await;
                sessions.sort_by_key(|s| s.creation_time);

                Ok(sessions.last().cloned())
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    /// Does this device support the olm.v2.curve25519-aes-sha2 encryption
    /// algorithm.
    #[cfg(feature = "experimental-algorithms")]
    pub fn supports_olm_v2(&self) -> bool {
        self.algorithms().contains(&EventEncryptionAlgorithm::OlmV2Curve25519AesSha2)
    }

    /// Get the optimal `SessionConfig` for this device.
    pub fn olm_session_config(&self) -> SessionConfig {
        #[cfg(feature = "experimental-algorithms")]
        if self.supports_olm_v2() {
            SessionConfig::version_2()
        } else {
            SessionConfig::version_1()
        }

        #[cfg(not(feature = "experimental-algorithms"))]
        SessionConfig::version_1()
    }

    /// Is the device deleted.
    pub fn is_deleted(&self) -> bool {
        self.deleted.load(Ordering::Relaxed)
    }

    pub(crate) fn is_verified(
        &self,
        own_identity: &Option<OwnUserIdentityData>,
        device_owner: &Option<UserIdentityData>,
    ) -> bool {
        self.is_locally_trusted() || self.is_cross_signing_trusted(own_identity, device_owner)
    }

    pub(crate) fn is_cross_signing_trusted(
        &self,
        own_identity: &Option<OwnUserIdentityData>,
        device_owner: &Option<UserIdentityData>,
    ) -> bool {
        own_identity.as_ref().zip(device_owner.as_ref()).is_some_and(
            |(own_identity, device_identity)| {
                match device_identity {
                    UserIdentityData::Own(_) => {
                        own_identity.is_verified() && own_identity.is_device_signed(self)
                    }

                    // If it's a device from someone else, first check
                    // that our user has verified the other user and then
                    // check if the other user has signed this device.
                    UserIdentityData::Other(device_identity) => {
                        own_identity.is_identity_verified(device_identity)
                            && device_identity.is_device_signed(self)
                    }
                }
            },
        )
    }

    pub(crate) fn is_cross_signed_by_owner(
        &self,
        device_owner_identity: &UserIdentityData,
    ) -> bool {
        match device_owner_identity {
            // If it's one of our own devices, just check that
            // we signed the device.
            UserIdentityData::Own(identity) => identity.is_device_signed(self),
            // If it's a device from someone else, check
            // if the other user has signed this device.
            UserIdentityData::Other(device_identity) => device_identity.is_device_signed(self),
        }
    }

    /// Encrypt the given content for this device.
    ///
    /// # Arguments
    ///
    /// * `store` - The crypto store. Used to find an established Olm session
    ///   for this device.
    /// * `event_type` - The type of the event that should be encrypted.
    /// * `content` - The content of the event that should be encrypted.
    ///
    /// # Returns
    ///
    /// On success, a tuple `(session, content)`, where `session` is the Olm
    /// [`Session`] that was used to encrypt the content, and `content` is
    /// the content for the `m.room.encrypted` to-device event.
    ///
    /// If an Olm session has not already been established with this device,
    /// returns `Err(OlmError::MissingSession)`.
    #[instrument(
        skip_all,
        fields(
            recipient = ?self.user_id(),
            recipient_device = ?self.device_id(),
            recipient_key = ?self.curve25519_key(),
            event_type,
            message_id,
        ))
    ]
    pub(crate) async fn encrypt(
        &self,
        store: &CryptoStoreWrapper,
        event_type: &str,
        content: impl Serialize,
    ) -> OlmResult<(Session, Raw<ToDeviceEncryptedEventContent>)> {
        #[cfg(not(target_family = "wasm"))]
        let message_id = ulid::Ulid::new().to_string();
        #[cfg(target_family = "wasm")]
        let message_id = ruma::TransactionId::new().to_string();

        tracing::Span::current().record("message_id", &message_id);

        let session = self.get_most_recent_session(store).await?;

        if let Some(mut session) = session {
            let message = session.encrypt(self, event_type, content, Some(message_id)).await?;
            Ok((session, message))
        } else {
            trace!("Trying to encrypt an event for a device, but no Olm session is found.");
            Err(OlmError::MissingSession)
        }
    }

    pub(crate) async fn maybe_encrypt_room_key(
        &self,
        store: &CryptoStoreWrapper,
        session: OutboundGroupSession,
    ) -> OlmResult<MaybeEncryptedRoomKey> {
        let content = session.as_content().await;
        let message_index = session.message_index().await;
        let event_type = content.event_type().to_owned();

        match self.encrypt(store, &event_type, content).await {
            Ok((session, encrypted)) => Ok(MaybeEncryptedRoomKey::Encrypted {
                share_info: Box::new(ShareInfo::new_shared(
                    session.sender_key().to_owned(),
                    message_index,
                    self.olm_wedging_index,
                )),
                used_session: Box::new(session),
                message: encrypted.cast(),
            }),

            Err(OlmError::MissingSession) => Ok(MaybeEncryptedRoomKey::MissingSession),
            Err(e) => Err(e),
        }
    }

    /// Update a device with a new device keys struct.
    ///
    /// Returns `true` if any changes were made to the data.
    pub(crate) fn update_device(
        &mut self,
        device_keys: &DeviceKeys,
    ) -> Result<bool, SignatureError> {
        device_keys.check_self_signature()?;

        if self.user_id() != device_keys.user_id || self.device_id() != device_keys.device_id {
            Err(SignatureError::UserIdMismatch)
        } else if self.ed25519_key() != device_keys.ed25519_key() {
            Err(SignatureError::SigningKeyChanged(
                self.ed25519_key().map(Box::new),
                device_keys.ed25519_key().map(Box::new),
            ))
        } else if self.device_keys.as_ref() != device_keys {
            trace!(
                user_id = ?self.user_id(),
                device_id = ?self.device_id(),
                keys = ?self.keys(),
                "Updated a device",
            );

            self.device_keys = device_keys.clone().into();

            Ok(true)
        } else {
            // no changes needed
            Ok(false)
        }
    }

    /// Return the device keys
    pub fn as_device_keys(&self) -> &DeviceKeys {
        &self.device_keys
    }

    /// Check if the given JSON is signed by this device key.
    ///
    /// This method should only be used if an object's signature needs to be
    /// checked multiple times, and you'd like to avoid performing the
    /// canonicalization step each time.
    ///
    /// **Note**: Use this method with caution, the `canonical_json` needs to be
    /// correctly canonicalized and make sure that the object you are checking
    /// the signature for is allowed to be signed by a device.
    pub(crate) fn has_signed_raw(
        &self,
        signatures: &Signatures,
        canonical_json: &str,
    ) -> Result<(), SignatureError> {
        let key = self.ed25519_key().ok_or(SignatureError::MissingSigningKey)?;
        let user_id = self.user_id();
        let key_id = &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.device_id());

        key.verify_canonicalized_json(user_id, key_id, signatures, canonical_json)
    }

    pub(crate) fn verify_one_time_key(
        &self,
        one_time_key: &SignedKey,
    ) -> Result<(), SignatureError> {
        self.device_keys.has_signed(one_time_key)
    }

    /// Mark the device as deleted.
    pub(crate) fn mark_as_deleted(&self) {
        self.deleted.store(true, Ordering::Relaxed);
    }

    #[cfg(any(test, feature = "testing"))]
    #[allow(dead_code)]
    /// Generate the Device from a reference of an OlmMachine.
    pub async fn from_machine_test_helper(
        machine: &OlmMachine,
    ) -> Result<DeviceData, crate::CryptoStoreError> {
        Ok(DeviceData::from_account(&*machine.store().cache().await?.account().await?))
    }

    /// Create [`DeviceData`] from an [`Account`].
    ///
    /// We will have our own device data in the store once we receive a
    /// `/keys/query` response, but this is useful to create it before we
    /// receive such a response.
    ///
    /// It also makes it easier to check that the server doesn't lie about our
    /// own device.
    ///
    /// *Don't* use this after we received a `/keys/query` response, other
    /// users/devices might add signatures to our own device, which can't be
    /// replicated locally.
    pub fn from_account(account: &Account) -> DeviceData {
        let device_keys = account.device_keys();
        let mut device = DeviceData::try_from(&device_keys)
            .expect("Creating a device from our own account should always succeed");
        device.first_time_seen_ts = account.creation_local_time();

        device
    }

    /// Get the local timestamp of when this device was first persisted, in
    /// milliseconds since epoch (client local time).
    pub fn first_time_seen_ts(&self) -> MilliSecondsSinceUnixEpoch {
        self.first_time_seen_ts
    }

    /// True if this device is an [MSC3814](https://github.com/matrix-org/matrix-spec-proposals/pull/3814) dehydrated device.
    pub fn is_dehydrated(&self) -> bool {
        self.device_keys.dehydrated.unwrap_or(false)
    }
}

impl TryFrom<&DeviceKeys> for DeviceData {
    type Error = SignatureError;

    fn try_from(device_keys: &DeviceKeys) -> Result<Self, Self::Error> {
        device_keys.check_self_signature()?;
        Ok(Self {
            device_keys: device_keys.clone().into(),
            deleted: Arc::new(AtomicBool::new(false)),
            trust_state: Arc::new(RwLock::new(LocalTrust::Unset)),
            withheld_code_sent: Arc::new(AtomicBool::new(false)),
            first_time_seen_ts: MilliSecondsSinceUnixEpoch::now(),
            olm_wedging_index: Default::default(),
        })
    }
}

impl PartialEq for DeviceData {
    fn eq(&self, other: &Self) -> bool {
        self.user_id() == other.user_id() && self.device_id() == other.device_id()
    }
}

/// Testing Facilities for Device Management
#[cfg(any(test, feature = "testing"))]
#[allow(dead_code)]
pub(crate) mod testing {
    use serde_json::json;

    use crate::{identities::DeviceData, types::DeviceKeys};

    /// Generate default DeviceKeys for tests
    pub fn device_keys() -> DeviceKeys {
        let device_keys = json!({
          "algorithms": vec![
              "m.olm.v1.curve25519-aes-sha2",
              "m.megolm.v1.aes-sha2"
          ],
          "device_id": "BNYQQWUMXO",
          "user_id": "@example:localhost",
          "keys": {
              "curve25519:BNYQQWUMXO": "xfgbLIC5WAl1OIkpOzoxpCe8FsRDT6nch7NQsOb15nc",
              "ed25519:BNYQQWUMXO": "2/5LWJMow5zhJqakV88SIc7q/1pa8fmkfgAzx72w9G4"
          },
          "signatures": {
              "@example:localhost": {
                  "ed25519:BNYQQWUMXO": "kTwMrbsLJJM/uFGOj/oqlCaRuw7i9p/6eGrTlXjo8UJMCFAetoyWzoMcF35vSe4S6FTx8RJmqX6rM7ep53MHDQ"
              }
          },
          "unsigned": {
              "device_display_name": "Alice's mobile phone"
          }
        });

        serde_json::from_value(device_keys).unwrap()
    }

    /// Generate default [`DeviceData`] for tests
    pub fn get_device() -> DeviceData {
        let device_keys = device_keys();
        DeviceData::try_from(&device_keys).unwrap()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use ruma::{MilliSecondsSinceUnixEpoch, user_id};
    use serde_json::json;
    use vodozemac::{Curve25519PublicKey, Ed25519PublicKey};

    use super::testing::{device_keys, get_device};
    use crate::{DeviceData, identities::LocalTrust};

    #[test]
    fn create_a_device() {
        let now = MilliSecondsSinceUnixEpoch::now();
        let user_id = user_id!("@example:localhost");
        let device_id = "BNYQQWUMXO";

        let device = get_device();

        assert_eq!(user_id, device.user_id());
        assert_eq!(device_id, device.device_id());
        assert_eq!(device.algorithms().len(), 2);
        assert_eq!(LocalTrust::Unset, device.local_trust_state());
        assert_eq!("Alice's mobile phone", device.display_name().unwrap());
        assert_eq!(
            device.curve25519_key().unwrap(),
            Curve25519PublicKey::from_base64("xfgbLIC5WAl1OIkpOzoxpCe8FsRDT6nch7NQsOb15nc")
                .unwrap(),
        );
        assert_eq!(
            device.ed25519_key().unwrap(),
            Ed25519PublicKey::from_base64("2/5LWJMow5zhJqakV88SIc7q/1pa8fmkfgAzx72w9G4").unwrap(),
        );

        let then = MilliSecondsSinceUnixEpoch::now();

        assert!(device.first_time_seen_ts() >= now);
        assert!(device.first_time_seen_ts() <= then);
    }

    #[test]
    fn update_a_device() {
        let mut device = get_device();

        assert_eq!("Alice's mobile phone", device.display_name().unwrap());

        let display_name = "Alice's work computer".to_owned();

        let mut device_keys = device_keys();
        device_keys.unsigned.device_display_name = Some(display_name.clone());
        assert!(device.update_device(&device_keys).unwrap());
        assert_eq!(&display_name, device.display_name().as_ref().unwrap());

        // A second call to `update_device` with the same data should return `false`.
        assert!(!device.update_device(&device_keys).unwrap());
    }

    #[test]
    #[allow(clippy::redundant_clone)]
    fn delete_a_device() {
        let device = get_device();
        assert!(!device.is_deleted());

        let device_clone = device.clone();

        device.mark_as_deleted();
        assert!(device.is_deleted());
        assert!(device_clone.is_deleted());
    }

    #[test]
    fn deserialize_device() {
        let user_id = user_id!("@example:localhost");
        let device_id = "BNYQQWUMXO";

        let device = json!({
            "inner": {
                "user_id": user_id,
                "device_id": device_id,
                "algorithms": ["m.olm.v1.curve25519-aes-sha2","m.megolm.v1.aes-sha2"],
                "keys": {
                    "curve25519:BNYQQWUMXO": "xfgbLIC5WAl1OIkpOzoxpCe8FsRDT6nch7NQsOb15nc",
                    "ed25519:BNYQQWUMXO": "2/5LWJMow5zhJqakV88SIc7q/1pa8fmkfgAzx72w9G4"
                },
                "signatures": {
                    "@example:localhost": {
                        "ed25519:BNYQQWUMXO": "kTwMrbsLJJM/uFGOj/oqlCaRuw7i9p/6eGrTlXjo8UJMCFAetoyWzoMcF35vSe4S6FTx8RJmqX6rM7ep53MHDQ"
                    }
                },
                "unsigned": {
                    "device_display_name": "Alice's mobile phone"
                }
            },
            "deleted": false,
            "trust_state": "Verified",
            "withheld_code_sent": false,
            "first_time_seen_ts": 1696931068314u64
        });

        let device: DeviceData =
            serde_json::from_value(device).expect("We should be able to deserialize our device");

        assert_eq!(user_id, device.user_id());
        assert_eq!(device_id, device.device_id());
        assert_eq!(device.algorithms().len(), 2);
        assert_eq!(LocalTrust::Verified, device.local_trust_state());
        assert_eq!("Alice's mobile phone", device.display_name().unwrap());
        assert_eq!(
            device.curve25519_key().unwrap(),
            Curve25519PublicKey::from_base64("xfgbLIC5WAl1OIkpOzoxpCe8FsRDT6nch7NQsOb15nc")
                .unwrap(),
        );
        assert_eq!(
            device.ed25519_key().unwrap(),
            Ed25519PublicKey::from_base64("2/5LWJMow5zhJqakV88SIc7q/1pa8fmkfgAzx72w9G4").unwrap(),
        );
    }
}
