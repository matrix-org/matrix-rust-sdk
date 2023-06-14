// Copyright 2022 The Matrix.org Foundation C.I.C.
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

//! Types for the `m.room.encrypted` room events.

use std::collections::BTreeMap;

use ruma::{JsOption, OwnedDeviceId, RoomId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use vodozemac::{megolm::MegolmMessage, olm::OlmMessage, Curve25519PublicKey};

use super::Event;
use crate::types::{
    deserialize_curve_key,
    events::{
        room_key_request::{self, SupportedKeyInfo},
        EventType, ToDeviceEvent,
    },
    serialize_curve_key, EventEncryptionAlgorithm,
};

/// An m.room.encrypted room event.
pub type EncryptedEvent = Event<RoomEncryptedEventContent>;

impl EncryptedEvent {
    /// Get the unique info about the room key that was used to encrypt this
    /// event.
    ///
    /// Returns `None` if we do not understand the algorithm that was used to
    /// encrypt the event.
    pub fn room_key_info(&self, room_id: &RoomId) -> Option<SupportedKeyInfo> {
        let room_id = room_id.to_owned();

        match &self.content.scheme {
            RoomEventEncryptionScheme::MegolmV1AesSha2(c) => Some(
                room_key_request::MegolmV1AesSha2Content {
                    room_id,
                    sender_key: c.sender_key,
                    session_id: c.session_id.clone(),
                }
                .into(),
            ),
            #[cfg(feature = "experimental-algorithms")]
            RoomEventEncryptionScheme::MegolmV2AesSha2(c) => Some(
                room_key_request::MegolmV2AesSha2Content {
                    room_id,
                    session_id: c.session_id.clone(),
                }
                .into(),
            ),
            RoomEventEncryptionScheme::Unknown(_) => None,
        }
    }
}

/// An m.room.encrypted to-device event.
pub type EncryptedToDeviceEvent = ToDeviceEvent<ToDeviceEncryptedEventContent>;

impl EncryptedToDeviceEvent {
    /// Get the algorithm of the encrypted event content.
    pub fn algorithm(&self) -> EventEncryptionAlgorithm {
        self.content.algorithm()
    }
}

/// The content for `m.room.encrypted` to-device events.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "Helper")]
pub enum ToDeviceEncryptedEventContent {
    /// The event content for events encrypted with the m.megolm.v1.aes-sha2
    /// algorithm.
    OlmV1Curve25519AesSha2(Box<OlmV1Curve25519AesSha2Content>),
    /// The event content for events encrypted with the m.olm.v2.aes-sha2
    /// algorithm.
    #[cfg(feature = "experimental-algorithms")]
    OlmV2Curve25519AesSha2(Box<OlmV2Curve25519AesSha2Content>),
    /// An event content that was encrypted with an unknown encryption
    /// algorithm.
    Unknown(UnknownEncryptedContent),
}

impl EventType for ToDeviceEncryptedEventContent {
    const EVENT_TYPE: &'static str = "m.room.encrypted";
}

impl ToDeviceEncryptedEventContent {
    /// Get the algorithm of the event content.
    pub fn algorithm(&self) -> EventEncryptionAlgorithm {
        match self {
            ToDeviceEncryptedEventContent::OlmV1Curve25519AesSha2(_) => {
                EventEncryptionAlgorithm::OlmV1Curve25519AesSha2
            }
            #[cfg(feature = "experimental-algorithms")]
            ToDeviceEncryptedEventContent::OlmV2Curve25519AesSha2(_) => {
                EventEncryptionAlgorithm::OlmV2Curve25519AesSha2
            }
            ToDeviceEncryptedEventContent::Unknown(c) => c.algorithm.to_owned(),
        }
    }
}

/// The event content for events encrypted with the m.olm.v1.curve25519-aes-sha2
/// algorithm.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "OlmHelper")]
pub struct OlmV1Curve25519AesSha2Content {
    /// The encrypted content of the event.
    pub ciphertext: OlmMessage,

    /// The Curve25519 key of the recipient device.
    pub recipient_key: Curve25519PublicKey,

    /// The Curve25519 key of the sender.
    pub sender_key: Curve25519PublicKey,

    /// The unique ID of this content.
    pub message_id: JsOption<String>,
}

/// The event content for events encrypted with the m.olm.v2.curve25519-aes-sha2
/// algorithm.
#[cfg(feature = "experimental-algorithms")]
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct OlmV2Curve25519AesSha2Content {
    /// The encrypted content of the event.
    pub ciphertext: OlmMessage,

    /// The Curve25519 key of the sender.
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    pub sender_key: Curve25519PublicKey,

    /// The unique ID of this content.
    #[serde(default, skip_serializing_if = "JsOption::is_undefined", rename = "org.matrix.msgid")]
    pub message_id: JsOption<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct OlmHelper {
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    sender_key: Curve25519PublicKey,
    ciphertext: BTreeMap<String, OlmMessage>,
    #[serde(default, skip_serializing_if = "JsOption::is_undefined", rename = "org.matrix.msgid")]
    message_id: JsOption<String>,
}

impl Serialize for OlmV1Curve25519AesSha2Content {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let ciphertext =
            BTreeMap::from([(self.recipient_key.to_base64(), self.ciphertext.clone())]);

        OlmHelper {
            sender_key: self.sender_key,
            ciphertext,
            message_id: self.message_id.to_owned(),
        }
        .serialize(serializer)
    }
}

impl TryFrom<OlmHelper> for OlmV1Curve25519AesSha2Content {
    type Error = serde_json::Error;

    fn try_from(value: OlmHelper) -> Result<Self, Self::Error> {
        let (recipient_key, ciphertext) = value.ciphertext.into_iter().next().ok_or_else(|| {
            serde::de::Error::custom(
                "The `m.room.encrypted` event is missing a ciphertext".to_owned(),
            )
        })?;

        let recipient_key =
            Curve25519PublicKey::from_base64(&recipient_key).map_err(serde::de::Error::custom)?;

        Ok(Self {
            ciphertext,
            recipient_key,
            sender_key: value.sender_key,
            message_id: value.message_id,
        })
    }
}

/// The content for `m.room.encrypted` room events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomEncryptedEventContent {
    /// Algorithm-specific fields.
    #[serde(flatten)]
    pub scheme: RoomEventEncryptionScheme,

    /// Information about related events.
    #[serde(rename = "m.relates_to", skip_serializing_if = "Option::is_none")]
    pub relates_to: Option<Value>,

    /// The other data of the encrypted content.
    #[serde(flatten)]
    pub(crate) other: BTreeMap<String, Value>,
}

