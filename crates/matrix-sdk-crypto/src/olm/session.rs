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

use std::{collections::BTreeMap, fmt, sync::Arc};

use matrix_sdk_common::{instant::Instant, locks::Mutex};
use ruma::{
    events::{
        room::encrypted::{
            CiphertextInfo, EncryptedEventScheme, OlmV1Curve25519AesSha2Content,
            ToDeviceRoomEncryptedEventContent,
        },
        AnyToDeviceEventContent, EventContent,
    },
    DeviceId, DeviceKeyAlgorithm, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use vodozemac::olm::{DecryptionError, OlmMessage, Session as InnerSession, SessionPickle};

use super::{deserialize_instant, serialize_instant, IdentityKeys};
use crate::{
    error::{EventError, OlmResult},
    ReadOnlyDevice,
};

/// Cryptographic session that enables secure communication between two
/// `Account`s
#[derive(Clone)]
pub struct Session {
    /// The `UserId` associated with this session
    pub user_id: Arc<UserId>,
    /// The specific `DeviceId` associated with this session
    pub device_id: Arc<DeviceId>,
    /// The `IdentityKeys` associated with this session
    pub our_identity_keys: Arc<IdentityKeys>,
    /// The OlmSession
    pub inner: Arc<Mutex<InnerSession>>,
    /// Our sessionId
    pub session_id: Arc<str>,
    /// The Key of the sender
    pub sender_key: Arc<str>,
    /// Has this been created using the fallback key
    pub created_using_fallback_key: bool,
    /// When the session was created
    pub creation_time: Arc<Instant>,
    /// When the session was last used
    pub last_use_time: Arc<Instant>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("session_id", &self.session_id())
            .field("sender_key", &self.sender_key)
            .finish()
    }
}

impl Session {
    /// Decrypt the given Olm message.
    ///
    /// Returns the decrypted plaintext or an `DecrypitonError` if decryption
    /// failed.
    ///
    /// # Arguments
    ///
    /// * `message` - The Olm message that should be decrypted.
    pub async fn decrypt(&mut self, message: &OlmMessage) -> Result<String, DecryptionError> {
        let plaintext = self.inner.lock().await.decrypt(message)?;
        self.last_use_time = Arc::new(Instant::now());
        Ok(plaintext)
    }

    /// Get the sender key that was used to establish this Session.
    pub fn sender_key(&self) -> &str {
        &self.sender_key
    }

    /// Encrypt the given plaintext as a OlmMessage.
    ///
    /// Returns the encrypted Olm message.
    ///
    /// # Arguments
    ///
    /// * `plaintext` - The plaintext that should be encrypted.
    pub(crate) async fn encrypt_helper(&mut self, plaintext: &str) -> OlmMessage {
        let message = self.inner.lock().await.encrypt(plaintext);
        self.last_use_time = Arc::new(Instant::now());
        message
    }

    /// Encrypt the given event content content as an m.room.encrypted event
    /// content.
    ///
    /// # Arguments
    ///
    /// * `recipient_device` - The device for which this message is going to be
    ///   encrypted, this needs to be the device that was used to create this
    ///   session with.
    ///
    /// * `content` - The content of the event.
    pub async fn encrypt(
        &mut self,
        recipient_device: &ReadOnlyDevice,
        content: AnyToDeviceEventContent,
    ) -> OlmResult<ToDeviceRoomEncryptedEventContent> {
        let recipient_signing_key = recipient_device
            .get_key(DeviceKeyAlgorithm::Ed25519)
            .ok_or(EventError::MissingSigningKey)?;

        let event_type = content.event_type();

        let payload = json!({
            "sender": self.user_id.as_str(),
            "sender_device": self.device_id.as_ref(),
            "keys": {
                "ed25519": self.our_identity_keys.ed25519.to_base64(),
            },
            "recipient": recipient_device.user_id(),
            "recipient_keys": {
                "ed25519": recipient_signing_key,
            },
            "type": event_type,
            "content": content,
        });

        let plaintext = serde_json::to_string(&payload)?;
        let ciphertext = self.encrypt_helper(&plaintext).await.to_parts();

        let message_type = ciphertext.0;
        let ciphertext = CiphertextInfo::new(ciphertext.1, (message_type as u32).into());

        let mut content = BTreeMap::new();
        content.insert((*self.sender_key).to_owned(), ciphertext);

        Ok(EncryptedEventScheme::OlmV1Curve25519AesSha2(OlmV1Curve25519AesSha2Content::new(
            content,
            self.our_identity_keys.curve25519.to_base64(),
        ))
        .into())
    }

    /// Returns the unique identifier for this session.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Store the session as a base64 encoded string.
    ///
    /// # Arguments
    ///
    /// * `pickle_mode` - The mode that was used to pickle the session, either
    /// an unencrypted mode or an encrypted using passphrase.
    pub async fn pickle(&self) -> PickledSession {
        let pickle = self.inner.lock().await.pickle();

        PickledSession {
            pickle,
            sender_key: self.sender_key.to_string(),
            created_using_fallback_key: self.created_using_fallback_key,
            creation_time: *self.creation_time,
            last_use_time: *self.last_use_time,
        }
    }

    /// Restore a Session from a previously pickled string.
    ///
    /// Returns the restored Olm Session or a `SessionUnpicklingError` if there
    /// was an error.
    ///
    /// # Arguments
    ///
    /// * `user_id` - Our own user id that the session belongs to.
    ///
    /// * `device_id` - Our own device id that the session belongs to.
    ///
    /// * `our_idenity_keys` - An clone of the Arc to our own identity keys.
    ///
    /// * `pickle` - The pickled version of the `Session`.
    ///
    /// * `pickle_mode` - The mode that was used to pickle the session, either
    /// an unencrypted mode or an encrypted using passphrase.
    pub fn from_pickle(
        user_id: Arc<UserId>,
        device_id: Arc<DeviceId>,
        our_identity_keys: Arc<IdentityKeys>,
        pickle: PickledSession,
    ) -> Self {
        let session: vodozemac::olm::Session = pickle.pickle.into();
        let session_id = session.session_id();

        Session {
            user_id,
            device_id,
            our_identity_keys,
            inner: Arc::new(Mutex::new(session)),
            session_id: session_id.into(),
            created_using_fallback_key: pickle.created_using_fallback_key,
            sender_key: pickle.sender_key.into(),
            creation_time: Arc::new(pickle.creation_time),
            last_use_time: Arc::new(pickle.last_use_time),
        }
    }
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.session_id() == other.session_id()
    }
}

/// A pickled version of a `Session`.
///
/// Holds all the information that needs to be stored in a database to restore
/// a Session.
#[derive(Serialize, Deserialize)]
#[allow(missing_debug_implementations)]
pub struct PickledSession {
    /// The pickle string holding the Olm Session.
    pub pickle: SessionPickle,
    /// The curve25519 key of the other user that we share this session with.
    pub sender_key: String,
    /// Was the session created using a fallback key.
    #[serde(default)]
    pub created_using_fallback_key: bool,
    /// The relative time elapsed since the session was created.
    #[serde(deserialize_with = "deserialize_instant", serialize_with = "serialize_instant")]
    pub creation_time: Instant,
    /// The relative time elapsed since the session was last used.
    #[serde(deserialize_with = "deserialize_instant", serialize_with = "serialize_instant")]
    pub last_use_time: Instant,
}
