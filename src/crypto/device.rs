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

use std::collections::HashMap;
use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use atomic::Atomic;

use crate::api::r0::keys::{DeviceKeys, KeyAlgorithm};
use crate::events::Algorithm;
use crate::identifiers::{DeviceId, UserId};

#[derive(Debug, Clone)]
pub struct Device {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceId>,
    algorithms: Arc<Vec<Algorithm>>,
    keys: Arc<HashMap<KeyAlgorithm, String>>,
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
        keys: HashMap<KeyAlgorithm, String>,
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
    pub fn get_key(&self, algorithm: &KeyAlgorithm) -> Option<&String> {
        self.keys.get(algorithm)
    }

    /// Get a map containing all the device keys.
    pub fn keys(&self) -> &HashMap<KeyAlgorithm, String> {
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

    /// Update a device with a new device keys struct.
    pub(crate) fn update_device(&mut self, device_keys: &DeviceKeys) {
        let mut keys = HashMap::new();

        for (key_id, key) in device_keys.keys.iter() {
            let key_id = key_id.0;
            keys.insert(key_id, key.clone());
        }

        let display_name = Arc::new(
            device_keys
                .unsigned
                .as_ref()
                .map(|d| d.device_display_name.clone()),
        );

        mem::replace(
            &mut self.algorithms,
            Arc::new(device_keys.algorithms.clone()),
        );
        mem::replace(&mut self.keys, Arc::new(keys));
        mem::replace(&mut self.display_name, display_name);
    }
}

impl From<&DeviceKeys> for Device {
    fn from(device_keys: &DeviceKeys) -> Self {
        let mut keys = HashMap::new();

        for (key_id, key) in device_keys.keys.iter() {
            let key_id = key_id.0;
            keys.insert(key_id, key.clone());
        }

        Device {
            user_id: Arc::new(device_keys.user_id.clone()),
            device_id: Arc::new(device_keys.device_id.clone()),
            algorithms: Arc::new(device_keys.algorithms.clone()),
            keys: Arc::new(keys),
            display_name: Arc::new(
                device_keys
                    .unsigned
                    .as_ref()
                    .map(|d| d.device_display_name.clone()),
            ),
            deleted: Arc::new(AtomicBool::new(false)),
            trust_state: Arc::new(Atomic::new(TrustState::Unset)),
        }
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
    use std::convert::{From, TryFrom};

    use crate::api::r0::keys::{DeviceKeys, KeyAlgorithm};
    use crate::crypto::device::{Device, TrustState};
    use crate::identifiers::UserId;

    fn device_keys() -> DeviceKeys {
        let user_id = UserId::try_from("@alice:example.org").unwrap();
        let device_id = "DEVICEID";

        let device_keys = json!({
          "algorithms": vec![
              "m.olm.v1.curve25519-aes-sha2",
              "m.megolm.v1.aes-sha2"
          ],
          "device_id": device_id,
          "user_id": user_id.to_string(),
          "keys": {
              "curve25519:DEVICEID": "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4",
              "ed25519:DEVICEID": "nE6W2fCblxDcOFmeEtCHNl8/l8bXcu7GKyAswA4r3mM"
          },
          "signatures": {
              user_id.to_string(): {
                  "ed25519:DEVICEID": "m53Wkbh2HXkc3vFApZvCrfXcX3AI51GsDHustMhKwlv3TuOJMj4wistcOTM8q2+e/Ro7rWFUb9ZfnNbwptSUBA"
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
        Device::from(&device_keys)
    }

    #[test]
    fn create_a_device() {
        let user_id = UserId::try_from("@alice:example.org").unwrap();
        let device_id = "DEVICEID";

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
            device.get_key(&KeyAlgorithm::Curve25519).unwrap(),
            "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4"
        );
        assert_eq!(
            device.get_key(&KeyAlgorithm::Ed25519).unwrap(),
            "nE6W2fCblxDcOFmeEtCHNl8/l8bXcu7GKyAswA4r3mM"
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
            "Alice's work computer".to_owned();
        device.update_device(&device_keys);

        assert_eq!(
            "Alice's work computer",
            device.display_name().as_ref().unwrap()
        );
    }
}
