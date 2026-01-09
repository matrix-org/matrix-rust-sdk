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

use std::{collections::BTreeMap, fmt, ops::Not, sync::Arc};

use ruma::{
    DeviceKeyAlgorithm, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedEventId, OwnedUserId,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, AnyTimelineEvent, AnyToDeviceEvent,
        MessageLikeEventType, room::encrypted::EncryptedEventScheme,
    },
    push::Action,
    serde::{
        AsRefStr, AsStrAsRefStr, DebugAsRefStr, DeserializeFromCowStr, FromString, JsonObject, Raw,
        SerializeAsRefStr,
    },
};
use serde::{Deserialize, Serialize};
use tracing::warn;
#[cfg(target_family = "wasm")]
use wasm_bindgen::prelude::*;

use crate::{
    debug::{DebugRawEvent, DebugStructExt},
    serde_helpers::{extract_bundled_thread_summary, extract_timestamp},
};

const AUTHENTICITY_NOT_GUARANTEED: &str =
    "The authenticity of this encrypted message can't be guaranteed on this device.";
const UNVERIFIED_IDENTITY: &str = "Encrypted by an unverified user.";
const VERIFICATION_VIOLATION: &str =
    "Encrypted by a previously-verified user who is no longer verified.";
const UNSIGNED_DEVICE: &str = "Encrypted by a device not verified by its owner.";
const UNKNOWN_DEVICE: &str = "Encrypted by an unknown or deleted device.";
const MISMATCHED_SENDER: &str = "\
    The sender of the event does not match the owner of the device \
    that created the Megolm session.";

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
                VerificationLevel::MismatchedSender => ShieldState::Red {
                    code: ShieldStateCode::MismatchedSender,
                    message: MISMATCHED_SENDER,
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
                VerificationLevel::MismatchedSender => ShieldState::Red {
                    code: ShieldStateCode::MismatchedSender,
                    message: MISMATCHED_SENDER,
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

    /// The `sender` field on the event does not match the owner of the device
    /// that established the Megolm session.
    MismatchedSender,
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
            VerificationLevel::MismatchedSender => MISMATCHED_SENDER,
        };
        write!(f, "{display}")
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
#[cfg_attr(target_family = "wasm", wasm_bindgen)]
pub enum ShieldStateCode {
    /// Not enough information available to check the authenticity.
    AuthenticityNotGuaranteed,
    /// The sending device isn't yet known by the Client.
    UnknownDevice,
    /// The sending device hasn't been verified by the sender.
    UnsignedDevice,
    /// The sender hasn't been verified by the Client's user.
    UnverifiedIdentity,
    /// The sender was previously verified but changed their identity.
    #[serde(alias = "PreviouslyVerified")]
    VerificationViolation,
    /// The `sender` field on the event does not match the owner of the device
    /// that established the Megolm session.
    MismatchedSender,
}

/// The algorithm specific information of a decrypted event.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
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

        /// The Megolm session ID that was used to encrypt this event, or None
        /// if this info was stored before we collected this data.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },

    /// The info if the event was encrypted using m.olm.v1.curve25519-aes-sha2
    OlmV1Curve25519AesSha2 {
        // The sender device key, base64 encoded
        curve25519_public_key_base64: String,
    },
}

/// Struct containing information on the forwarder of the keys used to decrypt
/// an event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ForwarderInfo {
    /// The user ID of the forwarder.
    pub user_id: OwnedUserId,
    /// The device ID of the forwarder.
    pub device_id: OwnedDeviceId,
}

/// Struct containing information on how an event was decrypted.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EncryptionInfo {
    /// The user ID of the event sender, note this is untrusted data unless the
    /// `verification_state` is `Verified` as well.
    pub sender: OwnedUserId,
    /// The device ID of the device that sent us the event, note this is
    /// untrusted data unless `verification_state` is `Verified` as well.
    pub sender_device: Option<OwnedDeviceId>,
    /// If the keys for this message were shared-on-invite as part of an
    /// [MSC4268] key bundle, information about the forwarder.
    ///
    /// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
    pub forwarder: Option<ForwarderInfo>,
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

impl EncryptionInfo {
    /// Helper to get the megolm session id used to encrypt.
    pub fn session_id(&self) -> Option<&str> {
        if let AlgorithmInfo::MegolmV1AesSha2 { session_id, .. } = &self.algorithm_info {
            session_id.as_deref()
        } else {
            None
        }
    }
}

impl<'de> Deserialize<'de> for EncryptionInfo {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Backwards compatibility: Capture session_id at root if exists. In legacy
        // EncryptionInfo the session_id was not in AlgorithmInfo
        #[derive(Deserialize)]
        struct Helper {
            pub sender: OwnedUserId,
            pub sender_device: Option<OwnedDeviceId>,
            pub forwarder: Option<ForwarderInfo>,
            pub algorithm_info: AlgorithmInfo,
            pub verification_state: VerificationState,
            #[serde(rename = "session_id")]
            pub old_session_id: Option<String>,
        }

        let Helper {
            sender,
            sender_device,
            forwarder,
            algorithm_info,
            verification_state,
            old_session_id,
        } = Helper::deserialize(deserializer)?;

        let algorithm_info = match algorithm_info {
            AlgorithmInfo::MegolmV1AesSha2 { curve25519_key, sender_claimed_keys, session_id } => {
                AlgorithmInfo::MegolmV1AesSha2 {
                    // Migration, merge the old_session_id in algorithm_info
                    session_id: session_id.or(old_session_id),
                    curve25519_key,
                    sender_claimed_keys,
                }
            }
            other => other,
        };

        Ok(EncryptionInfo { sender, sender_device, forwarder, algorithm_info, verification_state })
    }
}

/// A simplified thread summary.
///
/// A thread summary contains useful information pertaining to a thread, and
/// that would be usually attached in clients to a thread root event (i.e. the
/// first event from which the thread originated), along with links into the
/// thread's view. This summary may include, for instance:
///
/// - the number of replies to the thread,
/// - the full event of the latest reply to the thread,
/// - whether the user participated or not to this thread.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ThreadSummary {
    /// The event id for the latest reply to the thread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_reply: Option<OwnedEventId>,

    /// The number of replies to the thread.
    ///
    /// This doesn't include the thread root event itself. It can be zero if no
    /// events in the thread are considered to be meaningful (or they've all
    /// been redacted).
    pub num_replies: u32,
}

/// The status of a thread summary.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum ThreadSummaryStatus {
    /// We don't know if the event has a thread summary.
    #[default]
    Unknown,
    /// The event has no thread summary.
    None,
    /// The event has a thread summary, which is bundled in the event itself.
    Some(ThreadSummary),
}

impl ThreadSummaryStatus {
    /// Create a [`ThreadSummaryStatus`] from an optional thread summary.
    pub fn from_opt(summary: Option<ThreadSummary>) -> Self {
        match summary {
            None => ThreadSummaryStatus::None,
            Some(summary) => ThreadSummaryStatus::Some(summary),
        }
    }

    /// Is the thread status of this event unknown?
    fn is_unknown(&self) -> bool {
        matches!(self, ThreadSummaryStatus::Unknown)
    }

    /// Transforms the [`ThreadSummaryStatus`] into an optional thread summary,
    /// for cases where we don't care about distinguishing unknown and none.
    pub fn summary(&self) -> Option<&ThreadSummary> {
        match self {
            ThreadSummaryStatus::Unknown | ThreadSummaryStatus::None => None,
            ThreadSummaryStatus::Some(thread_summary) => Some(thread_summary),
        }
    }
}

