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

use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use atomic::Atomic;
use olm_rs::utility::OlmUtility;
use serde_json::{json, Value};

#[cfg(test)]
use super::OlmMachine;
use matrix_sdk_common::api::r0::keys::{DeviceKeys, KeyAlgorithm};
use matrix_sdk_common::events::Algorithm;
use matrix_sdk_common::identifiers::{DeviceId, UserId};

use crate::error::SignatureError;

/// A device represents a E2EE capable client of an user.
#[derive(Debug, Clone)]
pub struct Device {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceId>,
    algorithms: Arc<Vec<Algorithm>>,
    keys: Arc<BTreeMap<KeyAlgorithm, String>>,
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
        device_id: DeviceId,
        display_name: Option<String>,
        trust_state: TrustState,
        algorithms: Vec<Algorithm>,
        keys: BTreeMap<KeyAlgorithm, String>,
    ) -> Self {
        Device {
            user_id: Arc::new(user_id),
            device_id: Arc::new(device_id),
            display_name: Arc::new(display_name),
            trust_state: Arc::new(Atomic::new(trust_state)),
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
        self.keys.get(&algorithm)
    }

    /// Get a map containing all the device keys.
    pub fn keys(&self) -> &BTreeMap<KeyAlgorithm, String> {
        &self.keys
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

        let mut keys = BTreeMap::new();

        for (key_id, key) in device_keys.keys.iter() {
            let key_id = key_id.0;
            let _ = keys.insert(key_id, key.clone());
        }

        let display_name = Arc::new(
            device_keys
                .unsigned
                .as_ref()
                .map(|d| d.device_display_name.clone())
                .flatten(),
        );

        let _ = mem::replace(
            &mut self.algorithms,
            Arc::new(device_keys.algorithms.clone()),
        );
        let _ = mem::replace(&mut self.keys, Arc::new(keys));
        let _ = mem::replace(&mut self.display_name, display_name);

        Ok(())
    }

    fn is_signed_by_device(&self, json: &mut Value) -> Result<(), SignatureError> {
        let signing_key = self.keys.get(&KeyAlgorithm::Ed25519).unwrap();

        let json_object = json.as_object_mut().ok_or(SignatureError::NotAnObject)?;
        let unsigned = json_object.remove("unsigned");
        let signatures = json_object.remove("signatures");

        let canonical_json = cjson::to_string(json_object)?;

        if let Some(u) = unsigned {
            json_object.insert("unsigned".to_string(), u);
        }

        // TODO this should be part of ruma-client-api.
        let key_id_string = format!("{}:{}", KeyAlgorithm::Ed25519, self.device_id);

        let signatures = signatures.ok_or(SignatureError::NoSignatureFound)?;
        let signature_object = signatures
            .as_object()
            .ok_or(SignatureError::NoSignatureFound)?;
        let signature = signature_object
            .get(&self.user_id.to_string())
            .ok_or(SignatureError::NoSignatureFound)?;
        let signature = signature
            .get(key_id_string)
            .ok_or(SignatureError::NoSignatureFound)?;
        let signature = signature.as_str().ok_or(SignatureError::NoSignatureFound)?;

        let utility = OlmUtility::new();

        let ret = if utility
            .ed25519_verify(signing_key, &canonical_json, signature)
            .is_ok()
        {
            Ok(())
        } else {
            Err(SignatureError::VerificationError)
        };

        json_object.insert("signatures".to_string(), signatures);

        ret
    }

    pub(crate) fn verify_device_keys(
        &self,
        device_keys: &DeviceKeys,
    ) -> Result<(), SignatureError> {
        self.is_signed_by_device(&mut json!(&device_keys))
    }

    /// Mark the device as deleted.
    pub(crate) fn mark_as_deleted(&self) {
        self.deleted.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
impl From<&OlmMachine> for Device {
    fn from(machine: &OlmMachine) -> Self {
        Device {
            user_id: Arc::new(machine.user_id().clone()),
            device_id: Arc::new(machine.device_id().clone()),
            algorithms: Arc::new(vec![
                Algorithm::MegolmV1AesSha2,
                Algorithm::OlmV1Curve25519AesSha2,
            ]),
            keys: Arc::new(
                machine
                    .identity_keys()
                    .iter()
                    .map(|(key, value)| {
                        (
                            KeyAlgorithm::try_from(key.as_ref()).unwrap(),
                            value.to_owned(),
                        )
                    })
                    .collect(),
            ),
            display_name: Arc::new(None),
            deleted: Arc::new(AtomicBool::new(false)),
            trust_state: Arc::new(Atomic::new(TrustState::Unset)),
        }
    }
}

impl TryFrom<&DeviceKeys> for Device {
    type Error = SignatureError;

    fn try_from(device_keys: &DeviceKeys) -> Result<Self, Self::Error> {
        let mut keys = BTreeMap::new();

        for (key_id, key) in device_keys.keys.iter() {
            let key_id = key_id.0;
            let _ = keys.insert(key_id, key.clone());
        }

        let device = Device {
            user_id: Arc::new(device_keys.user_id.clone()),
            device_id: Arc::new(device_keys.device_id.clone()),
            algorithms: Arc::new(device_keys.algorithms.clone()),
            keys: Arc::new(keys),
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
    use matrix_sdk_common::api::r0::keys::{DeviceKeys, KeyAlgorithm};
    use matrix_sdk_common::identifiers::UserId;

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
