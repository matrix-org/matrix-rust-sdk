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
use std::sync::atomic::AtomicBool;
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

#[derive(Debug, Clone, Copy)]
pub enum TrustState {
    Verified,
    BlackListed,
    Ignored,
    Unset,
}

impl Device {
    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    pub fn keys(&self, algorithm: &KeyAlgorithm) -> Option<&String> {
        self.keys.get(algorithm)
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
    use crate::crypto::device::Device;
    use crate::identifiers::UserId;

    pub(crate) fn get_device() -> Device {
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

        let device_keys: DeviceKeys = serde_json::from_value(device_keys).unwrap();

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
        assert_eq!(
            device.keys(&KeyAlgorithm::Curve25519).unwrap(),
            "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4"
        );
        assert_eq!(
            device.keys(&KeyAlgorithm::Ed25519).unwrap(),
            "nE6W2fCblxDcOFmeEtCHNl8/l8bXcu7GKyAswA4r3mM"
        );
    }
}
