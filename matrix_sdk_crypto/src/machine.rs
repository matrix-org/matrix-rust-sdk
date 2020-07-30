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

#[cfg(feature = "sqlite-cryptostore")]
use std::path::Path;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    convert::{TryFrom, TryInto},
    mem,
    result::Result as StdResult,
};

#[cfg(feature = "sqlite-cryptostore")]
use super::store::sqlite::SqliteStore;
use super::{
    device::Device,
    error::{EventError, MegolmError, MegolmResult, OlmError, OlmResult},
    olm::{
        Account, GroupSessionKey, IdentityKeys, InboundGroupSession, OlmMessage,
        OutboundGroupSession,
    },
    store::{memorystore::MemoryStore, Result as StoreResult},
    CryptoStore,
};

use matrix_sdk_common::{
    api,
    events::{
        forwarded_room_key::ForwardedRoomKeyEventContent,
        room::{encrypted::EncryptedEventContent, message::MessageEventContent},
        room_key::RoomKeyEventContent,
        room_key_request::RoomKeyRequestEventContent,
        Algorithm, AnySyncRoomEvent, AnyToDeviceEvent, EventType, SyncMessageEvent, ToDeviceEvent,
    },
    identifiers::{DeviceId, RoomId, UserId},
    uuid::Uuid,
    Raw,
};

use api::r0::{
    keys,
    keys::{AlgorithmAndDeviceId, DeviceKeys, KeyAlgorithm, OneTimeKey},
    sync::sync_events::Response as SyncResponse,
    to_device::{send_event_to_device::Request as ToDeviceRequest, DeviceIdOrAllDevices},
};

use serde_json::Value;
use tracing::{debug, error, info, instrument, trace, warn};

/// A map from the algorithm and device id to a one-time key.
///
/// These keys need to be periodically uploaded to the server.
pub type OneTimeKeys = BTreeMap<AlgorithmAndDeviceId, OneTimeKey>;

/// State machine implementation of the Olm/Megolm encryption protocol used for
/// Matrix end to end encryption.
pub struct OlmMachine {
    /// The unique user id that owns this account.
    user_id: UserId,
    /// The unique device id of the device that holds this account.
    device_id: Box<DeviceId>,
    /// Our underlying Olm Account holding our identity keys.
    pub(crate) account: Account,
    /// Store for the encryption keys.
    /// Persists all the encryption keys so a client can resume the session
    /// without the need to create new keys.
    store: Box<dyn CryptoStore>,
    /// The currently active outbound group sessions.
    outbound_group_sessions: HashMap<RoomId, OutboundGroupSession>,
}

// #[cfg_attr(tarpaulin, skip)]
impl std::fmt::Debug for OlmMachine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OlmMachine")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .finish()
    }
}

impl OlmMachine {
    const MAX_TO_DEVICE_MESSAGES: usize = 20;

    /// Create a new memory based OlmMachine.
    ///
    /// The created machine will keep the encryption keys only in memory and
    /// once the object is dropped the keys will be lost.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that owns this machine.
    ///
    /// * `device_id` - The unique id of the device that owns this machine.
    #[allow(clippy::ptr_arg)]
    pub fn new(user_id: &UserId, device_id: &DeviceId) -> Self {
        OlmMachine {
            user_id: user_id.clone(),
            device_id: device_id.into(),
            account: Account::new(user_id, &device_id),
            store: Box::new(MemoryStore::new()),
            outbound_group_sessions: HashMap::new(),
        }
    }

    /// Create a new OlmMachine with the given `CryptoStore`.
    ///
    /// The created machine will keep the encryption keys only in memory and
    /// once the object is dropped the keys will be lost.
    ///
    /// If the store already contains encryption keys for the given user/device
    /// pair those will be re-used. Otherwise new ones will be created and
    /// stored.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that owns this machine.
    ///
    /// * `device_id` - The unique id of the device that owns this machine.
    ///
    /// * `store` - A `Cryptostore` implementation that will be used to store
    /// the encryption keys.
    pub async fn new_with_store(
        user_id: UserId,
        device_id: Box<DeviceId>,
        mut store: Box<dyn CryptoStore>,
    ) -> StoreResult<Self> {
        let account = match store.load_account().await? {
            Some(a) => {
                debug!("Restored account");
                a
            }
            None => {
                debug!("Creating a new account");
                Account::new(&user_id, &device_id)
            }
        };

        Ok(OlmMachine {
            user_id,
            device_id,
            account,
            store,
            outbound_group_sessions: HashMap::new(),
        })
    }

    #[cfg(feature = "sqlite-cryptostore")]
    #[instrument(skip(path, passphrase))]
    /// Create a new machine with the default crypto store.
    ///
    /// The default store uses a SQLite database to store the encryption keys.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that owns this machine.
    ///
    /// * `device_id` - The unique id of the device that owns this machine.
    pub async fn new_with_default_store<P: AsRef<Path>>(
        user_id: &UserId,
        device_id: &DeviceId,
        path: P,
        passphrase: &str,
    ) -> StoreResult<Self> {
        let store =
            SqliteStore::open_with_passphrase(&user_id, device_id, path, passphrase).await?;

        OlmMachine::new_with_store(user_id.to_owned(), device_id.into(), Box::new(store)).await
    }

    /// The unique user id that owns this identity.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// The unique device id of the device that holds this identity.
    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// Get the public parts of the identity keys.
    pub fn identity_keys(&self) -> &IdentityKeys {
        self.account.identity_keys()
    }

    /// Should account or one-time keys be uploaded to the server.
    pub async fn should_upload_keys(&self) -> bool {
        self.account.should_upload_keys().await
    }

    /// Update the count of one-time keys that are currently on the server.
    fn update_key_count(&mut self, count: u64) {
        self.account.update_uploaded_key_count(count);
    }

    /// Receive a successful keys upload response.
    ///
    /// # Arguments
    ///
    /// * `response` - The keys upload response of the request that the client
    /// performed.
    #[instrument]
    pub async fn receive_keys_upload_response(
        &mut self,
        response: &keys::upload_keys::Response,
    ) -> OlmResult<()> {
        if !self.account.shared() {
            debug!("Marking account as shared");
        }
        self.account.mark_as_shared();

        let one_time_key_count = response
            .one_time_key_counts
            .get(&keys::KeyAlgorithm::SignedCurve25519);

        let count: u64 = one_time_key_count.map_or(0, |c| (*c).into());
        debug!(
            "Updated uploaded one-time key count {} -> {}, marking keys as published",
            self.account.uploaded_key_count(),
            count
        );
        self.update_key_count(count);

        self.account.mark_keys_as_published().await;
        self.store.save_account(self.account.clone()).await?;

        Ok(())
    }

