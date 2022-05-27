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
    convert::{TryFrom, TryInto},
    ops::Deref,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use atomic::Atomic;
use matrix_sdk_common::locks::Mutex;
use ruma::{
    api::client::keys::upload_signatures::v3::Request as SignatureUploadRequest,
    events::{
        forwarded_room_key::ToDeviceForwardedRoomKeyEventContent,
        key::verification::VerificationMethod, room::encrypted::ToDeviceRoomEncryptedEventContent,
        AnyToDeviceEventContent,
    },
    DeviceId, DeviceKeyAlgorithm, DeviceKeyId, EventEncryptionAlgorithm, OwnedDeviceId,
    OwnedDeviceKeyId, UserId,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Value};
use tracing::warn;
use vodozemac::{Curve25519PublicKey, Ed25519PublicKey};

use super::{atomic_bool_deserializer, atomic_bool_serializer};
#[cfg(any(test, feature = "testing"))]
use crate::OlmMachine;
use crate::{
    error::{EventError, OlmError, OlmResult, SignatureError},
    identities::{ReadOnlyOwnUserIdentity, ReadOnlyUserIdentities},
    olm::{InboundGroupSession, Session, VerifyJson},
    store::{Changes, CryptoStore, DeviceChanges, Result as StoreResult},
    types::{DeviceKey, DeviceKeys, Signatures, SignedKey},
    verification::VerificationMachine,
    OutgoingVerificationRequest, ReadOnlyAccount, Sas, ToDeviceRequest, VerificationRequest,
};

/// A read-only version of a `Device`.
#[derive(Clone, Serialize, Deserialize)]
pub struct ReadOnlyDevice {
    pub(crate) inner: Arc<DeviceKeys>,
    #[serde(
        serialize_with = "atomic_bool_serializer",
        deserialize_with = "atomic_bool_deserializer"
    )]
    deleted: Arc<AtomicBool>,
    #[serde(
        serialize_with = "local_trust_serializer",
        deserialize_with = "local_trust_deserializer"
    )]
    trust_state: Arc<Atomic<LocalTrust>>,
}

impl std::fmt::Debug for ReadOnlyDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadOnlyDevice")
            .field("user_id", &self.user_id())
            .field("device_id", &self.device_id())
            .field("display_name", &self.display_name())
            .field("keys", self.keys())
            .field("deleted", &self.deleted.load(Ordering::SeqCst))
            .field("trust_state", &self.trust_state)
            .finish()
    }
}

fn local_trust_serializer<S>(x: &Atomic<LocalTrust>, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let value = x.load(Ordering::SeqCst);
    s.serialize_some(&value)
}

fn local_trust_deserializer<'de, D>(deserializer: D) -> Result<Arc<Atomic<LocalTrust>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = LocalTrust::deserialize(deserializer)?;
    Ok(Arc::new(Atomic::new(value)))
}

#[derive(Clone)]
/// A device represents a E2EE capable client of an user.
pub struct Device {
    pub(crate) inner: ReadOnlyDevice,
    pub(crate) verification_machine: VerificationMachine,
    pub(crate) own_identity: Option<ReadOnlyOwnUserIdentity>,
    pub(crate) device_owner_identity: Option<ReadOnlyUserIdentities>,
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device").field("device", &self.inner).finish()
    }
}

impl Deref for Device {
    type Target = ReadOnlyDevice;

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

    /// Request an interacitve verification with this `Device`
    ///
    /// Returns a `VerificationRequest` object and a to-device request that
    /// needs to be sent out.
    pub async fn request_verification(&self) -> (VerificationRequest, OutgoingVerificationRequest) {
        self.request_verification_helper(None).await
    }

    /// Request an interacitve verification with this `Device`
    ///
    /// Returns a `VerificationRequest` object and a to-device request that
    /// needs to be sent out.
    ///
    /// # Arguments
    ///
    /// * `methods` - The verification methods that we want to support.
    pub async fn request_verification_with_methods(
        &self,
        methods: Vec<VerificationMethod>,
    ) -> (VerificationRequest, OutgoingVerificationRequest) {
        self.request_verification_helper(Some(methods)).await
    }

