// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use ruma::{DeviceId, UserId};
use serde::{Deserialize, Serialize};

use crate::olm::{KnownSenderData, SenderData};

/// Represents information about the user who forwarded a megolm session under
/// [MSC4268]. This is similar to [`SenderData`], but it is limited to variants
/// where a cross-signing key has been received from the forwarder, as specified
/// in the MSC.
///
/// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ForwarderData {
    /// We have found proof that this user, with this cross-signing key, sent
    /// the to-device message that established this session, but we have not yet
    /// verified the cross-signing key.
    ///
    /// See [SenderData::SenderUnverified].
    SenderUnverified(KnownSenderData),

    /// We have found proof that this user, with this cross-signing key, sent
    /// the to-device message that established this session, and we have
    /// verified the cross-signing key.
    ///
    /// See [SenderData::SenderVerified].
    SenderVerified(KnownSenderData),
}

impl TryFrom<&SenderData> for ForwarderData {
    type Error = ();

    fn try_from(value: &SenderData) -> Result<Self, Self::Error> {
        // The sender's device must be either `SenderData::SenderUnverified` (i.e.,
        // TOFU-trusted) or `SenderData::SenderVerified` (i.e., fully verified
        // via user verification and cross-signing).
        match value {
            SenderData::SenderUnverified(known_sender_data) => {
                Ok(Self::SenderUnverified(known_sender_data.clone()))
            }
            SenderData::SenderVerified(known_sender_data) => {
                Ok(Self::SenderVerified(known_sender_data.clone()))
            }
            _ => Err(()),
        }
    }
}

impl ForwarderData {
    /// Return the user ID of the associated forwarder.
    pub fn user_id(&self) -> &UserId {
        match &self {
            ForwarderData::SenderUnverified(known_sender_data)
            | ForwarderData::SenderVerified(known_sender_data) => {
                known_sender_data.user_id.as_ref()
            }
        }
    }

    /// Return the device ID of the associated forwarder, if available.
    pub fn device_id(&self) -> Option<&DeviceId> {
        match &self {
            ForwarderData::SenderUnverified(known_sender_data)
            | ForwarderData::SenderVerified(known_sender_data) => {
                known_sender_data.device_id.as_deref()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_json_snapshot;
    use ruma::{device_id, owned_user_id, user_id};
    use vodozemac::Ed25519PublicKey;

    use super::ForwarderData;
    use crate::olm::{KnownSenderData, SenderData};

    #[test]
    fn snapshot_forwarder_data() {
        insta::with_settings!({prepend_module_to_snapshot => false}, {
            assert_json_snapshot!(ForwarderData::SenderUnverified(KnownSenderData {
                user_id: owned_user_id!("@foo:bar.baz"),
                device_id: None,
                master_key: Box::new(Ed25519PublicKey::from_slice(&[1u8; 32]).unwrap()),
            }));
            assert_json_snapshot!(ForwarderData::SenderVerified(KnownSenderData {
                user_id: owned_user_id!("@foo:bar.baz"),
                device_id: None,
                master_key: Box::new(Ed25519PublicKey::from_slice(&[1u8; 32]).unwrap()),
            }));
        });
    }

    // A previous version of this implementation used [`SenderData`] instead of
    // [`ForwarderData`]. To ensure we can still treat existing serialised data as
    // `ForwarderData`, we check we can deserialise the old variants.
    #[test]
    fn test_deserialize_old_format() {
        let master_key = Ed25519PublicKey::from_slice(&[1u8; 32]).unwrap();

        let sender_unverified =
            SenderData::sender_unverified(user_id!("@u:s.co"), device_id!("DEV"), master_key);
        let sender_verified =
            SenderData::sender_verified(user_id!("@u:s.co"), device_id!("DEV"), master_key);

        let serialized_unverified = serde_json::to_string(&sender_unverified).unwrap();
        let serialized_verified = serde_json::to_string(&sender_verified).unwrap();

        let deserialized_unverified: ForwarderData =
            serde_json::from_str(&serialized_unverified).unwrap();
        let deserialized_verified: ForwarderData =
            serde_json::from_str(&serialized_verified).unwrap();

        assert!(matches!(deserialized_unverified, ForwarderData::SenderUnverified(_)));
        assert!(matches!(deserialized_verified, ForwarderData::SenderVerified(_)));
    }

    #[test]
    fn test_try_from_senderdata() {
        let master_key = Ed25519PublicKey::from_slice(&[1u8; 32]).unwrap();

        let sender_unverified = SenderData::sender_unverified(
            user_id!("@test:example.org"),
            device_id!("TEST_DEV"),
            master_key,
        );
        let sender_verified = SenderData::sender_verified(
            user_id!("@test:example.org"),
            device_id!("TEST_DEV"),
            master_key,
        );

        let forwarder_unverified = ForwarderData::try_from(&sender_unverified).unwrap();
        let forwarder_verified = ForwarderData::try_from(&sender_verified).unwrap();

        assert!(matches!(forwarder_unverified, ForwarderData::SenderUnverified(_)));
        assert!(matches!(forwarder_verified, ForwarderData::SenderVerified(_)));

        assert!(
            ForwarderData::try_from(&SenderData::unknown()).is_err(),
            "We should not be able to convert a non-trusted SenderData to a ForwarderData"
        );
    }
}
