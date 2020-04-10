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

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use olm_rs::account::{IdentityKeys, OlmAccount, OneTimeKeys};
use olm_rs::errors::{OlmAccountError, OlmGroupSessionError, OlmSessionError};
use olm_rs::inbound_group_session::OlmInboundGroupSession;
use olm_rs::outbound_group_session::OlmOutboundGroupSession;
use olm_rs::session::{OlmMessage, OlmSession, PreKeyMessage};
use olm_rs::PicklingMode;

use crate::api::r0::keys::SignedKey;
use crate::identifiers::RoomId;

/// The Olm account.
/// An account is the central identity for encrypted communication between two
/// devices. It holds the two identity key pairs for a device.
pub struct Account {
    inner: OlmAccount,
    pub(crate) shared: bool,
}

impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Olm Account: {:?}, shared: {}",
            self.identity_keys(),
            self.shared
        )
    }
}

impl Account {
    /// Create a new account.
    pub fn new() -> Self {
        Account {
            inner: OlmAccount::new(),
            shared: false,
        }
    }

    /// Get the public parts of the identity keys for the account.
    pub fn identity_keys(&self) -> IdentityKeys {
        self.inner.parsed_identity_keys()
    }

    /// Has the account been shared with the server.
    pub fn shared(&self) -> bool {
        self.shared
    }

    /// Get the one-time keys of the account.
    ///
    /// This can be empty, keys need to be generated first.
    pub fn one_time_keys(&self) -> OneTimeKeys {
        self.inner.parsed_one_time_keys()
    }

    /// Generate count number of one-time keys.
    pub fn generate_one_time_keys(&self, count: usize) {
        self.inner.generate_one_time_keys(count);
    }

    /// Get the maximum number of one-time keys the account can hold.
    pub fn max_one_time_keys(&self) -> usize {
        self.inner.max_number_of_one_time_keys()
    }

    /// Mark the current set of one-time keys as being published.
    pub fn mark_keys_as_published(&self) {
        self.inner.mark_keys_as_published();
    }

    /// Sign the given string using the accounts signing key.
    ///
    /// Returns the signature as a base64 encoded string.
    pub fn sign(&self, string: &str) -> String {
        self.inner.sign(string)
    }

    /// Store the account as a base64 encoded string.
    ///
    /// # Arguments
    ///
    /// * `pickle_mode` - The mode that was used to pickle the account, either an
    /// unencrypted mode or an encrypted using passphrase.
    pub fn pickle(&self, pickle_mode: PicklingMode) -> String {
        self.inner.pickle(pickle_mode)
    }

    /// Restore an account from a previously pickled string.
    ///
    /// # Arguments
    ///
    /// * `pickle` - The pickled string of the account.
    ///
    /// * `pickle_mode` - The mode that was used to pickle the account, either an
    /// unencrypted mode or an encrypted using passphrase.
    ///
    /// * `shared` - Boolean determining if the account was uploaded to the
    /// server.
    pub fn from_pickle(
        pickle: String,
        pickle_mode: PicklingMode,
        shared: bool,
    ) -> Result<Self, OlmAccountError> {
        let acc = OlmAccount::unpickle(pickle, pickle_mode)?;
        Ok(Account { inner: acc, shared })
    }

    /// Create a new session with another account given a one-time key.
    ///
    /// Returns the newly created session or a `OlmSessionError` if creating a
    /// session failed.
    ///
    /// # Arguments
    /// * `their_identity_key` - The other account's identity/curve25519 key.
    ///
    /// * `their_one_time_key` - A signed one-time key that the other account
    /// created and shared with us.
    pub fn create_outbound_session(
        &self,
        their_identity_key: &str,
        their_one_time_key: &SignedKey,
    ) -> Result<Session, OlmSessionError> {
        let session = self
            .inner
            .create_outbound_session(their_identity_key, &their_one_time_key.key)?;

        let now = Instant::now();

        Ok(Session {
            inner: session,
            sender_key: their_identity_key.to_owned(),
            creation_time: now.clone(),
            last_use_time: now,
        })
    }

