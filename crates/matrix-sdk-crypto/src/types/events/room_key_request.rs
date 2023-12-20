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

//! Types for the `m.room_key_request` events.

use std::collections::BTreeMap;

use ruma::{OwnedDeviceId, OwnedRoomId, OwnedTransactionId, RoomId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use vodozemac::Curve25519PublicKey;

use super::{EventType, ToDeviceEvent};
use crate::types::{deserialize_curve_key, serialize_curve_key, EventEncryptionAlgorithm};

/// The `m.room_key_request` to-device event.
pub type RoomKeyRequestEvent = ToDeviceEvent<RoomKeyRequestContent>;

impl Clone for RoomKeyRequestEvent {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            content: self.content.clone(),
            other: self.other.clone(),
        }
    }
}

/// The content for a `m.room_key_request` event.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RoomKeyRequestContent {
    /// The action that this room key request is carrying.
    #[serde(flatten)]
    pub action: Action,
    /// The ID of the device that is requesting the room key.
    pub requesting_device_id: OwnedDeviceId,
    /// A random string uniquely identifying the request for a key. If the key
    /// is requested multiple times, it should be reused. It should also reused
    /// in order to cancel a request.
    pub request_id: OwnedTransactionId,
}

impl RoomKeyRequestContent {
    /// Create a new content for a `m.room_key_request` event with the action
    /// set to request a room key with the given `RequestedKeyInfo`.
    pub fn new_request(
        info: RequestedKeyInfo,
        requesting_device_id: OwnedDeviceId,
        request_id: OwnedTransactionId,
    ) -> Self {
        Self { action: Action::Request(info), requesting_device_id, request_id }
    }
}

impl EventType for RoomKeyRequestContent {
    const EVENT_TYPE: &'static str = "m.room_key_request";
}

/// Enum describing different actions a room key request event can be carrying.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "action", content = "body")]
pub enum Action {
    /// An action describing a cancellation of a previous room key request.
    #[serde(rename = "request_cancellation")]
    Cancellation,
    /// An action describing a room key request.
    #[serde(rename = "request")]
    Request(RequestedKeyInfo),
}

/// Info about the room key that is being requested.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(try_from = "RequestedKeyInfoHelper")]
pub enum RequestedKeyInfo {
    /// The `m.megolm.v1.aes-sha2` variant of the `m.room_key_request` content.
    MegolmV1AesSha2(MegolmV1AesSha2Content),
    /// The `m.megolm.v2.aes-sha2` variant of the `m.room_key_request` content.
    #[cfg(feature = "experimental-algorithms")]
    MegolmV2AesSha2(MegolmV2AesSha2Content),
    /// An unknown and unsupported variant of the `m.room_key_request`
    /// content.
    Unknown(UnknownRoomKeyRequest),
}

impl RequestedKeyInfo {
    /// Get the algorithm of the room key request content.
    pub fn algorithm(&self) -> EventEncryptionAlgorithm {
        match self {
            RequestedKeyInfo::MegolmV1AesSha2(_) => EventEncryptionAlgorithm::MegolmV1AesSha2,
            #[cfg(feature = "experimental-algorithms")]
            RequestedKeyInfo::MegolmV2AesSha2(_) => EventEncryptionAlgorithm::MegolmV2AesSha2,
            RequestedKeyInfo::Unknown(c) => c.algorithm.to_owned(),
        }
    }
}

impl From<SupportedKeyInfo> for RequestedKeyInfo {
    fn from(s: SupportedKeyInfo) -> Self {
        match s {
            SupportedKeyInfo::MegolmV1AesSha2(c) => Self::MegolmV1AesSha2(c),
            #[cfg(feature = "experimental-algorithms")]
            SupportedKeyInfo::MegolmV2AesSha2(c) => Self::MegolmV2AesSha2(c),
        }
    }
}

impl From<MegolmV1AesSha2Content> for RequestedKeyInfo {
    fn from(c: MegolmV1AesSha2Content) -> Self {
        Self::MegolmV1AesSha2(c)
    }
}

