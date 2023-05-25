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

//! Module containing specialized event types that were decrypted using the Olm
//! protocol

use std::fmt::Debug;

use ruma::{OwnedUserId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use vodozemac::Ed25519PublicKey;

use super::{
    dummy::DummyEventContent,
    forwarded_room_key::ForwardedRoomKeyContent,
    room_key::RoomKeyContent,
    room_key_request::{self, SupportedKeyInfo},
    secret_send::SecretSendContent,
    EventType,
};
use crate::types::{deserialize_ed25519_key, events::from_str, serialize_ed25519_key};

/// An `m.dummy` event that was decrypted using the
/// `m.olm.v1.curve25519-aes-sha2` algorithm
pub type DecryptedDummyEvent = DecryptedOlmV1Event<DummyEventContent>;

/// An `m.room_key` event that was decrypted using the
/// `m.olm.v1.curve25519-aes-sha2` algorithm
pub type DecryptedRoomKeyEvent = DecryptedOlmV1Event<RoomKeyContent>;

/// An `m.forwarded_room_key` event that was decrypted using the
/// `m.olm.v1.curve25519-aes-sha2` algorithm
pub type DecryptedForwardedRoomKeyEvent = DecryptedOlmV1Event<ForwardedRoomKeyContent>;

impl DecryptedForwardedRoomKeyEvent {
    /// Get the unique info about the room key that is contained in this
    /// forwarded room key event.
    ///
    /// Returns `None` if we do not understand the algorithm that was used to
    /// encrypt the event.
    pub fn room_key_info(&self) -> Option<SupportedKeyInfo> {
        match &self.content {
            ForwardedRoomKeyContent::MegolmV1AesSha2(c) => Some(
                room_key_request::MegolmV1AesSha2Content {
                    room_id: c.room_id.to_owned(),
                    sender_key: c.claimed_sender_key,
                    session_id: c.session_id.to_owned(),
                }
                .into(),
            ),
            #[cfg(feature = "experimental-algorithms")]
            ForwardedRoomKeyContent::MegolmV2AesSha2(c) => Some(
                room_key_request::MegolmV2AesSha2Content {
                    room_id: c.room_id.to_owned(),
                    session_id: c.session_id.to_owned(),
                }
                .into(),
            ),
            ForwardedRoomKeyContent::Unknown(_) => None,
        }
    }
}

/// An `m.secret.send` event that was decrypted using the
/// `m.olm.v1.curve25519-aes-sha2` algorithm
pub type DecryptedSecretSendEvent = DecryptedOlmV1Event<SecretSendContent>;

/// An enum over the various events that were decrypted using the
/// `m.olm.v1.curve25519-aes-sha2` algorithm.
#[derive(Debug)]
pub enum AnyDecryptedOlmEvent {
    /// The `m.room_key` decrypted to-device event.
    RoomKey(DecryptedRoomKeyEvent),
    /// The `m.forwarded_room_key` decrypted to-device event.
    ForwardedRoomKey(DecryptedForwardedRoomKeyEvent),
    /// The `m.secret.send` decrypted to-device event.
    SecretSend(DecryptedSecretSendEvent),
    /// The `m.dummy` decrypted to-device event.
    Dummy(DecryptedDummyEvent),
    /// A decrypted to-device event of an unknown or custom type.
    Custom(Box<ToDeviceCustomEvent>),
}

impl AnyDecryptedOlmEvent {
    /// The sender of the event, as set by the sender of the event.
    pub fn sender(&self) -> &UserId {
        match self {
            AnyDecryptedOlmEvent::RoomKey(e) => &e.sender,
            AnyDecryptedOlmEvent::ForwardedRoomKey(e) => &e.sender,
            AnyDecryptedOlmEvent::SecretSend(e) => &e.sender,
            AnyDecryptedOlmEvent::Custom(e) => &e.sender,
            AnyDecryptedOlmEvent::Dummy(e) => &e.sender,
        }
    }

    /// The intended recipient of the event, as set by the sender of the event.
    pub fn recipient(&self) -> &UserId {
        match self {
            AnyDecryptedOlmEvent::RoomKey(e) => &e.recipient,
            AnyDecryptedOlmEvent::ForwardedRoomKey(e) => &e.recipient,
            AnyDecryptedOlmEvent::SecretSend(e) => &e.recipient,
            AnyDecryptedOlmEvent::Custom(e) => &e.recipient,
            AnyDecryptedOlmEvent::Dummy(e) => &e.recipient,
        }
    }

    /// The sender's signing keys of the encrypted event.
    pub fn keys(&self) -> &OlmV1Keys {
        match self {
            AnyDecryptedOlmEvent::RoomKey(e) => &e.keys,
            AnyDecryptedOlmEvent::ForwardedRoomKey(e) => &e.keys,
            AnyDecryptedOlmEvent::SecretSend(e) => &e.keys,
            AnyDecryptedOlmEvent::Custom(e) => &e.keys,
            AnyDecryptedOlmEvent::Dummy(e) => &e.keys,
        }
    }

    /// The recipient's signing keys of the encrypted event.
    pub fn recipient_keys(&self) -> &OlmV1Keys {
        match self {
            AnyDecryptedOlmEvent::RoomKey(e) => &e.recipient_keys,
            AnyDecryptedOlmEvent::ForwardedRoomKey(e) => &e.recipient_keys,
            AnyDecryptedOlmEvent::SecretSend(e) => &e.recipient_keys,
            AnyDecryptedOlmEvent::Custom(e) => &e.recipient_keys,
            AnyDecryptedOlmEvent::Dummy(e) => &e.recipient_keys,
        }
    }

    /// The event type of the encrypted event.
    pub fn event_type(&self) -> &str {
        match self {
            AnyDecryptedOlmEvent::Custom(e) => &e.event_type,
            AnyDecryptedOlmEvent::RoomKey(e) => e.content.event_type(),
            AnyDecryptedOlmEvent::ForwardedRoomKey(e) => e.content.event_type(),
            AnyDecryptedOlmEvent::SecretSend(e) => e.content.event_type(),
            AnyDecryptedOlmEvent::Dummy(e) => e.content.event_type(),
        }
    }
}

/// An `m.olm.v1.curve25519-aes-sha2` decrypted to-device event.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DecryptedOlmV1Event<C>
where
    C: EventType + Debug + Sized + Serialize,
{
    /// The sender of the event, as set by the sender of the event.
    pub sender: OwnedUserId,
    /// The intended recipient of the event, as set by the sender of the event.
    pub recipient: OwnedUserId,
    /// The sender's signing keys of the encrypted event.
    pub keys: OlmV1Keys,
    /// The recipient's signing keys of the encrypted event.
    pub recipient_keys: OlmV1Keys,
    /// The type of the event.
    pub content: C,
}

impl<C: EventType + Debug + Sized + Serialize> DecryptedOlmV1Event<C> {
    #[cfg(test)]
    pub fn new(sender: &UserId, recipient: &UserId, key: Ed25519PublicKey, content: C) -> Self {
        Self {
            sender: sender.to_owned(),
            recipient: recipient.to_owned(),
            keys: OlmV1Keys { ed25519: key },
            recipient_keys: OlmV1Keys { ed25519: key },
            content,
        }
    }
}

/// A decrypted to-device event with an unknown type and content.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToDeviceCustomEvent {
    /// The sender of the encrypted to-device event.
    pub sender: OwnedUserId,
    /// The recipient of the encrypted to-device event.
    pub recipient: OwnedUserId,
    /// The sender's signing keys of the encrypted event.
    pub keys: OlmV1Keys,
    /// The recipient's signing keys of the encrypted event.
    pub recipient_keys: OlmV1Keys,
    /// The type of the event.
    #[serde(rename = "type")]
    pub event_type: String,
}