/// Represents a matrix room event that has been returned from a Matrix
/// client-server API endpoint such as `/sync` or `/messages`, after initial
/// processing.
///
/// The "initial processing" includes an attempt to decrypt encrypted events, so
/// the main thing this adds over [`AnyTimelineEvent`] is information on
/// encryption.
//
// 🚨 Note about this type, please read! 🚨
//
// `TimelineEvent` is heavily used across the SDK crates. In some cases, we
// are reaching a [`recursion_limit`] when the compiler is trying to figure out
// if `TimelineEvent` implements `Sync` when it's embedded in other types.
//
// We want to help the compiler so that one doesn't need to increase the
// `recursion_limit`. We stop the recursive check by (un)safely implement `Sync`
// and `Send` on `TimelineEvent` directly.
//
// See
// https://github.com/matrix-org/matrix-rust-sdk/pull/3749#issuecomment-2312939823
// which has addressed this issue first
//
// [`recursion_limit`]: https://doc.rust-lang.org/reference/attributes/limits.html#the-recursion_limit-attribute
#[derive(Clone, Debug, Serialize)]
pub struct TimelineEvent {
    /// The event itself, together with any information on decryption.
    pub kind: TimelineEventKind,

    /// The timestamp of the event. It's the `origin_server_ts` value (if any),
    /// corrected if detected as malicious.
    ///
    /// It can be `None` if the event has been serialised before the addition of
    /// this field, or if parsing the `origin_server_ts` value failed.
    pub timestamp: Option<MilliSecondsSinceUnixEpoch>,

    /// The push actions associated with this event.
    ///
    /// If it's set to `None`, then it means we couldn't compute those actions,
    /// or that they could be computed but there were none.
    #[serde(skip_serializing_if = "skip_serialize_push_actions")]
    push_actions: Option<Vec<Action>>,

    /// If the event is part of a thread, a thread summary.
    #[serde(default, skip_serializing_if = "ThreadSummaryStatus::is_unknown")]
    pub thread_summary: ThreadSummaryStatus,

    /// The bundled latest thread event, if it was provided in the unsigned
    /// relations of this event.
    ///
    /// Not serialized.
    #[serde(skip)]
    pub bundled_latest_thread_event: Option<Box<TimelineEvent>>,
}

// Don't serialize push actions if they're `None` or an empty vec.
fn skip_serialize_push_actions(push_actions: &Option<Vec<Action>>) -> bool {
    push_actions.as_ref().is_none_or(|v| v.is_empty())
}

// See https://github.com/matrix-org/matrix-rust-sdk/pull/3749#issuecomment-2312939823.
#[cfg(not(feature = "test-send-sync"))]
unsafe impl Send for TimelineEvent {}

// See https://github.com/matrix-org/matrix-rust-sdk/pull/3749#issuecomment-2312939823.
#[cfg(not(feature = "test-send-sync"))]
unsafe impl Sync for TimelineEvent {}

#[cfg(feature = "test-send-sync")]
#[test]
// See https://github.com/matrix-org/matrix-rust-sdk/pull/3749#issuecomment-2312939823.
fn test_send_sync_for_sync_timeline_event() {
    fn assert_send_sync<T: crate::SendOutsideWasm + crate::SyncOutsideWasm>() {}

    assert_send_sync::<TimelineEvent>();
}

impl TimelineEvent {
    /// Create a new [`TimelineEvent`] from the given raw event.
    ///
    /// This is a convenience constructor for a plaintext event when you don't
    /// need to set `push_action`, for example inside a test.
    pub fn from_plaintext(event: Raw<AnySyncTimelineEvent>) -> Self {
        Self::from_plaintext_with_max_timestamp(event, MilliSecondsSinceUnixEpoch::now())
    }

    /// Like [`TimelineEvent::from_plaintext`] but with a given `max_timestamp`.
    pub fn from_plaintext_with_max_timestamp(
        event: Raw<AnySyncTimelineEvent>,
        max_timestamp: MilliSecondsSinceUnixEpoch,
    ) -> Self {
        Self::new(TimelineEventKind::PlainText { event }, None, max_timestamp)
    }

    /// Create a new [`TimelineEvent`] from a decrypted event.
    pub fn from_decrypted(
        decrypted: DecryptedRoomEvent,
        push_actions: Option<Vec<Action>>,
    ) -> Self {
        Self::from_decrypted_with_max_timestamp(
            decrypted,
            push_actions,
            MilliSecondsSinceUnixEpoch::now(),
        )
    }

    /// Like [`TimelineEvent::from_decrypted`] but with a given `max_timestamp`.
    pub fn from_decrypted_with_max_timestamp(
        decrypted: DecryptedRoomEvent,
        push_actions: Option<Vec<Action>>,
        max_timestamp: MilliSecondsSinceUnixEpoch,
    ) -> Self {
        Self::new(TimelineEventKind::Decrypted(decrypted), push_actions, max_timestamp)
    }

    /// Create a new [`TimelineEvent`] to represent the given decryption
    /// failure.
    pub fn from_utd(event: Raw<AnySyncTimelineEvent>, utd_info: UnableToDecryptInfo) -> Self {
        Self::from_utd_with_max_timestamp(event, utd_info, MilliSecondsSinceUnixEpoch::now())
    }

    /// Like [`TimelineEvent::from_utd`] but with a given `max_timestamp`.
    pub fn from_utd_with_max_timestamp(
        event: Raw<AnySyncTimelineEvent>,
        utd_info: UnableToDecryptInfo,
        max_timestamp: MilliSecondsSinceUnixEpoch,
    ) -> Self {
        Self::new(TimelineEventKind::UnableToDecrypt { event, utd_info }, None, max_timestamp)
    }

    /// Internal only: helps extracting a thread summary and latest thread event
    /// when creating a new [`TimelineEvent`].
    ///
    /// Build the `timestamp` value by using `now()` as the max value.
    fn new(
        kind: TimelineEventKind,
        push_actions: Option<Vec<Action>>,
        max_timestamp: MilliSecondsSinceUnixEpoch,
    ) -> Self {
        let raw = kind.raw();

        let (thread_summary, latest_thread_event) = extract_bundled_thread_summary(raw);

        let bundled_latest_thread_event =
            Self::from_bundled_latest_event(&kind, latest_thread_event, max_timestamp);

        let timestamp = extract_timestamp(raw, max_timestamp);

        Self { kind, push_actions, timestamp, thread_summary, bundled_latest_thread_event }
    }

    /// Transform this [`TimelineEvent`] into another [`TimelineEvent`] with the
    /// [`TimelineEventKind::Decrypted`] kind.
    ///
    /// ## Panics
    ///
    /// It panics (on debug builds only) if the kind already is
    /// [`TimelineEventKind::Decrypted`].
    pub fn to_decrypted(
        &self,
        decrypted: DecryptedRoomEvent,
        push_actions: Option<Vec<Action>>,
    ) -> Self {
        debug_assert!(
            matches!(self.kind, TimelineEventKind::Decrypted(_)).not(),
            "`TimelineEvent::to_decrypted` has been called on an already decrypted `TimelineEvent`."
        );

        Self {
            kind: TimelineEventKind::Decrypted(decrypted),
            timestamp: self.timestamp,
            push_actions,
            thread_summary: self.thread_summary.clone(),
            bundled_latest_thread_event: self.bundled_latest_thread_event.clone(),
        }
    }

