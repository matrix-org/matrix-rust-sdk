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
    collections::BTreeMap,
    convert::{TryFrom, TryInto},
    ops::Deref,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use atomic::Atomic;
use matrix_sdk_common::{
    api::r0::keys::SignedKey,
    encryption::DeviceKeys,
    events::{
        forwarded_room_key::ForwardedRoomKeyEventContent, room::encrypted::EncryptedEventContent,
        EventType,
    },
    identifiers::{DeviceId, DeviceKeyAlgorithm, DeviceKeyId, EventEncryptionAlgorithm, UserId},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;

use crate::olm::InboundGroupSession;
#[cfg(test)]
use crate::{OlmMachine, ReadOnlyAccount};

use crate::{
    error::{EventError, OlmError, OlmResult, SignatureError},
    identities::{OwnUserIdentity, UserIdentities},
    olm::Utility,
    store::{caches::ReadOnlyUserDevices, CryptoStore, Result as StoreResult},
    verification::VerificationMachine,
    Sas, ToDeviceRequest,
};

/// A read-only version of a `Device`.
#[derive(Debug, Clone)]
pub struct ReadOnlyDevice {
    user_id: Arc<UserId>,
    device_id: Arc<Box<DeviceId>>,
    algorithms: Arc<[EventEncryptionAlgorithm]>,
    keys: Arc<BTreeMap<DeviceKeyId, String>>,
    signatures: Arc<BTreeMap<UserId, BTreeMap<DeviceKeyId, String>>>,
    display_name: Arc<Option<String>>,
    deleted: Arc<AtomicBool>,
    trust_state: Arc<Atomic<LocalTrust>>,
}

#[derive(Debug, Clone)]
/// A device represents a E2EE capable client of an user.
pub struct Device {
    pub(crate) inner: ReadOnlyDevice,
    pub(crate) verification_machine: VerificationMachine,
    pub(crate) own_identity: Option<OwnUserIdentity>,
    pub(crate) device_owner_identity: Option<UserIdentities>,
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
    /// Returns a `Sas` object and to-device request that needs to be sent out.
    pub async fn start_verification(&self) -> StoreResult<(Sas, ToDeviceRequest)> {
        self.verification_machine
            .start_sas(self.inner.clone())
            .await
    }

    /// Get the trust state of the device.
    pub fn trust_state(&self) -> bool {
        self.inner
            .trust_state(&self.own_identity, &self.device_owner_identity)
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

        self.verification_machine
            .store
            .save_devices(&[self.inner.clone()])
            .await
    }

    /// Encrypt the given content for this `Device`.
    ///
    /// # Arguments
    ///
    /// * `event_type` - The type of the event.
    ///
    /// * `content` - The content of the event that should be encrypted.
    pub(crate) async fn encrypt(
        &self,
        event_type: EventType,
        content: Value,
    ) -> OlmResult<EncryptedEventContent> {
        self.inner
            .encrypt(&**self.verification_machine.store, event_type, content)
            .await
    }

    /// Encrypt the given inbound group session as a forwarded room key for this
    /// device.
    pub async fn encrypt_session(
        &self,
        session: InboundGroupSession,
    ) -> OlmResult<EncryptedEventContent> {
        let export = session.export().await;

        let content: ForwardedRoomKeyEventContent = if let Ok(c) = export.try_into() {
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

        let content = serde_json::to_value(content)?;
        self.encrypt(EventType::ForwardedRoomKey, content).await
    }
}

/// A read only view over all devices belonging to a user.
#[derive(Debug)]
pub struct UserDevices {
    pub(crate) inner: ReadOnlyUserDevices,
    pub(crate) verification_machine: VerificationMachine,
    pub(crate) own_identity: Option<OwnUserIdentity>,
    pub(crate) device_owner_identity: Option<UserIdentities>,
}

impl UserDevices {
    /// Get the specific device with the given device id.
    pub fn get(&self, device_id: &DeviceId) -> Option<Device> {
        self.inner.get(device_id).map(|d| Device {
            inner: d,
            verification_machine: self.verification_machine.clone(),
            own_identity: self.own_identity.clone(),
            device_owner_identity: self.device_owner_identity.clone(),
        })
    }

    /// Iterator over all the device ids of the user devices.
    pub fn keys(&self) -> impl Iterator<Item = &DeviceId> {
        self.inner.keys()
    }

    /// Iterator over all the devices of the user devices.
    pub fn devices(&self) -> impl Iterator<Item = Device> + '_ {
        self.inner.devices().map(move |d| Device {
            inner: d.clone(),
            verification_machine: self.verification_machine.clone(),
            own_identity: self.own_identity.clone(),
            device_owner_identity: self.device_owner_identity.clone(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
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
    /// Create a new Device.
    pub fn new(
        user_id: UserId,
        device_id: Box<DeviceId>,
        display_name: Option<String>,
        trust_state: LocalTrust,
        algorithms: Vec<EventEncryptionAlgorithm>,
        keys: BTreeMap<DeviceKeyId, String>,
        signatures: BTreeMap<UserId, BTreeMap<DeviceKeyId, String>>,
    ) -> Self {
        Self {
            user_id: Arc::new(user_id),
            device_id: Arc::new(device_id),
            display_name: Arc::new(display_name),
            trust_state: Arc::new(Atomic::new(trust_state)),
            signatures: Arc::new(signatures),
            algorithms: algorithms.into(),
            keys: Arc::new(keys),
            deleted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The user id of the device owner.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// The unique ID of the device.
    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// Get the human readable name of the device.
    pub fn display_name(&self) -> &Option<String> {
        &self.display_name
    }

    /// Get the key of the given key algorithm belonging to this device.
    pub fn get_key(&self, algorithm: DeviceKeyAlgorithm) -> Option<&String> {
        self.keys
            .get(&DeviceKeyId::from_parts(algorithm, &self.device_id))
    }

    /// Get a map containing all the device keys.
    pub fn keys(&self) -> &BTreeMap<DeviceKeyId, String> {
        &self.keys
    }

    /// Get a map containing all the device signatures.
    pub fn signatures(&self) -> &BTreeMap<UserId, BTreeMap<DeviceKeyId, String>> {
        &self.signatures
    }

    /// Get the trust state of the device.
    pub fn local_trust_state(&self) -> LocalTrust {
        self.trust_state.load(Ordering::Relaxed)
    }

    /// Is the device locally marked as trusted.
    pub fn is_trusted(&self) -> bool {
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
    /// Note: This should only done in the cryptostore where the trust state can
    /// be stored.
    pub(crate) fn set_trust_state(&self, state: LocalTrust) {
        self.trust_state.store(state, Ordering::Relaxed)
    }

    /// Get the list of algorithms this device supports.
    pub fn algorithms(&self) -> &[EventEncryptionAlgorithm] {
        &self.algorithms
    }

    /// Is the device deleted.
    pub fn deleted(&self) -> bool {
        self.deleted.load(Ordering::Relaxed)
    }

    pub(crate) fn trust_state(
        &self,
        own_identity: &Option<OwnUserIdentity>,
        device_owner: &Option<UserIdentities>,
    ) -> bool {
        // TODO we want to return an enum mentioning if the trust is local, if
        // only the identity is trusted, if the identity and the device are
        // trusted.
        if self.is_trusted() {
            // If the device is localy marked as verified just return so, no
            // need to check signatures.
            true
        } else {
            own_identity.as_ref().map_or(false, |own_identity| {
                // Our own identity needs to be marked as verified.
                own_identity.is_verified()
                    && device_owner
                        .as_ref()
                        .map(|device_identity| match device_identity {
                            // If it's one of our own devices, just check that
                            // we signed the device.
                            UserIdentities::Own(_) => {
                                own_identity.is_device_signed(&self).map_or(false, |_| true)
                            }

                            // If it's a device from someone else, first check
                            // that our user has signed the other user and then
                            // check if the other user has signed this device.
                            UserIdentities::Other(device_identity) => {
                                own_identity
                                    .is_identity_signed(&device_identity)
                                    .map_or(false, |_| true)
                                    && device_identity
                                        .is_device_signed(&self)
                                        .map_or(false, |_| true)
                            }
                        })
                        .unwrap_or(false)
            })
        }
    }

    pub(crate) async fn encrypt(
        &self,
        store: &dyn CryptoStore,
        event_type: EventType,
        content: Value,
    ) -> OlmResult<EncryptedEventContent> {
        let sender_key = if let Some(k) = self.get_key(DeviceKeyAlgorithm::Curve25519) {
            k
        } else {
            warn!(
                "Trying to encrypt a Megolm session for user {} on device {}, \
                but the device doesn't have a curve25519 key",
                self.user_id(),
                self.device_id()
            );
            return Err(EventError::MissingSenderKey.into());
        };

        let session = if let Some(s) = store.get_sessions(sender_key).await? {
            let sessions = s.lock().await;
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

        let message = session.encrypt(&self, event_type, content).await;
        store.save_sessions(&[session]).await?;

        message
    }

    /// Update a device with a new device keys struct.
    pub(crate) fn update_device(&mut self, device_keys: &DeviceKeys) -> Result<(), SignatureError> {
        self.verify_device_keys(device_keys)?;

        let display_name = Arc::new(device_keys.unsigned.device_display_name.clone());

        self.algorithms = device_keys.algorithms.as_slice().into();
        self.keys = Arc::new(device_keys.keys.clone());
        self.signatures = Arc::new(device_keys.signatures.clone());
        self.display_name = display_name;

        Ok(())
    }

    fn is_signed_by_device(&self, json: &mut Value) -> Result<(), SignatureError> {
        let signing_key = self
            .get_key(DeviceKeyAlgorithm::Ed25519)
            .ok_or(SignatureError::MissingSigningKey)?;

        let utility = Utility::new();

        utility.verify_json(
            &self.user_id,
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.device_id()),
            signing_key,
            json,
        )
    }

    pub(crate) fn as_signature_message(&self) -> Value {
        json!({
            "user_id": &*self.user_id,
            "device_id": &*self.device_id,
            "keys": &*self.keys,
            "algorithms": &*self.algorithms,
            "signatures": &*self.signatures,
        })
    }

    pub(crate) fn verify_device_keys(
        &self,
        device_keys: &DeviceKeys,
    ) -> Result<(), SignatureError> {
        self.is_signed_by_device(&mut json!(&device_keys))
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

    #[cfg(test)]
    pub async fn from_machine(machine: &OlmMachine) -> ReadOnlyDevice {
        ReadOnlyDevice::from_account(machine.account()).await
    }

    #[cfg(test)]
    pub async fn from_account(account: &ReadOnlyAccount) -> ReadOnlyDevice {
        let device_keys = account.device_keys().await;
        ReadOnlyDevice::try_from(&device_keys).unwrap()
    }
}

impl TryFrom<&DeviceKeys> for ReadOnlyDevice {
    type Error = SignatureError;

    fn try_from(device_keys: &DeviceKeys) -> Result<Self, Self::Error> {
        let device = Self {
            user_id: Arc::new(device_keys.user_id.clone()),
            device_id: Arc::new(device_keys.device_id.clone()),
            algorithms: device_keys.algorithms.as_slice().into(),
            signatures: Arc::new(device_keys.signatures.clone()),
            keys: Arc::new(device_keys.keys.clone()),
            display_name: Arc::new(device_keys.unsigned.device_display_name.clone()),
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

#[cfg(test)]
pub(crate) mod test {
    use serde_json::json;
    use std::convert::TryFrom;

    use crate::identities::{LocalTrust, ReadOnlyDevice};
    use matrix_sdk_common::{
        encryption::DeviceKeys,
        identifiers::{user_id, DeviceKeyAlgorithm},
    };

    fn device_keys() -> DeviceKeys {
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

    pub(crate) fn get_device() -> ReadOnlyDevice {
        let device_keys = device_keys();
        ReadOnlyDevice::try_from(&device_keys).unwrap()
    }

    #[test]
    fn create_a_device() {
        let user_id = user_id!("@example:localhost");
        let device_id = "BNYQQWUMXO";

        let device = get_device();

        assert_eq!(&user_id, device.user_id());
        assert_eq!(device_id, device.device_id());
        assert_eq!(device.algorithms.len(), 2);
        assert_eq!(LocalTrust::Unset, device.local_trust_state());
        assert_eq!(
            "Alice's mobile phone",
            device.display_name().as_ref().unwrap()
        );
        assert_eq!(
            device.get_key(DeviceKeyAlgorithm::Curve25519).unwrap(),
            "xfgbLIC5WAl1OIkpOzoxpCe8FsRDT6nch7NQsOb15nc"
        );
        assert_eq!(
            device.get_key(DeviceKeyAlgorithm::Ed25519).unwrap(),
            "2/5LWJMow5zhJqakV88SIc7q/1pa8fmkfgAzx72w9G4"
        );
    }

    #[test]
    fn update_a_device() {
        let mut device = get_device();

        assert_eq!(
            "Alice's mobile phone",
            device.display_name().as_ref().unwrap()
        );

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