impl RoomEncryptedEventContent {
    /// Get the algorithm of the event content.
    pub fn algorithm(&self) -> EventEncryptionAlgorithm {
        self.scheme.algorithm()
    }
}

impl EventType for RoomEncryptedEventContent {
    const EVENT_TYPE: &'static str = "m.room.encrypted";
}

/// An enum for per encryption algorithm event contents.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "Helper")]
pub enum RoomEventEncryptionScheme {
    /// The event content for events encrypted with the m.megolm.v1.aes-sha2
    /// algorithm.
    MegolmV1AesSha2(MegolmV1AesSha2Content),
    /// The event content for events encrypted with the m.megolm.v2.aes-sha2
    /// algorithm.
    #[cfg(feature = "experimental-algorithms")]
    MegolmV2AesSha2(MegolmV2AesSha2Content),
    /// An event content that was encrypted with an unknown encryption
    /// algorithm.
    Unknown(UnknownEncryptedContent),
}

impl RoomEventEncryptionScheme {
    /// Get the algorithm of the event content.
    pub fn algorithm(&self) -> EventEncryptionAlgorithm {
        match self {
            RoomEventEncryptionScheme::MegolmV1AesSha2(_) => {
                EventEncryptionAlgorithm::MegolmV1AesSha2
            }
            #[cfg(feature = "experimental-algorithms")]
            RoomEventEncryptionScheme::MegolmV2AesSha2(_) => {
                EventEncryptionAlgorithm::MegolmV2AesSha2
            }
            RoomEventEncryptionScheme::Unknown(c) => c.algorithm.to_owned(),
        }
    }
}

pub(crate) enum SupportedEventEncryptionSchemes<'a> {
    MegolmV1AesSha2(&'a MegolmV1AesSha2Content),
    #[cfg(feature = "experimental-algorithms")]
    MegolmV2AesSha2(&'a MegolmV2AesSha2Content),
}

impl SupportedEventEncryptionSchemes<'_> {
    /// The ID of the session used to encrypt the message.
    pub fn session_id(&self) -> &str {
        match self {
            SupportedEventEncryptionSchemes::MegolmV1AesSha2(c) => &c.session_id,
            #[cfg(feature = "experimental-algorithms")]
            SupportedEventEncryptionSchemes::MegolmV2AesSha2(c) => &c.session_id,
        }
    }
}