    /// Transform this [`TimelineEvent`] into another [`TimelineEvent`] with the
    /// [`TimelineEventKind::Decrypted`] kind.
    ///
    /// ## Panics
    ///
    /// It panics (on debug builds only) if the kind already is
    /// [`TimelineEventKind::Decrypted`].
    pub fn to_utd(&self, utd_info: UnableToDecryptInfo) -> Self {
        debug_assert!(
            matches!(self.kind, TimelineEventKind::UnableToDecrypt { .. }).not(),
            "`TimelineEvent::to_utd` has been called on an already UTD `TimelineEvent`."
        );

        Self {
            kind: TimelineEventKind::UnableToDecrypt { event: self.raw().clone(), utd_info },
            timestamp: self.timestamp,
            push_actions: None,
            thread_summary: self.thread_summary.clone(),
            bundled_latest_thread_event: self.bundled_latest_thread_event.clone(),
        }
    }

    /// Try to create a new [`TimelineEvent`] for the bundled latest thread
    /// event, if available, and if we have enough information about the
    /// encryption status for it.
    fn from_bundled_latest_event(
        kind: &TimelineEventKind,
        latest_event: Option<Raw<AnySyncMessageLikeEvent>>,
        max_timestamp: MilliSecondsSinceUnixEpoch,
    ) -> Option<Box<Self>> {
        let latest_event = latest_event?;

        match kind {
            TimelineEventKind::Decrypted(decrypted) => {
                if let Some(unsigned_decryption_result) =
                    decrypted.unsigned_encryption_info.as_ref().and_then(|unsigned_map| {
                        unsigned_map.get(&UnsignedEventLocation::RelationsThreadLatestEvent)
                    })
                {
                    match unsigned_decryption_result {
                        UnsignedDecryptionResult::Decrypted(encryption_info) => {
                            // The bundled event was encrypted, and we could decrypt it: pass that
                            // information around.
                            return Some(Box::new(
                                TimelineEvent::from_decrypted_with_max_timestamp(
                                    DecryptedRoomEvent {
                                        // Safety: A decrypted event always includes a room_id in
                                        // its payload.
                                        event: latest_event.cast_unchecked(),
                                        encryption_info: encryption_info.clone(),
                                        // A bundled latest event is never a thread root. It could
                                        // have
                                        // a replacement event, but we don't carry this information
                                        // around.
                                        unsigned_encryption_info: None,
                                    },
                                    None,
                                    max_timestamp,
                                ),
                            ));
                        }

                        UnsignedDecryptionResult::UnableToDecrypt(utd_info) => {
                            // The bundled event was a UTD; store that information.
                            return Some(Box::new(TimelineEvent::from_utd_with_max_timestamp(
                                latest_event.cast(),
                                utd_info.clone(),
                                max_timestamp,
                            )));
                        }
                    }
                }
            }

            TimelineEventKind::UnableToDecrypt { .. } | TimelineEventKind::PlainText { .. } => {
                // Figure based on the event type below.
            }
        }

        match latest_event.get_field::<MessageLikeEventType>("type") {
            Ok(None) => {
                let event_id = latest_event.get_field::<OwnedEventId>("event_id").ok().flatten();
                warn!(
                    ?event_id,
                    "couldn't deserialize bundled latest thread event: missing `type` field \
                     in bundled latest thread event"
                );
                None
            }

            Ok(Some(MessageLikeEventType::RoomEncrypted)) => {
                // The bundled latest thread event is encrypted, but we didn't have any
                // information about it in the unsigned map. Try to fetch the information from
                // the content instead.
                let session_id = if let Some(content) =
                    latest_event.get_field::<EncryptedEventScheme>("content").ok().flatten()
                {
                    match content {
                        EncryptedEventScheme::MegolmV1AesSha2(content) => Some(content.session_id),
                        _ => None,
                    }
                } else {
                    None
                };
                Some(Box::new(TimelineEvent::from_utd_with_max_timestamp(
                    latest_event.cast(),
                    UnableToDecryptInfo { session_id, reason: UnableToDecryptReason::Unknown },
                    max_timestamp,
                )))
            }

            Ok(_) => Some(Box::new(TimelineEvent::from_plaintext_with_max_timestamp(
                latest_event.cast(),
                max_timestamp,
            ))),

            Err(err) => {
                let event_id = latest_event.get_field::<OwnedEventId>("event_id").ok().flatten();
                warn!(?event_id, "couldn't deserialize bundled latest thread event's type: {err}");
                None
            }
        }
    }

    /// Read the current push actions.
    ///
    /// Returns `None` if they were never computed, or if they could not be
    /// computed.
    pub fn push_actions(&self) -> Option<&[Action]> {
        self.push_actions.as_deref()
    }

    /// Set the push actions for this event.
    pub fn set_push_actions(&mut self, push_actions: Vec<Action>) {
        self.push_actions = Some(push_actions);
    }

    /// Get the event id of this [`TimelineEvent`] if the event has any valid
    /// id.
    pub fn event_id(&self) -> Option<OwnedEventId> {
        self.kind.event_id()
    }

    /// Returns a reference to the (potentially decrypted) Matrix event inside
    /// this [`TimelineEvent`].
    pub fn raw(&self) -> &Raw<AnySyncTimelineEvent> {
        self.kind.raw()
    }

    /// Replace the raw event included in this item by another one.
    pub fn replace_raw(&mut self, replacement: Raw<AnyTimelineEvent>) {
        match &mut self.kind {
            TimelineEventKind::Decrypted(decrypted) => decrypted.event = replacement,
            TimelineEventKind::UnableToDecrypt { event, .. }
            | TimelineEventKind::PlainText { event } => {
                // It's safe to cast `AnyMessageLikeEvent` into `AnySyncMessageLikeEvent`,
                // because the former contains a superset of the fields included in the latter.
                *event = replacement.cast();
            }
        }
    }

    /// Get the timestamp.
    ///
    /// If the timestamp is missing (most likely because the event has been
    /// created before the addition of the [`TimelineEvent::timestamp`] field),
    /// this method will try to extract it from the `origin_server_ts` value. If
    /// the `origin_server_ts` value is malicious, it will be capped to
    /// [`MilliSecondsSinceUnixEpoch::now`]. It means that the returned value
    /// might not be constant.
    pub fn timestamp(&self) -> Option<MilliSecondsSinceUnixEpoch> {
        self.timestamp.or_else(|| {
            warn!("`TimelineEvent::timestamp` is parsing the raw event to extract the `timestamp`");

            extract_timestamp(self.raw(), MilliSecondsSinceUnixEpoch::now())
        })
    }

    /// Get the timestamp value, without trying to backfill it if `None`.
    pub fn timestamp_raw(&self) -> Option<MilliSecondsSinceUnixEpoch> {
        self.timestamp
    }

    /// If the event was a decrypted event that was successfully decrypted, get
    /// its encryption info. Otherwise, `None`.
    pub fn encryption_info(&self) -> Option<&Arc<EncryptionInfo>> {
        self.kind.encryption_info()
    }

    /// Takes ownership of this [`TimelineEvent`], returning the (potentially
    /// decrypted) Matrix event within.
    pub fn into_raw(self) -> Raw<AnySyncTimelineEvent> {
        self.kind.into_raw()
    }
}

