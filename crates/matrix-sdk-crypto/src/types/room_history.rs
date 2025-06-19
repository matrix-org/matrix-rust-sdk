/*
Copyright 2025 The Matrix.org Foundation C.I.C.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

//! Types for sharing encrypted room history, per [MSC4268].
//!
//! [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268

use std::fmt::Debug;

use ruma::{DeviceKeyAlgorithm, OwnedRoomId};
use serde::{Deserialize, Serialize};
use vodozemac::{Curve25519PublicKey, megolm::ExportedSessionKey};

use super::RoomKeyExport;
use crate::{
    olm::ExportedRoomKey,
    types::{
        EventEncryptionAlgorithm, SigningKeys, deserialize_curve_key,
        events::room_key_withheld::RoomKeyWithheldContent, serialize_curve_key,
    },
};
#[cfg(doc)]
use crate::{olm::InboundGroupSession, types::events::room_key::RoomKeyContent};

/// A bundle of historic room keys, for sharing encrypted room history, per
/// [MSC4268].
///
/// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
#[derive(Deserialize, Serialize, Debug, Default)]
pub struct RoomKeyBundle {
    /// Keys that we are sharing with the recipient.
    pub room_keys: Vec<HistoricRoomKey>,

    /// Keys that we are *not* sharing with the recipient.
    pub withheld: Vec<RoomKeyWithheldContent>,
}

impl RoomKeyBundle {
    /// Returns true if there is nothing of value in this bundle.
    pub fn is_empty(&self) -> bool {
        self.room_keys.is_empty() && self.withheld.is_empty()
    }
}

/// An [`InboundGroupSession`] for sharing as part of a [`RoomKeyBundle`].
///
/// Note: unlike a room key received via an `m.room_key` message (i.e., a
/// [`RoomKeyContent`]), we have no direct proof that the original sender
/// actually created this session; rather, we have to take the word of
/// whoever sent us this key bundle.
#[derive(Deserialize, Serialize)]
pub struct HistoricRoomKey {
    /// The encryption algorithm that the session uses.
    pub algorithm: EventEncryptionAlgorithm,

    /// The room where the session is used.
    pub room_id: OwnedRoomId,

    /// The Curve25519 key of the device which initiated the session originally,
    /// according to the device that sent us this key.
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    pub sender_key: Curve25519PublicKey,

    /// The ID of the session that the key is for.
    pub session_id: String,

    /// The key for the session.
    pub session_key: ExportedSessionKey,

    /// The Ed25519 key of the device which initiated the session originally,
    /// according to the device that sent us this key.
    #[serde(default)]
    pub sender_claimed_keys: SigningKeys<DeviceKeyAlgorithm>,
}

impl Debug for HistoricRoomKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedRoomKey")
            .field("algorithm", &self.algorithm)
            .field("room_id", &self.room_id)
            .field("sender_key", &self.sender_key)
            .field("session_id", &self.session_id)
            .field("sender_claimed_keys", &self.sender_claimed_keys)
            .finish_non_exhaustive()
    }
}

impl RoomKeyExport for &HistoricRoomKey {
    fn room_id(&self) -> &ruma::RoomId {
        &self.room_id
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn sender_key(&self) -> Curve25519PublicKey {
        self.sender_key
    }
}

impl From<ExportedRoomKey> for HistoricRoomKey {
    fn from(exported_room_key: ExportedRoomKey) -> Self {
        let ExportedRoomKey {
            algorithm,
            room_id,
            sender_key,
            session_id,
            session_key,
            sender_claimed_keys,
            shared_history: _,
            forwarding_curve25519_key_chain: _,
        } = exported_room_key;
        HistoricRoomKey {
            algorithm,
            room_id,
            sender_key,
            session_id,
            session_key,
            sender_claimed_keys,
        }
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_debug_snapshot;
    use ruma::{DeviceKeyAlgorithm, owned_room_id};
    use vodozemac::{
        Curve25519PublicKey, Curve25519SecretKey, Ed25519SecretKey, megolm::ExportedSessionKey,
    };

    use crate::types::{EventEncryptionAlgorithm, room_history::HistoricRoomKey};

    #[test]
    fn test_historic_room_key_debug() {
        let key = HistoricRoomKey {
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
            room_id: owned_room_id!("!room:id"),
            sender_key: Curve25519PublicKey::from(&Curve25519SecretKey::from_slice(b"abcdabcdabcdabcdabcdabcdabcdabcd")),
            session_id: "id1234".to_owned(),
            session_key: ExportedSessionKey::from_base64("AQAAAAC2XHVzsMBKs4QCRElJ92CJKyGtknCSC8HY7cQ7UYwndMKLQAejXLh5UA0l6s736mgctcUMNvELScUWrObdflrHo+vth/gWreXOaCnaSxmyjjKErQwyIYTkUfqbHy40RJfEesLwnN23on9XAkch/iy8R2+Jz7B8zfG01f2Ow2SxPQFnAndcO1ZSD2GmXgedy6n4B20MWI1jGP2wiexOWbFS").unwrap(),
            sender_claimed_keys: vec![(DeviceKeyAlgorithm::Ed25519, Ed25519SecretKey::from_slice(b"abcdabcdabcdabcdabcdabcdabcdabcd").public_key().into())].into_iter().collect(),
        };

        assert_debug_snapshot!(key);
    }
}
