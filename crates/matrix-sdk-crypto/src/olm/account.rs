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
    collections::{BTreeMap, HashMap},
    convert::TryInto,
    fmt,
    ops::Deref,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use matrix_sdk_common::{instant::Instant, locks::Mutex};
use ruma::{
    api::client::keys::{
        upload_keys,
        upload_signatures::v3::{Request as SignatureUploadRequest, SignedKeys},
    },
    encryption::{CrossSigningKey, DeviceKeys, OneTimeKey, SignedKey},
    events::{
        room::encrypted::{
            EncryptedEventScheme, OlmV1Curve25519AesSha2Content, ToDeviceRoomEncryptedEvent,
        },
        AnyToDeviceEvent, OlmV1Keys,
    },
    serde::{Base64, CanonicalJsonValue, Raw},
    DeviceId, DeviceKeyAlgorithm, DeviceKeyId, EventEncryptionAlgorithm, RoomId, UInt, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, value::RawValue as RawJsonValue, Value};
use sha2::{Digest, Sha256};
use tracing::{debug, info, trace, warn};
use vodozemac::{
    olm::{Account as InnerAccount, AccountPickle, IdentityKeys, OlmMessage, PreKeyMessage},
    Curve25519PublicKey, KeyId, PickleError,
};

use super::{
    EncryptionSettings, InboundGroupSession, OutboundGroupSession, PrivateCrossSigningIdentity,
    Session,
};
use crate::{
    error::{EventError, OlmResult, SessionCreationError},
    identities::{MasterPubkey, ReadOnlyDevice},
    requests::UploadSigningKeysRequest,
    store::{Changes, Store},
    utilities::encode,
    CryptoStoreError, OlmError, SignatureError,
};

#[derive(Debug, Clone)]
pub struct Account {
    pub inner: ReadOnlyAccount,
    pub store: Store,
}

#[derive(Debug, Clone)]
pub enum SessionType {
    New(Session),
    Existing(Session),
}

#[derive(Debug)]
pub struct InboundCreationResult {
    pub session: Session,
    pub plaintext: String,
}

