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
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::Arc,
    time::Duration,
};

use dashmap::DashMap;
use matrix_sdk_common::deserialized_responses::{
    AlgorithmInfo, DeviceLinkProblem, EncryptionInfo, TimelineEvent, VerificationLevel,
    VerificationState,
};
use ruma::{
    api::client::{
        dehydrated_device::DehydratedDeviceData,
        keys::{
            claim_keys::v3::{Request as KeysClaimRequest, Response as KeysClaimResponse},
            get_keys::v3::Response as KeysQueryResponse,
            upload_keys,
            upload_signatures::v3::Request as UploadSignaturesRequest,
        },
        sync::sync_events::DeviceLists,
    },
    assign,
    events::{
        secret::request::SecretName, AnyMessageLikeEvent, AnyToDeviceEvent, MessageLikeEventContent,
    },
    serde::Raw,
    DeviceId, DeviceKeyAlgorithm, OwnedDeviceId, OwnedDeviceKeyId, OwnedTransactionId, OwnedUserId,
    RoomId, TransactionId, UInt, UserId,
};
use serde_json::{value::to_raw_value, Value};
use tokio::sync::Mutex;
use tracing::{
    debug, error,
    field::{debug, display},
    info, instrument, warn, Span,
};
use vodozemac::{
    megolm::{DecryptionError, SessionOrdering},
    Curve25519PublicKey, Ed25519Signature,
};

#[cfg(feature = "backups_v1")]
use crate::backups::BackupMachine;
use crate::{
    dehydrated_devices::{DehydratedDevices, DehydrationError},
    error::{EventError, MegolmError, MegolmResult, OlmError, OlmResult},
    gossiping::GossipMachine,
    identities::{user::UserIdentities, Device, IdentityManager, UserDevices},
    olm::{
        Account, CrossSigningStatus, EncryptionSettings, ExportedRoomKey, IdentityKeys,
        InboundGroupSession, OlmDecryptionInfo, PrivateCrossSigningIdentity, ReadOnlyAccount,
        SessionType,
    },
    requests::{IncomingResponse, OutgoingRequest, UploadSigningKeysRequest},
    session_manager::{GroupSessionManager, SessionManager},
    store::{
        locks::LockStoreError, Changes, DeviceChanges, DynCryptoStore, IdentityChanges,
        IntoCryptoStore, MemoryStore, Result as StoreResult, RoomKeyInfo, SecretImportError, Store,
    },
    types::{
        events::{
            olm_v1::{AnyDecryptedOlmEvent, DecryptedRoomKeyEvent},
            room::encrypted::{
                EncryptedEvent, EncryptedToDeviceEvent, RoomEncryptedEventContent,
                RoomEventEncryptionScheme, SupportedEventEncryptionSchemes,
            },
            room_key::{MegolmV1AesSha2Content, RoomKeyContent},
            room_key_withheld::{
                MegolmV1AesSha2WithheldContent, RoomKeyWithheldContent, RoomKeyWithheldEvent,
            },
            ToDeviceEvents,
        },
        Signatures,
    },
    verification::{Verification, VerificationMachine, VerificationRequest},
    CrossSigningKeyExport, CryptoStoreError, KeysQueryRequest, LocalTrust, ReadOnlyDevice,
    RoomKeyImportResult, SignatureError, ToDeviceRequest,
};

/// State machine implementation of the Olm/Megolm encryption protocol used for
/// Matrix end to end encryption.
#[derive(Clone)]
pub struct OlmMachine {
    pub(crate) inner: Arc<OlmMachineInner>,
}

pub struct OlmMachineInner {
    /// The unique user id that owns this account.
    user_id: OwnedUserId,
    /// The unique device ID of the device that holds this account.
    device_id: OwnedDeviceId,
    /// Our underlying Olm Account holding our identity keys.
    account: Account,
    /// The private part of our cross signing identity.
    /// Used to sign devices and other users, might be missing if some other
    /// device bootstrapped cross signing or cross signing isn't bootstrapped at
    /// all.
    user_identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
    /// Store for the encryption keys.
    /// Persists all the encryption keys so a client can resume the session
    /// without the need to create new keys.
    store: Store,
    /// A state machine that handles Olm sessions creation.
    session_manager: SessionManager,
    /// A state machine that keeps track of our outbound group sessions.
    pub(crate) group_session_manager: GroupSessionManager,
    /// A state machine that is responsible to handle and keep track of SAS
    /// verification flows.
    verification_machine: VerificationMachine,
    /// The state machine that is responsible to handle outgoing and incoming
    /// key requests.
    pub(crate) key_request_machine: GossipMachine,
    /// State machine handling public user identities and devices, keeping track
    /// of when a key query needs to be done and handling one.
    identity_manager: IdentityManager,
    /// A state machine that handles creating room key backups.
    #[cfg(feature = "backups_v1")]
    backup_machine: BackupMachine,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for OlmMachine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OlmMachine")
            .field("user_id", &self.user_id())
            .field("device_id", &self.device_id())
            .finish()
    }
}

impl OlmMachine {
    const CURRENT_GENERATION_STORE_KEY: &str = "generation-counter";

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
    pub async fn new(user_id: &UserId, device_id: &DeviceId) -> Self {
        OlmMachine::with_store(user_id, device_id, MemoryStore::new())
            .await
            .expect("Reading and writing to the memory store always succeeds")
    }

    pub(crate) async fn rehydrate(
        &self,
        pickle_key: &[u8; 32],
        device_id: &DeviceId,
        device_data: Raw<DehydratedDeviceData>,
    ) -> Result<OlmMachine, DehydrationError> {
        let account =
            ReadOnlyAccount::rehydrate(pickle_key, self.user_id(), device_id, device_data).await?;

        let store = MemoryStore::new().into_crypto_store();

        Ok(Self::new_helper(
            self.user_id(),
            self.device_id(),
            store,
            account,
            self.store().private_identity(),
        ))
    }

    fn new_helper(
        user_id: &UserId,
        device_id: &DeviceId,
        store: Arc<DynCryptoStore>,
        account: ReadOnlyAccount,
        user_identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
    ) -> Self {
        let user_id: OwnedUserId = user_id.into();

        let verification_machine =
            VerificationMachine::new(account.clone(), user_identity.clone(), store.clone());
        let store =
            Store::new(user_id.clone(), user_identity.clone(), store, verification_machine.clone());
        let device_id: OwnedDeviceId = device_id.into();
        let users_for_key_claim = Arc::new(DashMap::new());

        let account = Account { inner: account, store: store.clone() };

        let group_session_manager = GroupSessionManager::new(account.clone(), store.clone());

        let key_request_machine = GossipMachine::new(
            user_id.clone(),
            device_id.clone(),
            store.clone(),
            group_session_manager.session_cache(),
            users_for_key_claim.clone(),
        );
        let identity_manager =
            IdentityManager::new(user_id.clone(), device_id.clone(), store.clone());

        let session_manager = SessionManager::new(
            account.clone(),
            users_for_key_claim,
            key_request_machine.clone(),
            store.clone(),
        );

        #[cfg(feature = "backups_v1")]
        let backup_machine = BackupMachine::new(account.clone(), store.clone(), None);

        let inner = Arc::new(OlmMachineInner {
            user_id,
            device_id,
            account,
            user_identity,
            store,
            session_manager,
            group_session_manager,
            verification_machine,
            key_request_machine,
            identity_manager,
            #[cfg(feature = "backups_v1")]
            backup_machine,
        });

        Self { inner }
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
    #[instrument(skip(store), fields(ed25519_key, curve25519_key))]
    pub async fn with_store(
        user_id: &UserId,
        device_id: &DeviceId,
        store: impl IntoCryptoStore,
    ) -> StoreResult<Self> {
        let store = store.into_crypto_store();
        let account = match store.load_account().await? {
            Some(account) => {
                if user_id != account.user_id() || device_id != account.device_id() {
                    return Err(CryptoStoreError::MismatchedAccount {
                        expected: (account.user_id().to_owned(), account.device_id().to_owned()),
                        got: (user_id.to_owned(), device_id.to_owned()),
                    });
                }

                Span::current()
                    .record("ed25519_key", display(account.identity_keys().ed25519))
                    .record("curve25519_key", display(account.identity_keys().curve25519));
                debug!("Restored an Olm account");

                account
            }
            None => {
                let account = ReadOnlyAccount::with_device_id(user_id, device_id);

                Span::current()
                    .record("ed25519_key", display(account.identity_keys().ed25519))
                    .record("curve25519_key", display(account.identity_keys().curve25519));

                let device = ReadOnlyDevice::from_account(&account).await;

                // We just created this device from our own Olm `Account`. Since we are the
                // owners of the private keys of this device we can safely mark
                // the device as verified.
                device.set_trust_state(LocalTrust::Verified);

                let changes = Changes {
                    account: Some(account.clone()),
                    devices: DeviceChanges { new: vec![device], ..Default::default() },
                    ..Default::default()
                };
                store.save_changes(changes).await?;

                debug!("Created a new Olm account");

                account
            }
        };

        let identity = match store.load_identity().await? {
            Some(i) => {
                let master_key = i
                    .master_public_key()
                    .await
                    .and_then(|m| m.get_first_key().map(|m| m.to_owned()));
                debug!(?master_key, "Restored the cross signing identity");
                i
            }
            None => {
                debug!("Creating an empty cross signing identity stub");
                PrivateCrossSigningIdentity::empty(user_id)
            }
        };

        let identity = Arc::new(Mutex::new(identity));

        Ok(OlmMachine::new_helper(user_id, device_id, store, account, identity))
    }

    /// Get the crypto store associated with this `OlmMachine` instance.
    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    /// The unique user id that owns this `OlmMachine` instance.
    pub fn user_id(&self) -> &UserId {
        &self.inner.user_id
    }

    /// The unique device ID that identifies this `OlmMachine`.
    pub fn device_id(&self) -> &DeviceId {
        &self.inner.device_id
    }

    /// Get the public parts of our Olm identity keys.
    pub fn identity_keys(&self) -> IdentityKeys {
        self.inner.account.identity_keys()
    }

    /// Get the display name of our own device
    pub async fn display_name(&self) -> StoreResult<Option<String>> {
        self.store().device_display_name().await
    }

    /// Get the list of "tracked users".
    ///
    /// See [`update_tracked_users`](#method.update_tracked_users) for more
    /// information.
    pub async fn tracked_users(&self) -> StoreResult<HashSet<OwnedUserId>> {
        self.store().tracked_users().await
    }

    /// Enable or disable room key forwarding.
    ///
    /// Room key forwarding allows the device to request room keys that it might
    /// have missend in the original share using `m.room_key_request`
    /// events.
    #[cfg(feature = "automatic-room-key-forwarding")]
    pub fn toggle_room_key_forwarding(&self, enable: bool) {
        self.inner.key_request_machine.toggle_room_key_forwarding(enable)
    }

    /// Is room key forwarding enabled?
    pub fn is_room_key_forwarding_enabled(&self) -> bool {
        self.inner.key_request_machine.is_room_key_forwarding_enabled()
    }

    /// Get the outgoing requests that need to be sent out.
    ///
    /// This returns a list of [`OutgoingRequest`]. Those requests need to be
    /// sent out to the server and the responses need to be passed back to
    /// the state machine using [`mark_request_as_sent`].
    ///
    /// [`mark_request_as_sent`]: #method.mark_request_as_sent
    pub async fn outgoing_requests(&self) -> StoreResult<Vec<OutgoingRequest>> {
        let mut requests = Vec::new();

        if let Some(r) = self.keys_for_upload().await.map(|r| OutgoingRequest {
            request_id: TransactionId::new(),
            request: Arc::new(r.into()),
        }) {
            self.inner.account.save().await?;
            requests.push(r);
        }

        for request in self
            .inner
            .identity_manager
            .users_for_key_query()
            .await?
            .into_iter()
            .map(|(request_id, r)| OutgoingRequest { request_id, request: Arc::new(r.into()) })
        {
            requests.push(request);
        }

        requests.append(&mut self.inner.verification_machine.outgoing_messages());
        requests.append(&mut self.inner.key_request_machine.outgoing_to_device_requests().await?);

        Ok(requests)
    }

    /// Generate an "out-of-band" key query request for the given set of users.
    ///
    /// This can be useful if we need the results from [`get_identity`] or
    /// [`get_user_devices`] to be as up-to-date as possible.
    ///
    /// # Arguments
    ///
    /// * `users` - list of users whose keys should be queried
    ///
    /// # Returns
    ///
    /// A request to be sent out to the server. Once sent, the response should
    /// be passed back to the state machine using [`mark_request_as_sent`].
    ///
    /// [`mark_request_as_sent`]: OlmMachine::mark_request_as_sent
    /// [`get_identity`]: OlmMachine::get_identity
    /// [`get_user_devices`]: OlmMachine::get_user_devices
    pub fn query_keys_for_users<'a>(
        &self,
        users: impl IntoIterator<Item = &'a UserId>,
    ) -> (OwnedTransactionId, KeysQueryRequest) {
        self.inner.identity_manager.build_key_query_for_users(users)
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
        request_id: &TransactionId,
        response: impl Into<IncomingResponse<'a>>,
    ) -> OlmResult<()> {
        match response.into() {
            IncomingResponse::KeysUpload(response) => {
                self.receive_keys_upload_response(response).await?;
            }
            IncomingResponse::KeysQuery(response) => {
                self.receive_keys_query_response(request_id, response).await?;
            }
            IncomingResponse::KeysClaim(response) => {
                self.receive_keys_claim_response(response).await?;
            }
            IncomingResponse::ToDevice(_) => {
                self.mark_to_device_request_as_sent(request_id).await?;
            }
            IncomingResponse::SigningKeysUpload(_) => {
                self.receive_cross_signing_upload_response().await?;
            }
            IncomingResponse::SignatureUpload(_) => {
                self.inner.verification_machine.mark_request_as_sent(request_id);
            }
            IncomingResponse::RoomMessage(_) => {
                self.inner.verification_machine.mark_request_as_sent(request_id);
            }
            IncomingResponse::KeysBackup(_) => {
                #[cfg(feature = "backups_v1")]
                self.inner.backup_machine.mark_request_as_sent(request_id).await?;
            }
        };

        Ok(())
    }

