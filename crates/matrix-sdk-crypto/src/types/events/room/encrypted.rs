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

//! Types for the `m.room.encrypted` room events. Re-exports for compatibility with existing code.

use std::collections::BTreeMap;

use ruma::RoomId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(not(feature = "experimental-algorithms"))]
use crate::types::events::encryption_schemes::scheme_serialization;
use crate::types::{
    events::{
        encryption_schemes::{Helper, MegolmV1AesSha2Content, UnknownEncryptedContent},
        room::Event,
        room_key_request::{self, SupportedKeyInfo},
        EventType,
    },
    EventEncryptionAlgorithm,
};

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

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use serde_json::{json, Value};

    use crate::types::events::room::encrypted::{EncryptedEvent, RoomEventEncryptionScheme};

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
                "ciphertext":
                    "AwgAEiBQs2LgBD2CcB+RLH2bsgp9VadFUJhBXOtCmcJuttBDOeDNjL21d9\
                     z0AcVSfQFAh9huh4or7sWuNrHcvu9/sMbweTgc0UtdA5xFLheubHouXy4a\
                     ewze+ShndWAaTbjWJMLsPSQDUMQHBA",
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

    #[test]
    fn deserialization() -> Result<(), serde_json::Error> {
        let json = json();
        let event: EncryptedEvent = serde_json::from_value(json.clone())?;

        assert_matches!(event.content.scheme, RoomEventEncryptionScheme::MegolmV1AesSha2(_));
        assert!(event.content.relates_to.is_some());
        let serialized = serde_json::to_value(event)?;
        assert_eq!(json, serialized);

        Ok(())
    }
}
