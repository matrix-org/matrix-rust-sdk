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

use matrix_sdk_common::deserialized_responses::{
    UnableToDecryptInfo, UnableToDecryptReason, VerificationLevel,
};
use ruma::{events::AnySyncTimelineEvent, serde::Raw, MilliSecondsSinceUnixEpoch};
use serde::Deserialize;

/// Our best guess at the reason why an event can't be decrypted.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum UtdCause {
    /// We don't have an explanation for why this UTD happened - it is probably
    /// a bug, or a network split between the two homeservers.
    #[default]
    Unknown = 0,

    /// We are missing the keys for this event, and the event was sent when we
    /// were not a member of the room (or invited).
    SentBeforeWeJoined = 1,

    /// The message was sent by a user identity we have not verified, but the
    /// user was previously verified.
    VerificationViolation = 2,

    /// The [`crate::TrustRequirement`] requires that the sending device be
    /// signed by its owner, and it was not.
    UnsignedDevice = 3,

    /// The [`crate::TrustRequirement`] requires that the sending device be
    /// signed by its owner, and we were unable to securely find the device.
    ///
    /// This could be because the device has since been deleted, because we
    /// haven't yet downloaded it from the server, or because the session
    /// data was obtained from an insecure source (imported from a file,
    /// obtained from a legacy (asymmetric) backup, unsafe key forward, etc.)
    UnknownDevice = 4,

    /// We are missing the keys for this event, but it is a "device-historical"
    /// message and no backup is accessible or usable.
    ///
    /// Device-historical means that the message was sent before the current
    /// device existed (but the current user was probably a member of the room
    /// at the time the message was sent). Not to
    /// be confused with pre-join or pre-invite messages (see
    /// [`UtdCause::SentBeforeWeJoined`] for that).
    HistoricalMessage = 5,
}

/// MSC4115 membership info in the unsigned area.
#[derive(Deserialize)]
struct UnsignedWithMembership {
    #[serde(alias = "io.element.msc4115.membership")]
    membership: Membership,
}

/// MSC4115 contents of the membership property
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Membership {
    Leave,
    Invite,
    Join,
}

/// Contextual crypto information used by [`UtdCause::determine`] to properly
/// identify an Unable-To-Decrypt cause in addition to the
/// [`UnableToDecryptInfo`] and raw event info.
#[derive(Debug, Clone, Copy)]
pub struct CryptoContextInfo {
    /// The current device creation timestamp, used as a heuristic to determine
    /// if an event is device-historical or not (sent before the current device
    /// existed).
    pub device_creation_ts: MilliSecondsSinceUnixEpoch,
    /// True if key storage is correctly set up and can be used by the current
    /// client to download and decrypt message keys.
    pub is_backup_configured: bool,
}

