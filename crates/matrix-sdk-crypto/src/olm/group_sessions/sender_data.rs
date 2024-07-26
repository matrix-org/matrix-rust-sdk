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

use ruma::{MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedUserId};
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
        /// When we will next try again to find device info for this session,
        /// and how many times we have tried
        retry_details: SenderDataRetryDetails,

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
        /// When we will next try again to find a cross-signing key that signed
        /// the device information, and how many times we have tried.
        retry_details: SenderDataRetryDetails,

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
            retry_details: SenderDataRetryDetails::retry_soon(),
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
        Self::UnknownDevice {
            retry_details: SenderDataRetryDetails::retry_soon(),
            legacy_session: true,
            owner_check_failed: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn unknown_retry_at(retry_details: SenderDataRetryDetails) -> Self {
        Self::UnknownDevice { retry_details, legacy_session: false, owner_check_failed: false }
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

/// Tracking information about when we need to try again fetching device or
/// user information, and how many times we have already tried.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SenderDataRetryDetails {
    /// How many times we have already tried to find the currently-needed
    /// information for this session.
    pub retry_count: u8,

    /// What time to try again to find the currently-needed information.
    pub next_retry_time_ms: MilliSecondsSinceUnixEpoch,
}

impl SenderDataRetryDetails {
    /// Create a new RetryDetails with a retry count of zero, and retry time of
    /// now.
    pub(crate) fn retry_soon() -> Self {
        Self { retry_count: 0, next_retry_time_ms: MilliSecondsSinceUnixEpoch::now() }
    }

    #[cfg(test)]
    pub(crate) fn new(retry_count: u8, next_retry_time_ms: u64) -> Self {
        use ruma::UInt;

        Self {
            retry_count,
            next_retry_time_ms: MilliSecondsSinceUnixEpoch(
                UInt::try_from(next_retry_time_ms).unwrap_or(UInt::from(0u8)),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_let;

    use super::{SenderData, SenderDataRetryDetails};

    #[test]
    fn serializing_unknown_device_correctly_preserves_owner_check_failed_if_true() {
        // Given an unknown device SenderData with failed owner check
        let start = SenderData::UnknownDevice {
            retry_details: SenderDataRetryDetails::new(3, 10_000),
            legacy_session: false,
            owner_check_failed: true,
        };

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
        let start = SenderData::UnknownDevice {
            retry_details: SenderDataRetryDetails::new(3, 10_000),
            legacy_session: false,
            owner_check_failed: false,
        };

        // When we write it to JSON
        let json = serde_json::to_string(&start).unwrap();

        // Then the JSON does not mention `owner_check_failed`
        assert!(!json.contains("owner_check_failed"), "JSON contains 'owner_check_failed'!");

        // And for good measure, it round-trips fully
        let end: SenderData = serde_json::from_str(&json).unwrap();
        assert_eq!(start, end);
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

        let _end: SenderData = serde_json::from_str(json).expect("Failed to parse!");
    }
}
