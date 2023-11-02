// Copyright 2022 The Matrix.org Foundation C.I.C.
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

//! Types for `m.secret.send` to-device events.

use std::collections::BTreeMap;

use ruma::{events::secret::request::SecretName, OwnedTransactionId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroize;

use super::{EventType, ToDeviceEvent};

/// The `m.secret.send` to-device event.
pub type SecretSendEvent = ToDeviceEvent<SecretSendContent>;

/// The `m.secret.send` event content.
///
/// Sent by a client to share a secret with another device, in response to an
/// `m.secret.request` event. It must be encrypted as an `m.room.encrypted`
/// event, then sent as a to-device event.
#[derive(Clone, Serialize, Deserialize)]
pub struct SecretSendContent {
    /// The ID of the request that this a response to.
    pub request_id: OwnedTransactionId,
    /// The contents of the secret.
    pub secret: String,
    /// The name of the secret, typically not part of the event but can be
    /// inserted when processing `m.secret.send` events so other event consumers
    /// know which secret this event contains.
    #[serde(rename = "name", skip_serializing_if = "Option::is_none")]
    pub secret_name: Option<SecretName>,
    /// Any other, custom and non-specced fields of the content.
    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

impl SecretSendContent {
    /// Create a new `m.secret.send` content.
    pub fn new(request_id: OwnedTransactionId, secret: String) -> Self {
        Self { request_id, secret, secret_name: None, other: Default::default() }
    }
}

impl Zeroize for SecretSendContent {
    fn zeroize(&mut self) {
        self.secret.zeroize();
    }
}

impl Drop for SecretSendContent {
    fn drop(&mut self) {
        self.zeroize()
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for SecretSendContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretSendContent")
            .field("request_id", &self.request_id)
            .finish_non_exhaustive()
    }
}

impl EventType for SecretSendContent {
    const EVENT_TYPE: &'static str = "m.secret.send";
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::{json, Value};

    use super::SecretSendEvent;

    pub(crate) fn json() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
                "request_id": "randomly_generated_id_9573",
                "secret": "ThisIsASecretDon'tTellAnyone"
            },
            "type": "m.secret.send",
        })
    }

    #[test]
    fn deserialization() -> Result<(), serde_json::Error> {
        let json = json();
        let event: SecretSendEvent = serde_json::from_value(json.clone())?;

        assert_eq!(&event.content.secret, "ThisIsASecretDon'tTellAnyone");

        let serialized = serde_json::to_value(event)?;
        assert_eq!(json, serialized);

        Ok(())
    }
}
