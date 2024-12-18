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

use std::cmp::Ordering;

use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use serde::{Deserialize, Serialize};
use vodozemac::Ed25519PublicKey;

use crate::types::DeviceKeys;

/// Information about the sender of a megolm session where we know the
/// cross-signing identity of the sender.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct KnownSenderData {
    /// The user ID of the user who established this session.
    pub user_id: OwnedUserId,

    /// The device ID of the device that send the session.
    /// This is an `Option` for backwards compatibility, but we should always
    /// populate it on creation.
    pub device_id: Option<OwnedDeviceId>,

    /// The cross-signing key of the user who established this session.
    pub master_key: Box<Ed25519PublicKey>,
}

/// Information on the device and user that sent the megolm session data to us
///
/// Sessions start off in `UnknownDevice` state, and progress into `DeviceInfo`
/// state when we get the device info. Finally, if we can look up the sender
/// using the device info, the session can be moved into
/// `VerificationViolation`, `SenderUnverified`, or
/// `SenderVerified` state, depending on the verification status of the user.
/// If the user's verification state changes, the state may change accordingly.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(from = "SenderDataReader")]
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
    /// the to-device message that established this session, but we have not yet
    /// verified the cross-signing key, and we had verified a previous
    /// cross-signing key for this user.
    VerificationViolation(KnownSenderData),

    /// We have found proof that this user, with this cross-signing key, sent
    /// the to-device message that established this session, but we have not yet
    /// verified the cross-signing key.
    SenderUnverified(KnownSenderData),

    /// We have found proof that this user, with this cross-signing key, sent
    /// the to-device message that established this session, and we have
    /// verified the cross-signing key.
    SenderVerified(KnownSenderData),
}

impl SenderData {
    /// Create a [`SenderData`] which contains no device info.
    pub fn unknown() -> Self {
        Self::UnknownDevice { legacy_session: false, owner_check_failed: false }
    }

    /// Create a [`SenderData`] which contains device info.
    pub fn device_info(device_keys: DeviceKeys) -> Self {
        Self::DeviceInfo { device_keys, legacy_session: false }
    }

    /// Create a [`SenderData`] with a known but unverified sender, where the
    /// sender was previously verified.
    pub fn sender_verification_violation(
        user_id: &UserId,
        device_id: &DeviceId,
        master_key: Ed25519PublicKey,
    ) -> Self {
        Self::VerificationViolation(KnownSenderData {
            user_id: user_id.to_owned(),
            device_id: Some(device_id.to_owned()),
            master_key: Box::new(master_key),
        })
    }

    /// Create a [`SenderData`] with a known but unverified sender.
    pub fn sender_unverified(
        user_id: &UserId,
        device_id: &DeviceId,
        master_key: Ed25519PublicKey,
    ) -> Self {
        Self::SenderUnverified(KnownSenderData {
            user_id: user_id.to_owned(),
            device_id: Some(device_id.to_owned()),
            master_key: Box::new(master_key),
        })
    }

    /// Create a [`SenderData`] with a verified sender.
    pub fn sender_verified(
        user_id: &UserId,
        device_id: &DeviceId,
        master_key: Ed25519PublicKey,
    ) -> Self {
        Self::SenderVerified(KnownSenderData {
            user_id: user_id.to_owned(),
            device_id: Some(device_id.to_owned()),
            master_key: Box::new(master_key),
        })
    }

    /// Create a [`SenderData`] which has the legacy flag set. Caution: messages
    /// within sessions with this flag will be displayed in some contexts,
    /// even when we are unable to verify the sender.
    ///
    /// The returned struct contains no device info.
    pub fn legacy() -> Self {
        Self::UnknownDevice { legacy_session: true, owner_check_failed: false }
    }

    /// Returns `Greater` if this `SenderData` represents a greater level of
    /// trust than the supplied one, `Equal` if they have the same level, and
    /// `Less` if the supplied one has a greater level of trust.
    ///
    /// So calling this method on a `SenderKnown` or `DeviceInfo` `SenderData`
    /// would return `Greater` if passed an `UnknownDevice` as its
    /// argument, and a `SenderKnown` with `master_key_verified == true`
    /// would return `Greater` if passed a `SenderKnown` with
    /// `master_key_verified == false`.
    pub(crate) fn compare_trust_level(&self, other: &Self) -> Ordering {
        self.trust_number().cmp(&other.trust_number())
    }

