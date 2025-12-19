// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use ruma::{DeviceKeyAlgorithm, OwnedRoomId};
use serde::{Deserialize, Serialize};

mod forwarder_data;
mod inbound;
mod outbound;
mod sender_data;
pub(crate) mod sender_data_finder;

pub use forwarder_data::ForwarderData;
pub use inbound::{InboundGroupSession, PickledInboundGroupSession};
pub(crate) use outbound::ShareState;
pub use outbound::{
    EncryptionSettings, OutboundGroupSession, OutboundGroupSessionEncryptionResult,
    PickledOutboundGroupSession, ShareInfo,
};
pub use sender_data::{KnownSenderData, SenderData, SenderDataType};
use thiserror::Error;
pub use vodozemac::megolm::{ExportedSessionKey, SessionKey};
use vodozemac::{Curve25519PublicKey, megolm::SessionKeyDecodeError};

#[cfg(feature = "experimental-algorithms")]
use crate::types::events::forwarded_room_key::ForwardedMegolmV2AesSha2Content;
use crate::types::{
    EventEncryptionAlgorithm, RoomKeyExport, SigningKey, SigningKeys, deserialize_curve_key,
    deserialize_curve_key_vec,
    events::forwarded_room_key::{ForwardedMegolmV1AesSha2Content, ForwardedRoomKeyContent},
    serialize_curve_key, serialize_curve_key_vec,
};

/// An error type for the creation of group sessions.
#[derive(Debug, Error)]
pub enum SessionCreationError {
    /// The provided algorithm is not supported.
    #[error("The provided algorithm is not supported: {0}")]
    Algorithm(EventEncryptionAlgorithm),
    /// The room key key couldn't be decoded.
    #[error(transparent)]
    Decode(#[from] SessionKeyDecodeError),
}

/// An error type for the export of inbound group sessions.
///
/// Exported inbound group sessions will be either uploaded as backups, sent as
/// `m.forwarded_room_key`s, or exported into a file backup.
#[derive(Debug, Error)]
pub enum SessionExportError {
    /// The provided algorithm is not supported.
    #[error("The provided algorithm is not supported: {0}")]
    Algorithm(EventEncryptionAlgorithm),
    /// The session export is missing a claimed Ed25519 sender key.
    #[error("The provided room key export is missing a claimed Ed25519 sender key")]
    MissingEd25519Key,
}

/// An exported version of an [`InboundGroupSession`].
///
/// This can be used to share the `InboundGroupSession` in an exported file.
///
/// See <https://spec.matrix.org/v1.13/client-server-api/#key-export-format>.
#[derive(Deserialize, Serialize)]
#[allow(missing_debug_implementations)]
pub struct ExportedRoomKey {
    /// The encryption algorithm that the session uses.
    pub algorithm: EventEncryptionAlgorithm,

    /// The room where the session is used.
    pub room_id: OwnedRoomId,

    /// The Curve25519 key of the device which initiated the session originally.
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    pub sender_key: Curve25519PublicKey,

    /// The ID of the session that the key is for.
    pub session_id: String,

    /// The key for the session.
    pub session_key: ExportedSessionKey,

    /// The Ed25519 key of the device which initiated the session originally.
    #[serde(default)]
    pub sender_claimed_keys: SigningKeys<DeviceKeyAlgorithm>,

    /// Chain of Curve25519 keys through which this session was forwarded, via
    /// m.forwarded_room_key events.
    #[serde(
        default,
        deserialize_with = "deserialize_curve_key_vec",
        serialize_with = "serialize_curve_key_vec"
    )]
    pub forwarding_curve25519_key_chain: Vec<Curve25519PublicKey>,

    /// Whether this [`ExportedRoomKey`] can be shared with users who are
    /// invited to the room in the future, allowing access to history, as
    /// defined in [MSC3061].
    ///
    /// [MSC3061]: https://github.com/matrix-org/matrix-spec-proposals/pull/3061
    #[serde(default, rename = "org.matrix.msc3061.shared_history")]
    pub shared_history: bool,
}