impl<'de> Deserialize<'de> for TimelineEvent {
    /// Custom deserializer for [`TimelineEvent`], to support older formats.
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
                        "Unable to deserialize V0-format TimelineEvent: {e}",
                    ))
                })?;
            Ok(v0.into())
        }
        // Otherwise, it's V1
        else {
            let v1: SyncTimelineEventDeserializationHelperV1 =
                serde_json::from_value(Value::Object(value)).map_err(|e| {
                    serde::de::Error::custom(format!(
                        "Unable to deserialize V1-format TimelineEvent: {e}",
                    ))
                })?;
            Ok(v1.into())
        }
    }
}

/// The event within a [`TimelineEvent`], together with encryption data.
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
            TimelineEventKind::UnableToDecrypt { event, .. } => event,
            TimelineEventKind::PlainText { event } => event,
        }
    }

    /// Get the event id of this `TimelineEventKind` if the event has any valid
    /// id.
    pub fn event_id(&self) -> Option<OwnedEventId> {
        self.raw().get_field::<OwnedEventId>("event_id").ok().flatten()
    }

    /// Whether we could not decrypt the event (i.e. it is a UTD).
    pub fn is_utd(&self) -> bool {
        matches!(self, TimelineEventKind::UnableToDecrypt { .. })
    }

    /// If the event was a decrypted event that was successfully decrypted, get
    /// its encryption info. Otherwise, `None`.
    pub fn encryption_info(&self) -> Option<&Arc<EncryptionInfo>> {
        match self {
            TimelineEventKind::Decrypted(d) => Some(&d.encryption_info),
            TimelineEventKind::UnableToDecrypt { .. } | TimelineEventKind::PlainText { .. } => None,
        }
    }

    /// If the event was a decrypted event that was successfully decrypted, get
    /// the map of decryption metadata related to the bundled events.
    pub fn unsigned_encryption_map(
        &self,
    ) -> Option<&BTreeMap<UnsignedEventLocation, UnsignedDecryptionResult>> {
        match self {
            TimelineEventKind::Decrypted(d) => d.unsigned_encryption_info.as_ref(),
            TimelineEventKind::UnableToDecrypt { .. } | TimelineEventKind::PlainText { .. } => None,
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
            TimelineEventKind::UnableToDecrypt { event, .. } => event,
            TimelineEventKind::PlainText { event } => event,
        }
    }

    /// The Megolm session ID that was used to send this event, if it was
    /// encrypted.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            TimelineEventKind::Decrypted(decrypted_room_event) => {
                decrypted_room_event.encryption_info.session_id()
            }
            TimelineEventKind::UnableToDecrypt { utd_info, .. } => utd_info.session_id.as_deref(),
            TimelineEventKind::PlainText { .. } => None,
        }
    }

    /// Get the event type of this event.
    ///
    /// Returns `None` if there isn't an event type or if the event failed to be
    /// deserialized.
    pub fn event_type(&self) -> Option<String> {
        self.raw().get_field("type").ok().flatten()
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for TimelineEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self {
            Self::PlainText { event } => f
                .debug_struct("TimelineEventKind::PlainText")
                .field("event", &DebugRawEvent(event))
                .finish(),

            Self::UnableToDecrypt { event, utd_info } => f
                .debug_struct("TimelineEventKind::UnableToDecrypt")
                .field("event", &DebugRawEvent(event))
                .field("utd_info", &utd_info)
                .finish(),

            Self::Decrypted(decrypted) => {
                f.debug_tuple("TimelineEventKind::Decrypted").field(decrypted).finish()
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
/// A successfully-decrypted encrypted event.
pub struct DecryptedRoomEvent {
    /// The decrypted event.
    ///
    /// Note: it's not an error that this contains an [`AnyTimelineEvent`]
    /// (as opposed to an [`AnySyncTimelineEvent`]): an
    /// encrypted payload *always contains* a room id, by the [spec].
    ///
    /// [spec]: https://spec.matrix.org/v1.12/client-server-api/#mmegolmv1aes-sha2
    pub event: Raw<AnyTimelineEvent>,

    /// The encryption info about the event.
    pub encryption_info: Arc<EncryptionInfo>,

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
    Decrypted(Arc<EncryptionInfo>),
    /// The event failed to be decrypted.
    UnableToDecrypt(UnableToDecryptInfo),
}

impl UnsignedDecryptionResult {
    /// Returns the encryption info for this bundled event if it was
    /// successfully decrypted.
    pub fn encryption_info(&self) -> Option<&Arc<EncryptionInfo>> {
        match self {
            Self::Decrypted(info) => Some(info),
            Self::UnableToDecrypt(_) => None,
        }
    }
}

/// Metadata about an event that could not be decrypted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnableToDecryptInfo {
    /// The ID of the session used to encrypt the message, if it used the
    /// `m.megolm.v1.aes-sha2` algorithm.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,

    /// Reason code for the decryption failure
    #[serde(default = "unknown_utd_reason", deserialize_with = "deserialize_utd_reason")]
    pub reason: UnableToDecryptReason,
}

fn unknown_utd_reason() -> UnableToDecryptReason {
    UnableToDecryptReason::Unknown
}

/// Provides basic backward compatibility for deserializing older serialized
/// `UnableToDecryptReason` values.
pub fn deserialize_utd_reason<'de, D>(d: D) -> Result<UnableToDecryptReason, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Start by deserializing as to an untyped JSON value.
    let v: serde_json::Value = Deserialize::deserialize(d)?;
    // Backwards compatibility: `MissingMegolmSession` used to be stored without the
    // withheld code.
    if v.as_str().is_some_and(|s| s == "MissingMegolmSession") {
        return Ok(UnableToDecryptReason::MissingMegolmSession { withheld_code: None });
    }
    // Otherwise, use the derived deserialize impl to turn the JSON into a
    // UnableToDecryptReason
    serde_json::from_value::<UnableToDecryptReason>(v).map_err(serde::de::Error::custom)
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
    MissingMegolmSession {
        /// If the key was withheld on purpose, the associated code. `None`
        /// means no withheld code was received.
        withheld_code: Option<WithheldCode>,
    },

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

    /// The outer state key could not be verified against the inner encrypted
    /// state key and type.
    #[cfg(feature = "experimental-encrypted-state-events")]
    StateKeyVerificationFailed,
}

impl UnableToDecryptReason {
    /// Returns true if this UTD is due to a missing room key (and hence might
    /// resolve itself if we wait a bit.)
    pub fn is_missing_room_key(&self) -> bool {
        // In case of MissingMegolmSession with a withheld code we return false here
        // given that this API is used to decide if waiting a bit will help.
        matches!(
            self,
            Self::MissingMegolmSession { withheld_code: None } | Self::UnknownMegolmMessageIndex
        )
    }
}

/// A machine-readable code for why a Megolm key was not sent.
///
/// Normally sent as the payload of an [`m.room_key.withheld`](https://spec.matrix.org/v1.12/client-server-api/#mroom_keywithheld) to-device message.
#[derive(
    Clone,
    PartialEq,
    Eq,
    Hash,
    AsStrAsRefStr,
    AsRefStr,
    FromString,
    DebugAsRefStr,
    SerializeAsRefStr,
    DeserializeFromCowStr,
)]
pub enum WithheldCode {
    /// the user/device was blacklisted.
    #[ruma_enum(rename = "m.blacklisted")]
    Blacklisted,

