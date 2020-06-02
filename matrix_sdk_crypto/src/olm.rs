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

use matrix_sdk_common::instant::Instant;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use matrix_sdk_common::locks::Mutex;
use serde::Serialize;
use zeroize::Zeroize;

pub use olm_rs::account::IdentityKeys;
use olm_rs::account::{OlmAccount, OneTimeKeys};
use olm_rs::errors::{OlmAccountError, OlmGroupSessionError, OlmSessionError};
use olm_rs::inbound_group_session::OlmInboundGroupSession;
use olm_rs::outbound_group_session::OlmOutboundGroupSession;
use olm_rs::session::OlmSession;
use olm_rs::PicklingMode;

pub use olm_rs::{
    session::{OlmMessage, PreKeyMessage},
    utility::OlmUtility,
};

use matrix_sdk_common::api::r0::keys::SignedKey;
use matrix_sdk_common::identifiers::RoomId;

/// Account holding identity keys for which sessions can be created.
///
/// An account is the central identity for encrypted communication between two
/// devices.
#[derive(Clone)]
pub struct Account {
    inner: Arc<Mutex<OlmAccount>>,
    identity_keys: Arc<IdentityKeys>,
    shared: Arc<AtomicBool>,
}

#[cfg_attr(tarpaulin, skip)]
impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("identity_keys", self.identity_keys())
            .field("shared", &self.shared())
            .finish()
    }
}

#[cfg_attr(tarpaulin, skip)]
impl Default for Account {
    fn default() -> Self {
        Self::new()
    }
}

