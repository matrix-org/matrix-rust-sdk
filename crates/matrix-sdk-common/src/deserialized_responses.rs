// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::{collections::BTreeMap, fmt};

use ruma::{
    events::{AnySyncTimelineEvent, AnyTimelineEvent},
    push::Action,
    serde::{JsonObject, Raw},
    DeviceKeyAlgorithm, OwnedDeviceId, OwnedEventId, OwnedUserId,
};
use serde::{Deserialize, Serialize};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

use crate::debug::{DebugRawEvent, DebugStructExt};

const AUTHENTICITY_NOT_GUARANTEED: &str =
    "The authenticity of this encrypted message can't be guaranteed on this device.";
const UNVERIFIED_IDENTITY: &str = "Encrypted by an unverified user.";
const PREVIOUSLY_VERIFIED: &str = "Encrypted by a previously-verified user.";
const UNSIGNED_DEVICE: &str = "Encrypted by a device not verified by its owner.";
const UNKNOWN_DEVICE: &str = "Encrypted by an unknown or deleted device.";
pub const SENT_IN_CLEAR: &str = "Not encrypted.";

/// Represents the state of verification for a decrypted message sent by a
/// device.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(from = "OldVerificationStateHelper")]
pub enum VerificationState {
    /// This message is guaranteed to be authentic as it is coming from a device
    /// belonging to a user that we have verified.
    ///
    /// This is the only state where authenticity can be guaranteed.
    Verified,

    /// The message could not be linked to a verified device.
    ///
    /// For more detailed information on why the message is considered
    /// unverified, refer to the VerificationLevel sub-enum.
    Unverified(VerificationLevel),
}

// TODO: Remove this once we're confident that everybody that serialized these
// states uses the new enum.
#[derive(Clone, Debug, Deserialize)]
enum OldVerificationStateHelper {
    Untrusted,
    UnknownDevice,
    #[serde(alias = "Trusted")]
    Verified,
    Unverified(VerificationLevel),
}

impl From<OldVerificationStateHelper> for VerificationState {
    fn from(value: OldVerificationStateHelper) -> Self {
        match value {
            // This mapping isn't strictly correct but we don't know which part in the old
            // `VerificationState` enum was unverified.
            OldVerificationStateHelper::Untrusted => {
                VerificationState::Unverified(VerificationLevel::UnsignedDevice)
            }
            OldVerificationStateHelper::UnknownDevice => {
                Self::Unverified(VerificationLevel::None(DeviceLinkProblem::MissingDevice))
            }
            OldVerificationStateHelper::Verified => Self::Verified,
            OldVerificationStateHelper::Unverified(l) => Self::Unverified(l),
        }
    }
}

impl VerificationState {
    /// Convert the `VerificationState` into a `ShieldState` which can be
    /// directly used to decorate messages in the recommended way.
    ///
    /// This method decorates messages using a strict ruleset, for a more lax
    /// variant of this method take a look at
    /// [`VerificationState::to_shield_state_lax()`].
    pub fn to_shield_state_strict(&self) -> ShieldState {
        match self {
            VerificationState::Verified => ShieldState::None,
            VerificationState::Unverified(level) => match level {
                VerificationLevel::UnverifiedIdentity
                | VerificationLevel::PreviouslyVerified
                | VerificationLevel::UnsignedDevice => ShieldState::Red {
                    code: ShieldStateCode::UnverifiedIdentity,
                    message: UNVERIFIED_IDENTITY,
                },
                VerificationLevel::None(link) => match link {
                    DeviceLinkProblem::MissingDevice => ShieldState::Red {
                        code: ShieldStateCode::UnknownDevice,
                        message: UNKNOWN_DEVICE,
                    },
                    DeviceLinkProblem::InsecureSource => ShieldState::Red {
                        code: ShieldStateCode::AuthenticityNotGuaranteed,
                        message: AUTHENTICITY_NOT_GUARANTEED,
                    },
                },
            },
        }
    }

