use std::collections::BTreeMap;

use ruma::{
    events::{AnySyncTimelineEvent, AnyTimelineEvent},
    serde::Raw,
    DeviceKeyAlgorithm, OwnedDeviceId, OwnedEventId, OwnedUserId,
};
use serde::{Deserialize, Serialize};

/// Message shield decoration color abstraction.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum ShieldColor {
    /// Trusted
    GREEN,
    /// Important warning
    RED,
    /// Low warning
    GRAY,
    /// No warning
    NONE,
}

/// Directive for clients on how to decorate a decrypted message with this
/// state.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ShieldState {
    /// Recommended indicator color for this message
    pub color: ShieldColor,
    /// To display as a "tooltip" for users
    pub message: Option<String>,
}

const AUTHENTICITY_NOT_GUARANTEED: &str =
    "The authenticity of this encrypted message can't be guaranteed on this device.";
const UNVERIFIED_DEVICE: &str = "Encrypted by an unverified device.";
const UNSIGNED_DEVICE: &str = "Encrypted by a device not verified by its owner.";
const UNKNOWN_DEVICE: &str = "Encrypted by an unknown or deleted device.";

impl VerificationState {
    /// Converts the state to a shield state that can be used by clients
    pub fn to_shield_state_strict(&self) -> ShieldState {
        match self {
            VerificationState::Verified => ShieldState { color: ShieldColor::GREEN, message: None },
            VerificationState::Unverified(level) => ShieldState {
                color: ShieldColor::RED,
                message: match level {
                    VerificationLevel::UnverifiedIdentity | VerificationLevel::UnsignedDevice => {
                        Some(UNVERIFIED_DEVICE.to_owned())
                    }
                    VerificationLevel::None(link) => match link {
                        DeviceLinkProblem::MissingDevice => Some(UNKNOWN_DEVICE.to_owned()),
                        DeviceLinkProblem::InsecureSource => {
                            Some(AUTHENTICITY_NOT_GUARANTEED.to_owned())
                        }
                    },
                },
            },
        }
    }

    /// Legacy decoration mode.
    /// Converts the state to a shield state that can be used by clients
    pub fn to_shield_state_lax(&self) -> ShieldState {
        match self {
            VerificationState::Verified => ShieldState { color: ShieldColor::GREEN, message: None },
            VerificationState::Unverified(level) => match level {
                VerificationLevel::UnverifiedIdentity => {
                    // If you didn't show interest in verifying that user we don't
                    // nag you with an error message
                    // TODO We should detect identity rotation of a previously trusted identity and
                    // then warn see https://github.com/matrix-org/matrix-rust-sdk/issues/1129
                    ShieldState { color: ShieldColor::NONE, message: None }
                }
                VerificationLevel::UnsignedDevice => {
                    // This is a high warning. The sender hasn't verified his own device.
                    ShieldState {
                        color: ShieldColor::RED,
                        message: Some(UNSIGNED_DEVICE.to_owned()),
                    }
                }
                VerificationLevel::None(link) => match link {
                    DeviceLinkProblem::MissingDevice => {
                        // Have to warn as it could have been a temporary injected device.
                        // Notice that the device might just not be known at this time, so callers
                        // should retry when there is a device change for that user.
                        ShieldState {
                            color: ShieldColor::RED,
                            message: Some(UNKNOWN_DEVICE.to_owned()),
                        }
                    }
                    DeviceLinkProblem::InsecureSource => {
                        // In legacy mode we tone down this warning as it is quite common and
                        // mostly noise (due to legacy backup and lack of trusted forwards)
                        ShieldState {
                            color: ShieldColor::GRAY,
                            message: Some(AUTHENTICITY_NOT_GUARANTEED.to_owned()),
                        }
                    }
                },
            },
        }
    }
}

/// The verification state of the device that sent an event to us.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum VerificationState {
    /// This the only state were the authenticity is
    /// guaranteed. It's coming from a device belonging to a
    /// user that we have verified.
    /// Other states give you more details about the level of trust.
    Verified,
    /// The message could not be linked to a verified user identity. The
    /// verification level that was achieved is supplied as a struct member.
    Unverified(VerificationLevel),
}