    /// Create a new session with another account given a pre-key Olm message.
    ///
    /// Returns the newly created session or a `OlmSessionError` if creating a
    /// session failed.
    ///
    /// # Arguments
    /// * `their_identity_key` - The other account's identitiy/curve25519 key.
    ///
    /// * `message` - A pre-key Olm message that was sent to us by the other
    /// account.
    pub fn create_inbound_session_from(
        &self,
        their_identity_key: &str,
        message: PreKeyMessage,
    ) -> Result<Session, OlmSessionError> {
        let session = self
            .inner
            .create_inbound_session_from(their_identity_key, message)?;

        let now = Instant::now();

        Ok(Session {
            inner: session,
            sender_key: their_identity_key.to_owned(),
            creation_time: now.clone(),
            last_use_time: now,
        })
    }
}

impl PartialEq for Account {
    fn eq(&self, other: &Self) -> bool {
        self.identity_keys() == other.identity_keys() && self.shared == other.shared
    }
}

#[derive(Debug)]
/// The Olm Session.
///
/// Sessions are used to exchange encrypted messages between two
/// accounts/devices.
pub struct Session {
    inner: OlmSession,
    pub(crate) sender_key: String,
    pub(crate) creation_time: Instant,
    pub(crate) last_use_time: Instant,
}

impl Session {
    /// Decrypt the given Olm message.
    ///
    /// Returns the decrypted plaintext or an `OlmSessionError` if decryption
    /// failed.
    ///
    /// # Arguments
    ///
    /// * `message` - The Olm message that should be decrypted.
    pub fn decrypt(&mut self, message: OlmMessage) -> Result<String, OlmSessionError> {
        let plaintext = self.inner.decrypt(message)?;
        self.last_use_time = Instant::now();
        Ok(plaintext)
    }

    /// Encrypt the given plaintext as a OlmMessage.
    ///
    /// Returns the encrypted Olm message.
    ///
    /// # Arguments
    ///
    /// * `plaintext` - The plaintext that should be encrypted.
    pub fn encrypt(&mut self, plaintext: &str) -> OlmMessage {
        let message = self.inner.encrypt(plaintext);
        self.last_use_time = Instant::now();
        message
    }

    /// Check if a pre-key Olm message was encrypted for this session.
    ///
    /// Returns true if it matches, false if not and a OlmSessionError if there
    /// was an error checking if it matches.
    ///
    /// # Arguments
    ///
    /// * `their_identity_key` - The identity/curve25519 key of the account
    /// that encrypted this Olm message.
    ///
    /// * `message` - The pre-key Olm message that should be checked.
    pub fn matches(
        &self,
        their_identity_key: &str,
        message: PreKeyMessage,
    ) -> Result<bool, OlmSessionError> {
        self.inner
            .matches_inbound_session_from(their_identity_key, message)
    }

    /// Returns the unique identifier for this session.
    pub fn session_id(&self) -> String {
        self.inner.session_id()
    }

    /// Store the session as a base64 encoded string.
    ///
    /// # Arguments
    ///
    /// * `pickle_mode` - The mode that was used to pickle the session, either
    /// an unencrypted mode or an encrypted using passphrase.
    pub fn pickle(&self, pickle_mode: PicklingMode) -> String {
        self.inner.pickle(pickle_mode)
    }

    /// Restore a Session from a previously pickled string.
    ///
    /// Returns the restored Olm Session or a `OlmSessionError` if there was an
    /// error.
    ///
    /// # Arguments
    ///
    /// * `pickle` - The pickled string of the session.
    ///
    /// * `pickle_mode` - The mode that was used to pickle the session, either
    /// an unencrypted mode or an encrypted using passphrase.
    ///
    /// * `sender_key` - The public curve25519 key of the account that
    /// established the session with us.
    ///
    /// * `creation_time` - The timestamp that marks when the session was
    /// created.
    ///
    /// * `last_use_time` - The timestamp that marks when the session was
    /// last used to encrypt or decrypt an Olm message.
    pub fn from_pickle(
        pickle: String,
        pickle_mode: PicklingMode,
        sender_key: String,
        creation_time: Instant,
        last_use_time: Instant,
    ) -> Result<Self, OlmSessionError> {
        let session = OlmSession::unpickle(pickle, pickle_mode)?;
        Ok(Session {
            inner: session,
            sender_key,
            creation_time,
            last_use_time,
        })
    }
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.session_id() == other.session_id()
    }
}