impl UtdCause {
    /// Decide the cause of this UTD, based on the evidence we have.
    pub fn determine(
        raw_event: &Raw<AnySyncTimelineEvent>,
        crypto_context_info: CryptoContextInfo,
        unable_to_decrypt_info: &UnableToDecryptInfo,
    ) -> Self {
        // TODO: in future, use more information to give a richer answer. E.g.
        match unable_to_decrypt_info.reason {
            UnableToDecryptReason::MissingMegolmSession
            | UnableToDecryptReason::UnknownMegolmMessageIndex => {
                // Look in the unsigned area for a `membership` field.
                if let Some(unsigned) =
                    raw_event.get_field::<UnsignedWithMembership>("unsigned").ok().flatten()
                {
                    if let Membership::Leave = unsigned.membership {
                        // We were not a member - this is the cause of the UTD
                        return UtdCause::SentBeforeWeJoined;
                    }
                }

                if let Ok(timeline_event) = raw_event.deserialize() {
                    if crypto_context_info.is_backup_configured
                        && timeline_event.origin_server_ts()
                            < crypto_context_info.device_creation_ts
                    {
                        // It's a device-historical message and there is no accessible
                        // backup. The key is missing and it
                        // is expected.
                        return UtdCause::HistoricalMessage;
                    }
                }

                UtdCause::Unknown
            }

            UnableToDecryptReason::SenderIdentityNotTrusted(
                VerificationLevel::VerificationViolation,
            ) => UtdCause::VerificationViolation,

            UnableToDecryptReason::SenderIdentityNotTrusted(VerificationLevel::UnsignedDevice) => {
                UtdCause::UnsignedDevice
            }

            UnableToDecryptReason::SenderIdentityNotTrusted(VerificationLevel::None(_)) => {
                UtdCause::UnknownDevice
            }

            _ => UtdCause::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_common::deserialized_responses::{
        DeviceLinkProblem, UnableToDecryptInfo, UnableToDecryptReason, VerificationLevel,
    };
    use ruma::{events::AnySyncTimelineEvent, serde::Raw, uint, MilliSecondsSinceUnixEpoch};
    use serde_json::{json, value::to_raw_value};

    use crate::types::events::{utd_cause::CryptoContextInfo, UtdCause};

    #[test]
    fn test_a_missing_raw_event_means_we_guess_unknown() {
        // When we don't provide any JSON to check for membership, then we guess the UTD
        // is unknown.
        assert_eq!(
            UtdCause::determine(
                &raw_event(json!({})),
                some_crypto_context_info(),
                &missing_megolm_session()
            ),
            UtdCause::Unknown
        );
    }

    #[test]
    fn test_if_there_is_no_membership_info_we_guess_unknown() {
        // If our JSON contains no membership info, then we guess the UTD is unknown.
        assert_eq!(
            UtdCause::determine(
                &raw_event(json!({})),
                some_crypto_context_info(),
                &missing_megolm_session()
            ),
            UtdCause::Unknown
        );
    }

    #[test]
    fn test_if_membership_info_cant_be_parsed_we_guess_unknown() {
        // If our JSON contains a membership property but not the JSON we expected, then
        // we guess the UTD is unknown.
        assert_eq!(
            UtdCause::determine(
                &raw_event(json!({ "unsigned": { "membership": 3 } })),
                some_crypto_context_info(),
                &missing_megolm_session()
            ),
            UtdCause::Unknown
        );
    }

    #[test]
    fn test_if_membership_is_invite_we_guess_unknown() {
        // If membership=invite then we expected to be sent the keys so the cause of the
        // UTD is unknown.
        assert_eq!(
            UtdCause::determine(
                &raw_event(json!({ "unsigned": { "membership": "invite" } }),),
                some_crypto_context_info(),
                &missing_megolm_session()
            ),
            UtdCause::Unknown
        );
    }

    #[test]
    fn test_if_membership_is_join_we_guess_unknown() {
        // If membership=join then we expected to be sent the keys so the cause of the
        // UTD is unknown.
        assert_eq!(
            UtdCause::determine(
                &raw_event(json!({ "unsigned": { "membership": "join" } })),
                some_crypto_context_info(),
                &missing_megolm_session()
            ),
            UtdCause::Unknown
        );
    }

    #[test]
    fn test_if_membership_is_leave_we_guess_membership() {
        // If membership=leave then we have an explanation for why we can't decrypt,
        // until we have MSC3061.
        assert_eq!(
            UtdCause::determine(
                &raw_event(json!({ "unsigned": { "membership": "leave" } })),
                some_crypto_context_info(),
                &missing_megolm_session()
            ),
            UtdCause::SentBeforeWeJoined
        );
    }

    #[test]
    fn test_if_reason_is_not_missing_key_we_guess_unknown_even_if_membership_is_leave() {
        // If the UnableToDecryptReason is other than MissingMegolmSession or
        // UnknownMegolmMessageIndex, we do not know the reason for the failure
        // even if membership=leave.
        assert_eq!(
            UtdCause::determine(
                &raw_event(json!({ "unsigned": { "membership": "leave" } })),
                some_crypto_context_info(),
                &malformed_encrypted_event()
            ),
            UtdCause::Unknown
        );
    }

    #[test]
    fn test_if_unstable_prefix_membership_is_leave_we_guess_membership() {
        // Before MSC4115 is merged, we support the unstable prefix too.
        assert_eq!(
            UtdCause::determine(
                &raw_event(json!({ "unsigned": { "io.element.msc4115.membership": "leave" } })),
                some_crypto_context_info(),
                &missing_megolm_session()
            ),
            UtdCause::SentBeforeWeJoined
        );
    }

    #[test]
    fn test_verification_violation_is_passed_through() {
        assert_eq!(
            UtdCause::determine(
                &raw_event(json!({})),
                some_crypto_context_info(),
                &verification_violation()
            ),
            UtdCause::VerificationViolation
        );
    }

    #[test]
    fn test_unsigned_device_is_passed_through() {
        assert_eq!(
            UtdCause::determine(
                &raw_event(json!({})),
                some_crypto_context_info(),
                &unsigned_device()
            ),
            UtdCause::UnsignedDevice
        );
    }

    #[test]
    fn test_unknown_device_is_passed_through() {
        assert_eq!(
            UtdCause::determine(
                &raw_event(json!({})),
                some_crypto_context_info(),
                &missing_device()
            ),
            UtdCause::UnknownDevice
        );
    }

    #[test]
    fn test_date_of_device_determines_whether_a_missing_key_utd_is_expected_historical() {
        let message_creation_ts = 10000;
        let utd_event = a_utd_event_with_origin_ts(message_creation_ts);

        // Given the device is older than the event
        let older_than_event_device = CryptoContextInfo {
            device_creation_ts: MilliSecondsSinceUnixEpoch(
                (message_creation_ts - 1000).try_into().unwrap(),
            ),
            is_backup_configured: true,
        };

        // If the key was missing
        // Then we say the cause is unknown - this is not an historical event
        assert_eq!(
            UtdCause::determine(
                &raw_event(json!({})),
                older_than_event_device,
                &missing_megolm_session()
            ),
            UtdCause::Unknown
        );

        // But if the device is newer than the event
        let newer_than_event_device = CryptoContextInfo {
            device_creation_ts: MilliSecondsSinceUnixEpoch(
                (message_creation_ts + 1000).try_into().unwrap(),
            ),
            is_backup_configured: true,
        };

        // If the key was missing
        // Then we say this is expected, because the event is historical
        assert_eq!(
            UtdCause::determine(&utd_event, newer_than_event_device, &missing_megolm_session()),
            UtdCause::HistoricalMessage
        );
    }

    #[test]
    fn test_date_of_device_determines_whether_a_message_index_utd_is_expected_historical() {
        let message_creation_ts = 10000;
        let utd_event = a_utd_event_with_origin_ts(message_creation_ts);

        // Given the device is older than the event
        let older_than_event_device = CryptoContextInfo {
            device_creation_ts: MilliSecondsSinceUnixEpoch(
                (message_creation_ts - 1000).try_into().unwrap(),
            ),
            is_backup_configured: true,
        };

        // If the message index was incorrect
        // Then we say the cause is unknown - this is not an historical event
        assert_eq!(
            UtdCause::determine(
                &raw_event(json!({})),
                older_than_event_device,
                &unknown_megolm_message_index()
            ),
            UtdCause::Unknown
        );

        // But if the device is newer than the event
        let newer_than_event_device = CryptoContextInfo {
            device_creation_ts: MilliSecondsSinceUnixEpoch(
                (message_creation_ts + 1000).try_into().unwrap(),
            ),
            is_backup_configured: true,
        };

        // If the message index was incorrect
        // Then we say this is expected, because the event is historical
        assert_eq!(
            UtdCause::determine(
                &utd_event,
                newer_than_event_device,
                &unknown_megolm_message_index()
            ),
            UtdCause::HistoricalMessage
        );
    }

    #[test]
    fn test_when_event_is_old_and_message_index_is_wrong_this_is_expected_historical() {
        let message_creation_ts = 10000;
        let utd_event = a_utd_event_with_origin_ts(message_creation_ts);

        // Given the device is newer than the event
        let newer_than_event_device = CryptoContextInfo {
            device_creation_ts: MilliSecondsSinceUnixEpoch(
                (message_creation_ts + 1000).try_into().unwrap(),
            ),
            is_backup_configured: true,
        };

        // If the message index was incorrect
        // Then we say this is an expected UTD (because it's historical)
        assert_eq!(
            UtdCause::determine(
                &utd_event,
                newer_than_event_device,
                &unknown_megolm_message_index()
            ),
            UtdCause::HistoricalMessage
        );

        // But if we have some other failure
        // Then we say the UTD is unexpected, and we don't know what type it is
        assert_eq!(
            UtdCause::determine(&utd_event, newer_than_event_device, &malformed_encrypted_event()),
            UtdCause::Unknown
        );
        assert_eq!(
            UtdCause::determine(&utd_event, newer_than_event_device, &megolm_decryption_failure()),
            UtdCause::Unknown
        );
    }

    #[test]
    fn test_when_event_is_old_and_message_index_is_wrong_but_backup_is_disabled_this_is_unexpected()
    {
        let message_creation_ts = 10000;
        let utd_event = a_utd_event_with_origin_ts(message_creation_ts);

        // Given the device is newer than the event
        // But backup is disabled
        let crypto_context_info = CryptoContextInfo {
            device_creation_ts: MilliSecondsSinceUnixEpoch(
                (message_creation_ts + 1000).try_into().unwrap(),
            ),
            is_backup_configured: false,
        };

        // If the message key was missing
        // Then we say this was unexpected (because backup was disabled)
        assert_eq!(
            UtdCause::determine(&utd_event, crypto_context_info, &missing_megolm_session()),
            UtdCause::Unknown
        );

        // And if the message index was incorrect
        // Then we still say this was unexpected (because backup was disabled)
        assert_eq!(
            UtdCause::determine(&utd_event, crypto_context_info, &unknown_megolm_message_index()),
            UtdCause::Unknown
        );
    }

    fn a_utd_event_with_origin_ts(origin_server_ts: i32) -> Raw<AnySyncTimelineEvent> {
        raw_event(json!({
            "type": "m.room.encrypted",
            "event_id": "$0",
            // the values don't matter much but the expected fields should be there.
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2",
                "ciphertext": "FOO",
                "sender_key": "SENDERKEYSENDERKEY",
                "device_id": "ABCDEFGH",
                "session_id": "A0",
            },
            "sender": "@bob:localhost",
            "origin_server_ts": origin_server_ts,
            "unsigned": { "membership": "join" }
        }))
    }

    fn raw_event(value: serde_json::Value) -> Raw<AnySyncTimelineEvent> {
        Raw::from_json(to_raw_value(&value).unwrap())
    }

    fn some_crypto_context_info() -> CryptoContextInfo {
        CryptoContextInfo {
            device_creation_ts: MilliSecondsSinceUnixEpoch(uint!(42)),
            is_backup_configured: false,
        }
    }

    fn missing_megolm_session() -> UnableToDecryptInfo {
        UnableToDecryptInfo {
            session_id: None,
            reason: UnableToDecryptReason::MissingMegolmSession,
        }
    }

    fn malformed_encrypted_event() -> UnableToDecryptInfo {
        UnableToDecryptInfo {
            session_id: None,
            reason: UnableToDecryptReason::MalformedEncryptedEvent,
        }
    }

    fn unknown_megolm_message_index() -> UnableToDecryptInfo {
        UnableToDecryptInfo {
            session_id: None,
            reason: UnableToDecryptReason::UnknownMegolmMessageIndex,
        }
    }

    fn megolm_decryption_failure() -> UnableToDecryptInfo {
        UnableToDecryptInfo {
            session_id: None,
            reason: UnableToDecryptReason::MegolmDecryptionFailure,
        }
    }

    fn verification_violation() -> UnableToDecryptInfo {
        UnableToDecryptInfo {
            session_id: None,
            reason: UnableToDecryptReason::SenderIdentityNotTrusted(
                VerificationLevel::VerificationViolation,
            ),
        }
    }

    fn unsigned_device() -> UnableToDecryptInfo {
        UnableToDecryptInfo {
            session_id: None,
            reason: UnableToDecryptReason::SenderIdentityNotTrusted(
                VerificationLevel::UnsignedDevice,
            ),
        }
    }

    fn missing_device() -> UnableToDecryptInfo {
        UnableToDecryptInfo {
            session_id: None,
            reason: UnableToDecryptReason::SenderIdentityNotTrusted(VerificationLevel::None(
                DeviceLinkProblem::MissingDevice,
            )),
        }
    }
}
