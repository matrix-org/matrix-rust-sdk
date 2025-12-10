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
    EventType,
    dummy::DummyEventContent,
    forwarded_room_key::ForwardedRoomKeyContent,
    room_key::RoomKeyContent,
    room_key_request::{self, SupportedKeyInfo},
    secret_send::SecretSendContent,
};
use crate::types::{
    DeviceKeys, deserialize_ed25519_key,
    events::{from_str, room_key_bundle::RoomKeyBundleContent},
    serialize_ed25519_key,
};

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
                    sender_key: Some(c.claimed_sender_key),
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

/// An `io.element.msc4268.room_key_bundle` to-device event which has
/// been decrypted using using the `m.olm.v1.curve25519-aes-sha2` algorithm
pub type DecryptedRoomKeyBundleEvent = DecryptedOlmV1Event<RoomKeyBundleContent>;

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
    /// The `io.element.msc4268.room_key_bundle` decrypted to-device event.
    RoomKeyBundle(DecryptedRoomKeyBundleEvent),
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
            AnyDecryptedOlmEvent::RoomKeyBundle(e) => &e.sender,
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
            AnyDecryptedOlmEvent::RoomKeyBundle(e) => &e.recipient,
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
            AnyDecryptedOlmEvent::RoomKeyBundle(e) => &e.keys,
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
            AnyDecryptedOlmEvent::RoomKeyBundle(e) => &e.recipient_keys,
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
            AnyDecryptedOlmEvent::RoomKeyBundle(e) => e.content.event_type(),
            AnyDecryptedOlmEvent::Dummy(e) => e.content.event_type(),
        }
    }

    /// The sender's device keys, if supplied in the message as per MSC4147
    pub fn sender_device_keys(&self) -> Option<&DeviceKeys> {
        match self {
            AnyDecryptedOlmEvent::Custom(e) => e.sender_device_keys.as_ref(),
            AnyDecryptedOlmEvent::RoomKey(e) => e.sender_device_keys.as_ref(),
            AnyDecryptedOlmEvent::ForwardedRoomKey(e) => e.sender_device_keys.as_ref(),
            AnyDecryptedOlmEvent::SecretSend(e) => e.sender_device_keys.as_ref(),
            AnyDecryptedOlmEvent::RoomKeyBundle(e) => e.sender_device_keys.as_ref(),
            AnyDecryptedOlmEvent::Dummy(e) => e.sender_device_keys.as_ref(),
        }
    }
}

/// An `m.olm.v1.curve25519-aes-sha2` decrypted to-device event.
///
/// **Note**: This event will reserialize events lossily; unknown fields will be
/// lost during deserialization.
#[derive(Clone, Debug, Deserialize)]
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
    /// The device keys if supplied as per MSC4147
    #[serde(alias = "org.matrix.msc4147.device_keys")]
    pub sender_device_keys: Option<DeviceKeys>,
    /// The type of the event.
    pub content: C,
}

impl<C: EventType + Debug + Sized + Serialize> Serialize for DecryptedOlmV1Event<C> {
    /// A customized [`Serialize`] implementation that ensures that the
    /// `event_type` field is present in the serialized JSON.
    ///
    /// The `event_type` in the [`DecryptedOlmV1Event`] is omitted because the
    /// event type is expressed in the generic type `C`. To properly serialize
    /// the [`DecryptedOlmV1Event`] we'll must extract the event type from `C`
    /// and reintroduce it into the JSON field.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct DecryptedEventSerializationHelper<'a, C: EventType + Debug + Sized + Serialize> {
            sender: &'a UserId,
            recipient: &'a UserId,
            keys: &'a OlmV1Keys,
            recipient_keys: &'a OlmV1Keys,
            #[serde(skip_serializing_if = "Option::is_none")]
            sender_device_keys: Option<&'a DeviceKeys>,
            content: &'a C,
            #[serde(rename = "type")]
            event_type: &'a str,
        }

        let event_type = self.content.event_type();

        let DecryptedOlmV1Event {
            sender,
            recipient,
            keys,
            recipient_keys,
            sender_device_keys,
            content,
        } = &self;

        let event = DecryptedEventSerializationHelper {
            sender,
            recipient,
            keys,
            recipient_keys,
            sender_device_keys: sender_device_keys.as_ref(),
            content,
            event_type,
        };

        event.serialize(serializer)
    }
}