impl ExportedRoomKey {
    /// Create an `ExportedRoomKey` from a `BackedUpRoomKey`.
    ///
    /// This can be used when importing the keys from a backup into the store.
    pub fn from_backed_up_room_key(
        room_id: OwnedRoomId,
        session_id: String,
        room_key: BackedUpRoomKey,
    ) -> Self {
        let BackedUpRoomKey {
            algorithm,
            sender_key,
            session_key,
            sender_claimed_keys,
            forwarding_curve25519_key_chain,
            shared_history,
        } = room_key;

        Self {
            algorithm,
            room_id,
            sender_key,
            session_id,
            session_key,
            sender_claimed_keys,
            forwarding_curve25519_key_chain,
            shared_history,
        }
    }
}

impl RoomKeyExport for &ExportedRoomKey {
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

/// A backed up version of an [`InboundGroupSession`].
///
/// This can be used to back up the [`InboundGroupSession`] to the server using
/// [server-side key backups].
///
/// See <https://spec.matrix.org/v1.13/client-server-api/#definition-backedupsessiondata>.
///
/// [server-side key backups]: https://spec.matrix.org/v1.13/client-server-api/#server-side-key-backups
#[derive(Deserialize, Serialize)]
#[allow(missing_debug_implementations)]
pub struct BackedUpRoomKey {
    /// The encryption algorithm that the session uses.
    pub algorithm: EventEncryptionAlgorithm,

    /// The Curve25519 key of the device which initiated the session originally.
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    pub sender_key: Curve25519PublicKey,

    /// The key for the session.
    pub session_key: ExportedSessionKey,

    /// The Ed25519 key of the device which initiated the session originally.
    pub sender_claimed_keys: SigningKeys<DeviceKeyAlgorithm>,

    /// Chain of Curve25519 keys through which this session was forwarded, via
    /// `m.forwarded_room_key` events.
    #[serde(
        default,
        deserialize_with = "deserialize_curve_key_vec",
        serialize_with = "serialize_curve_key_vec"
    )]
    pub forwarding_curve25519_key_chain: Vec<Curve25519PublicKey>,

    /// Whether this [`BackedUpRoomKey`] can be shared with users who are
    /// invited to the room in the future, allowing access to history, as
    /// defined in [MSC3061].
    ///
    /// [MSC3061]: https://github.com/matrix-org/matrix-spec-proposals/pull/3061
    #[serde(default, rename = "org.matrix.msc3061.shared_history")]
    pub shared_history: bool,
}

impl TryFrom<ExportedRoomKey> for ForwardedRoomKeyContent {
    type Error = SessionExportError;

    /// Convert an exported room key into a content for a forwarded room key
    /// event.
    ///
    /// This will fail if the exported room key doesn't contain an Ed25519
    /// claimed sender key.
    fn try_from(room_key: ExportedRoomKey) -> Result<ForwardedRoomKeyContent, Self::Error> {
        match room_key.algorithm {
            EventEncryptionAlgorithm::MegolmV1AesSha2 => {
                // The forwarded room key content only supports a single claimed sender
                // key and it requires it to be a Ed25519 key. This here will be lossy
                // conversion since we're dropping all other key types.
                //
                // This was fixed by the megolm v2 content. Hopefully we'll deprecate megolm v1
                // before we have multiple signing keys.
                if let Some(SigningKey::Ed25519(claimed_ed25519_key)) =
                    room_key.sender_claimed_keys.get(&DeviceKeyAlgorithm::Ed25519)
                {
                    Ok(ForwardedRoomKeyContent::MegolmV1AesSha2(
                        ForwardedMegolmV1AesSha2Content {
                            room_id: room_key.room_id,
                            session_id: room_key.session_id,
                            session_key: room_key.session_key,
                            claimed_sender_key: room_key.sender_key,
                            claimed_ed25519_key: *claimed_ed25519_key,
                            forwarding_curve25519_key_chain: room_key
                                .forwarding_curve25519_key_chain
                                .clone(),
                            other: Default::default(),
                        }
                        .into(),
                    ))
                } else {
                    Err(SessionExportError::MissingEd25519Key)
                }
            }
            #[cfg(feature = "experimental-algorithms")]
            EventEncryptionAlgorithm::MegolmV2AesSha2 => {
                Ok(ForwardedRoomKeyContent::MegolmV2AesSha2(
                    ForwardedMegolmV2AesSha2Content {
                        room_id: room_key.room_id,
                        session_id: room_key.session_id,
                        session_key: room_key.session_key,
                        claimed_sender_key: room_key.sender_key,
                        claimed_signing_keys: room_key.sender_claimed_keys,
                        other: Default::default(),
                    }
                    .into(),
                ))
            }
            _ => Err(SessionExportError::Algorithm(room_key.algorithm)),
        }
    }
}

