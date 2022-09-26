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

use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering::SeqCst},
        Arc,
    },
};

use matrix_sdk_common::locks::Mutex;
use ruma::{
    events::{room::history_visibility::HistoryVisibility, AnyTimelineEvent},
    serde::Raw,
    DeviceKeyAlgorithm, OwnedRoomId, RoomId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use vodozemac::{
    megolm::{
        DecryptedMessage, DecryptionError, InboundGroupSession as InnerSession,
        InboundGroupSessionPickle, MegolmMessage, SessionConfig, SessionOrdering,
    },
    Curve25519PublicKey, Ed25519PublicKey, PickleError,
};

use super::{
    BackedUpRoomKey, ExportedRoomKey, OutboundGroupSession, SessionCreationError, SessionKey,
};
use crate::{
    error::{EventError, MegolmResult},
    types::{
        deserialize_curve_key,
        events::{
            forwarded_room_key::ForwardedMegolmV1AesSha2Content,
            room::encrypted::{EncryptedEvent, RoomEventEncryptionScheme},
        },
        serialize_curve_key, EventEncryptionAlgorithm, SigningKeys,
    },
};

// TODO add creation times to the inbound group sessions so we can export
// sessions that were created between some time period, this should only be set
// for non-imported sessions.

/// Inbound group session.
///
/// Inbound group sessions are used to exchange room messages between a group of
/// participants. Inbound group sessions are used to decrypt the room messages.
#[derive(Clone)]
pub struct InboundGroupSession {
    inner: Arc<Mutex<InnerSession>>,
    history_visibility: Arc<Option<HistoryVisibility>>,
    /// The SessionId associated to this GroupSession
    pub session_id: Arc<str>,
    first_known_index: u32,
    /// The sender_key associated to this GroupSession
    pub sender_key: Curve25519PublicKey,
    /// Map of DeviceKeyAlgorithm to the public ed25519 key of the account
    pub signing_keys: Arc<SigningKeys<DeviceKeyAlgorithm>>,
    /// The Room this GroupSession belongs to
    pub room_id: Arc<RoomId>,
    imported: bool,
    algorithm: Arc<EventEncryptionAlgorithm>,
    backed_up: Arc<AtomicBool>,
}

impl InboundGroupSession {
    /// Create a new inbound group session for the given room.
    ///
    /// These sessions are used to decrypt room messages.
    ///
    /// # Arguments
    ///
    /// * `sender_key` - The public curve25519 key of the account that
    /// sent us the session
    ///
    /// * `signing_key` - The public ed25519 key of the account that
    /// sent us the session.
    ///
    /// * `room_id` - The id of the room that the session is used in.
    ///
    /// * `session_key` - The private session key that is used to decrypt
    /// messages.
    pub fn new(
        sender_key: Curve25519PublicKey,
        signing_key: Ed25519PublicKey,
        room_id: &RoomId,
        session_key: &SessionKey,
        encryption_algorithm: EventEncryptionAlgorithm,
        history_visibility: Option<HistoryVisibility>,
    ) -> Result<Self, SessionCreationError> {
        let config = OutboundGroupSession::session_config(&encryption_algorithm)?;

        let session = InnerSession::new(session_key, config);
        let session_id = session.session_id();
        let first_known_index = session.first_known_index();

        let mut keys = SigningKeys::new();
        keys.insert(DeviceKeyAlgorithm::Ed25519, signing_key.into());

        Ok(InboundGroupSession {
            inner: Arc::new(Mutex::new(session)),
            history_visibility: history_visibility.into(),
            session_id: session_id.into(),
            first_known_index,
            sender_key,
            signing_keys: keys.into(),
            room_id: room_id.into(),
            imported: false,
            algorithm: encryption_algorithm.into(),
            backed_up: AtomicBool::new(false).into(),
        })
    }

    /// Create a InboundGroupSession from an exported version of the group
    /// session.
    ///
    /// Most notably this can be called with an `ExportedRoomKey` from a
    /// previous [`export()`] call.
    ///
    /// [`export()`]: #method.export
    pub fn from_export(exported_session: &ExportedRoomKey) -> Result<Self, SessionCreationError> {
        Self::try_from(exported_session)
    }

    #[allow(dead_code)]
    fn from_backup(
        room_id: &RoomId,
        backup: BackedUpRoomKey,
    ) -> Result<Self, SessionCreationError> {
        // We're using this session only to get the session id, the session
        // config doesn't matter here.
        let session = InnerSession::import(&backup.session_key, SessionConfig::default());
        let session_id = session.session_id();

        Self::from_export(&ExportedRoomKey {
            algorithm: backup.algorithm,
            room_id: room_id.to_owned(),
            sender_key: backup.sender_key,
            session_id,
            forwarding_curve25519_key_chain: vec![],
            session_key: backup.session_key,
            sender_claimed_keys: backup.sender_claimed_keys,
        })
    }

    /// Create a new inbound group session from a forwarded room key content.
    ///
    /// # Arguments
    ///
    /// * `sender_key` - The public curve25519 key of the account that
    /// sent us the session
    ///
    /// * `content` - A forwarded room key content that contains the session key
    /// to create the `InboundGroupSession`.
    pub fn from_forwarded_key(
        algorithm: &EventEncryptionAlgorithm,
        content: &ForwardedMegolmV1AesSha2Content,
    ) -> Result<Self, SessionCreationError> {
        let config = OutboundGroupSession::session_config(algorithm)?;

        let session = InnerSession::import(&content.session_key, config);

        let first_known_index = session.first_known_index();

        let mut sender_claimed_key = SigningKeys::new();
        sender_claimed_key.insert(DeviceKeyAlgorithm::Ed25519, content.claimed_ed25519_key.into());

        Ok(InboundGroupSession {
            inner: Mutex::new(session).into(),
            session_id: content.session_id.as_str().into(),
            sender_key: content.claimed_sender_key,
            first_known_index,
            history_visibility: None.into(),
            signing_keys: sender_claimed_key.into(),
            room_id: (*content.room_id).into(),
            imported: true,
            backed_up: AtomicBool::new(false).into(),
            algorithm: algorithm.to_owned().into(),
        })
    }

    /// Store the group session as a base64 encoded string.
    ///
    /// # Arguments
    ///
    /// * `pickle_mode` - The mode that was used to pickle the group session,
    /// either an unencrypted mode or an encrypted using passphrase.
    pub async fn pickle(&self) -> PickledInboundGroupSession {
        let pickle = self.inner.lock().await.pickle();

        PickledInboundGroupSession {
            pickle,
            sender_key: self.sender_key,
            signing_key: (*self.signing_keys).clone(),
            room_id: (*self.room_id).to_owned(),
            imported: self.imported,
            backed_up: self.backed_up(),
            history_visibility: self.history_visibility.as_ref().clone(),
            algorithm: (*self.algorithm).to_owned(),
        }
    }

    /// Export this session at the first known message index.
    ///
    /// If only a limited part of this session should be exported use
    /// [`export_at_index()`](#method.export_at_index).
    pub async fn export(&self) -> ExportedRoomKey {
        self.export_at_index(self.first_known_index()).await
    }

    /// Get the sender key that this session was received from.
    pub fn sender_key(&self) -> Curve25519PublicKey {
        self.sender_key
    }

    /// Has the session been backed up to the server.
    pub fn backed_up(&self) -> bool {
        self.backed_up.load(SeqCst)
    }

    /// Reset the backup state of the inbound group session.
    pub fn reset_backup_state(&self) {
        self.backed_up.store(false, SeqCst)
    }

    /// For testing, allow to manually mark this GroupSession to have been
    /// backed up
    pub fn mark_as_backed_up(&self) {
        self.backed_up.store(true, SeqCst)
    }

    /// Get the map of signing keys this session was received from.
    pub fn signing_keys(&self) -> &SigningKeys<DeviceKeyAlgorithm> {
        &self.signing_keys
    }

    /// Export this session at the given message index.
    pub async fn export_at_index(&self, message_index: u32) -> ExportedRoomKey {
        let message_index = std::cmp::max(self.first_known_index(), message_index);

        let session_key =
            self.inner.lock().await.export_at(message_index).expect("Can't export session");

        ExportedRoomKey {
            algorithm: self.algorithm().to_owned(),
            room_id: (*self.room_id).to_owned(),
            sender_key: self.sender_key,
            session_id: self.session_id().to_owned(),
            forwarding_curve25519_key_chain: vec![],
            sender_claimed_keys: (*self.signing_keys).clone(),
            session_key,
        }
    }

    /// Restore a Session from a previously pickled string.
    ///
    /// Returns the restored group session or a `UnpicklingError` if there
    /// was an error.
    ///
    /// # Arguments
    ///
    /// * `pickle` - The pickled version of the `InboundGroupSession`.
    ///
    /// * `pickle_mode` - The mode that was used to pickle the session, either
    /// an unencrypted mode or an encrypted using passphrase.
    pub fn from_pickle(pickle: PickledInboundGroupSession) -> Result<Self, PickleError> {
        let session: InnerSession = pickle.pickle.into();
        let first_known_index = session.first_known_index();
        let session_id = session.session_id();

        Ok(InboundGroupSession {
            inner: Mutex::new(session).into(),
            session_id: session_id.into(),
            sender_key: pickle.sender_key,
            history_visibility: pickle.history_visibility.into(),
            first_known_index,
            signing_keys: pickle.signing_key.into(),
            room_id: (*pickle.room_id).into(),
            backed_up: AtomicBool::from(pickle.backed_up).into(),
            algorithm: pickle.algorithm.into(),
            imported: pickle.imported,
        })
    }

    /// The room where this session is used in.
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Returns the unique identifier for this session.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The algorithm that this inbound group session is using to decrypt
    /// events.
    pub fn algorithm(&self) -> &EventEncryptionAlgorithm {
        &self.algorithm
    }

    /// Get the first message index we know how to decrypt.
    pub fn first_known_index(&self) -> u32 {
        self.first_known_index
    }

    /// Has the session been imported from a file or server-side backup? As
    /// opposed to being directly received as an `m.room_key` event.
    pub fn has_been_imported(&self) -> bool {
        self.imported
    }

    /// Check if the `InboundGroupSession` is better than the given other
    /// `InboundGroupSession`
    pub async fn compare(&self, other: &InboundGroupSession) -> SessionOrdering {
        let mut other = other.inner.lock().await;
        self.inner.lock().await.compare(&mut other)
    }

    /// Decrypt the given ciphertext.
    ///
    /// Returns the decrypted plaintext or an `DecryptionError` if
    /// decryption failed.
    ///
    /// # Arguments
    ///
    /// * `message` - The message that should be decrypted.
    pub(crate) async fn decrypt_helper(
        &self,
        message: &MegolmMessage,
    ) -> Result<DecryptedMessage, DecryptionError> {
        self.inner.lock().await.decrypt(message)
    }

    /// Export the inbound group session into a format that can be uploaded to
    /// the server as a backup.
    #[cfg(feature = "backups_v1")]
    pub async fn to_backup(&self) -> BackedUpRoomKey {
        self.export().await.into()
    }

    /// Decrypt an event from a room timeline.
    ///
    /// # Arguments
    ///
    /// * `event` - The event that should be decrypted.
    pub async fn decrypt(
        &self,
        event: &EncryptedEvent,
    ) -> MegolmResult<(Raw<AnyTimelineEvent>, u32)> {
        let decrypted = match &event.content.scheme {
            RoomEventEncryptionScheme::MegolmV1AesSha2(c) => {
                self.decrypt_helper(&c.ciphertext).await?
            }
            #[cfg(feature = "experimental-algorithms")]
            RoomEventEncryptionScheme::MegolmV2AesSha2(c) => {
                self.decrypt_helper(&c.ciphertext).await?
            }
            RoomEventEncryptionScheme::Unknown(_) => {
                return Err(EventError::UnsupportedAlgorithm.into());
            }
        };

        let plaintext = String::from_utf8_lossy(&decrypted.plaintext);

        let mut decrypted_value = serde_json::from_str::<Value>(&plaintext)?;
        let decrypted_object = decrypted_value.as_object_mut().ok_or(EventError::NotAnObject)?;

        let server_ts: i64 = event.origin_server_ts.0.into();

        decrypted_object.insert("sender".to_owned(), event.sender.to_string().into());
        decrypted_object.insert("event_id".to_owned(), event.event_id.to_string().into());
        decrypted_object.insert("origin_server_ts".to_owned(), server_ts.into());

        let room_id = decrypted_object
            .get("room_id")
            .and_then(|r| r.as_str().and_then(|r| RoomId::parse(r).ok()));

        // Check that we have a room id and that the event wasn't forwarded from
        // another room.
        if room_id.as_deref() != Some(self.room_id()) {
            return Err(EventError::MismatchedRoom(self.room_id().to_owned(), room_id).into());
        }

        decrypted_object.insert(
            "unsigned".to_owned(),
            serde_json::to_value(&event.unsigned).unwrap_or_default(),
        );

        if let Some(decrypted_content) =
            decrypted_object.get_mut("content").and_then(|c| c.as_object_mut())
        {
            if !decrypted_content.contains_key("m.relates_to") {
                if let Some(relation) = &event.content.relates_to {
                    decrypted_content.insert("m.relates_to".to_owned(), relation.to_owned());
                }
            }
        }

        Ok((
            serde_json::from_value::<Raw<AnyTimelineEvent>>(decrypted_value)?,
            decrypted.message_index,
        ))
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for InboundGroupSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InboundGroupSession").field("session_id", &self.session_id()).finish()
    }
}

impl PartialEq for InboundGroupSession {
    fn eq(&self, other: &Self) -> bool {
        self.session_id() == other.session_id()
    }
}

/// A pickled version of an `InboundGroupSession`.
///
/// Holds all the information that needs to be stored in a database to restore
/// an InboundGroupSession.
#[derive(Serialize, Deserialize)]
#[allow(missing_debug_implementations)]
pub struct PickledInboundGroupSession {
    /// The pickle string holding the InboundGroupSession.
    pub pickle: InboundGroupSessionPickle,
    /// The public curve25519 key of the account that sent us the session
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    pub sender_key: Curve25519PublicKey,
    /// The public ed25519 key of the account that sent us the session.
    pub signing_key: SigningKeys<DeviceKeyAlgorithm>,
    /// The id of the room that the session is used in.
    pub room_id: OwnedRoomId,
    /// Flag remembering if the session was directly sent to us by the sender
    /// or if it was imported.
    pub imported: bool,
    /// Flag remembering if the session has been backed up.
    #[serde(default)]
    pub backed_up: bool,
    /// History visibility of the room when the session was created.
    pub history_visibility: Option<HistoryVisibility>,
    /// The algorithm of this inbound group session.
    #[serde(default = "default_algorithm")]
    pub algorithm: EventEncryptionAlgorithm,
}

fn default_algorithm() -> EventEncryptionAlgorithm {
    EventEncryptionAlgorithm::MegolmV1AesSha2
}

impl TryFrom<&ExportedRoomKey> for InboundGroupSession {
    type Error = SessionCreationError;

    fn try_from(key: &ExportedRoomKey) -> Result<Self, Self::Error> {
        let config = OutboundGroupSession::session_config(&key.algorithm)?;
        let session = InnerSession::import(&key.session_key, config);
        let first_known_index = session.first_known_index();

        Ok(InboundGroupSession {
            inner: Mutex::new(session).into(),
            session_id: key.session_id.to_owned().into(),
            sender_key: key.sender_key,
            history_visibility: None.into(),
            first_known_index,
            signing_keys: key.sender_claimed_keys.to_owned().into(),
            room_id: key.room_id.to_owned().into(),
            imported: true,
            algorithm: key.algorithm.to_owned().into(),
            backed_up: AtomicBool::from(false).into(),
        })
    }
}

#[cfg(test)]
mod test {
    use matrix_sdk_test::async_test;

    use crate::olm::InboundGroupSession;

    #[async_test]
    async fn inbound_group_session_serialization() {
        let pickle = r#"
        {
            "pickle": {
                "initial_ratchet": {
                    "inner": [ 124, 251, 213, 204, 108, 247, 54, 7, 179, 162, 15, 107, 154, 215,
                               220, 46, 123, 113, 120, 162, 225, 246, 237, 203, 125, 102, 190, 212,
                               229, 195, 136, 185, 26, 31, 77, 140, 144, 181, 152, 177, 46, 105,
                               202, 6, 53, 158, 157, 170, 31, 155, 130, 87, 214, 110, 143, 55, 68,
                               138, 41, 35, 242, 230, 194, 15, 16, 145, 116, 94, 89, 35, 79, 145,
                               245, 117, 204, 173, 166, 178, 49, 131, 143, 61, 61, 15, 211, 167, 17,
                               2, 79, 110, 149, 200, 223, 23, 185, 200, 29, 64, 55, 39, 147, 167,
                               205, 224, 159, 101, 218, 249, 203, 30, 175, 174, 48, 252, 40, 131,
                               52, 135, 91, 57, 211, 96, 105, 58, 55, 68, 250, 24 ],
                    "counter": 0
                },
                "signing_key": [ 93, 185, 171, 61, 173, 100, 51, 9, 157, 180, 214, 39, 131, 80, 118,
                                 130, 199, 232, 163, 197, 45, 23, 227, 100, 151, 59, 19, 102, 38,
                                 149, 43, 38 ],
                "signing_key_verified": true,
                "config": {
                  "version": "V1"
                }
            },
            "sender_key": "AmM1DvVJarsNNXVuX7OarzfT481N37GtDwvDVF0RcR8",
            "signing_key": {
                "ed25519": "wTRTdz4rn4EY+68cKPzpMdQ6RAlg7T8cbTmEjaXuUww"
            },
            "room_id": "!test:localhost",
            "forwarding_chains": ["tb6kQKjk+SJl2KnfQ0lKVOZl6gDFMcsb9HcUP9k/4hc"],
            "imported": false,
            "backed_up": false,
            "history_visibility": "shared",
            "algorithm": "m.megolm.v1.aes-sha2"
        }
        "#;

        let deserialized = serde_json::from_str(pickle).unwrap();

        let unpickled = InboundGroupSession::from_pickle(deserialized).unwrap();

        assert_eq!(unpickled.session_id(), "XbmrPa1kMwmdtNYng1B2gsfoo8UtF+NklzsTZiaVKyY");
    }
}