/// Inbound group session.
///
/// Inbound group sessions are used to exchange room messages between a group of
/// participants. Inbound group sessions are used to decrypt the room messages.
pub struct InboundGroupSession {
    inner: OlmInboundGroupSession,
    pub(crate) sender_key: String,
    pub(crate) signing_key: String,
    pub(crate) room_id: RoomId,
    forwarding_chains: Option<Vec<String>>,
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
        session_key: &str,
    ) -> Result<Self, OlmGroupSessionError> {
        Ok(InboundGroupSession {
            inner: OlmInboundGroupSession::new(session_key)?,
            sender_key: sender_key.to_owned(),
            signing_key: signing_key.to_owned(),
            room_id: room_id.clone(),
            forwarding_chains: None,
        })
    }

    /// Store the group session as a base64 encoded string.
    ///
    /// # Arguments
    ///
    /// * `pickle_mode` - The mode that was used to pickle the group session,
    /// either an unencrypted mode or an encrypted using passphrase.
    pub fn pickle(&self, pickle_mode: PicklingMode) -> String {
        self.inner.pickle(pickle_mode)
    }

    /// Restore a Session from a previously pickled string.
    ///
    /// Returns the restored group session or a `OlmGroupSessionError` if there
    /// was an error.
    ///
    /// # Arguments
    ///
    /// * `pickle` - The pickled string of the group session session.
    ///
    /// * `pickle_mode` - The mode that was used to pickle the session, either
    /// an unencrypted mode or an encrypted using passphrase.
    ///
    /// * `sender_key` - The public curve25519 key of the account that
    /// sent us the session
    ///
    /// * `signing_key` - The public ed25519 key of the account that
    /// sent us the session.
    ///
    /// * `room_id` - The id of the room that the session is used in.
    pub fn from_pickle(
        pickle: String,
        pickle_mode: PicklingMode,
        sender_key: String,
        signing_key: String,
        room_id: RoomId,
    ) -> Result<Self, OlmGroupSessionError> {
        let session = OlmInboundGroupSession::unpickle(pickle, pickle_mode)?;
        Ok(InboundGroupSession {
            inner: session,
            sender_key,
            signing_key,
            room_id,
            forwarding_chains: None,
        })
    }

    /// Returns the unique identifier for this session.
    pub fn session_id(&self) -> String {
        self.inner.session_id()
    }

    /// Get the first message index we know how to decrypt.
    pub fn first_known_index(&self) -> u32 {
        self.inner.first_known_index()
    }

    /// Decrypt the given ciphertext.
    ///
    /// Returns the decrypted plaintext or an `OlmGroupSessionError` if
    /// decryption failed.
    ///
    /// # Arguments
    ///
    /// * `message` - The message that should be decrypted.
    pub fn decrypt(&self, message: String) -> Result<(String, u32), OlmGroupSessionError> {
        self.inner.decrypt(message)
    }
}

impl fmt::Debug for InboundGroupSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InboundGroupSession")
            .field("session_id", &self.session_id())
            .finish()
    }
}

/// Outbound group session.
///
/// Outbound group sessions are used to exchange room messages between a group
/// of participants. Outbound group sessions are used to encrypt the room
/// messages.
#[derive(Clone)]
pub struct OutboundGroupSession {
    inner: Arc<Mutex<OlmOutboundGroupSession>>,
    session_id: Arc<String>,
    room_id: Arc<RoomId>,
    creation_time: Arc<Instant>,
    message_count: Arc<AtomicUsize>,
    shared: Arc<AtomicBool>,
}

