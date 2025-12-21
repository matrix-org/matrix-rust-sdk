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

use std::{fmt, sync::Arc};

use ruma::{SecondsSinceUnixEpoch, serde::Raw};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{Span, debug};
use vodozemac::{
    Curve25519PublicKey,
    olm::{DecryptionError, OlmMessage, Session as InnerSession, SessionConfig, SessionPickle},
};

#[cfg(feature = "experimental-algorithms")]
use crate::types::events::room::encrypted::OlmV2Curve25519AesSha2Content;
use crate::{
    DeviceData,
    error::{EventError, OlmResult, SessionUnpickleError},
    types::{
        DeviceKeys, EventEncryptionAlgorithm,
        events::{
            EventType,
            olm_v1::{DecryptedOlmV1Event, OlmV1Keys},
            room::encrypted::{OlmV1Curve25519AesSha2Content, ToDeviceEncryptedEventContent},
        },
    },
};

/// Cryptographic session that enables secure communication between two
/// `Account`s
#[derive(Clone)]
pub struct Session {
    /// The OlmSession
    pub inner: Arc<Mutex<InnerSession>>,
    /// Our sessionId
    pub session_id: Arc<str>,
    /// The Key of the sender
    pub sender_key: Curve25519PublicKey,
    /// Our own signed device keys
    pub our_device_keys: DeviceKeys,
    /// Has this been created using the fallback key
    pub created_using_fallback_key: bool,
    /// When the session was created
    pub creation_time: SecondsSinceUnixEpoch,
    /// When the session was last used
    pub last_use_time: SecondsSinceUnixEpoch,
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
    /// Returns the decrypted plaintext or a [`DecryptionError`] if decryption
    /// failed.
    ///
    /// # Arguments
    ///
    /// * `message` - The Olm message that should be decrypted.
    pub async fn decrypt(&mut self, message: &OlmMessage) -> Result<String, DecryptionError> {
        let mut inner = self.inner.lock().await;
        Span::current().record("session_id", inner.session_id());

        let plaintext = inner.decrypt(message)?;
        debug!(session=?inner, "Decrypted an Olm message");

        let plaintext = String::from_utf8_lossy(&plaintext).to_string();

        self.last_use_time = SecondsSinceUnixEpoch::now();

        Ok(plaintext)
    }

    /// Get the sender key that was used to establish this Session.
    pub fn sender_key(&self) -> Curve25519PublicKey {
        self.sender_key
    }

    /// Get the [`SessionConfig`] that this session is using.
    pub async fn session_config(&self) -> SessionConfig {
        self.inner.lock().await.session_config()
    }

    /// Get the [`EventEncryptionAlgorithm`] of this [`Session`].
    #[allow(clippy::unused_async)] // The experimental-algorithms feature uses async code.
    pub async fn algorithm(&self) -> EventEncryptionAlgorithm {
        #[cfg(feature = "experimental-algorithms")]
        if self.session_config().await.version() == 2 {
            EventEncryptionAlgorithm::OlmV2Curve25519AesSha2
        } else {
            EventEncryptionAlgorithm::OlmV1Curve25519AesSha2
        }

        #[cfg(not(feature = "experimental-algorithms"))]
        EventEncryptionAlgorithm::OlmV1Curve25519AesSha2
    }

    /// Encrypt the given plaintext as a OlmMessage.
    ///
    /// Returns the encrypted Olm message.
    ///
    /// # Arguments
    ///
    /// * `plaintext` - The plaintext that should be encrypted.
    pub(crate) async fn encrypt_helper(&mut self, plaintext: &str) -> OlmMessage {
        let mut session = self.inner.lock().await;
        let message = session.encrypt(plaintext);
        self.last_use_time = SecondsSinceUnixEpoch::now();
        debug!(?session, "Successfully encrypted an event");
        message
    }