    /// Get the user/device pairs for which no Olm session exists.
    ///
    /// Returns a map from the user id, to a map from the device id to a key
    /// algorithm.
    ///
    /// This can be used to make a key claiming request to the server.
    ///
    /// Sessions need to be established between devices so group sessions for a
    /// room can be shared with them.
    ///
    /// This should be called every time a group session needs to be shared.
    ///
    /// The response of a successful key claiming requests needs to be passed to
    /// the `OlmMachine` with the `receive_keys_claim_response()`.
    ///
    /// # Arguments
    ///
    /// `users` - The list of users that we should check if we lack a session
    /// with one of their devices.
    pub async fn get_missing_sessions(
        &mut self,
        users: impl Iterator<Item = &UserId>,
    ) -> OlmResult<BTreeMap<UserId, BTreeMap<Box<DeviceId>, KeyAlgorithm>>> {
        let mut missing = BTreeMap::new();

        for user_id in users {
            let user_devices = self.store.get_user_devices(user_id).await?;

            for device in user_devices.devices() {
                let sender_key = if let Some(k) = device.get_key(KeyAlgorithm::Curve25519) {
                    k
                } else {
                    continue;
                };

                let sessions = self.store.get_sessions(sender_key).await?;

                let is_missing = if let Some(sessions) = sessions {
                    sessions.lock().await.is_empty()
                } else {
                    true
                };

                if is_missing {
                    if !missing.contains_key(user_id) {
                        let _ = missing.insert(user_id.clone(), BTreeMap::new());
                    }

                    let user_map = missing.get_mut(user_id).unwrap();
                    let _ =
                        user_map.insert(device.device_id().into(), KeyAlgorithm::SignedCurve25519);
                }
            }
        }

        Ok(missing)
    }