    /// Internal function to give a numeric value of how much trust this
    /// `SenderData` represents. Used to make the implementation of
    /// compare_trust_level simpler.
    fn trust_number(&self) -> u8 {
        match self {
            SenderData::UnknownDevice { .. } => 0,
            SenderData::DeviceInfo { .. } => 1,
            SenderData::VerificationViolation(..) => 2,
            SenderData::SenderUnverified(..) => 3,
            SenderData::SenderVerified(..) => 4,
        }
    }

    /// Return our type as a [`SenderDataType`].
    pub fn to_type(&self) -> SenderDataType {
        match self {
            Self::UnknownDevice { .. } => SenderDataType::UnknownDevice,
            Self::DeviceInfo { .. } => SenderDataType::DeviceInfo,
            Self::VerificationViolation { .. } => SenderDataType::VerificationViolation,
            Self::SenderUnverified { .. } => SenderDataType::SenderUnverified,
            Self::SenderVerified { .. } => SenderDataType::SenderVerified,
        }
    }
}

/// Used when deserialising and the sender_data property is missing.
/// If we are deserialising an InboundGroupSession session with missing
/// sender_data, this must be a legacy session (i.e. it was created before we
/// started tracking sender data). We set its legacy flag to true, so we can
/// populate it with trust information if it is available later.
impl Default for SenderData {
    fn default() -> Self {
        Self::legacy()
    }
}

/// Deserialisation type, to handle conversion from older formats
#[derive(Deserialize)]
enum SenderDataReader {
    UnknownDevice {
        legacy_session: bool,
        #[serde(default)]
        owner_check_failed: bool,
    },

    DeviceInfo {
        device_keys: DeviceKeys,
        legacy_session: bool,
    },

    #[serde(alias = "SenderUnverifiedButPreviouslyVerified")]
    VerificationViolation(KnownSenderData),

    SenderUnverified(KnownSenderData),

    SenderVerified(KnownSenderData),

    // If we read this older variant, it gets changed to SenderUnverified or
    // SenderVerified, depending on the master_key_verified flag.
    SenderKnown {
        user_id: OwnedUserId,
        device_id: Option<OwnedDeviceId>,
        master_key: Box<Ed25519PublicKey>,
        master_key_verified: bool,
    },
}

impl From<SenderDataReader> for SenderData {
    fn from(data: SenderDataReader) -> Self {
        match data {
            SenderDataReader::UnknownDevice { legacy_session, owner_check_failed } => {
                Self::UnknownDevice { legacy_session, owner_check_failed }
            }
            SenderDataReader::DeviceInfo { device_keys, legacy_session } => {
                Self::DeviceInfo { device_keys, legacy_session }
            }
            SenderDataReader::VerificationViolation(data) => Self::VerificationViolation(data),
            SenderDataReader::SenderUnverified(data) => Self::SenderUnverified(data),
            SenderDataReader::SenderVerified(data) => Self::SenderVerified(data),
            SenderDataReader::SenderKnown {
                user_id,
                device_id,
                master_key,
                master_key_verified,
            } => {
                let known_sender_data = KnownSenderData { user_id, device_id, master_key };
                if master_key_verified {
                    Self::SenderVerified(known_sender_data)
                } else {
                    Self::SenderUnverified(known_sender_data)
                }
            }
        }
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
    /// The [`SenderData`] is of type `VerificationViolation`.
    VerificationViolation = 3,
    /// The [`SenderData`] is of type `SenderUnverified`.
    SenderUnverified = 4,
    /// The [`SenderData`] is of type `SenderVerified`.
    SenderVerified = 5,
}

#[cfg(test)]
mod tests {
    use std::{cmp::Ordering, collections::BTreeMap};

    use assert_matches2::assert_let;
    use ruma::{device_id, owned_device_id, owned_user_id, user_id};
    use vodozemac::Ed25519PublicKey;

    use super::SenderData;
    use crate::{
        olm::KnownSenderData,
        types::{DeviceKeys, Signatures},
    };

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
        assert_let!(SenderData::SenderVerified { .. } = end);
    }

