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

//! DCGKA update types and serialization

use ruma::{OwnedDeviceId, OwnedUserId};
use serde::{Deserialize, Serialize};
use ulid::Ulid;
use vodozemac::Ed25519PublicKey;

use crate::types::{deserialize_ed25519_key, serialize_ed25519_key};

/// Type of DCGKA operation
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateType {
    /// Add a new member to the group
    Add,
    /// Remove an existing member from the group
    Remove,
    /// Rotate the group key without changing membership
    Rotate,
}

/// Type-specific payload for DCGKA updates
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UpdatePayload {
    /// Add member payload
    Add {
        /// Matrix user ID of member being added
        member_id: OwnedUserId,
        /// Ed25519 public key for signature verification
        #[serde(
            serialize_with = "serialize_ed25519_key",
            deserialize_with = "deserialize_ed25519_key"
        )]
        member_public_key: Ed25519PublicKey,
    },
    /// Remove member payload
    Remove {
        /// Matrix user ID of member being removed
        member_id: OwnedUserId,
    },
    /// Rotate key payload
    Rotate {
        /// Fresh random entropy for key rotation
        entropy: Vec<u8>,
    },
}

impl UpdatePayload {
    /// Get the update type from the payload variant
    pub fn update_type(&self) -> UpdateType {
        match self {
            UpdatePayload::Add { .. } => UpdateType::Add,
            UpdatePayload::Remove { .. } => UpdateType::Remove,
            UpdatePayload::Rotate { .. } => UpdateType::Rotate,
        }
    }

    /// Get the affected member ID (if applicable)
    pub fn member_id(&self) -> Option<&OwnedUserId> {
        match self {
            UpdatePayload::Add { member_id, .. } | UpdatePayload::Remove { member_id } => {
                Some(member_id)
            }
            UpdatePayload::Rotate { .. } => None,
        }
    }
}

/// Immutable, signed DCGKA update representing a membership or key rotation operation
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DcgkaUpdate {
    /// Unique identifier for this update
    pub update_id: Ulid,

    /// Matrix user ID of the device that created this update
    pub issuer: OwnedUserId,

    /// Device ID of the issuer
    pub issuer_device: OwnedDeviceId,

    /// Type of update (derived from payload)
    pub update_type: UpdateType,

    /// Set of update_ids that must be Accepted before this update can be Accepted
    #[serde(default)]
    pub dependencies: Vec<Ulid>,

    /// Type-specific update data
    pub payload: UpdatePayload,

    /// Ed25519 signature over canonical JSON (excluding this field)
    pub signature: Vec<u8>,

    /// Optional client timestamp (milliseconds since epoch) for debugging
    pub timestamp: u64,
}

impl DcgkaUpdate {
    /// Create a new unsigned update (signature must be added separately)
    pub fn new(
        issuer: OwnedUserId,
        issuer_device: OwnedDeviceId,
        dependencies: Vec<Ulid>,
        payload: UpdatePayload,
    ) -> Self {
        let update_type = payload.update_type();
        Self {
            update_id: Ulid::new(),
            issuer,
            issuer_device,
            update_type,
            dependencies,
            payload,
            signature: vec![],
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("System time before Unix epoch")
                .as_millis() as u64,
        }
    }

    /// Get the update type
    pub fn update_type(&self) -> UpdateType {
        self.payload.update_type()
    }

    /// Get canonical JSON for signing (excludes signature and timestamp fields)
    pub fn canonical_json(&self) -> String {
        let mut deps = self.dependencies.clone();
        deps.sort();

        let json_value = serde_json::json!({
            "update_id": self.update_id,
            "issuer": self.issuer,
            "issuer_device": self.issuer_device,
            "update_type": self.update_type,
            "dependencies": deps,
            "payload": self.payload,
        });

        serde_json::to_string(&json_value).expect("Failed to serialize")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_payload_variants() {
        let add = UpdatePayload::Add {
            member_id: ruma::user_id!("@alice:example.org").to_owned(),
            member_public_key: Ed25519PublicKey::from_slice(&[1u8; 32]).expect("Valid key"),
        };
        assert_eq!(add.update_type(), UpdateType::Add);
        assert!(add.member_id().is_some());

        let remove =
            UpdatePayload::Remove { member_id: ruma::user_id!("@bob:example.org").to_owned() };
        assert_eq!(remove.update_type(), UpdateType::Remove);
        assert!(remove.member_id().is_some());

        let rotate = UpdatePayload::Rotate { entropy: vec![42u8; 32] };
        assert_eq!(rotate.update_type(), UpdateType::Rotate);
        assert!(rotate.member_id().is_none());
    }

    #[test]
    fn test_dcgka_update_creation() {
        let issuer = ruma::user_id!("@alice:example.org").to_owned();
        let issuer_device = ruma::device_id!("DEVICE").to_owned();
        let payload = UpdatePayload::Rotate { entropy: vec![1u8; 32] };

        let update = DcgkaUpdate::new(issuer.clone(), issuer_device.clone(), vec![], payload);

        assert_eq!(update.issuer, issuer);
        assert_eq!(update.issuer_device, issuer_device);
        assert!(update.dependencies.is_empty());
        assert_eq!(update.update_type(), UpdateType::Rotate);
    }

    #[test]
    fn test_canonical_json_excludes_signature() {
        let update = DcgkaUpdate::new(
            ruma::user_id!("@alice:example.org").to_owned(),
            ruma::device_id!("DEVICE").to_owned(),
            vec![],
            UpdatePayload::Rotate { entropy: vec![1u8; 32] },
        );

        let json = update.canonical_json();
        assert!(json.contains("update_id"));
        assert!(json.contains("issuer"));
        assert!(json.contains("payload"));
        assert!(!json.contains("signature"));
        assert!(!json.contains("timestamp"));
    }
}
