// Copyright 2021 The Matrix.org Foundation C.I.C.
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

use matrix_sdk_crypto::Device as RSDevice;
use napi_derive::napi;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[napi(object)]
#[derive(Serialize, Deserialize)]
pub struct Device {
    pub user_id: String,
    pub device_id: String,
    pub keys: Map<String, Value>,
    pub algorithms: Vec<String>,
    pub display_name: Option<String>,
    pub is_blocked: bool,
    pub locally_trusted: bool,
    pub cross_signing_trusted: bool,
}

impl From<RSDevice> for Device {
    fn from(d: RSDevice) -> Self {
        Device {
            user_id: d.user_id().to_string(),
            device_id: d.device_id().to_string(),
            keys: d
                .keys()
                .iter()
                .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
                .collect::<Map<String, Value>>(),
            algorithms: d.algorithms().iter().map(|a| a.to_string()).collect(),
            display_name: d.display_name().map(|d| d.to_owned()),
            is_blocked: d.is_blacklisted(),
            locally_trusted: d.is_locally_trusted(),
            cross_signing_trusted: d.is_cross_signing_trusted(),
        }
    }
}
