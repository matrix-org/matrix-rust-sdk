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
    convert::TryFrom,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use atomic::Atomic;
use serde_json::{json, Value};

#[cfg(test)]
use super::OlmMachine;
use matrix_sdk_common::{
    api::r0::keys::{AlgorithmAndDeviceId, DeviceKeys, KeyAlgorithm, SignedKey},
    events::Algorithm,
    identifiers::{DeviceId, UserId},
};

use crate::{error::SignatureError, verify_json};

/// A device represents a E2EE capable client of an user.
#[derive(Debug, Clone)]
pub struct Device {
    user_id: Arc<UserId>,
    device_id: Arc<Box<DeviceId>>,
    algorithms: Arc<Vec<Algorithm>>,
    keys: Arc<BTreeMap<AlgorithmAndDeviceId, String>>,
    signatures: Arc<BTreeMap<UserId, BTreeMap<AlgorithmAndDeviceId, String>>>,
    display_name: Arc<Option<String>>,
    deleted: Arc<AtomicBool>,
    trust_state: Arc<Atomic<TrustState>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
/// The trust state of a device.
pub enum TrustState {
    /// The device has been verified and is trusted.
    Verified = 0,
    /// The device been blacklisted from communicating.
    BlackListed = 1,
    /// The trust state of the device is being ignored.
    Ignored = 2,
    /// The trust state is unset.
    Unset = 3,
}

impl From<i64> for TrustState {
    fn from(state: i64) -> Self {
        match state {
            0 => TrustState::Verified,
            1 => TrustState::BlackListed,
            2 => TrustState::Ignored,
            3 => TrustState::Unset,
            _ => TrustState::Unset,
        }
    }
}

impl Device {
    /// Create a new Device.
    pub fn new(
        user_id: UserId,
        device_id: Box<DeviceId>,
        display_name: Option<String>,
        trust_state: TrustState,
        algorithms: Vec<Algorithm>,
        keys: BTreeMap<AlgorithmAndDeviceId, String>,
        signatures: BTreeMap<UserId, BTreeMap<AlgorithmAndDeviceId, String>>,
    ) -> Self {
        Device {
            user_id: Arc::new(user_id),
            device_id: Arc::new(device_id),
            display_name: Arc::new(display_name),
            trust_state: Arc::new(Atomic::new(trust_state)),
            signatures: Arc::new(signatures),
            algorithms: Arc::new(algorithms),
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
    pub fn get_key(&self, algorithm: KeyAlgorithm) -> Option<&String> {
        self.keys.get(&AlgorithmAndDeviceId(
            algorithm,
            self.device_id.as_ref().clone(),
        ))
    }

    /// Get a map containing all the device keys.
    pub fn keys(&self) -> &BTreeMap<AlgorithmAndDeviceId, String> {
        &self.keys
    }

    /// Get a map containing all the device signatures.
    pub fn signatures(&self) -> &BTreeMap<UserId, BTreeMap<AlgorithmAndDeviceId, String>> {
        &self.signatures
    }

    /// Get the trust state of the device.
    pub fn trust_state(&self) -> TrustState {
        self.trust_state.load(Ordering::Relaxed)
    }

    /// Get the list of algorithms this device supports.
    pub fn algorithms(&self) -> &[Algorithm] {
        &self.algorithms
    }

    /// Is the device deleted.
    pub fn deleted(&self) -> bool {
        self.deleted.load(Ordering::Relaxed)
    }

    /// Update a device with a new device keys struct.
    pub(crate) fn update_device(&mut self, device_keys: &DeviceKeys) -> Result<(), SignatureError> {
        self.verify_device_keys(device_keys)?;

        let display_name = Arc::new(
            device_keys
                .unsigned
                .as_ref()
                .map(|d| d.device_display_name.clone())
                .flatten(),
        );

        self.algorithms = Arc::new(device_keys.algorithms.clone());
        self.keys = Arc::new(device_keys.keys.clone());
        self.signatures = Arc::new(device_keys.signatures.clone());
        self.display_name = display_name;

        Ok(())
    }

    fn is_signed_by_device(&self, json: &mut Value) -> Result<(), SignatureError> {
        let signing_key = self
            .get_key(KeyAlgorithm::Ed25519)
            .ok_or(SignatureError::MissingSigningKey)?;

        verify_json(&self.user_id, &self.device_id.as_str(), signing_key, json)
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
    pub async fn from_machine(machine: &OlmMachine) -> Device {
        let device_keys = machine.account.device_keys().await;
        Device::try_from(&device_keys).unwrap()
    }
}

impl TryFrom<&DeviceKeys> for Device {
    type Error = SignatureError;

    fn try_from(device_keys: &DeviceKeys) -> Result<Self, Self::Error> {
        let device = Device {
            user_id: Arc::new(device_keys.user_id.clone()),
            device_id: Arc::new(device_keys.device_id.clone()),
            algorithms: Arc::new(device_keys.algorithms.clone()),
            signatures: Arc::new(device_keys.signatures.clone()),
            keys: Arc::new(device_keys.keys.clone()),
            display_name: Arc::new(
                device_keys
                    .unsigned
                    .as_ref()
                    .map(|d| d.device_display_name.clone())
                    .flatten(),
            ),
            deleted: Arc::new(AtomicBool::new(false)),
            trust_state: Arc::new(Atomic::new(TrustState::Unset)),
        };

        device.verify_device_keys(device_keys)?;
        Ok(device)
    }
}

impl PartialEq for Device {
    fn eq(&self, other: &Self) -> bool {
        self.user_id() == other.user_id() && self.device_id() == other.device_id()
    }
}

#[cfg(test)]
pub(crate) mod test {
    use serde_json::json;
    use std::convert::TryFrom;

    use crate::device::{Device, TrustState};
    use matrix_sdk_common::{
        api::r0::keys::{DeviceKeys, KeyAlgorithm},
        identifiers::UserId,
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

    pub(crate) fn get_device() -> Device {
        let device_keys = device_keys();
        Device::try_from(&device_keys).unwrap()
    }

    #[test]
    fn create_a_device() {
        let user_id = UserId::try_from("@example:localhost").unwrap();
        let device_id = "BNYQQWUMXO";

        let device = get_device();

        assert_eq!(&user_id, device.user_id());
        assert_eq!(device_id, device.device_id());
        assert_eq!(device.algorithms.len(), 2);
        assert_eq!(TrustState::Unset, device.trust_state());
        assert_eq!(
            "Alice's mobile phone",
            device.display_name().as_ref().unwrap()
        );
        assert_eq!(
            device.get_key(KeyAlgorithm::Curve25519).unwrap(),
            "xfgbLIC5WAl1OIkpOzoxpCe8FsRDT6nch7NQsOb15nc"
        );
        assert_eq!(
            device.get_key(KeyAlgorithm::Ed25519).unwrap(),
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

        let mut device_keys = device_keys();
        device_keys.unsigned.as_mut().unwrap().device_display_name =
            Some("Alice's work computer".to_owned());
        device.update_device(&device_keys).unwrap();

        assert_eq!(
            "Alice's work computer",
            device.display_name().as_ref().unwrap()
        );
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