    #[test]
    fn deserializing_sender_unverified_but_previously_verified_migrates_to_verification_violation()
    {
        let json = r#"
            {
                "SenderUnverifiedButPreviouslyVerified":{
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
        assert_let!(SenderData::VerificationViolation(KnownSenderData { user_id, .. }) = end);
        assert_eq!(user_id, owned_user_id!("@u:s.co"));
    }

    #[test]
    fn deserializing_verification_violation() {
        let json = r#"
            {
                "VerificationViolation":{
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
        assert_let!(SenderData::VerificationViolation(KnownSenderData { user_id, .. }) = end);
        assert_eq!(user_id, owned_user_id!("@u:s.co"));
    }

    #[test]
    fn equal_sessions_have_same_trust_level() {
        let unknown = SenderData::unknown();
        let device_keys = SenderData::device_info(DeviceKeys::new(
            owned_user_id!("@u:s.co"),
            owned_device_id!("DEV"),
            Vec::new(),
            BTreeMap::new(),
            Signatures::new(),
        ));
        let master_key =
            Ed25519PublicKey::from_base64("2/5LWJMow5zhJqakV88SIc7q/1pa8fmkfgAzx72w9G4").unwrap();
        let sender_unverified =
            SenderData::sender_unverified(user_id!("@u:s.co"), device_id!("DEV"), master_key);
        let sender_verified =
            SenderData::sender_verified(user_id!("@u:s.co"), device_id!("DEV"), master_key);

        assert_eq!(unknown.compare_trust_level(&unknown), Ordering::Equal);
        assert_eq!(device_keys.compare_trust_level(&device_keys), Ordering::Equal);
        assert_eq!(sender_unverified.compare_trust_level(&sender_unverified), Ordering::Equal);
        assert_eq!(sender_verified.compare_trust_level(&sender_verified), Ordering::Equal);
    }

    #[test]
    fn more_trust_data_makes_you_more_trusted() {
        let unknown = SenderData::unknown();
        let device_keys = SenderData::device_info(DeviceKeys::new(
            owned_user_id!("@u:s.co"),
            owned_device_id!("DEV"),
            Vec::new(),
            BTreeMap::new(),
            Signatures::new(),
        ));
        let master_key =
            Ed25519PublicKey::from_base64("2/5LWJMow5zhJqakV88SIc7q/1pa8fmkfgAzx72w9G4").unwrap();
        let sender_verification_violation = SenderData::sender_verification_violation(
            user_id!("@u:s.co"),
            device_id!("DEV"),
            master_key,
        );
        let sender_unverified =
            SenderData::sender_unverified(user_id!("@u:s.co"), device_id!("DEV"), master_key);
        let sender_verified =
            SenderData::sender_verified(user_id!("@u:s.co"), device_id!("DEV"), master_key);

        assert_eq!(unknown.compare_trust_level(&device_keys), Ordering::Less);
        assert_eq!(unknown.compare_trust_level(&sender_verification_violation), Ordering::Less);
        assert_eq!(unknown.compare_trust_level(&sender_unverified), Ordering::Less);
        assert_eq!(unknown.compare_trust_level(&sender_verified), Ordering::Less);
        assert_eq!(device_keys.compare_trust_level(&unknown), Ordering::Greater);
        assert_eq!(sender_verification_violation.compare_trust_level(&unknown), Ordering::Greater);
        assert_eq!(sender_unverified.compare_trust_level(&unknown), Ordering::Greater);
        assert_eq!(sender_verified.compare_trust_level(&unknown), Ordering::Greater);

        assert_eq!(device_keys.compare_trust_level(&sender_unverified), Ordering::Less);
        assert_eq!(device_keys.compare_trust_level(&sender_verified), Ordering::Less);
        assert_eq!(
            sender_verification_violation.compare_trust_level(&device_keys),
            Ordering::Greater
        );
        assert_eq!(sender_unverified.compare_trust_level(&device_keys), Ordering::Greater);
        assert_eq!(sender_verified.compare_trust_level(&device_keys), Ordering::Greater);

        assert_eq!(
            sender_verification_violation.compare_trust_level(&sender_verified),
            Ordering::Less
        );
        assert_eq!(
            sender_verification_violation.compare_trust_level(&sender_unverified),
            Ordering::Less
        );
        assert_eq!(sender_unverified.compare_trust_level(&sender_verified), Ordering::Less);
        assert_eq!(sender_verified.compare_trust_level(&sender_unverified), Ordering::Greater);
    }
}