    /// Mark the cross signing identity as shared.
    async fn receive_cross_signing_upload_response(&self) -> StoreResult<()> {
        let identity = self.inner.user_identity.lock().await;
        identity.mark_as_shared();

        let changes = Changes { private_identity: Some(identity.clone()), ..Default::default() };

        self.store().save_changes(changes).await
    }

    /// Create a new cross signing identity and get the upload request to push
    /// the new public keys to the server.
    ///
    /// **Warning**: This will delete any existing cross signing keys that might
    /// exist on the server and thus will reset the trust between all the
    /// devices.
    ///
    /// # Returns
    ///
    /// A pair of requests which should be sent out to the server. Once
    /// sent, the responses should be passed back to the state machine using
    /// [`mark_request_as_sent`].
    ///
    /// These requests may require user interactive auth.
    ///
    /// [`mark_request_as_sent`]: #method.mark_request_as_sent
    pub async fn bootstrap_cross_signing(
        &self,
        reset: bool,
    ) -> StoreResult<(UploadSigningKeysRequest, UploadSignaturesRequest)> {
        let mut identity = self.inner.user_identity.lock().await;

        if identity.is_empty().await || reset {
            info!("Creating new cross signing identity");
            let (id, request, signature_request) =
                self.inner.account.bootstrap_cross_signing().await;

            *identity = id;

            let public = identity.to_public_identity().await.expect(
                "Couldn't create a public version of the identity from a new private identity",
            );

            let changes = Changes {
                identities: IdentityChanges { new: vec![public.into()], ..Default::default() },
                private_identity: Some(identity.clone()),
                ..Default::default()
            };

            self.store().save_changes(changes).await?;

            Ok((request, signature_request))
        } else {
            info!("Trying to upload the existing cross signing identity");
            let request = identity.as_upload_request().await;
            // TODO remove this expect.
            let signature_request =
                identity.sign_account(&self.inner.account).await.expect("Can't sign device keys");
            Ok((request, signature_request))
        }
    }

    /// Get the underlying Olm account of the machine.
    #[cfg(any(test, feature = "testing"))]
    #[allow(dead_code)]
    pub(crate) fn account(&self) -> &ReadOnlyAccount {
        &self.inner.account
    }

    /// Receive a successful keys upload response.
    ///
    /// # Arguments
    ///
    /// * `response` - The keys upload response of the request that the client
    /// performed.
    async fn receive_keys_upload_response(
        &self,
        response: &upload_keys::v3::Response,
    ) -> OlmResult<()> {
        self.inner.account.receive_keys_upload_response(response).await
    }

    /// Get the a key claiming request for the user/device pairs that we are
    /// missing Olm sessions for.
    ///
    /// Returns None if no key claiming request needs to be sent out.
    ///
    /// Sessions need to be established between devices so group sessions for a
    /// room can be shared with them.
    ///
    /// This should be called every time a group session needs to be shared as
    /// well as between sync calls. After a sync some devices may request room
    /// keys without us having a valid Olm session with them, making it
    /// impossible to server the room key request, thus it's necessary to check
    /// for missing sessions between sync as well.
    ///
    /// **Note**: Care should be taken that only one such request at a time is
    /// in flight, e.g. using a lock.
    ///
    /// The response of a successful key claiming requests needs to be passed to
    /// the `OlmMachine` with the [`mark_request_as_sent`].
    ///
    /// # Arguments
    ///
    /// `users` - The list of users that we should check if we lack a session
    /// with one of their devices. This can be an empty iterator when calling
    /// this method between sync requests.
    ///
    /// [`mark_request_as_sent`]: #method.mark_request_as_sent
    pub async fn get_missing_sessions(
        &self,
        users: impl Iterator<Item = &UserId>,
    ) -> StoreResult<Option<(OwnedTransactionId, KeysClaimRequest)>> {
        self.inner.session_manager.get_missing_sessions(users).await
    }

