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
use std::convert::TryFrom;
use std::convert::TryInto;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;

use matrix_sdk_common::locks::Mutex;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use zeroize::Zeroize;

pub use olm_rs::account::IdentityKeys;
use olm_rs::account::{OlmAccount, OneTimeKeys};
use olm_rs::errors::{OlmAccountError, OlmGroupSessionError, OlmSessionError};
use olm_rs::inbound_group_session::OlmInboundGroupSession;
use olm_rs::outbound_group_session::OlmOutboundGroupSession;
use olm_rs::session::OlmSession;
use olm_rs::PicklingMode;

use crate::device::Device;
use crate::error::{EventError, MegolmResult, SessionCreationError};
pub use olm_rs::{
    session::{OlmMessage, PreKeyMessage},
    utility::OlmUtility,
};

use matrix_sdk_common::identifiers::{DeviceId, RoomId, UserId};
use matrix_sdk_common::{
    api::r0::keys::{AlgorithmAndDeviceId, DeviceKeys, KeyAlgorithm, OneTimeKey, SignedKey},
    events::{
        room::{
            encrypted::{EncryptedEventContent, MegolmV1AesSha2Content},
            message::MessageEventContent,
        },
        Algorithm, AnyRoomEventStub, EventJson, EventType, MessageEventStub,
    },
};

/// Account holding identity keys for which sessions can be created.
///
/// An account is the central identity for encrypted communication between two
/// devices.
#[derive(Clone)]
pub struct Account {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceId>,
    inner: Arc<Mutex<OlmAccount>>,
    identity_keys: Arc<IdentityKeys>,
    shared: Arc<AtomicBool>,
    /// The number of signed one-time keys we have uploaded to the server. If
    /// this is None, no action will be taken. After a sync request the client
    /// needs to set this for us, depending on the count we will suggest the
    /// client to upload new keys.
    uploaded_signed_key_count: Arc<AtomicI64>,
}

// #[cfg_attr(tarpaulin, skip)]
impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("identity_keys", self.identity_keys())
            .field("shared", &self.shared())
            .finish()
    }
}

