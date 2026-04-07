// Copyright 2026 The Matrix.org Foundation C.I.C.
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

//! Types for `io.element.msc4385.secret.push` to-device events.

// This is here because we have a zeroize(skip) further below, which incorrectly triggers a
// unused_assignments warning due to the macro not using a variable.
//
// This will be fixed once we bump Zeroize.
#![allow(unused_assignments)]

use std::collections::BTreeMap;

use ruma::events::secret::request::SecretName;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::{Zeroize, ZeroizeOnDrop};

use super::{EventType, ToDeviceEvent};

/// The `io.element.msc4385.secret.push` to-device event.
pub type SecretPushEvent = ToDeviceEvent<SecretPushContent>;

/// The `io.element.msc4385.secret.push` event content.
///
/// Sent by a client to push a secret with another device. It must be encrypted
/// as an `m.room.encrypted` event, then sent as a to-device event.
#[derive(Clone, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct SecretPushContent {
    /// The name of the secret.
    #[zeroize(skip)]
    pub name: SecretName,
    /// The contents of the secret.
    pub secret: String,
    /// Any other, custom and non-specced fields of the content.
    #[serde(flatten)]
    #[zeroize(skip)]
    other: BTreeMap<String, Value>,
}

impl SecretPushContent {
    /// Create a new `io.element.msc4385.secret.push` content.
    pub fn new(name: SecretName, secret: String) -> Self {
        Self { name, secret, other: Default::default() }
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for SecretPushContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretPushContent").field("name", &self.name).finish_non_exhaustive()
    }
}

impl EventType for SecretPushContent {
    const EVENT_TYPE: &'static str = "io.element.msc4385.secret.push";
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::{Value, json};

    use super::SecretPushEvent;

    pub(crate) fn json() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
                "name": "org.example.secret.name",
                "secret": "ThisIsASecretDon'tTellAnyone"
            },
            "type": "io.element.msc4385.secret.push",
        })
    }

    #[test]
    fn deserialization() -> Result<(), serde_json::Error> {
        let json = json();
        let event: SecretPushEvent = serde_json::from_value(json.clone())?;

        assert_eq!(event.content.name.as_str(), "org.example.secret.name");
        assert_eq!(&event.content.secret, "ThisIsASecretDon'tTellAnyone");

        let serialized = serde_json::to_value(event)?;
        assert_eq!(json, serialized);

        Ok(())
    }
}