    /// the user/devices is unverified.
    #[ruma_enum(rename = "m.unverified")]
    Unverified,

    /// The user/device is not allowed have the key. For example, this would
    /// usually be sent in response to a key request if the user was not in
    /// the room when the message was sent.
    #[ruma_enum(rename = "m.unauthorised")]
    Unauthorised,

    /// Sent in reply to a key request if the device that the key is requested
    /// from does not have the requested key.
    #[ruma_enum(rename = "m.unavailable")]
    Unavailable,

    /// An olm session could not be established.
    /// This may happen, for example, if the sender was unable to obtain a
    /// one-time key from the recipient.
    #[ruma_enum(rename = "m.no_olm")]
    NoOlm,

    /// Normally used when sharing history, per [MSC4268]: indicates
    /// that the session was not marked as "shared_history".
    ///
    /// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
    #[ruma_enum(rename = "io.element.msc4268.history_not_shared", alias = "m.history_not_shared")]
    HistoryNotShared,

    #[doc(hidden)]
    _Custom(PrivOwnedStr),
}

impl fmt::Display for WithheldCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        let string = match self {
            WithheldCode::Blacklisted => "The sender has blocked you.",
            WithheldCode::Unverified => "The sender has disabled encrypting to unverified devices.",
            WithheldCode::Unauthorised => "You are not authorised to read the message.",
            WithheldCode::Unavailable => "The requested key was not found.",
            WithheldCode::NoOlm => "Unable to establish a secure channel.",
            WithheldCode::HistoryNotShared => "The sender disabled sharing encrypted history.",
            _ => self.as_str(),
        };

        f.write_str(string)
    }
}

// The Ruma macro expects the type to have this name.
// The payload is counter intuitively made public in order to avoid having
// multiple copies of this struct.
#[doc(hidden)]
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PrivOwnedStr(pub Box<str>);

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for PrivOwnedStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Deserialization helper for [`TimelineEvent`], for the modern format.
///
/// This has the exact same fields as [`TimelineEvent`] itself, but has a
/// regular `Deserialize` implementation.
#[derive(Debug, Deserialize)]
struct SyncTimelineEventDeserializationHelperV1 {
    /// The event itself, together with any information on decryption.
    kind: TimelineEventKind,

    /// The timestamp of the event. It's the `origin_server_ts` value (if any),
    /// corrected if detected as malicious.
    #[serde(default)]
    timestamp: Option<MilliSecondsSinceUnixEpoch>,

    /// The push actions associated with this event.
    #[serde(default)]
    push_actions: Vec<Action>,

    /// If the event is part of a thread, a thread summary.
    #[serde(default)]
    thread_summary: ThreadSummaryStatus,
}

impl From<SyncTimelineEventDeserializationHelperV1> for TimelineEvent {
    fn from(value: SyncTimelineEventDeserializationHelperV1) -> Self {
        let SyncTimelineEventDeserializationHelperV1 {
            kind,
            timestamp,
            push_actions,
            thread_summary,
        } = value;

        // If `timestamp` is `None`, it is very likely that the event was serialised
        // before the addition of the `timestamp` field. We _could_ compute it here, but
        // if the `timestamp` was malicious, it means we are going to _cap_ the
        // `timestamp` to `now()` for every deserialisation. It is annoying because it
        // means the event is no longer deterministic, it's not constant.
        // We don't want that. Consequently, we keep `None` here, and we let
        // [`TimelineEvent::timestamp`] to handle that case for us.

        TimelineEvent {
            kind,
            timestamp,
            push_actions: Some(push_actions),
            thread_summary,
            // Bundled latest thread event is not persisted.
            bundled_latest_thread_event: None,
        }
    }
}

/// Deserialization helper for [`TimelineEvent`], for an older format.
#[derive(Deserialize)]
struct SyncTimelineEventDeserializationHelperV0 {
    /// The actual event.
    event: Raw<AnySyncTimelineEvent>,

    /// The encryption info about the event.
    ///
    /// Will be `None` if the event was not encrypted.
    encryption_info: Option<Arc<EncryptionInfo>>,

    /// The push actions associated with this event.
    #[serde(default)]
    push_actions: Vec<Action>,

    /// The encryption info about the events bundled in the `unsigned`
    /// object.
    ///
    /// Will be `None` if no bundled event was encrypted.
    unsigned_encryption_info: Option<BTreeMap<UnsignedEventLocation, UnsignedDecryptionResult>>,
}

impl From<SyncTimelineEventDeserializationHelperV0> for TimelineEvent {
    fn from(value: SyncTimelineEventDeserializationHelperV0) -> Self {
        let SyncTimelineEventDeserializationHelperV0 {
            event,
            encryption_info,
            push_actions,
            unsigned_encryption_info,
        } = value;

        // We do not compute the `timestamp` value here because if the `timestamp` is
        // malicious, it means we are going to _cap_ the `timestamp` to `now()` for
        // every deserialisation. It is annoying because it means the event is no longer
        // deterministic, it's not constant. We don't want that. Consequently, we keep
        // `None` here, and we let [`TimelineEvent::timestamp`] to handle that case for
        // us.
        let timestamp = None;

        let kind = match encryption_info {
            Some(encryption_info) => {
                TimelineEventKind::Decrypted(DecryptedRoomEvent {
                    // We cast from `Raw<AnySyncTimelineEvent>` to
                    // `Raw<AnyMessageLikeEvent>`, which means
                    // we are asserting that it contains a room_id.
                    // That *should* be ok, because if this is genuinely a decrypted
                    // room event (as the encryption_info indicates), then it will have
                    // a room_id.
                    event: event.cast_unchecked(),
                    encryption_info,
                    unsigned_encryption_info,
                })
            }

            None => TimelineEventKind::PlainText { event },
        };

        TimelineEvent {
            kind,
            timestamp,
            push_actions: Some(push_actions),
            // No serialized events had a thread summary at this version of the struct.
            thread_summary: ThreadSummaryStatus::Unknown,
            // Bundled latest thread event is not persisted.
            bundled_latest_thread_event: None,
        }
    }
}

/// Reason code for a to-device decryption failure
#[derive(Debug, Clone, PartialEq)]
pub enum ToDeviceUnableToDecryptReason {
    /// An error occurred while encrypting the event. This covers all
    /// `OlmError` types.
    DecryptionFailure,

    /// We refused to decrypt the message because the sender's device is not
    /// verified, or more generally, the sender's identity did not match the
    /// trust requirement we were asked to provide.
    UnverifiedSenderDevice,

    /// We have no `OlmMachine`. This should not happen unless we forget to set
    /// things up by calling `OlmMachine::activate()`.
    NoOlmMachine,

    /// The Matrix SDK was compiled without encryption support.
    EncryptionIsDisabled,
}

/// Metadata about a to-device event that could not be decrypted.
#[derive(Clone, Debug)]
pub struct ToDeviceUnableToDecryptInfo {
    /// Reason code for the decryption failure
    pub reason: ToDeviceUnableToDecryptReason,
}

/// Represents a to-device event after it has been processed by the Olm machine.
#[derive(Clone, Debug)]
pub enum ProcessedToDeviceEvent {
    /// A successfully-decrypted encrypted event.
    /// Contains the raw decrypted event and encryption info
    Decrypted {
        /// The raw decrypted event
        raw: Raw<AnyToDeviceEvent>,
        /// The Olm encryption info
        encryption_info: EncryptionInfo,
    },

