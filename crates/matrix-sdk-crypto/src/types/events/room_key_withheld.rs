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

use ruma::{
    exports::ruma_macros::AsStrAsRefStr,
    serde::{AsRefStr, DebugAsRefStr, DeserializeFromCowStr, FromString, SerializeAsRefStr},
    JsOption, OwnedDeviceId, OwnedRoomId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use vodozemac::Curve25519PublicKey;

use super::{EventType, ToDeviceEvent};
use crate::types::{
    deserialize_curve_key, serialize_curve_key, EventEncryptionAlgorithm, PrivOwnedStr,
};

/// The `m.room_key_request` to-device event.
pub type RoomKeyWithheldEvent = ToDeviceEvent<RoomKeyWithheldContent>;

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
    // The `m.megolm.v2.aes-sha2` variant of the `m.room_key.withheld` content.
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
            | WithheldCode::Unavailable => {
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
            _ => unreachable!("Can't create an unknown withheld code content"),
        }
    };
}

impl RoomKeyWithheldContent {
    /// Creates a withheld content from the given info
    pub fn new(
        algorithm: EventEncryptionAlgorithm,
        code: WithheldCode,
        room_id: OwnedRoomId,
        session_id: String,
        sender_key: Curve25519PublicKey,
        from_device: OwnedDeviceId,
    ) -> Self {
        let from_device = JsOption::Some(from_device);

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
            _ => todo!(),
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
}

impl EventType for RoomKeyWithheldContent {
    const EVENT_TYPE: &'static str = "m.room_key.withheld";
}

/// A machine-readable code for why the megolm key was not sent.
#[derive(
    Eq,
    Hash,
    PartialEq,
    Clone,
    AsStrAsRefStr,
    AsRefStr,
    FromString,
    DebugAsRefStr,
    SerializeAsRefStr,
    DeserializeFromCowStr,
)]
#[non_exhaustive]
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

    #[doc(hidden)]
    _Custom(PrivOwnedStr),
}

impl std::fmt::Display for WithheldCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        let string = match self {
            WithheldCode::Blacklisted => "The sender has blocked you.",
            WithheldCode::Unverified => "The sender has disabled encrypting to unverified devices.",
            WithheldCode::Unauthorised => "You are not authorised to read the message.",
            WithheldCode::Unavailable => "The requested key was not found.",
            WithheldCode::NoOlm => "Unable to establish a secure channel.",
            _ => self.as_str(),
        };

        write!(f, "{string}")
    }
}

#[derive(Deserialize, Serialize, Debug)]
struct WithheldHelper {
    pub algorithm: EventEncryptionAlgorithm,
    pub reason: JsOption<String>,
    pub code: WithheldCode,
    #[serde(flatten)]
    other: Value,
}

#[derive(Clone, Debug)]
pub enum MegolmV1AesSha2WithheldContent {
    BlackListed(Box<CommonWithheldCodeContent>),
    Unverified(Box<CommonWithheldCodeContent>),
    Unauthorised(Box<CommonWithheldCodeContent>),
    Unavailable(Box<CommonWithheldCodeContent>),
    NoOlm(Box<NoOlmWithheldContent>),
}

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
    #[serde(default, skip_serializing_if = "JsOption::is_undefined")]
    pub from_device: JsOption<OwnedDeviceId>,

    #[serde(flatten)]
    pub other: BTreeMap<String, Value>,
}

impl MegolmV1AesSha2WithheldContent {
    /// Get the withheld code for this content
    pub fn withheld_code(&self) -> WithheldCode {
        match self {
            MegolmV1AesSha2WithheldContent::BlackListed(_) => WithheldCode::Blacklisted,
            MegolmV1AesSha2WithheldContent::Unverified(_) => WithheldCode::Unverified,
            MegolmV1AesSha2WithheldContent::Unauthorised(_) => WithheldCode::Unauthorised,
            MegolmV1AesSha2WithheldContent::Unavailable(_) => WithheldCode::Unavailable,
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
            _ => unreachable!("This constructor requires one of the common withheld codes"),
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
    #[serde(default, skip_serializing_if = "JsOption::is_undefined")]
    pub from_device: JsOption<OwnedDeviceId>,

    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

impl std::fmt::Debug for CommonWithheldCodeContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnyWithheldContent")
            .field("room_id", &self.room_id)
            .field("session_id", &self.session_id)
            .field("sender_key", &self.sender_key)
            .field("from_device", &self.from_device)
            .finish_non_exhaustive()
    }
}

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