#[cfg(feature = "experimental-algorithms")]
impl From<MegolmV2AesSha2Content> for RequestedKeyInfo {
    fn from(c: MegolmV2AesSha2Content) -> Self {
        Self::MegolmV2AesSha2(c)
    }
}

/// Info about the room key that is being requested.
///
/// This is similar to [`RequestedKeyInfo`] but it contains only supported
/// algorithms.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(try_from = "RequestedKeyInfo", into = "RequestedKeyInfo")]
pub enum SupportedKeyInfo {
    /// The `m.megolm.v1.aes-sha2` variant of the `m.room_key_request` content.
    MegolmV1AesSha2(MegolmV1AesSha2Content),
    /// The `m.megolm.v2.aes-sha2` variant of the `m.room_key_request` content.
    #[cfg(feature = "experimental-algorithms")]
    MegolmV2AesSha2(MegolmV2AesSha2Content),
}

impl SupportedKeyInfo {
    /// Get the algorithm of the room key request content.
    pub fn algorithm(&self) -> EventEncryptionAlgorithm {
        match self {
            Self::MegolmV1AesSha2(_) => EventEncryptionAlgorithm::MegolmV1AesSha2,
            #[cfg(feature = "experimental-algorithms")]
            Self::MegolmV2AesSha2(_) => EventEncryptionAlgorithm::MegolmV2AesSha2,
        }
    }

    /// Get the room ID where the room key is used.
    pub fn room_id(&self) -> &RoomId {
        match self {
            Self::MegolmV1AesSha2(c) => &c.room_id,
            #[cfg(feature = "experimental-algorithms")]
            Self::MegolmV2AesSha2(c) => &c.room_id,
        }
    }

    /// Get the unique ID of the room key.
    pub fn session_id(&self) -> &str {
        match self {
            Self::MegolmV1AesSha2(c) => &c.session_id,
            #[cfg(feature = "experimental-algorithms")]
            Self::MegolmV2AesSha2(c) => &c.session_id,
        }
    }
}

impl TryFrom<RequestedKeyInfo> for SupportedKeyInfo {
    type Error = &'static str;

    fn try_from(value: RequestedKeyInfo) -> Result<Self, Self::Error> {
        match value {
            RequestedKeyInfo::MegolmV1AesSha2(c) => Ok(Self::MegolmV1AesSha2(c)),
            #[cfg(feature = "experimental-algorithms")]
            RequestedKeyInfo::MegolmV2AesSha2(c) => Ok(Self::MegolmV2AesSha2(c)),
            RequestedKeyInfo::Unknown(_) => Err("Unsupported algorithm for a room key request"),
        }
    }
}

impl From<MegolmV1AesSha2Content> for SupportedKeyInfo {
    fn from(c: MegolmV1AesSha2Content) -> Self {
        Self::MegolmV1AesSha2(c)
    }
}

#[cfg(feature = "experimental-algorithms")]
impl From<MegolmV2AesSha2Content> for SupportedKeyInfo {
    fn from(c: MegolmV2AesSha2Content) -> Self {
        Self::MegolmV2AesSha2(c)
    }
}

/// The content for a `m.megolm.v2.aes-sha2` `m.room_key_request` event.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MegolmV1AesSha2Content {
    /// The room where the key is used.
    pub room_id: OwnedRoomId,

    /// The Curve25519 key of the device which initiated the session originally.
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    pub sender_key: Curve25519PublicKey,

    /// The ID of the session that the key is for.
    pub session_id: String,
}

/// The content for a `m.megolm.v1.aes-sha2` `m.room_key_request` event.
#[cfg(feature = "experimental-algorithms")]
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MegolmV2AesSha2Content {
    /// The room where the key is used.
    pub room_id: OwnedRoomId,

    /// The ID of the session that the key is for.
    pub session_id: String,
}

/// An unknown and unsupported `m.room_key_request` algorithm.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnknownRoomKeyRequest {
    /// The algorithm of the unknown room key request.
    pub algorithm: EventEncryptionAlgorithm,

    /// The other data of the unknown room key request.
    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RequestedKeyInfoHelper {
    pub algorithm: EventEncryptionAlgorithm,
    #[serde(flatten)]
    other: Value,
}