impl Account {
    const ALGORITHMS: &'static [&'static Algorithm] = &[
        &Algorithm::OlmV1Curve25519AesSha2,
        &Algorithm::MegolmV1AesSha2,
    ];

    /// Create a fresh new account, this will generate the identity key-pair.
    #[allow(clippy::ptr_arg)]
    pub fn new(user_id: &UserId, device_id: &DeviceId) -> Self {
        let account = OlmAccount::new();
        let identity_keys = account.parsed_identity_keys();

        Account {
            user_id: Arc::new(user_id.to_owned()),
            device_id: Arc::new(device_id.to_owned()),
            inner: Arc::new(Mutex::new(account)),
            identity_keys: Arc::new(identity_keys),
            shared: Arc::new(AtomicBool::new(false)),
            uploaded_signed_key_count: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Get the public parts of the identity keys for the account.
    pub fn identity_keys(&self) -> &IdentityKeys {
        &self.identity_keys
    }

    /// Update the uploaded key count.
    ///
    /// # Arguments
    ///
    /// * `new_count` - The new count that was reported by the server.
    pub(crate) fn update_uploaded_key_count(&self, new_count: u64) {
        let key_count = i64::try_from(new_count).unwrap_or(i64::MAX);
        self.uploaded_signed_key_count
            .store(key_count, Ordering::Relaxed);
    }

    /// Get the currently known uploaded key count.
    pub fn uploaded_key_count(&self) -> i64 {
        self.uploaded_signed_key_count.load(Ordering::Relaxed)
    }

    /// Has the account been shared with the server.
    pub fn shared(&self) -> bool {
        self.shared.load(Ordering::Relaxed)
    }

    /// Mark the account as shared.
    ///
    /// Messages shouldn't be encrypted with the session before it has been
    /// shared.
    pub(crate) fn mark_as_shared(&self) {
        self.shared.store(true, Ordering::Relaxed);
    }

    /// Get the one-time keys of the account.
    ///
    /// This can be empty, keys need to be generated first.
    pub(crate) async fn one_time_keys(&self) -> OneTimeKeys {
        self.inner.lock().await.parsed_one_time_keys()
    }

    /// Generate count number of one-time keys.
    pub(crate) async fn generate_one_time_keys_helper(&self, count: usize) {
        self.inner.lock().await.generate_one_time_keys(count);
    }

    /// Get the maximum number of one-time keys the account can hold.
    pub(crate) async fn max_one_time_keys(&self) -> usize {
        self.inner.lock().await.max_number_of_one_time_keys()
    }

    /// Get a tuple of device and one-time keys that need to be uploaded.
    ///
    /// Returns an empty error if no keys need to be uploaded.
    pub(crate) async fn generate_one_time_keys(&self) -> Result<u64, ()> {
        let count = self.uploaded_key_count() as u64;
        let max_keys = self.max_one_time_keys().await;
        let max_on_server = (max_keys as u64) / 2;

        if count >= (max_on_server) {
            return Err(());
        }

        let key_count = (max_on_server) - count;
        let key_count: usize = key_count.try_into().unwrap_or(max_keys);

        self.generate_one_time_keys_helper(key_count).await;
        Ok(key_count as u64)
    }

    /// Should account or one-time keys be uploaded to the server.
    pub(crate) async fn should_upload_keys(&self) -> bool {
        if !self.shared() {
            return true;
        }

        let count = self.uploaded_key_count() as u64;

        // If we have a known key count, check that we have more than
        // max_one_time_Keys() / 2, otherwise tell the client to upload more.
        let max_keys = self.max_one_time_keys().await as u64;
        // If there are more keys already uploaded than max_key / 2
        // bail out returning false, this also avoids overflow.
        if count > (max_keys / 2) {
            return false;
        }

        let key_count = (max_keys / 2) - count;
        key_count > 0
    }

    /// Get a tuple of device and one-time keys that need to be uploaded.
    ///
    /// Returns an empty error if no keys need to be uploaded.
    pub(crate) async fn keys_for_upload(
        &self,
    ) -> Result<
        (
            Option<DeviceKeys>,
            Option<BTreeMap<AlgorithmAndDeviceId, OneTimeKey>>,
        ),
        (),
    > {
        if !self.should_upload_keys().await {
            return Err(());
        }

        let device_keys = if !self.shared() {
            Some(self.device_keys().await)
        } else {
            None
        };

        let one_time_keys = self.signed_one_time_keys().await.ok();

        Ok((device_keys, one_time_keys))
    }

    /// Mark the current set of one-time keys as being published.
    pub(crate) async fn mark_keys_as_published(&self) {
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
    #[allow(clippy::ptr_arg)]
    pub fn from_pickle(
        pickle: String,
        pickle_mode: PicklingMode,
        shared: bool,
        uploaded_signed_key_count: i64,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Self, OlmAccountError> {
        let account = OlmAccount::unpickle(pickle, pickle_mode)?;
        let identity_keys = account.parsed_identity_keys();

        Ok(Account {
            user_id: Arc::new(user_id.to_owned()),
            device_id: Arc::new(device_id.to_owned()),
            inner: Arc::new(Mutex::new(account)),
            identity_keys: Arc::new(identity_keys),
            shared: Arc::new(AtomicBool::from(shared)),
            uploaded_signed_key_count: Arc::new(AtomicI64::new(uploaded_signed_key_count)),
        })
    }

    /// Sign the device keys of the account and return them so they can be
    /// uploaded.
    pub(crate) async fn device_keys(&self) -> DeviceKeys {
        let identity_keys = self.identity_keys();

        let mut keys = BTreeMap::new();

        keys.insert(
            AlgorithmAndDeviceId(KeyAlgorithm::Curve25519, (*self.device_id).clone()),
            identity_keys.curve25519().to_owned(),
        );
        keys.insert(
            AlgorithmAndDeviceId(KeyAlgorithm::Ed25519, (*self.device_id).clone()),
            identity_keys.ed25519().to_owned(),
        );

        let device_keys = json!({
            "user_id": (*self.user_id).clone(),
            "device_id": (*self.device_id).clone(),
            "algorithms": Account::ALGORITHMS,
            "keys": keys,
        });

        let mut signatures = BTreeMap::new();

        let mut signature = BTreeMap::new();
        signature.insert(
            AlgorithmAndDeviceId(KeyAlgorithm::Ed25519, (*self.device_id).clone()),
            self.sign_json(&device_keys).await,
        );
        signatures.insert((*self.user_id).clone(), signature);

        DeviceKeys {
            user_id: (*self.user_id).clone(),
            device_id: (*self.device_id).clone(),
            algorithms: vec![
                Algorithm::OlmV1Curve25519AesSha2,
                Algorithm::MegolmV1AesSha2,
            ],
            keys,
            signatures,
            unsigned: None,
        }
    }

    /// Convert a JSON value to the canonical representation and sign the JSON
    /// string.
    ///
    /// # Arguments
    ///
    /// * `json` - The value that should be converted into a canonical JSON
    /// string.
    ///
    /// # Panic
    ///
    /// Panics if the json value can't be serialized.
    pub async fn sign_json(&self, json: &Value) -> String {
        let canonical_json = cjson::to_string(json)
            .unwrap_or_else(|_| panic!(format!("Can't serialize {} to canonical JSON", json)));
        self.sign(&canonical_json).await
    }

    /// Generate, sign and prepare one-time keys to be uploaded.
    ///
    /// If no one-time keys need to be uploaded returns an empty error.
    pub(crate) async fn signed_one_time_keys(
        &self,
    ) -> Result<BTreeMap<AlgorithmAndDeviceId, OneTimeKey>, ()> {
        let _ = self.generate_one_time_keys().await?;

        let one_time_keys = self.one_time_keys().await;
        let mut one_time_key_map = BTreeMap::new();

        for (key_id, key) in one_time_keys.curve25519().iter() {
            let key_json = json!({
                "key": key,
            });

            let signature = self.sign_json(&key_json).await;

            let mut signature_map = BTreeMap::new();

            signature_map.insert(
                AlgorithmAndDeviceId(KeyAlgorithm::Ed25519, (*self.device_id).clone()),
                signature,
            );

            let mut signatures = BTreeMap::new();
            signatures.insert((*self.user_id).clone(), signature_map);

            let signed_key = SignedKey {
                key: key.to_owned(),
                signatures,
            };

            one_time_key_map.insert(
                AlgorithmAndDeviceId(KeyAlgorithm::SignedCurve25519, key_id.to_owned()),
                OneTimeKey::SignedKey(signed_key),
            );
        }

        Ok(one_time_key_map)
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
    pub(crate) async fn create_outbound_session_helper(
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

    /// Create a new session with another account given a one-time key and a
    /// device.
    ///
    /// Returns the newly created session or a `OlmSessionError` if creating a
    /// session failed.
    ///
    /// # Arguments
    /// * `device` - The other account's device.
    ///
    /// * `key_map` - A map from the algorithm and device id to the one-time
    ///     key that the other account created and shared with us.
    pub(crate) async fn create_outbound_session(
        &self,
        device: Device,
        key_map: &BTreeMap<AlgorithmAndDeviceId, OneTimeKey>,
    ) -> Result<Session, SessionCreationError> {
        let one_time_key = key_map.values().next().ok_or_else(|| {
            SessionCreationError::OneTimeKeyMissing(
                device.user_id().to_owned(),
                device.device_id().to_owned(),
            )
        })?;

        let one_time_key = match one_time_key {
            OneTimeKey::SignedKey(k) => k,
            OneTimeKey::Key(_) => {
                return Err(SessionCreationError::OneTimeKeyNotSigned(
                    device.user_id().to_owned(),
                    device.device_id().to_owned(),
                ));
            }
        };

        device.verify_one_time_key(&one_time_key).map_err(|e| {
            SessionCreationError::InvalidSignature(
                device.user_id().to_owned(),
                device.device_id().to_owned(),
                e,
            )
        })?;

        let curve_key = device.get_key(KeyAlgorithm::Curve25519).ok_or_else(|| {
            SessionCreationError::DeviceMissingCurveKey(
                device.user_id().to_owned(),
                device.device_id().to_owned(),
            )
        })?;

        self.create_outbound_session_helper(curve_key, &one_time_key)
            .await
            .map_err(|e| {
                SessionCreationError::OlmError(
                    device.user_id().to_owned(),
                    device.device_id().to_owned(),
                    e,
                )
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
    pub(crate) async fn create_inbound_session(
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

    /// Create a group session pair.
    ///
    /// This session pair can be used to encrypt and decrypt messages meant for
    /// a large group of participants.
    ///
    /// The outbound session is used to encrypt messages while the inbound one
    /// is used to decrypt messages encrypted by the outbound one.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The ID of the room where the group session will be used.
    pub(crate) async fn create_group_session_pair(
        &self,
        room_id: &RoomId,
    ) -> (OutboundGroupSession, InboundGroupSession) {
        let outbound =
            OutboundGroupSession::new(self.device_id.clone(), self.identity_keys.clone(), room_id);
        let identity_keys = self.identity_keys();

        let sender_key = identity_keys.curve25519();
        let signing_key = identity_keys.ed25519();

        let inbound = InboundGroupSession::new(
            sender_key,
            signing_key,
            &room_id,
            outbound.session_key().await,
        )
        .expect("Can't create inbound group session from a newly created outbound group session");

        (outbound, inbound)
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

// #[cfg_attr(tarpaulin, skip)]
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
        event: &MessageEventStub<EncryptedEventContent>,
    ) -> MegolmResult<(EventJson<AnyRoomEventStub>, u32)> {
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
            serde_json::from_value::<EventJson<AnyRoomEventStub>>(decrypted_value)?,
            message_index,
        ))
    }
}

// #[cfg_attr(tarpaulin, skip)]
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
    device_id: Arc<DeviceId>,
    account_identity_keys: Arc<IdentityKeys>,
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
    /// * `device_id` - The id of the device that created this session.
    ///
    /// * `identity_keys` - The identity keys of the account that created this
    /// session.
    ///
    /// * `room_id` - The id of the room that the session is used in.
    fn new(device_id: Arc<DeviceId>, identity_keys: Arc<IdentityKeys>, room_id: &RoomId) -> Self {
        let session = OlmOutboundGroupSession::new();
        let session_id = session.session_id();

        OutboundGroupSession {
            inner: Arc::new(Mutex::new(session)),
            room_id: Arc::new(room_id.to_owned()),
            device_id,
            account_identity_keys: identity_keys,
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
    async fn encrypt_helper(&self, plaintext: String) -> String {
        let session = self.inner.lock().await;
        session.encrypt(plaintext)
    }

    /// Encrypt a room message for the given room.
    ///
    /// Beware that a group session needs to be shared before this method can be
    /// called using the `share_group_session()` method.
    ///
    /// Since group sessions can expire or become invalid if the room membership
    /// changes client authors should check with the
    /// `should_share_group_session()` method if a new group session needs to
    /// be shared.
    ///
    /// # Arguments
    ///
    /// * `content` - The plaintext content of the message that should be
    /// encrypted.
    ///
    /// # Panics
    ///
    /// Panics if the content can't be serialized.
    pub async fn encrypt(&self, content: MessageEventContent) -> EncryptedEventContent {
        let json_content = json!({
            "content": content,
            "room_id": &*self.room_id,
            "type": EventType::RoomMessage,
        });

        let plaintext = cjson::to_string(&json_content).unwrap_or_else(|_| {
            panic!(format!(
                "Can't serialize {} to canonical JSON",
                json_content
            ))
        });

        let ciphertext = self.encrypt_helper(plaintext).await;

        EncryptedEventContent::MegolmV1AesSha2(MegolmV1AesSha2Content {
            ciphertext,
            sender_key: self.account_identity_keys.curve25519().to_owned(),
            session_id: self.session_id().to_owned(),
            device_id: (&*self.device_id).to_owned(),
        })
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

// #[cfg_attr(tarpaulin, skip)]
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
    use crate::olm::{Account, InboundGroupSession, Session};
    use matrix_sdk_common::api::r0::keys::SignedKey;
    use matrix_sdk_common::identifiers::{DeviceId, RoomId, UserId};
    use olm_rs::session::OlmMessage;
    use std::collections::BTreeMap;
    use std::convert::TryFrom;

    fn alice_id() -> UserId {
        UserId::try_from("@alice:example.org").unwrap()
    }

    fn alice_device_id() -> DeviceId {
        "ALICEDEVICE".to_string()
    }

    fn bob_id() -> UserId {
        UserId::try_from("@bob:example.org").unwrap()
    }

    fn bob_device_id() -> DeviceId {
        "BOBDEVICE".to_string()
    }

    pub(crate) async fn get_account_and_session() -> (Account, Session) {
        let alice = Account::new(&alice_id(), &alice_device_id());
        let bob = Account::new(&bob_id(), &bob_device_id());

        bob.generate_one_time_keys_helper(1).await;
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
            .create_outbound_session_helper(&sender_key, &one_time_key)
            .await
            .unwrap();

        (alice, session)
    }

    #[test]
    fn account_creation() {
        let account = Account::new(&alice_id(), &alice_device_id());
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
        let account = Account::new(&alice_id(), &alice_device_id());
        let one_time_keys = account.one_time_keys().await;

        assert!(one_time_keys.curve25519().is_empty());
        assert_ne!(account.max_one_time_keys().await, 0);

        account.generate_one_time_keys_helper(10).await;
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
        let alice = Account::new(&alice_id(), &alice_device_id());
        let bob = Account::new(&bob_id(), &bob_device_id());
        let alice_keys = alice.identity_keys();
        alice.generate_one_time_keys_helper(1).await;
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
            .create_outbound_session_helper(alice_keys.curve25519(), &one_time_key)
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
        let alice = Account::new(&alice_id(), &alice_device_id());
        let room_id = RoomId::try_from("!test:localhost").unwrap();

        let (outbound, _) = alice.create_group_session_pair(&room_id).await;

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
        let ciphertext = outbound.encrypt_helper(plaintext.clone()).await;

        assert_eq!(
            plaintext,
            inbound.decrypt_helper(ciphertext).await.unwrap().0
        );
    }
}
