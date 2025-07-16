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

//! Encryption schemes for `m.room.event`.

use std::collections::BTreeMap;

use ruma::OwnedDeviceId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use vodozemac::{megolm::MegolmMessage, olm::OlmMessage, Curve25519PublicKey};

use crate::types::{deserialize_curve_key, serialize_curve_key, EventEncryptionAlgorithm};

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
    pub message_id: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "org.matrix.msgid")]
    pub message_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct OlmHelper {
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    sender_key: Curve25519PublicKey,
    ciphertext: BTreeMap<String, OlmMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "org.matrix.msgid")]
    message_id: Option<String>,
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

    /// The index of the Megolm ratchet that was used to encrypt the message.
    pub fn message_index(&self) -> u32 {
        match self {
            SupportedEventEncryptionSchemes::MegolmV1AesSha2(c) => c.ciphertext.message_index(),
            #[cfg(feature = "experimental-algorithms")]
            SupportedEventEncryptionSchemes::MegolmV2AesSha2(c) => c.ciphertext.message_index(),
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
    pub other: BTreeMap<String, Value>,
}

/// A deserialisation helper.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Helper {
    pub algorithm: EventEncryptionAlgorithm,
    #[serde(flatten)]
    pub other: Value,
}

// #[macro_export]
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

pub(super) use scheme_serialization;
