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

//! Types for `m.room.encrypted` to-device events.

use serde::{Deserialize, Serialize};

use crate::types::{
    events::{
        room::scheme::{
            scheme_serialization, Helper, OlmV1Curve25519AesSha2Content, UnknownEncryptedContent,
        },
        EventType, ToDeviceEvent,
    },
    EventEncryptionAlgorithm,
};

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
pub(crate) mod tests {
    use assert_matches2::assert_let;
    use serde_json::{json, Value};
    use vodozemac::Curve25519PublicKey;

    use crate::types::events::room::{
        scheme::OlmV1Curve25519AesSha2Content,
        to_device::{EncryptedToDeviceEvent, ToDeviceEncryptedEventContent},
    };

    pub fn olm_v1_json() -> Value {
        json!({
            "algorithm": "m.olm.v1.curve25519-aes-sha2",
            "ciphertext": {
                "Nn0L2hkcCMFKqynTjyGsJbth7QrVmX3lbrksMkrGOAw": {
                    "body":
                        "Awogv7Iysf062hV1gZNfG/SdO5TdLYtkRI12em6LxralPxoSICC/Av\
                         nha6NfkaMWSC+5h+khS0wHiUzA2bPmAvVo/iYhGiAfDNh4F0eqPvOc\
                         4Hw9wMgd+frzedZgmhUNfKT0UzHQZSJPAwogF8fTdTcPt1ppJ/KAEi\
                         vFZ4dIyAlRUjzhlqzYsw9C1HoQACIgb9MK/a9TRLtwol9gfy7OeKdp\
                         mSe39YhP+5OchhKvX6eO3/aED3X1oA",
                    "type": 0
                }
            },
            "sender_key": "mjkTX0I0Cp44ZfolOVbFe5WYPRmT6AX3J0ZbnGWnnWs"
        })
    }

    pub fn json() -> Value {
        json!({
            "content": olm_v1_json(),
            "sender": "@example:morpheus.localhost",
            "type": "m.room.encrypted"
        })
    }

    #[test]
    fn deserialization() -> Result<(), serde_json::Error> {
        let olm_json = olm_v1_json();
        let content: OlmV1Curve25519AesSha2Content = serde_json::from_value(olm_json)?;

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

        let json = json();
        let event: EncryptedToDeviceEvent = serde_json::from_value(json.clone())?;

        assert_let!(
            ToDeviceEncryptedEventContent::OlmV1Curve25519AesSha2(content) = &event.content
        );
        assert!(content.message_id.is_none());

        let serialized = serde_json::to_value(event)?;
        assert_eq!(json, serialized);

        Ok(())
    }
}
