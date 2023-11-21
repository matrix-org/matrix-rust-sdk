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

mod inbound;
mod outbound;

pub use inbound::{InboundGroupSession, PickledInboundGroupSession};
pub(crate) use outbound::ShareState;
pub use outbound::{
    EncryptionSettings, OutboundGroupSession, PickledOutboundGroupSession, ShareInfo,
};
use thiserror::Error;
pub use vodozemac::megolm::{ExportedSessionKey, SessionKey};
use vodozemac::{megolm::SessionKeyDecodeError, Curve25519PublicKey};

#[cfg(feature = "experimental-algorithms")]
use crate::types::events::forwarded_room_key::ForwardedMegolmV2AesSha2Content;
use crate::types::{
    deserialize_curve_key, deserialize_curve_key_vec,
    events::forwarded_room_key::{ForwardedMegolmV1AesSha2Content, ForwardedRoomKeyContent},
    serialize_curve_key, serialize_curve_key_vec, EventEncryptionAlgorithm, SigningKey,
    SigningKeys,
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

/// An exported version of an `InboundGroupSession`
///
/// This can be used to share the `InboundGroupSession` in an exported file.
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
}

impl ExportedRoomKey {
    pub(crate) fn from_backed_up_room_key(
        room_id: OwnedRoomId,
        session_id: String,
        room_key: BackedUpRoomKey,
    ) -> Self {
        Self {
            algorithm: room_key.algorithm,
            room_id,
            sender_key: room_key.sender_key,
            session_id,
            session_key: room_key.session_key,
            sender_claimed_keys: room_key.sender_claimed_keys,
            forwarding_curve25519_key_chain: room_key.forwarding_curve25519_key_chain,
        }
    }
}

/// A backed up version of an `InboundGroupSession`
///
/// This can be used to backup the `InboundGroupSession` to the server.
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
    /// m.forwarded_room_key events.
    pub forwarding_curve25519_key_chain: Vec<Curve25519PublicKey>,
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
    fn from(k: ExportedRoomKey) -> Self {
        Self {
            algorithm: k.algorithm,
            sender_key: k.sender_key,
            session_key: k.session_key,
            sender_claimed_keys: k.sender_claimed_keys,
            forwarding_curve25519_key_chain: k.forwarding_curve25519_key_chain,
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
            }),
            ForwardedRoomKeyContent::Unknown(c) => Err(SessionExportError::Algorithm(c.algorithm)),
        }
    }
}