    /// Convert the `VerificationState` into a `ShieldState` which can be used
    /// to decorate messages in the recommended way.
    ///
    /// This implements a legacy, lax decoration mode.
    ///
    /// For a more strict variant of this method take a look at
    /// [`VerificationState::to_shield_state_strict()`].
    pub fn to_shield_state_lax(&self) -> ShieldState {
        match self {
            VerificationState::Verified => ShieldState::None,
            VerificationState::Unverified(level) => match level {
                VerificationLevel::UnverifiedIdentity => {
                    // If you didn't show interest in verifying that user we don't
                    // nag you with an error message.
                    ShieldState::None
                }
                VerificationLevel::PreviouslyVerified => {
                    // This is a high warning. The sender was previously
                    // verified, but changed their identity.
                    ShieldState::Red {
                        code: ShieldStateCode::PreviouslyVerified,
                        message: PREVIOUSLY_VERIFIED,
                    }
                }
                VerificationLevel::UnsignedDevice => {
                    // This is a high warning. The sender hasn't verified his own device.
                    ShieldState::Red {
                        code: ShieldStateCode::UnsignedDevice,
                        message: UNSIGNED_DEVICE,
                    }
                }
                VerificationLevel::None(link) => match link {
                    DeviceLinkProblem::MissingDevice => {
                        // Have to warn as it could have been a temporary injected device.
                        // Notice that the device might just not be known at this time, so callers
                        // should retry when there is a device change for that user.
                        ShieldState::Red {
                            code: ShieldStateCode::UnknownDevice,
                            message: UNKNOWN_DEVICE,
                        }
                    }
                    DeviceLinkProblem::InsecureSource => {
                        // In legacy mode, we tone down this warning as it is quite common and
                        // mostly noise (due to legacy backup and lack of trusted forwards).
                        ShieldState::Grey {
                            code: ShieldStateCode::AuthenticityNotGuaranteed,
                            message: AUTHENTICITY_NOT_GUARANTEED,
                        }
                    }
                },
            },
        }
    }
}

/// The sub-enum containing detailed information on why a message is considered
/// to be unverified.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum VerificationLevel {
    /// The message was sent by a user identity we have not verified.
    UnverifiedIdentity,

    /// The message was sent by a user identity we have not verified, but the
    /// user was previously verified.
    PreviouslyVerified,

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

/// The sub-enum containing detailed information on why we were not able to link
/// a message back to a device.
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

/// Recommended decorations for decrypted messages, representing the message's
/// authenticity properties.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum ShieldState {
    /// A red shield with a tooltip containing the associated message should be
    /// presented.
    Red {
        /// A machine-readable representation.
        code: ShieldStateCode,
        /// A human readable description.
        message: &'static str,
    },
    /// A grey shield with a tooltip containing the associated message should be
    /// presented.
    Grey {
        /// A machine-readable representation.
        code: ShieldStateCode,
        /// A human readable description.
        message: &'static str,
    },
    /// No shield should be presented.
    None,
}

/// A machine-readable representation of the authenticity for a `ShieldState`.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub enum ShieldStateCode {
    /// Not enough information available to check the authenticity.
    AuthenticityNotGuaranteed,
    /// The sending device isn't yet known by the Client.
    UnknownDevice,
    /// The sending device hasn't been verified by the sender.
    UnsignedDevice,
    /// The sender hasn't been verified by the Client's user.
    UnverifiedIdentity,
    /// An unencrypted event in an encrypted room.
    SentInClear,
    /// The sender was previously verified but changed their identity.
    PreviouslyVerified,
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
    /// untrusted data unless `verification_state` is `Verified` as well.
    pub sender_device: Option<OwnedDeviceId>,
    /// Information about the algorithm that was used to encrypt the event.
    pub algorithm_info: AlgorithmInfo,
    /// The verification state of the device that sent us the event, note this
    /// is the state of the device at the time of decryption. It may change in
    /// the future if a device gets verified or deleted.
    ///
    /// Callers that persist this should mark the state as dirty when a device
    /// change is received down the sync.
    pub verification_state: VerificationState,
}

