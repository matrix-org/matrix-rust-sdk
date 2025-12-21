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

//! Types for the `m.room_key.withheld` events.

use std::collections::BTreeMap;

use matrix_sdk_common::deserialized_responses::WithheldCode;
use ruma::{
    OwnedDeviceId, OwnedRoomId, RoomId, events::AnyToDeviceEventContent, serde::JsonCastable,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use vodozemac::Curve25519PublicKey;

use super::{EventType, ToDeviceEvent};
use crate::types::{EventEncryptionAlgorithm, deserialize_curve_key, serialize_curve_key};

/// The `m.room_key_request` to-device event.
pub type RoomKeyWithheldEvent = ToDeviceEvent<RoomKeyWithheldContent>;

impl Clone for RoomKeyWithheldEvent {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            content: self.content.clone(),
            other: self.other.clone(),
        }
    }
}

/// The `m.room_key.withheld` event content.
///
/// This is an enum over the different room key algorithms we support.
///
/// Devices that purposely do not send megolm keys to a device may instead send
/// an m.room_key.withheld event as a to-device message to the device to
/// indicate that it should not expect to receive keys for the message.
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "WithheldHelper")]
pub enum RoomKeyWithheldContent {
    /// The `m.megolm.v1.aes-sha2` variant of the `m.room_key.withheld` content.
    MegolmV1AesSha2(MegolmV1AesSha2WithheldContent),
    /// The `m.megolm.v2.aes-sha2` variant of the `m.room_key.withheld` content.
    #[cfg(feature = "experimental-algorithms")]
    MegolmV2AesSha2(MegolmV2AesSha2WithheldContent),
    /// An unknown and unsupported variant of the `m.room_key.withheld` content.
    Unknown(UnknownRoomKeyWithHeld),
}

macro_rules! construct_withheld_content {
    ($algorithm:ident, $code:ident, $room_id:ident, $session_id:ident, $sender_key:ident, $from_device:ident) => {
        match $code {
            WithheldCode::Blacklisted
            | WithheldCode::Unverified
            | WithheldCode::Unauthorised
            | WithheldCode::Unavailable
            | WithheldCode::HistoryNotShared => {
                let content = CommonWithheldCodeContent {
                    $room_id,
                    $session_id,
                    $sender_key,
                    $from_device,
                    other: Default::default(),
                };

                RoomKeyWithheldContent::$algorithm(
                    MegolmV1AesSha2WithheldContent::from_code_and_content($code, content),
                )
            }
            WithheldCode::NoOlm => {
                RoomKeyWithheldContent::$algorithm(MegolmV1AesSha2WithheldContent::NoOlm(
                    NoOlmWithheldContent { $sender_key, $from_device, other: Default::default() }
                        .into(),
                ))
            }
            WithheldCode::_Custom(_) => {
                unreachable!("Can't create an unknown withheld code content")
            }
        }
    };
}

impl RoomKeyWithheldContent {
    /// Creates a withheld content from the given info
    ///
    /// # Panics
    ///
    /// The method will panic if a unsupported algorithm is given. The only
    /// supported algorithm as of now is `m.megolm.v1.aes-sha2`.
    pub fn new(
        algorithm: EventEncryptionAlgorithm,
        code: WithheldCode,
        room_id: OwnedRoomId,
        session_id: String,
        sender_key: Curve25519PublicKey,
        from_device: OwnedDeviceId,
    ) -> Self {
        let from_device = Some(from_device);

        match algorithm {
            EventEncryptionAlgorithm::MegolmV1AesSha2 => {
                construct_withheld_content!(
                    MegolmV1AesSha2,
                    code,
                    room_id,
                    session_id,
                    sender_key,
                    from_device
                )
            }
            #[cfg(feature = "experimental-algorithms")]
            EventEncryptionAlgorithm::MegolmV2AesSha2 => {
                construct_withheld_content!(
                    MegolmV2AesSha2,
                    code,
                    room_id,
                    session_id,
                    sender_key,
                    from_device
                )
            }
            _ => unreachable!("Unsupported algorithm {algorithm}"),
        }
    }