impl<'a> From<&'a MegolmV1AesSha2Content> for SupportedEventEncryptionSchemes<'a> {
    fn from(c: &'a MegolmV1AesSha2Content) -> Self {
        Self::MegolmV1AesSha2(c)
    }
}

#[cfg(feature = "experimental-algorithms")]
impl<'a> From<&'a MegolmV2AesSha2Content> for SupportedEventEncryptionSchemes<'a> {
    fn from(c: &'a MegolmV2AesSha2Content) -> Self {
        Self::MegolmV2AesSha2(c)
    }
}

/// The event content for events encrypted with the m.megolm.v1.aes-sha2
/// algorithm.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MegolmV1AesSha2Content {
    /// The encrypted content of the event.
    pub ciphertext: MegolmMessage,

    /// The Curve25519 key of the sender.
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    pub sender_key: Curve25519PublicKey,

    /// The ID of the sending device.
    pub device_id: OwnedDeviceId,

    /// The ID of the session used to encrypt the message.
    pub session_id: String,
}

/// The event content for events encrypted with the m.megolm.v2.aes-sha2
/// algorithm.
#[cfg(feature = "experimental-algorithms")]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MegolmV2AesSha2Content {
    /// The encrypted content of the event.
    pub ciphertext: MegolmMessage,

    /// The ID of the session used to encrypt the message.
    pub session_id: String,
}

/// An unknown and unsupported `m.room.encrypted` event content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownEncryptedContent {
    /// The algorithm that was used to encrypt the given event content.
    pub algorithm: EventEncryptionAlgorithm,
    /// The other data of the unknown encrypted content.
    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Helper {
    algorithm: EventEncryptionAlgorithm,
    #[serde(flatten)]
    other: Value,
}

macro_rules! scheme_serialization {
    ($something:ident, $($algorithm:ident => $content:ident),+ $(,)?) => {
        $(
            impl From<$content> for $something {
                fn from(c: $content) -> Self {
                    Self::$algorithm(c.into())
                }
            }
        )+

        impl TryFrom<Helper> for $something {
            type Error = serde_json::Error;

            fn try_from(value: Helper) -> Result<Self, Self::Error> {
                Ok(match value.algorithm {
                    $(
                        EventEncryptionAlgorithm::$algorithm => {
                            let content: $content = serde_json::from_value(value.other)?;
                            content.into()
                        }
                    )+
                    _ => Self::Unknown(UnknownEncryptedContent {
                        algorithm: value.algorithm,
                        other: serde_json::from_value(value.other)?,
                    }),
                })
            }
        }

        impl Serialize for $something {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                let helper = match self {
                    $(
                        Self::$algorithm(r) => Helper {
                            algorithm: self.algorithm(),
                            other: serde_json::to_value(r).map_err(serde::ser::Error::custom)?,
                        },
                    )+
                    Self::Unknown(r) => Helper {
                        algorithm: r.algorithm.clone(),
                        other: serde_json::to_value(r.other.clone()).map_err(serde::ser::Error::custom)?,
                    },
                };

                helper.serialize(serializer)
            }
        }
    };
}

#[cfg(feature = "experimental-algorithms")]
scheme_serialization!(
    RoomEventEncryptionScheme,
    MegolmV1AesSha2 => MegolmV1AesSha2Content,
    MegolmV2AesSha2 => MegolmV2AesSha2Content
);

#[cfg(not(feature = "experimental-algorithms"))]
scheme_serialization!(
    RoomEventEncryptionScheme,
    MegolmV1AesSha2 => MegolmV1AesSha2Content,
);

#[cfg(feature = "experimental-algorithms")]
scheme_serialization!(
    ToDeviceEncryptedEventContent,
    OlmV1Curve25519AesSha2 => OlmV1Curve25519AesSha2Content,
    OlmV2Curve25519AesSha2 => OlmV2Curve25519AesSha2Content,
);

#[cfg(not(feature = "experimental-algorithms"))]
scheme_serialization!(
    ToDeviceEncryptedEventContent,
    OlmV1Curve25519AesSha2 => OlmV1Curve25519AesSha2Content,
);

#[cfg(test)]
pub(crate) mod test {
    use assert_matches::assert_matches;
    use serde_json::{json, Value};
    use vodozemac::Curve25519PublicKey;