    async fn request_verification_helper(
        &self,
        methods: Option<Vec<VerificationMethod>>,
    ) -> (VerificationRequest, OutgoingVerificationRequest) {
        self.verification_machine
            .request_to_device_verification(
                self.user_id(),
                vec![self.device_id().to_owned()],
                methods,
            )
            .await
    }

    /// Get the Olm sessions that belong to this device.
    pub(crate) async fn get_sessions(&self) -> StoreResult<Option<Arc<Mutex<Vec<Session>>>>> {
        if let Some(k) = self.curve25519_key() {
            self.verification_machine.store.get_sessions(&k.to_base64()).await
        } else {
            Ok(None)
        }
    }

    /// Is this device considered to be verified.
    ///
    /// This method returns true if either [`is_locally_trusted()`] returns true
    /// or if [`is_cross_signing_trusted()`] returns true.
    ///
    /// [`is_locally_trusted()`]: #method.is_locally_trusted
    /// [`is_cross_signing_trusted()`]: #method.is_cross_signing_trusted
    pub fn verified(&self) -> bool {
        self.inner.verified(&self.own_identity, &self.device_owner_identity)
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
        content: AnyToDeviceEventContent,
    ) -> OlmResult<(Session, ToDeviceRoomEncryptedEventContent)> {
        self.inner.encrypt(self.verification_machine.store.inner(), content).await
    }

    /// Encrypt the given inbound group session as a forwarded room key for this
    /// device.
    pub async fn encrypt_session(
        &self,
        session: InboundGroupSession,
        message_index: Option<u32>,
    ) -> OlmResult<(Session, ToDeviceRoomEncryptedEventContent)> {
        let export = if let Some(index) = message_index {
            session.export_at_index(index).await
        } else {
            session.export().await
        };

        let content: ToDeviceForwardedRoomKeyEventContent = if let Ok(c) = export.try_into() {
            c
        } else {
            // TODO remove this panic.
            panic!(
                "Can't share session {} with device {} {}, key export can't \
                 be converted to a forwarded room key content",
                session.session_id(),
                self.user_id(),
                self.device_id()
            );
        };

        self.encrypt(AnyToDeviceEventContent::ForwardedRoomKey(content)).await
    }
}

/// A read only view over all devices belonging to a user.
#[derive(Debug)]
pub struct UserDevices {
    pub(crate) inner: HashMap<OwnedDeviceId, ReadOnlyDevice>,
    pub(crate) verification_machine: VerificationMachine,
    pub(crate) own_identity: Option<ReadOnlyOwnUserIdentity>,
    pub(crate) device_owner_identity: Option<ReadOnlyUserIdentities>,
}

