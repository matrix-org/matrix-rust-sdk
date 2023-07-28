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
    fmt,
    ops::Deref,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use ruma::{
    api::client::keys::{
        upload_keys,
        upload_signatures::v3::{Request as SignatureUploadRequest, SignedKeys},
    },
    events::AnyToDeviceEvent,
    serde::Raw,
    DeviceId, DeviceKeyAlgorithm, DeviceKeyId, MilliSecondsSinceUnixEpoch, OwnedDeviceId,
    OwnedDeviceKeyId, OwnedUserId, RoomId, SecondsSinceUnixEpoch, UInt, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::{value::RawValue as RawJsonValue, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::{debug, field::debug, info, instrument, trace, warn, Span};
use vodozemac::{
    olm::{
        Account as InnerAccount, AccountPickle, IdentityKeys, OlmMessage,
        OneTimeKeyGenerationResult, PreKeyMessage, SessionConfig,
    },
    Curve25519PublicKey, Ed25519Signature, KeyId, PickleError,
};

use super::{
    utility::SignJson, EncryptionSettings, InboundGroupSession, OutboundGroupSession,
    PrivateCrossSigningIdentity, Session, SessionCreationError as MegolmSessionCreationError,
};
#[cfg(feature = "experimental-algorithms")]
use crate::types::events::room::encrypted::OlmV2Curve25519AesSha2Content;
use crate::{
    error::{EventError, OlmResult, SessionCreationError},
    identities::ReadOnlyDevice,
    requests::UploadSigningKeysRequest,
    store::{Changes, Store},
    types::{
        events::{
            olm_v1::AnyDecryptedOlmEvent,
            room::encrypted::{
                EncryptedToDeviceEvent, OlmV1Curve25519AesSha2Content,
                ToDeviceEncryptedEventContent,
            },
        },
        CrossSigningKey, DeviceKeys, EventEncryptionAlgorithm, MasterPubkey, OneTimeKey, SignedKey,
    },
    utilities::encode,
    CryptoStoreError, OlmError, SignatureError,
};

#[derive(Debug, Clone)]
pub(crate) struct Account {
    pub inner: ReadOnlyAccount,
    pub store: Store,
}

#[derive(Debug, Clone)]
pub(crate) enum SessionType {
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

/// A struct witnessing a successful decryption of an Olm-encrypted to-device
/// event.
///
/// Contains the decrypted event plaintext along with some associated metadata,
/// such as the identity (Curve25519) key of the to-device event sender.
#[derive(Debug)]
pub(crate) struct OlmDecryptionInfo {
    pub session: SessionType,
    pub message_hash: OlmMessageHash,
    pub inbound_group_session: Option<InboundGroupSession>,
    pub result: DecryptionResult,
}

#[derive(Debug)]
pub(crate) struct DecryptionResult {
    // AnyDecryptedOlmEvent is pretty big at 512 bytes, box it to reduce stack size
    pub event: Box<AnyDecryptedOlmEvent>,
    pub raw_event: Raw<AnyToDeviceEvent>,
    pub sender_key: Curve25519PublicKey,
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
    fn new(sender_key: Curve25519PublicKey, ciphertext: &OlmMessage) -> Self {
        let (message_type, ciphertext) = ciphertext.clone().to_parts();
        let sender_key = sender_key.to_base64();

        let sha = Sha256::new()
            .chain_update(sender_key.as_bytes())
            .chain_update([message_type as u8])
            .chain_update(ciphertext)
            .finalize();

        Self { sender_key, hash: encode(sha.as_slice()) }
    }
}

impl Deref for Account {
    type Target = ReadOnlyAccount;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Account {
    pub async fn save(&self) -> Result<(), CryptoStoreError> {
        self.store.save_account(self.inner.clone()).await
    }

    async fn decrypt_olm_helper(
        &self,
        sender: &UserId,
        sender_key: Curve25519PublicKey,
        ciphertext: &OlmMessage,
    ) -> OlmResult<OlmDecryptionInfo> {
        let message_hash = OlmMessageHash::new(sender_key, ciphertext);

        match self.decrypt_olm_message(sender, sender_key, ciphertext).await {
            Ok((session, result)) => {
                Ok(OlmDecryptionInfo { session, message_hash, result, inbound_group_session: None })
            }
            Err(OlmError::SessionWedged(user_id, sender_key)) => {
                if self.store.is_message_known(&message_hash).await? {
                    info!(?sender_key, "An Olm message got replayed, decryption failed");
                    Err(OlmError::ReplayedMessage(user_id, sender_key))
                } else {
                    Err(OlmError::SessionWedged(user_id, sender_key))
                }
            }
            Err(e) => Err(e),
        }
    }

    #[cfg(feature = "experimental-algorithms")]
    async fn decrypt_olm_v2(
        &self,
        sender: &UserId,
        content: &OlmV2Curve25519AesSha2Content,
    ) -> OlmResult<OlmDecryptionInfo> {
        self.decrypt_olm_helper(sender, content.sender_key, &content.ciphertext).await
    }

    #[instrument(skip_all, fields(sender, sender_key = %content.sender_key))]
    async fn decrypt_olm_v1(
        &self,
        sender: &UserId,
        content: &OlmV1Curve25519AesSha2Content,
    ) -> OlmResult<OlmDecryptionInfo> {
        if content.recipient_key != self.identity_keys().curve25519 {
            warn!("Olm event doesn't contain a ciphertext for our key");

            Err(EventError::MissingCiphertext.into())
        } else {
            Box::pin(self.decrypt_olm_helper(sender, content.sender_key, &content.ciphertext)).await
        }
    }

    #[instrument(skip_all, fields(algorithm = ?event.content.algorithm()))]
    pub(crate) async fn decrypt_to_device_event(
        &self,
        event: &EncryptedToDeviceEvent,
    ) -> OlmResult<OlmDecryptionInfo> {
        trace!("Decrypting a to-device event");

        match &event.content {
            ToDeviceEncryptedEventContent::OlmV1Curve25519AesSha2(c) => {
                self.decrypt_olm_v1(&event.sender, c).await
            }
            #[cfg(feature = "experimental-algorithms")]
            ToDeviceEncryptedEventContent::OlmV2Curve25519AesSha2(c) => {
                self.decrypt_olm_v2(&event.sender, c).await
            }
            ToDeviceEncryptedEventContent::Unknown(_) => {
                warn!(
                    "Error decrypting an to-device event, unsupported \
                    encryption algorithm"
                );

                Err(EventError::UnsupportedAlgorithm.into())
            }
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
        // First mark the current keys as published, as updating the key counts might
        // generate some new keys if we're still below the limit.
        self.inner.mark_keys_as_published().await;
        self.update_key_counts(&response.one_time_key_counts, None).await;
        self.store.save_account(self.inner.clone()).await?;

        Ok(())
    }

    /// Try to decrypt an Olm message.
    ///
    /// This try to decrypt an Olm message using all the sessions we share
    /// with the given sender.
    async fn decrypt_with_existing_sessions(
        &self,
        sender_key: Curve25519PublicKey,
        message: &OlmMessage,
    ) -> OlmResult<Option<(Session, String)>> {
        let s = self.store.get_sessions(&sender_key.to_base64()).await?;

        let Some(sessions) = s else {
            // We don't have any existing sessions, return early.
            return Ok(None);
        };

        let mut decrypted: Option<(Session, String)> = None;

        // Try to decrypt the message using each Session we share with the
        // given curve25519 sender key.
        for session in &mut *sessions.lock().await {
            if let Ok(p) = session.decrypt(message).await {
                decrypted = Some((session.clone(), p));
                break;
            } else if let OlmMessage::PreKey(message) = message {
                if message.session_id() == session.session_id() {
                    // The message was intended for this session, but we weren't able to decrypt it.
                    //
                    // We're going to return early here since no other session will be able to
                    // decrypt this message, nor should we try to create a new one since we had
                    // already previously created a `Session` with such a pre-key message.
                    //
                    // Creating this session would have likely failed anyway since the corresponding
                    // one-time key would've been already used up in the previous session creation
                    // operation. The one exception where this would not be so is if the fallback
                    // key was used for creating the session in lieu of an OTK.
                    return Err(OlmError::SessionWedged(
                        session.user_id.to_owned(),
                        session.sender_key(),
                    ));
                }
            } else {
                // An error here is completely normal, after all we don't know
                // which session was used to encrypt a message. We will log a
                // warning if no session was able to decrypt the message.
                continue;
            }
        }

        Ok(decrypted)
    }

    /// Decrypt an Olm message, creating a new Olm session if possible.
    #[instrument(skip(self, message))]
    async fn decrypt_olm_message(
        &self,
        sender: &UserId,
        sender_key: Curve25519PublicKey,
        message: &OlmMessage,
    ) -> OlmResult<(SessionType, DecryptionResult)> {
        // First try to decrypt using an existing session.
        let (session, plaintext) = if let Some(d) =
            self.decrypt_with_existing_sessions(sender_key, message).await?
        {
            // Decryption succeeded, de-structure the session/plaintext out of
            // the Option.
            (SessionType::Existing(d.0), d.1)
        } else {
            // Decryption failed with every known session, let's try to create a
            // new session.
            match message {
                // A new session can only be created using a pre-key message,
                // return with an error if it isn't one.
                OlmMessage::Normal(_) => {
                    let session_ids = if let Some(sessions) =
                        self.store.get_sessions(&sender_key.to_base64()).await?
                    {
                        sessions.lock().await.iter().map(|s| s.session_id().to_owned()).collect()
                    } else {
                        vec![]
                    };

                    warn!(
                        ?session_ids,
                        "Failed to decrypt a non-pre-key message with all available sessions",
                    );

                    return Err(OlmError::SessionWedged(sender.to_owned(), sender_key));
                }

                OlmMessage::PreKey(m) => {
                    // Create the new session.
                    let result = match self.inner.create_inbound_session(sender_key, m).await {
                        Ok(r) => r,
                        Err(_) => {
                            return Err(OlmError::SessionWedged(sender.to_owned(), sender_key));
                        }
                    };

                    // We need to add the new session to the session cache, otherwise
                    // we might try to create the same session again.
                    // TODO: separate the session cache from the storage so we only add
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

        {
            let session_id = match &session {
                SessionType::New(s) => s.session_id(),
                SessionType::Existing(s) => s.session_id(),
            };

            Span::current().record("session_id", session_id);
            trace!("Successfully decrypted an Olm message");
        }

        match self.parse_decrypted_to_device_event(sender, sender_key, plaintext).await {
            Ok(result) => Ok((session, result)),
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
                    error = ?e,
                    "A to-device message was successfully decrypted but \
                    parsing and checking the event fields failed"
                );

                Err(e)
            }
        }
    }

    /// Parse the decrypted plaintext as JSON and verify that it wasn't
    /// forwarded by a third party.
    ///
    /// These checks are mandated by the spec[1]:
    ///
    /// > Other properties are included in order to prevent an attacker from
    /// > publishing someone else's Curve25519 keys as their own and
    /// > subsequently claiming to have sent messages which they didn't.
    /// > sender must correspond to the user who sent the event, recipient to
    /// > the local user, and recipient_keys to the local Ed25519 key.
    ///
    /// # Arguments
    ///
    /// * `sender` -  The `sender` field from the top level of the received
    ///   event.
    /// * `sender_key` - The `sender_key` from the cleartext `content` of the
    ///   received event (which should also have been used to find or establish
    ///   the Olm session that was used to decrypt the event -- so it is
    ///   guaranteed to be correct).
    /// * `plaintext` - The decrypted content of the event.
    async fn parse_decrypted_to_device_event(
        &self,
        sender: &UserId,
        sender_key: Curve25519PublicKey,
        plaintext: String,
    ) -> OlmResult<DecryptionResult> {
        let event: Box<AnyDecryptedOlmEvent> = serde_json::from_str(&plaintext)?;
        let identity_keys = self.inner.identity_keys();

        if event.recipient() != self.user_id() {
            Err(EventError::MismatchedSender(
                event.recipient().to_owned(),
                self.user_id().to_owned(),
            )
            .into())
        }
        // Check that the `sender` in the decrypted to-device event matches that at the
        // top level of the encrypted event.
        else if event.sender() != sender {
            Err(EventError::MismatchedSender(event.sender().to_owned(), sender.to_owned()).into())
        } else if identity_keys.ed25519 != event.recipient_keys().ed25519 {
            Err(EventError::MismatchedKeys(
                identity_keys.ed25519.into(),
                event.recipient_keys().ed25519.into(),
            )
            .into())
        } else {
            // If this event is an `m.room_key` event, defer the check for the
            // Ed25519 key of the sender until we decrypt room events. This
            // ensures that we receive the room key even if we don't have access
            // to the device.
            if !matches!(*event, AnyDecryptedOlmEvent::RoomKey(_)) {
                let Some(device) =
                    self.store.get_device_from_curve_key(event.sender(), sender_key).await?
                else {
                    return Err(EventError::MissingSigningKey.into());
                };

                let Some(key) = device.ed25519_key() else {
                    return Err(EventError::MissingSigningKey.into());
                };

                if key != event.keys().ed25519 {
                    return Err(EventError::MismatchedKeys(
                        key.into(),
                        event.keys().ed25519.into(),
                    )
                    .into());
                }
            }

            Ok(DecryptionResult {
                event,
                raw_event: Raw::from_json(RawJsonValue::from_string(plaintext)?),
                sender_key,
            })
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
    pub user_id: OwnedUserId,
    /// The device_id of this entry
    pub device_id: OwnedDeviceId,
    inner: Arc<Mutex<InnerAccount>>,
    /// The associated identity keys
    pub identity_keys: Arc<IdentityKeys>,
    shared: Arc<AtomicBool>,
    /// The number of signed one-time keys we have uploaded to the server. If
    /// this is None, no action will be taken. After a sync request the client
    /// needs to set this for us, depending on the count we will suggest the
    /// client to upload new keys.
    uploaded_signed_key_count: Arc<AtomicU64>,
    // The creation time of the account in milliseconds since epoch.
    creation_local_time: MilliSecondsSinceUnixEpoch,
}

/// A pickled version of an `Account`.
///
/// Holds all the information that needs to be stored in a database to restore
/// an account.
#[derive(Serialize, Deserialize)]
#[allow(missing_debug_implementations)]
pub struct PickledAccount {
    /// The user id of the account owner.
    pub user_id: OwnedUserId,
    /// The device ID of the account owner.
    pub device_id: OwnedDeviceId,
    /// The pickled version of the Olm account.
    pub pickle: AccountPickle,
    /// Was the account shared.
    pub shared: bool,
    /// The number of uploaded one-time keys we have on the server.
    pub uploaded_signed_key_count: u64,
    /// The local time creation of this account (milliseconds since epoch), used
    /// as creation time of own device
    #[serde(default = "default_account_creation_time")]
    pub creation_local_time: MilliSecondsSinceUnixEpoch,
}

fn default_account_creation_time() -> MilliSecondsSinceUnixEpoch {
    MilliSecondsSinceUnixEpoch(UInt::default())
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
        #[cfg(feature = "experimental-algorithms")]
        &EventEncryptionAlgorithm::OlmV2Curve25519AesSha2,
        &EventEncryptionAlgorithm::MegolmV1AesSha2,
        #[cfg(feature = "experimental-algorithms")]
        &EventEncryptionAlgorithm::MegolmV2AesSha2,
    ];

    fn new_helper(mut account: InnerAccount, user_id: &UserId, device_id: &DeviceId) -> Self {
        let identity_keys = account.identity_keys();

        // Let's generate some initial one-time keys while we're here. Since we know
        // that this is a completely new [`Account`] we're certain that the
        // server does not yet have any one-time keys of ours.
        //
        // This ensures we upload one-time keys along with our device keys right
        // away, rather than waiting for the key counts to be echoed back to us
        // from the server.
        //
        // It would be nice to do this for the fallback key as well but we can't assume
        // that the server supports fallback keys. Maybe one of these days we
        // will be able to do so.
        account.generate_one_time_keys(account.max_number_of_one_time_keys());

        Self {
            user_id: user_id.into(),
            device_id: device_id.into(),
            inner: Arc::new(Mutex::new(account)),
            identity_keys: Arc::new(identity_keys),
            shared: Arc::new(AtomicBool::new(false)),
            uploaded_signed_key_count: Arc::new(AtomicU64::new(0)),
            creation_local_time: MilliSecondsSinceUnixEpoch::now(),
        }
    }

    /// Create a fresh new account, this will generate the identity key-pair.
    pub fn with_device_id(user_id: &UserId, device_id: &DeviceId) -> Self {
        let account = InnerAccount::new();

        Self::new_helper(account, user_id, device_id)
    }

    /// Create a new random Olm Account, the long-term Curve25519 identity key
    /// encoded as base64 will be used for the device ID.
    pub fn new(user_id: &UserId) -> Self {
        let account = InnerAccount::new();
        let device_id: OwnedDeviceId = encode(account.identity_keys().curve25519.as_bytes()).into();

        Self::new_helper(account, user_id, &device_id)
    }

    /// Get the user id of the owner of the account.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// Get the device ID that owns this account.
    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// Get the public parts of the identity keys for the account.
    pub fn identity_keys(&self) -> IdentityKeys {
        *self.identity_keys
    }

    /// Get the key ID of our Ed25519 signing key.
    pub fn signing_key_id(&self) -> OwnedDeviceKeyId {
        DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.device_id())
    }

    /// Get the local timestamp creation of the account in secs since epoch
    pub fn creation_local_time(&self) -> MilliSecondsSinceUnixEpoch {
        self.creation_local_time
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
    pub async fn generate_one_time_keys_helper(&self, count: usize) -> OneTimeKeyGenerationResult {
        self.inner.lock().await.generate_one_time_keys(count)
    }

    /// Get the maximum number of one-time keys the account can hold.
    pub async fn max_one_time_keys(&self) -> usize {
        self.inner.lock().await.max_number_of_one_time_keys()
    }

    pub(crate) async fn update_key_counts(
        &self,
        one_time_key_counts: &BTreeMap<DeviceKeyAlgorithm, UInt>,
        unused_fallback_keys: Option<&[DeviceKeyAlgorithm]>,
    ) {
        if let Some(count) = one_time_key_counts.get(&DeviceKeyAlgorithm::SignedCurve25519) {
            let count: u64 = (*count).into();
            let old_count = self.uploaded_key_count();

            // Some servers might always return the key counts in the sync
            // response, we don't want to the logs with noop changes if they do
            // so.
            if count != old_count {
                debug!(
                    "Updated uploaded one-time key count {} -> {count}.",
                    self.uploaded_key_count(),
                );
            }

            self.update_uploaded_key_count(count);
            self.generate_one_time_keys().await;
        }

        if let Some(unused) = unused_fallback_keys {
            if !unused.contains(&DeviceKeyAlgorithm::SignedCurve25519) {
                // Generate a new fallback key if we don't have one.
                self.generate_fallback_key_helper().await;
            }
        }
    }

    /// Generate new one-time keys that need to be uploaded to the server.
    ///
    /// Returns None if no keys need to be uploaded, otherwise the number of
    /// newly generated one-time keys. May return 0 if some one-time keys are
    /// already generated but weren't uploaded.
    ///
    /// Generally `Some` means that keys should be uploaded, while `None` means
    /// that keys should not be uploaded.
    #[instrument(skip_all)]
    pub async fn generate_one_time_keys(&self) -> Option<u64> {
        // Only generate one-time keys if there aren't any, otherwise the caller
        // might have failed to upload them the last time this method was
        // called.
        if self.one_time_keys().await.is_empty() {
            let count = self.uploaded_key_count();
            let max_keys = self.max_one_time_keys().await;

            if count >= max_keys as u64 {
                return None;
            }

            let key_count = (max_keys as u64) - count;
            let key_count: usize = key_count.try_into().unwrap_or(max_keys);

            let result = self.generate_one_time_keys_helper(key_count).await;

            debug!(
                count = key_count,
                discarded_keys = ?result.removed,
                created_keys = ?result.created,
                "Generated new one-time keys"
            );

            Some(key_count as u64)
        } else {
            Some(0)
        }
    }

    pub(crate) async fn generate_fallback_key_helper(&self) {
        let mut account = self.inner.lock().await;

        if account.fallback_key().is_empty() {
            let removed_fallback_key = account.generate_fallback_key();

            debug!(
                ?removed_fallback_key,
                "No unused fallback keys were found on the server, generated a new fallback key.",
            );
        }
    }

    async fn fallback_key(&self) -> HashMap<KeyId, Curve25519PublicKey> {
        self.inner.lock().await.fallback_key()
    }

    /// Get a tuple of device, one-time, and fallback keys that need to be
    /// uploaded.
    ///
    /// If no keys need to be uploaded the `DeviceKeys` will be `None` and the
    /// one-time and fallback keys maps will be empty.
    pub async fn keys_for_upload(
        &self,
    ) -> (
        Option<DeviceKeys>,
        BTreeMap<OwnedDeviceKeyId, Raw<ruma::encryption::OneTimeKey>>,
        BTreeMap<OwnedDeviceKeyId, Raw<ruma::encryption::OneTimeKey>>,
    ) {
        let device_keys = if !self.shared() { Some(self.device_keys().await) } else { None };

        let one_time_keys = self.signed_one_time_keys().await;
        let fallback_keys = self.signed_fallback_keys().await;

        (device_keys, one_time_keys, fallback_keys)
    }

    /// Mark the current set of one-time keys as being published.
    pub async fn mark_keys_as_published(&self) {
        self.inner.lock().await.mark_keys_as_published();
    }

    /// Sign the given string using the accounts signing key.
    ///
    /// Returns the signature as a base64 encoded string.
    pub async fn sign(&self, string: &str) -> Ed25519Signature {
        self.inner.lock().await.sign(string)
    }

    /// Check if the given JSON is signed by this Account key.
    ///
    /// This method should only be used if an object's signature needs to be
    /// checked multiple times, and you'd like to avoid performing the
    /// canonicalization step each time.
    ///
    /// **Note**: Use this method with caution, the `canonical_json` needs to be
    /// correctly canonicalized and make sure that the object you are checking
    /// the signature for is allowed to be signed by our own device.
    #[cfg(any(test, feature = "backups_v1"))]
    pub fn has_signed_raw(
        &self,
        signatures: &crate::types::Signatures,
        canonical_json: &str,
    ) -> Result<(), SignatureError> {
        use crate::olm::utility::VerifyJson;

        let signing_key = self.identity_keys.ed25519;

        signing_key.verify_canonicalized_json(
            &self.user_id,
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.device_id()),
            signatures,
            canonical_json,
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
            creation_local_time: self.creation_local_time,
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
            user_id: (*pickle.user_id).into(),
            device_id: (*pickle.device_id).into(),
            inner: Arc::new(Mutex::new(account)),
            identity_keys: Arc::new(identity_keys),
            shared: Arc::new(AtomicBool::from(pickle.shared)),
            uploaded_signed_key_count: Arc::new(AtomicU64::new(pickle.uploaded_signed_key_count)),
            creation_local_time: pickle.creation_local_time,
        })
    }

    /// Generate the unsigned `DeviceKeys` from this ReadOnlyAccount
    pub fn unsigned_device_keys(&self) -> DeviceKeys {
        let identity_keys = self.identity_keys();
        let keys = BTreeMap::from([
            (
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::Curve25519, &self.device_id),
                identity_keys.curve25519.into(),
            ),
            (
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, &self.device_id),
                identity_keys.ed25519.into(),
            ),
        ]);

        DeviceKeys::new(
            (*self.user_id).to_owned(),
            (*self.device_id).to_owned(),
            Self::ALGORITHMS.iter().map(|a| (**a).clone()).collect(),
            keys,
            Default::default(),
        )
    }

    /// Sign the device keys of the account and return them so they can be
    /// uploaded.
    pub async fn device_keys(&self) -> DeviceKeys {
        let mut device_keys = self.unsigned_device_keys();

        // Create a copy of the device keys containing only fields that will
        // get signed.
        let json_device_keys =
            serde_json::to_value(&device_keys).expect("device key is always safe to serialize");
        let signature = self
            .sign_json(json_device_keys)
            .await
            .expect("Newly created device keys can always be signed");

        device_keys.signatures.add_signature(
            self.user_id().to_owned(),
            DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, &self.device_id),
            signature,
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
        let signature = self.sign_json(serde_json::to_value(&cross_signing_key)?).await?;

        cross_signing_key.signatures.add_signature(
            self.user_id().to_owned(),
            DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.device_id()),
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
            master_key.get_first_key().ok_or(SignatureError::MissingSigningKey)?.to_base64().into();

        let mut cross_signing_key: CrossSigningKey = master_key.as_ref().clone();
        cross_signing_key.signatures.clear();
        self.sign_cross_signing_key(&mut cross_signing_key).await?;

        let mut user_signed_keys = SignedKeys::new();
        user_signed_keys.add_cross_signing_keys(public_key, cross_signing_key.to_raw());

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
    pub async fn sign_json(&self, json: Value) -> Result<Ed25519Signature, SignatureError> {
        self.inner.lock().await.sign_json(json)
    }

    /// Generate, sign and prepare one-time keys to be uploaded.
    ///
    /// If no one-time keys need to be uploaded returns an empty BTreeMap.
    pub async fn signed_one_time_keys(
        &self,
    ) -> BTreeMap<OwnedDeviceKeyId, Raw<ruma::encryption::OneTimeKey>> {
        let one_time_keys = self.one_time_keys().await;

        if one_time_keys.is_empty() {
            BTreeMap::new()
        } else {
            self.signed_keys(one_time_keys, false).await
        }
    }

    /// Sign and prepare fallback keys to be uploaded.
    ///
    /// If no fallback keys need to be uploaded returns an empty BTreeMap.
    pub async fn signed_fallback_keys(
        &self,
    ) -> BTreeMap<OwnedDeviceKeyId, Raw<ruma::encryption::OneTimeKey>> {
        let fallback_key = self.fallback_key().await;

        if fallback_key.is_empty() {
            BTreeMap::new()
        } else {
            self.signed_keys(fallback_key, true).await
        }
    }

    async fn signed_keys(
        &self,
        keys: HashMap<KeyId, Curve25519PublicKey>,
        fallback: bool,
    ) -> BTreeMap<OwnedDeviceKeyId, Raw<ruma::encryption::OneTimeKey>> {
        let mut keys_map = BTreeMap::new();

        for (key_id, key) in keys {
            let signed_key = self.sign_key(key, fallback).await;

            keys_map.insert(
                DeviceKeyId::from_parts(
                    DeviceKeyAlgorithm::SignedCurve25519,
                    key_id.to_base64().as_str().into(),
                ),
                signed_key.into_raw(),
            );
        }

        keys_map
    }

    async fn sign_key(&self, key: Curve25519PublicKey, fallback: bool) -> SignedKey {
        let mut key = if fallback {
            SignedKey::new_fallback(key.to_owned())
        } else {
            SignedKey::new(key.to_owned())
        };

        let signature = self
            .sign_json(serde_json::to_value(&key).expect("Can't serialize a signed key"))
            .await
            .expect("Newly created one-time keys can always be signed");

        key.signatures_mut().add_signature(
            self.user_id().to_owned(),
            DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.device_id()),
            signature,
        );

        key
    }

    /// Create a new session with another account given a one-time key.
    ///
    /// Returns the newly created session or a `OlmSessionError` if creating a
    /// session failed.
    ///
    /// # Arguments
    /// * `config` - The session config that should be used when creating the
    /// Session.
    /// * `identity_key` - The other account's identity/curve25519 key.
    ///
    /// * `one_time_key` - A signed one-time key that the other account
    /// created and shared with us.
    ///
    /// * `fallback_used` - Was the one-time key a fallback key.
    pub async fn create_outbound_session_helper(
        &self,
        config: SessionConfig,
        identity_key: Curve25519PublicKey,
        one_time_key: Curve25519PublicKey,
        fallback_used: bool,
    ) -> Session {
        let session =
            self.inner.lock().await.create_outbound_session(config, identity_key, one_time_key);

        let now = SecondsSinceUnixEpoch::now();
        let session_id = session.session_id();

        Session {
            user_id: self.user_id.clone(),
            device_id: self.device_id.clone(),
            our_identity_keys: self.identity_keys.clone(),
            inner: Arc::new(Mutex::new(session)),
            session_id: session_id.into(),
            sender_key: identity_key,
            created_using_fallback_key: fallback_used,
            creation_time: now,
            last_use_time: now,
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
    /// * `key_map` - A map from the algorithm and device ID to the one-time key
    ///   that the other account created and shared with us.
    pub async fn create_outbound_session(
        &self,
        device: &ReadOnlyDevice,
        key_map: &BTreeMap<OwnedDeviceKeyId, Raw<ruma::encryption::OneTimeKey>>,
    ) -> Result<Session, SessionCreationError> {
        let one_time_key = key_map.values().next().ok_or_else(|| {
            SessionCreationError::OneTimeKeyMissing(
                device.user_id().to_owned(),
                device.device_id().into(),
            )
        })?;

        let one_time_key: SignedKey = match one_time_key.deserialize_as() {
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

        device.verify_one_time_key(&one_time_key).map_err(|error| {
            SessionCreationError::InvalidSignature {
                signing_key: device.ed25519_key(),
                one_time_key: one_time_key.clone(),
                error,
            }
        })?;

        let identity_key = device.curve25519_key().ok_or_else(|| {
            SessionCreationError::DeviceMissingCurveKey(
                device.user_id().to_owned(),
                device.device_id().into(),
            )
        })?;

        let is_fallback = one_time_key.fallback();
        let one_time_key = one_time_key.key();
        let config = device.olm_session_config();

        Ok(self
            .create_outbound_session_helper(config, identity_key, one_time_key, is_fallback)
            .await)
    }

    /// Create a new session with another account given a pre-key Olm message.
    ///
    /// Returns the newly created session or a `OlmSessionError` if creating a
    /// session failed.
    ///
    /// # Arguments
    /// * `their_identity_key` - The other account's identity/curve25519 key.
    ///
    /// * `message` - A pre-key Olm message that was sent to us by the other
    /// account.
    #[instrument(
        skip_all,
        fields(
            message,
            session_id = message.session_id(),
            session,
        )
    )]
    pub async fn create_inbound_session(
        &self,
        their_identity_key: Curve25519PublicKey,
        message: &PreKeyMessage,
    ) -> Result<InboundCreationResult, SessionCreationError> {
        debug!("Creating a new Olm session from a pre-key message");

        let result =
            self.inner.lock().await.create_inbound_session(their_identity_key, message).map_err(
                |e| {
                    warn!("Failed to create a new Olm session from a pre-key message: {e:?}");
                    e
                },
            )?;

        let now = SecondsSinceUnixEpoch::now();
        let session_id = result.session.session_id();

        Span::current().record("session", debug(&result.session));

        trace!("Olm session created successfully");

        let session = Session {
            user_id: self.user_id.clone(),
            device_id: self.device_id.clone(),
            our_identity_keys: self.identity_keys.clone(),
            inner: Arc::new(Mutex::new(result.session)),
            session_id: session_id.into(),
            sender_key: their_identity_key,
            created_using_fallback_key: false,
            creation_time: now,
            last_use_time: now,
        };

        let plaintext = String::from_utf8_lossy(&result.plaintext).to_string();

        Ok(InboundCreationResult { session, plaintext })
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
    ) -> Result<(OutboundGroupSession, InboundGroupSession), MegolmSessionCreationError> {
        trace!(?room_id, algorithm = settings.algorithm.as_str(), "Creating a new room key");

        let visibility = settings.history_visibility.clone();
        let algorithm = settings.algorithm.to_owned();

        let outbound = OutboundGroupSession::new(
            self.device_id.clone(),
            self.identity_keys.clone(),
            room_id,
            settings,
        )?;

        let identity_keys = self.identity_keys();

        let sender_key = identity_keys.curve25519;
        let signing_key = identity_keys.ed25519;

        let inbound = InboundGroupSession::new(
            sender_key,
            signing_key,
            room_id,
            &outbound.session_key().await,
            algorithm,
            Some(visibility),
        )?;

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
        use ruma::events::dummy::ToDeviceDummyEventContent;

        other.generate_one_time_keys_helper(1).await;
        let one_time = other.signed_one_time_keys().await;

        let device = ReadOnlyDevice::from_account(other).await;

        let mut our_session = self.create_outbound_session(&device, &one_time).await.unwrap();

        other.mark_keys_as_published().await;

        let message = our_session
            .encrypt(
                &device,
                "m.dummy",
                serde_json::to_value(ToDeviceDummyEventContent::new()).unwrap(),
                None,
            )
            .await
            .unwrap()
            .deserialize()
            .unwrap();

        #[cfg(feature = "experimental-algorithms")]
        let content = if let ToDeviceEncryptedEventContent::OlmV2Curve25519AesSha2(c) = message {
            c
        } else {
            panic!("Invalid encrypted event algorithm {}", message.algorithm());
        };

        #[cfg(not(feature = "experimental-algorithms"))]
        let content = if let ToDeviceEncryptedEventContent::OlmV1Curve25519AesSha2(c) = message {
            c
        } else {
            panic!("Invalid encrypted event algorithm {}", message.algorithm());
        };

        let prekey = if let OlmMessage::PreKey(m) = content.ciphertext {
            m
        } else {
            panic!("Wrong Olm message type");
        };

        let our_device = ReadOnlyDevice::from_account(self).await;
        let other_session = other
            .create_inbound_session(our_device.curve25519_key().unwrap(), &prekey)
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
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        ops::Deref,
    };

    use anyhow::Result;
    use matrix_sdk_test::async_test;
    use ruma::{
        device_id, user_id, DeviceId, DeviceKeyAlgorithm, DeviceKeyId, MilliSecondsSinceUnixEpoch,
        UserId,
    };
    use serde_json::json;

    use super::ReadOnlyAccount;
    use crate::{
        olm::SignedJsonObject,
        types::{DeviceKeys, SignedKey},
        ReadOnlyDevice,
    };

    fn user_id() -> &'static UserId {
        user_id!("@alice:localhost")
    }

    fn device_id() -> &'static DeviceId {
        device_id!("DEVICEID")
    }

    #[async_test]
    async fn one_time_key_creation() -> Result<()> {
        let account = ReadOnlyAccount::with_device_id(user_id(), device_id());

        let (_, one_time_keys, _) = account.keys_for_upload().await;
        assert!(!one_time_keys.is_empty());

        let (_, second_one_time_keys, _) = account.keys_for_upload().await;
        assert!(!second_one_time_keys.is_empty());

        let device_key_ids: BTreeSet<&DeviceKeyId> =
            one_time_keys.keys().map(Deref::deref).collect();
        let second_device_key_ids: BTreeSet<&DeviceKeyId> =
            second_one_time_keys.keys().map(Deref::deref).collect();

        assert_eq!(device_key_ids, second_device_key_ids);

        account.mark_keys_as_published().await;
        account.update_uploaded_key_count(50);
        account.generate_one_time_keys().await;

        let (_, third_one_time_keys, _) = account.keys_for_upload().await;
        assert!(third_one_time_keys.is_empty());

        account.update_uploaded_key_count(0);
        account.generate_one_time_keys().await;

        let (_, fourth_one_time_keys, _) = account.keys_for_upload().await;
        assert!(!fourth_one_time_keys.is_empty());

        let fourth_device_key_ids: BTreeSet<&DeviceKeyId> =
            fourth_one_time_keys.keys().map(Deref::deref).collect();

        assert_ne!(device_key_ids, fourth_device_key_ids);
        Ok(())
    }

    #[async_test]
    async fn fallback_key_creation() -> Result<()> {
        let account = ReadOnlyAccount::with_device_id(user_id(), device_id());

        let (_, _, fallback_keys) = account.keys_for_upload().await;

        // We don't create fallback keys since we don't know if the server
        // supports them, we need to receive a sync response to decide if we're
        // going to create them or not.
        assert!(fallback_keys.is_empty());

        let one_time_keys = BTreeMap::from([(DeviceKeyAlgorithm::SignedCurve25519, 50u8.into())]);

        // A `None` here means that the server doesn't support fallback keys, no
        // fallback key gets uploaded.
        account.update_key_counts(&one_time_keys, None).await;
        let (_, _, fallback_keys) = account.keys_for_upload().await;
        assert!(fallback_keys.is_empty());

        // The empty array means that the server supports fallback keys but
        // there isn't a unused fallback key on the server. This time we upload
        // a fallback key.
        let unused_fallback_keys = &[];
        account.update_key_counts(&one_time_keys, Some(unused_fallback_keys.as_ref())).await;
        let (_, _, fallback_keys) = account.keys_for_upload().await;
        assert!(!fallback_keys.is_empty());
        account.mark_keys_as_published().await;

        // There's an unused fallback key on the server, nothing to do here.
        let unused_fallback_keys = &[DeviceKeyAlgorithm::SignedCurve25519];
        account.update_key_counts(&one_time_keys, Some(unused_fallback_keys.as_ref())).await;
        let (_, _, fallback_keys) = account.keys_for_upload().await;
        assert!(fallback_keys.is_empty());

        Ok(())
    }

    #[async_test]
    async fn fallback_key_signing() -> Result<()> {
        let key = vodozemac::Curve25519PublicKey::from_base64(
            "7PUPP6Ijt5R8qLwK2c8uK5hqCNF9tOzWYgGaAay5JBs",
        )?;
        let account = ReadOnlyAccount::with_device_id(user_id(), device_id());

        let key = account.sign_key(key, true).await;

        let canonical_key = key.to_canonical_json()?;

        assert_eq!(
            canonical_key,
            "{\"fallback\":true,\"key\":\"7PUPP6Ijt5R8qLwK2c8uK5hqCNF9tOzWYgGaAay5JBs\"}"
        );

        account
            .has_signed_raw(key.signatures(), &canonical_key)
            .expect("Couldn't verify signature");

        let device = ReadOnlyDevice::from_account(&account).await;
        device.verify_one_time_key(&key).expect("The device can verify its own signature");

        Ok(())
    }

    #[async_test]
    async fn test_account_and_device_creation_timestamp() -> Result<()> {
        let now = MilliSecondsSinceUnixEpoch::now();
        let account = ReadOnlyAccount::with_device_id(user_id(), device_id());
        let then = MilliSecondsSinceUnixEpoch::now();

        assert!(account.creation_local_time() >= now);
        assert!(account.creation_local_time() <= then);

        let device = ReadOnlyDevice::from_account(&account).await;
        assert_eq!(account.creation_local_time(), device.first_time_seen_ts());

        Ok(())
    }

    #[async_test]
    async fn fallback_key_signature_verification() -> Result<()> {
        let fallback_key = json!({
            "fallback": true,
            "key": "XPFqtLvBepBmW6jSAbBuJbhEpprBhQOX1IjUu+cnMF4",
            "signatures": {
                "@dkasak_c:matrix.org": {
                    "ed25519:EXPDYDPWZH": "RJCBMJPL5hvjxgq8rmLmqkNOuPsaan7JeL1wsE+gW6R39G894lb2sBmzapHeKCn/KFjmkonPLkICApRDS+zyDw"
                }
            }
        });

        let device_keys = json!({
            "algorithms": [
                "m.olm.v1.curve25519-aes-sha2",
                "m.megolm.v1.aes-sha2"
            ],
            "device_id": "EXPDYDPWZH",
            "keys": {
                "curve25519:EXPDYDPWZH": "k7f3igo0Vrdm88JSSA5d3OCuUfHYELChB2b57aOROB8",
                "ed25519:EXPDYDPWZH": "GdjYI8fxs175gSpYRJkyN6FRfvcyTsNOhJ2OR/Ggp+E"
            },
            "signatures": {
                "@dkasak_c:matrix.org": {
                    "ed25519:EXPDYDPWZH": "kzrtfQMbJXWXQ1uzhybtwFnGk0JJBS4Mg8VPMusMu6U8MPJccwoHVZKo5+owuHTzIodI+GZYqLmMSzvfvsChAA"
                }
            },
            "user_id": "@dkasak_c:matrix.org",
            "unsigned": {}
        });

        let device_keys: DeviceKeys = serde_json::from_value(device_keys).unwrap();
        let device = ReadOnlyDevice::try_from(&device_keys).unwrap();
        let fallback_key: SignedKey = serde_json::from_value(fallback_key).unwrap();

        device
            .verify_one_time_key(&fallback_key)
            .expect("The fallback key should pass the signature verification");

        Ok(())
    }
}