    /// Get the withheld code of this event.
    pub fn withheld_code(&self) -> WithheldCode {
        match self {
            RoomKeyWithheldContent::MegolmV1AesSha2(c) => c.withheld_code(),
            #[cfg(feature = "experimental-algorithms")]
            RoomKeyWithheldContent::MegolmV2AesSha2(c) => c.withheld_code(),
            RoomKeyWithheldContent::Unknown(c) => c.code.to_owned(),
        }
    }

    /// Get the algorithm of the room key withheld.
    pub fn algorithm(&self) -> EventEncryptionAlgorithm {
        match &self {
            RoomKeyWithheldContent::MegolmV1AesSha2(_) => EventEncryptionAlgorithm::MegolmV1AesSha2,
            #[cfg(feature = "experimental-algorithms")]
            RoomKeyWithheldContent::MegolmV2AesSha2(_) => EventEncryptionAlgorithm::MegolmV2AesSha2,
            RoomKeyWithheldContent::Unknown(c) => c.algorithm.to_owned(),
        }
    }

    /// Get the room ID of the withheld session, if known
    pub fn room_id(&self) -> Option<&RoomId> {
        match &self {
            RoomKeyWithheldContent::MegolmV1AesSha2(c) => c.room_id(),
            #[cfg(feature = "experimental-algorithms")]
            RoomKeyWithheldContent::MegolmV2AesSha2(c) => c.room_id(),
            RoomKeyWithheldContent::Unknown(_) => None,
        }
    }

    /// Get the megolm session ID of the withheld session, if it is in fact a
    /// megolm session.
    pub fn megolm_session_id(&self) -> Option<&str> {
        match &self {
            RoomKeyWithheldContent::MegolmV1AesSha2(c) => c.session_id(),
            #[cfg(feature = "experimental-algorithms")]
            RoomKeyWithheldContent::MegolmV2AesSha2(c) => c.session_id(),
            RoomKeyWithheldContent::Unknown(_) => None,
        }
    }
}

impl EventType for RoomKeyWithheldContent {
    const EVENT_TYPE: &'static str = "m.room_key.withheld";
}

impl JsonCastable<AnyToDeviceEventContent> for RoomKeyWithheldContent {}

#[derive(Debug, Deserialize, Serialize)]
struct WithheldHelper {
    pub algorithm: EventEncryptionAlgorithm,
    pub reason: Option<String>,
    pub code: WithheldCode,
    #[serde(flatten)]
    other: Value,
}

/// Content for the `m.room_key.withheld` event for the `m.megolm.v1.aes-sha2`
/// algorithm.
#[derive(Clone, Debug)]
pub enum MegolmV1AesSha2WithheldContent {
    /// The `m.blacklisted` variant of the withheld code content.
    BlackListed(Box<CommonWithheldCodeContent>),
    /// The `m.unverified` variant of the withheld code content.
    Unverified(Box<CommonWithheldCodeContent>),
    /// The `m.unauthorised` variant of the withheld code content.
    Unauthorised(Box<CommonWithheldCodeContent>),
    /// The `m.unavailable` variant of the withheld code content.
    Unavailable(Box<CommonWithheldCodeContent>),
    /// The `m.history_not_shared` variant of the withheld code content (cf
    /// [MSC4268](https://github.com/matrix-org/matrix-spec-proposals/pull/4268)).
    HistoryNotShared(Box<CommonWithheldCodeContent>),
    /// The `m.no_olm` variant of the withheld code content.
    NoOlm(Box<NoOlmWithheldContent>),
}

/// Struct for most of the withheld code content variants.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CommonWithheldCodeContent {
    /// The room where the key is used.
    pub room_id: OwnedRoomId,

    /// The ID of the session.
    pub session_id: String,

    /// The device curve25519 key of the session creator.
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    pub sender_key: Curve25519PublicKey,

    /// The device ID of the device sending the m.room_key.withheld message
    /// MSC3735.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_device: Option<OwnedDeviceId>,

    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

impl CommonWithheldCodeContent {
    /// Create a new common withheld code content.
    pub fn new(
        room_id: OwnedRoomId,
        session_id: String,
        sender_key: Curve25519PublicKey,
        device_id: OwnedDeviceId,
    ) -> Self {
        Self {
            room_id,
            session_id,
            sender_key,
            from_device: Some(device_id),
            other: Default::default(),
        }
    }
}

