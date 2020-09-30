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

#[cfg(feature = "sqlite_cryptostore")]
use std::path::Path;
use std::{collections::BTreeMap, mem, sync::Arc, time::Duration};

use dashmap::DashMap;
use tracing::{debug, error, info, instrument, trace, warn};

use matrix_sdk_common::{
    api::r0::{
        keys::{
            claim_keys::{Request as KeysClaimRequest, Response as KeysClaimResponse},
            get_keys::Response as KeysQueryResponse,
            upload_keys,
        },
        sync::sync_events::Response as SyncResponse,
    },
    assign,
    events::{
        room::encrypted::EncryptedEventContent, room_key::RoomKeyEventContent,
        AnyMessageEventContent, AnySyncRoomEvent, AnyToDeviceEvent, SyncMessageEvent,
        ToDeviceEvent,
    },
    identifiers::{
        DeviceId, DeviceIdBox, DeviceKeyAlgorithm, EventEncryptionAlgorithm, RoomId, UserId,
    },
    js_int::UInt,
    uuid::Uuid,
    Raw,
};

#[cfg(feature = "sqlite_cryptostore")]
use crate::store::sqlite::SqliteStore;
use crate::{
    error::{EventError, MegolmError, MegolmResult, OlmResult},
    group_manager::GroupSessionManager,
    identities::{Device, IdentityManager, ReadOnlyDevice, UserDevices, UserIdentities},
    key_request::KeyRequestMachine,
    olm::{
        Account, EncryptionSettings, ExportedRoomKey, GroupSessionKey, IdentityKeys,
        InboundGroupSession, ReadOnlyAccount,
    },
    requests::{IncomingResponse, OutgoingRequest},
    store::{CryptoStore, MemoryStore, Result as StoreResult, Store},
    verification::{Sas, VerificationMachine},
    ToDeviceRequest,
};

/// State machine implementation of the Olm/Megolm encryption protocol used for
/// Matrix end to end encryption.
#[derive(Clone)]
pub struct OlmMachine {
    /// The unique user id that owns this account.
    user_id: Arc<UserId>,
    /// The unique device id of the device that holds this account.
    device_id: Arc<Box<DeviceId>>,
    /// Our underlying Olm Account holding our identity keys.
    account: Account,
    /// Store for the encryption keys.
    /// Persists all the encryption keys so a client can resume the session
    /// without the need to create new keys.
    store: Store,
    /// A state machine that keeps track of our outbound group sessions.
    group_session_manager: GroupSessionManager,
    /// A state machine that is responsible to handle and keep track of SAS
    /// verification flows.
    verification_machine: VerificationMachine,
    /// The state machine that is responsible to handle outgoing and incoming
    /// key requests.
    key_request_machine: KeyRequestMachine,
    /// State machine handling public user identities and devices, keeping track
    /// of when a key query needs to be done and handling one.
    identity_manager: IdentityManager,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for OlmMachine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OlmMachine")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .finish()
    }
}

impl OlmMachine {
    const KEY_CLAIM_TIMEOUT: Duration = Duration::from_secs(10);

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
    pub fn new(user_id: &UserId, device_id: &DeviceId) -> Self {
        let store: Box<dyn CryptoStore> = Box::new(MemoryStore::new());
        let device_id: DeviceIdBox = device_id.into();
        let account = ReadOnlyAccount::new(&user_id, &device_id);

        OlmMachine::new_helper(user_id, device_id, store, account)
    }

    fn new_helper(
        user_id: &UserId,
        device_id: DeviceIdBox,
        store: Box<dyn CryptoStore>,
        account: ReadOnlyAccount,
    ) -> Self {
        let user_id = Arc::new(user_id.clone());

        let store = Store::new(user_id.clone(), store);
        let verification_machine = VerificationMachine::new(account.clone(), store.clone());
        let device_id: Arc<DeviceIdBox> = Arc::new(device_id);
        let outbound_group_sessions = Arc::new(DashMap::new());
        let key_request_machine = KeyRequestMachine::new(
            user_id.clone(),
            device_id.clone(),
            store.clone(),
            outbound_group_sessions.clone(),
        );
        let identity_manager =
            IdentityManager::new(user_id.clone(), device_id.clone(), store.clone());
        let account = Account {
            inner: account,
            store: store.clone(),
        };

        let group_session_manager = GroupSessionManager::new(account.clone(), store.clone());

        OlmMachine {
            user_id,
            device_id,
            account,
            store,
            group_session_manager,
            verification_machine,
            key_request_machine,
            identity_manager,
        }
    }

    /// Create a new OlmMachine with the given [`CryptoStore`].
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
    ///
    /// [`Cryptostore`]: trait.CryptoStore.html
    pub async fn new_with_store(
        user_id: UserId,
        device_id: DeviceIdBox,
        store: Box<dyn CryptoStore>,
    ) -> StoreResult<Self> {
        let account = match store.load_account().await? {
            Some(a) => {
                debug!("Restored account");
                a
            }
            None => {
                debug!("Creating a new account");
                ReadOnlyAccount::new(&user_id, &device_id)
            }
        };

        Ok(OlmMachine::new_helper(&user_id, device_id, store, account))
    }

