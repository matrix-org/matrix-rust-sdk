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

use ruma_client_api::r0::keys::{DeviceKeys, KeyAlgorithm};
use ruma_events::Algorithm;

#[derive(Debug, Clone)]
pub struct Device {
    user_id: Arc<String>,
    device_id: Arc<String>,
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
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
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
            user_id: Arc::new(device_keys.user_id.to_string()),
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