impl UserDevices {
    /// Get the specific device with the given device id.
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
            .any(|d| d.verified(&self.own_identity, &self.device_owner_identity))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// The local trust state of a device.
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

impl ReadOnlyDevice {
    /// Create a new Device, this constructor skips signature verification of
    /// the keys, `TryFrom` should be used for completely new devices we
    /// receive.
    pub fn new(device_keys: DeviceKeys, trust_state: LocalTrust) -> Self {
        Self {
            inner: device_keys.into(),
            trust_state: Arc::new(Atomic::new(trust_state)),
            deleted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The user id of the device owner.
    pub fn user_id(&self) -> &UserId {
        &self.inner.user_id
    }

    /// The unique ID of the device.
    pub fn device_id(&self) -> &DeviceId {
        &self.inner.device_id
    }

    /// Get the human readable name of the device.
    pub fn display_name(&self) -> Option<&str> {
        self.inner.unsigned.device_display_name.as_deref()
    }

    /// Get the key of the given key algorithm belonging to this device.
    pub fn get_key(&self, algorithm: DeviceKeyAlgorithm) -> Option<&DeviceKey> {
        self.inner.keys.get(&DeviceKeyId::from_parts(algorithm, self.device_id()))
    }

    /// Get the Curve25519 key of the given device.
    pub fn curve25519_key(&self) -> Option<Curve25519PublicKey> {
        self.get_key(DeviceKeyAlgorithm::Curve25519).and_then(|k| {
            if let DeviceKey::Curve25519(k) = k {
                Some(*k)
            } else {
                None
            }
        })
    }

    /// Get the Ed25519 key of the given device.
    pub fn ed25519_key(&self) -> Option<Ed25519PublicKey> {
        self.get_key(DeviceKeyAlgorithm::Ed25519).and_then(|k| {
            if let DeviceKey::Ed25519(k) = k {
                Some(*k)
            } else {
                None
            }
        })
    }

    /// Get a map containing all the device keys.
    pub fn keys(&self) -> &BTreeMap<OwnedDeviceKeyId, DeviceKey> {
        &self.inner.keys
    }

    /// Get a map containing all the device signatures.
    pub fn signatures(&self) -> &Signatures {
        &self.inner.signatures
    }

    /// Get the trust state of the device.
    pub fn local_trust_state(&self) -> LocalTrust {
        self.trust_state.load(Ordering::Relaxed)
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
        self.trust_state.store(state, Ordering::Relaxed)
    }

    /// Get the list of algorithms this device supports.
    pub fn algorithms(&self) -> &[EventEncryptionAlgorithm] {
        &self.inner.algorithms
    }

    /// Is the device deleted.
    pub fn deleted(&self) -> bool {
        self.deleted.load(Ordering::Relaxed)
    }

    pub(crate) fn verified(
        &self,
        own_identity: &Option<ReadOnlyOwnUserIdentity>,
        device_owner: &Option<ReadOnlyUserIdentities>,
    ) -> bool {
        self.is_locally_trusted() || self.is_cross_signing_trusted(own_identity, device_owner)
    }

    pub(crate) fn is_cross_signing_trusted(
        &self,
        own_identity: &Option<ReadOnlyOwnUserIdentity>,
        device_owner: &Option<ReadOnlyUserIdentities>,
    ) -> bool {
        own_identity.as_ref().map_or(false, |own_identity| {
            // Our own identity needs to be marked as verified.
            own_identity.is_verified()
                && device_owner
                    .as_ref()
                    .map(|device_identity| match device_identity {
                        // If it's one of our own devices, just check that
                        // we signed the device.
                        ReadOnlyUserIdentities::Own(_) => {
                            own_identity.is_device_signed(self).map_or(false, |_| true)
                        }

                        // If it's a device from someone else, first check
                        // that our user has signed the other user and then
                        // check if the other user has signed this device.
                        ReadOnlyUserIdentities::Other(device_identity) => {
                            own_identity.is_identity_signed(device_identity).map_or(false, |_| true)
                                && device_identity.is_device_signed(self).map_or(false, |_| true)
                        }
                    })
                    .unwrap_or(false)
        })
    }

    pub(crate) async fn encrypt(
        &self,
        store: &dyn CryptoStore,
        content: AnyToDeviceEventContent,
    ) -> OlmResult<(Session, ToDeviceRoomEncryptedEventContent)> {
        let sender_key = if let Some(k) = self.curve25519_key() {
            k
        } else {
            warn!(
                user_id = %self.user_id(),
                device_id = %self.device_id(),
                "Trying to encrypt a Megolm session, but the device doesn't \
                have a curve25519 key",
            );
            return Err(EventError::MissingSenderKey.into());
        };

        let session = if let Some(s) = store.get_sessions(&sender_key.to_base64()).await? {
            let mut sessions = s.lock().await;
            sessions.sort_by_key(|s| s.last_use_time);
            sessions.get(0).cloned()
        } else {
            None
        };

        let mut session = if let Some(s) = session {
            s
        } else {
            warn!(
                "Trying to encrypt a Megolm session for user {} on device {}, \
                but no Olm session is found",
                self.user_id(),
                self.device_id()
            );
            return Err(OlmError::MissingSession);
        };

        let message = session.encrypt(self, content).await?;

        Ok((session, message))
    }

    /// Update a device with a new device keys struct.
    pub(crate) fn update_device(&mut self, device_keys: &DeviceKeys) -> Result<(), SignatureError> {
        self.verify_device_keys(device_keys)?;
        self.inner = device_keys.clone().into();

        Ok(())
    }

    pub(crate) fn is_signed_by_device(&self, json: &mut Value) -> Result<(), SignatureError> {
        let key = self.ed25519_key().ok_or(SignatureError::MissingSigningKey)?;

        key.verify_json(
            self.user_id(),
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.device_id()),
            json,
        )
    }

    pub(crate) fn as_device_keys(&self) -> &DeviceKeys {
        &self.inner
    }

    pub(crate) fn verify_device_keys(
        &self,
        device_keys: &DeviceKeys,
    ) -> Result<(), SignatureError> {
        let mut device_keys = serde_json::to_value(device_keys)?;
        self.is_signed_by_device(&mut device_keys)
    }

    pub(crate) fn verify_one_time_key(
        &self,
        one_time_key: &SignedKey,
    ) -> Result<(), SignatureError> {
        self.is_signed_by_device(&mut json!(&one_time_key))
    }

    /// Mark the device as deleted.
    pub(crate) fn mark_as_deleted(&self) {
        self.deleted.store(true, Ordering::Relaxed);
    }

    #[cfg(any(test, feature = "testing"))]
    #[allow(dead_code)]
    /// Generate the Device from the reference of an OlmMachine.
    ///
    /// TESTING FACILITY ONLY, DO NOT USE OUTSIDE OF TESTS
    pub async fn from_machine(machine: &OlmMachine) -> ReadOnlyDevice {
        ReadOnlyDevice::from_account(machine.account()).await
    }

    /// Create a `ReadOnlyDevice` from an `Account`
    ///
    /// We will have our own device in the store once we receive a keys/query
    /// response, but this is useful to create it before we receive such a
    /// response.
    ///
    /// It also makes it easier to check that the server doesn't lie about our
    /// own device.
    ///
    /// *Don't* use this after we received a keys/query response, other
    /// users/devices might add signatures to our own device, which can't be
    /// replicated locally.
    pub async fn from_account(account: &ReadOnlyAccount) -> ReadOnlyDevice {
        let device_keys = account.device_keys().await;
        ReadOnlyDevice::try_from(&device_keys)
            .expect("Creating a device from our own account should always succeed")
    }
}

impl TryFrom<&DeviceKeys> for ReadOnlyDevice {
    type Error = SignatureError;