impl Account {
    /// Create a fresh new account, this will generate the identity key-pair.
    pub fn new() -> Self {
        let account = OlmAccount::new();
        let identity_keys = account.parsed_identity_keys();

        Account {
            inner: Arc::new(Mutex::new(account)),
            identity_keys: Arc::new(identity_keys),
            shared: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Get the public parts of the identity keys for the account.
    pub fn identity_keys(&self) -> &IdentityKeys {
        &self.identity_keys
    }

    /// Has the account been shared with the server.
    pub fn shared(&self) -> bool {
        self.shared.load(Ordering::Relaxed)
    }

    /// Mark the account as shared.
    ///
    /// Messages shouldn't be encrypted with the session before it has been
    /// shared.
    pub fn mark_as_shared(&self) {
        self.shared.store(true, Ordering::Relaxed);
    }

    /// Get the one-time keys of the account.
    ///
    /// This can be empty, keys need to be generated first.
    pub async fn one_time_keys(&self) -> OneTimeKeys {
        self.inner.lock().await.parsed_one_time_keys()
    }

    /// Generate count number of one-time keys.
    pub async fn generate_one_time_keys(&self, count: usize) {
        self.inner.lock().await.generate_one_time_keys(count);
    }

    /// Get the maximum number of one-time keys the account can hold.
    pub async fn max_one_time_keys(&self) -> usize {
        self.inner.lock().await.max_number_of_one_time_keys()
    }

    /// Mark the current set of one-time keys as being published.
    pub async fn mark_keys_as_published(&self) {
        self.inner.lock().await.mark_keys_as_published();
    }

    /// Sign the given string using the accounts signing key.
    ///
    /// Returns the signature as a base64 encoded string.
    pub async fn sign(&self, string: &str) -> String {
        self.inner.lock().await.sign(string)
    }

    /// Store the account as a base64 encoded string.
    ///
    /// # Arguments
    ///
    /// * `pickle_mode` - The mode that was used to pickle the account, either an
    /// unencrypted mode or an encrypted using passphrase.
    pub async fn pickle(&self, pickle_mode: PicklingMode) -> String {
        self.inner.lock().await.pickle(pickle_mode)
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
        let account = OlmAccount::unpickle(pickle, pickle_mode)?;
        let identity_keys = account.parsed_identity_keys();

        Ok(Account {
            inner: Arc::new(Mutex::new(account)),
            identity_keys: Arc::new(identity_keys),
            shared: Arc::new(AtomicBool::from(shared)),
        })
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
    pub async fn create_outbound_session(
        &self,
        their_identity_key: &str,
        their_one_time_key: &SignedKey,
    ) -> Result<Session, OlmSessionError> {
        let session = self
            .inner
            .lock()
            .await
            .create_outbound_session(their_identity_key, &their_one_time_key.key)?;

        let now = Instant::now();
        let session_id = session.session_id();

        Ok(Session {
            inner: Arc::new(Mutex::new(session)),
            session_id: Arc::new(session_id),
            sender_key: Arc::new(their_identity_key.to_owned()),
            creation_time: Arc::new(now),
            last_use_time: Arc::new(now),
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
    pub async fn create_inbound_session(
        &self,
        their_identity_key: &str,
        message: PreKeyMessage,
    ) -> Result<Session, OlmSessionError> {
        let session = self
            .inner
            .lock()
            .await
            .create_inbound_session_from(their_identity_key, message)?;

        self.inner
            .lock()
            .await
            .remove_one_time_keys(&session)
            .expect(
            "Session was successfully created but the account doesn't hold a matching one-time key",
        );

        let now = Instant::now();
        let session_id = session.session_id();

        Ok(Session {
            inner: Arc::new(Mutex::new(session)),
            session_id: Arc::new(session_id),
            sender_key: Arc::new(their_identity_key.to_owned()),
            creation_time: Arc::new(now),
            last_use_time: Arc::new(now),
        })
    }
}

impl PartialEq for Account {
    fn eq(&self, other: &Self) -> bool {
        self.identity_keys() == other.identity_keys() && self.shared() == other.shared()
    }
}

/// Cryptographic session that enables secure communication between two
/// `Account`s
#[derive(Clone)]
pub struct Session {
    inner: Arc<Mutex<OlmSession>>,
    session_id: Arc<String>,
    pub(crate) sender_key: Arc<String>,
    pub(crate) creation_time: Arc<Instant>,
    pub(crate) last_use_time: Arc<Instant>,
}

#[cfg_attr(tarpaulin, skip)]
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
    /// Returns the decrypted plaintext or an `OlmSessionError` if decryption
    /// failed.
    ///
    /// # Arguments
    ///
    /// * `message` - The Olm message that should be decrypted.
    pub async fn decrypt(&mut self, message: OlmMessage) -> Result<String, OlmSessionError> {
        let plaintext = self.inner.lock().await.decrypt(message)?;
        self.last_use_time = Arc::new(Instant::now());
        Ok(plaintext)
    }

    /// Encrypt the given plaintext as a OlmMessage.
    ///
    /// Returns the encrypted Olm message.
    ///
    /// # Arguments
    ///
    /// * `plaintext` - The plaintext that should be encrypted.
    pub async fn encrypt(&mut self, plaintext: &str) -> OlmMessage {
        let message = self.inner.lock().await.encrypt(plaintext);
        self.last_use_time = Arc::new(Instant::now());
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
    pub async fn matches(
        &self,
        their_identity_key: &str,
        message: PreKeyMessage,
    ) -> Result<bool, OlmSessionError> {
        self.inner
            .lock()
            .await
            .matches_inbound_session_from(their_identity_key, message)
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
    pub async fn pickle(&self, pickle_mode: PicklingMode) -> String {
        self.inner.lock().await.pickle(pickle_mode)
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
        let session_id = session.session_id();

        Ok(Session {
            inner: Arc::new(Mutex::new(session)),
            session_id: Arc::new(session_id),
            sender_key: Arc::new(sender_key),
            creation_time: Arc::new(creation_time),
            last_use_time: Arc::new(last_use_time),
        })
    }
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.session_id() == other.session_id()
    }
}

/// The private session key of a group session.
/// Can be used to create a new inbound group session.
#[derive(Clone, Debug, Serialize, Zeroize)]
#[zeroize(drop)]
pub struct GroupSessionKey(pub String);

/// Inbound group session.
///
/// Inbound group sessions are used to exchange room messages between a group of
/// participants. Inbound group sessions are used to decrypt the room messages.
#[derive(Clone)]
pub struct InboundGroupSession {
    inner: Arc<Mutex<OlmInboundGroupSession>>,
    session_id: Arc<String>,
    pub(crate) sender_key: Arc<String>,
    pub(crate) signing_key: Arc<String>,
    pub(crate) room_id: Arc<RoomId>,
    forwarding_chains: Arc<Mutex<Option<Vec<String>>>>,
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

        Ok(InboundGroupSession {
            inner: Arc::new(Mutex::new(session)),
            session_id: Arc::new(session_id),
            sender_key: Arc::new(sender_key.to_owned()),
            signing_key: Arc::new(signing_key.to_owned()),
            room_id: Arc::new(room_id.clone()),
            forwarding_chains: Arc::new(Mutex::new(None)),
        })
    }

    /// Store the group session as a base64 encoded string.
    ///
    /// # Arguments
    ///
    /// * `pickle_mode` - The mode that was used to pickle the group session,
    /// either an unencrypted mode or an encrypted using passphrase.
    pub async fn pickle(&self, pickle_mode: PicklingMode) -> String {
        self.inner.lock().await.pickle(pickle_mode)
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
        let session_id = session.session_id();

        Ok(InboundGroupSession {
            inner: Arc::new(Mutex::new(session)),
            session_id: Arc::new(session_id),
            sender_key: Arc::new(sender_key),
            signing_key: Arc::new(signing_key),
            room_id: Arc::new(room_id),
            forwarding_chains: Arc::new(Mutex::new(None)),
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
    pub async fn decrypt(&self, message: String) -> Result<(String, u32), OlmGroupSessionError> {
        self.inner.lock().await.decrypt(message)
    }
}

#[cfg_attr(tarpaulin, skip)]
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
    pub async fn session_key(&self) -> GroupSessionKey {
        let session = self.inner.lock().await;
        GroupSessionKey(session.session_key())
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

#[cfg_attr(tarpaulin, skip)]
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
pub(crate) mod test {
    use crate::olm::{Account, InboundGroupSession, OutboundGroupSession, Session};
    use matrix_sdk_common::api::r0::keys::SignedKey;
    use matrix_sdk_common::identifiers::RoomId;
    use olm_rs::session::OlmMessage;
    use std::collections::BTreeMap;
    use std::convert::TryFrom;

    pub(crate) async fn get_account_and_session() -> (Account, Session) {
        let alice = Account::new();

        let bob = Account::new();

        bob.generate_one_time_keys(1).await;
        let one_time_key = bob
            .one_time_keys()
            .await
            .curve25519()
            .iter()
            .nth(0)
            .unwrap()
            .1
            .to_owned();
        let one_time_key = SignedKey {
            key: one_time_key,
            signatures: BTreeMap::new(),
        };
        let sender_key = bob.identity_keys().curve25519().to_owned();
        let session = alice
            .create_outbound_session(&sender_key, &one_time_key)
            .await
            .unwrap();

        (alice, session)
    }

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

        account.mark_as_shared();
        assert!(account.shared());
    }

    #[tokio::test]
    async fn one_time_keys_creation() {
        let account = Account::new();
        let one_time_keys = account.one_time_keys().await;

        assert!(one_time_keys.curve25519().is_empty());
        assert_ne!(account.max_one_time_keys().await, 0);

        account.generate_one_time_keys(10).await;
        let one_time_keys = account.one_time_keys().await;

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

        account.mark_keys_as_published().await;
        let one_time_keys = account.one_time_keys().await;
        assert!(one_time_keys.curve25519().is_empty());
    }

    #[tokio::test]
    async fn session_creation() {
        let alice = Account::new();
        let bob = Account::new();
        let alice_keys = alice.identity_keys();
        alice.generate_one_time_keys(1).await;
        let one_time_keys = alice.one_time_keys().await;
        alice.mark_keys_as_published().await;

        let one_time_key = one_time_keys
            .curve25519()
            .iter()
            .nth(0)
            .unwrap()
            .1
            .to_owned();

        let one_time_key = SignedKey {
            key: one_time_key,
            signatures: BTreeMap::new(),
        };

        let mut bob_session = bob
            .create_outbound_session(alice_keys.curve25519(), &one_time_key)
            .await
            .unwrap();

        let plaintext = "Hello world";

        let message = bob_session.encrypt(plaintext).await;

        let prekey_message = match message.clone() {
            OlmMessage::PreKey(m) => m,
            OlmMessage::Message(_) => panic!("Incorrect message type"),
        };

        let bob_keys = bob.identity_keys();
        let mut alice_session = alice
            .create_inbound_session(bob_keys.curve25519(), prekey_message.clone())
            .await
            .unwrap();

        assert!(alice_session
            .matches(bob_keys.curve25519(), prekey_message)
            .await
            .unwrap());

        assert_eq!(bob_session.session_id(), alice_session.session_id());

        let decyrpted = alice_session.decrypt(message).await.unwrap();
        assert_eq!(plaintext, decyrpted);
    }

    #[tokio::test]
    async fn group_session_creation() {
        let room_id = RoomId::try_from("!test:localhost").unwrap();

        let outbound = OutboundGroupSession::new(&room_id);

        assert_eq!(0, outbound.message_index().await);
        assert!(!outbound.shared());
        outbound.mark_as_shared();
        assert!(outbound.shared());

        let inbound = InboundGroupSession::new(
            "test_key",
            "test_key",
            &room_id,
            outbound.session_key().await,
        )
        .unwrap();

        assert_eq!(0, inbound.first_known_index().await);

        assert_eq!(outbound.session_id(), inbound.session_id());

        let plaintext = "This is a secret to everybody".to_owned();
        let ciphertext = outbound.encrypt(plaintext.clone()).await;

        assert_eq!(plaintext, inbound.decrypt(ciphertext).await.unwrap().0);
    }
}