    /// An encrypted event which could not be decrypted.
    UnableToDecrypt {
        encrypted_event: Raw<AnyToDeviceEvent>,
        utd_info: ToDeviceUnableToDecryptInfo,
    },

    /// An unencrypted event.
    PlainText(Raw<AnyToDeviceEvent>),

    /// An invalid to device event that was ignored because it is missing some
    /// required information to be processed (like no event `type` for
    /// example)
    Invalid(Raw<AnyToDeviceEvent>),
}

impl ProcessedToDeviceEvent {
    /// Converts a ProcessedToDeviceEvent to the `Raw<AnyToDeviceEvent>` it
    /// encapsulates
    pub fn to_raw(&self) -> Raw<AnyToDeviceEvent> {
        match self {
            ProcessedToDeviceEvent::Decrypted { raw, .. } => raw.clone(),
            ProcessedToDeviceEvent::UnableToDecrypt { encrypted_event, .. } => {
                encrypted_event.clone()
            }
            ProcessedToDeviceEvent::PlainText(event) => event.clone(),
            ProcessedToDeviceEvent::Invalid(event) => event.clone(),
        }
    }

    /// Gets the raw to-device event.
    pub fn as_raw(&self) -> &Raw<AnyToDeviceEvent> {
        match self {
            ProcessedToDeviceEvent::Decrypted { raw, .. } => raw,
            ProcessedToDeviceEvent::UnableToDecrypt { encrypted_event, .. } => encrypted_event,
            ProcessedToDeviceEvent::PlainText(event) => event,
            ProcessedToDeviceEvent::Invalid(event) => event,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use insta::{assert_json_snapshot, with_settings};
    use ruma::{
        DeviceKeyAlgorithm, MilliSecondsSinceUnixEpoch, UInt, device_id, event_id,
        events::{AnySyncTimelineEvent, room::message::RoomMessageEventContent},
        serde::Raw,
        user_id,
    };
    use serde::Deserialize;
    use serde_json::json;

    use super::{
        AlgorithmInfo, DecryptedRoomEvent, DeviceLinkProblem, EncryptionInfo, ShieldState,
        ShieldStateCode, TimelineEvent, TimelineEventKind, UnableToDecryptInfo,
        UnableToDecryptReason, UnsignedDecryptionResult, UnsignedEventLocation, VerificationLevel,
        VerificationState, WithheldCode,
    };
    use crate::deserialized_responses::{ThreadSummary, ThreadSummaryStatus};

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
        let room_event =
            TimelineEvent::from_plaintext(Raw::new(&example_event()).unwrap().cast_unchecked());
        let debug_s = format!("{room_event:?}");
        assert!(
            !debug_s.contains("secret"),
            "Debug representation contains event content!\n{debug_s}"
        );
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
    fn test_shield_state_code_deserializes() {
        // Given a JSON ShieldStateCode with value VerificationViolation
        #[derive(Deserialize)]
        struct Container {
            shield_state_code: ShieldStateCode,
        }
        let container = json!({ "shield_state_code": "VerificationViolation" });

        // When we deserialize it
        let deserialized: Container = serde_json::from_value(container)
            .expect("We can deserialize the old PreviouslyVerified value");

        // Then it is populated correctly
        assert_eq!(deserialized.shield_state_code, ShieldStateCode::VerificationViolation);
    }

    #[test]
    fn test_shield_state_code_deserializes_from_old_previously_verified_value() {
        // Given a JSON ShieldStateCode with the old value PreviouslyVerified
        #[derive(Deserialize)]
        struct Container {
            shield_state_code: ShieldStateCode,
        }
        let container = json!({ "shield_state_code": "PreviouslyVerified" });

        // When we deserialize it
        let deserialized: Container = serde_json::from_value(container)
            .expect("We can deserialize the old PreviouslyVerified value");

        // Then it is migrated to the new value
        assert_eq!(deserialized.shield_state_code, ShieldStateCode::VerificationViolation);
    }

    #[test]
    fn sync_timeline_event_serialisation() {
        let room_event = TimelineEvent {
            kind: TimelineEventKind::Decrypted(DecryptedRoomEvent {
                event: Raw::new(&example_event()).unwrap().cast_unchecked(),
                encryption_info: Arc::new(EncryptionInfo {
                    sender: user_id!("@sender:example.com").to_owned(),
                    sender_device: None,
                    forwarder: None,
                    algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
                        curve25519_key: "xxx".to_owned(),
                        sender_claimed_keys: Default::default(),
                        session_id: Some("xyz".to_owned()),
                    },
                    verification_state: VerificationState::Verified,
                }),
                unsigned_encryption_info: Some(BTreeMap::from([(
                    UnsignedEventLocation::RelationsReplace,
                    UnsignedDecryptionResult::UnableToDecrypt(UnableToDecryptInfo {
                        session_id: Some("xyz".to_owned()),
                        reason: UnableToDecryptReason::MalformedEncryptedEvent,
                    }),
                )])),
            }),
            timestamp: Some(MilliSecondsSinceUnixEpoch(UInt::new_saturating(2189))),
            push_actions: Default::default(),
            thread_summary: ThreadSummaryStatus::Unknown,
            bundled_latest_thread_event: None,
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
                            "forwarder": null,
                            "algorithm_info": {
                                "MegolmV1AesSha2": {
                                    "curve25519_key": "xxx",
                                    "sender_claimed_keys": {},
                                    "session_id": "xyz",
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
                },
                "timestamp": 2189,
            })
        );

        // And it can be properly deserialized from the new format.
        let event: TimelineEvent = serde_json::from_value(serialized).unwrap();
        assert_eq!(event.event_id(), Some(event_id!("$xxxxx:example.org").to_owned()));
        assert_matches!(
            event.encryption_info().unwrap().algorithm_info,
            AlgorithmInfo::MegolmV1AesSha2 { .. }
        );
        assert_eq!(event.timestamp(), Some(MilliSecondsSinceUnixEpoch(UInt::new_saturating(2189))));
        assert_eq!(event.timestamp(), event.timestamp_raw());

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
        let event: TimelineEvent = serde_json::from_value(serialized).unwrap();
        assert_eq!(event.event_id(), Some(event_id!("$xxxxx:example.org").to_owned()));
        assert_matches!(
            event.encryption_info().unwrap().algorithm_info,
            AlgorithmInfo::MegolmV1AesSha2 { session_id: None, .. }
        );
        assert_eq!(event.timestamp(), Some(MilliSecondsSinceUnixEpoch(UInt::new_saturating(2189))));
        assert!(event.timestamp_raw().is_none());

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
        let event: TimelineEvent = serde_json::from_value(serialized).unwrap();
        assert_eq!(event.event_id(), Some(event_id!("$xxxxx:example.org").to_owned()));
        assert_matches!(
            event.encryption_info().unwrap().algorithm_info,
            AlgorithmInfo::MegolmV1AesSha2 { .. }
        );
        assert_eq!(event.timestamp(), Some(MilliSecondsSinceUnixEpoch(UInt::new_saturating(2189))));
        assert!(event.timestamp_raw().is_none());
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

    #[test]
    fn test_creating_or_deserializing_an_event_extracts_summary() {
        let event = json!({
            "event_id": "$eid:example.com",
            "type": "m.room.message",
            "sender": "@alice:example.com",
            "origin_server_ts": 42,
            "content": {
                "body": "Hello, world!",
            },
            "unsigned": {
                "m.relations": {
                    "m.thread": {
                        "latest_event": {
                            "event_id": "$latest_event:example.com",
                            "type": "m.room.message",
                            "sender": "@bob:example.com",
                            "origin_server_ts": 42,
                            "content": {
                                "body": "Hello to you too!",
                                "msgtype": "m.text",
                            }
                        },
                        "count": 2,
                        "current_user_participated": true,
                    }
                }
            }
        });

        let raw = Raw::new(&event).unwrap().cast_unchecked();

        // When creating a timeline event from a raw event, the thread summary is always
        // extracted, if available.
        let timeline_event = TimelineEvent::from_plaintext(raw);
        assert_matches!(timeline_event.thread_summary, ThreadSummaryStatus::Some(ThreadSummary { num_replies, latest_reply }) => {
            assert_eq!(num_replies, 2);
            assert_eq!(latest_reply.as_deref(), Some(event_id!("$latest_event:example.com")));
        });

        assert!(timeline_event.bundled_latest_thread_event.is_some());

        // When deserializing an old serialized timeline event, the thread summary is
        // also extracted, if it wasn't serialized.
        let serialized_timeline_item = json!({
            "kind": {
                "PlainText": {
                    "event": event
                }
            }
        });

        let timeline_event: TimelineEvent =
            serde_json::from_value(serialized_timeline_item).unwrap();
        assert_matches!(timeline_event.thread_summary, ThreadSummaryStatus::Unknown);

        // The bundled latest thread event is not persisted, so it should be `None` when
        // deserialized from a previously serialized `TimelineEvent`.
        assert!(timeline_event.bundled_latest_thread_event.is_none());
    }

    #[test]
    fn sync_timeline_event_deserialisation_migration_for_withheld() {
        // Old serialized version was
        //    "utd_info": {
        //         "reason": "MissingMegolmSession",
        //         "session_id": "session000"
        //       }

        // The new version would be
        //      "utd_info": {
        //         "reason": {
        //           "MissingMegolmSession": {
        //              "withheld_code": null
        //           }
        //         },
        //         "session_id": "session000"
        //       }

        let serialized = json!({
             "kind": {
                "UnableToDecrypt": {
                  "event": {
                    "content": {
                      "algorithm": "m.megolm.v1.aes-sha2",
                      "ciphertext": "AwgAEoABzL1JYhqhjW9jXrlT3M6H8mJ4qffYtOQOnPuAPNxsuG20oiD/Fnpv6jnQGhU6YbV9pNM+1mRnTvxW3CbWOPjLKqCWTJTc7Q0vDEVtYePg38ncXNcwMmfhgnNAoW9S7vNs8C003x3yUl6NeZ8bH+ci870BZL+kWM/lMl10tn6U7snNmSjnE3ckvRdO+11/R4//5VzFQpZdf4j036lNSls/WIiI67Fk9iFpinz9xdRVWJFVdrAiPFwb8L5xRZ8aX+e2JDMlc1eW8gk",
                      "device_id": "SKCGPNUWAU",
                      "sender_key": "Gim/c7uQdSXyrrUbmUOrBT6sMC0gO7QSLmOK6B7NOm0",
                      "session_id": "hgLyeSqXfb8vc5AjQLsg6TSHVu0HJ7HZ4B6jgMvxkrs"
                    },
                    "event_id": "$xxxxx:example.org",
                    "origin_server_ts": 2189,
                    "room_id": "!someroom:example.com",
                    "sender": "@carl:example.com",
                    "type": "m.room.message"
                  },
                  "utd_info": {
                    "reason": "MissingMegolmSession",
                    "session_id": "session000"
                  }
                }
              }
        });

        let result = serde_json::from_value(serialized);
        assert!(result.is_ok());

        // should have migrated to the new format
        let event: TimelineEvent = result.unwrap();
        assert_matches!(
            event.kind,
            TimelineEventKind::UnableToDecrypt { utd_info, .. }=> {
                assert_matches!(
                    utd_info.reason,
                    UnableToDecryptReason::MissingMegolmSession { withheld_code: None }
                );
            }
        )
    }

    #[test]
    fn unable_to_decrypt_info_migration_for_withheld() {
        let old_format = json!({
            "reason": "MissingMegolmSession",
            "session_id": "session000"
        });

        let deserialized = serde_json::from_value::<UnableToDecryptInfo>(old_format).unwrap();
        let session_id = Some("session000".to_owned());

        assert_eq!(deserialized.session_id, session_id);
        assert_eq!(
            deserialized.reason,
            UnableToDecryptReason::MissingMegolmSession { withheld_code: None },
        );

        let new_format = json!({
             "session_id": "session000",
              "reason": {
                "MissingMegolmSession": {
                  "withheld_code": null
                }
              }
        });

        let deserialized = serde_json::from_value::<UnableToDecryptInfo>(new_format).unwrap();

        assert_eq!(
            deserialized.reason,
            UnableToDecryptReason::MissingMegolmSession { withheld_code: None },
        );
        assert_eq!(deserialized.session_id, session_id);
    }

    #[test]
    fn unable_to_decrypt_reason_is_missing_room_key() {
        let reason = UnableToDecryptReason::MissingMegolmSession { withheld_code: None };
        assert!(reason.is_missing_room_key());

        let reason = UnableToDecryptReason::MissingMegolmSession {
            withheld_code: Some(WithheldCode::Blacklisted),
        };
        assert!(!reason.is_missing_room_key());

        let reason = UnableToDecryptReason::UnknownMegolmMessageIndex;
        assert!(reason.is_missing_room_key());
    }

    #[test]
    fn snapshot_test_verification_level() {
        with_settings!({ prepend_module_to_snapshot => false }, {
            assert_json_snapshot!(VerificationLevel::VerificationViolation);
            assert_json_snapshot!(VerificationLevel::UnsignedDevice);
            assert_json_snapshot!(VerificationLevel::None(DeviceLinkProblem::InsecureSource));
            assert_json_snapshot!(VerificationLevel::None(DeviceLinkProblem::MissingDevice));
            assert_json_snapshot!(VerificationLevel::UnverifiedIdentity);
        });
    }

    #[test]
    fn snapshot_test_verification_states() {
        with_settings!({ prepend_module_to_snapshot => false }, {
            assert_json_snapshot!(VerificationState::Unverified(VerificationLevel::UnsignedDevice));
            assert_json_snapshot!(VerificationState::Unverified(
                VerificationLevel::VerificationViolation
            ));
            assert_json_snapshot!(VerificationState::Unverified(VerificationLevel::None(
                DeviceLinkProblem::InsecureSource,
            )));
            assert_json_snapshot!(VerificationState::Unverified(VerificationLevel::None(
                DeviceLinkProblem::MissingDevice,
            )));
            assert_json_snapshot!(VerificationState::Verified);
        });
    }

    #[test]
    fn snapshot_test_shield_states() {
        with_settings!({ prepend_module_to_snapshot => false }, {
            assert_json_snapshot!(ShieldState::None);
            assert_json_snapshot!(ShieldState::Red {
                code: ShieldStateCode::UnverifiedIdentity,
                message: "a message"
            });
            assert_json_snapshot!(ShieldState::Grey {
                code: ShieldStateCode::AuthenticityNotGuaranteed,
                message: "authenticity of this message cannot be guaranteed",
            });
        });
    }

    #[test]
    fn snapshot_test_shield_codes() {
        with_settings!({ prepend_module_to_snapshot => false }, {
            assert_json_snapshot!(ShieldStateCode::AuthenticityNotGuaranteed);
            assert_json_snapshot!(ShieldStateCode::UnknownDevice);
            assert_json_snapshot!(ShieldStateCode::UnsignedDevice);
            assert_json_snapshot!(ShieldStateCode::UnverifiedIdentity);
            assert_json_snapshot!(ShieldStateCode::VerificationViolation);
        });
    }

    #[test]
    fn snapshot_test_algorithm_info() {
        let mut map = BTreeMap::new();
        map.insert(DeviceKeyAlgorithm::Curve25519, "claimedclaimedcurve25519".to_owned());
        map.insert(DeviceKeyAlgorithm::Ed25519, "claimedclaimeded25519".to_owned());
        let info = AlgorithmInfo::MegolmV1AesSha2 {
            curve25519_key: "curvecurvecurve".into(),
            sender_claimed_keys: BTreeMap::from([
                (DeviceKeyAlgorithm::Curve25519, "claimedclaimedcurve25519".to_owned()),
                (DeviceKeyAlgorithm::Ed25519, "claimedclaimeded25519".to_owned()),
            ]),
            session_id: None,
        };

        with_settings!({ prepend_module_to_snapshot => false }, {
            assert_json_snapshot!(info)
        });
    }

    #[test]
    fn test_encryption_info_migration() {
        // In the old format the session_id was in the EncryptionInfo, now
        // it is moved to the `algorithm_info` struct.
        let old_format = json!({
          "sender": "@alice:localhost",
          "sender_device": "ABCDEFGH",
          "algorithm_info": {
            "MegolmV1AesSha2": {
              "curve25519_key": "curvecurvecurve",
              "sender_claimed_keys": {}
            }
          },
          "verification_state": "Verified",
          "session_id": "mysessionid76"
        });

        let deserialized = serde_json::from_value::<EncryptionInfo>(old_format).unwrap();
        let expected_session_id = Some("mysessionid76".to_owned());

        assert_let!(
            AlgorithmInfo::MegolmV1AesSha2 { session_id, .. } = deserialized.algorithm_info.clone()
        );
        assert_eq!(session_id, expected_session_id);

        assert_json_snapshot!(deserialized);
    }

    #[test]
    fn snapshot_test_encryption_info() {
        let info = EncryptionInfo {
            sender: user_id!("@alice:localhost").to_owned(),
            sender_device: Some(device_id!("ABCDEFGH").to_owned()),
            forwarder: None,
            algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
                curve25519_key: "curvecurvecurve".into(),
                sender_claimed_keys: Default::default(),
                session_id: Some("mysessionid76".to_owned()),
            },
            verification_state: VerificationState::Verified,
        };

        with_settings!({ sort_maps => true, prepend_module_to_snapshot => false }, {
            assert_json_snapshot!(info)
        })
    }

