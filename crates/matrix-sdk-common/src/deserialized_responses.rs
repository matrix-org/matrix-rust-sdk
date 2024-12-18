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
    events::{AnyMessageLikeEvent, AnySyncTimelineEvent, AnyTimelineEvent},
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
const VERIFICATION_VIOLATION: &str =
    "Encrypted by a previously-verified user who is no longer verified.";
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
                | VerificationLevel::VerificationViolation
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
                VerificationLevel::VerificationViolation => {
                    // This is a high warning. The sender was previously
                    // verified, but changed their identity.
                    ShieldState::Red {
                        code: ShieldStateCode::VerificationViolation,
                        message: VERIFICATION_VIOLATION,
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
    #[serde(alias = "PreviouslyVerified")]
    VerificationViolation,

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

impl fmt::Display for VerificationLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        let display = match self {
            VerificationLevel::UnverifiedIdentity => "The sender's identity was not verified",
            VerificationLevel::VerificationViolation => {
                "The sender's identity was previously verified but has changed"
            }
            VerificationLevel::UnsignedDevice => {
                "The sending device was not signed by the user's identity"
            }
            VerificationLevel::None(..) => "The sending device is not known",
        };
        write!(f, "{}", display)
    }
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
    VerificationViolation,
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

/// Represents a matrix room event that has been returned from `/sync`,
/// after initial processing.
///
/// Previously, this differed from [`TimelineEvent`] by wrapping an
/// [`AnySyncTimelineEvent`] instead of an [`AnyTimelineEvent`], but nowadays
/// they are essentially identical, and one of them should probably be removed.
#[derive(Clone, Debug, Serialize)]
pub struct SyncTimelineEvent {
    /// The event itself, together with any information on decryption.
    pub kind: TimelineEventKind,

    /// The push actions associated with this event.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub push_actions: Vec<Action>,
}

impl SyncTimelineEvent {
    /// Create a new `SyncTimelineEvent` from the given raw event.
    ///
    /// This is a convenience constructor for a plaintext event when you don't
    /// need to set `push_action`, for example inside a test.
    pub fn new(event: Raw<AnySyncTimelineEvent>) -> Self {
        Self { kind: TimelineEventKind::PlainText { event }, push_actions: vec![] }
    }

    /// Create a new `SyncTimelineEvent` from the given raw event and push
    /// actions.
    ///
    /// This is a convenience constructor for a plaintext event, for example
    /// inside a test.
    pub fn new_with_push_actions(
        event: Raw<AnySyncTimelineEvent>,
        push_actions: Vec<Action>,
    ) -> Self {
        Self { kind: TimelineEventKind::PlainText { event }, push_actions }
    }

    /// Create a new `SyncTimelineEvent` to represent the given decryption
    /// failure.
    pub fn new_utd_event(event: Raw<AnySyncTimelineEvent>, utd_info: UnableToDecryptInfo) -> Self {
        Self { kind: TimelineEventKind::UnableToDecrypt { event, utd_info }, push_actions: vec![] }
    }

    /// Get the event id of this `SyncTimelineEvent` if the event has any valid
    /// id.
    pub fn event_id(&self) -> Option<OwnedEventId> {
        self.kind.event_id()
    }

    /// Returns a reference to the (potentially decrypted) Matrix event inside
    /// this `TimelineEvent`.
    pub fn raw(&self) -> &Raw<AnySyncTimelineEvent> {
        self.kind.raw()
    }

    /// If the event was a decrypted event that was successfully decrypted, get
    /// its encryption info. Otherwise, `None`.
    pub fn encryption_info(&self) -> Option<&EncryptionInfo> {
        self.kind.encryption_info()
    }

    /// Takes ownership of this `TimelineEvent`, returning the (potentially
    /// decrypted) Matrix event within.
    pub fn into_raw(self) -> Raw<AnySyncTimelineEvent> {
        self.kind.into_raw()
    }
}

impl From<TimelineEvent> for SyncTimelineEvent {
    fn from(o: TimelineEvent) -> Self {
        Self { kind: o.kind, push_actions: o.push_actions.unwrap_or_default() }
    }
}

impl From<DecryptedRoomEvent> for SyncTimelineEvent {
    fn from(decrypted: DecryptedRoomEvent) -> Self {
        let timeline_event: TimelineEvent = decrypted.into();
        timeline_event.into()
    }
}

impl<'de> Deserialize<'de> for SyncTimelineEvent {
    /// Custom deserializer for [`SyncTimelineEvent`], to support older formats.
    ///
    /// Ideally we might use an untagged enum and then convert from that;
    /// however, that doesn't work due to a [serde bug](https://github.com/serde-rs/json/issues/497).
    ///
    /// Instead, we first deserialize into an unstructured JSON map, and then
    /// inspect the json to figure out which format we have.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde_json::{Map, Value};

        // First, deserialize to an unstructured JSON map
        let value = Map::<String, Value>::deserialize(deserializer)?;

        // If we have a top-level `event`, it's V0
        if value.contains_key("event") {
            let v0: SyncTimelineEventDeserializationHelperV0 =
                serde_json::from_value(Value::Object(value)).map_err(|e| {
                    serde::de::Error::custom(format!(
                        "Unable to deserialize V0-format SyncTimelineEvent: {}",
                        e
                    ))
                })?;
            Ok(v0.into())
        }
        // Otherwise, it's V1
        else {
            let v1: SyncTimelineEventDeserializationHelperV1 =
                serde_json::from_value(Value::Object(value)).map_err(|e| {
                    serde::de::Error::custom(format!(
                        "Unable to deserialize V1-format SyncTimelineEvent: {}",
                        e
                    ))
                })?;
            Ok(v1.into())
        }
    }
}