impl Serialize for RequestedKeyInfo {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let other = match self {
            Self::MegolmV1AesSha2(r) => {
                serde_json::to_value(r).map_err(serde::ser::Error::custom)?
            }
            #[cfg(feature = "experimental-algorithms")]
            Self::MegolmV2AesSha2(r) => {
                serde_json::to_value(r).map_err(serde::ser::Error::custom)?
            }
            Self::Unknown(r) => {
                serde_json::to_value(r.other.clone()).map_err(serde::ser::Error::custom)?
            }
        };

        let helper = RequestedKeyInfoHelper { algorithm: self.algorithm(), other };

        helper.serialize(serializer)
    }
}

impl TryFrom<RequestedKeyInfoHelper> for RequestedKeyInfo {
    type Error = serde_json::Error;

    fn try_from(value: RequestedKeyInfoHelper) -> Result<Self, Self::Error> {
        Ok(match value.algorithm {
            EventEncryptionAlgorithm::MegolmV1AesSha2 => {
                Self::MegolmV1AesSha2(serde_json::from_value(value.other)?)
            }
            #[cfg(feature = "experimental-algorithms")]
            EventEncryptionAlgorithm::MegolmV2AesSha2 => {
                Self::MegolmV2AesSha2(serde_json::from_value(value.other)?)
            }
            _ => Self::Unknown(UnknownRoomKeyRequest {
                algorithm: value.algorithm,
                other: serde_json::from_value(value.other)?,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use serde_json::{json, Value};

    use super::{Action, RequestedKeyInfo, RoomKeyRequestEvent};

    pub fn json() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
                "action": "request",
                "body": {
                    "algorithm": "m.megolm.v1.aes-sha2",
                    "room_id": "!Cuyf34gef24t:localhost",
                    "sender_key": "RF3s+E7RkTQTGF2d8Deol0FkQvgII2aJDf3/Jp5mxVU",
                    "session_id": "X3lUlvLELLYxeTx4yOVu6UDpasGEVO0Jbu+QFnm0cKQ"
                },
                "request_id": "1495474790150.19",
                "requesting_device_id": "RJYKSTBOIE"
            },
            "type": "m.room_key_request"
        })
    }

    pub fn json_cancellation() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
                "action": "request_cancellation",
                "request_id": "1495474790150.19",
                "requesting_device_id": "RJYKSTBOIE"
            },
            "type": "m.room_key_request"
        })
    }

    #[cfg(feature = "experimental-algorithms")]
    pub fn json_megolm_v2() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
                "action": "request",
                "body": {
                    "algorithm": "m.megolm.v2.aes-sha2",
                    "room_id": "!Cuyf34gef24t:localhost",
                    "session_id": "X3lUlvLELLYxeTx4yOVu6UDpasGEVO0Jbu+QFnm0cKQ"
                },
                "request_id": "1495474790150.19",
                "requesting_device_id": "RJYKSTBOIE"
            },
            "type": "m.room_key_request"
        })
    }

    #[test]
    fn deserialization() -> Result<(), serde_json::Error> {
        let json = json();
        let event: RoomKeyRequestEvent = serde_json::from_value(json.clone())?;

        assert_matches!(
            event.content.action,
            Action::Request(RequestedKeyInfo::MegolmV1AesSha2(_))
        );
        let serialized = serde_json::to_value(event)?;
        assert_eq!(json, serialized);

        let json = json_cancellation();
        let event: RoomKeyRequestEvent = serde_json::from_value(json.clone())?;

        assert_matches!(event.content.action, Action::Cancellation);
        let serialized = serde_json::to_value(event)?;
        assert_eq!(json, serialized);

        Ok(())
    }

    #[test]
    #[cfg(feature = "experimental-algorithms")]
    fn deserialization_megolm_v2() -> Result<(), serde_json::Error> {
        let json = json_megolm_v2();
        let event: RoomKeyRequestEvent = serde_json::from_value(json.clone())?;

        assert_matches!(
            event.content.action,
            Action::Request(RequestedKeyInfo::MegolmV2AesSha2(_))
        );

        let serialized = serde_json::to_value(event)?;
        assert_eq!(json, serialized);

        Ok(())
    }
}
