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

use ruma::{OwnedDeviceId, OwnedUserId};
use serde::{Deserialize, Serialize};
use vodozemac::Ed25519PublicKey;

use crate::types::DeviceKeys;

/// Information on the device and user that sent the megolm session data to us
///
/// Sessions start off in `UnknownDevice` state, and progress into `DeviceInfo`
/// state when we get the device info. Finally, if we can look up the sender
/// using the device info, the session can be moved into `SenderKnown` state.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub enum SenderData {
    /// We have not yet found the (signed) device info for the sending device,
    /// or we did find a device but it does not own the session.
    UnknownDevice {
        /// Was this session created before we started collecting trust
        /// information about sessions? If so, we may choose to display its
        /// messages even though trust info is missing.
        legacy_session: bool,

        /// If true, we found the device but it was not the owner of the
        /// session. If false, we could not find the device.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        #[serde(default)]
        owner_check_failed: bool,
    },

    /// We have the signed device info for the sending device, but not yet the
    /// cross-signing key that it was signed with.
    DeviceInfo {
        /// Information about the device that sent the to-device message
        /// creating this session.
        device_keys: DeviceKeys,

        /// Was this session created before we started collecting trust
        /// information about sessions? If so, we may choose to display its
        /// messages even though trust info is missing.
        legacy_session: bool,
    },

    /// We have found proof that this user, with this cross-signing key, sent
    /// the to-device message that established this session.
    SenderKnown {
        /// The user ID of the user who established this session.
        user_id: OwnedUserId,

        /// The device ID of the device that send the session.
        /// This is an `Option` for backwards compatibility, but we should
        /// always populate it on creation.
        device_id: Option<OwnedDeviceId>,

        /// The cross-signing key of the user who established this session.
        master_key: Box<Ed25519PublicKey>,

        /// Whether, at the time we checked the signature on the device,
        /// we had actively verified that `master_key` belongs to the user.
        /// If false, we had simply accepted the key as this user's latest
        /// key.
        master_key_verified: bool,
    },
}

impl SenderData {
    /// Create a [`SenderData`] which contains no device info and will be
    /// retried soon.
    pub fn unknown() -> Self {
        Self::UnknownDevice {
            // TODO: when we have implemented all of SenderDataFinder,
            // legacy_session should be set to false, but for now we leave
            // it as true because we might lose device info while
            // this code is still in transition.
            legacy_session: true,
            owner_check_failed: false,
        }
    }

    /// Create a [`SenderData`] which has the legacy flag set. Caution: messages
    /// within sessions with this flag will be displayed in some contexts,
    /// even when we are unable to verify the sender.
    ///
    /// The returned struct contains no device info, and will be retried soon.
    pub fn legacy() -> Self {
        Self::UnknownDevice { legacy_session: true, owner_check_failed: false }
    }

    /// Return our type: `UnknownDevice`, `DeviceInfo`, or `SenderKnown`.
    pub fn to_type(&self) -> SenderDataType {
        match self {
            Self::UnknownDevice { .. } => SenderDataType::UnknownDevice,
            Self::DeviceInfo { .. } => SenderDataType::DeviceInfo,
            Self::SenderKnown { .. } => SenderDataType::SenderKnown,
        }
    }
}

/// Used when deserialising and the sender_data property is missing.
/// If we are deserialising an InboundGroupSession session with missing
/// sender_data, this must be a legacy session (i.e. it was created before we
/// started tracking sender data). We set its legacy flag to true, and set it up
/// to be retried soon, so we can populate it with trust information if it is
/// available.
impl Default for SenderData {
    fn default() -> Self {
        Self::legacy()
    }
}

/// Used when serializing [`crate::olm::group_sessions::InboundGroupSession`]s.
/// We want just the type of the session's [`SenderData`] to be queryable, so we
/// store the type as a separate column/property in the database.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
pub enum SenderDataType {
    /// The [`SenderData`] is of type `UnknownDevice`.
    UnknownDevice = 1,
    /// The [`SenderData`] is of type `DeviceInfo`.
    DeviceInfo = 2,
    /// The [`SenderData`] is of type `SenderKnown`.
    SenderKnown = 3,
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_let;

    use super::SenderData;

    #[test]
    fn serializing_unknown_device_correctly_preserves_owner_check_failed_if_true() {
        // Given an unknown device SenderData with failed owner check
        let start = SenderData::UnknownDevice { legacy_session: false, owner_check_failed: true };

        // When we round-trip it to JSON and back
        let json = serde_json::to_string(&start).unwrap();
        let end: SenderData = serde_json::from_str(&json).unwrap();

        // Then the failed owner check flag is preserved
        assert_let!(SenderData::UnknownDevice { owner_check_failed, .. } = &end);
        assert!(owner_check_failed);

        // And for good measure, everything is preserved
        assert_eq!(start, end);
    }

    #[test]
    fn serializing_unknown_device_without_failed_owner_check_excludes_it() {
        // Given an unknown device SenderData with owner_check_failed==false
        let start = SenderData::UnknownDevice { legacy_session: false, owner_check_failed: false };

        // When we write it to JSON
        let json = serde_json::to_string(&start).unwrap();

        // Then the JSON does not mention `owner_check_failed`
        assert!(!json.contains("owner_check_failed"), "JSON contains 'owner_check_failed'!");

        // And for good measure, it round-trips fully
        let end: SenderData = serde_json::from_str(&json).unwrap();
        assert_eq!(start, end);
    }

    #[test]
    fn deserializing_unknown_device_with_extra_retry_info_ignores_it() {
        // Previously, SenderData contained `retry_details` but it is no longer needed -
        // just check that we are able to deserialize even if it is present.
        let json = r#"
            {
                "UnknownDevice":{
                    "retry_details":{
                        "retry_count":3,
                        "next_retry_time_ms":10000
                    },
                    "legacy_session":false
                }
            }
            "#;

        let end: SenderData = serde_json::from_str(json).expect("Failed to parse!");
        assert_let!(SenderData::UnknownDevice { .. } = end);
    }

    #[test]
    fn deserializing_senderknown_without_device_id_defaults_to_none() {
        let json = r#"
            {
                "SenderKnown":{
                    "user_id":"@u:s.co",
                    "master_key":[
                        150,140,249,139,141,29,63,230,179,14,213,175,176,61,11,255,
                        26,103,10,51,100,154,183,47,181,117,87,204,33,215,241,92
                    ],
                    "master_key_verified":true
                }
            }
            "#;

        let end: SenderData = serde_json::from_str(json).expect("Failed to parse!");
        assert_let!(SenderData::SenderKnown { .. } = end);
    }
}
