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
    collections::BTreeMap,
    convert::{TryFrom, TryInto},
    fmt,
    sync::Arc,
};

use matrix_sdk_common::{
    events::{room::encrypted::EncryptedEventContent, AnySyncRoomEvent, SyncMessageEvent},
    identifiers::{DeviceKeyAlgorithm, EventEncryptionAlgorithm, RoomId},
    locks::Mutex,
    Raw,
};
use olm_rs::{
    errors::OlmGroupSessionError, inbound_group_session::OlmInboundGroupSession, PicklingMode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use olm_rs::{
    account::IdentityKeys,
    session::{OlmMessage, PreKeyMessage},
    utility::OlmUtility,
};

use super::{ExportedGroupSessionKey, ExportedRoomKey, GroupSessionKey};
use crate::error::{EventError, MegolmResult};

/// Inbound group session.
///
/// Inbound group sessions are used to exchange room messages between a group of
/// participants. Inbound group sessions are used to decrypt the room messages.
#[derive(Clone)]
pub struct InboundGroupSession {
    inner: Arc<Mutex<OlmInboundGroupSession>>,
    session_id: Arc<String>,
    pub(crate) sender_key: Arc<String>,
    pub(crate) signing_key: Arc<BTreeMap<DeviceKeyAlgorithm, String>>,
    pub(crate) room_id: Arc<RoomId>,
    forwarding_chains: Arc<Mutex<Option<Vec<String>>>>,
    imported: Arc<bool>,
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
        sender_key: &str,
        signing_key: &str,
        room_id: &RoomId,
        session_key: GroupSessionKey,
    ) -> Result<Self, OlmGroupSessionError> {
        let session = OlmInboundGroupSession::new(&session_key.0)?;
        let session_id = session.session_id();

        let mut keys: BTreeMap<DeviceKeyAlgorithm, String> = BTreeMap::new();
        keys.insert(DeviceKeyAlgorithm::Ed25519, signing_key.to_owned());

        Ok(InboundGroupSession {
            inner: Arc::new(Mutex::new(session)),
            session_id: Arc::new(session_id),
            sender_key: Arc::new(sender_key.to_owned()),
            signing_key: Arc::new(keys),
            room_id: Arc::new(room_id.clone()),
            forwarding_chains: Arc::new(Mutex::new(None)),
            imported: Arc::new(false),
        })
    }

    /// Create a InboundGroupSession from an exported version of the group
    /// session.
    ///
    /// Most notably this can be called with an `ExportedRoomKey` from a
    /// previous [`export()`] call.
    ///
    ///
    /// [`export()`]: #method.export
    pub fn from_export(
        exported_session: impl Into<ExportedRoomKey>,
    ) -> Result<Self, OlmGroupSessionError> {
        Self::try_from(exported_session.into())
    }

    /// Store the group session as a base64 encoded string.
    ///
    /// # Arguments
    ///
    /// * `pickle_mode` - The mode that was used to pickle the group session,
    /// either an unencrypted mode or an encrypted using passphrase.
    pub async fn pickle(&self, pickle_mode: PicklingMode) -> PickledInboundGroupSession {
        let pickle = self.inner.lock().await.pickle(pickle_mode);

        PickledInboundGroupSession {
            pickle: InboundGroupSessionPickle::from(pickle),
            sender_key: self.sender_key.to_string(),
            signing_key: (&*self.signing_key).clone(),
            room_id: (&*self.room_id).clone(),
            forwarding_chains: self.forwarding_chains.lock().await.clone(),
            imported: *self.imported,
        }
    }

    /// Export this session at the first known message index.
    ///
    /// If only a limited part of this session should be exported use
    /// [`export_at_index()`](#method.export_at_index).
    pub async fn export(&self) -> ExportedRoomKey {
        self.export_at_index(self.first_known_index().await)
            .await
            .expect("Can't export at the first known index")
    }

    /// Export this session at the given message index.
    pub async fn export_at_index(&self, message_index: u32) -> Option<ExportedRoomKey> {
        let session_key =
            ExportedGroupSessionKey(self.inner.lock().await.export(message_index).ok()?);

        Some(ExportedRoomKey {
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
            room_id: (&*self.room_id).clone(),
            sender_key: (&*self.sender_key).to_owned(),
            session_id: self.session_id().to_owned(),
            forwarding_curve25519_key_chain: self
                .forwarding_chains
                .lock()
                .await
                .as_ref()
                .cloned()
                .unwrap_or_default(),
            sender_claimed_keys: (&*self.signing_key).clone(),
            session_key,
        })
    }

    /// Restore a Session from a previously pickled string.
    ///
    /// Returns the restored group session or a `OlmGroupSessionError` if there
    /// was an error.
    ///
    /// # Arguments
    ///
    /// * `pickle` - The pickled version of the `InboundGroupSession`.
    ///
    /// * `pickle_mode` - The mode that was used to pickle the session, either
    /// an unencrypted mode or an encrypted using passphrase.
    pub fn from_pickle(
        pickle: PickledInboundGroupSession,
        pickle_mode: PicklingMode,
    ) -> Result<Self, OlmGroupSessionError> {
        let session = OlmInboundGroupSession::unpickle(pickle.pickle.0, pickle_mode)?;
        let session_id = session.session_id();

        Ok(InboundGroupSession {
            inner: Arc::new(Mutex::new(session)),
            session_id: Arc::new(session_id),
            sender_key: Arc::new(pickle.sender_key),
            signing_key: Arc::new(pickle.signing_key),
            room_id: Arc::new(pickle.room_id),
            forwarding_chains: Arc::new(Mutex::new(pickle.forwarding_chains)),
            imported: Arc::new(pickle.imported),
        })
    }

    /// Returns the unique identifier for this session.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get the first message index we know how to decrypt.
    pub async fn first_known_index(&self) -> u32 {
        self.inner.lock().await.first_known_index()
    }

    /// Decrypt the given ciphertext.
    ///
    /// Returns the decrypted plaintext or an `OlmGroupSessionError` if
    /// decryption failed.
    ///
    /// # Arguments
    ///
    /// * `message` - The message that should be decrypted.
    pub async fn decrypt_helper(
        &self,
        message: String,
    ) -> Result<(String, u32), OlmGroupSessionError> {
        self.inner.lock().await.decrypt(message)
    }

    /// Decrypt an event from a room timeline.
    ///
    /// # Arguments
    ///
    /// * `event` - The event that should be decrypted.
    pub async fn decrypt(
        &self,
        event: &SyncMessageEvent<EncryptedEventContent>,
    ) -> MegolmResult<(Raw<AnySyncRoomEvent>, u32)> {
        let content = match &event.content {
            EncryptedEventContent::MegolmV1AesSha2(c) => c,
            _ => return Err(EventError::UnsupportedAlgorithm.into()),
        };

        let (plaintext, message_index) = self.decrypt_helper(content.ciphertext.clone()).await?;

        let mut decrypted_value = serde_json::from_str::<Value>(&plaintext)?;
        let decrypted_object = decrypted_value
            .as_object_mut()
            .ok_or(EventError::NotAnObject)?;

        // TODO better number conversion here.
        let server_ts = event
            .origin_server_ts
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let server_ts: i64 = server_ts.try_into().unwrap_or_default();

        decrypted_object.insert("sender".to_owned(), event.sender.to_string().into());
        decrypted_object.insert("event_id".to_owned(), event.event_id.to_string().into());
        decrypted_object.insert("origin_server_ts".to_owned(), server_ts.into());

        decrypted_object.insert(
            "unsigned".to_owned(),
            serde_json::to_value(&event.unsigned).unwrap_or_default(),
        );

        Ok((
            serde_json::from_value::<Raw<AnySyncRoomEvent>>(decrypted_value)?,
            message_index,
        ))
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for InboundGroupSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InboundGroupSession")
            .field("session_id", &self.session_id())
            .finish()
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickledInboundGroupSession {
    /// The pickle string holding the InboundGroupSession.
    pub pickle: InboundGroupSessionPickle,
    /// The public curve25519 key of the account that sent us the session
    pub sender_key: String,
    /// The public ed25519 key of the account that sent us the session.
    pub signing_key: BTreeMap<DeviceKeyAlgorithm, String>,
    /// The id of the room that the session is used in.
    pub room_id: RoomId,
    /// The list of claimed ed25519 that forwarded us this key. Will be None if
    /// we dirrectly received this session.
    pub forwarding_chains: Option<Vec<String>>,
    /// Flag remembering if the session was dirrectly sent to us by the sender
    /// or if it was imported.
    pub imported: bool,
}

/// The typed representation of a base64 encoded string of the GroupSession pickle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundGroupSessionPickle(String);

impl From<String> for InboundGroupSessionPickle {
    fn from(pickle_string: String) -> Self {
        InboundGroupSessionPickle(pickle_string)
    }
}

impl InboundGroupSessionPickle {
    /// Get the string representation of the pickle.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<ExportedRoomKey> for InboundGroupSession {
    type Error = OlmGroupSessionError;

    fn try_from(key: ExportedRoomKey) -> Result<Self, Self::Error> {
        let session = OlmInboundGroupSession::import(&key.session_key.0)?;

        let forwarding_chains = if key.forwarding_curve25519_key_chain.is_empty() {
            None
        } else {
            Some(key.forwarding_curve25519_key_chain)
        };

        Ok(InboundGroupSession {
            inner: Arc::new(Mutex::new(session)),
            session_id: Arc::new(key.session_id),
            sender_key: Arc::new(key.sender_key),
            signing_key: Arc::new(key.sender_claimed_keys),
            room_id: Arc::new(key.room_id),
            forwarding_chains: Arc::new(Mutex::new(forwarding_chains)),
            imported: Arc::new(true),
        })
    }
}