    use super::{
        EncryptedEvent, EncryptedToDeviceEvent, OlmV1Curve25519AesSha2Content,
        RoomEventEncryptionScheme, ToDeviceEncryptedEventContent,
    };

    pub fn json() -> Value {
        json!({
            "sender": "@alice:example.org",
            "event_id": "$Nhl3rsgHMjk-DjMJANawr9HHAhLg4GcoTYrSiYYGqEE",
            "content": {
                "m.custom": "something custom",
                "algorithm": "m.megolm.v1.aes-sha2",
                "device_id": "DEWRCMENGS",
                "session_id": "ZFD6+OmV7fVCsJ7Gap8UnORH8EnmiAkes8FAvQuCw/I",
                "sender_key": "WJ6Ce7U67a6jqkHYHd8o0+5H4bqdi9hInZdk0+swuXs",
                "ciphertext": "AwgAEiBQs2LgBD2CcB+RLH2bsgp9VadFUJhBXOtCmcJuttBD\
                               OeDNjL21d9z0AcVSfQFAh9huh4or7sWuNrHcvu9/sMbweTgc\
                               0UtdA5xFLheubHouXy4aewze+ShndWAaTbjWJMLsPSQDUMQH\
                               BA",
                "m.relates_to": {
                    "rel_type": "m.reference",
                    "event_id": "$WUreEJERkFzO8i2dk6CmTex01cP1dZ4GWKhKCwkWHrQ"
                },
            },
            "type": "m.room.encrypted",
            "origin_server_ts": 1632491098485u64,
            "m.custom.top": "something custom in the top",
        })
    }

    pub fn olm_v1_json() -> Value {
        json!({
            "algorithm": "m.olm.v1.curve25519-aes-sha2",
            "ciphertext": {
                "Nn0L2hkcCMFKqynTjyGsJbth7QrVmX3lbrksMkrGOAw": {
                    "body": "Awogv7Iysf062hV1gZNfG/SdO5TdLYtkRI12em6LxralPxoSIC\
                             C/Avnha6NfkaMWSC+5h+khS0wHiUzA2bPmAvVo/iYhGiAfDNh4\
                             F0eqPvOc4Hw9wMgd+frzedZgmhUNfKT0UzHQZSJPAwogF8fTdT\
                             cPt1ppJ/KAEivFZ4dIyAlRUjzhlqzYsw9C1HoQACIgb9MK/a9T\
                             RLtwol9gfy7OeKdpmSe39YhP+5OchhKvX6eO3/aED3X1oA",
                    "type": 0
                }
            },
            "sender_key": "mjkTX0I0Cp44ZfolOVbFe5WYPRmT6AX3J0ZbnGWnnWs"
        })
    }

    pub fn to_device_json() -> Value {
        json!({
            "content": olm_v1_json(),
            "sender": "@example:morpheus.localhost",
            "type": "m.room.encrypted"
        })
    }

    #[test]
    fn deserialization() -> Result<(), serde_json::Error> {
        let json = json();
        let event: EncryptedEvent = serde_json::from_value(json.clone())?;

        assert_matches!(event.content.scheme, RoomEventEncryptionScheme::MegolmV1AesSha2(_));
        assert!(event.content.relates_to.is_some());
        let serialized = serde_json::to_value(event)?;
        assert_eq!(json, serialized);

        let json = olm_v1_json();
        let content: OlmV1Curve25519AesSha2Content = serde_json::from_value(json)?;

        assert_eq!(
            content.sender_key,
            Curve25519PublicKey::from_base64("mjkTX0I0Cp44ZfolOVbFe5WYPRmT6AX3J0ZbnGWnnWs")
                .unwrap()
        );

        assert_eq!(
            content.recipient_key,
            Curve25519PublicKey::from_base64("Nn0L2hkcCMFKqynTjyGsJbth7QrVmX3lbrksMkrGOAw")
                .unwrap()
        );

        let json = to_device_json();
        let event: EncryptedToDeviceEvent = serde_json::from_value(json.clone())?;

        let content = assert_matches!(
            &event.content,
            ToDeviceEncryptedEventContent::OlmV1Curve25519AesSha2(c) => c.to_owned()
        );
        assert!(content.message_id.is_undefined());

        let serialized = serde_json::to_value(event)?;
        assert_eq!(json, serialized);

        Ok(())
    }
}
