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

//! Types for `m.room_key` to-device events.

use std::collections::BTreeMap;

use ruma::{OwnedRoomId, RoomId, serde::Raw};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::to_raw_value};
use vodozemac::megolm::SessionKey;

use super::{EventType, ToDeviceEvent};
#[cfg(doc)]
use crate::olm::InboundGroupSession;
use crate::types::EventEncryptionAlgorithm;

/// The `m.room_key` to-device event.
pub type RoomKeyEvent = ToDeviceEvent<RoomKeyContent>;

impl RoomKeyEvent {
    /// Get the algorithm of the room key.
    pub fn algorithm(&self) -> EventEncryptionAlgorithm {
        self.content.algorithm()
    }
}

impl EventType for RoomKeyContent {
    const EVENT_TYPE: &'static str = "m.room_key";
}

/// The `m.room_key` event content.
///
/// This is an enum over the different room key algorithms we support.  The
/// currently-supported implementations are used to share
/// [`InboundGroupSession`]s.
///
/// This event type is used to exchange keys for end-to-end encryption.
/// Typically, it is encrypted as an m.room.encrypted event, then sent as a
/// to-device event.
///
/// See <https://spec.matrix.org/v1.13/client-server-api/#mroom_key>.
#[derive(Debug, Deserialize)]
#[serde(try_from = "RoomKeyHelper")]
pub enum RoomKeyContent {
    /// The `m.megolm.v1.aes-sha2` variant of the `m.room_key` content.
    MegolmV1AesSha2(Box<MegolmV1AesSha2Content>),
    /// The `m.megolm.v2.aes-sha2` variant of the `m.room_key` content.
    #[cfg(feature = "experimental-algorithms")]
    MegolmV2AesSha2(Box<MegolmV1AesSha2Content>),
    /// An unknown and unsupported variant of the `m.room_key` content.
    Unknown(UnknownRoomKey),
}

impl RoomKeyContent {
    /// Get the algorithm of the room key.
    pub fn algorithm(&self) -> EventEncryptionAlgorithm {
        match &self {
            RoomKeyContent::MegolmV1AesSha2(_) => EventEncryptionAlgorithm::MegolmV1AesSha2,
            #[cfg(feature = "experimental-algorithms")]
            RoomKeyContent::MegolmV2AesSha2(_) => EventEncryptionAlgorithm::MegolmV2AesSha2,
            RoomKeyContent::Unknown(c) => c.algorithm.to_owned(),
        }
    }

    pub(super) fn serialize_zeroized(&self) -> Result<Raw<RoomKeyContent>, serde_json::Error> {
        #[derive(Serialize)]
        struct Helper<'a> {
            pub room_id: &'a RoomId,
            pub session_id: &'a str,
            pub session_key: &'a str,
            #[serde(flatten)]
            other: &'a BTreeMap<String, Value>,
        }

        let serialize_helper = |content: &MegolmV1AesSha2Content| {
            let helper = Helper {
                room_id: &content.room_id,
                session_id: &content.session_id,
                session_key: "",
                other: &content.other,
            };

            let helper =
                RoomKeyHelper { algorithm: self.algorithm(), other: serde_json::to_value(helper)? };

            Ok(Raw::from_json(to_raw_value(&helper)?))
        };

        match self {
            RoomKeyContent::MegolmV1AesSha2(c) => serialize_helper(c),
            #[cfg(feature = "experimental-algorithms")]
            RoomKeyContent::MegolmV2AesSha2(c) => serialize_helper(c),
            RoomKeyContent::Unknown(c) => Ok(Raw::from_json(to_raw_value(&c)?)),
        }
    }
}