    /// Receive a successful key claim response and create new Olm sessions with
    /// the claimed keys.
    ///
    /// # Arguments
    ///
    /// * `response` - The response containing the claimed one-time keys.
    async fn receive_keys_claim_response(&self, response: &KeysClaimResponse) -> OlmResult<()> {
        self.inner.session_manager.receive_keys_claim_response(response).await
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
        request_id: &TransactionId,
        response: &KeysQueryResponse,
    ) -> OlmResult<(DeviceChanges, IdentityChanges)> {
        self.inner.identity_manager.receive_keys_query_response(request_id, response).await
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
    async fn keys_for_upload(&self) -> Option<upload_keys::v3::Request> {
        let (device_keys, one_time_keys, fallback_keys) =
            self.inner.account.keys_for_upload().await;

        if device_keys.is_none() && one_time_keys.is_empty() && fallback_keys.is_empty() {
            None
        } else {
            let device_keys = device_keys.map(|d| d.to_raw());

            Some(assign!(upload_keys::v3::Request::new(), {
                device_keys, one_time_keys, fallback_keys
            }))
        }
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
        event: &EncryptedToDeviceEvent,
        changes: &mut Changes,
    ) -> OlmResult<OlmDecryptionInfo> {
        let mut decrypted = self.inner.account.decrypt_to_device_event(event).await?;
        // Handle the decrypted event, e.g. fetch out Megolm sessions out of
        // the event.
        self.handle_decrypted_to_device_event(&mut decrypted, changes).await?;

        Ok(decrypted)
    }

    #[instrument(
    skip_all,
        // This function is only ever called by add_room_key via
        // handle_decrypted_to_device_event, so sender, sender_key, and algorithm are
        // already recorded.
        fields(room_id = ?content.room_id, session_id)
    )]
    async fn handle_key(
        &self,
        sender_key: Curve25519PublicKey,
        event: &DecryptedRoomKeyEvent,
        content: &MegolmV1AesSha2Content,
    ) -> OlmResult<Option<InboundGroupSession>> {
        let session = InboundGroupSession::new(
            sender_key,
            event.keys.ed25519,
            &content.room_id,
            &content.session_key,
            event.content.algorithm(),
            None,
        );

        match session {
            Ok(session) => {
                tracing::Span::current().record("session_id", session.session_id());

                if self.store().compare_group_session(&session).await? == SessionOrdering::Better {
                    info!("Received a new megolm room key");

                    Ok(Some(session))
                } else {
                    warn!(
                        "Received a megolm room key that we already have a better version of, \
                        discarding",
                    );

                    Ok(None)
                }
            }
            Err(e) => {
                tracing::Span::current().record("session_id", &content.session_id);
                warn!("Received a room key event which contained an invalid session key: {e}");

                Ok(None)
            }
        }
    }

    /// Create a group session from a room key and add it to our crypto store.
    #[instrument(skip_all, fields(algorithm = ?event.content.algorithm()))]
    async fn add_room_key(
        &self,
        sender_key: Curve25519PublicKey,
        event: &DecryptedRoomKeyEvent,
    ) -> OlmResult<Option<InboundGroupSession>> {
        match &event.content {
            RoomKeyContent::MegolmV1AesSha2(content) => {
                self.handle_key(sender_key, event, content).await
            }
            #[cfg(feature = "experimental-algorithms")]
            RoomKeyContent::MegolmV2AesSha2(content) => {
                self.handle_key(sender_key, event, content).await
            }
            RoomKeyContent::Unknown(_) => {
                warn!("Received a room key with an unsupported algorithm");
                Ok(None)
            }
        }
    }

    async fn add_withheld_info(&self, changes: &mut Changes, event: &RoomKeyWithheldEvent) {
        if let RoomKeyWithheldContent::MegolmV1AesSha2(
            MegolmV1AesSha2WithheldContent::BlackListed(c)
            | MegolmV1AesSha2WithheldContent::Unverified(c),
        ) = &event.content
        {
            changes
                .withheld_session_info
                .entry(c.room_id.to_owned())
                .or_default()
                .insert(c.session_id.to_owned(), event.to_owned());
        }
    }

    #[cfg(test)]
    pub(crate) async fn create_outbound_group_session_with_defaults(
        &self,
        room_id: &RoomId,
    ) -> OlmResult<()> {
        let (_, session) = self
            .inner
            .group_session_manager
            .create_outbound_group_session(room_id, EncryptionSettings::default())
            .await?;

        self.store().save_inbound_group_sessions(&[session]).await?;

        Ok(())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) async fn create_inbound_session(
        &self,
        room_id: &RoomId,
    ) -> OlmResult<InboundGroupSession> {
        let (_, session) = self
            .inner
            .group_session_manager
            .create_outbound_group_session(room_id, EncryptionSettings::default())
            .await?;

        Ok(session)
    }

    /// Encrypt a room message for the given room.
    ///
    /// Beware that a room key needs to be shared before this method
    /// can be called using the [`OlmMachine::share_room_key`] method.
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
    /// Panics if a room key for the given room wasn't shared beforehand.
    pub async fn encrypt_room_event(
        &self,
        room_id: &RoomId,
        content: impl MessageLikeEventContent,
    ) -> MegolmResult<Raw<RoomEncryptedEventContent>> {
        let event_type = content.event_type().to_string();
        let content = serde_json::to_value(&content)?;
        self.encrypt_room_event_raw(room_id, content, &event_type).await
    }

    /// Encrypt a json [`Value`] content for the given room.
    ///
    /// This method is equivalent to the [`OlmMachine::encrypt_room_event()`]
    /// method but operates on an arbitrary JSON value instead of strongly-typed
    /// event content struct.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which the message should be
    /// encrypted.
    ///
    /// * `content` - The plaintext content of the message that should be
    /// encrypted as a json [`Value`].
    ///
    /// * `event_type` - The plaintext type of the event.
    ///
    /// # Panics
    ///
    /// Panics if a group session for the given room wasn't shared beforehand.
    pub async fn encrypt_room_event_raw(
        &self,
        room_id: &RoomId,
        content: Value,
        event_type: &str,
    ) -> MegolmResult<Raw<RoomEncryptedEventContent>> {
        self.inner.group_session_manager.encrypt(room_id, content, event_type).await
    }

    /// Invalidate the currently active outbound group session for the given
    /// room.
    ///
    /// Returns true if a session was invalidated, false if there was no session
    /// to invalidate.
    pub async fn invalidate_group_session(&self, room_id: &RoomId) -> StoreResult<bool> {
        self.inner.group_session_manager.invalidate_group_session(room_id).await
    }

    /// Get to-device requests to share a room key with users in a room.
    ///
    /// # Arguments
    ///
    /// `room_id` - The room id of the room where the room key will be
    /// used.
    ///
    /// `users` - The list of users that should receive the room key.
    ///
    /// `settings` - Encryption settings that affect when are room keys rotated
    /// and who are they shared with
    pub async fn share_room_key(
        &self,
        room_id: &RoomId,
        users: impl Iterator<Item = &UserId>,
        encryption_settings: impl Into<EncryptionSettings>,
    ) -> OlmResult<Vec<Arc<ToDeviceRequest>>> {
        self.inner.group_session_manager.share_room_key(room_id, users, encryption_settings).await
    }

    /// Receive an unencrypted verification event.
    ///
    /// This method can be used to pass verification events that are happening
    /// in unencrypted rooms to the `OlmMachine`.
    ///
    /// **Note**: This does not need to be called for encrypted events since
    /// those will get passed to the `OlmMachine` during decryption.
    #[deprecated(note = "Use OlmMachine::receive_verification_event instead", since = "0.7.0")]
    pub async fn receive_unencrypted_verification_event(
        &self,
        event: &AnyMessageLikeEvent,
    ) -> StoreResult<()> {
        self.inner.verification_machine.receive_any_event(event).await
    }

    /// Receive a verification event.
    ///
    /// in rooms to the `OlmMachine`. The event should be in the decrypted form.
    /// in rooms to the `OlmMachine`.
    pub async fn receive_verification_event(&self, event: &AnyMessageLikeEvent) -> StoreResult<()> {
        self.inner.verification_machine.receive_any_event(event).await
    }

    /// Receive and properly handle a decrypted to-device event.
    ///
    /// # Arguments
    ///
    /// * `decrypted` - The decrypted event and some associated metadata.
    #[instrument(
        skip_all,
        fields(
            sender_key = ?decrypted.result.sender_key,
            event_type = decrypted.result.event.event_type(),
        ),
    )]
    async fn handle_decrypted_to_device_event(
        &self,
        decrypted: &mut OlmDecryptionInfo,
        changes: &mut Changes,
    ) -> OlmResult<()> {
        debug!("Received a decrypted to-device event");

        match &*decrypted.result.event {
            AnyDecryptedOlmEvent::RoomKey(e) => {
                let session = self.add_room_key(decrypted.result.sender_key, e).await?;
                decrypted.inbound_group_session = session;
            }
            AnyDecryptedOlmEvent::ForwardedRoomKey(e) => {
                let session = self
                    .inner
                    .key_request_machine
                    .receive_forwarded_room_key(decrypted.result.sender_key, e)
                    .await?;
                decrypted.inbound_group_session = session;
            }
            AnyDecryptedOlmEvent::SecretSend(e) => {
                let name = self
                    .inner
                    .key_request_machine
                    .receive_secret_event(decrypted.result.sender_key, e, changes)
                    .await?;

                // Set the secret name so other consumers of the event know
                // what this event is about.
                if let Ok(ToDeviceEvents::SecretSend(mut e)) =
                    decrypted.result.raw_event.deserialize_as()
                {
                    e.content.secret_name = name;
                    decrypted.result.raw_event = Raw::from_json(to_raw_value(&e)?);
                }
            }
            AnyDecryptedOlmEvent::Dummy(_) => {
                debug!("Received an `m.dummy` event");
            }
            AnyDecryptedOlmEvent::Custom(_) => {
                warn!("Received an unexpected encrypted to-device event");
            }
        }

        Ok(())
    }

    async fn handle_verification_event(&self, event: &ToDeviceEvents) {
        if let Err(e) = self.inner.verification_machine.receive_any_event(event).await {
            error!("Error handling a verification event: {e:?}");
        }
    }

    /// Mark an outgoing to-device requests as sent.
    async fn mark_to_device_request_as_sent(&self, request_id: &TransactionId) -> StoreResult<()> {
        self.inner.verification_machine.mark_request_as_sent(request_id);
        self.inner.key_request_machine.mark_outgoing_request_as_sent(request_id).await?;
        self.inner.group_session_manager.mark_request_as_sent(request_id).await?;
        self.inner.session_manager.mark_outgoing_request_as_sent(request_id);
        Ok(())
    }

    /// Get a verification object for the given user id with the given flow id.
    pub fn get_verification(&self, user_id: &UserId, flow_id: &str) -> Option<Verification> {
        self.inner.verification_machine.get_verification(user_id, flow_id)
    }

    /// Get a verification request object with the given flow id.
    pub fn get_verification_request(
        &self,
        user_id: &UserId,
        flow_id: impl AsRef<str>,
    ) -> Option<VerificationRequest> {
        self.inner.verification_machine.get_request(user_id, flow_id)
    }

    /// Get all the verification requests of a given user.
    pub fn get_verification_requests(&self, user_id: &UserId) -> Vec<VerificationRequest> {
        self.inner.verification_machine.get_requests(user_id)
    }

    async fn update_key_counts(
        &self,
        one_time_key_count: &BTreeMap<DeviceKeyAlgorithm, UInt>,
        unused_fallback_keys: Option<&[DeviceKeyAlgorithm]>,
    ) {
        self.inner.account.update_key_counts(one_time_key_count, unused_fallback_keys).await;
    }

    async fn handle_to_device_event(&self, changes: &mut Changes, event: &ToDeviceEvents) {
        use crate::types::events::ToDeviceEvents::*;

        match event {
            RoomKeyRequest(e) => self.inner.key_request_machine.receive_incoming_key_request(e),
            SecretRequest(e) => self.inner.key_request_machine.receive_incoming_secret_request(e),
            RoomKeyWithheld(e) => self.add_withheld_info(changes, e).await,
            KeyVerificationAccept(..)
            | KeyVerificationCancel(..)
            | KeyVerificationKey(..)
            | KeyVerificationMac(..)
            | KeyVerificationRequest(..)
            | KeyVerificationReady(..)
            | KeyVerificationDone(..)
            | KeyVerificationStart(..) => {
                self.handle_verification_event(event).await;
            }
            Dummy(_) | RoomKey(_) | ForwardedRoomKey(_) | RoomEncrypted(_) => {}
            _ => {}
        }
    }

    fn record_message_id(event: &Raw<AnyToDeviceEvent>) {
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct ContentStub<'a> {
            #[serde(borrow, rename = "org.matrix.msgid")]
            message_id: Option<&'a str>,
        }

        #[derive(Deserialize)]
        struct ToDeviceStub<'a> {
            sender: &'a str,
            #[serde(rename = "type")]
            event_type: &'a str,
            #[serde(borrow)]
            content: ContentStub<'a>,
        }

        if let Ok(event) = event.deserialize_as::<ToDeviceStub<'_>>() {
            tracing::Span::current().record("sender", event.sender);
            tracing::Span::current().record("event_type", event.event_type);
            tracing::Span::current().record("message_id", event.content.message_id);
        }
    }

    #[instrument(skip_all, fields(sender, event_type, message_id))]
    async fn receive_to_device_event(
        &self,
        changes: &mut Changes,
        mut raw_event: Raw<AnyToDeviceEvent>,
    ) -> OlmResult<Raw<AnyToDeviceEvent>> {
        Self::record_message_id(&raw_event);

        let event: ToDeviceEvents = match raw_event.deserialize_as() {
            Ok(e) => e,
            Err(e) => {
                // Skip invalid events.
                warn!("Received an invalid to-device event: {e}");

                return Ok(raw_event);
            }
        };

        debug!("Received a to-device event");

        match event {
            ToDeviceEvents::RoomEncrypted(e) => {
                let decrypted = match self.decrypt_to_device_event(&e, changes).await {
                    Ok(e) => e,
                    Err(err) => {
                        if let OlmError::SessionWedged(sender, curve_key) = err {
                            if let Err(e) = self
                                .inner
                                .session_manager
                                .mark_device_as_wedged(&sender, curve_key)
                                .await
                            {
                                error!(
                                    error = ?e,
                                    "Couldn't mark device from to be unwedged",
                                );
                            }
                        }

                        return Ok(raw_event);
                    }
                };

                // New sessions modify the account so we need to save that
                // one as well.
                match decrypted.session {
                    SessionType::New(s) => {
                        changes.account = Some((*self.inner.account).clone());
                        changes.sessions.push(s);
                    }
                    SessionType::Existing(s) => {
                        changes.sessions.push(s);
                    }
                }

                changes.message_hashes.push(decrypted.message_hash);

                if let Some(group_session) = decrypted.inbound_group_session {
                    changes.inbound_group_sessions.push(group_session);
                }

                match decrypted.result.raw_event.deserialize_as() {
                    Ok(event) => {
                        self.handle_to_device_event(changes, &event).await;

                        raw_event = event
                            .serialize_zeroized()
                            .expect("Zeroizing and reserializing our events should always work")
                            .cast();
                    }
                    Err(e) => {
                        warn!("Received an invalid encrypted to-device event: {e}");
                        raw_event = decrypted.result.raw_event;
                    }
                }
            }

            e => self.handle_to_device_event(changes, &e).await,
        }

        Ok(raw_event)
    }

    /// Handle a to-device and one-time key counts from a sync response.
    ///
    /// This will decrypt and handle to-device events returning the decrypted
    /// versions of them.
    ///
    /// To decrypt an event from the room timeline call [`decrypt_room_event`].
    ///
    /// # Arguments
    ///
    /// * `to_device_events` - The to-device events of the current sync
    /// response.
    ///
    /// * `changed_devices` - The list of devices that changed in this sync
    /// response.
    ///
    /// * `one_time_keys_count` - The current one-time keys counts that the sync
    /// response returned.
    ///
    /// [`decrypt_room_event`]: #method.decrypt_room_event
    ///
    /// # Returns
    ///
    /// A tuple of (decrypted to-device events, updated room keys).
    #[instrument(skip_all)]
    pub async fn receive_sync_changes(
        &self,
        sync_changes: EncryptionSyncChanges<'_>,
    ) -> OlmResult<(Vec<Raw<AnyToDeviceEvent>>, Vec<RoomKeyInfo>)> {
        let (events, changes) = self.preprocess_sync_changes(sync_changes).await?;

        // Technically save_changes also does the same work, so if it's slow we could
        // refactor this to do it only once.
        let room_key_updates: Vec<_> =
            changes.inbound_group_sessions.iter().map(RoomKeyInfo::from).collect();

        self.store().save_changes(changes).await?;

        Ok((events, room_key_updates))
    }

    pub(crate) async fn preprocess_sync_changes(
        &self,
        sync_changes: EncryptionSyncChanges<'_>,
    ) -> OlmResult<(Vec<Raw<AnyToDeviceEvent>>, Changes)> {
        // Remove verification objects that have expired or are done.
        let mut events = self.inner.verification_machine.garbage_collect();

        // Always save the account, a new session might get created which also
        // touches the account.
        let mut changes =
            Changes { account: Some((*self.inner.account).clone()), ..Default::default() };

        self.update_key_counts(
            sync_changes.one_time_keys_counts,
            sync_changes.unused_fallback_keys,
        )
        .await;

        if let Err(e) = self
            .inner
            .identity_manager
            .receive_device_changes(sync_changes.changed_devices.changed.iter().map(|u| u.as_ref()))
            .await
        {
            error!(error = ?e, "Error marking a tracked user as changed");
        }

        for raw_event in sync_changes.to_device_events {
            let raw_event = Box::pin(self.receive_to_device_event(&mut changes, raw_event)).await?;
            events.push(raw_event);
        }

        let changed_sessions =
            self.inner.key_request_machine.collect_incoming_key_requests().await?;

        changes.sessions.extend(changed_sessions);
        changes.next_batch_token = sync_changes.next_batch_token;

        Ok((events, changes))
    }

    /// Request a room key from our devices.
    ///
    /// This method will return a request cancellation and a new key request if
    /// the key was already requested, otherwise it will return just the key
    /// request.
    ///
    /// The request cancellation *must* be sent out before the request is sent
    /// out, otherwise devices will ignore the key request.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room where the key is used in.
    ///
    /// * `sender_key` - The curve25519 key of the sender that owns the key.
    ///
    /// * `session_id` - The id that uniquely identifies the session.
    pub async fn request_room_key(
        &self,
        event: &Raw<EncryptedEvent>,
        room_id: &RoomId,
    ) -> MegolmResult<(Option<OutgoingRequest>, OutgoingRequest)> {
        let event = event.deserialize()?;
        self.inner.key_request_machine.request_key(room_id, &event).await
    }

    async fn get_verification_state(
        &self,
        session: &InboundGroupSession,
        sender: &UserId,
    ) -> MegolmResult<(VerificationState, Option<OwnedDeviceId>)> {
        let claimed_device = self
            .get_user_devices(sender, None)
            .await?
            .devices()
            .find(|d| d.curve25519_key() == Some(session.sender_key()));

        Ok(match claimed_device {
            None => {
                // We didn't find a device, no way to know if we should trust the
                // `InboundGroupSession` or not.

                let link_problem = if session.has_been_imported() {
                    DeviceLinkProblem::InsecureSource
                } else {
                    DeviceLinkProblem::MissingDevice
                };

                (VerificationState::Unverified(VerificationLevel::None(link_problem)), None)
            }
            Some(device) => {
                let device_id = device.device_id().to_owned();

                // We found a matching device, let's check if it owns the session.
                if !(device.is_owner_of_session(session)?) {
                    // The key cannot be linked to an owning device.
                    (
                        VerificationState::Unverified(VerificationLevel::None(
                            DeviceLinkProblem::InsecureSource,
                        )),
                        Some(device_id),
                    )
                } else {
                    // We only consider cross trust and not local trust. If your own device is not
                    // signed and send a message, it will be seen as Unverified.
                    if device.is_cross_signed_by_owner() {
                        // The device is cross signed by this owner Meaning that the user did self
                        // verify it properly. Let's check if we trust the identity.
                        if device.is_device_owner_verified() {
                            (VerificationState::Verified, Some(device_id))
                        } else {
                            (
                                VerificationState::Unverified(
                                    VerificationLevel::UnverifiedIdentity,
                                ),
                                Some(device_id),
                            )
                        }
                    } else {
                        // The device owner hasn't self-verified its device.
                        (
                            VerificationState::Unverified(VerificationLevel::UnsignedDevice),
                            Some(device_id),
                        )
                    }
                }
            }
        })
    }

    /// Get some metadata pertaining to a given group session.
    ///
    /// This includes the session owner's Matrix user ID, their device ID, info
    /// regarding the cryptographic algorithm and whether the session, and by
    /// extension the events decrypted by the session, are trusted.
    async fn get_encryption_info(
        &self,
        session: &InboundGroupSession,
        sender: &UserId,
    ) -> MegolmResult<EncryptionInfo> {
        let (verification_state, device_id) = self.get_verification_state(session, sender).await?;

        let sender = sender.to_owned();

        Ok(EncryptionInfo {
            sender,
            sender_device: device_id,
            algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
                curve25519_key: session.sender_key().to_base64(),
                sender_claimed_keys: session
                    .signing_keys()
                    .iter()
                    .map(|(k, v)| (k.to_owned(), v.to_base64()))
                    .collect(),
            },
            verification_state,
        })
    }

    async fn decrypt_megolm_events(
        &self,
        room_id: &RoomId,
        event: &EncryptedEvent,
        content: &SupportedEventEncryptionSchemes<'_>,
    ) -> MegolmResult<TimelineEvent> {
        if let Some(session) =
            self.store().get_inbound_group_session(room_id, content.session_id()).await?
        {
            // This function is only ever called by decrypt_room_event, so
            // room_id, sender, algorithm and session_id are recorded already
            //
            // While we already record the sender key in some cases from the event, the
            // sender key in the event is deprecated, so let's record it now.
            tracing::Span::current().record("sender_key", debug(session.sender_key()));

            let result = session.decrypt(event).await;
            match result {
                Ok((decrypted_event, _)) => {
                    let encryption_info = self.get_encryption_info(&session, &event.sender).await?;
                    Ok(TimelineEvent {
                        encryption_info: Some(encryption_info),
                        event: decrypted_event,
                        push_actions: None,
                    })
                }
                Err(error) => Err(
                    if let MegolmError::Decryption(DecryptionError::UnknownMessageIndex(_, _)) =
                        error
                    {
                        let withheld_code = self
                            .inner
                            .store
                            .get_withheld_info(room_id, content.session_id())
                            .await?
                            .map(|e| e.content.withheld_code());

                        if withheld_code.is_some() {
                            // Partially withheld, report with a withheld code if we have one.
                            MegolmError::MissingRoomKey(withheld_code)
                        } else {
                            error
                        }
                    } else {
                        error
                    },
                ),
            }
        } else {
            let withheld_code = self
                .inner
                .store
                .get_withheld_info(room_id, content.session_id())
                .await?
                .map(|e| e.content.withheld_code());

            Err(MegolmError::MissingRoomKey(withheld_code))
        }
    }

    /// Decrypt an event from a room timeline.
    ///
    /// # Arguments
    ///
    /// * `event` - The event that should be decrypted.
    ///
    /// * `room_id` - The ID of the room where the event was sent to.
    #[instrument(skip_all, fields(?room_id, event_id, sender, algorithm, session_id, sender_key))]
    pub async fn decrypt_room_event(
        &self,
        event: &Raw<EncryptedEvent>,
        room_id: &RoomId,
    ) -> MegolmResult<TimelineEvent> {
        let event = event.deserialize()?;

        tracing::Span::current()
            .record("sender", debug(&event.sender))
            .record("event_id", debug(&event.event_id))
            .record("algorithm", debug(event.content.algorithm()));

        let content: SupportedEventEncryptionSchemes<'_> = match &event.content.scheme {
            RoomEventEncryptionScheme::MegolmV1AesSha2(c) => {
                tracing::Span::current().record("sender_key", debug(c.sender_key));
                c.into()
            }
            #[cfg(feature = "experimental-algorithms")]
            RoomEventEncryptionScheme::MegolmV2AesSha2(c) => c.into(),
            RoomEventEncryptionScheme::Unknown(_) => {
                warn!("Received an encrypted room event with an unsupported algorithm");
                return Err(EventError::UnsupportedAlgorithm.into());
            }
        };

        tracing::Span::current().record("session_id", content.session_id());
        let result = self.decrypt_megolm_events(room_id, &event, &content).await;

        if let Err(e) = &result {
            #[cfg(feature = "automatic-room-key-forwarding")]
            match e {
                // Optimisation should we request if we received a withheld code?
                // Maybe for some code there is no point
                MegolmError::MissingRoomKey(_)
                | MegolmError::Decryption(DecryptionError::UnknownMessageIndex(_, _)) => {
                    self.inner
                        .key_request_machine
                        .create_outgoing_key_request(room_id, &event)
                        .await?;
                }
                _ => {}
            }

            warn!("Failed to decrypt a room event: {e}");
        }

        result
    }

    /// Update the list of tracked users.
    ///
    /// The OlmMachine maintains a list of users whose devices we are keeping
    /// track of: these are known as "tracked users". These must be users
    /// that we share a room with, so that the server sends us updates for
    /// their device lists.
    ///
    /// # Arguments
    ///
    /// * `users` - An iterator over user ids that should be added to the list
    ///   of tracked users
    ///
    /// Any users that hadn't been seen before will be flagged for a key query
    /// immediately, and whenever [`OlmMachine::receive_sync_changes()`]
    /// receives a "changed" notification for that user in the future.
    ///
    /// Users that were already in the list are unaffected.
    pub async fn update_tracked_users(
        &self,
        users: impl IntoIterator<Item = &UserId>,
    ) -> StoreResult<()> {
        self.inner.identity_manager.update_tracked_users(users).await
    }

    async fn wait_if_user_pending(&self, user_id: &UserId, timeout: Option<Duration>) {
        if let Some(timeout) = timeout {
            self.store().wait_if_user_key_query_pending(timeout, user_id).await;
        }
    }

    /// Get a specific device of a user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the device belongs to.
    ///
    /// * `device_id` - The unique id of the device.
    ///
    /// * `timeout` - The amount of time we should wait before returning if the
    /// user's device list has been marked as stale. **Note**, this assumes that
    /// the requests from [`OlmMachine::outgoing_requests`] are being
    /// processed and sent out.
    ///
    /// Returns a `Device` if one is found and the crypto store didn't throw an
    /// error.
    ///
    /// # Examples
    ///
    /// ```
    /// # use matrix_sdk_crypto::OlmMachine;
    /// # use ruma::{device_id, user_id};
    /// # let alice = user_id!("@alice:example.org").to_owned();
    /// # futures_executor::block_on(async {
    /// # let machine = OlmMachine::new(&alice, device_id!("DEVICEID")).await;
    /// let device = machine.get_device(&alice, device_id!("DEVICEID"), None).await;
    ///
    /// println!("{:?}", device);
    /// # });
    /// ```
    pub async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        timeout: Option<Duration>,
    ) -> StoreResult<Option<Device>> {
        self.wait_if_user_pending(user_id, timeout).await;
        self.store().get_device(user_id, device_id).await
    }

    /// Get the cross signing user identity of a user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the identity belongs to
    ///
    /// * `timeout` - The amount of time we should wait before returning if the
    /// user's device list has been marked as stale. **Note**, this assumes that
    /// the requests from [`OlmMachine::outgoing_requests`] are being
    /// processed and sent out.
    ///
    /// Returns a `UserIdentities` enum if one is found and the crypto store
    /// didn't throw an error.
    pub async fn get_identity(
        &self,
        user_id: &UserId,
        timeout: Option<Duration>,
    ) -> StoreResult<Option<UserIdentities>> {
        self.wait_if_user_pending(user_id, timeout).await;
        self.store().get_identity(user_id).await
    }

    /// Get a map holding all the devices of an user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the devices belong to.
    ///
    /// * `timeout` - The amount of time we should wait before returning if the
    /// user's device list has been marked as stale. **Note**, this assumes that
    /// the requests from [`OlmMachine::outgoing_requests`] are being
    /// processed and sent out.
    ///
    /// # Examples
    ///
    /// ```
    /// # use matrix_sdk_crypto::OlmMachine;
    /// # use ruma::{device_id, user_id};
    /// # let alice = user_id!("@alice:example.org").to_owned();
    /// # futures_executor::block_on(async {
    /// # let machine = OlmMachine::new(&alice, device_id!("DEVICEID")).await;
    /// let devices = machine.get_user_devices(&alice, None).await.unwrap();
    ///
    /// for device in devices.devices() {
    ///     println!("{:?}", device);
    /// }
    /// # });
    /// ```
    pub async fn get_user_devices(
        &self,
        user_id: &UserId,
        timeout: Option<Duration>,
    ) -> StoreResult<UserDevices> {
        self.wait_if_user_pending(user_id, timeout).await;
        self.store().get_user_devices(user_id).await
    }

    /// Import the given room keys into our store.
    ///
    /// # Arguments
    ///
    /// * `exported_keys` - A list of previously exported keys that should be
    /// imported into our store. If we already have a better version of a key
    /// the key will *not* be imported.
    ///
    /// * `from_backup` - Were the room keys imported from the backup, if true
    /// will mark the room keys as already backed up. This will prevent backing
    /// up keys that are already backed up.
    ///
    /// Returns a tuple of numbers that represent the number of sessions that
    /// were imported and the total number of sessions that were found in the
    /// key export.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::io::Cursor;
    /// # use matrix_sdk_crypto::{OlmMachine, decrypt_room_key_export};
    /// # use ruma::{device_id, user_id};
    /// # let alice = user_id!("@alice:example.org");
    /// # async {
    /// # let machine = OlmMachine::new(&alice, device_id!("DEVICEID")).await;
    /// # let export = Cursor::new("".to_owned());
    /// let exported_keys = decrypt_room_key_export(export, "1234").unwrap();
    /// machine.import_room_keys(exported_keys, false, |_, _| {}).await.unwrap();
    /// # };
    /// ```
    pub async fn import_room_keys(
        &self,
        exported_keys: Vec<ExportedRoomKey>,
        #[allow(unused_variables)] from_backup: bool,
        progress_listener: impl Fn(usize, usize),
    ) -> StoreResult<RoomKeyImportResult> {
        let mut sessions = Vec::new();

        async fn new_session_better(
            session: &InboundGroupSession,
            old_session: Option<InboundGroupSession>,
        ) -> bool {
            if let Some(old_session) = &old_session {
                session.compare(old_session).await == SessionOrdering::Better
            } else {
                true
            }
        }

        let total_count = exported_keys.len();
        let mut keys = BTreeMap::new();

        for (i, key) in exported_keys.into_iter().enumerate() {
            match InboundGroupSession::from_export(&key) {
                Ok(session) => {
                    let old_session = self
                        .inner
                        .store
                        .get_inbound_group_session(session.room_id(), session.session_id())
                        .await?;

                    // Only import the session if we didn't have this session or
                    // if it's a better version of the same session.
                    if new_session_better(&session, old_session).await {
                        #[cfg(feature = "backups_v1")]
                        if from_backup {
                            session.mark_as_backed_up();
                        }

                        keys.entry(session.room_id().to_owned())
                            .or_insert_with(BTreeMap::new)
                            .entry(session.sender_key().to_base64())
                            .or_insert_with(BTreeSet::new)
                            .insert(session.session_id().to_owned());

                        sessions.push(session);
                    }
                }
                Err(e) => {
                    warn!(
                        sender_key= key.sender_key.to_base64(),
                        room_id = ?key.room_id,
                        session_id = key.session_id,
                        error = ?e,
                        "Couldn't import a room key from a file export."
                    );
                }
            }

            progress_listener(i, total_count);
        }

        let imported_count = sessions.len();

        let changes = Changes { inbound_group_sessions: sessions, ..Default::default() };

        self.store().save_changes(changes).await?;

        info!(total_count, imported_count, room_keys = ?keys, "Successfully imported room keys");

        Ok(RoomKeyImportResult::new(imported_count, total_count, keys))
    }

    /// Export the keys that match the given predicate.
    ///
    /// # Arguments
    ///
    /// * `predicate` - A closure that will be called for every known
    /// `InboundGroupSession`, which represents a room key. If the closure
    /// returns `true` the `InboundGroupSession` will be included in the export,
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
    /// # use matrix_sdk_crypto::{OlmMachine, encrypt_room_key_export};
    /// # use ruma::{device_id, user_id, room_id};
    /// # let alice = user_id!("@alice:example.org");
    /// # async {
    /// # let machine = OlmMachine::new(&alice, device_id!("DEVICEID")).await;
    /// let room_id = room_id!("!test:localhost");
    /// let exported_keys = machine.export_room_keys(|s| s.room_id() == room_id).await.unwrap();
    /// let encrypted_export = encrypt_room_key_export(&exported_keys, "1234", 1);
    /// # };
    /// ```
    pub async fn export_room_keys(
        &self,
        mut predicate: impl FnMut(&InboundGroupSession) -> bool,
    ) -> StoreResult<Vec<ExportedRoomKey>> {
        let mut exported = Vec::new();

        let sessions: Vec<InboundGroupSession> = self
            .store()
            .get_inbound_group_sessions()
            .await?
            .into_iter()
            .filter(|s| predicate(s))
            .collect();

        for session in sessions {
            let export = session.export().await;
            exported.push(export);
        }

        Ok(exported)
    }

    /// Get the status of the private cross signing keys.
    ///
    /// This can be used to check which private cross signing keys we have
    /// stored locally.
    pub async fn cross_signing_status(&self) -> CrossSigningStatus {
        self.inner.user_identity.lock().await.status().await
    }

    /// Export all the private cross signing keys we have.
    ///
    /// The export will contain the seed for the ed25519 keys as a unpadded
    /// base64 encoded string.
    ///
    /// This method returns `None` if we don't have any private cross signing
    /// keys.
    pub async fn export_cross_signing_keys(&self) -> StoreResult<Option<CrossSigningKeyExport>> {
        let master_key = self.store().export_secret(&SecretName::CrossSigningMasterKey).await?;
        let self_signing_key =
            self.store().export_secret(&SecretName::CrossSigningSelfSigningKey).await?;
        let user_signing_key =
            self.store().export_secret(&SecretName::CrossSigningUserSigningKey).await?;

        Ok(if master_key.is_none() && self_signing_key.is_none() && user_signing_key.is_none() {
            None
        } else {
            Some(CrossSigningKeyExport { master_key, self_signing_key, user_signing_key })
        })
    }

    /// Import our private cross signing keys.
    ///
    /// The export needs to contain the seed for the ed25519 keys as an unpadded
    /// base64 encoded string.
    pub async fn import_cross_signing_keys(
        &self,
        export: CrossSigningKeyExport,
    ) -> Result<CrossSigningStatus, SecretImportError> {
        self.store().import_cross_signing_keys(export).await
    }

    async fn sign_with_master_key(
        &self,
        message: &str,
    ) -> Result<(OwnedDeviceKeyId, Ed25519Signature), SignatureError> {
        let identity = &*self.inner.user_identity.lock().await;
        let key_id = identity.master_key_id().await.ok_or(SignatureError::MissingSigningKey)?;

        let signature = identity.sign(message).await?;

        Ok((key_id, signature))
    }

    /// Sign the given message using our device key and if available cross
    /// signing master key.
    ///
    /// Presently, this should only be used for signing the server-side room
    /// key backups.
    pub async fn sign(&self, message: &str) -> Signatures {
        let mut signatures = Signatures::new();

        let key_id = self.inner.account.signing_key_id();
        let signature = self.inner.account.sign(message).await;
        signatures.add_signature(self.user_id().to_owned(), key_id, signature);

        match self.sign_with_master_key(message).await {
            Ok((key_id, signature)) => {
                signatures.add_signature(self.user_id().to_owned(), key_id, signature);
            }
            Err(e) => {
                warn!(error = ?e, "Couldn't sign the message using the cross signing master key")
            }
        }

        signatures
    }

    /// Get a reference to the backup related state machine.
    ///
    /// This state machine can be used to incrementally backup all room keys to
    /// the server.
    #[cfg(feature = "backups_v1")]
    pub fn backup_machine(&self) -> &BackupMachine {
        &self.inner.backup_machine
    }

    /// Syncs the database and in-memory generation counter.
    ///
    /// This requires that the crypto store lock has been acquired already.
    pub async fn initialize_crypto_store_generation(
        &self,
        generation: &Mutex<Option<u64>>,
    ) -> StoreResult<()> {
        // Avoid reentrant initialization by taking the lock for the entire's function
        // scope.
        let mut gen_guard = generation.lock().await;

        let prev_generation =
            self.inner.store.get_custom_value(Self::CURRENT_GENERATION_STORE_KEY).await?;

        let gen = match prev_generation {
            Some(val) => {
                // There was a value in the store. We need to signal that we're a different
                // process, so we don't just reuse the value but increment it.
                u64::from_le_bytes(
                    val.try_into().map_err(|_| LockStoreError::InvalidGenerationFormat)?,
                )
                .wrapping_add(1)
            }
            None => 0,
        };

        self.inner
            .store
            .set_custom_value(Self::CURRENT_GENERATION_STORE_KEY, gen.to_le_bytes().to_vec())
            .await?;

        *gen_guard = Some(gen);

        Ok(())
    }

    /// If needs be, update the local and on-disk crypto store generation.
    ///
    /// Returns true whether another user has modified the internal generation
    /// counter, and as such we've incremented and updated it in the
    /// database.
    ///
    /// ## Requirements
    ///
    /// - This assumes that `initialize_crypto_store_generation` has been called
    /// beforehand.
    /// - This requires that the crypto store lock has been acquired.
    pub async fn maintain_crypto_store_generation(
        &self,
        generation: &Mutex<Option<u64>>,
    ) -> StoreResult<bool> {
        let mut gen_guard = generation.lock().await;

        // The database value must be there:
        // - either we could initialize beforehand, thus write into the database,
        // - or we couldn't, and then another process was holding onto the database's
        //   lock, thus
        // has written a generation counter in there.
        let actual_gen = self
            .inner
            .store
            .get_custom_value(Self::CURRENT_GENERATION_STORE_KEY)
            .await?
            .ok_or(LockStoreError::MissingGeneration)?;

        let actual_gen = u64::from_le_bytes(
            actual_gen.try_into().map_err(|_| LockStoreError::InvalidGenerationFormat)?,
        );

        let expected_gen = match gen_guard.as_ref() {
            Some(expected_gen) => {
                if actual_gen == *expected_gen {
                    return Ok(false);
                }
                // Increment the biggest, and store it everywhere.
                actual_gen.max(*expected_gen).wrapping_add(1)
            }
            None => {
                // Some other process hold onto the lock when initializing, so we must reload.
                // Increment database value, and store it everywhere.
                actual_gen.wrapping_add(1)
            }
        };

        tracing::debug!(
            "Crypto store generation mismatch: previously known was {:?}, actual is {:?}, next is {}",
            *gen_guard,
            actual_gen,
            expected_gen
        );

        // Update known value.
        *gen_guard = Some(expected_gen);

        // Update value in database.
        self.inner
            .store
            .set_custom_value(
                Self::CURRENT_GENERATION_STORE_KEY,
                expected_gen.to_le_bytes().to_vec(),
            )
            .await?;

        Ok(true)
    }

    /// Manage dehydrated devices.
    pub fn dehydrated_devices(&self) -> DehydratedDevices {
        DehydratedDevices { inner: self.to_owned() }
    }

    #[cfg(any(feature = "testing", test))]
    /// Returns whether this `OlmMachine` is the same another one.
    ///
    /// Useful for testing purposes only.
    pub fn same_as(&self, other: &OlmMachine) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Testing purposes only.
    #[cfg(any(feature = "testing", test))]
    pub fn uploaded_key_count(&self) -> u64 {
        self.inner.account.uploaded_key_count()
    }
}

