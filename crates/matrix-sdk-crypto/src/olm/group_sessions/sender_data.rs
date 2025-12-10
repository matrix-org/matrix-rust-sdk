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

use std::{cmp::Ordering, fmt};

use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use serde::{Deserialize, Deserializer, Serialize, de, de::Visitor};
use tracing::error;
use vodozemac::Ed25519PublicKey;

use crate::{
    Device,
    types::{DeviceKeys, serialize_ed25519_key},
};

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
    #[serde(
        serialize_with = "serialize_ed25519_key",
        deserialize_with = "deserialize_sender_msk_base64_or_array"
    )]
    pub master_key: Box<Ed25519PublicKey>,
}

/// In an initial version the master key was serialized as an array of number,
/// it is now exported in base64. This code adds backward compatibility.
pub(crate) fn deserialize_sender_msk_base64_or_array<'de, D>(
    de: D,
) -> Result<Box<Ed25519PublicKey>, D::Error>
where
    D: Deserializer<'de>,
{
    struct KeyVisitor;

    impl<'de> Visitor<'de> for KeyVisitor {
        type Value = Box<Ed25519PublicKey>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "a base64 string or an array of 32 bytes")
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            let decoded = Ed25519PublicKey::from_base64(v)
                .map_err(|_| de::Error::custom("Base64 decoding error"))?;
            Ok(Box::new(decoded))
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut buf = [0u8; Ed25519PublicKey::LENGTH];

            for (i, item) in buf.iter_mut().enumerate() {
                *item = seq.next_element()?.ok_or_else(|| de::Error::invalid_length(i, &self))?;
            }

            let key = Ed25519PublicKey::from_slice(&buf).map_err(|e| de::Error::custom(&e))?;

            Ok(Box::new(key))
        }

        fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if v.len() == Ed25519PublicKey::LENGTH {
                let mut buf = [0u8; Ed25519PublicKey::LENGTH];
                buf.copy_from_slice(v);

                let key = Ed25519PublicKey::from_slice(&buf).map_err(|e| de::Error::custom(&e))?;
                Ok(Box::new(key))
            } else {
                Err(de::Error::invalid_length(v.len(), &self))
            }
        }
    }

    de.deserialize_any(KeyVisitor)
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
    /// Whether we should recalculate the Megolm sender's data, given the
    /// current sender data. We only want to recalculate if it might
    /// increase trust and allow us to decrypt messages that we
    /// otherwise might refuse to decrypt.
    ///
    /// We recalculate for all states except:
    ///
    /// - SenderUnverified: the sender is trusted enough that we will decrypt
    ///   their messages in all cases, or
    /// - SenderVerified: the sender is the most trusted they can be.
    pub fn should_recalculate(&self) -> bool {
        matches!(
            self,
            SenderData::UnknownDevice { .. }
                | SenderData::DeviceInfo { .. }
                | SenderData::VerificationViolation { .. }
        )
    }

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

    /// Create a [`SenderData`] representing the current verification state of
    /// the given device.
    ///
    /// Depending on whether the device is correctly cross-signed or not, and
    /// whether the user has been verified or not, this can return
    /// [`SenderData::DeviceInfo`], [`SenderData::VerificationViolation`],
    /// [`SenderData::SenderUnverified`] or [`SenderData::SenderVerified`]
    pub fn from_device(sender_device: &Device) -> Self {
        // Is the device cross-signed?
        // Does the cross-signing key match that used to sign the device?
        // And is the signature in the device valid?
        let cross_signed = sender_device.is_cross_signed_by_owner();

        if cross_signed {
            SenderData::from_cross_signed_device(sender_device)
        } else {
            // We have device keys, but they are not signed by the sender
            SenderData::device_info(sender_device.as_device_keys().clone())
        }
    }

    fn from_cross_signed_device(sender_device: &Device) -> Self {
        let user_id = sender_device.user_id().to_owned();
        let device_id = Some(sender_device.device_id().to_owned());

        let device_owner = sender_device.device_owner_identity.as_ref();
        let master_key = device_owner.and_then(|i| i.master_key().get_first_key());

        match (device_owner, master_key) {
            (Some(device_owner), Some(master_key)) => {
                // We have user_id and master_key for the user sending the to-device message.
                let master_key = Box::new(master_key);
                let known_sender_data = KnownSenderData { user_id, device_id, master_key };
                if sender_device.is_cross_signing_trusted() {
                    Self::SenderVerified(known_sender_data)
                } else if device_owner.was_previously_verified() {
                    Self::VerificationViolation(known_sender_data)
                } else {
                    Self::SenderUnverified(known_sender_data)
                }
            }

            (_, _) => {
                // Surprisingly, there was no key in the MasterPubkey. We did not expect this:
                // treat it as if the device was not signed by this master key.
                //
                error!("MasterPubkey for user {user_id} does not contain any keys!");
                Self::device_info(sender_device.as_device_keys().clone())
            }
        }
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

    /// Return our best guess of the owner of the associated megolm session.
    ///
    /// For `SenderData::UnknownDevice`, we don't record any information about
    /// the owner of the sender, so returns `None`.
    pub(crate) fn user_id(&self) -> Option<OwnedUserId> {
        match &self {
            SenderData::UnknownDevice { .. } => None,
            SenderData::DeviceInfo { device_keys, .. } => Some(device_keys.user_id.clone()),
            SenderData::VerificationViolation(known_sender_data) => {
                Some(known_sender_data.user_id.clone())
            }
            SenderData::SenderUnverified(known_sender_data) => {
                Some(known_sender_data.user_id.clone())
            }
            SenderData::SenderVerified(known_sender_data) => {
                Some(known_sender_data.user_id.clone())
            }
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
    use std::{cmp::Ordering, collections::BTreeMap, ops::Deref};

    use assert_matches2::assert_let;
    use insta::assert_json_snapshot;
    use matrix_sdk_test::async_test;
    use ruma::{
        DeviceKeyAlgorithm, DeviceKeyId, device_id, owned_device_id, owned_user_id, user_id,
    };
    use serde_json::json;
    use vodozemac::{Curve25519PublicKey, Ed25519PublicKey, base64_decode};

    use super::SenderData;
    use crate::{
        Account,
        machine::test_helpers::{
            create_signed_device_of_unverified_user, create_signed_device_of_verified_user,
            create_unsigned_device,
        },
        olm::{KnownSenderData, PickledInboundGroupSession, PrivateCrossSigningIdentity},
        types::{DeviceKey, DeviceKeys, EventEncryptionAlgorithm, Signatures},
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

    #[test]
    fn snapshot_sender_data() {
        assert_json_snapshot!(SenderData::UnknownDevice {
            legacy_session: false,
            owner_check_failed: true,
        });

        assert_json_snapshot!(SenderData::UnknownDevice {
            legacy_session: true,
            owner_check_failed: false,
        });

        assert_json_snapshot!(SenderData::DeviceInfo {
            device_keys: DeviceKeys::new(
                owned_user_id!("@foo:bar.baz"),
                owned_device_id!("DEV"),
                vec![
                    EventEncryptionAlgorithm::MegolmV1AesSha2,
                    EventEncryptionAlgorithm::OlmV1Curve25519AesSha2
                ],
                BTreeMap::from_iter(vec![(
                    DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, device_id!("ABCDEFGH")),
                    DeviceKey::Curve25519(Curve25519PublicKey::from_bytes([0u8; 32])),
                )]),
                Default::default(),
            ),
            legacy_session: false,
        });

        assert_json_snapshot!(SenderData::VerificationViolation(KnownSenderData {
            user_id: owned_user_id!("@foo:bar.baz"),
            device_id: Some(owned_device_id!("DEV")),
            master_key: Box::new(Ed25519PublicKey::from_slice(&[0u8; 32]).unwrap()),
        }));

        assert_json_snapshot!(SenderData::SenderUnverified(KnownSenderData {
            user_id: owned_user_id!("@foo:bar.baz"),
            device_id: None,
            master_key: Box::new(Ed25519PublicKey::from_slice(&[1u8; 32]).unwrap()),
        }));

        assert_json_snapshot!(SenderData::SenderVerified(KnownSenderData {
            user_id: owned_user_id!("@foo:bar.baz"),
            device_id: None,
            master_key: Box::new(Ed25519PublicKey::from_slice(&[1u8; 32]).unwrap()),
        }));
    }

    #[test]
    fn test_sender_known_data_migration() {
        let old_format = json!(
        {
            "SenderVerified": {
                "user_id": "@foo:bar.baz",
                "device_id": null,
                "master_key": [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]
            }
        });

        let migrated: SenderData = serde_json::from_value(old_format).unwrap();

        assert_let!(SenderData::SenderVerified(KnownSenderData { master_key, .. }) = migrated);

        assert_eq!(
            master_key.to_base64(),
            Ed25519PublicKey::from_slice(&[0u8; 32]).unwrap().to_base64()
        );
    }

    #[test]
    fn test_sender_known_data_migration_with_efficient_bytes_array() {
        // This is an serialized PickledInboundGroupSession as rmp_serde will generate.
        //
        // This export usse a more efficient serialization format for bytes. This was
        // exported when the `KnownSenderData` master_key was serialized as an byte
        // array instead of a base64 encoded string.
        const SERIALIZED_B64: &str = "\
            iaZwaWNrbGWEr2luaXRpYWxfcmF0Y2hldIKlaW5uZXLcAIABYMzfSnBRzMlPKF1uKjYbzLtkzNJ4RcylzN0HzP\
            9DzON1Tm05zO7M2MzFQsy9Acz9zPnMqDvM4syQzNrMzxF5KzbM4sy9zPUbBWfM7m4/zJzM18zDzMESKgfMkE7M\
            yszIHszqWjYyQURbzKTMkx7M58zANsy+AGPM2A8tbcyFYczge8ykzMFdbVxJMMyAzN8azJEXGsy8zPJazMMaP8\
            ziDszmWwfM+My2ajLMr8y+eczTRm9TFadjb3VudGVyAKtzaWduaW5nX2tlecQgefpCr6Duu7QUWzKIeMOFmxv/\
            NjfcsYwZz8IN2ZOhdaS0c2lnbmluZ19rZXlfdmVyaWZpZWTDpmNvbmZpZ4GndmVyc2lvbqJWMapzZW5kZXJfa2\
            V52StoMkIySDg2ajFpYmk2SW13ak9UUkhzbTVMamtyT2kyUGtiSXVUb0w0TWtFq3NpZ25pbmdfa2V5gadlZDI1\
            NTE52StUWHJqNS9UYXpia3Yram1CZDl4UlB4NWNVaFFzNUNnblc1Q1pNRjgvNjZzq3NlbmRlcl9kYXRhgbBTZW\
            5kZXJVbnZlcmlmaWVkg6d1c2VyX2lks0B2YWxvdTM1Om1hdHJpeC5vcmepZGV2aWNlX2lkqkZJQlNaRlJLUE2q\
            bWFzdGVyX2tlecQgkOp9s4ClyQujYD7rRZA8xgE6kvYlqKSNnMrQNmSrcuGncm9vbV9pZL4hRWt5VEtGdkViYl\
            B6SmxhaUhFOm1hdHJpeC5vcmeoaW1wb3J0ZWTCqWJhY2tlZF91cMKyaGlzdG9yeV92aXNpYmlsaXR5wKlhbGdv\
            cml0aG20bS5tZWdvbG0udjEuYWVzLXNoYTI";

        let input = base64_decode(SERIALIZED_B64).unwrap();
        let sender_data: PickledInboundGroupSession = rmp_serde::from_slice(&input)
            .expect("Should be able to deserialize serialized inbound group session");

        assert_let!(
            SenderData::SenderUnverified(KnownSenderData { master_key, .. }) =
                sender_data.sender_data
        );

        assert_eq!(master_key.to_base64(), "kOp9s4ClyQujYD7rRZA8xgE6kvYlqKSNnMrQNmSrcuE");
    }

    #[async_test]
    async fn test_from_device_for_unsigned_device() {
        let bob_account =
            Account::with_device_id(user_id!("@bob:example.com"), device_id!("BOB_DEVICE"));
        let bob_device = create_unsigned_device(bob_account.device_keys());

        let sender_data = SenderData::from_device(&bob_device);

        assert_eq!(
            sender_data,
            SenderData::DeviceInfo {
                device_keys: bob_device.device_keys.deref().clone(),
                legacy_session: false
            }
        );
    }

    #[async_test]
    async fn test_from_device_for_unverified_user() {
        let bob_identity =
            PrivateCrossSigningIdentity::new(user_id!("@bob:example.com").to_owned());
        let bob_account =
            Account::with_device_id(user_id!("@bob:example.com"), device_id!("BOB_DEVICE"));
        let bob_device = create_signed_device_of_unverified_user(
            bob_account.device_keys().clone(),
            &bob_identity,
        )
        .await;

        let sender_data = SenderData::from_device(&bob_device);

        assert_eq!(
            sender_data,
            SenderData::SenderUnverified(KnownSenderData {
                user_id: bob_account.user_id().to_owned(),
                device_id: Some(bob_account.device_id().to_owned()),
                master_key: Box::new(
                    bob_identity.master_public_key().await.unwrap().get_first_key().unwrap()
                ),
            })
        );
    }

    #[async_test]
    async fn test_from_device_for_verified_user() {
        let alice_account =
            Account::with_device_id(user_id!("@alice:example.com"), device_id!("ALICE_DEVICE"));
        let alice_identity = PrivateCrossSigningIdentity::for_account(&alice_account);

        let bob_identity =
            PrivateCrossSigningIdentity::new(user_id!("@bob:example.com").to_owned());
        let bob_account =
            Account::with_device_id(user_id!("@bob:example.com"), device_id!("BOB_DEVICE"));
        let bob_device = create_signed_device_of_verified_user(
            bob_account.device_keys().clone(),
            &bob_identity,
            &alice_identity,
        )
        .await;

        let sender_data = SenderData::from_device(&bob_device);

        assert_eq!(
            sender_data,
            SenderData::SenderVerified(KnownSenderData {
                user_id: bob_account.user_id().to_owned(),
                device_id: Some(bob_account.device_id().to_owned()),
                master_key: Box::new(
                    bob_identity.master_public_key().await.unwrap().get_first_key().unwrap()
                ),
            })
        );
    }

    #[async_test]
    async fn test_from_device_for_verification_violation_user() {
        let bob_identity =
            PrivateCrossSigningIdentity::new(user_id!("@bob:example.com").to_owned());
        let bob_account =
            Account::with_device_id(user_id!("@bob:example.com"), device_id!("BOB_DEVICE"));
        let bob_device =
            create_signed_device_of_unverified_user(bob_account.device_keys(), &bob_identity).await;
        bob_device
            .device_owner_identity
            .as_ref()
            .unwrap()
            .other()
            .unwrap()
            .mark_as_previously_verified();

        let sender_data = SenderData::from_device(&bob_device);

        assert_eq!(
            sender_data,
            SenderData::VerificationViolation(KnownSenderData {
                user_id: bob_account.user_id().to_owned(),
                device_id: Some(bob_account.device_id().to_owned()),
                master_key: Box::new(
                    bob_identity.master_public_key().await.unwrap().get_first_key().unwrap()
                ),
            })
        );
    }
}