impl From<ExportedRoomKey> for BackedUpRoomKey {
    fn from(value: ExportedRoomKey) -> Self {
        let ExportedRoomKey {
            algorithm,
            room_id: _,
            sender_key,
            session_id: _,
            session_key,
            sender_claimed_keys,
            forwarding_curve25519_key_chain,
            shared_history,
        } = value;

        Self {
            algorithm,
            sender_key,
            session_key,
            sender_claimed_keys,
            forwarding_curve25519_key_chain,
            shared_history,
        }
    }
}

impl TryFrom<ForwardedRoomKeyContent> for ExportedRoomKey {
    type Error = SessionExportError;

    /// Convert the content of a forwarded room key into a exported room key.
    fn try_from(forwarded_key: ForwardedRoomKeyContent) -> Result<Self, Self::Error> {
        let algorithm = forwarded_key.algorithm();

        match forwarded_key {
            ForwardedRoomKeyContent::MegolmV1AesSha2(content) => {
                let mut sender_claimed_keys = SigningKeys::new();
                sender_claimed_keys
                    .insert(DeviceKeyAlgorithm::Ed25519, content.claimed_ed25519_key.into());

                Ok(Self {
                    algorithm,
                    room_id: content.room_id,
                    session_id: content.session_id,
                    forwarding_curve25519_key_chain: content.forwarding_curve25519_key_chain,
                    sender_claimed_keys,
                    sender_key: content.claimed_sender_key,
                    session_key: content.session_key,
                    shared_history: false,
                })
            }
            #[cfg(feature = "experimental-algorithms")]
            ForwardedRoomKeyContent::MegolmV2AesSha2(content) => Ok(Self {
                algorithm,
                room_id: content.room_id,
                session_id: content.session_id,
                forwarding_curve25519_key_chain: Default::default(),
                sender_claimed_keys: content.claimed_signing_keys,
                sender_key: content.claimed_sender_key,
                session_key: content.session_key,
                shared_history: false,
            }),
            ForwardedRoomKeyContent::Unknown(c) => Err(SessionExportError::Algorithm(c.algorithm)),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::BackedUpRoomKey;

    #[test]
    fn test_deserialize_backed_up_key() {
        let data = json!({
                "algorithm": "m.megolm.v1.aes-sha2",
                "room_id": "!room:id",
                "sender_key": "FOvlmz18LLI3k/llCpqRoKT90+gFF8YhuL+v1YBXHlw",
                "session_id": "/2K+V777vipCxPZ0gpY9qcpz1DYaXwuMRIu0UEP0Wa0",
                "session_key": "AQAAAAAclzWVMeWBKH+B/WMowa3rb4ma3jEl6n5W4GCs9ue65CruzD3ihX+85pZ9hsV9Bf6fvhjp76WNRajoJYX0UIt7aosjmu0i+H+07hEQ0zqTKpVoSH0ykJ6stAMhdr6Q4uW5crBmdTTBIsqmoWsNJZKKoE2+ldYrZ1lrFeaJbjBIY/9ivle++74qQsT2dIKWPanKc9Q2Gl8LjESLtFBD9Fmt",
                "sender_claimed_keys": {
                    "ed25519": "F4P7f1Z0RjbiZMgHk1xBCG3KC4/Ng9PmxLJ4hQ13sHA"
                },
                "forwarding_curve25519_key_chain": ["DBPC2zr6c9qimo9YRFK3RVr0Two/I6ODb9mbsToZN3Q", "bBc/qzZFOOKshMMT+i4gjS/gWPDoKfGmETs9yfw9430"]
        });

        let backed_up_room_key: BackedUpRoomKey = serde_json::from_value(data)
            .expect("We should be able to deserialize the backed up room key.");
        assert_eq!(
            backed_up_room_key.forwarding_curve25519_key_chain.len(),
            2,
            "The number of forwarding Curve25519 chains should be two."
        );
    }
}
