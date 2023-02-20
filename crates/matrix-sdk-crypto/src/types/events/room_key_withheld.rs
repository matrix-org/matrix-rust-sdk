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

use ruma::{JsOption, OwnedDeviceId, OwnedRoomId};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use vodozemac::Curve25519PublicKey;

use super::{EventType, ToDeviceEvent};
use crate::types::{deserialize_curve_key, serialize_curve_key, EventEncryptionAlgorithm};

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
    MegolmV1AesSha2(Box<MegolmV1AesSha2WithheldContent>),
    /// The `m.megolm.v2.aes-sha2` variant of the `m.room_key.withheld` content.
    #[cfg(feature = "experimental-algorithms")]
    MegolmV2AesSha2(Box<MegolmV2AesSha2WithheldContent>),
    /// An unknown and unsupported variant of the `m.room_key.withheld` content.
    Unknown(UnknownRoomKeyWithHeld),
}

impl RoomKeyWithheldContent {
    /// Get the algorithm of the room key withheld.
    pub fn algorithm(&self) -> EventEncryptionAlgorithm {
        match &self {
            RoomKeyWithheldContent::MegolmV1AesSha2(_) => EventEncryptionAlgorithm::MegolmV1AesSha2,
            #[cfg(feature = "experimental-algorithms")]
            RoomKeyWithheldContent::MegolmV2AesSha2(_) => EventEncryptionAlgorithm::MegolmV2AesSha2,
            RoomKeyWithheldContent::Unknown(c) => c.algorithm.to_owned(),
        }
    }

    /// Creates a withheld content from the given info
    pub fn create(
        algorithm: EventEncryptionAlgorithm,
        code: WithheldCode,
        room_id: OwnedRoomId,
        session_id: String,
        sender_key: Curve25519PublicKey,
        from_device: Option<OwnedDeviceId>,
    ) -> Self {
        let (room_id, session_id) = match code {
            WithheldCode::NoOlm => (None, None),
            _ => (Some(room_id), Some(session_id)),
        };
        match algorithm {
            EventEncryptionAlgorithm::MegolmV1AesSha2 => {
                let content = MegolmV1AesSha2WithheldContent {
                    room_id: JsOption::from_implicit_option(room_id),
                    session_id: JsOption::from_implicit_option(session_id),
                    sender_key,
                    code,
                    reason: JsOption::Some(code.to_human_readable().to_owned()),
                    from_device: JsOption::from_implicit_option(from_device),
                    other: Default::default(),
                };
                let helper = WithheldHelper {
                    algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
                    other: serde_json::to_value(content)
                        .expect("We can always serialize this construction"),
                };
                RoomKeyWithheldContent::try_from(helper)
                    .expect("We can always serialize this construction")
            }
            #[cfg(feature = "experimental-algorithms")]
            EventEncryptionAlgorithm::MegolmV2AesSha2 => {
                let content = MegolmV2AesSha2WithheldContent {
                    room_id: JsOption::from_implicit_option(room_id),
                    session_id: JsOption::from_implicit_option(session_id),
                    sender_key,
                    code,
                    reason: JsOption::Some(code.to_human_readable().to_owned()),
                    from_device: JsOption::from_implicit_option(from_device),
                    other: Default::default(),
                };
                let helper = WithheldHelper {
                    algorithm: EventEncryptionAlgorithm::MegolmV2AesSha2,
                    other: serde_json::to_value(content)
                        .expect("We can always serialize this construction"),
                };
                RoomKeyWithheldContent::try_from(helper)
                    .expect("We can always serialize this construction")
            }
            _ => RoomKeyWithheldContent::Unknown(UnknownRoomKeyWithHeld {
                algorithm,
                other: Default::default(),
            }),
        }
    }
}

impl EventType for RoomKeyWithheldContent {
    const EVENT_TYPE: &'static str = "m.room_key.withheld";
}

/// A machine-readable code for why the megolm key was not sent.
#[derive(Eq, Hash, PartialEq, Debug, Serialize, Deserialize, Copy, Clone)]
pub enum WithheldCode {
    /// the user/device was blacklisted.
    #[serde(rename = "m.blacklisted")]
    Blacklisted,

    /// the user/devices is unverified.
    #[serde(rename = "m.unverified")]
    Unverified,