/// Public keys used for an m.olm.v1.curve25519-aes-sha2 event.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OlmV1Keys {
    /// The Ed25519 public key of the `m.olm.v1.curve25519-aes-sha2` keys.
    #[serde(
        deserialize_with = "deserialize_ed25519_key",
        serialize_with = "serialize_ed25519_key"
    )]
    pub ed25519: Ed25519PublicKey,
}

impl<'de> Deserialize<'de> for AnyDecryptedOlmEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Debug, Deserialize)]
        struct Helper<'a> {
            #[serde(rename = "type")]
            event_type: &'a str,
        }

        let json = Box::<RawValue>::deserialize(deserializer)?;
        let helper: Helper<'_> =
            serde_json::from_str(json.get()).map_err(serde::de::Error::custom)?;

        let json = json.get();

        Ok(match helper.event_type {
            "m.room_key" => AnyDecryptedOlmEvent::RoomKey(from_str(json)?),
            "m.forwarded_room_key" => AnyDecryptedOlmEvent::ForwardedRoomKey(from_str(json)?),
            "m.secret.send" => AnyDecryptedOlmEvent::SecretSend(from_str(json)?),
            "m.dummy" => AnyDecryptedOlmEvent::Dummy(from_str(json)?),

            _ => AnyDecryptedOlmEvent::Custom(from_str(json)?),
        })
    }
}

#[cfg(test)]
mod test {
    use assert_matches::assert_matches;
    use serde_json::{json, Value};

    use super::*;

    const ED25519_KEY: &str = "aOfOnlaeMb5GW1TxkZ8pXnblkGMgAvps+lAukrdYaZk";

