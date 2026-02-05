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

//! Matrix event content for DCGKA updates

use std::collections::HashSet;

use ruma::{OwnedUserId, events::macros::EventContent};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Content of m.room.dcgka.update event
#[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
#[ruma_event(type = "m.room.dcgka.update", kind = MessageLike)]
pub struct DcgkaUpdateEventContent {
    /// Unique identifier for this update
    pub update_id: Ulid,

    /// Type of update: "Add", "Remove", or "Rotate"
    pub update_type: String,

    /// Matrix user ID of the device that created this update
    pub issuer: OwnedUserId,

    /// Set of update_ids that must be accepted first
    #[serde(default)]
    pub dependencies: HashSet<Ulid>,

    /// Type-specific payload
    pub payload: serde_json::Value,

    /// Ed25519 signature (base64-encoded)
    pub signature: String,

    /// Optional client timestamp
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
}

impl DcgkaUpdateEventContent {
    /// Create new event content from a DCGKA update
    pub fn new(
        update_id: Ulid,
        update_type: String,
        issuer: OwnedUserId,
        dependencies: HashSet<Ulid>,
        payload: serde_json::Value,
        signature: String,
        timestamp: Option<u64>,
    ) -> Self {
        Self { update_id, update_type, issuer, dependencies, payload, signature, timestamp }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_content_creation() {
        let content = DcgkaUpdateEventContent::new(
            Ulid::new(),
            "Rotate".to_owned(),
            ruma::user_id!("@alice:example.org").to_owned(),
            HashSet::new(),
            serde_json::json!({"entropy": "base64..."}),
            "signature_base64".to_owned(),
            Some(1234567890),
        );

        assert_eq!(content.update_type, "Rotate");
        assert_eq!(content.issuer.as_str(), "@alice:example.org");
    }
}