    /// The user/device is not allowed have the key. For example, this would
    /// usually be sent in response to a key request if the user was not in
    /// the room when the message was sent.
    #[serde(rename = "m.unauthorised")]
    Unauthorised,

    /// Sent in reply to a key request if the device that the key is requested
    /// from does not have the requested key.
    #[serde(rename = "m.unavailable")]
    Unavailable,

    /// An olm session could not be established.
    /// This may happen, for example, if the sender was unable to obtain a
    /// one-time key from the recipient.
    #[serde(rename = "m.no_olm")]
    NoOlm,
    // TODO need another custom one? that would use `reason`? for future compatibility?
}

impl WithheldCode {
    /// A human-readable reason for why the key was not sent.
    pub fn to_human_readable(&self) -> &'static str {
        match self {
            WithheldCode::Blacklisted => "The sender has blocked you.",
            WithheldCode::Unverified => "The sender has disabled encrypting to unverified devices.",
            WithheldCode::Unauthorised => "You are not authorised to read the message.",
            WithheldCode::Unavailable => "The requested key was not found.",
            WithheldCode::NoOlm => "Unable to establish a secure channel.",
        }
    }
}

#[derive(Deserialize, Serialize)]
struct WithheldHelper {
    algorithm: EventEncryptionAlgorithm,
    #[serde(flatten)]
    other: Value,
}

/// Devices that purposely do not send megolm keys to a device may instead send
/// an m.room_key.withheld event as a to-device message to the device to
/// indicate that it should not expect to receive keys for the message.
#[derive(Clone, Deserialize, Serialize)]
pub struct MegolmV1AesSha2WithheldContent {
    /// The room where the key is used.
    /// Required if code is not m.no_olm.
    #[serde(default, skip_serializing_if = "JsOption::is_undefined")]
    pub room_id: JsOption<OwnedRoomId>,
    /// Required if code is not m.no_olm. The ID of the session.
    #[serde(default, skip_serializing_if = "JsOption::is_undefined")]
    pub session_id: JsOption<String>,
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    /// The device curve25519 key of the session creator.
    pub sender_key: Curve25519PublicKey,

    ///A machine-readable code for why the megolm key was not sent
    pub code: WithheldCode,

    /// A human-readable reason for why the key was not sent. The receiving
    /// client should only use this string if it does not understand the code
    #[serde(default, skip_serializing_if = "JsOption::is_undefined")]
    pub reason: JsOption<String>,

    /// The device ID of the device sending the m.room_key.withheld message
    /// MSC3735.
    #[serde(default, skip_serializing_if = "JsOption::is_undefined")]
    pub from_device: JsOption<OwnedDeviceId>,

    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

impl MegolmV1AesSha2WithheldContent {
    /// Create a new `m.megolm.v1.aes-sha2` `m.room_key.withheld` content.
    pub fn new(
        room_id: OwnedRoomId,
        session_id: Option<String>,
        sender_key: Curve25519PublicKey,
        code: WithheldCode,
        from_device: Option<OwnedDeviceId>,
    ) -> Self {
        Self {
            room_id: JsOption::Some(room_id),
            session_id: JsOption::from_implicit_option(session_id),
            sender_key,
            code,
            reason: JsOption::Some(code.to_human_readable().to_owned()),
            from_device: JsOption::from_implicit_option(from_device),
            // Help
            other: BTreeMap::from([("algorithm".to_owned(), json!("m.megolm.v1.aes-sha2"))]),
        }
    }
}

impl std::fmt::Debug for MegolmV1AesSha2WithheldContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MegolmV1AesSha2WithheldContent")
            .field("room_id", &self.room_id)
            .field("session_id", &self.session_id)
            .field("code", &self.code)
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
    /// The other data of the unknown room key.
    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

impl TryFrom<WithheldHelper> for RoomKeyWithheldContent {
    type Error = serde_json::Error;