    fn dummy_event() -> Value {
        json!({
            "sender": "@alice:example.org",
            "sender_device": "DEVICEID",
            "keys": {
                "ed25519": ED25519_KEY,
            },
            "recipient": "@bob:example.org",
            "recipient_keys": {
                "ed25519": ED25519_KEY,
            },
            "content": {},
            "type": "m.dummy"
        })
    }

    fn room_key_event() -> Value {
        json!({
            "sender": "@alice:example.org",
            "sender_device": "DEVICEID",
            "keys": {
                "ed25519": ED25519_KEY,
            },
            "recipient": "@bob:example.org",
            "recipient_keys": {
                "ed25519": ED25519_KEY,
            },
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2",
                "room_id": "!Cuyf34gef24t:localhost",
                "session_id": "ZFD6+OmV7fVCsJ7Gap8UnORH8EnmiAkes8FAvQuCw/I",
                "session_key": "AgAAAADNp1EbxXYOGmJtyX4AkD1bvJvAUyPkbIaKxtnGKjv\
                                SQ3E/4mnuqdM4vsmNzpO1EeWzz1rDkUpYhYE9kP7sJhgLXi\
                                jVv80fMPHfGc49hPdu8A+xnwD4SQiYdFmSWJOIqsxeo/fiH\
                                tino//CDQENtcKuEt0I9s0+Kk4YSH310Szse2RQ+vjple31\
                                QrCexmqfFJzkR/BJ5ogJHrPBQL0LgsPyglIbMTLg7qygIaY\
                                U5Fe2QdKMH7nTZPNIRHh1RaMfHVETAUJBax88EWZBoifk80\
                                gdHUwHSgMk77vCc2a5KHKLDA"
            },
            "type": "m.room_key"
        })
    }

    fn forwarded_room_key_event() -> Value {
        json!({
            "sender": "@alice:example.org",
            "sender_device": "DEVICEID",
            "keys": {
                "ed25519": ED25519_KEY,
            },
            "recipient": "@bob:example.org",
            "recipient_keys": {
                "ed25519": ED25519_KEY,
            },
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2",
                "forwarding_curve25519_key_chain": [
                    "hPQNcabIABgGnx3/ACv/jmMmiQHoeFfuLB17tzWp6Hw"
                ],
                "room_id": "!Cuyf34gef24t:localhost",
                "sender_claimed_ed25519_key": "aj40p+aw64yPIdsxoog8jhPu9i7l7NcFRecuOQblE3Y",
                "sender_key": "RF3s+E7RkTQTGF2d8Deol0FkQvgII2aJDf3/Jp5mxVU",
                "session_id": "X3lUlvLELLYxeTx4yOVu6UDpasGEVO0Jbu+QFnm0cKQ",
                "session_key": "AQAAAAq2JpkMceK5f6JrZPJWwzQTn59zliuIv0F7apVLXDcZCCT\
                                3LqBjD21sULYEO5YTKdpMVhi9i6ZSZhdvZvp//tzRpDT7wpWVWI\
                                00Y3EPEjmpm/HfZ4MMAKpk+tzJVuuvfAcHBZgpnxBGzYOc/DAqa\
                                pK7Tk3t3QJ1UMSD94HfAqlb1JF5QBPwoh0fOvD8pJdanB8zxz05\
                                tKFdR73/vo2Q/zE3"
            },
            "type": "m.forwarded_room_key"
        })
    }

    fn secret_send_event() -> Value {
        json!({
            "sender": "@alice:example.org",
            "sender_device": "DEVICEID",
            "keys": {
                "ed25519": ED25519_KEY,
            },
            "recipient": "@bob:example.org",
            "recipient_keys": {
                "ed25519": ED25519_KEY,
            },
            "content": {
                "request_id": "randomly_generated_id_9573",
                "secret": "ThisIsASecretDon'tTellAnyone"
            },
            "type": "m.secret.send"
        })
    }

    #[test]
    fn deserialization() -> Result<(), serde_json::Error> {
        macro_rules! assert_deserialization_result {
            ( $( $json:path => $to_device_events:ident ),* $(,)? ) => {
                $(
                    let json = $json();
                    let event: AnyDecryptedOlmEvent = serde_json::from_value(json)?;

                    assert_matches!(event, AnyDecryptedOlmEvent::$to_device_events(_));
                )*
            }
        }

        assert_deserialization_result!(
            // `m.room_key
            room_key_event => RoomKey,

            // `m.forwarded_room_key`
            forwarded_room_key_event => ForwardedRoomKey,

            // `m.secret.send`
            secret_send_event => SecretSend,

            // `m.dummy`
            dummy_event => Dummy,
        );

        Ok(())
    }
}
