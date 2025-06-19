// Copyright 2023 The Matrix.org Foundation C.I.C.
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

//! Types for `m.dummy` to-device events.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{EventType, ToDeviceEvent};

/// The `m.dummy` to-device event.
pub type DummyEvent = ToDeviceEvent<DummyEventContent>;

/// The content of an `m.dummy` event.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DummyEventContent {
    /// Any other, custom and non-specced fields of the content.
    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

impl DummyEventContent {
    /// Create a new `m.dummy` event content.
    pub fn new() -> Self {
        Default::default()
    }
}

impl EventType for DummyEventContent {
    const EVENT_TYPE: &'static str = "m.dummy";
}

#[cfg(test)]
pub(super) mod tests {
    use serde_json::{Value, json};

    use super::DummyEvent;

    pub fn json() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
                "m.custom": "something custom",
            },
            "type": "m.dummy",
            "m.custom.top": "something custom in the top",
        })
    }

    #[test]
    fn deserialization() -> Result<(), serde_json::Error> {
        let json = json();
        let event: DummyEvent = serde_json::from_value(json.clone())?;

        let serialized = serde_json::to_value(event)?;
        assert_eq!(json, serialized);

        Ok(())
    }
}