impl MegolmV1AesSha2WithheldContent {
    /// Get the session ID for this content, if available.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            MegolmV1AesSha2WithheldContent::BlackListed(content)
            | MegolmV1AesSha2WithheldContent::Unverified(content)
            | MegolmV1AesSha2WithheldContent::Unauthorised(content)
            | MegolmV1AesSha2WithheldContent::Unavailable(content)
            | MegolmV1AesSha2WithheldContent::HistoryNotShared(content) => {
                Some(&content.session_id)
            }
            MegolmV1AesSha2WithheldContent::NoOlm(_) => None,
        }
    }

    /// Get the room ID for this content, if available.
    pub fn room_id(&self) -> Option<&RoomId> {
        match self {
            MegolmV1AesSha2WithheldContent::BlackListed(content)
            | MegolmV1AesSha2WithheldContent::Unverified(content)
            | MegolmV1AesSha2WithheldContent::Unauthorised(content)
            | MegolmV1AesSha2WithheldContent::Unavailable(content)
            | MegolmV1AesSha2WithheldContent::HistoryNotShared(content) => Some(&content.room_id),
            MegolmV1AesSha2WithheldContent::NoOlm(_) => None,
        }
    }

    /// Get the withheld code for this content
    pub fn withheld_code(&self) -> WithheldCode {
        match self {
            MegolmV1AesSha2WithheldContent::BlackListed(_) => WithheldCode::Blacklisted,
            MegolmV1AesSha2WithheldContent::Unverified(_) => WithheldCode::Unverified,
            MegolmV1AesSha2WithheldContent::Unauthorised(_) => WithheldCode::Unauthorised,
            MegolmV1AesSha2WithheldContent::Unavailable(_) => WithheldCode::Unavailable,
            MegolmV1AesSha2WithheldContent::HistoryNotShared(_) => WithheldCode::HistoryNotShared,
            MegolmV1AesSha2WithheldContent::NoOlm(_) => WithheldCode::NoOlm,
        }
    }

    fn from_code_and_content(code: WithheldCode, content: CommonWithheldCodeContent) -> Self {
        let content = content.into();

        match code {
            WithheldCode::Blacklisted => Self::BlackListed(content),
            WithheldCode::Unverified => Self::Unverified(content),
            WithheldCode::Unauthorised => Self::Unauthorised(content),
            WithheldCode::Unavailable => Self::Unavailable(content),
            WithheldCode::HistoryNotShared => Self::HistoryNotShared(content),
            WithheldCode::NoOlm | WithheldCode::_Custom(_) => {
                unreachable!("This constructor requires one of the common withheld codes")
            }
        }
    }
}

/// No olm content (no room_id nor session_id)
#[derive(Clone, Deserialize, Serialize)]
pub struct NoOlmWithheldContent {
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    /// The device curve25519 key of the session creator.
    pub sender_key: Curve25519PublicKey,

    /// The device ID of the device sending the m.room_key.withheld message
    /// MSC3735.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_device: Option<OwnedDeviceId>,

    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for CommonWithheldCodeContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommonWithheldCodeContent")
            .field("room_id", &self.room_id)
            .field("session_id", &self.session_id)
            .field("sender_key", &self.sender_key)
            .field("from_device", &self.from_device)
            .finish_non_exhaustive()
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for NoOlmWithheldContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoOlmWithheldContent")
            .field("sender_key", &self.sender_key)
            .field("from_device", &self.from_device)
            .finish_non_exhaustive()
    }
}

/// Megolm V2 variant of withheld content
pub type MegolmV2AesSha2WithheldContent = MegolmV1AesSha2WithheldContent;