impl<C: EventType + Debug + Sized + Serialize> DecryptedOlmV1Event<C> {
    #[cfg(test)]
    /// Test helper to create a new [`DecryptedOlmV1Event`] with the given
    /// content.
    ///
    /// This should never be done in real code as we need to deserialize
    /// decrypted events.
    pub fn new(
        sender: &UserId,
        recipient: &UserId,
        key: Ed25519PublicKey,
        device_keys: Option<DeviceKeys>,
        content: C,
    ) -> Self {
        Self {
            sender: sender.to_owned(),
            recipient: recipient.to_owned(),
            keys: OlmV1Keys { ed25519: key },
            recipient_keys: OlmV1Keys { ed25519: key },
            sender_device_keys: device_keys,
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
    /// The device keys if supplied as per MSC4147
    #[serde(alias = "org.matrix.msc4147.device_keys")]
    pub sender_device_keys: Option<DeviceKeys>,
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
            RoomKeyContent::EVENT_TYPE => AnyDecryptedOlmEvent::RoomKey(from_str(json)?),
            ForwardedRoomKeyContent::EVENT_TYPE => {
                AnyDecryptedOlmEvent::ForwardedRoomKey(from_str(json)?)
            }
            SecretSendContent::EVENT_TYPE => AnyDecryptedOlmEvent::SecretSend(from_str(json)?),
            DummyEventContent::EVENT_TYPE => AnyDecryptedOlmEvent::Dummy(from_str(json)?),
            RoomKeyBundleContent::EVENT_TYPE => {
                AnyDecryptedOlmEvent::RoomKeyBundle(from_str(json)?)
            }
            _ => AnyDecryptedOlmEvent::Custom(from_str(json)?),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use assert_matches::assert_matches;
    use insta::{assert_json_snapshot, with_settings};
    use ruma::{KeyId, device_id, owned_user_id};
    use serde_json::{Value, json};
    use similar_asserts::assert_eq;
    use vodozemac::{Curve25519PublicKey, Ed25519PublicKey, Ed25519Signature};

    use super::AnyDecryptedOlmEvent;
    use crate::types::{
        DeviceKey, DeviceKeys, EventEncryptionAlgorithm, Signatures,
        events::olm_v1::DecryptedRoomKeyEvent,
    };

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

    /// Return the JSON for creating sender device keys, and the matching
    /// `DeviceKeys` object that should be created when the JSON is
    /// deserialized.
    fn sender_device_keys() -> (Value, DeviceKeys) {
        let sender_device_keys_json = json!({
                "user_id": "@u:s.co",
                "device_id": "DEV",
                "algorithms": [
                    "m.olm.v1.curve25519-aes-sha2"
                ],
                "keys": {
                    "curve25519:DEV": "c29vb29vb29vb29vb29vb29vb29vb29vb29vb29vb28",
                    "ed25519:DEV": "b29vb29vb29vb29vb29vb29vb29vb29vb29vb29vb28"
                },
                "signatures": {
                    "@u:s.co": {
                        "ed25519:DEV": "mia28GKixFzOWKJ0h7Bdrdy2fjxiHCsst1qpe467FbW85H61UlshtKBoAXfTLlVfi0FX+/noJ8B3noQPnY+9Cg",
                        "ed25519:ssk": "mia28GKixFzOWKJ0h7Bdrdy2fjxiHCsst1qpe467FbW85H61UlshtKBoAXfTLlVfi0FX+/noJ8B3noQPnY+9Cg"
                    }
                }
            }
        );

        let user_id = owned_user_id!("@u:s.co");
        let device_id = device_id!("DEV");
        let ssk_id = device_id!("ssk");

        let ed25519_device_key_id = KeyId::from_parts(ruma::DeviceKeyAlgorithm::Ed25519, device_id);
        let curve25519_device_key_id =
            KeyId::from_parts(ruma::DeviceKeyAlgorithm::Curve25519, device_id);
        let ed25519_ssk_id = KeyId::from_parts(ruma::DeviceKeyAlgorithm::Ed25519, ssk_id);

        let mut keys = BTreeMap::new();
        keys.insert(
            ed25519_device_key_id.clone(),
            DeviceKey::Ed25519(
                Ed25519PublicKey::from_base64("b29vb29vb29vb29vb29vb29vb29vb29vb29vb29vb28")
                    .unwrap(),
            ),
        );
        keys.insert(
            curve25519_device_key_id,
            DeviceKey::Curve25519(
                Curve25519PublicKey::from_base64("c29vb29vb29vb29vb29vb29vb29vb29vb29vb29vb28")
                    .unwrap(),
            ),
        );

        let mut signatures = Signatures::new();
        signatures.add_signature(
            user_id.clone(),
            ed25519_device_key_id,
            Ed25519Signature::from_base64(
                "mia28GKixFzOWKJ0h7Bdrdy2fjxiHCsst1qpe467FbW85H61UlshtKBoAXfTLlVfi0FX+/noJ8B3noQPnY+9Cg"
            ).unwrap()
        );
        signatures. add_signature(
            user_id.clone(),
            ed25519_ssk_id,
            Ed25519Signature::from_base64(
                "mia28GKixFzOWKJ0h7Bdrdy2fjxiHCsst1qpe467FbW85H61UlshtKBoAXfTLlVfi0FX+/noJ8B3noQPnY+9Cg"
            ).unwrap()
        );
        let sender_device_keys = DeviceKeys::new(
            user_id,
            device_id.to_owned(),
            vec![EventEncryptionAlgorithm::OlmV1Curve25519AesSha2],
            keys,
            signatures,
        );

        (sender_device_keys_json, sender_device_keys)
    }

    #[test]
    fn decrypted_to_device_event_snapshot() {
        let event_json = room_key_event();
        let event: DecryptedRoomKeyEvent = serde_json::from_value(event_json)
            .expect("JSON should deserialize to the right event type");

        with_settings!({ sort_maps => true, prepend_module_to_snapshot => false }, {
            assert_json_snapshot!(event);
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
            // `m.room_key`
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

    #[test]
    fn sender_device_keys_are_deserialized_unstable() {
        let (sender_device_keys_json, sender_device_keys) = sender_device_keys();

        // Given JSON for a room key event with sender_device_keys using the unstable
        // prefix
        let mut event_json = room_key_event();
        event_json
            .as_object_mut()
            .unwrap()
            .insert("org.matrix.msc4147.device_keys".to_owned(), sender_device_keys_json);

        // When we deserialize it
        let event: DecryptedRoomKeyEvent = serde_json::from_value(event_json)
            .expect("JSON should deserialize to the right event type");

        // Then it contains the sender_device_keys
        assert_eq!(event.sender_device_keys, Some(sender_device_keys));
    }

    #[test]
    fn sender_device_keys_are_deserialized() {
        let (sender_device_keys_json, sender_device_keys) = sender_device_keys();

        // Given JSON for a room key event with sender_device_keys
        let mut event_json = room_key_event();
        event_json
            .as_object_mut()
            .unwrap()
            .insert("sender_device_keys".to_owned(), sender_device_keys_json);

        // When we deserialize it
        let event: DecryptedRoomKeyEvent = serde_json::from_value(event_json)
            .expect("JSON should deserialize to the right event type");

        // Then it contains the sender_device_keys
        assert_eq!(event.sender_device_keys, Some(sender_device_keys));

        with_settings!({ sort_maps => true, prepend_module_to_snapshot => false }, {
            assert_json_snapshot!(event);
        });
    }

    #[test]
    fn test_serialization_cycle() {
        let event_json = json!({
            "sender": "@alice:example.org",
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
                "org.matrix.msc3061.shared_history": true,
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
        });

        let event: DecryptedRoomKeyEvent = serde_json::from_value(event_json.clone())
            .expect("JSON should deserialize to the right event type");

        let reserialized =
            serde_json::to_value(event).expect("We should be able to serialize the event");

        assert_eq!(
            event_json, reserialized,
            "The reserialized JSON should match the original value"
        );
    }
}