impl SessionType {
    #[cfg(test)]
    pub fn session(self) -> Session {
        match self {
            SessionType::New(s) => s,
            SessionType::Existing(s) => s,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OlmDecryptionInfo {
    pub sender: Box<UserId>,
    pub session: SessionType,
    pub message_hash: OlmMessageHash,
    pub deserialized_event: Option<AnyToDeviceEvent>,
    pub event: Raw<AnyToDeviceEvent>,
    pub signing_key: String,
    pub sender_key: String,
    pub inbound_group_session: Option<InboundGroupSession>,
}

/// A hash of a successfully decrypted Olm message.
///
/// Can be used to check if a message has been replayed to us.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OlmMessageHash {
    /// The curve25519 key of the sender that sent us the Olm message.
    pub sender_key: String,
    /// The hash of the message.
    pub hash: String,
}

impl OlmMessageHash {
    fn new(sender_key: &str, message_type: u8, ciphertext: &str) -> Self {
        let sha = Sha256::new()
            .chain_update(sender_key)
            .chain_update(&[message_type])
            .chain_update(&ciphertext)
            .finalize();

        Self { sender_key: sender_key.to_owned(), hash: encode(sha.as_slice()) }
    }
}

impl Deref for Account {
    type Target = ReadOnlyAccount;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Account {
    fn parse_message(
        sender_key: &str,
        message_type: UInt,
        ciphertext: String,
    ) -> Result<(OlmMessage, OlmMessageHash), EventError> {
        let message_type: u8 = message_type
            .try_into()
            .map_err(|_| EventError::UnsupportedOlmType(message_type.into()))?;

        let message_hash = OlmMessageHash::new(sender_key, message_type, &ciphertext);
        let message = OlmMessage::from_parts(message_type.into(), &ciphertext)
            .map_err(|_| EventError::UnsupportedOlmType(message_type.into()))?;

        Ok((message, message_hash))
    }

    pub async fn save(&self) -> Result<(), CryptoStoreError> {
        self.store.save_account(self.inner.clone()).await
    }

    async fn decrypt_olm_v1(
        &self,
        sender: &UserId,
        content: &OlmV1Curve25519AesSha2Content,
    ) -> OlmResult<OlmDecryptionInfo> {
        let identity_keys = self.inner.identity_keys();

        // Try to find a ciphertext that was meant for our device.
        if let Some(ciphertext) = content.ciphertext.get(&identity_keys.curve25519.to_base64()) {
            let (message, message_hash) = match Self::parse_message(
                &content.sender_key,
                ciphertext.message_type,
                ciphertext.body.clone(),
            ) {
                Ok(m) => m,
                Err(e) => {
                    warn!(error =? e, "Encrypted to-device event isn't valid");
                    return Err(e.into());
                }
            };

            // Decrypt the OlmMessage and get a Ruma event out of it.
            match self.decrypt_olm_message(sender, &content.sender_key, message).await {
                Ok((session, event, signing_key)) => Ok(OlmDecryptionInfo {
                    sender: sender.to_owned(),
                    session,
                    message_hash,
                    event,
                    signing_key,
                    sender_key: content.sender_key.clone(),
                    deserialized_event: None,
                    inbound_group_session: None,
                }),
                Err(OlmError::SessionWedged(user_id, sender_key)) => {
                    if self.store.is_message_known(&message_hash).await? {
                        info!(
                            sender = sender.as_str(),
                            sender_key = content.sender_key.as_str(),
                            "An Olm message got replayed, decryption failed"
                        );

                        Err(OlmError::ReplayedMessage(user_id, sender_key))
                    } else {
                        Err(OlmError::SessionWedged(user_id, sender_key))
                    }
                }
                Err(e) => Err(e),
            }
        } else {
            warn!(
                sender = sender.as_str(),
                sender_key = content.sender_key.as_str(),
                "Olm event doesn't contain a ciphertext for our key"
            );

            Err(EventError::MissingCiphertext.into())
        }
    }

    pub async fn decrypt_to_device_event(
        &self,
        event: &ToDeviceRoomEncryptedEvent,
    ) -> OlmResult<OlmDecryptionInfo> {
        trace!(sender = event.sender.as_str(), "Decrypting a to-device event");

        if let EncryptedEventScheme::OlmV1Curve25519AesSha2(c) = &event.content.scheme {
            self.decrypt_olm_v1(&event.sender, c).await
        } else {
            warn!(
                sender = event.sender.as_str(),
                algorithm =? event.content.scheme,
                "Error, unsupported encryption algorithm"
            );

            Err(EventError::UnsupportedAlgorithm.into())
        }
    }

    pub fn update_uploaded_key_count(&self, key_count: &BTreeMap<DeviceKeyAlgorithm, UInt>) {
        if let Some(count) = key_count.get(&DeviceKeyAlgorithm::SignedCurve25519) {
            let count: u64 = (*count).into();
            let old_count = self.inner.uploaded_key_count();

            // Some servers might always return the key counts in the sync
            // response, we don't want to the logs with noop changes if they do
            // so.
            if count != old_count {
                debug!(
                    "Updated uploaded one-time key count {} -> {}.",
                    self.inner.uploaded_key_count(),
                    count
                );
            }

            self.inner.update_uploaded_key_count(count);
        }
    }

    pub async fn receive_keys_upload_response(
        &self,
        response: &upload_keys::v3::Response,
    ) -> OlmResult<()> {
        if !self.inner.shared() {
            debug!("Marking account as shared");
        }
        self.inner.mark_as_shared();

        debug!("Marking one-time keys as published");
        self.update_uploaded_key_count(&response.one_time_key_counts);
        self.inner.mark_keys_as_published().await;
        self.store.save_account(self.inner.clone()).await?;

        Ok(())
    }

    /// Try to decrypt an Olm message.
    ///
    /// This try to decrypt an Olm message using all the sessions we share
    /// with the given sender.
    async fn decrypt_with_existing_sessions(
        &self,
        sender_key: &str,
        message: &OlmMessage,
    ) -> OlmResult<Option<(Session, String)>> {
        let s = self.store.get_sessions(sender_key).await?;

        // We don't have any existing sessions, return early.
        let sessions = if let Some(s) = s {
            s
        } else {
            return Ok(None);
        };

        let mut decrypted: Option<(Session, String)> = None;

        for session in &mut *sessions.lock().await {
            let ret = session.decrypt(message).await;

            match ret {
                Ok(p) => {
                    decrypted = Some((session.clone(), p));
                    break;
                }
                Err(_) => {
                    continue;
                }
            }
        }

        Ok(decrypted)
    }

    /// Decrypt an Olm message, creating a new Olm session if possible.
    async fn decrypt_olm_message(
        &self,
        sender: &UserId,
        sender_key: &str,
        message: OlmMessage,
    ) -> OlmResult<(SessionType, Raw<AnyToDeviceEvent>, String)> {
        // First try to decrypt using an existing session.
        let (session, plaintext) = if let Some(d) =
            self.decrypt_with_existing_sessions(sender_key, &message).await?
        {
            // Decryption succeeded, de-structure the session/plaintext out of
            // the Option.
            (SessionType::Existing(d.0), d.1)
        } else {
            // Decryption failed with every known session, let's try to create a
            // new session.
            match &message {
                // A new session can only be created using a pre-key message,
                // return with an error if it isn't one.
                OlmMessage::Normal(_) => {
                    warn!(
                        sender = sender.as_str(),
                        sender_key = sender_key,
                        "Failed to decrypt a non-pre-key message with all \
                        available sessions",
                    );
                    return Err(OlmError::SessionWedged(sender.to_owned(), sender_key.to_owned()));
                }

                OlmMessage::PreKey(m) => {
                    // Create the new session.
                    let result = match self.inner.create_inbound_session(sender_key, m).await {
                        Ok(r) => r,
                        Err(e) => {
                            warn!(
                                sender = sender.as_str(),
                                sender_key = sender_key,
                                error =? e,
                                "Failed to create a new Olm session from a \
                                prekey message",
                            );
                            return Err(OlmError::SessionWedged(
                                sender.to_owned(),
                                sender_key.to_owned(),
                            ));
                        }
                    };

                    // We need to add the new session to the session cache, otherwise
                    // we might try to create the same session again.
                    // TODO separate the session cache from the storage so we only add
                    // it to the cache but don't store it.
                    let changes = Changes {
                        account: Some(self.inner.clone()),
                        sessions: vec![result.session.clone()],
                        ..Default::default()
                    };
                    self.store.save_changes(changes).await?;

                    (SessionType::New(result.session), result.plaintext)
                }
            }
        };

        trace!(
            sender = sender.as_str(),
            sender_key = sender_key,
            "Successfully decrypted an Olm message"
        );

        match self.parse_decrypted_to_device_event(sender, plaintext) {
            Ok((event, signing_key)) => Ok((session, event, signing_key)),
            Err(e) => {
                // We might created a new session but decryption might still
                // have failed, store it for the error case here, this is fine
                // since we don't expect this to happen often or at all.
                match session {
                    SessionType::New(s) => {
                        let changes = Changes {
                            account: Some(self.inner.clone()),
                            sessions: vec![s],
                            ..Default::default()
                        };
                        self.store.save_changes(changes).await?;
                    }
                    SessionType::Existing(s) => {
                        self.store.save_sessions(&[s]).await?;
                    }
                }

                warn!(
                    sender = sender.as_str(),
                    sender_key = sender_key,
                    error =? e,
                    "A to-device message was successfully decrypted but \
                    parsing and checking the event fields failed"
                );

                Err(e)
            }
        }
    }

    /// Parse a decrypted Olm message, check that the plaintext and encrypted
    /// senders match and that the message was meant for us.
    fn parse_decrypted_to_device_event(
        &self,
        sender: &UserId,
        plaintext: String,
    ) -> OlmResult<(Raw<AnyToDeviceEvent>, String)> {
        #[derive(Deserialize)]
        struct DecryptedEvent {
            sender: Box<UserId>,
            recipient: Box<UserId>,
            recipient_keys: OlmV1Keys,
            keys: OlmV1Keys,
        }

        let event: DecryptedEvent = serde_json::from_str(&plaintext)?;
        let identity_keys = self.inner.identity_keys();

        if event.recipient != self.user_id() {
            Err(EventError::MismatchedSender(event.recipient, self.user_id().to_owned()).into())
        } else if event.sender != sender {
            Err(EventError::MismatchedSender(event.sender, sender.to_owned()).into())
        } else if identity_keys.ed25519.to_base64() != event.recipient_keys.ed25519 {
            Err(EventError::MismatchedKeys(
                identity_keys.ed25519.to_base64(),
                event.recipient_keys.ed25519,
            )
            .into())
        } else {
            Ok((Raw::from_json(RawJsonValue::from_string(plaintext)?), event.keys.ed25519))
        }
    }
}

/// Account holding identity keys for which sessions can be created.
///
/// An account is the central identity for encrypted communication between two
/// devices.
#[derive(Clone)]
pub struct ReadOnlyAccount {
    /// The user_id this account belongs to
    pub user_id: Arc<UserId>,
    /// The device_id of this entry
    pub device_id: Arc<DeviceId>,
    inner: Arc<Mutex<InnerAccount>>,
    /// The associated identity keys
    pub identity_keys: Arc<IdentityKeys>,
    shared: Arc<AtomicBool>,
    /// The number of signed one-time keys we have uploaded to the server. If
    /// this is None, no action will be taken. After a sync request the client
    /// needs to set this for us, depending on the count we will suggest the
    /// client to upload new keys.
    uploaded_signed_key_count: Arc<AtomicU64>,
}

/// A pickled version of an `Account`.
///
/// Holds all the information that needs to be stored in a database to restore
/// an account.
#[derive(Serialize, Deserialize)]
#[allow(missing_debug_implementations)]
pub struct PickledAccount {
    /// The user id of the account owner.
    pub user_id: Box<UserId>,
    /// The device id of the account owner.
    pub device_id: Box<DeviceId>,
    /// The pickled version of the Olm account.
    pub pickle: AccountPickle,
    /// Was the account shared.
    pub shared: bool,
    /// The number of uploaded one-time keys we have on the server.
    pub uploaded_signed_key_count: u64,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for ReadOnlyAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("identity_keys", &self.identity_keys())
            .field("shared", &self.shared())
            .finish()
    }
}

impl ReadOnlyAccount {
    const ALGORITHMS: &'static [&'static EventEncryptionAlgorithm] = &[
        &EventEncryptionAlgorithm::OlmV1Curve25519AesSha2,
        &EventEncryptionAlgorithm::MegolmV1AesSha2,
    ];

    /// Create a fresh new account, this will generate the identity key-pair.
    #[allow(clippy::ptr_arg)]
    pub fn new(user_id: &UserId, device_id: &DeviceId) -> Self {
        let account = InnerAccount::new();
        let identity_keys = account.identity_keys();

        Self {
            user_id: user_id.into(),
            device_id: device_id.into(),
            inner: Arc::new(Mutex::new(account)),
            identity_keys: Arc::new(identity_keys),
            shared: Arc::new(AtomicBool::new(false)),
            uploaded_signed_key_count: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Get the user id of the owner of the account.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// Get the device id that owns this account.
    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// Get the public parts of the identity keys for the account.
    pub fn identity_keys(&self) -> IdentityKeys {
        *self.identity_keys
    }

    /// Update the uploaded key count.
    ///
    /// # Arguments
    ///
    /// * `new_count` - The new count that was reported by the server.
    pub fn update_uploaded_key_count(&self, new_count: u64) {
        self.uploaded_signed_key_count.store(new_count, Ordering::SeqCst);
    }

    /// Get the currently known uploaded key count.
    pub fn uploaded_key_count(&self) -> u64 {
        self.uploaded_signed_key_count.load(Ordering::SeqCst)
    }

    /// Has the account been shared with the server.
    pub fn shared(&self) -> bool {
        self.shared.load(Ordering::SeqCst)
    }

    /// Mark the account as shared.
    ///
    /// Messages shouldn't be encrypted with the session before it has been
    /// shared.
    pub fn mark_as_shared(&self) {
        self.shared.store(true, Ordering::SeqCst);
    }

    /// Get the one-time keys of the account.
    ///
    /// This can be empty, keys need to be generated first.
    pub async fn one_time_keys(&self) -> HashMap<KeyId, Curve25519PublicKey> {
        self.inner.lock().await.one_time_keys()
    }

    /// Generate count number of one-time keys.
    pub async fn generate_one_time_keys_helper(&self, count: usize) {
        self.inner.lock().await.generate_one_time_keys(count);
    }

    /// Get the maximum number of one-time keys the account can hold.
    pub async fn max_one_time_keys(&self) -> usize {
        self.inner.lock().await.max_number_of_one_time_keys()
    }

    /// Get a tuple of device and one-time keys that need to be uploaded.
    ///
    /// Returns an empty error if no keys need to be uploaded.
    pub async fn generate_one_time_keys(&self) -> Result<u64, ()> {
        // Only generate one-time keys if there aren't any, otherwise the caller
        // might have failed to upload them the last time this method was
        // called.
        if self.one_time_keys().await.is_empty() {
            let count = self.uploaded_key_count();
            let max_keys = self.max_one_time_keys().await;

            if count >= max_keys as u64 {
                return Err(());
            }

            let key_count = (max_keys as u64) - count;
            let key_count: usize = key_count.try_into().unwrap_or(max_keys);

            self.generate_one_time_keys_helper(key_count).await;
            Ok(key_count as u64)
        } else {
            Ok(0)
        }
    }

    /// Should account or one-time keys be uploaded to the server.
    pub async fn should_upload_keys(&self) -> bool {
        if !self.shared() {
            return true;
        }

        let count = self.uploaded_key_count();

        // If we have a known key count, check that we have more than
        // max_one_time_Keys(), otherwise tell the client to upload more.
        let max_keys = self.max_one_time_keys().await as u64;
        // If there are more keys already uploaded than max_key bail out
        // returning false, this also avoids overflow.
        if count > max_keys {
            return false;
        }

        let key_count = max_keys - count;
        key_count > 0
    }

    /// Get a tuple of device and one-time keys that need to be uploaded.
    ///
    /// Returns None if no keys need to be uploaded.
    pub async fn keys_for_upload(
        &self,
    ) -> Option<(Option<DeviceKeys>, BTreeMap<Box<DeviceKeyId>, Raw<OneTimeKey>>)> {
        if !self.should_upload_keys().await {
            return None;
        }

        let device_keys = if !self.shared() { Some(self.device_keys().await) } else { None };

        let one_time_keys = self.signed_one_time_keys().await.ok().unwrap_or_default();

        Some((device_keys, one_time_keys))
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

    /// Check that the given json value is signed by this account.
    #[cfg(feature = "backups_v1")]
    pub fn is_signed(&self, json: &mut Value) -> Result<(), SignatureError> {
        use crate::olm::utility::VerifyJson;

        let signing_key = self.identity_keys.ed25519;

        signing_key.verify_json(
            &self.user_id,
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.device_id()),
            json,
        )
    }

    /// Get a serializeable version of the `Account` so it can be persisted.
    pub async fn pickle(&self) -> PickledAccount {
        let pickle = self.inner.lock().await.pickle();

        PickledAccount {
            user_id: self.user_id().to_owned(),
            device_id: self.device_id().to_owned(),
            pickle,
            shared: self.shared(),
            uploaded_signed_key_count: self.uploaded_key_count(),
        }
    }

    /// Restore an account from a previously pickled one.
    ///
    /// # Arguments
    ///
    /// * `pickle` - The pickled version of the Account.
    ///
    /// * `pickle_mode` - The mode that was used to pickle the account, either
    ///   an
    /// unencrypted mode or an encrypted using passphrase.
    pub fn from_pickle(pickle: PickledAccount) -> Result<Self, PickleError> {
        let account: vodozemac::olm::Account = pickle.pickle.into();
        let identity_keys = account.identity_keys();

        Ok(Self {
            user_id: pickle.user_id.into(),
            device_id: pickle.device_id.into(),
            inner: Arc::new(Mutex::new(account)),
            identity_keys: Arc::new(identity_keys),
            shared: Arc::new(AtomicBool::from(pickle.shared)),
            uploaded_signed_key_count: Arc::new(AtomicU64::new(pickle.uploaded_signed_key_count)),
        })
    }

    /// Generate the unsigned `DeviceKeys` from this ReadOnlyAccount
    pub fn unsigned_device_keys(&self) -> DeviceKeys {
        let identity_keys = self.identity_keys();
        let keys = BTreeMap::from([
            (
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::Curve25519, &self.device_id),
                identity_keys.curve25519.to_base64(),
            ),
            (
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, &self.device_id),
                identity_keys.ed25519.to_base64(),
            ),
        ]);

        DeviceKeys::new(
            (*self.user_id).to_owned(),
            (*self.device_id).to_owned(),
            Self::ALGORITHMS.iter().map(|a| (**a).clone()).collect(),
            keys,
            BTreeMap::new(),
        )
    }

    /// Sign the device keys of the account and return them so they can be
    /// uploaded.
    pub async fn device_keys(&self) -> DeviceKeys {
        let mut device_keys = self.unsigned_device_keys();

        // Create a copy of the device keys containing only fields that will
        // get signed.
        let json_device_keys = json!({
            "user_id": device_keys.user_id,
            "device_id": device_keys.device_id,
            "algorithms": device_keys.algorithms,
            "keys": device_keys.keys,
        });

        device_keys
            .signatures
            .entry(self.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, &self.device_id),
                self.sign_json(json_device_keys).await,
            );

        device_keys
    }