    /// Create a new machine with the default crypto store.
    ///
    /// The default store uses a SQLite database to store the encryption keys.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that owns this machine.
    ///
    /// * `device_id` - The unique id of the device that owns this machine.
    #[cfg(feature = "sqlite_cryptostore")]
    #[instrument(skip(path, passphrase))]
    #[cfg_attr(feature = "docs", doc(cfg(r#sqlite_cryptostore)))]
    pub async fn new_with_default_store(
        user_id: &UserId,
        device_id: &DeviceId,
        path: impl AsRef<Path>,
        passphrase: &str,
    ) -> StoreResult<Self> {
        let store =
            SqliteStore::open_with_passphrase(&user_id, device_id, path, passphrase).await?;

        OlmMachine::new_with_store(user_id.to_owned(), device_id.into(), Box::new(store)).await
    }

    /// The unique user id that owns this `OlmMachine` instance.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// The unique device id that identifies this `OlmMachine`.
    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// Get the public parts of our Olm identity keys.
    pub fn identity_keys(&self) -> &IdentityKeys {
        self.account.identity_keys()
    }

    /// Get the outgoing requests that need to be sent out.
    ///
    /// This returns a list of `OutGoingRequest`, those requests need to be sent
    /// out to the server and the responses need to be passed back to the state
    /// machine using [`mark_request_as_sent`].
    ///
    /// [`mark_request_as_sent`]: #method.mark_request_as_sent
    pub async fn outgoing_requests(&self) -> Vec<OutgoingRequest> {
        let mut requests = Vec::new();

        if let Some(r) = self.keys_for_upload().await.map(|r| OutgoingRequest {
            request_id: Uuid::new_v4(),
            request: Arc::new(r.into()),
        }) {
            requests.push(r);
        }

        if let Some(r) =
            self.identity_manager
                .users_for_key_query()
                .await
                .map(|r| OutgoingRequest {
                    request_id: Uuid::new_v4(),
                    request: Arc::new(r.into()),
                })
        {
            requests.push(r);
        }

        requests.append(&mut self.outgoing_to_device_requests());
        requests.append(&mut self.key_request_machine.outgoing_to_device_requests());

        requests
    }

