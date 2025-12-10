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
    UnableToDecryptInfo, UnableToDecryptReason, VerificationLevel, WithheldCode,
};
use ruma::{MilliSecondsSinceUnixEpoch, events::AnySyncTimelineEvent, serde::Raw};
use serde::Deserialize;

/// Our best guess at the reason why an event can't be decrypted.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum UtdCause {
    /// We don't have an explanation for why this UTD happened - it is probably
    /// a bug, or a network split between the two homeservers.
    ///
    /// For example:
    ///
    /// - the keys for this event are missing, but a key storage backup exists
    ///   and is working, so we should be able to find the keys in the backup.
    ///
    /// - the keys for this event are missing, and a key storage backup exists
    ///   on the server, but that backup is not working on this client even
    ///   though this device is verified.
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
    /// message and there is no key storage backup on the server, presumably
    /// because the user has turned it off.
    ///
    /// Device-historical means that the message was sent before the current
    /// device existed (but the current user was probably a member of the room
    /// at the time the message was sent). Not to
    /// be confused with pre-join or pre-invite messages (see
    /// [`UtdCause::SentBeforeWeJoined`] for that).
    ///
    /// Expected message to user: "History is not available on this device".
    HistoricalMessageAndBackupIsDisabled = 5,

    /// The keys for this event are intentionally withheld.
    ///
    /// The sender has refused to share the key because our device does not meet
    /// the sender's security requirements.
    WithheldForUnverifiedOrInsecureDevice = 6,

    /// The keys for this event are missing, likely because the sender was
    /// unable to share them (e.g., failure to establish an Olm 1:1
    /// channel). Alternatively, the sender may have deliberately excluded
    /// this device by cherry-picking and blocking it, in which case, no action
    /// can be taken on our side.
    WithheldBySender = 7,

    /// We are missing the keys for this event, but it is a "device-historical"
    /// message, and even though a key storage backup does exist, we can't use
    /// it because our device is unverified.
    ///
    /// Device-historical means that the message was sent before the current
    /// device existed (but the current user was probably a member of the room
    /// at the time the message was sent). Not to
    /// be confused with pre-join or pre-invite messages (see
    /// [`UtdCause::SentBeforeWeJoined`] for that).
    ///
    /// Expected message to user: "You need to verify this device".
    HistoricalMessageAndDeviceIsUnverified = 8,
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

    /// True if this device is secure because it has been verified by us
    pub this_device_is_verified: bool,

    /// True if key storage exists on the server, even if we are unable to use
    /// it
    pub backup_exists_on_server: bool,

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
        use UnableToDecryptReason::*;
        match &unable_to_decrypt_info.reason {
            MissingMegolmSession { withheld_code: Some(WithheldCode::Unverified) } => {
                UtdCause::WithheldForUnverifiedOrInsecureDevice
            }

            MissingMegolmSession { withheld_code: Some(WithheldCode::Blacklisted) }
            | MissingMegolmSession { withheld_code: Some(WithheldCode::Unauthorised) }
            | MissingMegolmSession { withheld_code: Some(WithheldCode::Unavailable) }
            | MissingMegolmSession { withheld_code: Some(WithheldCode::NoOlm) }
            | MissingMegolmSession { withheld_code: Some(WithheldCode::_Custom(_)) } => {
                UtdCause::WithheldBySender
            }

            MissingMegolmSession { withheld_code: None }
            | MissingMegolmSession { withheld_code: Some(WithheldCode::HistoryNotShared) }
            | UnknownMegolmMessageIndex => {
                // Look in the unsigned area for a `membership` field.
                if let Some(unsigned) =
                    raw_event.get_field::<UnsignedWithMembership>("unsigned").ok().flatten()
                    && let Membership::Leave = unsigned.membership
                {
                    // We were not a member - this is the cause of the UTD
                    return UtdCause::SentBeforeWeJoined;
                }

                if let Ok(timeline_event) = raw_event.deserialize()
                    && timeline_event.origin_server_ts() < crypto_context_info.device_creation_ts
                {
                    // This event was sent before this device existed, so it is "historical"
                    return UtdCause::determine_historical(crypto_context_info);
                }

                UtdCause::Unknown
            }

            SenderIdentityNotTrusted(VerificationLevel::VerificationViolation) => {
                UtdCause::VerificationViolation
            }

            SenderIdentityNotTrusted(VerificationLevel::UnsignedDevice) => UtdCause::UnsignedDevice,

            SenderIdentityNotTrusted(VerificationLevel::None(_)) => UtdCause::UnknownDevice,

            _ => UtdCause::Unknown,
        }
    }

    /**
     * Below is the flow chart we follow for deciding whether historical
     * UTDs are expected. This function starts at position `B`.
     *
     * ```text
     * A: Is the message newer than the device?
     *   No -> B
     *   Yes - Normal UTD error
     *
     * B: Is there a backup on the server?
     *   No -> History is not available on this device
     *   Yes -> C
     *
     * C: Is backup working on this device?
     *   No -> D
     *   Yes -> Normal UTD error
     *
     * D: Is this device verified?
     *   No -> You need to verify this device
     *   Yes -> Normal UTD error
     * ```
     */
    fn determine_historical(crypto_context_info: CryptoContextInfo) -> UtdCause {
        let backup_disabled = !crypto_context_info.backup_exists_on_server;
        let backup_failing = !crypto_context_info.is_backup_configured;
        let unverified = !crypto_context_info.this_device_is_verified;

        if backup_disabled {
            UtdCause::HistoricalMessageAndBackupIsDisabled
        } else if backup_failing && unverified {
            UtdCause::HistoricalMessageAndDeviceIsUnverified
        } else {
            // We didn't get the key from key storage backup, but we think we should have,
            // because either:
            //
            // * backup is working (so why didn't we get it?), or
            // * backup is not working for an unknown reason (because the device is
            //   verified, and that is the only reason we check).
            //
            // In either case, we shrug and give an `Unknown` cause.
            UtdCause::Unknown
        }
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_common::deserialized_responses::{
        DeviceLinkProblem, UnableToDecryptInfo, UnableToDecryptReason, VerificationLevel,
        WithheldCode,
    };
    use ruma::{MilliSecondsSinceUnixEpoch, events::AnySyncTimelineEvent, serde::Raw};
    use serde_json::{json, value::to_raw_value};

    use crate::types::events::{UtdCause, utd_cause::CryptoContextInfo};

    const EVENT_TIME: usize = 5555;
    const BEFORE_EVENT_TIME: usize = 1111;
    const AFTER_EVENT_TIME: usize = 9999;

    #[test]
    fn test_if_there_is_no_membership_info_we_guess_unknown() {
        // If our JSON contains no membership info, then we guess the UTD is unknown.
        assert_eq!(
            UtdCause::determine(&raw_event(json!({})), device_old(), &missing_megolm_session()),
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
                device_old(),
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
                device_old(),
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
                device_old(),
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
                device_old(),
                &missing_megolm_session()
            ),
            UtdCause::SentBeforeWeJoined
        );
    }

    #[test]
    fn test_if_membership_is_leave_and_session_not_shared_we_guess_membership() {
        // Even under MSC4268 (history sharing), the session may have been withheld. We
        // use the same UTD cause.
        assert_eq!(
            UtdCause::determine(
                &raw_event(json!({ "unsigned": { "membership": "leave" } })),
                device_old(),
                &UnableToDecryptInfo {
                    session_id: None,
                    reason: UnableToDecryptReason::MissingMegolmSession {
                        withheld_code: Some(WithheldCode::HistoryNotShared)
                    },
                }
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
                device_old(),
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
                device_old(),
                &missing_megolm_session()
            ),
            UtdCause::SentBeforeWeJoined
        );
    }

    #[test]
    fn test_verification_violation_is_passed_through() {
        assert_eq!(
            UtdCause::determine(&raw_event(json!({})), device_old(), &verification_violation()),
            UtdCause::VerificationViolation
        );
    }

    #[test]
    fn test_unsigned_device_is_passed_through() {
        assert_eq!(
            UtdCause::determine(&raw_event(json!({})), device_old(), &unsigned_device()),
            UtdCause::UnsignedDevice
        );
    }

    #[test]
    fn test_unknown_device_is_passed_through() {
        assert_eq!(
            UtdCause::determine(&raw_event(json!({})), device_old(), &missing_device()),
            UtdCause::UnknownDevice
        );
    }

    #[test]
    fn test_old_devices_dont_cause_historical_utds() {
        // Message key is missing.
        let info = missing_megolm_session();

        // The device is old.
        let context = device_old();

        // So we have no explanation for this UTD.
        assert_eq!(UtdCause::determine(&utd_event(), context, &info), UtdCause::Unknown);

        // Same for unknown megolm message index
        let info = unknown_megolm_message_index();
        assert_eq!(UtdCause::determine(&utd_event(), context, &info), UtdCause::Unknown);
    }

    #[test]
    fn test_if_backup_is_disabled_historical_utd_is_expected() {
        // Message key is missing.
        let info = missing_megolm_session();

        // The device is new.
        let mut context = device_new();

        // There is no key storage backup on the server.
        context.backup_exists_on_server = false;

        // So this UTD is expected, and the solution (for future messages!) is to turn
        // on key storage backups.
        assert_eq!(
            UtdCause::determine(&utd_event(), context, &info),
            UtdCause::HistoricalMessageAndBackupIsDisabled
        );

        // Same for unknown megolm message index
        let info = unknown_megolm_message_index();
        assert_eq!(
            UtdCause::determine(&utd_event(), context, &info),
            UtdCause::HistoricalMessageAndBackupIsDisabled
        );
    }

    #[test]
    fn test_malformed_events_are_never_expected_utds() {
        // The event was malformed.
        let info = malformed_encrypted_event();

        // The device is new.
        let mut context = device_new();

        // There is no key storage backup on the server.
        context.backup_exists_on_server = false;

        // So this could be expected historical like the previous test, but because the
        // encrypted event is malformed, that takes precedence, and it's unexpected.
        assert_eq!(UtdCause::determine(&utd_event(), context, &info), UtdCause::Unknown);

        // Same for decryption failures
        let info = megolm_decryption_failure();
        assert_eq!(UtdCause::determine(&utd_event(), context, &info), UtdCause::Unknown);
    }

    #[test]
    fn test_new_devices_with_nonworking_backups_because_unverified_cause_expected_utds() {
        // Message key is missing.
        let info = missing_megolm_session();

        // The device is new.
        let mut context = device_new();

        // There is a key storage backup on the server.
        context.backup_exists_on_server = true;

        // The key storage backup is not working,
        context.is_backup_configured = false;

        // probably because...
        // Our device is not verified.
        context.this_device_is_verified = false;

        // So this UTD is expected, and the solution is (hopefully) to verify.
        assert_eq!(
            UtdCause::determine(&utd_event(), context, &info),
            UtdCause::HistoricalMessageAndDeviceIsUnverified
        );

        // Same for unknown megolm message index
        let info = unknown_megolm_message_index();
        assert_eq!(
            UtdCause::determine(&utd_event(), context, &info),
            UtdCause::HistoricalMessageAndDeviceIsUnverified
        );
    }

    #[test]
    fn test_if_backup_is_working_then_historical_utd_is_unexpected() {
        // Message key is missing.
        let info = missing_megolm_session();

        // The device is new.
        let mut context = device_new();

        // There is a key storage backup on the server.
        context.backup_exists_on_server = true;

        // The key storage backup is working.
        context.is_backup_configured = true;

        // So this UTD is unexpected since we should be able to fetch the key from
        // storage.
        assert_eq!(UtdCause::determine(&utd_event(), context, &info), UtdCause::Unknown);

        // Same for unknown megolm message index
        let info = unknown_megolm_message_index();
        assert_eq!(UtdCause::determine(&utd_event(), context, &info), UtdCause::Unknown);
    }

    #[test]
    fn test_if_backup_is_not_working_even_though_verified_then_historical_utd_is_unexpected() {
        // Message key is missing.
        let info = missing_megolm_session();

        // The device is new.
        let mut context = device_new();

        // There is a key storage backup on the server.
        context.backup_exists_on_server = true;

        // The key storage backup is working.
        context.is_backup_configured = false;

        // even though...
        // Our device is verified.
        context.this_device_is_verified = true;

        // So this UTD is unexpected since we can't explain why our backup is not
        // working.
        //
        // TODO: it might be nice to tell the user that our backup is not working!
        // Currently we don't distinguish between Unknown cases, since we want
        // to make sure they are all reported as unexpected UTDs.
        assert_eq!(UtdCause::determine(&utd_event(), context, &info), UtdCause::Unknown);

        // Same for unknown megolm message index
        let info = unknown_megolm_message_index();
        assert_eq!(UtdCause::determine(&utd_event(), context, &info), UtdCause::Unknown);
    }

    fn utd_event() -> Raw<AnySyncTimelineEvent> {
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
            "origin_server_ts": EVENT_TIME,
            "unsigned": { "membership": "join" }
        }))
    }

    fn raw_event(value: serde_json::Value) -> Raw<AnySyncTimelineEvent> {
        Raw::from_json(to_raw_value(&value).unwrap())
    }

    fn device_old() -> CryptoContextInfo {
        CryptoContextInfo {
            device_creation_ts: MilliSecondsSinceUnixEpoch((BEFORE_EVENT_TIME).try_into().unwrap()),
            this_device_is_verified: false,
            is_backup_configured: false,
            backup_exists_on_server: false,
        }
    }

    fn device_new() -> CryptoContextInfo {
        CryptoContextInfo {
            device_creation_ts: MilliSecondsSinceUnixEpoch((AFTER_EVENT_TIME).try_into().unwrap()),
            this_device_is_verified: false,
            is_backup_configured: false,
            backup_exists_on_server: false,
        }
    }

    fn missing_megolm_session() -> UnableToDecryptInfo {
        UnableToDecryptInfo {
            session_id: None,
            reason: UnableToDecryptReason::MissingMegolmSession { withheld_code: None },
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