/// Data contained from a sync response and that needs to be processed by the
/// OlmMachine.
#[derive(Debug)]
pub struct EncryptionSyncChanges<'a> {
    /// The list of to-device events received in the sync.
    pub to_device_events: Vec<Raw<AnyToDeviceEvent>>,
    /// The mapping of changed and left devices, per user.
    pub changed_devices: &'a DeviceLists,
    /// The number of one time keys.
    pub one_time_keys_counts: &'a BTreeMap<DeviceKeyAlgorithm, UInt>,
    /// An optional list of fallback keys.
    pub unused_fallback_keys: Option<&'a [DeviceKeyAlgorithm]>,
    /// A next-batch token obtained from a to-device sync query.
    pub next_batch_token: Option<String>,
}

#[cfg(any(feature = "testing", test))]
pub(crate) mod testing {
    #![allow(dead_code)]
    use http::Response;

    pub fn response_from_file(json: &serde_json::Value) -> Response<Vec<u8>> {
        Response::builder().status(200).body(json.to_string().as_bytes().to_vec()).unwrap()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        collections::BTreeMap,
        iter,
        sync::Arc,
        time::{Duration, SystemTime},
    };

    use assert_matches::assert_matches;
    use futures_util::{FutureExt, StreamExt};
    use matrix_sdk_common::deserialized_responses::{
        DeviceLinkProblem, ShieldState, VerificationLevel, VerificationState,
    };
    use matrix_sdk_test::{async_test, test_json};
    use ruma::{
        api::{
            client::{
                keys::{
                    claim_keys, get_keys, get_keys::v3::Response as KeyQueryResponse, upload_keys,
                },
                sync::sync_events::DeviceLists,
                to_device::send_event_to_device::v3::Response as ToDeviceResponse,
            },
            IncomingResponse,
        },
        device_id,
        encryption::OneTimeKey,
        events::{
            dummy::ToDeviceDummyEventContent,
            key::verification::VerificationMethod,
            room::message::{MessageType, RoomMessageEventContent},
            AnyMessageLikeEvent, AnyMessageLikeEventContent, AnyTimelineEvent, AnyToDeviceEvent,
            MessageLikeEvent, OriginalMessageLikeEvent,
        },
        room_id,
        serde::Raw,
        uint, user_id, DeviceId, DeviceKeyAlgorithm, DeviceKeyId, MilliSecondsSinceUnixEpoch,
        OwnedDeviceKeyId, SecondsSinceUnixEpoch, TransactionId, UserId,
    };
    use serde_json::json;
    use vodozemac::{
        megolm::{GroupSession, SessionConfig},
        Curve25519PublicKey, Ed25519PublicKey,
    };