/// A customized version of a room event coming from a sync that holds optional
/// encryption info.
#[derive(Clone, Deserialize, Serialize)]
pub struct SyncTimelineEvent {
    /// The actual event.
    pub event: Raw<AnySyncTimelineEvent>,
    /// The encryption info about the event. Will be `None` if the event was not
    /// encrypted.
    pub encryption_info: Option<EncryptionInfo>,
    /// The push actions associated with this event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub push_actions: Vec<Action>,
    /// The encryption info about the events bundled in the `unsigned` object.
    ///
    /// Will be `None` if no bundled event was encrypted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsigned_encryption_info: Option<BTreeMap<UnsignedEventLocation, UnsignedDecryptionResult>>,
}

impl SyncTimelineEvent {
    /// Create a new `SyncTimelineEvent` from the given raw event.
    ///
    /// This is a convenience constructor for when you don't need to set
    /// `encryption_info` or `push_action`, for example inside a test.
    pub fn new(event: Raw<AnySyncTimelineEvent>) -> Self {
        Self { event, encryption_info: None, push_actions: vec![], unsigned_encryption_info: None }
    }

    /// Create a new `SyncTimelineEvent` from the given raw event and push
    /// actions.
    ///
    /// This is a convenience constructor for when you don't need to set
    /// `encryption_info`, for example inside a test.
    pub fn new_with_push_actions(
        event: Raw<AnySyncTimelineEvent>,
        push_actions: Vec<Action>,
    ) -> Self {
        Self { event, encryption_info: None, push_actions, unsigned_encryption_info: None }
    }

    /// Get the event id of this `SyncTimelineEvent` if the event has any valid
    /// id.
    pub fn event_id(&self) -> Option<OwnedEventId> {
        self.event.get_field::<OwnedEventId>("event_id").ok().flatten()
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SyncTimelineEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let SyncTimelineEvent { event, encryption_info, push_actions, unsigned_encryption_info } =
            self;
        let mut s = f.debug_struct("SyncTimelineEvent");
        s.field("event", &DebugRawEvent(event));
        s.maybe_field("encryption_info", encryption_info);
        if !push_actions.is_empty() {
            s.field("push_actions", push_actions);
        }
        s.maybe_field("unsigned_encryption_info", unsigned_encryption_info);
        s.finish()
    }
}

impl From<Raw<AnySyncTimelineEvent>> for SyncTimelineEvent {
    fn from(inner: Raw<AnySyncTimelineEvent>) -> Self {
        Self::new(inner)
    }
}

impl From<TimelineEvent> for SyncTimelineEvent {
    fn from(o: TimelineEvent) -> Self {
        // This conversion is unproblematic since a `SyncTimelineEvent` is just a
        // `TimelineEvent` without the `room_id`. By converting the raw value in
        // this way, we simply cause the `room_id` field in the json to be
        // ignored by a subsequent deserialization.
        Self {
            event: o.event.cast(),
            encryption_info: o.encryption_info,
            push_actions: o.push_actions.unwrap_or_default(),
            unsigned_encryption_info: o.unsigned_encryption_info,
        }
    }
}

#[derive(Clone)]
pub struct TimelineEvent {
    /// The actual event.
    pub event: Raw<AnyTimelineEvent>,
    /// The encryption info about the event. Will be `None` if the event was not
    /// encrypted.
    pub encryption_info: Option<EncryptionInfo>,
    /// The push actions associated with this event, if we had sufficient
    /// context to compute them.
    pub push_actions: Option<Vec<Action>>,
    /// The encryption info about the events bundled in the `unsigned` object.
    ///
    /// Will be `None` if no bundled event was encrypted.
    pub unsigned_encryption_info: Option<BTreeMap<UnsignedEventLocation, UnsignedDecryptionResult>>,
}

impl TimelineEvent {
    /// Create a new `TimelineEvent` from the given raw event.
    ///
    /// This is a convenience constructor for when you don't need to set
    /// `encryption_info` or `push_action`, for example inside a test.
    pub fn new(event: Raw<AnyTimelineEvent>) -> Self {
        Self { event, encryption_info: None, push_actions: None, unsigned_encryption_info: None }
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for TimelineEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let TimelineEvent { event, encryption_info, push_actions, unsigned_encryption_info } = self;
        let mut s = f.debug_struct("TimelineEvent");
        s.field("event", &DebugRawEvent(event));
        s.maybe_field("encryption_info", encryption_info);
        if let Some(push_actions) = &push_actions {
            if !push_actions.is_empty() {
                s.field("push_actions", push_actions);
            }
        }
        s.maybe_field("unsigned_encryption_info", unsigned_encryption_info);
        s.finish()
    }
}