/// The partial verification level we were able to achieve for a received event
/// or to-device message.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum VerificationLevel {
    /// The message was sent by a user identity we have not verified.
    UnverifiedIdentity,
    /// The message was sent by a device not linked to (signed by) any user
    /// identity.
    UnsignedDevice,
    /// We weren't able to link the message back to any device. This might be
    /// because the message claims to have been sent by a device which we have
    /// not been able to obtain (for example, because the device was since
    /// deleted) or because the key to decrypt the message was obtained from
    /// an insecure source.
    None(DeviceLinkProblem),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum DeviceLinkProblem {
    /// The device is missing, either because it was deleted, or you haven't
    /// yet downoaled it or the server is erroneously omitting it (federation
    /// lag).
    MissingDevice,
    /// The key was obtained from an insecure source: imported from a file,
    /// obtained from a legacy (asymmetric) backup, unsafe key forward, etc.
    InsecureSource,
}

/// The algorithm specific information of a decrypted event.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum AlgorithmInfo {
    /// The info if the event was encrypted using m.megolm.v1.aes-sha2
    MegolmV1AesSha2 {
        /// The curve25519 key of the device that created the megolm decryption
        /// key originally.
        curve25519_key: String,
        /// The signing keys that have created the megolm key that was used to
        /// decrypt this session. This map will usually contain a single ed25519
        /// key.
        sender_claimed_keys: BTreeMap<DeviceKeyAlgorithm, String>,
    },
}

/// Struct containing information on how an event was decrypted.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EncryptionInfo {
    /// The user ID of the event sender, note this is untrusted data unless the
    /// `verification_state` is `Verified` as well.
    pub sender: OwnedUserId,
    /// The device ID of the device that sent us the event, note this is
    ///  untrusted data unless `verification_state` is `Verified` as well.
    pub sender_device: Option<OwnedDeviceId>,
    /// Information about the algorithm that was used to encrypt the event.
    pub algorithm_info: AlgorithmInfo,
    /// The verification state of the device that sent us the event, note this
    /// is the state of the device at the time of decryption. It may change in
    /// the future if a device gets verified or deleted.
    /// Callers that persist this should mark the state as dirty when a device
    /// change is received down the sync.
    pub verification_state: VerificationState,
}

/// A customized version of a room event coming from a sync that holds optional
/// encryption info.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SyncTimelineEvent {
    /// The actual event.
    pub event: Raw<AnySyncTimelineEvent>,
    /// The encryption info about the event. Will be `None` if the event was not
    /// encrypted.
    pub encryption_info: Option<EncryptionInfo>,
}

impl SyncTimelineEvent {
    /// Get the event id of this `SyncTimelineEvent` if the event has any valid
    /// id.
    pub fn event_id(&self) -> Option<OwnedEventId> {
        self.event.get_field::<OwnedEventId>("event_id").ok().flatten()
    }
}

impl From<Raw<AnySyncTimelineEvent>> for SyncTimelineEvent {
    fn from(inner: Raw<AnySyncTimelineEvent>) -> Self {
        Self { encryption_info: None, event: inner }
    }
}

impl From<TimelineEvent> for SyncTimelineEvent {
    fn from(o: TimelineEvent) -> Self {
        // This conversion is unproblematic since a `SyncTimelineEvent` is just a
        // `TimelineEvent` without the `room_id`. By converting the raw value in
        // this way, we simply cause the `room_id` field in the json to be
        // ignored by a subsequent deserialization.
        Self { encryption_info: o.encryption_info, event: o.event.cast() }
    }
}

#[derive(Clone, Debug)]
pub struct TimelineEvent {
    /// The actual event.
    pub event: Raw<AnyTimelineEvent>,
    /// The encryption info about the event. Will be `None` if the event was not
    /// encrypted.
    pub encryption_info: Option<EncryptionInfo>,
}

#[cfg(test)]
mod tests {
    use ruma::{
        events::{room::message::RoomMessageEventContent, AnySyncTimelineEvent},
        serde::Raw,
    };
    use serde_json::json;

    use super::{SyncTimelineEvent, TimelineEvent};

    #[test]
    fn room_event_to_sync_room_event() {
        let event = json!({
            "content": RoomMessageEventContent::text_plain("foobar"),
            "type": "m.room.message",
            "event_id": "$xxxxx:example.org",
            "room_id": "!someroom:example.com",
            "origin_server_ts": 2189,
            "sender": "@carl:example.com",
        });

        let room_event =
            TimelineEvent { event: Raw::new(&event).unwrap().cast(), encryption_info: None };

        let converted_room_event: SyncTimelineEvent = room_event.into();

        let converted_event: AnySyncTimelineEvent =
            converted_room_event.event.deserialize().unwrap();

        assert_eq!(converted_event.event_id(), "$xxxxx:example.org");
        assert_eq!(converted_event.sender(), "@carl:example.com");
    }
}