    fn try_from(value: WithheldHelper) -> Result<Self, Self::Error> {
        Ok(match value.algorithm {
            EventEncryptionAlgorithm::MegolmV1AesSha2 => {
                let content: MegolmV1AesSha2WithheldContent = serde_json::from_value(value.other)?;
                Self::MegolmV1AesSha2(content.into())
            }
            #[cfg(feature = "experimental-algorithms")]
            EventEncryptionAlgorithm::MegolmV2AesSha2 => {
                let content: MegolmV1AesSha2WithheldContent = serde_json::from_value(value.other)?;
                Self::MegolmV2AesSha2(content.into())
            }
            _ => Self::Unknown(UnknownRoomKeyWithHeld {
                algorithm: value.algorithm,
                other: serde_json::from_value(value.other)?,
            }),
        })
    }
}

impl Serialize for RoomKeyWithheldContent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let helper = match self {
            Self::MegolmV1AesSha2(r) => WithheldHelper {
                algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
                other: serde_json::to_value(r).map_err(serde::ser::Error::custom)?,
            },
            #[cfg(feature = "experimental-algorithms")]
            Self::MegolmV2AesSha2(r) => WithheldHelper {
                algorithm: EventEncryptionAlgorithm::MegolmV2AesSha2,
                other: serde_json::to_value(r).map_err(serde::ser::Error::custom)?,
            },
            Self::Unknown(r) => WithheldHelper {
                algorithm: r.algorithm.clone(),
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
    use ruma::{
        device_id, room_id, serde::Raw, to_device::DeviceIdOrAllDevices, user_id, JsOption,
    };
    use serde_json::{json, Value};
    use vodozemac::Curve25519PublicKey;

    use super::RoomKeyWithheldEvent;
    use crate::types::{
        events::room_key_withheld::{RoomKeyWithheldContent, WithheldCode},
        EventEncryptionAlgorithm,
    };

    pub fn json() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
                "room_id": "!DwLygpkclUAfQNnfva:localhost:8481",
                "session_id": "0ZcULv8j1nqVWx6orFjD6OW9JQHydDPXfaanA+uRyfs",
                "algorithm": "m.megolm.v1.aes-sha2",
                "sender_key": "9n7mdWKOjr9c4NTlG6zV8dbFtNK79q9vZADoh7nMUwA",
                "code": "m.unverified",
                "reason": "The sender has disabled encrypting to unverified devices.",
                "org.matrix.msgid": "8836f2f0-635d-4f0e-9228-446c63ba3ea3"
            },
            "type": "m.room_key.withheld",
            "m.custom.top": "something custom in the top",
        })
    }

    #[test]
    fn deserialization() -> Result<(), serde_json::Error> {
        let json = json();
        let event: RoomKeyWithheldEvent = serde_json::from_value(json.clone())?;

        assert_matches!(event.content, RoomKeyWithheldContent::MegolmV1AesSha2(_));
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

        let content = RoomKeyWithheldContent::create(
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            WithheldCode::Unverified,
            room_id.to_owned(),
            "0ZcULv8j1nqVWx6orFjD6OW9JQHydDPXfaanA+uRyfs".to_owned(),
            sender_key,
            Some(device_id.to_owned()),
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

        let content = RoomKeyWithheldContent::create(
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            WithheldCode::NoOlm,
            room_id.to_owned(),
            "0ZcULv8j1nqVWx6orFjD6OW9JQHydDPXfaanA+uRyfs".to_owned(),
            sender_key,
            Some(device_id.to_owned()),
        );

        assert_matches!(content, RoomKeyWithheldContent::MegolmV1AesSha2(_));

        let content_box = if let RoomKeyWithheldContent::MegolmV1AesSha2(content_box) = content {
            content_box
        } else {
            panic!("")
        };

        assert_matches!(content_box.room_id, JsOption::Undefined);
        assert_matches!(content_box.session_id, JsOption::Undefined);

        let content = RoomKeyWithheldContent::create(
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            WithheldCode::Unverified,
            room_id.to_owned(),
            "0ZcULv8j1nqVWx6orFjD6OW9JQHydDPXfaanA+uRyfs".to_owned(),
            sender_key,
            Some(device_id.to_owned()),
        );

        assert_matches!(content, RoomKeyWithheldContent::MegolmV1AesSha2(_));

        let content_box = if let RoomKeyWithheldContent::MegolmV1AesSha2(content_box) = content {
            content_box
        } else {
            panic!("")
        };

        assert_matches!(content_box.room_id, JsOption::Some(_));
        assert_matches!(content_box.session_id, JsOption::Some(_));
    }
}