    pub reason: JsOption<String>,
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
                | WithheldCode::Unavailable => {
                    let content: CommonWithheldCodeContent = serde_json::from_value(value.other)?;

                    Self::MegolmV1AesSha2(MegolmV1AesSha2WithheldContent::from_code_and_content(
                        value.code, content,
                    ))
                }
                _ => unknown(value)?,
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
                | WithheldCode::Unavailable => {
                    let content: CommonWithheldCodeContent = serde_json::from_value(value.other)?;

                    Self::MegolmV1AesSha2(MegolmV1AesSha2WithheldContent::from_code_and_content(
                        value.code, content,
                    ))
                }
                _ => unknown(value)?,
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
                let reason = JsOption::Some(code.to_string());

                match r {
                    MegolmV1AesSha2WithheldContent::BlackListed(content)
                    | MegolmV1AesSha2WithheldContent::Unverified(content)
                    | MegolmV1AesSha2WithheldContent::Unauthorised(content)
                    | MegolmV1AesSha2WithheldContent::Unavailable(content) => WithheldHelper {
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
                let reason = JsOption::Some(code.to_string());

                match r {
                    MegolmV1AesSha2WithheldContent::BlackListed(content)
                    | MegolmV1AesSha2WithheldContent::Unverified(content)
                    | MegolmV1AesSha2WithheldContent::Unauthorised(content)
                    | MegolmV1AesSha2WithheldContent::Unavailable(content) => WithheldHelper {
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
pub(super) mod test {
    use std::collections::BTreeMap;

    use assert_matches::assert_matches;
    use ruma::{device_id, room_id, serde::Raw, to_device::DeviceIdOrAllDevices, user_id};
    use serde_json::{json, Value};
    use vodozemac::Curve25519PublicKey;

    use super::RoomKeyWithheldEvent;
    use crate::types::{
        events::room_key_withheld::{
            MegolmV1AesSha2WithheldContent, RoomKeyWithheldContent, WithheldCode,
        },
        EventEncryptionAlgorithm,
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
        ];
        for code in codes {
            let json = json(&code);
            let event: RoomKeyWithheldEvent = serde_json::from_value(json.clone())?;

            assert_matches!(&event.content, RoomKeyWithheldContent::MegolmV1AesSha2(content) if code == content.withheld_code());

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

        assert_matches!(
            &event.content,
            RoomKeyWithheldContent::Unknown(content)
            if content.code.as_str() == "org.mscXXX.new_code"
        );

        let serialized = serde_json::to_value(event)?;
        assert_eq!(json, serialized);

        Ok(())
    }

    #[test]
    fn deserialization_unknown_alg() -> Result<(), serde_json::Error> {
        let json = unknown_alg_json();
        let event: RoomKeyWithheldEvent = serde_json::from_value(json.clone())?;
        assert_matches!(event.content, RoomKeyWithheldContent::Unknown(_));

        if let RoomKeyWithheldContent::Unknown(content) = &event.content {
            assert_matches!(content.code, WithheldCode::_Custom(_));
        } else {
            panic!()
        }
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
        let content: Raw<RoomKeyWithheldContent> =
            Raw::new(&content).expect("We can always serialize a withheld content info").cast();

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

        assert_matches!(
            content,
            RoomKeyWithheldContent::MegolmV1AesSha2(MegolmV1AesSha2WithheldContent::NoOlm(content))
            if content.sender_key == sender_key
        );

        let content = RoomKeyWithheldContent::new(
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            WithheldCode::Unverified,
            room_id.to_owned(),
            "0ZcULv8j1nqVWx6orFjD6OW9JQHydDPXfaanA+uRyfs".to_owned(),
            sender_key,
            device_id.to_owned(),
        );

        assert_matches!(
            content,
            RoomKeyWithheldContent::MegolmV1AesSha2(MegolmV1AesSha2WithheldContent::Unverified(content))
            if content.session_id == "0ZcULv8j1nqVWx6orFjD6OW9JQHydDPXfaanA+uRyfs"
        );
    }
}
