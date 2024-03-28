// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use matrix_sdk_base::crypto::types::SecretsBundle;
use ruma::OwnedDeviceId;
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Serialize, Deserialize)]
pub enum QrAuthMessage {
    #[serde(rename = "m.login.protocols")]
    LoginProtocols { protocols: Vec<u8>, homeserver: Url },
    #[serde(rename = "m.login.protocol")]
    LoginProtocol { device_authorization_grant: AuthorizationGrant, protocol: u8 },
    #[serde(rename = "m.login.protocol_accepted")]
    LoginProtocolAccepted(),
    #[serde(rename = "m.login.success")]
    LoginSuccess { device_id: OwnedDeviceId },
    #[serde(rename = "m.login.declined")]
    LoginDeclined(),
    #[serde(rename = "m.login.secrets")]
    LoginSecrets(SecretsBundle),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthorizationGrant {
    pub verification_uri: Url,
    pub verification_uri_complete: Url,
}
