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

            _ => AnyDecryptedOlmEvent::Custom(from_str(json)?),
        })
    }
}