    /// Encrypt the given event content as an m.room.encrypted event
    /// content.
    ///
    /// # Arguments
    ///
    /// * `recipient_device` - The device for which this message is going to be
    ///   encrypted, this needs to be the device that was used to create this
    ///   session with.
    ///
    /// * `event_type` - The type of the event content.
    ///
    /// * `content` - The content of the event.
    pub async fn encrypt(
        &mut self,
        recipient_device: &DeviceData,
        event_type: &str,
        content: impl Serialize,
        message_id: Option<String>,
    ) -> OlmResult<Raw<ToDeviceEncryptedEventContent>> {
        #[derive(Debug)]
        struct Content<'a> {
            event_type: &'a str,
            content: Raw<Value>,
        }

        impl EventType for Content<'_> {
            // This is a bit of a hack: usually we just define the `EVENT_TYPE` and use the
            // default implementation of `event_type()`. We can't do this here
            // because the event type isn't static.
            //
            // We have to provide `EVENT_TYPE` to conform to the `EventType` trait, but
            // don't actually use it, so we just leave it empty.
            //
            // This works because the serialization uses `event_type()` and this type is
            // contained to this function.
            const EVENT_TYPE: &'static str = "";

            fn event_type(&self) -> &str {
                self.event_type
            }
        }

        impl Serialize for Content<'_> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                self.content.serialize(serializer)
            }
        }

        let plaintext = {
            let content = serde_json::to_value(content)?;
            let content = Content { event_type, content: Raw::new(&content)? };

            let recipient_signing_key =
                recipient_device.ed25519_key().ok_or(EventError::MissingSigningKey)?;

            let content = DecryptedOlmV1Event {
                sender: self.our_device_keys.user_id.clone(),
                recipient: recipient_device.user_id().into(),
                keys: OlmV1Keys {
                    ed25519: self
                        .our_device_keys
                        .ed25519_key()
                        .expect("Our own device should have an Ed25519 public key"),
                },
                recipient_keys: OlmV1Keys { ed25519: recipient_signing_key },
                sender_device_keys: Some(self.our_device_keys.clone()),
                content,
            };

            serde_json::to_string(&content)?
        };

        let ciphertext = self.encrypt_helper(&plaintext).await;

        let content = self.build_encrypted_event(ciphertext, message_id).await?;
        let content = Raw::new(&content)?;
        Ok(content)
    }

    /// Take the given ciphertext, and package it into an `m.room.encrypted`
    /// to-device message content.
    ///
    /// # Arguments
    ///
    /// * `ciphertext` - The encrypted message content.
    /// * `message_id` - The ID to use for this to-device message, as
    ///   `org.matrix.msgid`.
    pub(crate) async fn build_encrypted_event(
        &self,
        ciphertext: OlmMessage,
        message_id: Option<String>,
    ) -> OlmResult<ToDeviceEncryptedEventContent> {
        let content = match self.algorithm().await {
            EventEncryptionAlgorithm::OlmV1Curve25519AesSha2 => OlmV1Curve25519AesSha2Content {
                ciphertext,
                recipient_key: self.sender_key,
                sender_key: self
                    .our_device_keys
                    .curve25519_key()
                    .expect("Device doesn't have curve25519 key"),
                message_id,
            }
            .into(),
            #[cfg(feature = "experimental-algorithms")]
            EventEncryptionAlgorithm::OlmV2Curve25519AesSha2 => OlmV2Curve25519AesSha2Content {
                ciphertext,
                sender_key: self
                    .our_device_keys
                    .curve25519_key()
                    .expect("Device doesn't have curve25519 key"),
                message_id,
            }
            .into(),
            _ => unreachable!(),
        };

        Ok(content)
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
    ///   an unencrypted mode or an encrypted using passphrase.
    pub async fn pickle(&self) -> PickledSession {
        let pickle = self.inner.lock().await.pickle();

        PickledSession {
            pickle,
            sender_key: self.sender_key,
            created_using_fallback_key: self.created_using_fallback_key,
            creation_time: self.creation_time,
            last_use_time: self.last_use_time,
        }
    }

    /// Restore a Session from a previously pickled string.
    ///
    /// Returns the restored Olm Session or a `SessionUnpicklingError` if there
    /// was an error.
    ///
    /// # Arguments
    ///
    /// * `our_device_keys` - Our own signed device keys.
    ///
    /// * `pickle` - The pickled version of the `Session`.
    pub fn from_pickle(
        our_device_keys: DeviceKeys,
        pickle: PickledSession,
    ) -> Result<Self, SessionUnpickleError> {
        if our_device_keys.curve25519_key().is_none() {
            return Err(SessionUnpickleError::MissingIdentityKey);
        }
        if our_device_keys.ed25519_key().is_none() {
            return Err(SessionUnpickleError::MissingSigningKey);
        }

        let session: vodozemac::olm::Session = pickle.pickle.into();
        let session_id = session.session_id();

        Ok(Session {
            inner: Arc::new(Mutex::new(session)),
            session_id: session_id.into(),
            created_using_fallback_key: pickle.created_using_fallback_key,
            sender_key: pickle.sender_key,
            our_device_keys,
            creation_time: pickle.creation_time,
            last_use_time: pickle.last_use_time,
        })
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
    pub sender_key: Curve25519PublicKey,
    /// Was the session created using a fallback key.
    #[serde(default)]
    pub created_using_fallback_key: bool,
    /// The Unix timestamp when the session was created.
    pub creation_time: SecondsSinceUnixEpoch,
    /// The Unix timestamp when the session was last used.
    pub last_use_time: SecondsSinceUnixEpoch,
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_let;
    use matrix_sdk_test::async_test;
    use ruma::{device_id, user_id};
    use serde_json::{self, Value};
    use vodozemac::olm::{OlmMessage, SessionConfig};

    use crate::{
        identities::DeviceData,
        olm::Account,
        types::events::{
            dummy::DummyEventContent, olm_v1::DecryptedOlmV1Event,
            room::encrypted::ToDeviceEncryptedEventContent,
        },
    };

    #[async_test]
    async fn test_encryption_and_decryption() {
        use ruma::events::dummy::ToDeviceDummyEventContent;

        // Given users Alice and Bob
        let alice =
            Account::with_device_id(user_id!("@alice:localhost"), device_id!("ALICEDEVICE"));
        let mut bob = Account::with_device_id(user_id!("@bob:localhost"), device_id!("BOBDEVICE"));

        // When Alice creates an Olm session with Bob
        bob.generate_one_time_keys(1);
        let one_time_key = *bob.one_time_keys().values().next().unwrap();
        let sender_key = bob.identity_keys().curve25519;
        let mut alice_session = alice.create_outbound_session_helper(
            SessionConfig::default(),
            sender_key,
            one_time_key,
            false,
            alice.device_keys(),
        );

        let alice_device = DeviceData::from_account(&alice);

        // and encrypts a message
        let message = alice_session
            .encrypt(&alice_device, "m.dummy", ToDeviceDummyEventContent::new(), None)
            .await
            .unwrap()
            .deserialize()
            .unwrap();

        #[cfg(feature = "experimental-algorithms")]
        assert_let!(ToDeviceEncryptedEventContent::OlmV2Curve25519AesSha2(content) = message);
        #[cfg(not(feature = "experimental-algorithms"))]
        assert_let!(ToDeviceEncryptedEventContent::OlmV1Curve25519AesSha2(content) = message);

        let OlmMessage::PreKey(prekey) = content.ciphertext else {
            panic!("Wrong Olm message type");
        };

        // Then Bob should be able to create a session from the message and decrypt it.
        let bob_session_result = bob
            .create_inbound_session(
                alice_device.curve25519_key().unwrap(),
                bob.device_keys(),
                &prekey,
            )
            .unwrap();

        // Also ensure that the encrypted payload has the device keys under the stable
        // prefix
        let plaintext: Value = serde_json::from_str(&bob_session_result.plaintext).unwrap();
        assert_eq!(plaintext["sender_device_keys"]["user_id"].as_str(), Some("@alice:localhost"));

        // And the serialized object matches the format as specified in
        // DecryptedOlmV1Event
        let event: DecryptedOlmV1Event<DummyEventContent> =
            serde_json::from_str(&bob_session_result.plaintext).unwrap();
        assert_eq!(event.sender_device_keys.unwrap(), alice.device_keys());
    }
}