    /// Bootstrap Cross-Signing
    pub async fn bootstrap_cross_signing(
        &self,
    ) -> (PrivateCrossSigningIdentity, UploadSigningKeysRequest, SignatureUploadRequest) {
        PrivateCrossSigningIdentity::with_account(self).await
    }

    /// Sign the given CrossSigning Key in place
    pub async fn sign_cross_signing_key(
        &self,
        cross_signing_key: &mut CrossSigningKey,
    ) -> Result<(), SignatureError> {
        let signature = self.sign_json(serde_json::to_value(&cross_signing_key)?).await;

        cross_signing_key
            .signatures
            .entry(self.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.device_id()).to_string(),
                signature,
            );

        Ok(())
    }

    /// Sign the given Master Key
    pub async fn sign_master_key(
        &self,
        master_key: MasterPubkey,
    ) -> Result<SignatureUploadRequest, SignatureError> {
        let public_key =
            master_key.get_first_key().ok_or(SignatureError::MissingSigningKey)?.into();
        let mut cross_signing_key: CrossSigningKey = master_key.into();
        cross_signing_key.signatures.clear();
        self.sign_cross_signing_key(&mut cross_signing_key).await?;

        let mut user_signed_keys = SignedKeys::new();
        user_signed_keys.add_cross_signing_keys(public_key, Raw::new(&cross_signing_key)?);

        let signed_keys = [(self.user_id().to_owned(), user_signed_keys)].into();
        Ok(SignatureUploadRequest::new(signed_keys))
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
    pub async fn sign_json(&self, mut json: Value) -> String {
        let object = json.as_object_mut().expect("Canonical json value isn't an object");
        object.remove("unsigned");
        object.remove("signatures");

        let canonical_json: CanonicalJsonValue =
            json.try_into().expect("Can't canonicalize the json value");

        self.sign(&canonical_json.to_string()).await
    }

    /// Generate a Map of One-Time-Keys for each DeviceKeyId
    pub async fn signed_one_time_keys_helper(&self) -> BTreeMap<Box<DeviceKeyId>, Raw<OneTimeKey>> {
        let one_time_keys = self.one_time_keys().await;
        let mut one_time_key_map = BTreeMap::new();

        for (key_id, key) in one_time_keys.into_iter() {
            let mut signed_key = SignedKey::new(Base64::new(key.to_vec()), BTreeMap::new());

            let key_json =
                serde_json::to_value(&signed_key).expect("Can't serialize a new signed key");

            let signature = BTreeMap::from([(
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.device_id()),
                self.sign_json(key_json).await,
            )]);

            signed_key.signatures.insert(self.user_id().to_owned(), signature);

            one_time_key_map.insert(
                DeviceKeyId::from_parts(
                    DeviceKeyAlgorithm::SignedCurve25519,
                    &Box::<DeviceId>::from(key_id.to_base64()),
                ),
                Raw::new(&OneTimeKey::SignedKey(signed_key))
                    .expect("Couldn't serialize a new signed key"),
            );
        }

        one_time_key_map
    }

    /// Generate, sign and prepare one-time keys to be uploaded.
    ///
    /// If no one-time keys need to be uploaded returns an empty error.
    pub async fn signed_one_time_keys(
        &self,
    ) -> Result<BTreeMap<Box<DeviceKeyId>, Raw<OneTimeKey>>, ()> {
        let _ = self.generate_one_time_keys().await?;

        Ok(self.signed_one_time_keys_helper().await)
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
    pub async fn create_outbound_session_helper(
        &self,
        identity_key: Curve25519PublicKey,
        one_time_key: Curve25519PublicKey,
        fallback_used: bool,
    ) -> Session {
        let session = self.inner.lock().await.create_outbound_session(identity_key, one_time_key);

        let now = Instant::now();
        let session_id = session.session_id();

        Session {
            user_id: self.user_id.clone(),
            device_id: self.device_id.clone(),
            our_identity_keys: self.identity_keys.clone(),
            inner: Arc::new(Mutex::new(session)),
            session_id: session_id.into(),
            sender_key: identity_key.to_base64().into(),
            created_using_fallback_key: fallback_used,
            creation_time: Arc::new(now),
            last_use_time: Arc::new(now),
        }
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
    /// * `key_map` - A map from the algorithm and device id to the one-time key
    ///   that the other account created and shared with us.
    pub async fn create_outbound_session(
        &self,
        device: ReadOnlyDevice,
        key_map: &BTreeMap<Box<DeviceKeyId>, Raw<OneTimeKey>>,
    ) -> Result<Session, SessionCreationError> {
        let one_time_key = key_map.values().next().ok_or_else(|| {
            SessionCreationError::OneTimeKeyMissing(
                device.user_id().to_owned(),
                device.device_id().into(),
            )
        })?;

        let one_time_key: SignedKey = match one_time_key.deserialize() {
            Ok(OneTimeKey::SignedKey(k)) => k,
            Ok(OneTimeKey::Key(_)) => {
                return Err(SessionCreationError::OneTimeKeyNotSigned(
                    device.user_id().to_owned(),
                    device.device_id().into(),
                ));
            }
            Ok(_) => {
                return Err(SessionCreationError::OneTimeKeyUnknown(
                    device.user_id().to_owned(),
                    device.device_id().into(),
                ));
            }
            Err(e) => return Err(SessionCreationError::InvalidJson(e)),
        };

        device.verify_one_time_key(&one_time_key).map_err(|e| {
            SessionCreationError::InvalidSignature(
                device.user_id().to_owned(),
                device.device_id().into(),
                e,
            )
        })?;

        let identity_key = device.get_key(DeviceKeyAlgorithm::Curve25519).ok_or_else(|| {
            SessionCreationError::DeviceMissingCurveKey(
                device.user_id().to_owned(),
                device.device_id().into(),
            )
        })?;

        let identity_key = Curve25519PublicKey::from_base64(identity_key)?;
        let is_fallback = one_time_key.fallback;
        let one_time_key = Curve25519PublicKey::from_slice(one_time_key.key.as_bytes())?;

        Ok(self.create_outbound_session_helper(identity_key, one_time_key, is_fallback).await)
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
        message: &PreKeyMessage,
    ) -> Result<InboundCreationResult, ()> {
        let their_identity_key = Curve25519PublicKey::from_base64(their_identity_key).unwrap();
        let result =
            self.inner.lock().await.create_inbound_session(&their_identity_key, message).unwrap();

        let now = Instant::now();
        let session_id = result.session.session_id();

        let session = Session {
            user_id: self.user_id.clone(),
            device_id: self.device_id.clone(),
            our_identity_keys: self.identity_keys.clone(),
            inner: Arc::new(Mutex::new(result.session)),
            session_id: session_id.into(),
            sender_key: their_identity_key.to_base64().into(),
            created_using_fallback_key: false,
            creation_time: Arc::new(now),
            last_use_time: Arc::new(now),
        };

        Ok(InboundCreationResult { session, plaintext: result.plaintext })
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
    ///
    /// * `settings` - Settings determining the algorithm and rotation period of
    /// the outbound group session.
    pub async fn create_group_session_pair(
        &self,
        room_id: &RoomId,
        settings: EncryptionSettings,
    ) -> Result<(OutboundGroupSession, InboundGroupSession), ()> {
        if settings.algorithm != EventEncryptionAlgorithm::MegolmV1AesSha2 {
            return Err(());
        }

        let visibility = settings.history_visibility.clone();

        let outbound = OutboundGroupSession::new(
            self.device_id.clone(),
            self.identity_keys.clone(),
            room_id,
            settings,
        );
        let identity_keys = self.identity_keys();

        let sender_key = identity_keys.curve25519.to_base64();
        let signing_key = identity_keys.ed25519.to_base64();

        let inbound = InboundGroupSession::new(
            &sender_key,
            &signing_key,
            room_id,
            outbound.session_key().await,
            Some(visibility),
        )
        .expect("Can't create inbound group session from a newly created outbound group session");

        Ok((outbound, inbound))
    }

    #[cfg(any(test, feature = "testing"))]
    #[allow(dead_code)]
    /// Testing only facility to create a group session pair with default
    /// settings
    pub async fn create_group_session_pair_with_defaults(
        &self,
        room_id: &RoomId,
    ) -> (OutboundGroupSession, InboundGroupSession) {
        self.create_group_session_pair(room_id, EncryptionSettings::default())
            .await
            .expect("Can't create default group session pair")
    }

    #[cfg(any(test, feature = "testing"))]
    #[allow(dead_code)]
    /// Testing only helper to create a session for the given Account
    pub async fn create_session_for(&self, other: &ReadOnlyAccount) -> (Session, Session) {
        use ruma::events::{dummy::ToDeviceDummyEventContent, AnyToDeviceEventContent};

        other.generate_one_time_keys_helper(1).await;
        let one_time = other.signed_one_time_keys().await.unwrap();

        let device = ReadOnlyDevice::from_account(other).await;

        let mut our_session =
            self.create_outbound_session(device.clone(), &one_time).await.unwrap();

        other.mark_keys_as_published().await;

        let message = our_session
            .encrypt(&device, AnyToDeviceEventContent::Dummy(ToDeviceDummyEventContent::new()))
            .await
            .unwrap();
        let content = if let EncryptedEventScheme::OlmV1Curve25519AesSha2(c) = message.scheme {
            c
        } else {
            panic!("Invalid encrypted event algorithm");
        };

        let own_ciphertext =
            content.ciphertext.get(&other.identity_keys.curve25519.to_base64()).unwrap();
        let message_type: u8 = own_ciphertext.message_type.try_into().unwrap();

        let message = OlmMessage::from_parts(message_type.into(), &own_ciphertext.body).unwrap();

        let prekey = if let OlmMessage::PreKey(m) = message.clone() {
            m
        } else {
            panic!("Wrong Olm message type");
        };

        let our_device = ReadOnlyDevice::from_account(self).await;
        let other_session = other
            .create_inbound_session(
                our_device
                    .keys()
                    .get(&DeviceKeyId::from_parts(
                        DeviceKeyAlgorithm::Curve25519,
                        our_device.device_id(),
                    ))
                    .unwrap(),
                &prekey,
            )
            .await
            .unwrap();

        (our_session, other_session.session)
    }
}

impl PartialEq for ReadOnlyAccount {
    fn eq(&self, other: &Self) -> bool {
        self.identity_keys() == other.identity_keys() && self.shared() == other.shared()
    }
}

#[cfg(test)]
mod test {
    use std::{collections::BTreeSet, ops::Deref};

    use matrix_sdk_test::async_test;
    use ruma::{device_id, user_id, DeviceId, DeviceKeyId, UserId};

    use super::ReadOnlyAccount;
    use crate::error::OlmResult as Result;

    fn user_id() -> &'static UserId {
        user_id!("@alice:localhost")
    }

    fn device_id() -> &'static DeviceId {
        device_id!("DEVICEID")
    }

    #[async_test]
    async fn one_time_key_creation() -> Result<()> {
        let account = ReadOnlyAccount::new(user_id(), device_id());

        let one_time_keys = account
            .keys_for_upload()
            .await
            .map(|(_, k)| k)
            .expect("Initial keys can't be generated");

        assert!(!one_time_keys.is_empty());

        let second_one_time_keys = account
            .keys_for_upload()
            .await
            .map(|(_, k)| k)
            .expect("Second round of one-time keys isn't generated");

        assert!(!second_one_time_keys.is_empty());

        let device_key_ids: BTreeSet<&DeviceKeyId> =
            one_time_keys.keys().map(Deref::deref).collect();
        let second_device_key_ids: BTreeSet<&DeviceKeyId> =
            second_one_time_keys.keys().map(Deref::deref).collect();

        assert_eq!(device_key_ids, second_device_key_ids);

        account.mark_keys_as_published().await;
        account.update_uploaded_key_count(50);

        let third_one_time_keys = account.keys_for_upload().await.map(|(_, k)| k).unwrap();

        assert!(third_one_time_keys.is_empty());

        account.update_uploaded_key_count(0);

        let fourth_one_time_keys = account
            .keys_for_upload()
            .await
            .map(|(_, k)| k)
            .expect("Fourth round of one-time keys isn't generated");

        let fourth_device_key_ids: BTreeSet<&DeviceKeyId> =
            fourth_one_time_keys.keys().map(Deref::deref).collect();

        assert_ne!(device_key_ids, fourth_device_key_ids);
        Ok(())
    }
}