/// Withheld info for unknown/unsupported encryption alhg
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UnknownRoomKeyWithHeld {
    /// The algorithm of the unknown room key.
    pub algorithm: EventEncryptionAlgorithm,
    /// The withheld code
    pub code: WithheldCode,
    /// A human-readable reason for why the key was not sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The other data of the unknown room key.
    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

impl TryFrom<WithheldHelper> for RoomKeyWithheldContent {
    type Error = serde_json::Error;

    fn try_from(value: WithheldHelper) -> Result<Self, Self::Error> {
        let unknown = |value: WithheldHelper| -> Result<RoomKeyWithheldContent, _> {
            Ok(Self::Unknown(UnknownRoomKeyWithHeld {
                algorithm: value.algorithm,
                code: value.code,
                reason: value.reason,
                other: serde_json::from_value(value.other)?,
            }))
        };

        Ok(match value.algorithm {
            EventEncryptionAlgorithm::MegolmV1AesSha2 => match value.code {
                WithheldCode::NoOlm => {
                    let content: NoOlmWithheldContent = serde_json::from_value(value.other)?;
                    Self::MegolmV1AesSha2(MegolmV1AesSha2WithheldContent::NoOlm(content.into()))
                }
                WithheldCode::Blacklisted
                | WithheldCode::Unverified
                | WithheldCode::Unauthorised
                | WithheldCode::Unavailable
                | WithheldCode::HistoryNotShared => {
                    let content: CommonWithheldCodeContent = serde_json::from_value(value.other)?;

                    Self::MegolmV1AesSha2(MegolmV1AesSha2WithheldContent::from_code_and_content(
                        value.code, content,
                    ))
                }
                WithheldCode::_Custom(_) => unknown(value)?,
            },
            #[cfg(feature = "experimental-algorithms")]
            EventEncryptionAlgorithm::MegolmV2AesSha2 => match value.code {
                WithheldCode::NoOlm => {
                    let content: NoOlmWithheldContent = serde_json::from_value(value.other)?;
                    Self::MegolmV1AesSha2(MegolmV1AesSha2WithheldContent::NoOlm(content.into()))
                }
                WithheldCode::Blacklisted
                | WithheldCode::Unverified
                | WithheldCode::Unauthorised
                | WithheldCode::Unavailable
                | WithheldCode::HistoryNotShared => {
                    let content: CommonWithheldCodeContent = serde_json::from_value(value.other)?;

                    Self::MegolmV1AesSha2(MegolmV1AesSha2WithheldContent::from_code_and_content(
                        value.code, content,
                    ))
                }
                WithheldCode::_Custom(_) => unknown(value)?,
            },
            _ => unknown(value)?,
        })
    }
}

impl Serialize for RoomKeyWithheldContent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let algorithm = self.algorithm();

        let helper = match self {
            Self::MegolmV1AesSha2(r) => {
                let code = r.withheld_code();
                let reason = Some(code.to_string());

                match r {
                    MegolmV1AesSha2WithheldContent::BlackListed(content)
                    | MegolmV1AesSha2WithheldContent::Unverified(content)
                    | MegolmV1AesSha2WithheldContent::Unauthorised(content)
                    | MegolmV1AesSha2WithheldContent::Unavailable(content)
                    | MegolmV1AesSha2WithheldContent::HistoryNotShared(content) => WithheldHelper {
                        algorithm,
                        code,
                        reason,
                        other: serde_json::to_value(content).map_err(serde::ser::Error::custom)?,
                    },
                    MegolmV1AesSha2WithheldContent::NoOlm(content) => WithheldHelper {
                        algorithm,
                        code,
                        reason,
                        other: serde_json::to_value(content).map_err(serde::ser::Error::custom)?,
                    },
                }
            }
            #[cfg(feature = "experimental-algorithms")]
            Self::MegolmV2AesSha2(r) => {
                let code = r.withheld_code();
                let reason = Some(code.to_string());

                match r {
                    MegolmV1AesSha2WithheldContent::BlackListed(content)
                    | MegolmV1AesSha2WithheldContent::Unverified(content)
                    | MegolmV1AesSha2WithheldContent::Unauthorised(content)
                    | MegolmV1AesSha2WithheldContent::Unavailable(content)
                    | MegolmV1AesSha2WithheldContent::HistoryNotShared(content) => WithheldHelper {
                        algorithm,
                        code,
                        reason,
                        other: serde_json::to_value(content).map_err(serde::ser::Error::custom)?,
                    },
                    MegolmV1AesSha2WithheldContent::NoOlm(content) => WithheldHelper {
                        algorithm,
                        code,
                        reason,
                        other: serde_json::to_value(content).map_err(serde::ser::Error::custom)?,
                    },
                }
            }
            Self::Unknown(r) => WithheldHelper {
                algorithm: r.algorithm.to_owned(),
                code: r.code.to_owned(),
                reason: r.reason.to_owned(),
                other: serde_json::to_value(r.other.clone()).map_err(serde::ser::Error::custom)?,
            },
        };

        helper.serialize(serializer)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::BTreeMap;

    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use matrix_sdk_common::deserialized_responses::WithheldCode;
    use ruma::{device_id, room_id, serde::Raw, to_device::DeviceIdOrAllDevices, user_id};
    use serde_json::{Value, json};
    use vodozemac::Curve25519PublicKey;

    use super::RoomKeyWithheldEvent;
    use crate::types::{
        EventEncryptionAlgorithm,
        events::room_key_withheld::{MegolmV1AesSha2WithheldContent, RoomKeyWithheldContent},
    };

    pub fn json(code: &WithheldCode) -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
                "room_id": "!DwLygpkclUAfQNnfva:localhost:8481",
                "session_id": "0ZcULv8j1nqVWx6orFjD6OW9JQHydDPXfaanA+uRyfs",
                "algorithm": "m.megolm.v1.aes-sha2",
                "sender_key": "9n7mdWKOjr9c4NTlG6zV8dbFtNK79q9vZADoh7nMUwA",
                "code": code.to_owned(),
                "reason": code.to_string(),
                "org.matrix.msgid": "8836f2f0-635d-4f0e-9228-446c63ba3ea3"
            },
            "type": "m.room_key.withheld",
            "m.custom.top": "something custom in the top",
        })
    }

    pub fn no_olm_json() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2",
                "sender_key": "9n7mdWKOjr9c4NTlG6zV8dbFtNK79q9vZADoh7nMUwA",
                "code": "m.no_olm",
                "reason": "Unable to establish a secure channel.",
                "org.matrix.msgid": "8836f2f0-635d-4f0e-9228-446c63ba3ea3"
            },
            "type": "m.room_key.withheld",
            "m.custom.top": "something custom in the top",
        })
    }

    pub fn unknown_alg_json() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
                "algorithm": "caesar.cipher",
                "sender_key": "9n7mdWKOjr9c4NTlG6zV8dbFtNK79q9vZADoh7nMUwA",
                "code": "m.brutus",
                "reason": "Tu quoque fili",
                "org.matrix.msgid": "8836f2f0-635d-4f0e-9228-446c63ba3ea3"
            },
            "type": "m.room_key.withheld",
            "m.custom.top": "something custom in the top",
        })
    }

    pub fn unknown_code_json() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
                "room_id": "!DwLygpkclUAfQNnfva:localhost:8481",
                "session_id": "0ZcULv8j1nqVWx6orFjD6OW9JQHydDPXfaanA+uRyfs",
                "algorithm": "m.megolm.v1.aes-sha2",
                "sender_key": "9n7mdWKOjr9c4NTlG6zV8dbFtNK79q9vZADoh7nMUwA",
                "code": "org.mscXXX.new_code",
                "reason": "Unable to establish a secure channel.",
                "org.matrix.msgid": "8836f2f0-635d-4f0e-9228-446c63ba3ea3"
            },
            "type": "m.room_key.withheld",
            "m.custom.top": "something custom in the top",
        })
    }

    #[test]
    fn deserialization() -> Result<(), serde_json::Error> {
        let codes = [
            WithheldCode::Unverified,
            WithheldCode::Blacklisted,
            WithheldCode::Unauthorised,
            WithheldCode::Unavailable,
            WithheldCode::HistoryNotShared,
        ];
        for code in codes {
            let json = json(&code);
            let event: RoomKeyWithheldEvent = serde_json::from_value(json.clone())?;

            assert_let!(RoomKeyWithheldContent::MegolmV1AesSha2(content) = &event.content);
            assert_eq!(code, content.withheld_code());

            assert_eq!(event.content.algorithm(), EventEncryptionAlgorithm::MegolmV1AesSha2);
            let serialized = serde_json::to_value(event)?;
            assert_eq!(json, serialized);
        }
        Ok(())
    }

    #[test]
    fn deserialization_no_olm() -> Result<(), serde_json::Error> {
        let json = no_olm_json();
        let event: RoomKeyWithheldEvent = serde_json::from_value(json.clone())?;
        assert_matches!(
            event.content,
            RoomKeyWithheldContent::MegolmV1AesSha2(MegolmV1AesSha2WithheldContent::NoOlm(_))
        );
        let serialized = serde_json::to_value(event)?;
        assert_eq!(json, serialized);

        Ok(())
    }

    #[test]
    fn deserialization_unknown_code() -> Result<(), serde_json::Error> {
        let json = unknown_code_json();
        let event: RoomKeyWithheldEvent = serde_json::from_value(json.clone())?;
        assert_matches!(event.content, RoomKeyWithheldContent::Unknown(_));

        assert_let!(RoomKeyWithheldContent::Unknown(content) = &event.content);
        assert_eq!(content.code.as_str(), "org.mscXXX.new_code");

        let serialized = serde_json::to_value(event)?;
        assert_eq!(json, serialized);

        Ok(())
    }

    #[test]
    fn deserialization_unknown_alg() -> Result<(), serde_json::Error> {
        let json = unknown_alg_json();
        let event: RoomKeyWithheldEvent = serde_json::from_value(json.clone())?;
        assert_matches!(event.content, RoomKeyWithheldContent::Unknown(_));

        assert_let!(RoomKeyWithheldContent::Unknown(content) = &event.content);
        assert_matches!(&content.code, WithheldCode::_Custom(_));
        let serialized = serde_json::to_value(event)?;
        assert_eq!(json, serialized);

        Ok(())
    }

    #[test]
    fn serialization_to_device() {
        let mut messages = BTreeMap::new();

        let room_id = room_id!("!DwLygpkclUAfQNnfva:localhost:8481");
        let user_id = user_id!("@alice:example.org");
        let device_id = device_id!("DEV001");
        let sender_key =
            Curve25519PublicKey::from_base64("9n7mdWKOjr9c4NTlG6zV8dbFtNK79q9vZADoh7nMUwA")
                .unwrap();

        let content = RoomKeyWithheldContent::new(
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            WithheldCode::Unverified,
            room_id.to_owned(),
            "0ZcULv8j1nqVWx6orFjD6OW9JQHydDPXfaanA+uRyfs".to_owned(),
            sender_key,
            device_id.to_owned(),
        );
        let content = Raw::new(&content).expect("We can always serialize a withheld content info");

        messages
            .entry(user_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(DeviceIdOrAllDevices::DeviceId(device_id.to_owned()), content);

        let serialized = serde_json::to_value(messages).unwrap();

        let expected: Value = json!({
            "@alice:example.org":{
                "DEV001":{
                    "algorithm":"m.megolm.v1.aes-sha2",
                    "code":"m.unverified",
                    "from_device":"DEV001",
                    "reason":"The sender has disabled encrypting to unverified devices.",
                    "room_id":"!DwLygpkclUAfQNnfva:localhost:8481",
                    "sender_key":"9n7mdWKOjr9c4NTlG6zV8dbFtNK79q9vZADoh7nMUwA",
                    "session_id":"0ZcULv8j1nqVWx6orFjD6OW9JQHydDPXfaanA+uRyfs"
                }
            }
        });
        assert_eq!(serialized, expected);
    }

    #[test]
    fn no_olm_should_not_have_room_and_session() {
        let room_id = room_id!("!DwLygpkclUAfQNnfva:localhost:8481");
        let device_id = device_id!("DEV001");
        let sender_key =
            Curve25519PublicKey::from_base64("9n7mdWKOjr9c4NTlG6zV8dbFtNK79q9vZADoh7nMUwA")
                .unwrap();

        let content = RoomKeyWithheldContent::new(
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            WithheldCode::NoOlm,
            room_id.to_owned(),
            "0ZcULv8j1nqVWx6orFjD6OW9JQHydDPXfaanA+uRyfs".to_owned(),
            sender_key,
            device_id.to_owned(),
        );

        assert_let!(
            RoomKeyWithheldContent::MegolmV1AesSha2(MegolmV1AesSha2WithheldContent::NoOlm(
                content
            )) = content
        );
        assert_eq!(content.sender_key, sender_key);

        let content = RoomKeyWithheldContent::new(
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            WithheldCode::Unverified,
            room_id.to_owned(),
            "0ZcULv8j1nqVWx6orFjD6OW9JQHydDPXfaanA+uRyfs".to_owned(),
            sender_key,
            device_id.to_owned(),
        );

        assert_let!(
            RoomKeyWithheldContent::MegolmV1AesSha2(MegolmV1AesSha2WithheldContent::Unverified(
                content
            )) = content
        );
        assert_eq!(content.session_id, "0ZcULv8j1nqVWx6orFjD6OW9JQHydDPXfaanA+uRyfs");
    }
}