    use super::testing::response_from_file;
    use crate::{
        error::EventError,
        machine::{EncryptionSyncChanges, OlmMachine},
        olm::{InboundGroupSession, OutboundGroupSession, VerifyJson},
        store::Changes,
        types::{
            events::{
                room::encrypted::{EncryptedToDeviceEvent, ToDeviceEncryptedEventContent},
                room_key_withheld::{RoomKeyWithheldContent, WithheldCode},
                ToDeviceEvent,
            },
            CrossSigningKey, DeviceKeys, EventEncryptionAlgorithm, SignedKey, SigningKeys,
        },
        utilities::json_convert,
        verification::tests::{outgoing_request_to_event, request_to_event},
        EncryptionSettings, LocalTrust, MegolmError, OlmError, ReadOnlyDevice, ToDeviceRequest,
        UserIdentities,
    };

    /// These keys need to be periodically uploaded to the server.
    type OneTimeKeys = BTreeMap<OwnedDeviceKeyId, Raw<OneTimeKey>>;

    fn alice_id() -> &'static UserId {
        user_id!("@alice:example.org")
    }

    fn alice_device_id() -> &'static DeviceId {
        device_id!("JLAFKJWSCS")
    }

    fn bob_device_id() -> &'static DeviceId {
        device_id!("NTHHPZDPRN")
    }

    fn user_id() -> &'static UserId {
        user_id!("@bob:example.com")
    }

    fn keys_upload_response() -> upload_keys::v3::Response {
        let data = response_from_file(&test_json::KEYS_UPLOAD);
        upload_keys::v3::Response::try_from_http_response(data)
            .expect("Can't parse the keys upload response")
    }

    fn keys_query_response() -> get_keys::v3::Response {
        let data = response_from_file(&test_json::KEYS_QUERY);
        get_keys::v3::Response::try_from_http_response(data)
            .expect("Can't parse the keys upload response")
    }

    pub fn to_device_requests_to_content(
        requests: Vec<Arc<ToDeviceRequest>>,
    ) -> ToDeviceEncryptedEventContent {
        let to_device_request = &requests[0];

        to_device_request
            .messages
            .values()
            .next()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .deserialize_as()
            .unwrap()
    }

    pub(crate) async fn get_prepared_machine(
        user_id: &UserId,
        use_fallback_key: bool,
    ) -> (OlmMachine, OneTimeKeys) {
        let machine = OlmMachine::new(user_id, bob_device_id()).await;
        machine.account().generate_fallback_key_helper().await;
        machine.account().update_uploaded_key_count(0);
        machine.account().generate_one_time_keys().await;
        let request = machine.keys_for_upload().await.expect("Can't prepare initial key upload");
        let response = keys_upload_response();
        machine.receive_keys_upload_response(&response).await.unwrap();

        let keys = if use_fallback_key { request.fallback_keys } else { request.one_time_keys };

        (machine, keys)
    }

    async fn get_machine_after_query() -> (OlmMachine, OneTimeKeys) {
        let (machine, otk) = get_prepared_machine(user_id(), false).await;
        let response = keys_query_response();
        let req_id = TransactionId::new();

        machine.receive_keys_query_response(&req_id, &response).await.unwrap();

        (machine, otk)
    }

    async fn get_machine_pair(
        alice: &UserId,
        bob: &UserId,
        use_fallback_key: bool,
    ) -> (OlmMachine, OlmMachine, OneTimeKeys) {
        let (bob, otk) = get_prepared_machine(bob, use_fallback_key).await;

        let alice_device = alice_device_id();
        let alice = OlmMachine::new(alice, alice_device).await;

        let alice_device = ReadOnlyDevice::from_machine(&alice).await;
        let bob_device = ReadOnlyDevice::from_machine(&bob).await;
        alice.store().save_devices(&[bob_device]).await.unwrap();
        bob.store().save_devices(&[alice_device]).await.unwrap();

        (alice, bob, otk)
    }

    async fn get_machine_pair_with_session(
        alice: &UserId,
        bob: &UserId,
        use_fallback_key: bool,
    ) -> (OlmMachine, OlmMachine) {
        let (alice, bob, one_time_keys) = get_machine_pair(alice, bob, use_fallback_key).await;

        let mut bob_keys = BTreeMap::new();

        let (device_key_id, one_time_key) = one_time_keys.iter().next().unwrap();
        let mut keys = BTreeMap::new();
        keys.insert(device_key_id.clone(), one_time_key.clone());
        bob_keys.insert(bob.device_id().into(), keys);

        let mut one_time_keys = BTreeMap::new();
        one_time_keys.insert(bob.user_id().to_owned(), bob_keys);

        let response = claim_keys::v3::Response::new(one_time_keys);

        alice.receive_keys_claim_response(&response).await.unwrap();

        (alice, bob)
    }

    pub(crate) async fn get_machine_pair_with_setup_sessions(
        alice: &UserId,
        bob: &UserId,
        use_fallback_key: bool,
    ) -> (OlmMachine, OlmMachine) {
        let (alice, bob) = get_machine_pair_with_session(alice, bob, use_fallback_key).await;

        let bob_device =
            alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();

        let (session, content) = bob_device
            .encrypt("m.dummy", serde_json::to_value(ToDeviceDummyEventContent::new()).unwrap())
            .await
            .unwrap();
        alice.store().save_sessions(&[session]).await.unwrap();

        let event =
            ToDeviceEvent::new(alice.user_id().to_owned(), content.deserialize_as().unwrap());

        let decrypted = bob.decrypt_to_device_event(&event, &mut Changes::default()).await.unwrap();
        bob.store().save_sessions(&[decrypted.session.session()]).await.unwrap();

        (alice, bob)
    }

    #[async_test]
    async fn create_olm_machine() {
        let machine = OlmMachine::new(user_id(), alice_device_id()).await;
        assert!(!machine.account().shared());

        let own_device = machine
            .get_device(machine.user_id(), machine.device_id(), None)
            .await
            .unwrap()
            .expect("We should always have our own device in the store");

        assert!(own_device.is_locally_trusted(), "Our own device should always be locally trusted");
    }

    #[async_test]
    async fn generate_one_time_keys() {
        let machine = OlmMachine::new(user_id(), alice_device_id()).await;

        assert!(machine.account().generate_one_time_keys().await.is_some());

        let mut response = keys_upload_response();

        machine.receive_keys_upload_response(&response).await.unwrap();
        assert!(machine.account().generate_one_time_keys().await.is_some());

        response.one_time_key_counts.insert(DeviceKeyAlgorithm::SignedCurve25519, uint!(50));
        machine.receive_keys_upload_response(&response).await.unwrap();
        assert!(machine.account().generate_one_time_keys().await.is_none());
    }

    #[async_test]
    async fn test_device_key_signing() {
        let machine = OlmMachine::new(user_id(), alice_device_id()).await;

        let device_keys = machine.account().device_keys().await;
        let identity_keys = machine.account().identity_keys();
        let ed25519_key = identity_keys.ed25519;

        let ret = ed25519_key.verify_json(
            machine.user_id(),
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, machine.device_id()),
            &device_keys,
        );
        ret.unwrap();
    }

    #[async_test]
    async fn tests_session_invalidation() {
        let machine = OlmMachine::new(user_id(), alice_device_id()).await;
        let room_id = room_id!("!test:example.org");

        machine.create_outbound_group_session_with_defaults(room_id).await.unwrap();
        assert!(machine.inner.group_session_manager.get_outbound_group_session(room_id).is_some());

        machine.invalidate_group_session(room_id).await.unwrap();

        assert!(machine
            .inner
            .group_session_manager
            .get_outbound_group_session(room_id)
            .unwrap()
            .invalidated());
    }

    #[async_test]
    async fn test_invalid_signature() {
        let machine = OlmMachine::new(user_id(), alice_device_id()).await;

        let device_keys = machine.account().device_keys().await;

        let key = Ed25519PublicKey::from_slice(&[0u8; 32]).unwrap();

        let ret = key.verify_json(
            machine.user_id(),
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, machine.device_id()),
            &device_keys,
        );
        ret.unwrap_err();
    }

    #[async_test]
    async fn one_time_key_signing() {
        let machine = OlmMachine::new(user_id(), alice_device_id()).await;
        machine.account().update_uploaded_key_count(49);
        machine.account().generate_one_time_keys().await;

        let mut one_time_keys = machine.account().signed_one_time_keys().await;
        let ed25519_key = machine.account().identity_keys().ed25519;

        let one_time_key: SignedKey = one_time_keys
            .values_mut()
            .next()
            .expect("One time keys should be generated")
            .deserialize_as()
            .unwrap();

        ed25519_key
            .verify_json(
                machine.user_id(),
                &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, machine.device_id()),
                &one_time_key,
            )
            .expect("One-time key has been signed successfully");
    }

    #[async_test]
    async fn test_keys_for_upload() {
        let machine = OlmMachine::new(user_id(), alice_device_id()).await;
        let key_counts = BTreeMap::from([(DeviceKeyAlgorithm::SignedCurve25519, 49u8.into())]);
        machine
            .receive_sync_changes(EncryptionSyncChanges {
                to_device_events: Vec::new(),
                changed_devices: &Default::default(),
                one_time_keys_counts: &key_counts,
                unused_fallback_keys: None,
                next_batch_token: None,
            })
            .await
            .expect("We should be able to update our one-time key counts");

        let ed25519_key = machine.account().identity_keys().ed25519;

        let mut request =
            machine.keys_for_upload().await.expect("Can't prepare initial key upload");

        let one_time_key: SignedKey = request
            .one_time_keys
            .values_mut()
            .next()
            .expect("One time keys should be generated")
            .deserialize_as()
            .unwrap();

        let ret = ed25519_key.verify_json(
            machine.user_id(),
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, machine.device_id()),
            &one_time_key,
        );
        ret.unwrap();

        let device_keys: DeviceKeys = request.device_keys.unwrap().deserialize_as().unwrap();

        let ret = ed25519_key.verify_json(
            machine.user_id(),
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, machine.device_id()),
            &device_keys,
        );
        ret.unwrap();

        let mut response = keys_upload_response();
        response.one_time_key_counts.insert(
            DeviceKeyAlgorithm::SignedCurve25519,
            (machine.account().max_one_time_keys().await).try_into().unwrap(),
        );

        machine.receive_keys_upload_response(&response).await.unwrap();

        let ret = machine.keys_for_upload().await;
        assert!(ret.is_none());
    }

    #[async_test]
    async fn test_keys_query() {
        let (machine, _) = get_prepared_machine(user_id(), false).await;
        let response = keys_query_response();
        let alice_id = user_id!("@alice:example.org");
        let alice_device_id: &DeviceId = device_id!("JLAFKJWSCS");

        let alice_devices = machine.store().get_user_devices(alice_id).await.unwrap();
        assert!(alice_devices.devices().peekable().peek().is_none());

        let req_id = TransactionId::new();
        machine.receive_keys_query_response(&req_id, &response).await.unwrap();

        let device = machine.store().get_device(alice_id, alice_device_id).await.unwrap().unwrap();
        assert_eq!(device.user_id(), alice_id);
        assert_eq!(device.device_id(), alice_device_id);
    }

    #[async_test]
    async fn test_query_keys_for_users() {
        let (machine, _) = get_prepared_machine(user_id(), false).await;
        let alice_id = user_id!("@alice:example.org");
        let (_, request) = machine.query_keys_for_users(vec![alice_id]);
        assert!(request.device_keys.contains_key(alice_id));
    }

    #[async_test]
    async fn test_missing_sessions_calculation() {
        let (machine, _) = get_machine_after_query().await;

        let alice = alice_id();
        let alice_device = alice_device_id();

        let (_, missing_sessions) =
            machine.get_missing_sessions(iter::once(alice)).await.unwrap().unwrap();

        assert!(missing_sessions.one_time_keys.contains_key(alice));
        let user_sessions = missing_sessions.one_time_keys.get(alice).unwrap();
        assert!(user_sessions.contains_key(alice_device));
    }

    pub async fn create_session(
        machine: &OlmMachine,
        user_id: &UserId,
        device_id: &DeviceId,
        key_id: OwnedDeviceKeyId,
        one_time_key: Raw<OneTimeKey>,
    ) {
        let keys = BTreeMap::from([(key_id, one_time_key)]);
        let keys = BTreeMap::from([(device_id.to_owned(), keys)]);
        let one_time_keys = BTreeMap::from([(user_id.to_owned(), keys)]);
        let response = claim_keys::v3::Response::new(one_time_keys);

        machine.receive_keys_claim_response(&response).await.unwrap();
    }

    #[async_test]
    async fn test_session_creation() {
        let (alice_machine, bob_machine, mut one_time_keys) =
            get_machine_pair(alice_id(), user_id(), false).await;
        let (device_key_id, one_time_key) = one_time_keys.pop_first().unwrap();

        create_session(
            &alice_machine,
            bob_machine.user_id(),
            bob_machine.device_id(),
            device_key_id,
            one_time_key,
        )
        .await;

        let session = alice_machine
            .store()
            .get_sessions(&bob_machine.account().identity_keys().curve25519.to_base64())
            .await
            .unwrap()
            .unwrap();

        assert!(!session.lock().await.is_empty())
    }

    #[async_test]
    async fn getting_most_recent_session() {
        let (alice_machine, bob_machine, mut one_time_keys) =
            get_machine_pair(alice_id(), user_id(), false).await;
        let (device_key_id, one_time_key) = one_time_keys.pop_first().unwrap();

        let device = alice_machine
            .get_device(bob_machine.user_id(), bob_machine.device_id(), None)
            .await
            .unwrap()
            .unwrap();

        assert!(device.get_most_recent_session().await.unwrap().is_none());

        create_session(
            &alice_machine,
            bob_machine.user_id(),
            bob_machine.device_id(),
            device_key_id,
            one_time_key.to_owned(),
        )
        .await;

        for _ in 0..10 {
            let (device_key_id, one_time_key) = one_time_keys.pop_first().unwrap();

            create_session(
                &alice_machine,
                bob_machine.user_id(),
                bob_machine.device_id(),
                device_key_id,
                one_time_key.to_owned(),
            )
            .await;
        }

        // Since the sessions are created quickly in succession and our timestamps have
        // a resolution in seconds, it's very likely that we're going to end up
        // with the same timestamps, so we manually masage them to be 10s apart.
        let session_id = {
            let sessions = alice_machine
                .store()
                .get_sessions(&bob_machine.identity_keys().curve25519.to_base64())
                .await
                .unwrap()
                .unwrap();

            let mut use_time = SystemTime::now();
            let mut sessions = sessions.lock().await;

            let mut session_id = None;

            // Iterate through the sessions skipping the first and last element so we know
            // that the correct session isn't the first nor the last one.
            let (_, sessions_slice) = sessions.as_mut_slice().split_last_mut().unwrap();

            for session in sessions_slice.iter_mut().skip(1) {
                session.creation_time = SecondsSinceUnixEpoch::from_system_time(use_time).unwrap();
                use_time += Duration::from_secs(10);

                session_id = Some(session.session_id().to_owned());
            }

            session_id.unwrap()
        };

        let newest_session = device.get_most_recent_session().await.unwrap().unwrap();

        assert_eq!(
            newest_session.session_id(),
            session_id,
            "The session we found is the one that was most recently created"
        );
    }

    async fn olm_encryption_test(use_fallback_key: bool) {
        let (alice, bob) =
            get_machine_pair_with_session(alice_id(), user_id(), use_fallback_key).await;

        let bob_device =
            alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();

        let (_, content) = bob_device
            .encrypt("m.dummy", serde_json::to_value(ToDeviceDummyEventContent::new()).unwrap())
            .await
            .expect("We should be able to encrypt a dummy event.");

        let event = ToDeviceEvent::new(
            alice.user_id().to_owned(),
            content
                .deserialize_as()
                .expect("We should be able to deserialize the encrypted content"),
        );

        // Decrypting the first time should succeed.
        let decrypted = bob
            .decrypt_to_device_event(&event, &mut Changes::default())
            .await
            .expect("We should be able to decrypt the event.")
            .result
            .raw_event
            .deserialize()
            .expect("We should be able to deserialize the decrypted event.");

        let decrypted = assert_matches!(decrypted, AnyToDeviceEvent::Dummy(decrypted) => decrypted);
        assert_eq!(&decrypted.sender, alice.user_id());

        // Replaying the event should now result in a decryption failure.
        bob.decrypt_to_device_event(&event, &mut Changes::default()).await.expect_err(
            "Decrypting a replayed event should not succeed, even if it's a pre-key message",
        );
    }

    #[async_test]
    async fn olm_encryption() {
        olm_encryption_test(false).await;
    }

    #[async_test]
    async fn olm_encryption_with_fallback_key() {
        olm_encryption_test(true).await;
    }

    #[async_test]
    async fn test_room_key_sharing() {
        let (alice, bob) = get_machine_pair_with_session(alice_id(), user_id(), false).await;

        let room_id = room_id!("!test:example.org");

        let to_device_requests = alice
            .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
            .await
            .unwrap();

        let event = ToDeviceEvent::new(
            alice.user_id().to_owned(),
            to_device_requests_to_content(to_device_requests),
        );
        let event = json_convert(&event).unwrap();

        let alice_session =
            alice.inner.group_session_manager.get_outbound_group_session(room_id).unwrap();

        let (decrypted, room_key_updates) = bob
            .receive_sync_changes(EncryptionSyncChanges {
                to_device_events: vec![event],
                changed_devices: &Default::default(),
                one_time_keys_counts: &Default::default(),
                unused_fallback_keys: None,
                next_batch_token: None,
            })
            .await
            .unwrap();

        let event = decrypted[0].deserialize().unwrap();

        if let AnyToDeviceEvent::RoomKey(event) = event {
            assert_eq!(&event.sender, alice.user_id());
            assert!(event.content.session_key.is_empty());
        } else {
            panic!("expected RoomKeyEvent found {event:?}");
        }

        let session =
            bob.store().get_inbound_group_session(room_id, alice_session.session_id()).await;

        assert!(session.unwrap().is_some());

        assert_eq!(room_key_updates.len(), 1);
        assert_eq!(room_key_updates[0].room_id, room_id);
        assert_eq!(room_key_updates[0].session_id, alice_session.session_id());
    }

    #[async_test]
    async fn test_megolm_encryption() {
        let (alice, bob) = get_machine_pair_with_setup_sessions(alice_id(), user_id(), false).await;
        let room_id = room_id!("!test:example.org");

        let to_device_requests = alice
            .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
            .await
            .unwrap();

        let event = ToDeviceEvent::new(
            alice.user_id().to_owned(),
            to_device_requests_to_content(to_device_requests),
        );

        let mut room_keys_received_stream = Box::pin(bob.store().room_keys_received_stream());

        let group_session = bob
            .decrypt_to_device_event(&event, &mut Changes::default())
            .await
            .unwrap()
            .inbound_group_session
            .unwrap();
        bob.store().save_inbound_group_sessions(&[group_session.clone()]).await.unwrap();

        // when we decrypt the room key, the
        // inbound_group_session_streamroom_keys_received_stream should tell us
        // about it.
        let room_keys = room_keys_received_stream
            .next()
            .now_or_never()
            .flatten()
            .expect("We should have received an update of room key infos");
        assert_eq!(room_keys.len(), 1);
        assert_eq!(room_keys[0].session_id, group_session.session_id());

        let plaintext = "It is a secret to everybody";

        let content = RoomMessageEventContent::text_plain(plaintext);

        let encrypted_content = alice
            .encrypt_room_event(room_id, AnyMessageLikeEventContent::RoomMessage(content.clone()))
            .await
            .unwrap();

        let event = json!({
            "event_id": "$xxxxx:example.org",
            "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
            "sender": alice.user_id(),
            "type": "m.room.encrypted",
            "content": encrypted_content,
        });

        let event = json_convert(&event).unwrap();

        let decrypted_event =
            bob.decrypt_room_event(&event, room_id).await.unwrap().event.deserialize().unwrap();

        if let AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(
            MessageLikeEvent::Original(OriginalMessageLikeEvent { sender, content, .. }),
        )) = decrypted_event
        {
            assert_eq!(&sender, alice.user_id());
            if let MessageType::Text(c) = &content.msgtype {
                assert_eq!(&c.body, plaintext);
            } else {
                panic!("Decrypted event has a mismatched content");
            }
        } else {
            panic!("Decrypted room event has the wrong type");
        }

        // Just decrypting the event should *not* cause an update on the
        // inbound_group_session_stream.
        if let Some(igs) = room_keys_received_stream.next().now_or_never() {
            panic!("Session stream unexpectedly returned update: {igs:?}");
        }
    }

    #[async_test]
    async fn test_withheld_unverified() {
        let (alice, bob) = get_machine_pair_with_setup_sessions(alice_id(), user_id(), false).await;
        let room_id = room_id!("!test:example.org");

        let encryption_settings = EncryptionSettings::default();
        let encryption_settings =
            EncryptionSettings { only_allow_trusted_devices: true, ..encryption_settings };

        let to_device_requests = alice
            .share_room_key(room_id, iter::once(bob.user_id()), encryption_settings)
            .await
            .expect("Share room key should be ok");

        // Here there will be only one request, and it's for a m.room_key.withheld

        // Transform that into an event to feed it back to bob machine
        let wh_content = to_device_requests[0]
            .messages
            .values()
            .next()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .deserialize_as::<RoomKeyWithheldContent>()
            .expect("Deserialize should work");

        let event = ToDeviceEvent::new(alice.user_id().to_owned(), wh_content);

        let event = json_convert(&event).unwrap();

        bob.receive_sync_changes(EncryptionSyncChanges {
            to_device_events: vec![event],
            changed_devices: &Default::default(),
            one_time_keys_counts: &Default::default(),
            unused_fallback_keys: None,
            next_batch_token: None,
        })
        .await
        .unwrap();

        let plaintext = "You shouldn't be able to decrypt that message";

        let content = RoomMessageEventContent::text_plain(plaintext);

        let content = alice
            .encrypt_room_event(room_id, AnyMessageLikeEventContent::RoomMessage(content.clone()))
            .await
            .unwrap();

        let room_event = json!({
            "event_id": "$xxxxx:example.org",
            "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
            "sender": alice.user_id(),
            "type": "m.room.encrypted",
            "content": content,
        });
        let room_event = json_convert(&room_event).unwrap();

        let decrypt_result = bob.decrypt_room_event(&room_event, room_id).await;

        assert_matches!(decrypt_result, Err(MegolmError::MissingRoomKey(Some(_))));

        let err = decrypt_result.err().unwrap();
        assert_matches!(err, MegolmError::MissingRoomKey(Some(WithheldCode::Unverified)));
    }

    #[async_test]
    async fn test_decryption_verification_state() {
        macro_rules! assert_shield {
            ($foo: ident, $strict: ident, $lax: ident) => {
                let lax = $foo.verification_state.to_shield_state_lax();
                let strict = $foo.verification_state.to_shield_state_strict();

                assert_matches!(lax, ShieldState::$lax { .. });
                assert_matches!(strict, ShieldState::$strict { .. });
            };
        }
        let (alice, bob) = get_machine_pair_with_setup_sessions(alice_id(), user_id(), false).await;
        let room_id = room_id!("!test:example.org");

        let to_device_requests = alice
            .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
            .await
            .unwrap();

        let event = ToDeviceEvent::new(
            alice.user_id().to_owned(),
            to_device_requests_to_content(to_device_requests),
        );

        let group_session = bob
            .decrypt_to_device_event(&event, &mut Changes::default())
            .await
            .unwrap()
            .inbound_group_session;

        let export = group_session.as_ref().unwrap().clone().export().await;

        bob.store().save_inbound_group_sessions(&[group_session.unwrap()]).await.unwrap();

        let plaintext = "It is a secret to everybody";

        let content = RoomMessageEventContent::text_plain(plaintext);

        let encrypted_content = alice
            .encrypt_room_event(room_id, AnyMessageLikeEventContent::RoomMessage(content.clone()))
            .await
            .unwrap();

        let event = json!({
            "event_id": "$xxxxx:example.org",
            "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
            "sender": alice.user_id(),
            "type": "m.room.encrypted",
            "content": encrypted_content,
        });

        let event = json_convert(&event).unwrap();

        let encryption_info =
            bob.decrypt_room_event(&event, room_id).await.unwrap().encryption_info.unwrap();

        assert_eq!(
            VerificationState::Unverified(VerificationLevel::UnsignedDevice),
            encryption_info.verification_state
        );

        assert_shield!(encryption_info, Red, Red);

        // Local trust state has no effect
        bob.get_device(alice.user_id(), alice_device_id(), None)
            .await
            .unwrap()
            .unwrap()
            .set_trust_state(LocalTrust::Verified);

        let encryption_info =
            bob.decrypt_room_event(&event, room_id).await.unwrap().encryption_info.unwrap();

        assert_eq!(
            VerificationState::Unverified(VerificationLevel::UnsignedDevice),
            encryption_info.verification_state
        );
        assert_shield!(encryption_info, Red, Red);

        setup_cross_signing_for_machine(&alice, &bob).await;
        let bob_id_from_alice = alice.get_identity(bob.user_id(), None).await.unwrap();
        assert_matches!(bob_id_from_alice, Some(UserIdentities::Other(_)));
        let alice_id_from_bob = bob.get_identity(alice.user_id(), None).await.unwrap();
        assert_matches!(alice_id_from_bob, Some(UserIdentities::Other(_)));

        // we setup cross signing but nothing is signed yet
        let encryption_info =
            bob.decrypt_room_event(&event, room_id).await.unwrap().encryption_info.unwrap();

        assert_eq!(
            VerificationState::Unverified(VerificationLevel::UnsignedDevice),
            encryption_info.verification_state
        );
        assert_shield!(encryption_info, Red, Red);

        // Let alice sign her device
        sign_alice_device_for_machine(&alice, &bob).await;

        let encryption_info =
            bob.decrypt_room_event(&event, room_id).await.unwrap().encryption_info.unwrap();

        assert_eq!(
            VerificationState::Unverified(VerificationLevel::UnverifiedIdentity),
            encryption_info.verification_state
        );

        assert_shield!(encryption_info, Red, None);

        mark_alice_identity_as_verified(&alice, &bob).await;
        let encryption_info =
            bob.decrypt_room_event(&event, room_id).await.unwrap().encryption_info.unwrap();
        assert_eq!(VerificationState::Verified, encryption_info.verification_state);
        assert_shield!(encryption_info, None, None);

        // Simulate an imported session, to change verification state
        let imported = InboundGroupSession::from_export(&export).unwrap();
        bob.store().save_inbound_group_sessions(&[imported]).await.unwrap();

        let encryption_info =
            bob.decrypt_room_event(&event, room_id).await.unwrap().encryption_info.unwrap();

        // As soon as the key source is unsafe the verification state (or existence) of
        // the device is meaningless
        assert_eq!(
            VerificationState::Unverified(VerificationLevel::None(
                DeviceLinkProblem::InsecureSource
            )),
            encryption_info.verification_state
        );

        assert_shield!(encryption_info, Red, Grey);
    }

    async fn setup_cross_signing_for_machine(alice: &OlmMachine, bob: &OlmMachine) {
        let (alice_upload_signing, _) =
            alice.bootstrap_cross_signing(false).await.expect("Expect Alice x-signing key request");

        let (bob_upload_signing, _) =
            bob.bootstrap_cross_signing(false).await.expect("Expect Bob x-signing key request");

        let bob_device_keys = bob
            .get_device(bob.user_id(), bob.device_id(), None)
            .await
            .unwrap()
            .unwrap()
            .as_device_keys()
            .to_owned();

        let alice_device_keys = alice
            .get_device(alice.user_id(), alice.device_id(), None)
            .await
            .unwrap()
            .unwrap()
            .as_device_keys()
            .to_owned();

        // We only want to setup cross signing we don't actually sign the current
        // devices. so we ignore the new device signatures
        let json = json!({
            "device_keys": {
                bob.user_id() : { bob.device_id() : bob_device_keys},
                alice.user_id() : { alice.device_id():  alice_device_keys }
            },
            "failures": {},
            "master_keys": {
                bob.user_id() : bob_upload_signing.master_key.unwrap(),
                alice.user_id() : alice_upload_signing.master_key.unwrap()
            },
            "user_signing_keys": {
                bob.user_id() : bob_upload_signing.user_signing_key.unwrap(),
                alice.user_id() : alice_upload_signing.user_signing_key.unwrap()
            },
            "self_signing_keys": {
                bob.user_id() : bob_upload_signing.self_signing_key.unwrap(),
                alice.user_id() : alice_upload_signing.self_signing_key.unwrap()
            },
          }
        );

        let kq_response = KeyQueryResponse::try_from_http_response(response_from_file(&json))
            .expect("Can't parse the keys upload response");

        alice.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
        bob.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
    }

    async fn sign_alice_device_for_machine(alice: &OlmMachine, bob: &OlmMachine) {
        let (upload_signing, upload_signature) =
            alice.bootstrap_cross_signing(false).await.expect("Expect Alice x-signing key request");

        let mut device_keys = alice
            .get_device(alice.user_id(), alice.device_id(), None)
            .await
            .unwrap()
            .unwrap()
            .as_device_keys()
            .to_owned();

        let raw_extracted = upload_signature
            .signed_keys
            .get(alice.user_id())
            .unwrap()
            .iter()
            .next()
            .unwrap()
            .1
            .get();

        let new_signature: DeviceKeys = serde_json::from_str(raw_extracted).unwrap();

        let self_sign_key_id = upload_signing
            .self_signing_key
            .as_ref()
            .unwrap()
            .get_first_key_and_id()
            .unwrap()
            .0
            .to_owned();

        device_keys.signatures.add_signature(
            alice.user_id().to_owned(),
            self_sign_key_id.to_owned(),
            new_signature.signatures.get_signature(alice.user_id(), &self_sign_key_id).unwrap(),
        );

        let updated_keys_with_x_signing = json!({ device_keys.device_id.to_string(): device_keys });

        let json = json!({
            "device_keys": {
                alice.user_id() : updated_keys_with_x_signing
            },
            "failures": {},
            "master_keys": {
                alice.user_id() : upload_signing.master_key.unwrap(),
            },
            "user_signing_keys": {
                alice.user_id() : upload_signing.user_signing_key.unwrap(),
            },
            "self_signing_keys": {
                alice.user_id() : upload_signing.self_signing_key.unwrap(),
            },
          }
        );

        let kq_response = KeyQueryResponse::try_from_http_response(response_from_file(&json))
            .expect("Can't parse the keys upload response");

        alice.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
        bob.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
    }

    async fn mark_alice_identity_as_verified(alice: &OlmMachine, bob: &OlmMachine) {
        let alice_device =
            bob.get_device(alice.user_id(), alice.device_id(), None).await.unwrap().unwrap();

        let alice_identity =
            bob.get_identity(alice.user_id(), None).await.unwrap().unwrap().other().unwrap();
        let upload_request = alice_identity.verify().await.unwrap();

        let raw_extracted =
            upload_request.signed_keys.get(alice.user_id()).unwrap().iter().next().unwrap().1.get();

        let new_signature: CrossSigningKey = serde_json::from_str(raw_extracted).unwrap();

        let user_key_id = bob
            .bootstrap_cross_signing(false)
            .await
            .expect("Expect Alice x-signing key request")
            .0
            .user_signing_key
            .unwrap()
            .get_first_key_and_id()
            .unwrap()
            .0
            .to_owned();

        // add the new signature to alice msk
        let mut alice_updated_msk =
            alice_device.device_owner_identity.as_ref().unwrap().master_key().as_ref().to_owned();

        alice_updated_msk.signatures.add_signature(
            bob.user_id().to_owned(),
            user_key_id.to_owned(),
            new_signature.signatures.get_signature(bob.user_id(), &user_key_id).unwrap(),
        );

        let alice_x_keys = alice
            .bootstrap_cross_signing(false)
            .await
            .expect("Expect Alice x-signing key request")
            .0;

        let json = json!({
            "device_keys": {
                alice.user_id() : { alice.device_id():  alice_device.as_device_keys().to_owned() }
            },
            "failures": {},
            "master_keys": {
                alice.user_id() : alice_updated_msk,
            },
            "user_signing_keys": {
                alice.user_id() : alice_x_keys.user_signing_key.unwrap(),
            },
            "self_signing_keys": {
                alice.user_id() : alice_x_keys.self_signing_key.unwrap(),
            },
          }
        );

        let kq_response = KeyQueryResponse::try_from_http_response(response_from_file(&json))
            .expect("Can't parse the keys upload response");

        alice.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
        bob.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();

        // so alice identity should be now trusted

        assert!(bob
            .get_identity(alice.user_id(), None)
            .await
            .unwrap()
            .unwrap()
            .other()
            .unwrap()
            .is_verified());
    }

    #[async_test]
    async fn test_verification_states_multiple_device() {
        let (bob, _) = get_prepared_machine(user_id(), false).await;

        let other_user_id = user_id!("@web2:localhost:8482");

        let data = response_from_file(&test_json::KEYS_QUERY_TWO_DEVICES_ONE_SIGNED);
        let response = get_keys::v3::Response::try_from_http_response(data)
            .expect("Can't parse the keys upload response");

        let (device_change, identity_change) =
            bob.receive_keys_query_response(&TransactionId::new(), &response).await.unwrap();
        assert_eq!(device_change.new.len(), 2);
        assert_eq!(identity_change.new.len(), 1);
        //
        let devices = bob.store().get_user_devices(other_user_id).await.unwrap();
        assert_eq!(devices.devices().count(), 2);

        let fake_room_id = room_id!("!roomid:example.com");

        // We just need a fake session to export it
        // We will use the export to create various inbounds with other claimed
        // ownership
        let id_keys = bob.identity_keys();
        let fake_device_id = bob.device_id().into();
        let olm = OutboundGroupSession::new(
            fake_device_id,
            Arc::new(id_keys),
            fake_room_id,
            EncryptionSettings::default(),
        )
        .unwrap()
        .session_key()
        .await;

        let web_unverified_inbound_session = InboundGroupSession::new(
            Curve25519PublicKey::from_base64("LTpv2DGMhggPAXO02+7f68CNEp6A40F0Yl8B094Y8gc")
                .unwrap(),
            Ed25519PublicKey::from_base64("loz5i40dP+azDtWvsD0L/xpnCjNkmrcvtXVXzCHX8Vw").unwrap(),
            fake_room_id,
            &olm,
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            None,
        )
        .unwrap();

        let (state, _) = bob
            .get_verification_state(&web_unverified_inbound_session, other_user_id)
            .await
            .unwrap();
        assert_eq!(VerificationState::Unverified(VerificationLevel::UnsignedDevice), state);

        let web_signed_inbound_session = InboundGroupSession::new(
            Curve25519PublicKey::from_base64("XJixbpnfIk+RqcK5T6moqVY9d9Q1veR8WjjSlNiQNT0")
                .unwrap(),
            Ed25519PublicKey::from_base64("48f3WQAMGwYLBg5M5qUhqnEVA8yeibjZpPsShoWMFT8").unwrap(),
            fake_room_id,
            &olm,
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            None,
        )
        .unwrap();

        let (state, _) =
            bob.get_verification_state(&web_signed_inbound_session, other_user_id).await.unwrap();

        assert_eq!(VerificationState::Unverified(VerificationLevel::UnverifiedIdentity), state);
    }

    #[async_test]
    #[cfg(feature = "automatic-room-key-forwarding")]
    async fn test_query_ratcheted_key() {
        let (alice, bob) = get_machine_pair_with_setup_sessions(alice_id(), user_id(), false).await;
        let room_id = room_id!("!test:example.org");

        // Need a second bob session to check gossiping
        let bob_id = user_id();
        let bob_other_device = device_id!("OTHERBOB");
        let bob_other_machine = OlmMachine::new(bob_id, bob_other_device).await;
        let bob_other_device = ReadOnlyDevice::from_machine(&bob_other_machine).await;
        bob.store().save_devices(&[bob_other_device]).await.unwrap();
        bob.get_device(bob_id, device_id!("OTHERBOB"), None)
            .await
            .unwrap()
            .expect("should exist")
            .set_trust_state(crate::LocalTrust::Verified);

        alice.create_outbound_group_session_with_defaults(room_id).await.unwrap();

        let plaintext = "It is a secret to everybody";

        let content = RoomMessageEventContent::text_plain(plaintext);

        let content = alice
            .encrypt_room_event(room_id, AnyMessageLikeEventContent::RoomMessage(content.clone()))
            .await
            .unwrap();

        let room_event = json!({
            "event_id": "$xxxxx:example.org",
            "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
            "sender": alice.user_id(),
            "type": "m.room.encrypted",
            "content": content,
        });

        // should share at index 1
        let to_device_requests = alice
            .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
            .await
            .unwrap();

        let event = ToDeviceEvent::new(
            alice.user_id().to_owned(),
            to_device_requests_to_content(to_device_requests),
        );

        let group_session = bob
            .decrypt_to_device_event(&event, &mut Changes::default())
            .await
            .unwrap()
            .inbound_group_session;
        bob.store().save_inbound_group_sessions(&[group_session.unwrap()]).await.unwrap();

        let room_event = json_convert(&room_event).unwrap();

        let decrypt_error = bob.decrypt_room_event(&room_event, room_id).await.unwrap_err();

        if let crate::MegolmError::Decryption(vodo_error) = decrypt_error {
            if let vodozemac::megolm::DecryptionError::UnknownMessageIndex(_, _) = vodo_error {
                // check that key has been requested
                let outgoing_to_devices =
                    bob.inner.key_request_machine.outgoing_to_device_requests().await.unwrap();
                assert_eq!(1, outgoing_to_devices.len());
            } else {
                panic!("Should be UnknownMessageIndex error ")
            }
        } else {
            panic!("Should have been unable to decrypt")
        }
    }

    #[async_test]
    async fn interactive_verification() {
        let (alice, bob) = get_machine_pair_with_setup_sessions(alice_id(), user_id(), false).await;

        let bob_device =
            alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();

        assert!(!bob_device.is_verified());

        let (alice_sas, request) = bob_device.start_verification().await.unwrap();

        let event = request_to_event(alice.user_id(), &request.into());
        bob.handle_verification_event(&event).await;

        let bob_sas = bob
            .get_verification(alice.user_id(), alice_sas.flow_id().as_str())
            .unwrap()
            .sas_v1()
            .unwrap();

        assert!(alice_sas.emoji().is_none());
        assert!(bob_sas.emoji().is_none());

        let event = bob_sas.accept().map(|r| request_to_event(bob.user_id(), &r)).unwrap();

        alice.handle_verification_event(&event).await;

        let (event, request_id) = alice
            .inner
            .verification_machine
            .outgoing_messages()
            .first()
            .map(|r| (outgoing_request_to_event(alice.user_id(), r), r.request_id.to_owned()))
            .unwrap();
        alice.mark_request_as_sent(&request_id, &ToDeviceResponse::new()).await.unwrap();
        bob.handle_verification_event(&event).await;

        let (event, request_id) = bob
            .inner
            .verification_machine
            .outgoing_messages()
            .first()
            .map(|r| (outgoing_request_to_event(bob.user_id(), r), r.request_id.to_owned()))
            .unwrap();
        alice.handle_verification_event(&event).await;
        bob.mark_request_as_sent(&request_id, &ToDeviceResponse::new()).await.unwrap();

        assert!(alice_sas.emoji().is_some());
        assert!(bob_sas.emoji().is_some());

        assert_eq!(alice_sas.emoji(), bob_sas.emoji());
        assert_eq!(alice_sas.decimals(), bob_sas.decimals());

        let contents = bob_sas.confirm().await.unwrap().0;
        assert!(contents.len() == 1);
        let event = request_to_event(bob.user_id(), &contents[0]);
        alice.handle_verification_event(&event).await;

        assert!(!alice_sas.is_done());
        assert!(!bob_sas.is_done());

        let contents = alice_sas.confirm().await.unwrap().0;
        assert!(contents.len() == 1);
        let event = request_to_event(alice.user_id(), &contents[0]);

        assert!(alice_sas.is_done());
        assert!(bob_device.is_verified());

        let alice_device =
            bob.get_device(alice.user_id(), alice.device_id(), None).await.unwrap().unwrap();

        assert!(!alice_device.is_verified());
        bob.handle_verification_event(&event).await;
        assert!(bob_sas.is_done());
        assert!(alice_device.is_verified());
    }

    #[async_test]
    async fn interactive_verification_started_from_request() {
        let (alice, bob) = get_machine_pair_with_setup_sessions(alice_id(), user_id(), false).await;

        // ----------------------------------------------------------------------------
        // On Alice's device:
        let bob_device =
            alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();

        assert!(!bob_device.is_verified());

        // Alice sends a verification request with her desired methods to Bob
        let (alice_ver_req, request) =
            bob_device.request_verification_with_methods(vec![VerificationMethod::SasV1]).await;

        // ----------------------------------------------------------------------------
        // On Bobs's device:
        let event = request_to_event(alice.user_id(), &request);
        bob.handle_verification_event(&event).await;
        let flow_id = alice_ver_req.flow_id().as_str();

        let verification_request = bob.get_verification_request(alice.user_id(), flow_id).unwrap();

        // Bob accepts the request, sending a Ready request
        let accept_request =
            verification_request.accept_with_methods(vec![VerificationMethod::SasV1]).unwrap();
        // And also immediately sends a start request
        let (_, start_request_from_bob) = verification_request.start_sas().await.unwrap().unwrap();

        // ----------------------------------------------------------------------------
        // On Alice's device:

        // Alice receives the Ready
        let event = request_to_event(bob.user_id(), &accept_request);
        alice.handle_verification_event(&event).await;

        let verification_request = alice.get_verification_request(bob.user_id(), flow_id).unwrap();

        // And also immediately sends a start request
        let (alice_sas, start_request_from_alice) =
            verification_request.start_sas().await.unwrap().unwrap();

        // Now alice receives Bob's start:
        let event = request_to_event(bob.user_id(), &start_request_from_bob);
        alice.handle_verification_event(&event).await;

        // Since Alice's user id is lexicographically smaller than Bob's, Alice does not
        // do anything with the request, however.
        assert!(alice.user_id() < bob.user_id());

        // ----------------------------------------------------------------------------
        // On Bob's device:

        // Bob receives Alice's start:
        let event = request_to_event(alice.user_id(), &start_request_from_alice);
        bob.handle_verification_event(&event).await;

        let bob_sas = bob
            .get_verification(alice.user_id(), alice_sas.flow_id().as_str())
            .unwrap()
            .sas_v1()
            .unwrap();

        assert!(alice_sas.emoji().is_none());
        assert!(bob_sas.emoji().is_none());

        // ... and accepts it
        let event = bob_sas.accept().map(|r| request_to_event(bob.user_id(), &r)).unwrap();

        // ----------------------------------------------------------------------------
        // On Alice's device:

        // Alice receives the Accept request:
        alice.handle_verification_event(&event).await;

        // Alice sends a key
        let msgs = alice.inner.verification_machine.outgoing_messages();
        assert!(msgs.len() == 1);
        let msg = msgs.first().unwrap();
        let event = outgoing_request_to_event(alice.user_id(), msg);
        alice.inner.verification_machine.mark_request_as_sent(&msg.request_id);

        // ----------------------------------------------------------------------------
        // On Bob's device:

        // And bob receive's it:
        bob.handle_verification_event(&event).await;

        // Now bob sends a key
        let msgs = bob.inner.verification_machine.outgoing_messages();
        assert!(msgs.len() == 1);
        let msg = msgs.first().unwrap();
        let event = outgoing_request_to_event(bob.user_id(), msg);
        bob.inner.verification_machine.mark_request_as_sent(&msg.request_id);

        // ----------------------------------------------------------------------------
        // On Alice's device:

        // And alice receives it
        alice.handle_verification_event(&event).await;

        // As a result, both devices now can show emojis/decimals
        assert!(alice_sas.emoji().is_some());
        assert!(bob_sas.emoji().is_some());

        // ----------------------------------------------------------------------------
        // On Bob's device:

        assert_eq!(alice_sas.emoji(), bob_sas.emoji());
        assert_eq!(alice_sas.decimals(), bob_sas.decimals());

        // Bob first confirms that the emojis match and sends the MAC...
        let contents = bob_sas.confirm().await.unwrap().0;
        assert!(contents.len() == 1);
        let event = request_to_event(bob.user_id(), &contents[0]);

        // ----------------------------------------------------------------------------
        // On Alice's device:

        // ...which alice receives
        alice.handle_verification_event(&event).await;

        assert!(!alice_sas.is_done());
        assert!(!bob_sas.is_done());

        // Now alice confirms that the emojis match and sends...
        let contents = alice_sas.confirm().await.unwrap().0;
        assert!(contents.len() == 2);
        // ... her own MAC...
        let event_mac = request_to_event(alice.user_id(), &contents[0]);
        // ... and a Done message
        let event_done = request_to_event(alice.user_id(), &contents[1]);

        // ----------------------------------------------------------------------------
        // On Bob's device:

        // Bob receives the MAC message
        bob.handle_verification_event(&event_mac).await;

        // Bob verifies that the MAC is valid and also sends a "done" message.
        let msgs = bob.inner.verification_machine.outgoing_messages();
        eprintln!("{msgs:?}");
        assert!(msgs.len() == 1);
        let event = msgs.first().map(|r| outgoing_request_to_event(bob.user_id(), r)).unwrap();

        let alice_device =
            bob.get_device(alice.user_id(), alice.device_id(), None).await.unwrap().unwrap();

        assert!(!bob_sas.is_done());
        assert!(!alice_device.is_verified());
        // And Bob receives the Done message of alice.
        bob.handle_verification_event(&event_done).await;

        assert!(bob_sas.is_done());
        assert!(alice_device.is_verified());

        // ----------------------------------------------------------------------------
        // On Alice's device:

        assert!(!alice_sas.is_done());
        assert!(!bob_device.is_verified());
        // Alices receives the done message
        eprintln!("{event:?}");
        alice.handle_verification_event(&event).await;

        assert!(alice_sas.is_done());
        assert!(bob_device.is_verified());
    }

    #[async_test]
    async fn room_key_over_megolm() {
        let (alice, bob) = get_machine_pair_with_setup_sessions(alice_id(), user_id(), false).await;
        let room_id = room_id!("!test:example.org");

        let to_device_requests = alice
            .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
            .await
            .unwrap();

        let event = ToDeviceEvent {
            sender: alice.user_id().to_owned(),
            content: to_device_requests_to_content(to_device_requests),
            other: Default::default(),
        };
        let event = json_convert(&event).unwrap();
        let changed_devices = DeviceLists::new();
        let key_counts: BTreeMap<_, _> = Default::default();

        let _ = bob
            .receive_sync_changes(EncryptionSyncChanges {
                to_device_events: vec![event],
                changed_devices: &changed_devices,
                one_time_keys_counts: &key_counts,
                unused_fallback_keys: None,
                next_batch_token: None,
            })
            .await
            .unwrap();

        let group_session = GroupSession::new(SessionConfig::version_1());
        let session_key = group_session.session_key();
        let session_id = group_session.session_id();

        let content = json!({
            "algorithm": "m.megolm.v1.aes-sha2",
            "room_id": room_id,
            "session_id": session_id,
            "session_key": session_key.to_base64(),

        });

        let encrypted_content =
            alice.encrypt_room_event_raw(room_id, content, "m.room_key").await.unwrap();
        let event = json!({
            "sender": alice.user_id(),
            "content": encrypted_content,
            "type": "m.room.encrypted",
        });

        let event: EncryptedToDeviceEvent = serde_json::from_value(event).unwrap();

        assert_matches!(
            bob.decrypt_to_device_event(&event, &mut Changes::default()).await,
            Err(OlmError::EventError(EventError::UnsupportedAlgorithm))
        );

        let event: Raw<AnyToDeviceEvent> = json_convert(&event).unwrap();

        bob.receive_sync_changes(EncryptionSyncChanges {
            to_device_events: vec![event],
            changed_devices: &changed_devices,
            one_time_keys_counts: &key_counts,
            unused_fallback_keys: None,
            next_batch_token: None,
        })
        .await
        .unwrap();

        let session = bob.store().get_inbound_group_session(room_id, &session_id).await;

        assert!(session.unwrap().is_none());
    }

    #[async_test]
    async fn room_key_with_fake_identity_keys() {
        let room_id = room_id!("!test:localhost");
        let (alice, _) = get_machine_pair_with_setup_sessions(alice_id(), user_id(), false).await;
        let device = ReadOnlyDevice::from_machine(&alice).await;
        alice.store().save_devices(&[device]).await.unwrap();

        let (outbound, mut inbound) =
            alice.account().create_group_session_pair(room_id, Default::default()).await.unwrap();

        let fake_key = Ed25519PublicKey::from_base64("ee3Ek+J2LkkPmjGPGLhMxiKnhiX//xcqaVL4RP6EypE")
            .unwrap()
            .into();
        let signing_keys = SigningKeys::from([(DeviceKeyAlgorithm::Ed25519, fake_key)]);
        inbound.creator_info.signing_keys = signing_keys.into();

        let content = json!({});
        let content = outbound.encrypt(content, "m.dummy").await;
        alice.store().save_inbound_group_sessions(&[inbound]).await.unwrap();

        let event = json!({
            "sender": alice.user_id(),
            "event_id": "$xxxxx:example.org",
            "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
            "type": "m.room.encrypted",
            "content": content,
        });
        let event = json_convert(&event).unwrap();

        assert_matches!(
            alice.decrypt_room_event(&event, room_id).await,
            Err(MegolmError::MismatchedIdentityKeys { .. })
        );
    }

    #[async_test]
    async fn importing_private_cross_signing_keys_verifies_the_public_identity() {
        async fn create_additional_machine(machine: &OlmMachine) -> OlmMachine {
            let second_machine =
                OlmMachine::new(machine.user_id(), "ADDITIONAL_MACHINE".into()).await;

            let identity = machine
                .get_identity(machine.user_id(), None)
                .await
                .unwrap()
                .expect("We should know about our own user identity if we bootstrapped it")
                .own()
                .unwrap();

            let mut changes = Changes::default();
            identity.mark_as_unverified();
            changes.identities.new.push(crate::ReadOnlyUserIdentities::Own(identity.inner));

            second_machine.store().save_changes(changes).await.unwrap();

            second_machine
        }

        let (alice, bob) = get_machine_pair_with_setup_sessions(alice_id(), user_id(), false).await;
        setup_cross_signing_for_machine(&alice, &bob).await;

        let second_alice = create_additional_machine(&alice).await;

        let export = alice
            .export_cross_signing_keys()
            .await
            .unwrap()
            .expect("We should be able to export our cross-signing keys");

        let identity = second_alice
            .get_identity(second_alice.user_id(), None)
            .await
            .unwrap()
            .expect("We should know about our own user identity")
            .own()
            .unwrap();

        assert!(!identity.is_verified(), "Initially our identity should not be verified");

        second_alice
            .import_cross_signing_keys(export)
            .await
            .expect("We should be able to import our cross-signing keys");

        let identity = second_alice
            .get_identity(second_alice.user_id(), None)
            .await
            .unwrap()
            .expect("We should know about our own user identity")
            .own()
            .unwrap();

        assert!(
            identity.is_verified(),
            "Our identity should be verified after we imported the private cross-signing keys"
        );

        let second_bob = create_additional_machine(&bob).await;

        let export = second_alice
            .export_cross_signing_keys()
            .await
            .unwrap()
            .expect("The machine should now be able to export cross-signing keys as well");

        second_bob.import_cross_signing_keys(export).await.expect_err(
            "Importing cross-signing keys that don't match our public identity should fail",
        );

        let identity = second_bob
            .get_identity(second_bob.user_id(), None)
            .await
            .unwrap()
            .expect("We should know about our own user identity")
            .own()
            .unwrap();

        assert!(
            !identity.is_verified(),
            "Our identity should not be verified when there's a mismatch in the cross-signing keys"
        );
    }
}