    #[test]
    fn snapshot_test_sync_timeline_event() {
        let room_event = TimelineEvent {
            kind: TimelineEventKind::Decrypted(DecryptedRoomEvent {
                event: Raw::new(&example_event()).unwrap().cast_unchecked(),
                encryption_info: Arc::new(EncryptionInfo {
                    sender: user_id!("@sender:example.com").to_owned(),
                    sender_device: Some(device_id!("ABCDEFGHIJ").to_owned()),
                    forwarder: None,
                    algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
                        curve25519_key: "xxx".to_owned(),
                        sender_claimed_keys: BTreeMap::from([
                            (
                                DeviceKeyAlgorithm::Ed25519,
                                "I3YsPwqMZQXHkSQbjFNEs7b529uac2xBpI83eN3LUXo".to_owned(),
                            ),
                            (
                                DeviceKeyAlgorithm::Curve25519,
                                "qzdW3F5IMPFl0HQgz5w/L5Oi/npKUFn8Um84acIHfPY".to_owned(),
                            ),
                        ]),
                        session_id: Some("mysessionid112".to_owned()),
                    },
                    verification_state: VerificationState::Verified,
                }),
                unsigned_encryption_info: Some(BTreeMap::from([(
                    UnsignedEventLocation::RelationsThreadLatestEvent,
                    UnsignedDecryptionResult::UnableToDecrypt(UnableToDecryptInfo {
                        session_id: Some("xyz".to_owned()),
                        reason: UnableToDecryptReason::MissingMegolmSession {
                            withheld_code: Some(WithheldCode::Unverified),
                        },
                    }),
                )])),
            }),
            timestamp: Some(MilliSecondsSinceUnixEpoch(UInt::new_saturating(2189))),
            push_actions: Default::default(),
            thread_summary: ThreadSummaryStatus::Some(ThreadSummary {
                num_replies: 2,
                latest_reply: None,
            }),
            bundled_latest_thread_event: None,
        };