/// Represents a matrix room event that has been returned from a Matrix
/// client-server API endpoint such as `/messages`, after initial processing.
///
/// The "initial processing" includes an attempt to decrypt encrypted events, so
/// the main thing this adds over [`AnyTimelineEvent`] is information on
/// encryption.
///
/// Previously, this differed from [`SyncTimelineEvent`] by wrapping an
/// [`AnyTimelineEvent`] instead of an [`AnySyncTimelineEvent`], but nowadays
/// they are essentially identical, and one of them should probably be removed.
#[derive(Clone, Debug)]
pub struct TimelineEvent {
    /// The event itself, together with any information on decryption.
    pub kind: TimelineEventKind,

    /// The push actions associated with this event, if we had sufficient
    /// context to compute them.
    pub push_actions: Option<Vec<Action>>,
}

impl TimelineEvent {
    /// Create a new `TimelineEvent` from the given raw event.
    ///
    /// This is a convenience constructor for a plaintext event when you don't
    /// need to set `push_action`, for example inside a test.
    pub fn new(event: Raw<AnyTimelineEvent>) -> Self {
        Self {
            // This conversion is unproblematic since a `SyncTimelineEvent` is just a
            // `TimelineEvent` without the `room_id`. By converting the raw value in
            // this way, we simply cause the `room_id` field in the json to be
            // ignored by a subsequent deserialization.
            kind: TimelineEventKind::PlainText { event: event.cast() },
            push_actions: None,
        }
    }

    /// Create a new `TimelineEvent` to represent the given decryption failure.
    pub fn new_utd_event(event: Raw<AnySyncTimelineEvent>, utd_info: UnableToDecryptInfo) -> Self {
        Self { kind: TimelineEventKind::UnableToDecrypt { event, utd_info }, push_actions: None }
    }

    /// Returns a reference to the (potentially decrypted) Matrix event inside
    /// this `TimelineEvent`.
    pub fn raw(&self) -> &Raw<AnySyncTimelineEvent> {
        self.kind.raw()
    }

    /// If the event was a decrypted event that was successfully decrypted, get
    /// its encryption info. Otherwise, `None`.
    pub fn encryption_info(&self) -> Option<&EncryptionInfo> {
        self.kind.encryption_info()
    }

    /// Takes ownership of this `TimelineEvent`, returning the (potentially
    /// decrypted) Matrix event within.
    pub fn into_raw(self) -> Raw<AnySyncTimelineEvent> {
        self.kind.into_raw()
    }
}

impl From<DecryptedRoomEvent> for TimelineEvent {
    fn from(decrypted: DecryptedRoomEvent) -> Self {
        Self { kind: TimelineEventKind::Decrypted(decrypted), push_actions: None }
    }
}