    fn try_from(device_keys: &DeviceKeys) -> Result<Self, Self::Error> {
        let device = Self {
            inner: device_keys.clone().into(),
            deleted: Arc::new(AtomicBool::new(false)),
            trust_state: Arc::new(Atomic::new(LocalTrust::Unset)),
        };

        device.verify_device_keys(device_keys)?;
        Ok(device)
    }
}

impl PartialEq for ReadOnlyDevice {
    fn eq(&self, other: &Self) -> bool {
        self.user_id() == other.user_id() && self.device_id() == other.device_id()
    }
}

#[cfg(any(test, feature = "testing"))]
pub(crate) mod testing {
    //! Testing Facilities for Device Management
    #![allow(dead_code)]
    use serde_json::json;

    use crate::{identities::ReadOnlyDevice, types::DeviceKeys};

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

    /// Generate default ReadOnlyDevice for tests
    pub fn get_device() -> ReadOnlyDevice {
        let device_keys = device_keys();
        ReadOnlyDevice::try_from(&device_keys).unwrap()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use ruma::user_id;
    use vodozemac::{Curve25519PublicKey, Ed25519PublicKey};

    use super::testing::{device_keys, get_device};
    use crate::identities::LocalTrust;

    #[test]
    fn create_a_device() {
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
    }

    #[test]
    fn update_a_device() {
        let mut device = get_device();

        assert_eq!("Alice's mobile phone", device.display_name().unwrap());

        let display_name = "Alice's work computer".to_owned();

        let mut device_keys = device_keys();
        device_keys.unsigned.device_display_name = Some(display_name.clone());
        device.update_device(&device_keys).unwrap();

        assert_eq!(&display_name, device.display_name().as_ref().unwrap());
    }

    #[test]
    fn delete_a_device() {
        let device = get_device();
        assert!(!device.deleted());

        let device_clone = device.clone();

        device.mark_as_deleted();
        assert!(device.deleted());
        assert!(device_clone.deleted());
    }
}