/// The `m.megolm.v1.aes-sha2` variant of the `m.room_key` content.
#[derive(Deserialize, Serialize)]
pub struct MegolmV1AesSha2Content {
    /// The room where the key is used.
    pub room_id: OwnedRoomId,
    /// The ID of the session that the key is for.
    pub session_id: String,
    /// The key to be exchanged. Can be used to create a [`InboundGroupSession`]
    /// that can be used to decrypt room events.
    ///
    /// [`InboundGroupSession`]: vodozemac::megolm::InboundGroupSession
    pub session_key: SessionKey,
    /// Whether this room key can be shared with users who are invited to the
    /// room in the future, allowing access to history, as defined in
    /// [MSC3061].
    ///
    /// [MSC3061]: https://github.com/matrix-org/matrix-spec-proposals/pull/3061
    #[serde(default, rename = "org.matrix.msc3061.shared_history")]
    pub shared_history: bool,
    /// Any other, custom and non-specced fields of the content.
    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

impl MegolmV1AesSha2Content {
    /// Create a new `m.megolm.v1.aes-sha2` `m.room_key` content.
    pub fn new(
        room_id: OwnedRoomId,
        session_id: String,
        session_key: SessionKey,
        shared_history: bool,
    ) -> Self {
        Self { room_id, session_id, session_key, other: Default::default(), shared_history }
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for MegolmV1AesSha2Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MegolmV1AesSha2Content")
            .field("room_id", &self.room_id)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

/// The `m.megolm.v2.aes-sha2` variant of the `m.room_key` content.
pub type MegolmV2AesSha2Content = MegolmV1AesSha2Content;

/// An unknown and unsupported `m.room_key` algorithm.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UnknownRoomKey {
    /// The algorithm of the unknown room key.
    pub algorithm: EventEncryptionAlgorithm,
    /// The other data of the unknown room key.
    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

#[derive(Deserialize, Serialize)]
struct RoomKeyHelper {
    algorithm: EventEncryptionAlgorithm,
    #[serde(flatten)]
    other: Value,
}

impl TryFrom<RoomKeyHelper> for RoomKeyContent {
    type Error = serde_json::Error;

    fn try_from(value: RoomKeyHelper) -> Result<Self, Self::Error> {
        Ok(match value.algorithm {
            EventEncryptionAlgorithm::MegolmV1AesSha2 => {
                let content: MegolmV1AesSha2Content = serde_json::from_value(value.other)?;
                Self::MegolmV1AesSha2(content.into())
            }
            #[cfg(feature = "experimental-algorithms")]
            EventEncryptionAlgorithm::MegolmV2AesSha2 => {
                let content: MegolmV2AesSha2Content = serde_json::from_value(value.other)?;
                Self::MegolmV2AesSha2(content.into())
            }
            _ => Self::Unknown(UnknownRoomKey {
                algorithm: value.algorithm,
                other: serde_json::from_value(value.other)?,
            }),
        })
    }
}

impl Serialize for RoomKeyContent {
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

#[cfg(test)]
pub(super) mod tests {
    use assert_matches::assert_matches;
    use serde_json::{Value, json};
    use similar_asserts::assert_eq;

    use super::RoomKeyEvent;
    use crate::types::events::room_key::RoomKeyContent;

    pub fn json() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
                "m.custom": "something custom",
                "algorithm": "m.megolm.v1.aes-sha2",
                "room_id": "!Cuyf34gef24t:localhost",
                "org.matrix.msc3061.shared_history": false,
                "session_id": "ZFD6+OmV7fVCsJ7Gap8UnORH8EnmiAkes8FAvQuCw/I",
                "session_key": "AgAAAADNp1EbxXYOGmJtyX4AkD1bvJvAUyPkbIaKxtnGKjv\
                                SQ3E/4mnuqdM4vsmNzpO1EeWzz1rDkUpYhYE9kP7sJhgLXi\
                                jVv80fMPHfGc49hPdu8A+xnwD4SQiYdFmSWJOIqsxeo/fiH\
                                tino//CDQENtcKuEt0I9s0+Kk4YSH310Szse2RQ+vjple31\
                                QrCexmqfFJzkR/BJ5ogJHrPBQL0LgsPyglIbMTLg7qygIaY\
                                U5Fe2QdKMH7nTZPNIRHh1RaMfHVETAUJBax88EWZBoifk80\
                                gdHUwHSgMk77vCc2a5KHKLDA",
            },
            "type": "m.room_key",
            "m.custom.top": "something custom in the top",
        })
    }

    #[test]
    fn deserialization() -> Result<(), serde_json::Error> {
        let json = json();
        let event: RoomKeyEvent = serde_json::from_value(json.clone())?;

        assert_matches!(event.content, RoomKeyContent::MegolmV1AesSha2(_));
        let serialized = serde_json::to_value(event)?;
        assert_eq!(json, serialized);

        Ok(())
    }
}
