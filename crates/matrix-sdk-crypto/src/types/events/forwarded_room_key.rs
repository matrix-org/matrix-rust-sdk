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

//! Types for `m.forwarded_room_key` to-device events.

use std::collections::BTreeMap;

use ruma::{DeviceKeyAlgorithm, OwnedRoomId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use vodozemac::{Curve25519PublicKey, Ed25519PublicKey, megolm::ExportedSessionKey};

use super::{EventType, ToDeviceEvent};
#[cfg(doc)]
use crate::olm::InboundGroupSession;
use crate::types::{
    EventEncryptionAlgorithm, SigningKeys, deserialize_curve_key, deserialize_curve_key_vec,
    deserialize_ed25519_key, serialize_curve_key, serialize_curve_key_vec, serialize_ed25519_key,
};

/// The `m.forwarded_room_key` to-device event.
pub type ForwardedRoomKeyEvent = ToDeviceEvent<ForwardedRoomKeyContent>;

impl ForwardedRoomKeyEvent {
    /// Get the algorithm of the forwarded room key.
    pub fn algorithm(&self) -> EventEncryptionAlgorithm {
        self.content.algorithm()
    }
}

/// The `m.forwarded_room_key` event content.
///
/// This is an enum over the different room key algorithms we support. The
/// currently-supported implementations are used to share
/// [`InboundGroupSession`]s.
///
/// This event type is used to forward keys for end-to-end encryption.
/// Typically, it is encrypted as an m.room.encrypted event, then sent as a
/// to-device event.
///
/// See <https://spec.matrix.org/v1.13/client-server-api/#mforwarded_room_key>.
#[derive(Debug, Deserialize)]
#[serde(try_from = "RoomKeyHelper")]
pub enum ForwardedRoomKeyContent {
    /// The `m.megolm.v1.aes-sha2` variant of the `m.forwarded_room_key`
    /// content.
    MegolmV1AesSha2(Box<ForwardedMegolmV1AesSha2Content>),
    /// The `m.megolm.v2.aes-sha2` variant of the `m.forwarded_room_key`
    /// content.
    #[cfg(feature = "experimental-algorithms")]
    MegolmV2AesSha2(Box<ForwardedMegolmV2AesSha2Content>),
    /// An unknown and unsupported variant of the `m.forwarded_room_key`
    /// content.
    Unknown(UnknownRoomKeyContent),
}

impl ForwardedRoomKeyContent {
    /// Get the algorithm of the forwarded room key content.
    pub fn algorithm(&self) -> EventEncryptionAlgorithm {
        match self {
            ForwardedRoomKeyContent::MegolmV1AesSha2(_) => {
                EventEncryptionAlgorithm::MegolmV1AesSha2
            }
            #[cfg(feature = "experimental-algorithms")]
            ForwardedRoomKeyContent::MegolmV2AesSha2(_) => {
                EventEncryptionAlgorithm::MegolmV2AesSha2
            }
            ForwardedRoomKeyContent::Unknown(c) => c.algorithm.to_owned(),
        }
    }
}

impl EventType for ForwardedRoomKeyContent {
    const EVENT_TYPE: &'static str = "m.forwarded_room_key";
}

/// The `m.megolm.v1.aes-sha2` variant of the `m.forwarded_room_key` content.
#[derive(Deserialize, Serialize)]
pub struct ForwardedMegolmV1AesSha2Content {
    /// The room where the key is used.
    pub room_id: OwnedRoomId,

    /// The ID of the session that the key is for.
    pub session_id: String,

    /// The key to be exchanged. Can be used to create a [`InboundGroupSession`]
    /// that can be used to decrypt room events.
    ///
    /// [`InboundGroupSession`]: vodozemac::megolm::InboundGroupSession
    pub session_key: ExportedSessionKey,

    /// Chain of Curve25519 keys. It starts out empty, but each time the key is
    /// forwarded to another device, the previous sender in the chain is added
    /// to the end of the list.
    #[serde(
        deserialize_with = "deserialize_curve_key_vec",
        serialize_with = "serialize_curve_key_vec"
    )]
    pub forwarding_curve25519_key_chain: Vec<Curve25519PublicKey>,

    /// The Curve25519 key of the device which initiated the session originally.
    ///
    /// It is ‘claimed’ because the receiving device has no way to tell that
    /// the original room_key actually came from a device which owns the private
    /// part of this key.
    #[serde(
        rename = "sender_key",
        deserialize_with = "deserialize_curve_key",
        serialize_with = "serialize_curve_key"
    )]
    pub claimed_sender_key: Curve25519PublicKey,

    /// The Ed25519 key of the device which initiated the session originally.
    ///
    /// It is ‘claimed’ because the receiving device has no way to tell that
    /// the original room_key actually came from a device which owns the private
    /// part of this key.
    #[serde(
        rename = "sender_claimed_ed25519_key",
        deserialize_with = "deserialize_ed25519_key",
        serialize_with = "serialize_ed25519_key"
    )]
    pub claimed_ed25519_key: Ed25519PublicKey,

    #[serde(flatten)]
    pub(crate) other: BTreeMap<String, Value>,
}