    /// Receive a successful key claim response and create new Olm sessions with
    /// the claimed keys.
    ///
    /// # Arguments
    ///
    /// * `response` - The response containing the claimed one-time keys.
    pub async fn receive_keys_claim_response(
        &mut self,
        response: &keys::claim_keys::Response,
    ) -> OlmResult<()> {
        // TODO log the failures here

        for (user_id, user_devices) in &response.one_time_keys {
            for (device_id, key_map) in user_devices {
                let device: Device = match self.store.get_device(&user_id, device_id).await {
                    Ok(Some(d)) => d,
                    Ok(None) => {
                        warn!(
                            "Tried to create an Olm session for {} {}, but the device is unknown",
                            user_id, device_id
                        );
                        continue;
                    }
                    Err(e) => {
                        warn!(
                            "Tried to create an Olm session for {} {}, but \
                            can't fetch the device from the store {:?}",
                            user_id, device_id, e
                        );
                        continue;
                    }
                };

                info!("Creating outbound Session for {} {}", user_id, device_id);

                let session = match self.account.create_outbound_session(device, &key_map).await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("{:?}", e);
                        continue;
                    }
                };

                if let Err(e) = self.store.save_sessions(&[session]).await {
                    error!("Failed to store newly created Olm session {}", e);
                    continue;
                }

                // TODO if this session was created because a previous one was
                // wedged queue up a dummy event to be sent out.
                // TODO if this session was created because of a key request,
                // mark the forwarding keys to be sent out
            }
        }
        Ok(())
    }

    async fn handle_devices_from_key_query(
        &mut self,
        device_keys_map: &BTreeMap<UserId, BTreeMap<Box<DeviceId>, DeviceKeys>>,
    ) -> StoreResult<Vec<Device>> {
        let mut changed_devices = Vec::new();

        for (user_id, device_map) in device_keys_map {
            self.store.update_tracked_user(user_id, false).await?;

            for (device_id, device_keys) in device_map.iter() {
                // We don't need our own device in the device store.
                if user_id == &self.user_id && device_id == &self.device_id {
                    continue;
                }

                if user_id != &device_keys.user_id || device_id != &device_keys.device_id {
                    warn!(
                        "Mismatch in device keys payload of device {} from user {}",
                        device_keys.device_id, device_keys.user_id
                    );
                    continue;
                }

                let device = self.store.get_device(&user_id, device_id).await?;

                let device = if let Some(mut device) = device {
                    if let Err(e) = device.update_device(device_keys) {
                        warn!(
                            "Failed to update the device keys for {} {}: {:?}",
                            user_id, device_id, e
                        );
                        continue;
                    }
                    device
                } else {
                    let device = match Device::try_from(device_keys) {
                        Ok(d) => d,
                        Err(e) => {
                            warn!(
                                "Failed to create a new device for {} {}: {:?}",
                                user_id, device_id, e
                            );
                            continue;
                        }
                    };
                    info!("Adding a new device to the device store {:?}", device);
                    device
                };

                changed_devices.push(device);
            }

            let current_devices: HashSet<&DeviceId> =
                device_map.keys().map(|id| id.as_ref()).collect();
            let stored_devices = self.store.get_user_devices(&user_id).await.unwrap();
            let stored_devices_set: HashSet<&DeviceId> = stored_devices.keys().collect();

            let deleted_devices = stored_devices_set.difference(&current_devices);

            for device_id in deleted_devices {
                if let Some(device) = stored_devices.get(device_id) {
                    device.mark_as_deleted();
                    self.store.delete_device(device).await?;
                }
            }
        }

        Ok(changed_devices)
    }

    /// Receive a successful keys query response.
    ///
    /// Returns a list of devices newly discovered devices and devices that
    /// changed.
    ///
    /// # Arguments
    ///
    /// * `response` - The keys query response of the request that the client
    /// performed.
    pub async fn receive_keys_query_response(
        &mut self,
        response: &keys::get_keys::Response,
    ) -> OlmResult<Vec<Device>> {
        let changed_devices = self
            .handle_devices_from_key_query(&response.device_keys)
            .await?;
        self.store.save_devices(&changed_devices).await?;

        Ok(changed_devices)
    }

    /// Get a tuple of device and one-time keys that need to be uploaded.
    ///
    /// Returns an empty error if no keys need to be uploaded.
    pub async fn keys_for_upload(
        &self,
    ) -> StdResult<(Option<DeviceKeys>, Option<OneTimeKeys>), ()> {
        self.account.keys_for_upload().await
    }

    /// Try to decrypt an Olm message.
    ///
    /// This try to decrypt an Olm message using all the sessions we share
    /// have with the given sender.
    async fn try_decrypt_olm_message(
        &mut self,
        sender: &UserId,
        sender_key: &str,
        message: &OlmMessage,
    ) -> OlmResult<Option<String>> {
        let s = self.store.get_sessions(sender_key).await?;

        // We don't have any existing sessions, return early.
        let sessions = if let Some(s) = s {
            s
        } else {
            return Ok(None);
        };

        let mut session_to_save = None;
        let mut plaintext = None;

        for session in &mut *sessions.lock().await {
            let mut matches = false;

            // If this is a pre-key message check if it was encrypted for our
            // session, if it wasn't decryption will fail so no need to try.
            if let OlmMessage::PreKey(m) = &message {
                matches = session.matches(sender_key, m.clone()).await?;

                if !matches {
                    continue;
                }
            }

            let ret = session.decrypt(message.clone()).await;

            if let Ok(p) = ret {
                plaintext = Some(p);
                session_to_save = Some(session.clone());

                break;
            } else {
                // Decryption failed with a matching session, the session is
                // likely wedged and needs to be rotated.
                if matches {
                    warn!(
                        "Found a matching Olm session yet decryption failed
                          for sender {} and sender_key {}",
                        sender, sender_key
                    );
                    return Err(OlmError::SessionWedged);
                }
            }
        }

        if let Some(session) = session_to_save {
            // Decryption was successful, save the new ratchet state of the
            // session that was used to decrypt the message.
            trace!("Saved the new session state for {}", sender);
            self.store.save_sessions(&[session]).await?;
        }

        Ok(plaintext)
    }

    async fn decrypt_olm_message(
        &mut self,
        sender: &UserId,
        sender_key: &str,
        message: OlmMessage,
    ) -> OlmResult<(Raw<AnyToDeviceEvent>, String)> {
        // First try to decrypt using an existing session.
        let plaintext = if let Some(p) = self
            .try_decrypt_olm_message(sender, sender_key, &message)
            .await?
        {
            // Decryption succeeded, de-structure the plaintext out of the
            // Option.
            p
        } else {
            // Decryption failed with every known session, let's try to create a
            // new session.
            let mut session = match &message {
                // A new session can only be created using a pre-key message,
                // return with an error if it isn't one.
                OlmMessage::Message(_) => {
                    warn!(
                        "Failed to decrypt a non-pre-key message with all
                          available sessions {} {}",
                        sender, sender_key
                    );
                    return Err(OlmError::SessionWedged);
                }

                OlmMessage::PreKey(m) => {
                    // Create the new session.
                    let session = match self
                        .account
                        .create_inbound_session(sender_key, m.clone())
                        .await
                    {
                        Ok(s) => s,
                        Err(e) => {
                            warn!(
                                "Failed to create a new Olm session for {} {}
                                      from a prekey message: {}",
                                sender, sender_key, e
                            );
                            return Err(OlmError::SessionWedged);
                        }
                    };

                    // Save the account since we remove the one-time key that
                    // was used to create this session.
                    self.store.save_account(self.account.clone()).await?;
                    session
                }
            };

            // Decrypt our message, this shouldn't fail since we're using a
            // newly created Session.
            let plaintext = session.decrypt(message).await?;

            // Save the new ratcheted state of the session.
            self.store.save_sessions(&[session]).await?;
            plaintext
        };

        trace!("Successfully decrypted a Olm message: {}", plaintext);

        Ok(self.parse_decrypted_to_device_event(sender, &plaintext)?)
    }

    /// Parse a decrypted Olm message, check that the plaintext and encrypted
    /// senders match and that the message was meant for us.
    fn parse_decrypted_to_device_event(
        &self,
        sender: &UserId,
        plaintext: &str,
    ) -> OlmResult<(Raw<AnyToDeviceEvent>, String)> {
        // TODO make the errors a bit more specific.
        let decrypted_json: Value = serde_json::from_str(&plaintext)?;

        let encrytped_sender = decrypted_json
            .get("sender")
            .cloned()
            .ok_or_else(|| EventError::MissingField("sender".to_string()))?;
        let encrytped_sender: UserId = serde_json::from_value(encrytped_sender)?;
        let recipient = decrypted_json
            .get("recipient")
            .cloned()
            .ok_or_else(|| EventError::MissingField("recipient".to_string()))?;
        let recipient: UserId = serde_json::from_value(recipient)?;

        let recipient_keys: BTreeMap<KeyAlgorithm, String> = serde_json::from_value(
            decrypted_json
                .get("recipient_keys")
                .cloned()
                .ok_or_else(|| EventError::MissingField("recipient_keys".to_string()))?,
        )?;
        let keys: BTreeMap<KeyAlgorithm, String> = serde_json::from_value(
            decrypted_json
                .get("keys")
                .cloned()
                .ok_or_else(|| EventError::MissingField("keys".to_string()))?,
        )?;

        if recipient != self.user_id || sender != &encrytped_sender {
            return Err(EventError::MissmatchedSender.into());
        }

        if self.account.identity_keys().ed25519()
            != recipient_keys
                .get(&KeyAlgorithm::Ed25519)
                .ok_or(EventError::MissingSigningKey)?
        {
            return Err(EventError::MissmatchedKeys.into());
        }

        let signing_key = keys
            .get(&KeyAlgorithm::Ed25519)
            .ok_or(EventError::MissingSigningKey)?;

        Ok((
            Raw::from(serde_json::from_value::<AnyToDeviceEvent>(decrypted_json)?),
            signing_key.to_owned(),
        ))
    }

    /// Decrypt a to-device event.
    ///
    /// Returns a decrypted `ToDeviceEvent` if the decryption was successful,
    /// an error indicating why decryption failed otherwise.
    ///
    /// # Arguments
    ///
    /// * `event` - The to-device event that should be decrypted.
    async fn decrypt_to_device_event(
        &mut self,
        event: &ToDeviceEvent<EncryptedEventContent>,
    ) -> OlmResult<Raw<AnyToDeviceEvent>> {
        info!("Decrypting to-device event");

        let content = if let EncryptedEventContent::OlmV1Curve25519AesSha2(c) = &event.content {
            c
        } else {
            warn!("Error, unsupported encryption algorithm");
            return Err(EventError::UnsupportedAlgorithm.into());
        };

        let identity_keys = self.account.identity_keys();
        let own_key = identity_keys.curve25519();
        let own_ciphertext = content.ciphertext.get(own_key);

        // Try to find a ciphertext that was meant for our device.
        if let Some(ciphertext) = own_ciphertext {
            let message_type: u8 = ciphertext
                .message_type
                .try_into()
                .map_err(|_| EventError::UnsupportedOlmType)?;

            // Create a OlmMessage from the ciphertext and the type.
            let message =
                OlmMessage::from_type_and_ciphertext(message_type.into(), ciphertext.body.clone())
                    .map_err(|_| EventError::UnsupportedOlmType)?;

            // Decrypt the OlmMessage and get a Ruma event out of it.
            let (decrypted_event, signing_key) = self
                .decrypt_olm_message(&event.sender, &content.sender_key, message)
                .await?;

            debug!("Decrypted a to-device event {:?}", decrypted_event);

            // Handle the decrypted event, e.g. fetch out Megolm sessions out of
            // the event.
            if let Some(event) = self
                .handle_decrypted_to_device_event(
                    &content.sender_key,
                    &signing_key,
                    &decrypted_event,
                )
                .await?
            {
                // Some events may have sensitive data e.g. private keys, while we
                // want to notify our users that a private key was received we
                // don't want them to be able to do silly things with it. Handling
                // events modifies them and returns a modified one, so replace it
                // here if we get one.
                Ok(event)
            } else {
                Ok(decrypted_event)
            }
        } else {
            warn!("Olm event doesn't contain a ciphertext for our key");
            Err(EventError::MissingCiphertext.into())
        }
    }

    /// Create a group session from a room key and add it to our crypto store.
    async fn add_room_key(
        &mut self,
        sender_key: &str,
        signing_key: &str,
        event: &mut ToDeviceEvent<RoomKeyEventContent>,
    ) -> OlmResult<Option<Raw<AnyToDeviceEvent>>> {
        match event.content.algorithm {
            Algorithm::MegolmV1AesSha2 => {
                let session_key = GroupSessionKey(mem::take(&mut event.content.session_key));

                let session = InboundGroupSession::new(
                    sender_key,
                    signing_key,
                    &event.content.room_id,
                    session_key,
                )?;
                let _ = self.store.save_inbound_group_session(session).await?;

                let event = Raw::from(AnyToDeviceEvent::RoomKey(event.clone()));
                Ok(Some(event))
            }
            _ => {
                warn!(
                    "Received room key with unsupported key algorithm {}",
                    event.content.algorithm
                );
                Ok(None)
            }
        }
    }

    /// Create a new outbound group session.
    ///
    /// This also creates a matching inbound group session and saves that one in
    /// the store.
    async fn create_outbound_group_session(&mut self, room_id: &RoomId) -> OlmResult<()> {
        let (outbound, inbound) = self.account.create_group_session_pair(room_id).await;

        let _ = self.store.save_inbound_group_session(inbound).await?;

        let _ = self
            .outbound_group_sessions
            .insert(room_id.to_owned(), outbound);
        Ok(())
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
    /// * `room_id` - The id of the room for which the message should be
    /// encrypted.
    ///
    /// * `content` - The plaintext content of the message that should be
    /// encrypted.
    ///
    /// # Panics
    ///
    /// Panics if a group session for the given room wasn't shared beforehand.
    pub async fn encrypt(
        &self,
        room_id: &RoomId,
        content: MessageEventContent,
    ) -> MegolmResult<EncryptedEventContent> {
        let session = self.outbound_group_sessions.get(room_id);

        let session = if let Some(s) = session {
            s
        } else {
            panic!("Session wasn't created nor shared");
        };

        if session.expired() {
            panic!("Session is expired");
        }

        Ok(session.encrypt(content).await)
    }

    /// Encrypt the given event for the given Device
    ///
    /// # Arguments
    ///
    /// * `reciepient_device` - The device that the event should be encrypted
    ///     for.
    ///
    /// * `event_type` - The type of the event.
    ///
    /// * `content` - The content of the event that should be encrypted.
    async fn olm_encrypt(
        &mut self,
        recipient_device: &Device,
        event_type: EventType,
        content: Value,
    ) -> OlmResult<EncryptedEventContent> {
        let sender_key = if let Some(k) = recipient_device.get_key(KeyAlgorithm::Curve25519) {
            k
        } else {
            warn!(
                "Trying to encrypt a Megolm session for user {} on device {}, \
                but the device doesn't have a curve25519 key",
                recipient_device.user_id(),
                recipient_device.device_id()
            );
            return Err(EventError::MissingSenderKey.into());
        };

        let mut session = if let Some(s) = self.store.get_sessions(sender_key).await? {
            let session = &s.lock().await[0];
            session.clone()
        } else {
            warn!(
                "Trying to encrypt a Megolm session for user {} on device {}, \
                but no Olm session is found",
                recipient_device.user_id(),
                recipient_device.device_id()
            );
            return Err(OlmError::MissingSession);
        };

        let message = session.encrypt(recipient_device, event_type, content).await;
        self.store.save_sessions(&[session]).await?;

        message
    }

    /// Should the client share a group session for the given room.
    ///
    /// Returns true if a session needs to be shared before room messages can be
    /// encrypted, false if one is already shared and ready to encrypt room
    /// messages.
    ///
    /// This should be called every time a new room message wants to be sent out
    /// since group sessions can expire at any time.
    pub fn should_share_group_session(&self, room_id: &RoomId) -> bool {
        let session = self.outbound_group_sessions.get(room_id);

        match session {
            Some(s) => !s.shared() || s.expired(),
            None => true,
        }
    }

    /// Invalidate the currently active outbound group session for the given
    /// room.
    ///
    /// Returns true if a session was invalidated, false if there was no session
    /// to invalidate.
    pub fn invalidate_group_session(&mut self, room_id: &RoomId) -> bool {
        self.outbound_group_sessions.remove(room_id).is_some()
    }

    // TODO accept an algorithm here
    /// Get to-device requests to share a group session with users in a room.
    ///
    /// # Arguments
    ///
    /// `room_id` - The room id of the room where the group session will be
    /// used.
    ///
    /// `users` - The list of users that should receive the group session.
    pub async fn share_group_session<'a, I>(
        &mut self,
        room_id: &RoomId,
        users: I,
    ) -> OlmResult<Vec<ToDeviceRequest>>
    where
        I: IntoIterator<Item = &'a UserId>,
    {
        self.create_outbound_group_session(room_id).await?;
        let session = self.outbound_group_sessions.get(room_id).unwrap();

        if session.shared() {
            panic!("Session is already shared");
        }

        // TODO don't mark the session as shared automatically only, when all
        // the requests are done, failure to send these requests will likely end
        // up in wedged sessions. We'll need to store the requests and let the
        // caller mark them as sent using an UUID.
        session.mark_as_shared();

        let mut devices = Vec::new();

        for user_id in users {
            for device in self.store.get_user_devices(user_id).await?.devices() {
                // TODO abort if the device isn't verified
                devices.push(device.clone());
            }
        }

        let mut requests = Vec::new();
        let key_content = session.as_json().await;

        for device_map_chunk in devices.chunks(OlmMachine::MAX_TO_DEVICE_MESSAGES) {
            let mut messages = BTreeMap::new();

            for device in device_map_chunk {
                let encrypted = self
                    .olm_encrypt(&device, EventType::RoomKey, key_content.clone())
                    .await;

                let encrypted = match encrypted {
                    Ok(c) => c,
                    Err(OlmError::MissingSession)
                    | Err(OlmError::EventError(EventError::MissingSenderKey)) => {
                        continue;
                    }
                    Err(e) => return Err(e),
                };

                if !messages.contains_key(device.user_id()) {
                    messages.insert(device.user_id().clone(), BTreeMap::new());
                };

                let user_messages = messages.get_mut(device.user_id()).unwrap();

                user_messages.insert(
                    DeviceIdOrAllDevices::DeviceId(device.device_id().into()),
                    serde_json::value::to_raw_value(&encrypted)?,
                );
            }

            requests.push(ToDeviceRequest {
                event_type: EventType::RoomEncrypted,
                txn_id: Uuid::new_v4().to_string(),
                messages,
            });
        }

        Ok(requests)
    }

    fn add_forwarded_room_key(
        &self,
        _sender_key: &str,
        _signing_key: &str,
        _event: &ToDeviceEvent<ForwardedRoomKeyEventContent>,
    ) -> OlmResult<()> {
        Ok(())
        // TODO
    }

    /// Receive and properly handle a decrypted to-device event.
    ///
    /// # Arguments
    ///
    /// * `sender_key` - The sender (curve25519) key of the event sender.
    ///
    /// * `signing_key` - The signing (ed25519) key of the event sender.
    ///
    /// * `event` - The decrypted to-device event.
    async fn handle_decrypted_to_device_event(
        &mut self,
        sender_key: &str,
        signing_key: &str,
        event: &Raw<AnyToDeviceEvent>,
    ) -> OlmResult<Option<Raw<AnyToDeviceEvent>>> {
        let event = if let Ok(e) = event.deserialize() {
            e
        } else {
            warn!("Decrypted to-device event failed to be parsed correctly");
            return Ok(None);
        };

        match event {
            AnyToDeviceEvent::RoomKey(mut e) => {
                Ok(self.add_room_key(sender_key, signing_key, &mut e).await?)
            }
            AnyToDeviceEvent::ForwardedRoomKey(e) => {
                self.add_forwarded_room_key(sender_key, signing_key, &e)?;
                Ok(None)
            }
            _ => {
                warn!("Received a unexpected encrypted to-device event");
                Ok(None)
            }
        }
    }

    fn handle_room_key_request(&self, _: &ToDeviceEvent<RoomKeyRequestEventContent>) {
        // TODO handle room key requests here.
    }

    fn handle_verification_event(&self, _: &AnyToDeviceEvent) {
        // TODO handle to-device verification events here.
    }

    /// Handle a sync response and update the internal state of the Olm machine.
    ///
    /// This will decrypt to-device events but will not touch events in the room
    /// timeline.
    ///
    /// To decrypt an event from the room timeline call `decrypt_room_event()`.
    ///
    /// # Arguments
    ///
    /// * `response` - The sync latest sync response.
    #[instrument(skip(response))]
    pub async fn receive_sync_response(&mut self, response: &mut SyncResponse) {
        let one_time_key_count = response
            .device_one_time_keys_count
            .get(&keys::KeyAlgorithm::SignedCurve25519);

        let count: u64 = one_time_key_count.map_or(0, |c| (*c).into());
        self.update_key_count(count);

        for user_id in &response.device_lists.changed {
            if let Err(e) = self.mark_user_as_changed(&user_id).await {
                error!("Error marking a tracked user as changed {:?}", e);
            }
        }

        for event_result in &mut response.to_device.events {
            let event = if let Ok(e) = event_result.deserialize() {
                e
            } else {
                // Skip invalid events.
                warn!("Received an invalid to-device event {:?}", event_result);
                continue;
            };

            info!("Received a to-device event {:?}", event);

            match &event {
                AnyToDeviceEvent::RoomEncrypted(e) => {
                    let decrypted_event = match self.decrypt_to_device_event(e).await {
                        Ok(e) => e,
                        Err(err) => {
                            warn!(
                                "Failed to decrypt to-device event from {} {}",
                                e.sender, err
                            );
                            // TODO if the session is wedged mark it for
                            // unwedging.
                            continue;
                        }
                    };

                    *event_result = decrypted_event;
                }
                AnyToDeviceEvent::RoomKeyRequest(e) => self.handle_room_key_request(e),
                AnyToDeviceEvent::KeyVerificationAccept(..)
                | AnyToDeviceEvent::KeyVerificationCancel(..)
                | AnyToDeviceEvent::KeyVerificationKey(..)
                | AnyToDeviceEvent::KeyVerificationMac(..)
                | AnyToDeviceEvent::KeyVerificationRequest(..)
                | AnyToDeviceEvent::KeyVerificationStart(..) => {
                    self.handle_verification_event(&event)
                }
                _ => continue,
            }
        }
    }

    /// Decrypt an event from a room timeline.
    ///
    /// # Arguments
    ///
    /// * `event` - The event that should be decrypted.
    ///
    /// * `room_id` - The ID of the room where the event was sent to.
    pub async fn decrypt_room_event(
        &mut self,
        event: &SyncMessageEvent<EncryptedEventContent>,
        room_id: &RoomId,
    ) -> MegolmResult<Raw<AnySyncRoomEvent>> {
        let content = match &event.content {
            EncryptedEventContent::MegolmV1AesSha2(c) => c,
            _ => return Err(EventError::UnsupportedAlgorithm.into()),
        };

        let session = self
            .store
            .get_inbound_group_session(room_id, &content.sender_key, &content.session_id)
            .await?;
        // TODO check if the Olm session is wedged and re-request the key.
        let session = session.ok_or(MegolmError::MissingSession)?;

        // TODO check the message index.
        // TODO check if this is from a verified device.
        let (decrypted_event, _) = session.decrypt(event).await?;

        trace!("Successfully decrypted Megolm event {:?}", decrypted_event);
        // TODO set the encryption info on the event (is it verified, was it
        // decrypted, sender key...)

        Ok(decrypted_event)
    }

    /// Mark that the given user has changed his devices.
    ///
    /// This will queue up the given user for a key query.
    ///
    /// Note: The user already needs to be tracked for it to be queued up for a
    /// key query.
    ///
    /// Returns true if the user was queued up for a key query, false otherwise.
    pub async fn mark_user_as_changed(&mut self, user_id: &UserId) -> StoreResult<bool> {
        if self.store.tracked_users().contains(user_id) {
            self.store.update_tracked_user(user_id, true).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Update the tracked users.
    ///
    /// # Arguments
    ///
    /// * `users` - An iterator over user ids that should be marked for
    /// tracking.
    ///
    /// This will mark users that weren't seen before for a key query and
    /// tracking.
    ///
    /// If the user is already known to the Olm machine it will not be
    /// considered for a key query.
    ///
    /// Use the `mark_user_as_changed()` if the user really needs a key query.
    pub async fn update_tracked_users<'a, I>(&mut self, users: I)
    where
        I: IntoIterator<Item = &'a UserId>,
    {
        for user in users {
            if self.store.tracked_users().contains(user) {
                continue;
            }

            if let Err(e) = self.store.update_tracked_user(user, true).await {
                warn!("Error storing users for tracking {}", e);
            }
        }
    }

    /// Should the client perform a key query request.
    pub fn should_query_keys(&self) -> bool {
        !self.store.users_for_key_query().is_empty()
    }

    /// Get the set of users that we need to query keys for.
    ///
    /// Returns a hash set of users that need to be queried for keys.
    pub fn users_for_key_query(&self) -> HashSet<UserId> {
        self.store.users_for_key_query().clone()
    }
}

#[cfg(test)]
mod test {
    static USER_ID: &str = "@bob:example.org";

    use matrix_sdk_common::js_int::uint;
    use std::{
        collections::BTreeMap,
        convert::{TryFrom, TryInto},
        time::SystemTime,
    };

    use http::Response;
    use serde_json::json;

    use crate::{
        machine::{OlmMachine, OneTimeKeys},
        verify_json, Device,
    };

    use matrix_sdk_common::{
        api::r0::{keys, to_device::send_event_to_device::Request as ToDeviceRequest},
        events::{
            room::{
                encrypted::EncryptedEventContent,
                message::{MessageEventContent, TextMessageEventContent},
            },
            AnySyncMessageEvent, AnySyncRoomEvent, AnyToDeviceEvent, EventType, SyncMessageEvent,
            ToDeviceEvent, Unsigned,
        },
        identifiers::{DeviceId, EventId, RoomId, UserId},
        Raw,
    };
    use matrix_sdk_test::test_json;

    fn alice_id() -> UserId {
        UserId::try_from("@alice:example.org").unwrap()
    }

    fn alice_device_id() -> Box<DeviceId> {
        "JLAFKJWSCS".into()
    }

    fn user_id() -> UserId {
        UserId::try_from(USER_ID).unwrap()
    }

    fn response_from_file(json: &serde_json::Value) -> Response<Vec<u8>> {
        Response::builder()
            .status(200)
            .body(json.to_string().as_bytes().to_vec())
            .unwrap()
    }

    fn keys_upload_response() -> keys::upload_keys::Response {
        let data = response_from_file(&test_json::KEYS_UPLOAD);
        keys::upload_keys::Response::try_from(data).expect("Can't parse the keys upload response")
    }

    fn keys_query_response() -> keys::get_keys::Response {
        let data = response_from_file(&test_json::KEYS_QUERY);
        keys::get_keys::Response::try_from(data).expect("Can't parse the keys upload response")
    }

    fn to_device_requests_to_content(requests: Vec<ToDeviceRequest>) -> EncryptedEventContent {
        let to_device_request = &requests[0];

        let content: Raw<EncryptedEventContent> = serde_json::from_str(
            to_device_request
                .messages
                .values()
                .next()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .get(),
        )
        .unwrap();

        content.deserialize().unwrap()
    }

    async fn get_prepared_machine() -> (OlmMachine, OneTimeKeys) {
        let mut machine = OlmMachine::new(&user_id(), &alice_device_id());
        machine.account.update_uploaded_key_count(0);
        let (_, otk) = machine
            .keys_for_upload()
            .await
            .expect("Can't prepare initial key upload");
        let response = keys_upload_response();
        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();

        (machine, otk.unwrap())
    }

    async fn get_machine_after_query() -> (OlmMachine, OneTimeKeys) {
        let (mut machine, otk) = get_prepared_machine().await;
        let response = keys_query_response();

        machine
            .receive_keys_query_response(&response)
            .await
            .unwrap();

        (machine, otk)
    }

    async fn get_machine_pair() -> (OlmMachine, OlmMachine, OneTimeKeys) {
        let (bob, otk) = get_prepared_machine().await;

        let alice_id = alice_id();
        let alice_device = alice_device_id();
        let alice = OlmMachine::new(&alice_id, &alice_device);

        let alice_deivce = Device::from_machine(&alice).await;
        let bob_device = Device::from_machine(&bob).await;
        alice.store.save_devices(&[bob_device]).await.unwrap();
        bob.store.save_devices(&[alice_deivce]).await.unwrap();

        (alice, bob, otk)
    }

    async fn get_machine_pair_with_session() -> (OlmMachine, OlmMachine) {
        let (mut alice, bob, one_time_keys) = get_machine_pair().await;

        let mut bob_keys = BTreeMap::new();

        let one_time_key = one_time_keys.iter().next().unwrap();
        let mut keys = BTreeMap::new();
        keys.insert(one_time_key.0.clone(), one_time_key.1.clone());
        bob_keys.insert(bob.device_id.clone(), keys);

        let mut one_time_keys = BTreeMap::new();
        one_time_keys.insert(bob.user_id.clone(), bob_keys);

        let response = keys::claim_keys::Response {
            failures: BTreeMap::new(),
            one_time_keys,
        };

        alice.receive_keys_claim_response(&response).await.unwrap();

        (alice, bob)
    }

    async fn get_machine_pair_with_setup_sessions() -> (OlmMachine, OlmMachine) {
        let (mut alice, mut bob) = get_machine_pair_with_session().await;

        let bob_device = alice
            .store
            .get_device(&bob.user_id, &bob.device_id)
            .await
            .unwrap()
            .unwrap();

        let event = ToDeviceEvent {
            sender: alice.user_id.clone(),
            content: alice
                .olm_encrypt(&bob_device, EventType::Dummy, json!({}))
                .await
                .unwrap(),
        };

        bob.decrypt_to_device_event(&event).await.unwrap();

        (alice, bob)
    }

    #[tokio::test]
    async fn create_olm_machine() {
        let machine = OlmMachine::new(&user_id(), &alice_device_id());
        assert!(machine.should_upload_keys().await);
    }

    #[tokio::test]
    async fn receive_keys_upload_response() {
        let mut machine = OlmMachine::new(&user_id(), &alice_device_id());
        let mut response = keys_upload_response();

        response
            .one_time_key_counts
            .remove(&keys::KeyAlgorithm::SignedCurve25519)
            .unwrap();

        assert!(machine.should_upload_keys().await);
        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();
        assert!(machine.should_upload_keys().await);

        response
            .one_time_key_counts
            .insert(keys::KeyAlgorithm::SignedCurve25519, uint!(10));
        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();
        assert!(machine.should_upload_keys().await);

        response
            .one_time_key_counts
            .insert(keys::KeyAlgorithm::SignedCurve25519, uint!(50));
        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();
        assert!(!machine.should_upload_keys().await);
    }

    #[tokio::test]
    async fn generate_one_time_keys() {
        let mut machine = OlmMachine::new(&user_id(), &alice_device_id());

        let mut response = keys_upload_response();

        assert!(machine.should_upload_keys().await);

        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();
        assert!(machine.should_upload_keys().await);
        assert!(machine.account.generate_one_time_keys().await.is_ok());

        response
            .one_time_key_counts
            .insert(keys::KeyAlgorithm::SignedCurve25519, uint!(50));
        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();
        assert!(machine.account.generate_one_time_keys().await.is_err());
    }

    #[tokio::test]
    async fn test_device_key_signing() {
        let machine = OlmMachine::new(&user_id(), &alice_device_id());

        let mut device_keys = machine.account.device_keys().await;
        let identity_keys = machine.account.identity_keys();
        let ed25519_key = identity_keys.ed25519();

        let ret = verify_json(
            &machine.user_id,
            machine.device_id.as_str(),
            ed25519_key,
            &mut json!(&mut device_keys),
        );
        assert!(ret.is_ok());
    }

    #[tokio::test]
    async fn tests_session_invalidation() {
        let mut machine = OlmMachine::new(&user_id(), &alice_device_id());
        let room_id = RoomId::try_from("!test:example.org").unwrap();

        machine
            .create_outbound_group_session(&room_id)
            .await
            .unwrap();
        assert!(machine.outbound_group_sessions.get(&room_id).is_some());

        machine.invalidate_group_session(&room_id);

        assert!(machine.outbound_group_sessions.get(&room_id).is_none());
    }

    #[tokio::test]
    async fn test_invalid_signature() {
        let machine = OlmMachine::new(&user_id(), &alice_device_id());

        let mut device_keys = machine.account.device_keys().await;

        let ret = verify_json(
            &machine.user_id,
            machine.device_id.as_str(),
            "fake_key",
            &mut json!(&mut device_keys),
        );
        assert!(ret.is_err());
    }

    #[tokio::test]
    async fn test_one_time_key_signing() {
        let machine = OlmMachine::new(&user_id(), &alice_device_id());
        machine.account.update_uploaded_key_count(49);

        let mut one_time_keys = machine.account.signed_one_time_keys().await.unwrap();
        let identity_keys = machine.account.identity_keys();
        let ed25519_key = identity_keys.ed25519();

        let mut one_time_key = one_time_keys.values_mut().next().unwrap();

        let ret = verify_json(
            &machine.user_id,
            machine.device_id.as_str(),
            ed25519_key,
            &mut json!(&mut one_time_key),
        );
        assert!(ret.is_ok());
    }

    #[tokio::test]
    async fn test_keys_for_upload() {
        let mut machine = OlmMachine::new(&user_id(), &alice_device_id());
        machine.account.update_uploaded_key_count(0);

        let identity_keys = machine.account.identity_keys();
        let ed25519_key = identity_keys.ed25519();

        let (device_keys, mut one_time_keys) = machine
            .keys_for_upload()
            .await
            .expect("Can't prepare initial key upload");

        let ret = verify_json(
            &machine.user_id,
            machine.device_id.as_str(),
            ed25519_key,
            &mut json!(&mut one_time_keys.as_mut().unwrap().values_mut().next()),
        );
        assert!(ret.is_ok());

        let ret = verify_json(
            &machine.user_id,
            machine.device_id.as_str(),
            ed25519_key,
            &mut json!(&mut device_keys.unwrap()),
        );
        assert!(ret.is_ok());

        let mut response = keys_upload_response();
        response.one_time_key_counts.insert(
            keys::KeyAlgorithm::SignedCurve25519,
            (one_time_keys.unwrap().len() as u64).try_into().unwrap(),
        );

        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();

        let ret = machine.keys_for_upload().await;
        assert!(ret.is_err());
    }

    #[tokio::test]
    async fn test_keys_query() {
        let (mut machine, _) = get_prepared_machine().await;
        let response = keys_query_response();
        let alice_id = UserId::try_from("@alice:example.org").unwrap();
        let alice_device_id: &DeviceId = "JLAFKJWSCS".into();

        let alice_devices = machine.store.get_user_devices(&alice_id).await.unwrap();
        assert!(alice_devices.devices().peekable().peek().is_none());

        machine
            .receive_keys_query_response(&response)
            .await
            .unwrap();

        let device = machine
            .store
            .get_device(&alice_id, alice_device_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(device.user_id(), &alice_id);
        assert_eq!(device.device_id(), alice_device_id);
    }

    #[tokio::test]
    async fn test_missing_sessions_calculation() {
        let (mut machine, _) = get_machine_after_query().await;

        let alice = alice_id();
        let alice_device = alice_device_id();

        let missing_sessions = machine
            .get_missing_sessions([alice.clone()].iter())
            .await
            .unwrap();

        assert!(missing_sessions.contains_key(&alice));
        let user_sessions = missing_sessions.get(&alice).unwrap();
        assert!(user_sessions.contains_key(&alice_device));
    }

    #[tokio::test]
    async fn test_session_creation() {
        let (mut alice_machine, bob_machine, one_time_keys) = get_machine_pair().await;

        let mut bob_keys = BTreeMap::new();

        let one_time_key = one_time_keys.iter().next().unwrap();
        let mut keys = BTreeMap::new();
        keys.insert(one_time_key.0.clone(), one_time_key.1.clone());
        bob_keys.insert(bob_machine.device_id.clone(), keys);

        let mut one_time_keys = BTreeMap::new();
        one_time_keys.insert(bob_machine.user_id.clone(), bob_keys);

        let response = keys::claim_keys::Response {
            failures: BTreeMap::new(),
            one_time_keys,
        };

        alice_machine
            .receive_keys_claim_response(&response)
            .await
            .unwrap();

        let session = alice_machine
            .store
            .get_sessions(bob_machine.account.identity_keys().curve25519())
            .await
            .unwrap()
            .unwrap();

        assert!(!session.lock().await.is_empty())
    }

    #[tokio::test]
    async fn test_olm_encryption() {
        let (mut alice, mut bob) = get_machine_pair_with_session().await;

        let bob_device = alice
            .store
            .get_device(&bob.user_id, &bob.device_id)
            .await
            .unwrap()
            .unwrap();

        let event = ToDeviceEvent {
            sender: alice.user_id.clone(),
            content: alice
                .olm_encrypt(&bob_device, EventType::Dummy, json!({}))
                .await
                .unwrap(),
        };

        let event = bob
            .decrypt_to_device_event(&event)
            .await
            .unwrap()
            .deserialize()
            .unwrap();

        if let AnyToDeviceEvent::Dummy(e) = event {
            assert_eq!(e.sender, alice.user_id);
        } else {
            panic!("Wrong event type found {:?}", event);
        }
    }

    #[tokio::test]
    async fn test_room_key_sharing() {
        let (mut alice, mut bob) = get_machine_pair_with_session().await;

        let room_id = RoomId::try_from("!test:example.org").unwrap();

        let to_device_requests = alice
            .share_group_session(&room_id, [bob.user_id.clone()].iter())
            .await
            .unwrap();

        let event = ToDeviceEvent {
            sender: alice.user_id.clone(),
            content: to_device_requests_to_content(to_device_requests),
        };

        let alice_session = alice.outbound_group_sessions.get(&room_id).unwrap();

        let event = bob
            .decrypt_to_device_event(&event)
            .await
            .unwrap()
            .deserialize()
            .unwrap();

        if let AnyToDeviceEvent::RoomKey(event) = event {
            assert_eq!(event.sender, alice.user_id);
            assert!(event.content.session_key.is_empty());
        } else {
            panic!("expected RoomKeyEvent found {:?}", event);
        }

        let session = bob
            .store
            .get_inbound_group_session(
                &room_id,
                alice.account.identity_keys().curve25519(),
                alice_session.session_id(),
            )
            .await;

        assert!(session.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_megolm_encryption() {
        let (mut alice, mut bob) = get_machine_pair_with_setup_sessions().await;
        let room_id = RoomId::try_from("!test:example.org").unwrap();

        let to_device_requests = alice
            .share_group_session(&room_id, [bob.user_id().clone()].iter())
            .await
            .unwrap();

        let event = ToDeviceEvent {
            sender: alice.user_id.clone(),
            content: to_device_requests_to_content(to_device_requests),
        };

        bob.decrypt_to_device_event(&event).await.unwrap();

        let plaintext = "It is a secret to everybody";

        let content = MessageEventContent::Text(TextMessageEventContent::plain(plaintext));

        let encrypted_content = alice.encrypt(&room_id, content.clone()).await.unwrap();

        let event = SyncMessageEvent {
            event_id: EventId::try_from("$xxxxx:example.org").unwrap(),
            origin_server_ts: SystemTime::now(),
            sender: alice.user_id().clone(),
            content: encrypted_content,
            unsigned: Unsigned::default(),
        };

        let decrypted_event = bob
            .decrypt_room_event(&event, &room_id)
            .await
            .unwrap()
            .deserialize()
            .unwrap();

        match decrypted_event {
            AnySyncRoomEvent::Message(AnySyncMessageEvent::RoomMessage(SyncMessageEvent {
                sender,
                content,
                ..
            })) => {
                assert_eq!(&sender, alice.user_id());
                if let MessageEventContent::Text(c) = &content {
                    assert_eq!(&c.body, plaintext);
                } else {
                    panic!("Decrypted event has a missmatched content");
                }
            }
            _ => panic!("Decrypted room event has the wrong type"),
        }
    }
}