/// The location of an event bundled in an `unsigned` object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum UnsignedEventLocation {
    /// An event at the `m.replace` key of the `m.relations` object, that is a
    /// bundled replacement.
    RelationsReplace,
    /// An event at the `latest_event` key of the `m.thread` object of the
    /// `m.relations` object, that is the latest event of a thread.
    RelationsThreadLatestEvent,
}

impl UnsignedEventLocation {
    /// Find the mutable JSON value at this location in the given unsigned
    /// object.
    ///
    /// # Arguments
    ///
    /// * `unsigned` - The `unsigned` property of an event as a JSON object.
    pub fn find_mut<'a>(&self, unsigned: &'a mut JsonObject) -> Option<&'a mut serde_json::Value> {
        let relations = unsigned.get_mut("m.relations")?.as_object_mut()?;

        match self {
            Self::RelationsReplace => relations.get_mut("m.replace"),
            Self::RelationsThreadLatestEvent => {
                relations.get_mut("m.thread")?.as_object_mut()?.get_mut("latest_event")
            }
        }
    }
}

/// The result of the decryption of an event bundled in an `unsigned` object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UnsignedDecryptionResult {
    /// The event was successfully decrypted.
    Decrypted(EncryptionInfo),
    /// The event failed to be decrypted.
    UnableToDecrypt(UnableToDecryptInfo),
}

/// Metadata about an event that could not be decrypted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnableToDecryptInfo {
    /// The ID of the session used to encrypt the message, if it used the
    /// `m.megolm.v1.aes-sha2` algorithm.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use ruma::{
        events::{room::message::RoomMessageEventContent, AnySyncTimelineEvent},
        serde::Raw,
    };
    use serde::Deserialize;
    use serde_json::json;

    use super::{SyncTimelineEvent, TimelineEvent, VerificationState};
    use crate::deserialized_responses::{DeviceLinkProblem, VerificationLevel};

    fn example_event() -> serde_json::Value {
        json!({
            "content": RoomMessageEventContent::text_plain("secret"),
            "type": "m.room.message",
            "event_id": "$xxxxx:example.org",
            "room_id": "!someroom:example.com",
            "origin_server_ts": 2189,
            "sender": "@carl:example.com",
        })
    }

    #[test]
    fn sync_timeline_debug_content() {
        let room_event = SyncTimelineEvent::new(Raw::new(&example_event()).unwrap().cast());
        let debug_s = format!("{room_event:?}");
        assert!(
            !debug_s.contains("secret"),
            "Debug representation contains event content!\n{debug_s}"
        );
    }

    #[test]
    fn room_event_to_sync_room_event() {
        let room_event = TimelineEvent::new(Raw::new(&example_event()).unwrap().cast());
        let converted_room_event: SyncTimelineEvent = room_event.into();

        let converted_event: AnySyncTimelineEvent =
            converted_room_event.event.deserialize().unwrap();

        assert_eq!(converted_event.event_id(), "$xxxxx:example.org");
        assert_eq!(converted_event.sender(), "@carl:example.com");
    }

    #[test]
    fn old_verification_state_to_new_migration() {
        #[derive(Deserialize)]
        struct State {
            state: VerificationState,
        }

        let state = json!({
            "state": "Trusted",
        });
        let deserialized: State =
            serde_json::from_value(state).expect("We can deserialize the old trusted value");
        assert_eq!(deserialized.state, VerificationState::Verified);

        let state = json!({
            "state": "UnknownDevice",
        });

        let deserialized: State =
            serde_json::from_value(state).expect("We can deserialize the old unknown device value");

        assert_eq!(
            deserialized.state,
            VerificationState::Unverified(VerificationLevel::None(
                DeviceLinkProblem::MissingDevice
            ))
        );

        let state = json!({
            "state": "Untrusted",
        });
        let deserialized: State =
            serde_json::from_value(state).expect("We can deserialize the old trusted value");

        assert_eq!(
            deserialized.state,
            VerificationState::Unverified(VerificationLevel::UnsignedDevice)
        );
    }
}