/// The event within a [`TimelineEvent`] or [`SyncTimelineEvent`], together with
/// encryption data.
#[derive(Clone, Serialize, Deserialize)]
pub enum TimelineEventKind {
    /// A successfully-decrypted encrypted event.
    Decrypted(DecryptedRoomEvent),

    /// An encrypted event which could not be decrypted.
    UnableToDecrypt {
        /// The `m.room.encrypted` event. Depending on the source of the event,
        /// it could actually be an [`AnyTimelineEvent`] (i.e., it may
        /// have a `room_id` property).
        event: Raw<AnySyncTimelineEvent>,

        /// Information on the reason we failed to decrypt
        utd_info: UnableToDecryptInfo,
    },

    /// An unencrypted event.
    PlainText {
        /// The actual event. Depending on the source of the event, it could
        /// actually be a [`AnyTimelineEvent`] (which differs from
        /// [`AnySyncTimelineEvent`] by the addition of a `room_id` property).
        event: Raw<AnySyncTimelineEvent>,
    },
}

impl TimelineEventKind {
    /// Returns a reference to the (potentially decrypted) Matrix event inside
    /// this `TimelineEvent`.
    pub fn raw(&self) -> &Raw<AnySyncTimelineEvent> {
        match self {
            // It is safe to cast from an `AnyMessageLikeEvent` (i.e. JSON which does
            // *not* contain a `state_key` and *does* contain a `room_id`) into an
            // `AnySyncTimelineEvent` (i.e. JSON which *may* contain a `state_key` and is *not*
            // expected to contain a `room_id`). It just means that the `room_id` will be ignored
            // in a future deserialization.
            TimelineEventKind::Decrypted(d) => d.event.cast_ref(),
            TimelineEventKind::UnableToDecrypt { event, .. } => event.cast_ref(),
            TimelineEventKind::PlainText { event } => event,
        }
    }

    /// Get the event id of this `TimelineEventKind` if the event has any valid
    /// id.
    pub fn event_id(&self) -> Option<OwnedEventId> {
        self.raw().get_field::<OwnedEventId>("event_id").ok().flatten()
    }

    /// If the event was a decrypted event that was successfully decrypted, get
    /// its encryption info. Otherwise, `None`.
    pub fn encryption_info(&self) -> Option<&EncryptionInfo> {
        match self {
            TimelineEventKind::Decrypted(d) => Some(&d.encryption_info),
            TimelineEventKind::UnableToDecrypt { .. } => None,
            TimelineEventKind::PlainText { .. } => None,
        }
    }