/// The `m.megolm.v2.aes-sha2` variant of the `m.forwarded_room_key` content.
#[derive(Deserialize, Serialize)]
pub struct ForwardedMegolmV2AesSha2Content {
    /// The room where the key is used.
    pub room_id: OwnedRoomId,

    /// The ID of the session that the key is for.
    pub session_id: String,

    /// The key to be exchanged. Can be used to create a [`InboundGroupSession`]
    /// that can be used to decrypt room events.
    ///
    /// [`InboundGroupSession`]: vodozemac::megolm::InboundGroupSession
    pub session_key: ExportedSessionKey,

    /// The Curve25519 key of the device which initiated the session originally.
    ///
    /// It is ‘claimed’ because the receiving device has no way to tell that
    /// the original room_key actually came from a device which owns the private
    /// part of this key.
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    pub claimed_sender_key: Curve25519PublicKey,

    /// The Ed25519 key of the device which initiated the session originally.
    ///
    /// It is ‘claimed’ because the receiving device has no way to tell that
    /// the original room_key actually came from a device which owns the private
    /// part of this key.
    #[serde(default)]
    pub claimed_signing_keys: SigningKeys<DeviceKeyAlgorithm>,

    #[serde(flatten)]
    pub(crate) other: BTreeMap<String, Value>,
}

/// An unknown and unsupported `m.forwarded_room_key` algorithm.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UnknownRoomKeyContent {
    /// The algorithm of the unknown room key.
    pub algorithm: EventEncryptionAlgorithm,
    /// The other data of the unknown room key.
    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for ForwardedMegolmV1AesSha2Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwardedMegolmV1AesSha2Content")
            .field("room_id", &self.room_id)
            .field("session_id", &self.session_id)
            .field("forwarding_curve25519_key_chain", &self.forwarding_curve25519_key_chain)
            .field("claimed_sender_key", &self.claimed_sender_key)
            .field("claimed_ed25519_key", &self.claimed_ed25519_key)
            .finish_non_exhaustive()
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for ForwardedMegolmV2AesSha2Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwardedMegolmV2AesSha2Content")
            .field("room_id", &self.room_id)
            .field("session_id", &self.session_id)
            .field("claimed_sender_key", &self.claimed_sender_key)
            .field("sender_claimed_keys", &self.claimed_signing_keys)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize, Serialize)]
struct RoomKeyHelper {
    algorithm: EventEncryptionAlgorithm,
    #[serde(flatten)]
    other: Value,
}

impl TryFrom<RoomKeyHelper> for ForwardedRoomKeyContent {
    type Error = serde_json::Error;

    fn try_from(value: RoomKeyHelper) -> Result<Self, Self::Error> {
        Ok(match value.algorithm {
            EventEncryptionAlgorithm::MegolmV1AesSha2 => {
                let content: ForwardedMegolmV1AesSha2Content = serde_json::from_value(value.other)?;
                Self::MegolmV1AesSha2(content.into())
            }
            #[cfg(feature = "experimental-algorithms")]
            EventEncryptionAlgorithm::MegolmV2AesSha2 => {
                let content: ForwardedMegolmV2AesSha2Content = serde_json::from_value(value.other)?;
                Self::MegolmV2AesSha2(content.into())
            }
            _ => Self::Unknown(UnknownRoomKeyContent {
                algorithm: value.algorithm,
                other: serde_json::from_value(value.other)?,
            }),
        })
    }
}

impl Serialize for ForwardedRoomKeyContent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let helper = match self {
            Self::MegolmV1AesSha2(r) => RoomKeyHelper {
                algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
                other: serde_json::to_value(r).map_err(serde::ser::Error::custom)?,
            },
            #[cfg(feature = "experimental-algorithms")]
            Self::MegolmV2AesSha2(r) => RoomKeyHelper {
                algorithm: EventEncryptionAlgorithm::MegolmV2AesSha2,
                other: serde_json::to_value(r).map_err(serde::ser::Error::custom)?,
            },
            Self::Unknown(r) => RoomKeyHelper {
                algorithm: r.algorithm.clone(),
                other: serde_json::to_value(r.other.clone()).map_err(serde::ser::Error::custom)?,
            },
        };

        helper.serialize(serializer)
    }
}
