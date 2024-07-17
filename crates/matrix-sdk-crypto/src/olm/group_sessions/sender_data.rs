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

use ruma::{MilliSecondsSinceUnixEpoch, OwnedUserId};
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
    /// We have not yet found the (signed) device info for the sending device
    UnknownDevice {
        /// When we will next try again to find device info for this session,
        /// and how many times we have tried
        retry_details: SenderDataRetryDetails,

        /// Was this session created before we started collecting trust
        /// information about sessions? If so, we may choose to display its
        /// messages even though trust info is missing.
        legacy_session: bool,
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

        /// The cross-signing key of the user who established this session.
        master_key: Ed25519PublicKey,

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
        }
    }

    #[cfg(test)]
    pub(crate) fn unknown_retry_at(retry_details: SenderDataRetryDetails) -> Self {
        Self::UnknownDevice { retry_details, legacy_session: false }
    }

    /// Provide the time when we should retry fetching sender data for this
    /// session, or None if there is no need to retry because the sender is
    /// already known.
    pub fn next_retry_time_ms(&self) -> Option<MilliSecondsSinceUnixEpoch> {
        match self {
            SenderData::UnknownDevice { retry_details, .. } => {
                Some(retry_details.next_retry_time_ms)
            }
            SenderData::DeviceInfo { retry_details, .. } => Some(retry_details.next_retry_time_ms),
            SenderData::SenderKnown { .. } => None,
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

    /// Create new details about when to retry, with the supplied count and next
    /// retry time in milliseconds since the epoch.
    pub fn new(retry_count: u8, next_retry_time_ms: u64) -> Self {
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
    use std::collections::BTreeMap;

    use ruma::{owned_device_id, owned_user_id, MilliSecondsSinceUnixEpoch, UInt};
    use vodozemac::Ed25519PublicKey;

    use super::SenderData;
    use crate::{
        olm::SenderDataRetryDetails,
        types::{DeviceKeys, Signatures},
    };

    #[test]
    fn next_retry_time_for_known_sender_is_none() {
        // Given a SenderUnknown
        let sender_data = SenderData::SenderKnown {
            user_id: owned_user_id!("@u:s.co"),
            master_key: Ed25519PublicKey::from_base64(
                "ee3Ek+J2LkkPmjGPGLhMxiKnhiX//xcqaVL4RP6EypE",
            )
            .unwrap(),
            master_key_verified: false,
        };

        // When we ask for the retry time, it is None because there is no need to retry.
        assert!(
            sender_data.next_retry_time_ms().is_none(),
            "next_retry_time_ms for a SenderKnown should be None"
        );
    }

    #[test]
    fn next_retry_time_for_device_info_is_found() {
        // Given a DeviceInfo, with retry details
        let sender_data = SenderData::DeviceInfo {
            device_keys: DeviceKeys::new(
                owned_user_id!("@u:s.co"),
                owned_device_id!("DEV"),
                vec![],
                BTreeMap::new(),
                Signatures::new(),
            ),
            retry_details: SenderDataRetryDetails::new(0, 13_000),
            legacy_session: false,
        };

        // When we ask for retry time, the provided value is returned
        assert_eq!(
            sender_data.next_retry_time_ms(),
            Some(MilliSecondsSinceUnixEpoch(UInt::new_saturating(13_000)))
        );
    }

    #[test]
    fn next_retry_time_for_unknown_device_is_found() {
        // Given an UnknownDevice, with retry details
        let sender_data = SenderData::UnknownDevice {
            retry_details: SenderDataRetryDetails::new(0, 93_000),
            legacy_session: false,
        };

        // When we ask for retry time, the provided value is returned
        assert_eq!(
            sender_data.next_retry_time_ms(),
            Some(MilliSecondsSinceUnixEpoch(UInt::new_saturating(93_000)))
        );
    }
}