    /// Takes ownership of this `TimelineEvent`, returning the (potentially
    /// decrypted) Matrix event within.
    pub fn into_raw(self) -> Raw<AnySyncTimelineEvent> {
        match self {
            // It is safe to cast from an `AnyMessageLikeEvent` (i.e. JSON which does
            // *not* contain a `state_key` and *does* contain a `room_id`) into an
            // `AnySyncTimelineEvent` (i.e. JSON which *may* contain a `state_key` and is *not*
            // expected to contain a `room_id`). It just means that the `room_id` will be ignored
            // in a future deserialization.
            TimelineEventKind::Decrypted(d) => d.event.cast(),
            TimelineEventKind::UnableToDecrypt { event, .. } => event.cast(),
            TimelineEventKind::PlainText { event } => event,
        }
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for TimelineEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self {
            Self::PlainText { event } => f
                .debug_struct("TimelineEventDecryptionResult::PlainText")
                .field("event", &DebugRawEvent(event))
                .finish(),

            Self::UnableToDecrypt { event, utd_info } => f
                .debug_struct("TimelineEventDecryptionResult::UnableToDecrypt")
                .field("event", &DebugRawEvent(event))
                .field("utd_info", &utd_info)
                .finish(),

            Self::Decrypted(decrypted) => {
                f.debug_tuple("TimelineEventDecryptionResult::Decrypted").field(decrypted).finish()
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
/// A successfully-decrypted encrypted event.
pub struct DecryptedRoomEvent {
    /// The decrypted event.
    pub event: Raw<AnyMessageLikeEvent>,

    /// The encryption info about the event.
    pub encryption_info: EncryptionInfo,

    /// The encryption info about the events bundled in the `unsigned`
    /// object.
    ///
    /// Will be `None` if no bundled event was encrypted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsigned_encryption_info: Option<BTreeMap<UnsignedEventLocation, UnsignedDecryptionResult>>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for DecryptedRoomEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let DecryptedRoomEvent { event, encryption_info, unsigned_encryption_info } = self;

        f.debug_struct("DecryptedRoomEvent")
            .field("event", &DebugRawEvent(event))
            .field("encryption_info", encryption_info)
            .maybe_field("unsigned_encryption_info", unsigned_encryption_info)
            .finish()
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

    /// Reason code for the decryption failure
    #[serde(default = "unknown_utd_reason")]
    pub reason: UnableToDecryptReason,
}

fn unknown_utd_reason() -> UnableToDecryptReason {
    UnableToDecryptReason::Unknown
}

/// Reason code for a decryption failure
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum UnableToDecryptReason {
    /// The reason for the decryption failure is unknown. This is only intended
    /// for use when deserializing old UnableToDecryptInfo instances.
    #[doc(hidden)]
    Unknown,

    /// The `m.room.encrypted` event that should have been decrypted is
    /// malformed in some way (e.g. unsupported algorithm, missing fields,
    /// unknown megolm message type).
    MalformedEncryptedEvent,

    /// Decryption failed because we're missing the megolm session that was used
    /// to encrypt the event.
    ///
    /// TODO: support withheld codes?
    MissingMegolmSession,

    /// Decryption failed because, while we have the megolm session that was
    /// used to encrypt the message, it is ratcheted too far forward.
    UnknownMegolmMessageIndex,

    /// We found the Megolm session, but were unable to decrypt the event using
    /// that session for some reason (e.g. incorrect MAC).
    ///
    /// This represents all `vodozemac::megolm::DecryptionError`s, except
    /// `UnknownMessageIndex`, which is represented as
    /// `UnknownMegolmMessageIndex`.
    MegolmDecryptionFailure,

    /// The event could not be deserialized after decryption.
    PayloadDeserializationFailure,

    /// Decryption failed because of a mismatch between the identity keys of the
    /// device we received the room key from and the identity keys recorded in
    /// the plaintext of the room key to-device message.
    MismatchedIdentityKeys,

    /// An encrypted message wasn't decrypted, because the sender's
    /// cross-signing identity did not satisfy the requested
    /// `TrustRequirement`.
    SenderIdentityNotTrusted(VerificationLevel),
}

impl UnableToDecryptReason {
    /// Returns true if this UTD is due to a missing room key (and hence might
    /// resolve itself if we wait a bit.)
    pub fn is_missing_room_key(&self) -> bool {
        matches!(self, Self::MissingMegolmSession | Self::UnknownMegolmMessageIndex)
    }
}

/// Deserialization helper for [`SyncTimelineEvent`], for the modern format.
///
/// This has the exact same fields as [`SyncTimelineEvent`] itself, but has a
/// regular `Deserialize` implementation.
#[derive(Debug, Deserialize)]
struct SyncTimelineEventDeserializationHelperV1 {
    /// The event itself, together with any information on decryption.
    kind: TimelineEventKind,

    /// The push actions associated with this event.
    #[serde(default)]
    push_actions: Vec<Action>,
}

impl From<SyncTimelineEventDeserializationHelperV1> for SyncTimelineEvent {
    fn from(value: SyncTimelineEventDeserializationHelperV1) -> Self {
        let SyncTimelineEventDeserializationHelperV1 { kind, push_actions } = value;
        SyncTimelineEvent { kind, push_actions }
    }
}

/// Deserialization helper for [`SyncTimelineEvent`], for an older format.
#[derive(Deserialize)]
struct SyncTimelineEventDeserializationHelperV0 {
    /// The actual event.
    event: Raw<AnySyncTimelineEvent>,

    /// The encryption info about the event. Will be `None` if the event
    /// was not encrypted.
    encryption_info: Option<EncryptionInfo>,

    /// The push actions associated with this event.
    #[serde(default)]
    push_actions: Vec<Action>,

    /// The encryption info about the events bundled in the `unsigned`
    /// object.
    ///
    /// Will be `None` if no bundled event was encrypted.
    unsigned_encryption_info: Option<BTreeMap<UnsignedEventLocation, UnsignedDecryptionResult>>,
}

impl From<SyncTimelineEventDeserializationHelperV0> for SyncTimelineEvent {
    fn from(value: SyncTimelineEventDeserializationHelperV0) -> Self {
        let SyncTimelineEventDeserializationHelperV0 {
            event,
            encryption_info,
            push_actions,
            unsigned_encryption_info,
        } = value;

        let kind = match encryption_info {
            Some(encryption_info) => {
                TimelineEventKind::Decrypted(DecryptedRoomEvent {
                    // We cast from `Raw<AnySyncTimelineEvent>` to
                    // `Raw<AnyMessageLikeEvent>`, which means
                    // we are asserting that it contains a room_id.
                    // That *should* be ok, because if this is genuinely a decrypted
                    // room event (as the encryption_info indicates), then it will have
                    // a room_id.
                    event: event.cast(),
                    encryption_info,
                    unsigned_encryption_info,
                })
            }

            None => TimelineEventKind::PlainText { event },
        };

        SyncTimelineEvent { kind, push_actions }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use assert_matches::assert_matches;
    use ruma::{
        event_id,
        events::{room::message::RoomMessageEventContent, AnySyncTimelineEvent},
        serde::Raw,
        user_id,
    };
    use serde::Deserialize;
    use serde_json::json;

    use super::{
        AlgorithmInfo, DecryptedRoomEvent, EncryptionInfo, SyncTimelineEvent, TimelineEvent,
        TimelineEventKind, UnableToDecryptInfo, UnableToDecryptReason, UnsignedDecryptionResult,
        UnsignedEventLocation, VerificationState,
    };
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
            converted_room_event.raw().deserialize().unwrap();

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

    #[test]
    fn test_verification_level_deserializes() {
        // Given a JSON VerificationLevel
        #[derive(Deserialize)]
        struct Container {
            verification_level: VerificationLevel,
        }
        let container = json!({ "verification_level": "VerificationViolation" });

        // When we deserialize it
        let deserialized: Container = serde_json::from_value(container)
            .expect("We can deserialize the old PreviouslyVerified value");

        // Then it is populated correctly
        assert_eq!(deserialized.verification_level, VerificationLevel::VerificationViolation);
    }

    #[test]
    fn test_verification_level_deserializes_from_old_previously_verified_value() {
        // Given a JSON VerificationLevel with the old value PreviouslyVerified
        #[derive(Deserialize)]
        struct Container {
            verification_level: VerificationLevel,
        }
        let container = json!({ "verification_level": "PreviouslyVerified" });

        // When we deserialize it
        let deserialized: Container = serde_json::from_value(container)
            .expect("We can deserialize the old PreviouslyVerified value");

        // Then it is migrated to the new value
        assert_eq!(deserialized.verification_level, VerificationLevel::VerificationViolation);
    }

    #[test]
    fn sync_timeline_event_serialisation() {
        let room_event = SyncTimelineEvent {
            kind: TimelineEventKind::Decrypted(DecryptedRoomEvent {
                event: Raw::new(&example_event()).unwrap().cast(),
                encryption_info: EncryptionInfo {
                    sender: user_id!("@sender:example.com").to_owned(),
                    sender_device: None,
                    algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
                        curve25519_key: "xxx".to_owned(),
                        sender_claimed_keys: Default::default(),
                    },
                    verification_state: VerificationState::Verified,
                },
                unsigned_encryption_info: Some(BTreeMap::from([(
                    UnsignedEventLocation::RelationsReplace,
                    UnsignedDecryptionResult::UnableToDecrypt(UnableToDecryptInfo {
                        session_id: Some("xyz".to_owned()),
                        reason: UnableToDecryptReason::MalformedEncryptedEvent,
                    }),
                )])),
            }),
            push_actions: Default::default(),
        };

        let serialized = serde_json::to_value(&room_event).unwrap();

        // Test that the serialization is as expected
        assert_eq!(
            serialized,
            json!({
                "kind": {
                    "Decrypted": {
                        "event": {
                            "content": {"body": "secret", "msgtype": "m.text"},
                            "event_id": "$xxxxx:example.org",
                            "origin_server_ts": 2189,
                            "room_id": "!someroom:example.com",
                            "sender": "@carl:example.com",
                            "type": "m.room.message",
                        },
                        "encryption_info": {
                            "sender": "@sender:example.com",
                            "sender_device": null,
                            "algorithm_info": {
                                "MegolmV1AesSha2": {
                                    "curve25519_key": "xxx",
                                    "sender_claimed_keys": {}
                                }
                            },
                            "verification_state": "Verified",
                        },
                        "unsigned_encryption_info": {
                            "RelationsReplace": {"UnableToDecrypt": {
                                "session_id": "xyz",
                                "reason": "MalformedEncryptedEvent",
                            }}
                        }
                    }
                }
            })
        );

        // And it can be properly deserialized from the new format.
        let event: SyncTimelineEvent = serde_json::from_value(serialized).unwrap();
        assert_eq!(event.event_id(), Some(event_id!("$xxxxx:example.org").to_owned()));
        assert_matches!(
            event.encryption_info().unwrap().algorithm_info,
            AlgorithmInfo::MegolmV1AesSha2 { .. }
        );

        // Test that the previous format can also be deserialized.
        let serialized = json!({
            "event": {
                "content": {"body": "secret", "msgtype": "m.text"},
                "event_id": "$xxxxx:example.org",
                "origin_server_ts": 2189,
                "room_id": "!someroom:example.com",
                "sender": "@carl:example.com",
                "type": "m.room.message",
            },
            "encryption_info": {
                "sender": "@sender:example.com",
                "sender_device": null,
                "algorithm_info": {
                    "MegolmV1AesSha2": {
                        "curve25519_key": "xxx",
                        "sender_claimed_keys": {}
                    }
                },
                "verification_state": "Verified",
            },
        });
        let event: SyncTimelineEvent = serde_json::from_value(serialized).unwrap();
        assert_eq!(event.event_id(), Some(event_id!("$xxxxx:example.org").to_owned()));
        assert_matches!(
            event.encryption_info().unwrap().algorithm_info,
            AlgorithmInfo::MegolmV1AesSha2 { .. }
        );

        // Test that the previous format, with an undecryptable unsigned event, can also
        // be deserialized.
        let serialized = json!({
            "event": {
                "content": {"body": "secret", "msgtype": "m.text"},
                "event_id": "$xxxxx:example.org",
                "origin_server_ts": 2189,
                "room_id": "!someroom:example.com",
                "sender": "@carl:example.com",
                "type": "m.room.message",
            },
            "encryption_info": {
                "sender": "@sender:example.com",
                "sender_device": null,
                "algorithm_info": {
                    "MegolmV1AesSha2": {
                        "curve25519_key": "xxx",
                        "sender_claimed_keys": {}
                    }
                },
                "verification_state": "Verified",
            },
            "unsigned_encryption_info": {
                "RelationsReplace": {"UnableToDecrypt": {"session_id": "xyz"}}
            }
        });
        let event: SyncTimelineEvent = serde_json::from_value(serialized).unwrap();
        assert_eq!(event.event_id(), Some(event_id!("$xxxxx:example.org").to_owned()));
        assert_matches!(
            event.encryption_info().unwrap().algorithm_info,
            AlgorithmInfo::MegolmV1AesSha2 { .. }
        );
        assert_matches!(event.kind, TimelineEventKind::Decrypted(decrypted) => {
            assert_matches!(decrypted.unsigned_encryption_info, Some(map) => {
                assert_eq!(map.len(), 1);
                let (location, result) = map.into_iter().next().unwrap();
                assert_eq!(location, UnsignedEventLocation::RelationsReplace);
                assert_matches!(result, UnsignedDecryptionResult::UnableToDecrypt(utd_info) => {
                    assert_eq!(utd_info.session_id, Some("xyz".to_owned()));
                    assert_eq!(utd_info.reason, UnableToDecryptReason::Unknown);
                })
            });
        });
    }
}