        with_settings!({ sort_maps => true, prepend_module_to_snapshot => false }, {
            // We use directly the serde_json formatter here, because of a bug in insta
            // not serializing custom BTreeMap key enum https://github.com/mitsuhiko/insta/issues/689
            assert_json_snapshot! {
                serde_json::to_value(&room_event).unwrap(),
            }
        });
    }

    #[test]
    fn test_from_bundled_latest_event_keeps_session_id() {
        let session_id = "hgLyeSqXfb8vc5AjQLsg6TSHVu0HJ7HZ4B6jgMvxkrs";
        let serialized = json!({
            "content": {
              "algorithm": "m.megolm.v1.aes-sha2",
              "ciphertext": "AwgAEoABzL1JYhqhjW9jXrlT3M6H8mJ4qffYtOQOnPuAPNxsuG20oiD/Fnpv6jnQGhU6YbV9pNM+1mRnTvxW3CbWOPjLKqCWTJTc7Q0vDEVtYePg38ncXNcwMmfhgnNAoW9S7vNs8C003x3yUl6NeZ8bH+ci870BZL+kWM/lMl10tn6U7snNmSjnE3ckvRdO+11/R4//5VzFQpZdf4j036lNSls/WIiI67Fk9iFpinz9xdRVWJFVdrAiPFwb8L5xRZ8aX+e2JDMlc1eW8gk",
              "device_id": "SKCGPNUWAU",
              "sender_key": "Gim/c7uQdSXyrrUbmUOrBT6sMC0gO7QSLmOK6B7NOm0",
              "session_id": session_id,
            },
            "event_id": "$xxxxx:example.org",
            "origin_server_ts": 2189,
            "room_id": "!someroom:example.com",
            "sender": "@carl:example.com",
            "type": "m.room.encrypted"
        });
        let json = serialized.to_string();
        let value = Raw::<AnySyncTimelineEvent>::from_json_string(json).unwrap();

        let kind = TimelineEventKind::UnableToDecrypt {
            event: value.clone(),
            utd_info: UnableToDecryptInfo {
                session_id: None,
                reason: UnableToDecryptReason::Unknown,
            },
        };
        let result = TimelineEvent::from_bundled_latest_event(
            &kind,
            Some(value.cast_unchecked()),
            MilliSecondsSinceUnixEpoch::now(),
        )
        .expect("Could not get bundled latest event");

        assert_let!(TimelineEventKind::UnableToDecrypt { utd_info, .. } = result.kind);
        assert!(utd_info.session_id.is_some());
        assert_eq!(utd_info.session_id.unwrap(), session_id);
    }
}