    /// Mark the request with the given request id as sent.
    ///
    /// # Arguments
    ///
    /// * `request_id` - The unique id of the request that was sent out. This is
    /// needed to couple the response with the now sent out request.
    ///
    /// * `response` - The response that was received from the server after the
    /// outgoing request was sent out.
    pub async fn mark_request_as_sent<'a>(
        &self,
        request_id: &Uuid,
        response: impl Into<IncomingResponse<'a>>,
    ) -> OlmResult<()> {
        match response.into() {
            IncomingResponse::KeysUpload(response) => {
                self.receive_keys_upload_response(response).await?;
            }
            IncomingResponse::KeysQuery(response) => {
                self.receive_keys_query_response(response).await?;
            }
            IncomingResponse::KeysClaim(response) => {
                self.receive_keys_claim_response(response).await?;
            }
            IncomingResponse::ToDevice(_) => {
                self.mark_to_device_request_as_sent(&request_id).await?;
            }
        };

        Ok(())
    }

    /// Should device or one-time keys be uploaded to the server.
    ///
    /// This needs to be checked periodically, ideally after every sync request.
    ///
    /// # Example
    ///
    /// ```
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk_crypto::OlmMachine;
    /// # use matrix_sdk_common::identifiers::UserId;
    /// # use futures::executor::block_on;
    /// # let alice = UserId::try_from("@alice:example.org").unwrap();
    /// # let machine = OlmMachine::new(&alice, "DEVICEID".into());
    /// # block_on(async {
    /// if machine.should_upload_keys().await {
    ///     let request = machine
    ///         .keys_for_upload()
    ///         .await
    ///         .unwrap();
    ///
    ///     // Upload the keys here.
    /// }
    /// # });
    /// ```
    #[cfg(test)]
    async fn should_upload_keys(&self) -> bool {
        self.account.should_upload_keys().await
    }

    /// Get the underlying Olm account of the machine.
    #[cfg(test)]
    pub(crate) fn account(&self) -> &ReadOnlyAccount {
        &self.account
    }

    /// Receive a successful keys upload response.
    ///
    /// # Arguments
    ///
    /// * `response` - The keys upload response of the request that the client
    /// performed.
    #[instrument]
    async fn receive_keys_upload_response(
        &self,
        response: &upload_keys::Response,
    ) -> OlmResult<()> {
        self.account.receive_keys_upload_response(response).await
    }

    /// Get the a key claiming request for the user/device pairs that we are
    /// missing Olm sessions for.
    ///
    /// Returns None if no key claiming request needs to be sent out.
    ///
    /// Sessions need to be established between devices so group sessions for a
    /// room can be shared with them.
    ///
    /// This should be called every time a group session needs to be shared.
    ///
    /// The response of a successful key claiming requests needs to be passed to
    /// the `OlmMachine` with the [`mark_request_as_sent`].
    ///
    /// # Arguments
    ///
    /// `users` - The list of users that we should check if we lack a session
    /// with one of their devices.
    ///
    /// [`mark_request_as_sent`]: #method.mark_request_as_sent
    pub async fn get_missing_sessions(
        &self,
        users: impl Iterator<Item = &UserId>,
    ) -> OlmResult<Option<(Uuid, KeysClaimRequest)>> {
        let mut missing = BTreeMap::new();

        for user_id in users {
            let user_devices = self.store.get_user_devices(user_id).await?;

            for device in user_devices.devices() {
                let sender_key = if let Some(k) = device.get_key(DeviceKeyAlgorithm::Curve25519) {
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
                    let _ = user_map.insert(
                        device.device_id().into(),
                        DeviceKeyAlgorithm::SignedCurve25519,
                    );
                }
            }
        }

        if missing.is_empty() {
            Ok(None)
        } else {
            Ok(Some((
                Uuid::new_v4(),
                assign!(KeysClaimRequest::new(missing), {
                    timeout: Some(OlmMachine::KEY_CLAIM_TIMEOUT),
                }),
            )))
        }
    }

    /// Receive a successful key claim response and create new Olm sessions with
    /// the claimed keys.
    ///
    /// # Arguments
    ///
    /// * `response` - The response containing the claimed one-time keys.
    async fn receive_keys_claim_response(&self, response: &KeysClaimResponse) -> OlmResult<()> {
        // TODO log the failures here

        for (user_id, user_devices) in &response.one_time_keys {
            for (device_id, key_map) in user_devices {
                let device = match self.store.get_device(&user_id, device_id).await {
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

    /// Receive a successful keys query response.
    ///
    /// Returns a list of devices newly discovered devices and devices that
    /// changed.
    ///
    /// # Arguments
    ///
    /// * `response` - The keys query response of the request that the client
    /// performed.
    async fn receive_keys_query_response(
        &self,
        response: &KeysQueryResponse,
    ) -> OlmResult<(Vec<ReadOnlyDevice>, Vec<UserIdentities>)> {
        self.identity_manager
            .receive_keys_query_response(response)
            .await
    }

    /// Get a request to upload E2EE keys to the server.
    ///
    /// Returns None if no keys need to be uploaded.
    ///
    /// The response of a successful key upload requests needs to be passed to
    /// the [`OlmMachine`] with the [`receive_keys_upload_response`].
    ///
    /// [`receive_keys_upload_response`]: #method.receive_keys_upload_response
    /// [`OlmMachine`]: struct.OlmMachine.html
    async fn keys_for_upload(&self) -> Option<upload_keys::Request> {
        let (device_keys, one_time_keys) = self.account.keys_for_upload().await?;
        Some(assign!(upload_keys::Request::new(), { device_keys, one_time_keys }))
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
        &self,
        event: &ToDeviceEvent<EncryptedEventContent>,
    ) -> OlmResult<Raw<AnyToDeviceEvent>> {
        let (decrypted_event, sender_key, signing_key) =
            self.account.decrypt_to_device_event(event).await?;
        // Handle the decrypted event, e.g. fetch out Megolm sessions out of
        // the event.
        if let Some(event) = self
            .handle_decrypted_to_device_event(&sender_key, &signing_key, &decrypted_event)
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
    }

    /// Create a group session from a room key and add it to our crypto store.
    async fn add_room_key(
        &self,
        sender_key: &str,
        signing_key: &str,
        event: &mut ToDeviceEvent<RoomKeyEventContent>,
    ) -> OlmResult<Option<Raw<AnyToDeviceEvent>>> {
        match event.content.algorithm {
            EventEncryptionAlgorithm::MegolmV1AesSha2 => {
                let session_key = GroupSessionKey(mem::take(&mut event.content.session_key));

                let session = InboundGroupSession::new(
                    sender_key,
                    signing_key,
                    &event.content.room_id,
                    session_key,
                )?;
                let _ = self.store.save_inbound_group_sessions(&[session]).await?;

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

    #[cfg(test)]
    pub(crate) async fn create_outbound_group_session_with_defaults(
        &self,
        room_id: &RoomId,
    ) -> OlmResult<()> {
        self.group_session_manager
            .create_outbound_group_session(room_id, EncryptionSettings::default(), [].iter())
            .await
    }

    /// Encrypt a room message for the given room.
    ///
    /// Beware that a group session needs to be shared before this method can be
    /// called using the [`share_group_session`] method.
    ///
    /// Since group sessions can expire or become invalid if the room membership
    /// changes client authors should check with the
    /// [`should_share_group_session`] method if a new group session needs to
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
    ///
    /// [`should_share_group_session`]: #method.should_share_group_session
    /// [`share_group_session`]: #method.share_group_session
    pub async fn encrypt(
        &self,
        room_id: &RoomId,
        content: AnyMessageEventContent,
    ) -> MegolmResult<EncryptedEventContent> {
        self.group_session_manager.encrypt(room_id, content).await
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
        self.group_session_manager
            .should_share_group_session(room_id)
    }

    /// Invalidate the currently active outbound group session for the given
    /// room.
    ///
    /// Returns true if a session was invalidated, false if there was no session
    /// to invalidate.
    pub fn invalidate_group_session(&self, room_id: &RoomId) -> bool {
        self.group_session_manager.invalidate_group_session(room_id)
    }

    /// Get to-device requests to share a group session with users in a room.
    ///
    /// # Arguments
    ///
    /// `room_id` - The room id of the room where the group session will be
    /// used.
    ///
    /// `users` - The list of users that should receive the group session.
    pub async fn share_group_session(
        &self,
        room_id: &RoomId,
        users: impl Iterator<Item = &UserId>,
        encryption_settings: impl Into<EncryptionSettings>,
    ) -> OlmResult<Vec<ToDeviceRequest>> {
        self.group_session_manager
            .share_group_session(room_id, users, encryption_settings)
            .await
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
        &self,
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
            AnyToDeviceEvent::ForwardedRoomKey(mut e) => Ok(self
                .key_request_machine
                .receive_forwarded_room_key(sender_key, &mut e)
                .await?),
            _ => {
                warn!("Received a unexpected encrypted to-device event");
                Ok(None)
            }
        }
    }

    async fn handle_verification_event(&self, mut event: &mut AnyToDeviceEvent) {
        if let Err(e) = self.verification_machine.receive_event(&mut event).await {
            error!("Error handling a verification event: {:?}", e);
        }
    }

    /// Get the to-device requests that need to be sent out.
    fn outgoing_to_device_requests(&self) -> Vec<OutgoingRequest> {
        self.verification_machine.outgoing_to_device_requests()
    }

    /// Mark an outgoing to-device requests as sent.
    async fn mark_to_device_request_as_sent(&self, request_id: &Uuid) -> StoreResult<()> {
        self.verification_machine.mark_request_as_sent(request_id);
        self.key_request_machine
            .mark_outgoing_request_as_sent(request_id)
            .await?;

        Ok(())
    }

    /// Get a `Sas` verification object with the given flow id.
    pub fn get_verification(&self, flow_id: &str) -> Option<Sas> {
        self.verification_machine.get_sas(flow_id)
    }

    async fn update_one_time_key_count(
        &self,
        key_count: &BTreeMap<DeviceKeyAlgorithm, UInt>,
    ) -> StoreResult<()> {
        self.account.update_uploaded_key_count(key_count).await
    }

    /// Handle a sync response and update the internal state of the Olm machine.
    ///
    /// This will decrypt to-device events but will not touch events in the room
    /// timeline.
    ///
    /// To decrypt an event from the room timeline call [`decrypt_room_event`].
    ///
    /// # Arguments
    ///
    /// * `response` - The sync latest sync response.
    ///
    /// [`decrypt_room_event`]: #method.decrypt_room_event
    #[instrument(skip(response))]
    pub async fn receive_sync_response(&self, response: &mut SyncResponse) {
        self.verification_machine.garbage_collect();

        if let Err(e) = self
            .update_one_time_key_count(&response.device_one_time_keys_count)
            .await
        {
            error!("Error updating the one-time key count {:?}", e);
        }

        for user_id in &response.device_lists.changed {
            if let Err(e) = self.identity_manager.mark_user_as_changed(&user_id).await {
                error!("Error marking a tracked user as changed {:?}", e);
            }
        }

        for event_result in &mut response.to_device.events {
            let mut event = if let Ok(e) = event_result.deserialize() {
                e
            } else {
                // Skip invalid events.
                warn!("Received an invalid to-device event {:?}", event_result);
                continue;
            };

            info!("Received a to-device event {:?}", event);

            match &mut event {
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
                AnyToDeviceEvent::RoomKeyRequest(e) => {
                    self.key_request_machine.receive_incoming_key_request(e)
                }
                AnyToDeviceEvent::KeyVerificationAccept(..)
                | AnyToDeviceEvent::KeyVerificationCancel(..)
                | AnyToDeviceEvent::KeyVerificationKey(..)
                | AnyToDeviceEvent::KeyVerificationMac(..)
                | AnyToDeviceEvent::KeyVerificationRequest(..)
                | AnyToDeviceEvent::KeyVerificationStart(..) => {
                    self.handle_verification_event(&mut event).await;
                }
                _ => continue,
            }
        }

        // TODO remove this unwrap.
        self.key_request_machine
            .collect_incoming_key_requests()
            .await
            .unwrap();
    }

    /// Decrypt an event from a room timeline.
    ///
    /// # Arguments
    ///
    /// * `event` - The event that should be decrypted.
    ///
    /// * `room_id` - The ID of the room where the event was sent to.
    pub async fn decrypt_room_event(
        &self,
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
        let session = if let Some(s) = session {
            s
        } else {
            self.key_request_machine
                .create_outgoing_key_request(room_id, &content.sender_key, &content.session_id)
                .await?;
            return Err(MegolmError::MissingSession);
        };

        // TODO check the message index.
        // TODO check if this is from a verified device.
        let (decrypted_event, _) = session.decrypt(event).await?;

        trace!("Successfully decrypted Megolm event {:?}", decrypted_event);
        // TODO set the encryption info on the event (is it verified, was it
        // decrypted, sender key...)

        Ok(decrypted_event)
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
    pub async fn update_tracked_users(&self, users: impl IntoIterator<Item = &UserId>) {
        self.identity_manager.update_tracked_users(users).await
    }

    /// Get a specific device of a user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the device belongs to.
    ///
    /// * `device_id` - The unique id of the device.
    ///
    /// Returns a `Device` if one is found and the crypto store didn't throw an
    /// error.
    ///
    /// # Example
    ///
    /// ```
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk_crypto::OlmMachine;
    /// # use matrix_sdk_common::identifiers::UserId;
    /// # use futures::executor::block_on;
    /// # let alice = UserId::try_from("@alice:example.org").unwrap();
    /// # let machine = OlmMachine::new(&alice, "DEVICEID".into());
    /// # block_on(async {
    /// let device = machine.get_device(&alice, "DEVICEID".into()).await;
    ///
    /// println!("{:?}", device);
    /// # });
    /// ```
    pub async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> StoreResult<Option<Device>> {
        Ok(self
            .store
            .get_device_and_users(user_id, device_id)
            .await?
            .map(|(d, o, u)| Device {
                inner: d,
                verification_machine: self.verification_machine.clone(),
                own_identity: o,
                device_owner_identity: u,
            }))
    }

    /// Get a map holding all the devices of an user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the devices belong to.
    ///
    /// # Example
    ///
    /// ```
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk_crypto::OlmMachine;
    /// # use matrix_sdk_common::identifiers::UserId;
    /// # use futures::executor::block_on;
    /// # let alice = UserId::try_from("@alice:example.org").unwrap();
    /// # let machine = OlmMachine::new(&alice, "DEVICEID".into());
    /// # block_on(async {
    /// let devices = machine.get_user_devices(&alice).await.unwrap();
    ///
    /// for device in devices.devices() {
    ///     println!("{:?}", device);
    /// }
    /// # });
    /// ```
    pub async fn get_user_devices(&self, user_id: &UserId) -> StoreResult<UserDevices> {
        let devices = self.store.get_user_devices(user_id).await?;

        let own_identity = self
            .store
            .get_user_identity(self.user_id())
            .await?
            .map(|i| i.own().cloned())
            .flatten();
        let device_owner_identity = self.store.get_user_identity(user_id).await.ok().flatten();

        Ok(UserDevices {
            inner: devices,
            verification_machine: self.verification_machine.clone(),
            own_identity,
            device_owner_identity,
        })
    }

    /// Import the given room keys into our store.
    ///
    /// # Arguments
    ///
    /// * `exported_keys` - A list of previously exported keys that should be
    /// imported into our store. If we already have a better version of a key
    /// the key will *not* be imported.
    ///
    /// Returns the number of sessions that were imported to the store.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::io::Cursor;
    /// # use matrix_sdk_crypto::{OlmMachine, decrypt_key_export};
    /// # use matrix_sdk_common::identifiers::user_id;
    /// # use futures::executor::block_on;
    /// # let alice = user_id!("@alice:example.org");
    /// # let machine = OlmMachine::new(&alice, "DEVICEID".into());
    /// # block_on(async {
    /// # let export = Cursor::new("".to_owned());
    /// let exported_keys = decrypt_key_export(export, "1234").unwrap();
    /// machine.import_keys(exported_keys).await.unwrap();
    /// # });
    /// ```
    pub async fn import_keys(&self, mut exported_keys: Vec<ExportedRoomKey>) -> StoreResult<usize> {
        let mut sessions = Vec::new();

        for key in exported_keys.drain(..) {
            let session = InboundGroupSession::from_export(key)?;

            // Only import the session if we didn't have this session or if it's
            // a better version of the same session, that is the first known
            // index is lower.
            if let Some(existing_session) = self
                .store
                .get_inbound_group_session(
                    &session.room_id,
                    &session.sender_key,
                    session.session_id(),
                )
                .await?
            {
                let first_index = session.first_known_index().await;
                let existing_index = existing_session.first_known_index().await;

                if first_index < existing_index {
                    sessions.push(session)
                }
            } else {
                sessions.push(session)
            }
        }

        let num_sessions = sessions.len();

        self.store.save_inbound_group_sessions(&sessions).await?;

        Ok(num_sessions)
    }

    /// Export the keys that match the given predicate.
    ///
    /// # Arguments
    ///
    /// * `predicate` - A closure that will be called for every known
    /// `InboundGroupSession`, which represents a room key. If the closure
    /// returns `true` the `InboundGroupSessoin` will be included in the export,
    /// if the closure returns `false` it will not be included.
    ///
    /// # Panics
    ///
    /// This method will panic if it can't get enough randomness from the OS to
    /// encrypt the exported keys securely.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk_crypto::{OlmMachine, encrypt_key_export};
    /// # use matrix_sdk_common::identifiers::{user_id, room_id};
    /// # use futures::executor::block_on;
    /// # let alice = user_id!("@alice:example.org");
    /// # let machine = OlmMachine::new(&alice, "DEVICEID".into());
    /// # block_on(async {
    /// let room_id = room_id!("!test:localhost");
    /// let exported_keys = machine.export_keys(|s| s.room_id() == &room_id).await.unwrap();
    /// let encrypted_export = encrypt_key_export(&exported_keys, "1234", 1);
    /// # });
    /// ```
    pub async fn export_keys(
        &self,
        mut predicate: impl FnMut(&InboundGroupSession) -> bool,
    ) -> StoreResult<Vec<ExportedRoomKey>> {
        let mut exported = Vec::new();

        let mut sessions: Vec<InboundGroupSession> = self
            .store
            .get_inbound_group_sessions()
            .await?
            .drain(..)
            .filter(|s| predicate(&s))
            .collect();

        for session in sessions.drain(..) {
            let export = session.export().await;
            exported.push(export);
        }

        Ok(exported)
    }
}

#[cfg(test)]
pub(crate) mod test {
    static USER_ID: &str = "@bob:example.org";

    use std::{
        collections::BTreeMap,
        convert::{TryFrom, TryInto},
        time::SystemTime,
    };

    use http::Response;
    use serde_json::json;
    #[cfg(feature = "sqlite_cryptostore")]
    use tempfile::tempdir;

    use crate::{
        machine::OlmMachine,
        olm::Utility,
        verification::test::{outgoing_request_to_event, request_to_event},
        EncryptionSettings, ReadOnlyDevice, ToDeviceRequest,
    };

    use matrix_sdk_common::{
        api::r0::keys::{claim_keys, get_keys, upload_keys, OneTimeKey},
        events::{
            room::{
                encrypted::EncryptedEventContent,
                message::{MessageEventContent, TextMessageEventContent},
            },
            AnyMessageEventContent, AnySyncMessageEvent, AnySyncRoomEvent, AnyToDeviceEvent,
            EventType, SyncMessageEvent, ToDeviceEvent, Unsigned,
        },
        identifiers::{
            event_id, room_id, user_id, DeviceId, DeviceKeyAlgorithm, DeviceKeyId, UserId,
        },
        Raw,
    };
    use matrix_sdk_test::test_json;

    /// These keys need to be periodically uploaded to the server.
    type OneTimeKeys = BTreeMap<DeviceKeyId, OneTimeKey>;

    use matrix_sdk_common::js_int::uint;

    fn alice_id() -> UserId {
        user_id!("@alice:example.org")
    }

    fn alice_device_id() -> Box<DeviceId> {
        "JLAFKJWSCS".into()
    }

    fn user_id() -> UserId {
        UserId::try_from(USER_ID).unwrap()
    }

    pub fn response_from_file(json: &serde_json::Value) -> Response<Vec<u8>> {
        Response::builder()
            .status(200)
            .body(json.to_string().as_bytes().to_vec())
            .unwrap()
    }

    fn keys_upload_response() -> upload_keys::Response {
        let data = response_from_file(&test_json::KEYS_UPLOAD);
        upload_keys::Response::try_from(data).expect("Can't parse the keys upload response")
    }

    fn keys_query_response() -> get_keys::Response {
        let data = response_from_file(&test_json::KEYS_QUERY);
        get_keys::Response::try_from(data).expect("Can't parse the keys upload response")
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

    pub(crate) async fn get_prepared_machine() -> (OlmMachine, OneTimeKeys) {
        let machine = OlmMachine::new(&user_id(), &alice_device_id());
        machine.account.inner.update_uploaded_key_count(0);
        let request = machine
            .keys_for_upload()
            .await
            .expect("Can't prepare initial key upload");
        let response = keys_upload_response();
        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();

        (machine, request.one_time_keys.unwrap())
    }

    async fn get_machine_after_query() -> (OlmMachine, OneTimeKeys) {
        let (machine, otk) = get_prepared_machine().await;
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

        let alice_deivce = ReadOnlyDevice::from_machine(&alice).await;
        let bob_device = ReadOnlyDevice::from_machine(&bob).await;
        alice.store.save_devices(&[bob_device]).await.unwrap();
        bob.store.save_devices(&[alice_deivce]).await.unwrap();

        (alice, bob, otk)
    }

    async fn get_machine_pair_with_session() -> (OlmMachine, OlmMachine) {
        let (alice, bob, one_time_keys) = get_machine_pair().await;

        let mut bob_keys = BTreeMap::new();

        let one_time_key = one_time_keys.iter().next().unwrap();
        let mut keys = BTreeMap::new();
        keys.insert(one_time_key.0.clone(), one_time_key.1.clone());
        bob_keys.insert(bob.device_id().into(), keys);

        let mut one_time_keys = BTreeMap::new();
        one_time_keys.insert(bob.user_id().clone(), bob_keys);

        let response = claim_keys::Response::new(one_time_keys);

        alice.receive_keys_claim_response(&response).await.unwrap();

        (alice, bob)
    }

    async fn get_machine_pair_with_setup_sessions() -> (OlmMachine, OlmMachine) {
        let (alice, bob) = get_machine_pair_with_session().await;

        let bob_device = alice
            .get_device(&bob.user_id, &bob.device_id)
            .await
            .unwrap()
            .unwrap();

        let event = ToDeviceEvent {
            sender: alice.user_id().clone(),
            content: bob_device
                .encrypt(EventType::Dummy, json!({}))
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
        let machine = OlmMachine::new(&user_id(), &alice_device_id());
        let mut response = keys_upload_response();

        response
            .one_time_key_counts
            .remove(&DeviceKeyAlgorithm::SignedCurve25519)
            .unwrap();

        assert!(machine.should_upload_keys().await);
        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();
        assert!(machine.should_upload_keys().await);

        response
            .one_time_key_counts
            .insert(DeviceKeyAlgorithm::SignedCurve25519, uint!(10));
        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();
        assert!(machine.should_upload_keys().await);

        response
            .one_time_key_counts
            .insert(DeviceKeyAlgorithm::SignedCurve25519, uint!(50));
        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();
        assert!(!machine.should_upload_keys().await);
    }

    #[tokio::test]
    async fn generate_one_time_keys() {
        let machine = OlmMachine::new(&user_id(), &alice_device_id());

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
            .insert(DeviceKeyAlgorithm::SignedCurve25519, uint!(50));
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

        let utility = Utility::new();
        let ret = utility.verify_json(
            &machine.user_id,
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, machine.device_id()),
            ed25519_key,
            &mut json!(&mut device_keys),
        );
        assert!(ret.is_ok());
    }

    #[tokio::test]
    async fn tests_session_invalidation() {
        let machine = OlmMachine::new(&user_id(), &alice_device_id());
        let room_id = room_id!("!test:example.org");

        machine
            .create_outbound_group_session_with_defaults(&room_id)
            .await
            .unwrap();
        assert!(machine
            .group_session_manager
            .get_outbound_group_session(&room_id)
            .is_some());

        machine.invalidate_group_session(&room_id);

        assert!(machine
            .group_session_manager
            .get_outbound_group_session(&room_id)
            .is_none());
    }

    #[tokio::test]
    async fn test_invalid_signature() {
        let machine = OlmMachine::new(&user_id(), &alice_device_id());

        let mut device_keys = machine.account.device_keys().await;

        let utility = Utility::new();
        let ret = utility.verify_json(
            &machine.user_id,
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, machine.device_id()),
            "fake_key",
            &mut json!(&mut device_keys),
        );
        assert!(ret.is_err());
    }

    #[tokio::test]
    async fn test_one_time_key_signing() {
        let machine = OlmMachine::new(&user_id(), &alice_device_id());
        machine.account.inner.update_uploaded_key_count(49);

        let mut one_time_keys = machine.account.signed_one_time_keys().await.unwrap();
        let identity_keys = machine.account.identity_keys();
        let ed25519_key = identity_keys.ed25519();

        let mut one_time_key = one_time_keys.values_mut().next().unwrap();

        let utility = Utility::new();
        let ret = utility.verify_json(
            &machine.user_id,
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, machine.device_id()),
            ed25519_key,
            &mut json!(&mut one_time_key),
        );
        assert!(ret.is_ok());
    }

    #[tokio::test]
    async fn test_keys_for_upload() {
        let machine = OlmMachine::new(&user_id(), &alice_device_id());
        machine.account.inner.update_uploaded_key_count(0);

        let identity_keys = machine.account.identity_keys();
        let ed25519_key = identity_keys.ed25519();

        let mut request = machine
            .keys_for_upload()
            .await
            .expect("Can't prepare initial key upload");

        let utility = Utility::new();
        let ret = utility.verify_json(
            &machine.user_id,
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, machine.device_id()),
            ed25519_key,
            &mut json!(&mut request.one_time_keys.as_mut().unwrap().values_mut().next()),
        );
        assert!(ret.is_ok());

        let utility = Utility::new();
        let ret = utility.verify_json(
            &machine.user_id,
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, machine.device_id()),
            ed25519_key,
            &mut json!(&mut request.device_keys.unwrap()),
        );
        assert!(ret.is_ok());

        let mut response = keys_upload_response();
        response.one_time_key_counts.insert(
            DeviceKeyAlgorithm::SignedCurve25519,
            (request.one_time_keys.unwrap().len() as u64)
                .try_into()
                .unwrap(),
        );

        machine
            .receive_keys_upload_response(&response)
            .await
            .unwrap();

        let ret = machine.keys_for_upload().await;
        assert!(ret.is_none());
    }

    #[tokio::test]
    async fn test_keys_query() {
        let (machine, _) = get_prepared_machine().await;
        let response = keys_query_response();
        let alice_id = user_id!("@alice:example.org");
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
        let (machine, _) = get_machine_after_query().await;

        let alice = alice_id();
        let alice_device = alice_device_id();

        let (_, missing_sessions) = machine
            .get_missing_sessions([alice.clone()].iter())
            .await
            .unwrap()
            .unwrap();

        assert!(missing_sessions.one_time_keys.contains_key(&alice));
        let user_sessions = missing_sessions.one_time_keys.get(&alice).unwrap();
        assert!(user_sessions.contains_key(&alice_device));
    }

    #[tokio::test]
    async fn test_session_creation() {
        let (alice_machine, bob_machine, one_time_keys) = get_machine_pair().await;

        let mut bob_keys = BTreeMap::new();

        let one_time_key = one_time_keys.iter().next().unwrap();
        let mut keys = BTreeMap::new();
        keys.insert(one_time_key.0.clone(), one_time_key.1.clone());
        bob_keys.insert(bob_machine.device_id().into(), keys);

        let mut one_time_keys = BTreeMap::new();
        one_time_keys.insert(bob_machine.user_id().clone(), bob_keys);

        let response = claim_keys::Response::new(one_time_keys);

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
        let (alice, bob) = get_machine_pair_with_session().await;

        let bob_device = alice
            .get_device(&bob.user_id, &bob.device_id)
            .await
            .unwrap()
            .unwrap();

        let event = ToDeviceEvent {
            sender: alice.user_id().clone(),
            content: bob_device
                .encrypt(EventType::Dummy, json!({}))
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
            assert_eq!(&e.sender, alice.user_id());
        } else {
            panic!("Wrong event type found {:?}", event);
        }
    }

    #[tokio::test]
    async fn test_room_key_sharing() {
        let (alice, bob) = get_machine_pair_with_session().await;

        let room_id = room_id!("!test:example.org");

        let to_device_requests = alice
            .share_group_session(
                &room_id,
                [bob.user_id().clone()].iter(),
                EncryptionSettings::default(),
            )
            .await
            .unwrap();

        let event = ToDeviceEvent {
            sender: alice.user_id().clone(),
            content: to_device_requests_to_content(to_device_requests),
        };

        let alice_session = alice
            .group_session_manager
            .get_outbound_group_session(&room_id)
            .unwrap();

        let event = bob
            .decrypt_to_device_event(&event)
            .await
            .unwrap()
            .deserialize()
            .unwrap();

        if let AnyToDeviceEvent::RoomKey(event) = event {
            assert_eq!(&event.sender, alice.user_id());
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
        let (alice, bob) = get_machine_pair_with_setup_sessions().await;
        let room_id = room_id!("!test:example.org");

        let to_device_requests = alice
            .share_group_session(
                &room_id,
                [bob.user_id().clone()].iter(),
                EncryptionSettings::default(),
            )
            .await
            .unwrap();

        let event = ToDeviceEvent {
            sender: alice.user_id().clone(),
            content: to_device_requests_to_content(to_device_requests),
        };

        bob.decrypt_to_device_event(&event).await.unwrap();

        let plaintext = "It is a secret to everybody";

        let content = MessageEventContent::Text(TextMessageEventContent::plain(plaintext));

        let encrypted_content = alice
            .encrypt(
                &room_id,
                AnyMessageEventContent::RoomMessage(content.clone()),
            )
            .await
            .unwrap();

        let event = SyncMessageEvent {
            event_id: event_id!("$xxxxx:example.org"),
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

    #[tokio::test]
    #[cfg(feature = "sqlite_cryptostore")]
    async fn test_machine_with_default_store() {
        let tmpdir = tempdir().unwrap();

        let machine = OlmMachine::new_with_default_store(
            &user_id(),
            &alice_device_id(),
            tmpdir.as_ref(),
            "test",
        )
        .await
        .unwrap();

        let user_id = machine.user_id().to_owned();
        let device_id = machine.device_id().to_owned();
        let ed25519_key = machine.identity_keys().ed25519().to_owned();

        machine
            .receive_keys_upload_response(&keys_upload_response())
            .await
            .unwrap();

        drop(machine);

        let machine = OlmMachine::new_with_default_store(
            &user_id,
            &alice_device_id(),
            tmpdir.as_ref(),
            "test",
        )
        .await
        .unwrap();

        assert_eq!(&user_id, machine.user_id());
        assert_eq!(&*device_id, machine.device_id());
        assert_eq!(ed25519_key, machine.identity_keys().ed25519());
    }

    #[tokio::test]
    async fn interactive_verification() {
        let (alice, bob) = get_machine_pair_with_setup_sessions().await;

        let bob_device = alice
            .get_device(bob.user_id(), bob.device_id())
            .await
            .unwrap()
            .unwrap();

        assert!(!bob_device.is_trusted());

        let (alice_sas, request) = bob_device.start_verification().await.unwrap();

        let mut event = request_to_event(alice.user_id(), &request);
        bob.handle_verification_event(&mut event).await;

        let bob_sas = bob.get_verification(alice_sas.flow_id()).unwrap();

        assert!(alice_sas.emoji().is_none());
        assert!(bob_sas.emoji().is_none());

        let mut event = bob_sas
            .accept()
            .map(|r| request_to_event(bob.user_id(), &r))
            .unwrap();

        alice.handle_verification_event(&mut event).await;

        let mut event = alice
            .outgoing_to_device_requests()
            .first()
            .map(|r| outgoing_request_to_event(alice.user_id(), r))
            .unwrap();
        bob.handle_verification_event(&mut event).await;

        let mut event = bob
            .outgoing_to_device_requests()
            .first()
            .map(|r| outgoing_request_to_event(bob.user_id(), r))
            .unwrap();
        alice.handle_verification_event(&mut event).await;

        assert!(alice_sas.emoji().is_some());
        assert!(bob_sas.emoji().is_some());

        assert_eq!(alice_sas.emoji(), bob_sas.emoji());
        assert_eq!(alice_sas.decimals(), bob_sas.decimals());

        let mut event = bob_sas
            .confirm()
            .await
            .unwrap()
            .map(|r| request_to_event(bob.user_id(), &r))
            .unwrap();
        alice.handle_verification_event(&mut event).await;

        assert!(!alice_sas.is_done());
        assert!(!bob_sas.is_done());

        let mut event = alice_sas
            .confirm()
            .await
            .unwrap()
            .map(|r| request_to_event(alice.user_id(), &r))
            .unwrap();

        assert!(alice_sas.is_done());
        assert!(bob_device.is_trusted());

        let alice_device = bob
            .get_device(alice.user_id(), alice.device_id())
            .await
            .unwrap()
            .unwrap();

        assert!(!alice_device.is_trusted());
        bob.handle_verification_event(&mut event).await;
        assert!(bob_sas.is_done());
        assert!(alice_device.is_trusted());
    }
}