impl OutboundGroupSession {
    /// Create a new outbound group session for the given room.
    ///
    /// Outbound group sessions are used to encrypt room messages.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room that the session is used in.
    pub fn new(room_id: &RoomId) -> Self {
        let session = OlmOutboundGroupSession::new();
        let session_id = session.session_id();

        OutboundGroupSession {
            inner: Arc::new(Mutex::new(session)),
            room_id: Arc::new(room_id.to_owned()),
            session_id: Arc::new(session_id),
            creation_time: Arc::new(Instant::now()),
            message_count: Arc::new(AtomicUsize::new(0)),
            shared: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Encrypt the given plaintext using this session.
    ///
    /// Returns the encrypted ciphertext.
    ///
    /// # Arguments
    ///
    /// * `plaintext` - The plaintext that should be encrypted.
    pub async fn encrypt(&self, plaintext: String) -> String {
        let session = self.inner.lock().await;
        session.encrypt(plaintext)
    }

    /// Check if the session has expired and if it should be rotated.
    ///
    /// A session will expire after some time or if enough messages have been
    /// encrypted using it.
    pub fn expired(&self) -> bool {
        // TODO implement this.
        false
    }

    /// Mark the session as shared.
    ///
    /// Messages shouldn't be encrypted with the session before it has been
    /// shared.
    pub fn mark_as_shared(&self) {
        self.shared.store(true, Ordering::Relaxed);
    }

    /// Check if the session has been marked as shared.
    pub fn shared(&self) -> bool {
        self.shared.load(Ordering::Relaxed)
    }

    /// Get the session key of this session.
    ///
    /// A session key can be used to to create an `InboundGroupSession`.
    pub async fn session_key(&self) -> String {
        let session = self.inner.lock().await;
        session.session_key()
    }

    /// Returns the unique identifier for this session.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get the current message index for this session.
    ///
    /// Each message is sent with an increasing index. This returns the
    /// message index that will be used for the next encrypted message.
    pub async fn message_index(&self) -> u32 {
        let session = self.inner.lock().await;
        session.session_message_index()
    }
}

impl std::fmt::Debug for OutboundGroupSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboundGroupSession")
            .field("session_id", &self.session_id)
            .field("room_id", &self.room_id)
            .field("creation_time", &self.creation_time)
            .field("message_count", &self.message_count)
            .finish()
    }
}

#[cfg(test)]
mod test {
    use crate::crypto::olm::Account;

    #[test]
    fn account_creation() {
        let account = Account::new();
        let identyty_keys = account.identity_keys();

        assert!(!account.shared());
        assert!(!identyty_keys.ed25519().is_empty());
        assert_ne!(identyty_keys.values().len(), 0);
        assert_ne!(identyty_keys.keys().len(), 0);
        assert_ne!(identyty_keys.iter().len(), 0);
        assert!(identyty_keys.contains_key("ed25519"));
        assert_eq!(
            identyty_keys.ed25519(),
            identyty_keys.get("ed25519").unwrap()
        );
        assert!(!identyty_keys.curve25519().is_empty());
    }

    #[test]
    fn one_time_keys_creation() {
        let account = Account::new();
        let one_time_keys = account.one_time_keys();

        assert!(one_time_keys.curve25519().is_empty());
        assert_ne!(account.max_one_time_keys(), 0);

        account.generate_one_time_keys(10);
        let one_time_keys = account.one_time_keys();

        assert!(!one_time_keys.curve25519().is_empty());
        assert_ne!(one_time_keys.values().len(), 0);
        assert_ne!(one_time_keys.keys().len(), 0);
        assert_ne!(one_time_keys.iter().len(), 0);
        assert!(one_time_keys.contains_key("curve25519"));
        assert_eq!(one_time_keys.curve25519().keys().len(), 10);
        assert_eq!(
            one_time_keys.curve25519(),
            one_time_keys.get("curve25519").unwrap()
        );

        account.mark_keys_as_published();
        let one_time_keys = account.one_time_keys();
        assert!(one_time_keys.curve25519().is_empty());
    }
}
