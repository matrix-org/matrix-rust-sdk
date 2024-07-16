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
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, RwLock as StdRwLock},
    time::Duration,
};

use itertools::Itertools;
use matrix_sdk_common::{
    deserialized_responses::{
        AlgorithmInfo, DeviceLinkProblem, EncryptionInfo, TimelineEvent, UnableToDecryptInfo,
        UnsignedDecryptionResult, UnsignedEventLocation, VerificationLevel, VerificationState,
    },
    BoxFuture,
};
use ruma::{
    api::client::{
        dehydrated_device::DehydratedDeviceData,
        keys::{
            claim_keys::v3::Request as KeysClaimRequest,
            get_keys::v3::Response as KeysQueryResponse,
            upload_keys::v3::{Request as UploadKeysRequest, Response as UploadKeysResponse},
            upload_signatures::v3::Request as UploadSignaturesRequest,
        },
        sync::sync_events::DeviceLists,
    },
    assign,
    events::{
        secret::request::SecretName, AnyMessageLikeEvent, AnyMessageLikeEventContent,
        AnyTimelineEvent, AnyToDeviceEvent, MessageLikeEventContent,
    },
    serde::{JsonObject, Raw},
    DeviceId, DeviceKeyAlgorithm, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedDeviceKeyId,
    OwnedTransactionId, OwnedUserId, RoomId, TransactionId, UInt, UserId,
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

use crate::{
    backups::{BackupMachine, MegolmV1BackupKey},
    dehydrated_devices::{DehydratedDevices, DehydrationError},
    error::{EventError, MegolmError, MegolmResult, OlmError, OlmResult, SetRoomSettingsError},
    gossiping::GossipMachine,
    identities::{user::UserIdentities, Device, IdentityManager, UserDevices},
    olm::{
        Account, CrossSigningStatus, EncryptionSettings, IdentityKeys, InboundGroupSession,
        OlmDecryptionInfo, PrivateCrossSigningIdentity, SenderDataFinder, SessionType,
        StaticAccountData,
    },
    requests::{IncomingResponse, OutgoingRequest, UploadSigningKeysRequest},
    session_manager::{GroupSessionManager, SessionManager},
    store::{
        Changes, CryptoStoreWrapper, DeviceChanges, IdentityChanges, IntoCryptoStore, MemoryStore,
        PendingChanges, Result as StoreResult, RoomKeyInfo, RoomSettings, SecretImportError, Store,
        StoreCache, StoreTransaction,
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
        EventEncryptionAlgorithm, Signatures,
    },
    utilities::timestamp_to_iso8601,
    verification::{Verification, VerificationMachine, VerificationRequest},
    CrossSigningKeyExport, CryptoStoreError, DeviceData, KeysQueryRequest, LocalTrust,
    SignatureError, ToDeviceRequest,
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
    const CURRENT_GENERATION_STORE_KEY: &'static str = "generation-counter";

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
        OlmMachine::with_store(user_id, device_id, MemoryStore::new(), None)
            .await
            .expect("Reading and writing to the memory store always succeeds")
    }

    pub(crate) async fn rehydrate(
        &self,
        pickle_key: &[u8; 32],
        device_id: &DeviceId,
        device_data: Raw<DehydratedDeviceData>,
    ) -> Result<OlmMachine, DehydrationError> {
        let account = Account::rehydrate(pickle_key, self.user_id(), device_id, device_data)?;
        let static_account = account.static_data().clone();

        let store = Arc::new(CryptoStoreWrapper::new(self.user_id(), MemoryStore::new()));
        let device = DeviceData::from_account(&account);
        store.save_pending_changes(PendingChanges { account: Some(account) }).await?;
        store
            .save_changes(Changes {
                devices: DeviceChanges { new: vec![device], ..Default::default() },
                ..Default::default()
            })
            .await?;

        Ok(Self::new_helper(
            device_id,
            store,
            static_account,
            self.store().private_identity(),
            None,
        ))
    }

    fn new_helper(
        device_id: &DeviceId,
        store: Arc<CryptoStoreWrapper>,
        account: StaticAccountData,
        user_identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
        maybe_backup_key: Option<MegolmV1BackupKey>,
    ) -> Self {
        let verification_machine =
            VerificationMachine::new(account.clone(), user_identity.clone(), store.clone());
        let store = Store::new(account, user_identity.clone(), store, verification_machine.clone());

        let group_session_manager = GroupSessionManager::new(store.clone());

        let identity_manager = IdentityManager::new(store.clone());

        let users_for_key_claim = Arc::new(StdRwLock::new(BTreeMap::new()));
        let key_request_machine = GossipMachine::new(
            store.clone(),
            identity_manager.clone(),
            group_session_manager.session_cache(),
            users_for_key_claim.clone(),
        );

        let session_manager =
            SessionManager::new(users_for_key_claim, key_request_machine.clone(), store.clone());

        let backup_machine = BackupMachine::new(store.clone(), maybe_backup_key);

        let inner = Arc::new(OlmMachineInner {
            user_id: store.user_id().to_owned(),
            device_id: device_id.to_owned(),
            user_identity,
            store,
            session_manager,
            group_session_manager,
            verification_machine,
            key_request_machine,
            identity_manager,
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
    /// * `store` - A `CryptoStore` implementation that will be used to store
    /// the encryption keys.
    ///
    /// * `custom_account` - A custom [`vodozemac::olm::Account`] to be used for
    ///   the identity and one-time keys of this [`OlmMachine`]. If no account
    ///   is provided, a new default one or one from the store will be used. If
    ///   an account is provided and one already exists in the store for this
    ///   [`UserId`]/[`DeviceId`] combination, an error will be raised. This is
    ///   useful if one wishes to create identity keys before knowing the
    ///   user/device IDs, e.g., to use the identity key as the device ID.
    ///
    /// [`CryptoStore`]: crate::store::CryptoStore
    #[instrument(skip(store, custom_account), fields(ed25519_key, curve25519_key))]
    pub async fn with_store(
        user_id: &UserId,
        device_id: &DeviceId,
        store: impl IntoCryptoStore,
        custom_account: Option<vodozemac::olm::Account>,
    ) -> StoreResult<Self> {
        let store = store.into_crypto_store();

        let static_account = match store.load_account().await? {
            Some(account) => {
                if user_id != account.user_id()
                    || device_id != account.device_id()
                    || custom_account.is_some()
                {
                    return Err(CryptoStoreError::MismatchedAccount {
                        expected: (account.user_id().to_owned(), account.device_id().to_owned()),
                        got: (user_id.to_owned(), device_id.to_owned()),
                    });
                }

                Span::current()
                    .record("ed25519_key", display(account.identity_keys().ed25519))
                    .record("curve25519_key", display(account.identity_keys().curve25519));
                debug!("Restored an Olm account");

                account.static_data().clone()
            }

            None => {
                let account = if let Some(account) = custom_account {
                    Account::new_helper(account, user_id, device_id)
                } else {
                    Account::with_device_id(user_id, device_id)
                };

                let static_account = account.static_data().clone();

                Span::current()
                    .record("ed25519_key", display(account.identity_keys().ed25519))
                    .record("curve25519_key", display(account.identity_keys().curve25519));

                let device = DeviceData::from_account(&account);

                // We just created this device from our own Olm `Account`. Since we are the
                // owners of the private keys of this device we can safely mark
                // the device as verified.
                device.set_trust_state(LocalTrust::Verified);

                let changes = Changes {
                    devices: DeviceChanges { new: vec![device], ..Default::default() },
                    ..Default::default()
                };
                store.save_changes(changes).await?;
                store.save_pending_changes(PendingChanges { account: Some(account) }).await?;

                debug!("Created a new Olm account");

                static_account
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

        // FIXME: This is a workaround for `regenerate_olm` clearing the backup
        // state. Ideally, backups should not get automatically enabled since
        // the `OlmMachine` doesn't get enough info from the homeserver for this
        // to work reliably.
        let saved_keys = store.load_backup_keys().await?;
        let maybe_backup_key = saved_keys.decryption_key.and_then(|k| {
            if let Some(version) = saved_keys.backup_version {
                let megolm_v1_backup_key = k.megolm_v1_public_key();
                megolm_v1_backup_key.set_version(version);
                Some(megolm_v1_backup_key)
            } else {
                None
            }
        });

        let identity = Arc::new(Mutex::new(identity));
        let store = Arc::new(CryptoStoreWrapper::new(user_id, store));
        Ok(OlmMachine::new_helper(device_id, store, static_account, identity, maybe_backup_key))
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

    /// The time at which the `Account` backing this `OlmMachine` was created.
    ///
    /// An [`Account`] is created when an `OlmMachine` is first instantiated
    /// against a given [`Store`], at which point it creates identity keys etc.
    /// This method returns the timestamp, according to the local clock, at
    /// which that happened.
    pub fn device_creation_time(&self) -> MilliSecondsSinceUnixEpoch {
        self.inner.store.static_account().creation_local_time()
    }

    /// Get the public parts of our Olm identity keys.
    pub fn identity_keys(&self) -> IdentityKeys {
        let account = self.inner.store.static_account();
        account.identity_keys()
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
        let cache = self.store().cache().await?;
        Ok(self.inner.identity_manager.key_query_manager.synced(&cache).await?.tracked_users())
    }

    /// Enable or disable room key requests.
    ///
    /// Room key requests allow the device to request room keys that it might
    /// have missed in the original share using `m.room_key_request`
    /// events.
    ///
    /// See also [`OlmMachine::set_room_key_forwarding_enabled`] and
    /// [`OlmMachine::are_room_key_requests_enabled`].
    #[cfg(feature = "automatic-room-key-forwarding")]
    pub fn set_room_key_requests_enabled(&self, enable: bool) {
        self.inner.key_request_machine.set_room_key_requests_enabled(enable)
    }

    /// Query whether we should send outgoing `m.room_key_request`s on
    /// decryption failure.
    ///
    /// See also [`OlmMachine::set_room_key_requests_enabled`].
    pub fn are_room_key_requests_enabled(&self) -> bool {
        self.inner.key_request_machine.are_room_key_requests_enabled()
    }

    /// Enable or disable room key forwarding.
    ///
    /// If room key forwarding is enabled, we will automatically reply to
    /// incoming `m.room_key_request` messages from verified devices by
    /// forwarding the requested key (if we have it).
    ///
    /// See also [`OlmMachine::set_room_key_requests_enabled`] and
    /// [`OlmMachine::is_room_key_forwarding_enabled`].
    #[cfg(feature = "automatic-room-key-forwarding")]
    pub fn set_room_key_forwarding_enabled(&self, enable: bool) {
        self.inner.key_request_machine.set_room_key_forwarding_enabled(enable)
    }

    /// Is room key forwarding enabled?
    ///
    /// See also [`OlmMachine::set_room_key_forwarding_enabled`].
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

        {
            let store_cache = self.inner.store.cache().await?;
            let account = store_cache.account().await?;
            if let Some(r) = self.keys_for_upload(&account).await.map(|r| OutgoingRequest {
                request_id: TransactionId::new(),
                request: Arc::new(r.into()),
            }) {
                requests.push(r);
            }
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
    /// Note that this request won't be awaited by other calls waiting for a
    /// user's or device's keys, since this is an out-of-band query.
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
    ///   needed to couple the response with the now sent out request.
    ///
    /// * `response` - The response that was received from the server after the
    ///   outgoing request was sent out.
    pub async fn mark_request_as_sent<'a>(
        &self,
        request_id: &TransactionId,
        response: impl Into<IncomingResponse<'a>>,
    ) -> OlmResult<()> {
        match response.into() {
            IncomingResponse::KeysUpload(response) => {
                Box::pin(self.receive_keys_upload_response(response)).await?;
            }
            IncomingResponse::KeysQuery(response) => {
                Box::pin(self.receive_keys_query_response(request_id, response)).await?;
            }
            IncomingResponse::KeysClaim(response) => {
                Box::pin(
                    self.inner.session_manager.receive_keys_claim_response(request_id, response),
                )
                .await?;
            }
            IncomingResponse::ToDevice(_) => {
                Box::pin(self.mark_to_device_request_as_sent(request_id)).await?;
            }
            IncomingResponse::SigningKeysUpload(_) => {
                Box::pin(self.receive_cross_signing_upload_response()).await?;
            }
            IncomingResponse::SignatureUpload(_) => {
                self.inner.verification_machine.mark_request_as_sent(request_id);
            }
            IncomingResponse::RoomMessage(_) => {
                self.inner.verification_machine.mark_request_as_sent(request_id);
            }
            IncomingResponse::KeysBackup(_) => {
                Box::pin(self.inner.backup_machine.mark_request_as_sent(request_id)).await?;
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
    /// **Warning**: if called with `reset`, this will delete any existing cross
    /// signing keys that might exist on the server and thus will reset the
    /// trust between all the devices.
    ///
    /// # Returns
    ///
    /// A triple of requests which should be sent out to the server, in the
    /// order they appear in the return tuple.
    ///
    /// The first request's response, if present, should be passed back to the
    /// state machine using [`mark_request_as_sent`].
    ///
    /// These requests may require user interactive auth.
    ///
    /// [`mark_request_as_sent`]: #method.mark_request_as_sent
    pub async fn bootstrap_cross_signing(
        &self,
        reset: bool,
    ) -> StoreResult<CrossSigningBootstrapRequests> {
        let mut identity = self.inner.user_identity.lock().await;

        let (upload_signing_keys_req, upload_signatures_req) = if reset || identity.is_empty().await
        {
            info!("Creating new cross signing identity");

            let (new_identity, upload_signing_keys_req, upload_signatures_req) = {
                let cache = self.inner.store.cache().await?;
                let account = cache.account().await?;
                account.bootstrap_cross_signing().await
            };

            *identity = new_identity;

            let public = identity.to_public_identity().await.expect(
                "Couldn't create a public version of the identity from a new private identity",
            );

            self.store()
                .save_changes(Changes {
                    identities: IdentityChanges { new: vec![public.into()], ..Default::default() },
                    private_identity: Some(identity.clone()),
                    ..Default::default()
                })
                .await?;

            (upload_signing_keys_req, upload_signatures_req)
        } else {
            info!("Trying to upload the existing cross signing identity");
            let upload_signing_keys_req = identity.as_upload_request().await;

            // TODO remove this expect.
            let upload_signatures_req = identity
                .sign_account(self.inner.store.static_account())
                .await
                .expect("Can't sign device keys");

            (upload_signing_keys_req, upload_signatures_req)
        };

        // `upload_device_keys()` will attempt to sign the device keys using this
        // `identity`, it will attempt to acquire the lock, so we need to drop
        // here to avoid a deadlock.
        drop(identity);

        // If there are any *device* keys to upload (i.e. the account isn't shared),
        // upload them before we upload the signatures, since the signatures may
        // reference keys to be uploaded.
        let upload_keys_req =
            self.upload_device_keys().await?.map(|(_, request)| OutgoingRequest::from(request));

        Ok(CrossSigningBootstrapRequests {
            upload_signing_keys_req,
            upload_keys_req,
            upload_signatures_req,
        })
    }

    /// Upload the device keys for this [`OlmMachine`].
    ///
    /// **Warning**: Do not use this method if
    /// [`OlmMachine::outgoing_requests()`] is already in use. This method
    /// is intended for explicitly uploading the device keys before starting
    /// a sync and before using [`OlmMachine::outgoing_requests()`].
    ///
    /// # Returns
    ///
    /// A tuple containing a transaction ID and a request if the device keys
    /// need to be uploaded. Otherwise, returns `None`.
    pub async fn upload_device_keys(
        &self,
    ) -> StoreResult<Option<(OwnedTransactionId, UploadKeysRequest)>> {
        let cache = self.store().cache().await?;
        let account = cache.account().await?;

        Ok(self.keys_for_upload(&account).await.map(|request| (TransactionId::new(), request)))
    }

    /// Receive a successful `/keys/upload` response.
    ///
    /// # Arguments
    ///
    /// * `response` - The response of the `/keys/upload` request that the
    ///   client performed.
    async fn receive_keys_upload_response(&self, response: &UploadKeysResponse) -> OlmResult<()> {
        self.inner
            .store
            .with_transaction(|mut tr| async {
                let account = tr.account().await?;
                account.receive_keys_upload_response(response)?;
                Ok((tr, ()))
            })
            .await
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
    #[instrument(skip_all)]
    pub async fn get_missing_sessions(
        &self,
        users: impl Iterator<Item = &UserId>,
    ) -> StoreResult<Option<(OwnedTransactionId, KeysClaimRequest)>> {
        self.inner.session_manager.get_missing_sessions(users).await
    }

    /// Receive a successful `/keys/query` response.
    ///
    /// Returns a list of newly discovered devices and devices that changed.
    ///
    /// # Arguments
    ///
    /// * `response` - The response of the `/keys/query` request that the client
    ///   performed.
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
    async fn keys_for_upload(&self, account: &Account) -> Option<UploadKeysRequest> {
        let (mut device_keys, one_time_keys, fallback_keys) = account.keys_for_upload();

        // When uploading the device keys, if all private cross-signing keys are
        // available locally, sign the device using these cross-signing keys.
        // This will mark the device as verified if the user identity (i.e., the
        // cross-signing keys) is also marked as verified.
        //
        // This approach eliminates the need to upload signatures in a separate request,
        // ensuring that other users/devices will never encounter this device
        // without a signature from their user identity. Consequently, they will
        // never see the device as unverified.
        if let Some(device_keys) = &mut device_keys {
            let private_identity = self.store().private_identity();
            let guard = private_identity.lock().await;

            if guard.status().await.is_complete() {
                guard.sign_device_keys(device_keys).await.expect(
                    "We should be able to sign our device keys since we confirmed that we \
                     have a complete set of private cross-signing keys",
                );
            }
        }

        if device_keys.is_none() && one_time_keys.is_empty() && fallback_keys.is_empty() {
            None
        } else {
            let device_keys = device_keys.map(|d| d.to_raw());

            Some(assign!(UploadKeysRequest::new(), {
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
        transaction: &mut StoreTransaction,
        event: &EncryptedToDeviceEvent,
        changes: &mut Changes,
    ) -> OlmResult<OlmDecryptionInfo> {
        let mut decrypted =
            transaction.account().await?.decrypt_to_device_event(&self.inner.store, event).await?;

        // Handle the decrypted event, e.g. fetch out Megolm sessions out of
        // the event.
        self.handle_decrypted_to_device_event(transaction.cache(), &mut decrypted, changes).await?;

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
        let sender_data =
            SenderDataFinder::find_using_event(self.store(), sender_key, event).await?;

        let session = InboundGroupSession::new(
            sender_key,
            event.keys.ed25519,
            &content.room_id,
            &content.session_key,
            sender_data,
            event.content.algorithm(),
            None,
        );

        match session {
            Ok(session) => {
                Span::current().record("session_id", session.session_id());

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
                Span::current().record("session_id", &content.session_id);
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

    fn add_withheld_info(&self, changes: &mut Changes, event: &RoomKeyWithheldEvent) {
        debug!(?event.content, "Processing `m.room_key.withheld` event");

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
    pub(crate) async fn create_outbound_group_session_with_defaults_test_helper(
        &self,
        room_id: &RoomId,
    ) -> OlmResult<()> {
        use crate::olm::SenderData;

        let (_, session) = self
            .inner
            .group_session_manager
            .create_outbound_group_session(
                room_id,
                EncryptionSettings::default(),
                SenderData::unknown(),
            )
            .await?;

        self.store().save_inbound_group_sessions(&[session]).await?;

        Ok(())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) async fn create_inbound_session_test_helper(
        &self,
        room_id: &RoomId,
    ) -> OlmResult<InboundGroupSession> {
        use crate::olm::SenderData;

        let (_, session) = self
            .inner
            .group_session_manager
            .create_outbound_group_session(
                room_id,
                EncryptionSettings::default(),
                SenderData::unknown(),
            )
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
    ///   encrypted.
    ///
    /// * `content` - The plaintext content of the message that should be
    ///   encrypted.
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
        let content = Raw::new(&content)?.cast();
        self.encrypt_room_event_raw(room_id, &event_type, &content).await
    }

    /// Encrypt a raw JSON content for the given room.
    ///
    /// This method is equivalent to the [`OlmMachine::encrypt_room_event()`]
    /// method but operates on an arbitrary JSON value instead of strongly-typed
    /// event content struct.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which the message should be
    ///   encrypted.
    ///
    /// * `content` - The plaintext content of the message that should be
    ///   encrypted as a raw JSON value.
    ///
    /// * `event_type` - The plaintext type of the event.
    ///
    /// # Panics
    ///
    /// Panics if a group session for the given room wasn't shared beforehand.
    pub async fn encrypt_room_event_raw(
        &self,
        room_id: &RoomId,
        event_type: &str,
        content: &Raw<AnyMessageLikeEventContent>,
    ) -> MegolmResult<Raw<RoomEncryptedEventContent>> {
        self.inner.group_session_manager.encrypt(room_id, event_type, content).await
    }

    /// Forces the currently active room key, which is used to encrypt messages,
    /// to be rotated.
    ///
    /// A new room key will be crated and shared with all the room members the
    /// next time a message will be sent. You don't have to call this method,
    /// room keys will be rotated automatically when necessary. This method is
    /// still useful for debugging purposes.
    ///
    /// Returns true if a session was invalidated, false if there was no session
    /// to invalidate.
    pub async fn discard_room_key(&self, room_id: &RoomId) -> StoreResult<bool> {
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
    /// and who are they shared with.
    ///
    /// # Returns
    ///
    /// List of the to-device requests that need to be sent out to the server
    /// and the responses need to be passed back to the state machine with
    /// [`mark_request_as_sent`], using the to-device `txn_id` as `request_id`.
    ///
    /// [`mark_request_as_sent`]: #method.mark_request_as_sent
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
        cache: &StoreCache,
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
                    .receive_secret_event(cache, decrypted.result.sender_key, e, changes)
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

    async fn handle_to_device_event(&self, changes: &mut Changes, event: &ToDeviceEvents) {
        use crate::types::events::ToDeviceEvents::*;

        match event {
            RoomKeyRequest(e) => self.inner.key_request_machine.receive_incoming_key_request(e),
            SecretRequest(e) => self.inner.key_request_machine.receive_incoming_secret_request(e),
            RoomKeyWithheld(e) => self.add_withheld_info(changes, e),
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
            Span::current().record("sender", event.sender);
            Span::current().record("event_type", event.event_type);
            Span::current().record("message_id", event.content.message_id);
        }
    }

    #[instrument(skip_all, fields(sender, event_type, message_id))]
    async fn receive_to_device_event(
        &self,
        transaction: &mut StoreTransaction,
        changes: &mut Changes,
        mut raw_event: Raw<AnyToDeviceEvent>,
    ) -> Raw<AnyToDeviceEvent> {
        Self::record_message_id(&raw_event);

        let event: ToDeviceEvents = match raw_event.deserialize_as() {
            Ok(e) => e,
            Err(e) => {
                // Skip invalid events.
                warn!("Received an invalid to-device event: {e}");

                return raw_event;
            }
        };

        debug!("Received a to-device event");

        match event {
            ToDeviceEvents::RoomEncrypted(e) => {
                let decrypted = match self.decrypt_to_device_event(transaction, &e, changes).await {
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

                        return raw_event;
                    }
                };

                // New sessions modify the account so we need to save that
                // one as well.
                match decrypted.session {
                    SessionType::New(s) | SessionType::Existing(s) => {
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

        raw_event
    }

    /// Handle a to-device and one-time key counts from a sync response.
    ///
    /// This will decrypt and handle to-device events returning the decrypted
    /// versions of them.
    ///
    /// To decrypt an event from the room timeline, call [`decrypt_room_event`].
    ///
    /// # Arguments
    ///
    /// * `sync_changes` - an [`EncryptionSyncChanges`] value, constructed from
    ///   a sync response.
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
        let mut store_transaction = self.inner.store.transaction().await;

        let (events, changes) =
            self.preprocess_sync_changes(&mut store_transaction, sync_changes).await?;

        // Technically save_changes also does the same work, so if it's slow we could
        // refactor this to do it only once.
        let room_key_updates: Vec<_> =
            changes.inbound_group_sessions.iter().map(RoomKeyInfo::from).collect();

        self.store().save_changes(changes).await?;
        store_transaction.commit().await?;

        Ok((events, room_key_updates))
    }

    pub(crate) async fn preprocess_sync_changes(
        &self,
        transaction: &mut StoreTransaction,
        sync_changes: EncryptionSyncChanges<'_>,
    ) -> OlmResult<(Vec<Raw<AnyToDeviceEvent>>, Changes)> {
        // Remove verification objects that have expired or are done.
        let mut events = self.inner.verification_machine.garbage_collect();

        // The account is automatically saved by the store transaction created by the
        // caller.
        let mut changes = Default::default();

        {
            let account = transaction.account().await?;
            account.update_key_counts(
                sync_changes.one_time_keys_counts,
                sync_changes.unused_fallback_keys,
            )
        }

        if let Err(e) = self
            .inner
            .identity_manager
            .receive_device_changes(
                transaction.cache(),
                sync_changes.changed_devices.changed.iter().map(|u| u.as_ref()),
            )
            .await
        {
            error!(error = ?e, "Error marking a tracked user as changed");
        }

        for raw_event in sync_changes.to_device_events {
            let raw_event =
                Box::pin(self.receive_to_device_event(transaction, &mut changes, raw_event)).await;
            events.push(raw_event);
        }

        let changed_sessions = self
            .inner
            .key_request_machine
            .collect_incoming_key_requests(transaction.cache())
            .await?;

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

    /// Request missing local secrets from our devices (cross signing private
    /// keys, megolm backup). This will ask the sdk to create outgoing
    /// request to get the missing secrets.
    ///
    /// The requests will be processed as soon as `outgoing_requests()` is
    /// called to process them.
    ///
    /// # Returns
    ///
    /// A bool result saying if actual secrets were missing and have been
    /// requested
    ///
    /// # Examples
    //
    /// ```
    /// # async {
    /// # use matrix_sdk_crypto::OlmMachine;
    /// # let machine: OlmMachine = unimplemented!();
    /// if machine.query_missing_secrets_from_other_sessions().await.unwrap() {
    ///     let to_send = machine.outgoing_requests().await.unwrap();
    ///     // send the to device requests
    /// };
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn query_missing_secrets_from_other_sessions(&self) -> StoreResult<bool> {
        let identity = self.inner.user_identity.lock().await;
        let mut secrets = identity.get_missing_secrets().await;

        if self.store().load_backup_keys().await?.decryption_key.is_none() {
            secrets.push(SecretName::RecoveryKey);
        }

        if secrets.is_empty() {
            debug!("No missing requests to query");
            return Ok(false);
        }

        let secret_requests = GossipMachine::request_missing_secrets(self.user_id(), secrets);

        // Check if there are already in-flight requests for these secrets?
        let unsent_request = self.store().get_unsent_secret_requests().await?;
        let not_yet_requested = secret_requests
            .into_iter()
            .filter(|request| !unsent_request.iter().any(|unsent| unsent.info == request.info))
            .collect_vec();

        if not_yet_requested.is_empty() {
            debug!("The missing secrets have already been requested");
            Ok(false)
        } else {
            debug!("Requesting missing secrets");

            let changes = Changes { key_requests: not_yet_requested, ..Default::default() };

            self.store().save_changes(changes).await?;
            Ok(true)
        }
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

    async fn get_megolm_encryption_info(
        &self,
        room_id: &RoomId,
        event: &EncryptedEvent,
        content: &SupportedEventEncryptionSchemes<'_>,
    ) -> MegolmResult<EncryptionInfo> {
        let session =
            self.get_inbound_group_session_or_error(room_id, content.session_id()).await?;
        self.get_encryption_info(&session, &event.sender).await
    }

    async fn decrypt_megolm_events(
        &self,
        room_id: &RoomId,
        event: &EncryptedEvent,
        content: &SupportedEventEncryptionSchemes<'_>,
    ) -> MegolmResult<(JsonObject, EncryptionInfo)> {
        let session =
            self.get_inbound_group_session_or_error(room_id, content.session_id()).await?;

        // This function is only ever called by decrypt_room_event, so
        // room_id, sender, algorithm and session_id are recorded already
        //
        // While we already record the sender key in some cases from the event, the
        // sender key in the event is deprecated, so let's record it now.
        Span::current().record("sender_key", debug(session.sender_key()));

        let result = session.decrypt(event).await;
        match result {
            Ok((decrypted_event, _)) => {
                let encryption_info = self.get_encryption_info(&session, &event.sender).await?;
                Ok((decrypted_event, encryption_info))
            }
            Err(error) => Err(
                if let MegolmError::Decryption(DecryptionError::UnknownMessageIndex(_, _)) = error {
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
    }

    /// Attempt to retrieve an inbound group session from the store.
    ///
    /// If the session is not found, checks for withheld reports, and returns a
    /// [`MegolmError::MissingRoomKey`] error.
    async fn get_inbound_group_session_or_error(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> MegolmResult<InboundGroupSession> {
        match self.store().get_inbound_group_session(room_id, session_id).await? {
            Some(session) => Ok(session),
            None => {
                let withheld_code = self
                    .inner
                    .store
                    .get_withheld_info(room_id, session_id)
                    .await?
                    .map(|e| e.content.withheld_code());
                Err(MegolmError::MissingRoomKey(withheld_code))
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
        &self,
        event: &Raw<EncryptedEvent>,
        room_id: &RoomId,
    ) -> MegolmResult<TimelineEvent> {
        self.decrypt_room_event_inner(event, room_id, true).await
    }

    #[instrument(name = "decrypt_room_event", skip_all, fields(?room_id, event_id, origin_server_ts, sender, algorithm, session_id, sender_key))]
    async fn decrypt_room_event_inner(
        &self,
        event: &Raw<EncryptedEvent>,
        room_id: &RoomId,
        decrypt_unsigned: bool,
    ) -> MegolmResult<TimelineEvent> {
        let event = event.deserialize()?;

        Span::current()
            .record("sender", debug(&event.sender))
            .record("event_id", debug(&event.event_id))
            .record(
                "origin_server_ts",
                timestamp_to_iso8601(event.origin_server_ts)
                    .unwrap_or_else(|| "<out of range>".to_owned()),
            )
            .record("algorithm", debug(event.content.algorithm()));

        let content: SupportedEventEncryptionSchemes<'_> = match &event.content.scheme {
            RoomEventEncryptionScheme::MegolmV1AesSha2(c) => {
                Span::current().record("sender_key", debug(c.sender_key));
                c.into()
            }
            #[cfg(feature = "experimental-algorithms")]
            RoomEventEncryptionScheme::MegolmV2AesSha2(c) => c.into(),
            RoomEventEncryptionScheme::Unknown(_) => {
                warn!("Received an encrypted room event with an unsupported algorithm");
                return Err(EventError::UnsupportedAlgorithm.into());
            }
        };

        Span::current().record("session_id", content.session_id());
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

        let (mut decrypted_event, encryption_info) = result?;

        let mut unsigned_encryption_info = None;
        if decrypt_unsigned {
            // Try to decrypt encrypted unsigned events.
            unsigned_encryption_info =
                self.decrypt_unsigned_events(&mut decrypted_event, room_id).await;
        }

        let event = serde_json::from_value::<Raw<AnyTimelineEvent>>(decrypted_event.into())?;

        Ok(TimelineEvent {
            event,
            encryption_info: Some(encryption_info),
            push_actions: None,
            unsigned_encryption_info,
        })
    }

    /// Try to decrypt the events bundled in the `unsigned` object of the given
    /// event.
    ///
    /// # Arguments
    ///
    /// * `main_event` - The event that may contain bundled encrypted events in
    ///   its `unsigned` object.
    ///
    /// * `room_id` - The ID of the room where the event was sent to.
    async fn decrypt_unsigned_events(
        &self,
        main_event: &mut JsonObject,
        room_id: &RoomId,
    ) -> Option<BTreeMap<UnsignedEventLocation, UnsignedDecryptionResult>> {
        let unsigned = main_event.get_mut("unsigned")?.as_object_mut()?;
        let mut unsigned_encryption_info: Option<
            BTreeMap<UnsignedEventLocation, UnsignedDecryptionResult>,
        > = None;

        // Search for an encrypted event in `m.replace`, an edit.
        let location = UnsignedEventLocation::RelationsReplace;
        let replace = location.find_mut(unsigned);
        if let Some(decryption_result) = self.decrypt_unsigned_event(replace, room_id).await {
            unsigned_encryption_info
                .get_or_insert_with(Default::default)
                .insert(location, decryption_result);
        }

        // Search for an encrypted event in `latest_event` in `m.thread`, the
        // latest event of a thread.
        let location = UnsignedEventLocation::RelationsThreadLatestEvent;
        let thread_latest_event = location.find_mut(unsigned);
        if let Some(decryption_result) =
            self.decrypt_unsigned_event(thread_latest_event, room_id).await
        {
            unsigned_encryption_info
                .get_or_insert_with(Default::default)
                .insert(location, decryption_result);
        }

        unsigned_encryption_info
    }

    /// Try to decrypt the given bundled event.
    ///
    /// # Arguments
    ///
    /// * `event` - The bundled event that may be encrypted
    ///
    /// * `room_id` - The ID of the room where the event was sent to.
    fn decrypt_unsigned_event<'a>(
        &'a self,
        event: Option<&'a mut Value>,
        room_id: &'a RoomId,
    ) -> BoxFuture<'a, Option<UnsignedDecryptionResult>> {
        Box::pin(async move {
            let event = event?;

            let is_encrypted = event
                .get("type")
                .and_then(|type_| type_.as_str())
                .is_some_and(|s| s == "m.room.encrypted");
            if !is_encrypted {
                return None;
            }

            let raw_event = serde_json::from_value(event.clone()).ok()?;
            match self.decrypt_room_event_inner(&raw_event, room_id, false).await {
                Ok(decrypted_event) => {
                    // Replace the encrypted event.
                    *event = serde_json::to_value(decrypted_event.event).ok()?;
                    Some(UnsignedDecryptionResult::Decrypted(decrypted_event.encryption_info?))
                }
                Err(_) => {
                    let session_id =
                        raw_event.deserialize().ok().and_then(|ev| match ev.content.scheme {
                            RoomEventEncryptionScheme::MegolmV1AesSha2(s) => Some(s.session_id),
                            #[cfg(feature = "experimental-algorithms")]
                            RoomEventEncryptionScheme::MegolmV2AesSha2(s) => Some(s.session_id),
                            RoomEventEncryptionScheme::Unknown(_) => None,
                        });

                    Some(UnsignedDecryptionResult::UnableToDecrypt(UnableToDecryptInfo {
                        session_id,
                    }))
                }
            }
        })
    }

    /// Check if we have the room key for the given event in the store.
    ///
    /// # Arguments
    ///
    /// * `event` - The event to get information for.
    /// * `room_id` - The ID of the room where the event was sent to.
    pub async fn is_room_key_available(
        &self,
        event: &Raw<EncryptedEvent>,
        room_id: &RoomId,
    ) -> Result<bool, CryptoStoreError> {
        let event = event.deserialize()?;

        let (session_id, message_index) = match &event.content.scheme {
            RoomEventEncryptionScheme::MegolmV1AesSha2(c) => {
                (&c.session_id, c.ciphertext.message_index())
            }
            #[cfg(feature = "experimental-algorithms")]
            RoomEventEncryptionScheme::MegolmV2AesSha2(c) => {
                (&c.session_id, c.ciphertext.message_index())
            }
            RoomEventEncryptionScheme::Unknown(_) => {
                // We don't support this encryption algorithm, so clearly don't have its key.
                return Ok(false);
            }
        };

        // Check that we have the session in the store, and that its first known index
        // predates the index of our message.
        Ok(self
            .store()
            .get_inbound_group_session(room_id, session_id)
            .await?
            .filter(|s| s.first_known_index() <= message_index)
            .is_some())
    }

    /// Get encryption info for a decrypted timeline event.
    ///
    /// This recalculates the [`EncryptionInfo`] data that is returned by
    /// [`OlmMachine::decrypt_room_event`], based on the current
    /// verification status of the sender, etc.
    ///
    /// Returns an error for an unencrypted event.
    ///
    /// # Arguments
    ///
    /// * `event` - The event to get information for.
    /// * `room_id` - The ID of the room where the event was sent to.
    pub async fn get_room_event_encryption_info(
        &self,
        event: &Raw<EncryptedEvent>,
        room_id: &RoomId,
    ) -> MegolmResult<EncryptionInfo> {
        let event = event.deserialize()?;

        let content: SupportedEventEncryptionSchemes<'_> = match &event.content.scheme {
            RoomEventEncryptionScheme::MegolmV1AesSha2(c) => c.into(),
            #[cfg(feature = "experimental-algorithms")]
            RoomEventEncryptionScheme::MegolmV2AesSha2(c) => c.into(),
            RoomEventEncryptionScheme::Unknown(_) => {
                return Err(EventError::UnsupportedAlgorithm.into());
            }
        };

        self.get_megolm_encryption_info(room_id, &event, &content).await
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

    async fn wait_if_user_pending(
        &self,
        user_id: &UserId,
        timeout: Option<Duration>,
    ) -> StoreResult<()> {
        if let Some(timeout) = timeout {
            let cache = self.store().cache().await?;
            self.inner
                .identity_manager
                .key_query_manager
                .wait_if_user_key_query_pending(cache, timeout, user_id)
                .await?;
        }
        Ok(())
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
    #[instrument(skip(self))]
    pub async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        timeout: Option<Duration>,
    ) -> StoreResult<Option<Device>> {
        self.wait_if_user_pending(user_id, timeout).await?;
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
    #[instrument(skip(self))]
    pub async fn get_identity(
        &self,
        user_id: &UserId,
        timeout: Option<Duration>,
    ) -> StoreResult<Option<UserIdentities>> {
        self.wait_if_user_pending(user_id, timeout).await?;
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
    #[instrument(skip(self))]
    pub async fn get_user_devices(
        &self,
        user_id: &UserId,
        timeout: Option<Duration>,
    ) -> StoreResult<UserDevices> {
        self.wait_if_user_pending(user_id, timeout).await?;
        self.store().get_user_devices(user_id).await
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
    pub async fn sign(&self, message: &str) -> Result<Signatures, CryptoStoreError> {
        let mut signatures = Signatures::new();

        {
            let cache = self.inner.store.cache().await?;
            let account = cache.account().await?;
            let key_id = account.signing_key_id();
            let signature = account.sign(message);
            signatures.add_signature(self.user_id().to_owned(), key_id, signature);
        }

        match self.sign_with_master_key(message).await {
            Ok((key_id, signature)) => {
                signatures.add_signature(self.user_id().to_owned(), key_id, signature);
            }
            Err(e) => {
                warn!(error = ?e, "Couldn't sign the message using the cross signing master key")
            }
        }

        Ok(signatures)
    }

    /// Get a reference to the backup related state machine.
    ///
    /// This state machine can be used to incrementally backup all room keys to
    /// the server.
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
                u64::from_le_bytes(val.try_into().map_err(|_| {
                    CryptoStoreError::InvalidLockGeneration("invalid format".to_owned())
                })?)
                .wrapping_add(1)
            }
            None => 0,
        };

        tracing::debug!("Initialising crypto store generation at {}", gen);

        self.inner
            .store
            .set_custom_value(Self::CURRENT_GENERATION_STORE_KEY, gen.to_le_bytes().to_vec())
            .await?;

        *gen_guard = Some(gen);

        Ok(())
    }

    /// If needs be, update the local and on-disk crypto store generation.
    ///
    /// ## Requirements
    ///
    /// - This assumes that `initialize_crypto_store_generation` has been called
    ///   beforehand.
    /// - This requires that the crypto store lock has been acquired.
    ///
    /// # Arguments
    ///
    /// * `generation` - The in-memory generation counter (or rather, the
    ///   `Mutex` wrapping it). This defines the "expected" generation on entry,
    ///   and, if we determine an update is needed, is updated to hold the "new"
    ///   generation.
    ///
    /// # Returns
    ///
    /// A tuple containing:
    ///
    /// * A `bool`, set to `true` if another process has updated the generation
    ///   number in the `Store` since our expected value, and as such we've
    ///   incremented and updated it in the database. Otherwise, `false`.
    ///
    /// * The (possibly updated) generation counter.
    pub async fn maintain_crypto_store_generation<'a>(
        &'a self,
        generation: &Mutex<Option<u64>>,
    ) -> StoreResult<(bool, u64)> {
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
            .ok_or_else(|| {
                CryptoStoreError::InvalidLockGeneration("counter missing in store".to_owned())
            })?;

        let actual_gen =
            u64::from_le_bytes(actual_gen.try_into().map_err(|_| {
                CryptoStoreError::InvalidLockGeneration("invalid format".to_owned())
            })?);

        let new_gen = match gen_guard.as_ref() {
            Some(expected_gen) => {
                if actual_gen == *expected_gen {
                    return Ok((false, actual_gen));
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
            new_gen
        );

        // Update known value.
        *gen_guard = Some(new_gen);

        // Update value in database.
        self.inner
            .store
            .set_custom_value(Self::CURRENT_GENERATION_STORE_KEY, new_gen.to_le_bytes().to_vec())
            .await?;

        Ok((true, new_gen))
    }

    /// Manage dehydrated devices.
    pub fn dehydrated_devices(&self) -> DehydratedDevices {
        DehydratedDevices { inner: self.to_owned() }
    }

    /// Get the stored encryption settings for the given room, such as the
    /// encryption algorithm or whether to encrypt only for trusted devices.
    ///
    /// These settings can be modified via [`OlmMachine::set_room_settings`].
    pub async fn room_settings(&self, room_id: &RoomId) -> StoreResult<Option<RoomSettings>> {
        // There's not much to do here: it's just exposed for symmetry with
        // `set_room_settings`.
        self.inner.store.get_room_settings(room_id).await
    }

    /// Store encryption settings for the given room.
    ///
    /// This method checks if the new settings are "safe" -- ie, that they do
    /// not represent a downgrade in encryption security from any previous
    /// settings. Attempts to downgrade security will result in a
    /// [`SetRoomSettingsError::EncryptionDowngrade`].
    ///
    /// If the settings are valid, they will be persisted to the crypto store.
    /// These settings are not used directly by this library, but the saved
    /// settings can be retrieved via [`OlmMachine::room_settings`].
    pub async fn set_room_settings(
        &self,
        room_id: &RoomId,
        new_settings: &RoomSettings,
    ) -> Result<(), SetRoomSettingsError> {
        let store = &self.inner.store;

        // We want to make sure that we do not race against a second concurrent call to
        // `set_room_settings`. By way of an easy way to do so, we start a
        // StoreTransaction. There's no need to commit() it: we're just using it as a
        // lock guard.
        let _store_transaction = store.transaction().await;

        let old_settings = store.get_room_settings(room_id).await?;

        // We want to make sure that the change to the room settings does not represent
        // a downgrade in security. The [E2EE implementation guide] recommends:
        //
        //  > This flag should **not** be cleared if a later `m.room.encryption` event
        //  > changes the configuration.
        //
        // (However, it doesn't really address how to handle changes to the rotation
        // parameters, etc.) For now at least, we are very conservative here:
        // any new settings are rejected if they differ from the existing settings.
        // merit improvement (cf https://github.com/element-hq/element-meta/issues/69).
        //
        // [E2EE implementation guide]: https://matrix.org/docs/matrix-concepts/end-to-end-encryption/#handling-an-m-room-encryption-state-event
        if let Some(old_settings) = old_settings {
            if old_settings != *new_settings {
                return Err(SetRoomSettingsError::EncryptionDowngrade);
            } else {
                // nothing to do here
                return Ok(());
            }
        }

        // Make sure that the new settings are valid
        match new_settings.algorithm {
            EventEncryptionAlgorithm::MegolmV1AesSha2 => (),

            #[cfg(feature = "experimental-algorithms")]
            EventEncryptionAlgorithm::MegolmV2AesSha2 => (),

            _ => {
                warn!(
                    ?room_id,
                    "Rejecting invalid encryption algorithm {}", new_settings.algorithm
                );
                return Err(SetRoomSettingsError::InvalidSettings);
            }
        }

        // The new settings are acceptable, so let's save them.
        store
            .save_changes(Changes {
                room_settings: HashMap::from([(room_id.to_owned(), new_settings.clone())]),
                ..Default::default()
            })
            .await?;

        Ok(())
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
    pub async fn uploaded_key_count(&self) -> Result<u64, CryptoStoreError> {
        let cache = self.inner.store.cache().await?;
        let account = cache.account().await?;
        Ok(account.uploaded_key_count())
    }

    /// Clear any in-memory caches because they may be out of sync with the
    /// underlying data store.
    pub async fn clear_crypto_cache(&self) {
        let crypto_store = self.store().crypto_store();
        crypto_store.as_ref().clear_caches().await;
    }
}

/// A set of requests to be executed when bootstrapping cross-signing using
/// [`OlmMachine::bootstrap_cross_signing`].
#[derive(Debug)]
pub struct CrossSigningBootstrapRequests {
    /// An optional request to upload a device key.
    ///
    /// Should be sent first, if present.
    ///
    /// If present, its result must be processed back with
    /// `OlmMachine::mark_request_as_sent`.
    pub upload_keys_req: Option<OutgoingRequest>,

    /// Request to upload the cross-signing keys.
    ///
    /// Should be sent second.
    pub upload_signing_keys_req: UploadSigningKeysRequest,

    /// Request to upload key signatures, including those for the cross-signing
    /// keys, and maybe some for the optional uploaded key too.
    ///
    /// Should be sent last.
    pub upload_signatures_req: UploadSignaturesRequest,
}

/// Data contained from a sync response and that needs to be processed by the
/// OlmMachine.
#[derive(Debug)]
pub struct EncryptionSyncChanges<'a> {
    /// The list of to-device events received in the sync.
    pub to_device_events: Vec<Raw<AnyToDeviceEvent>>,
    /// The mapping of changed and left devices, per user, as returned in the
    /// sync response.
    pub changed_devices: &'a DeviceLists,
    /// The number of one time keys, as returned in the sync response.
    pub one_time_keys_counts: &'a BTreeMap<DeviceKeyAlgorithm, UInt>,
    /// An optional list of fallback keys.
    pub unused_fallback_keys: Option<&'a [DeviceKeyAlgorithm]>,
    /// A next-batch token obtained from a to-device sync query.
    pub next_batch_token: Option<String>,
}

#[cfg(any(feature = "testing", test))]
#[allow(dead_code)]
pub(crate) mod testing {
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

    use assert_matches2::{assert_let, assert_matches};
    use futures_util::{pin_mut, FutureExt, StreamExt};
    use itertools::Itertools;
    use matrix_sdk_common::deserialized_responses::{
        DeviceLinkProblem, ShieldState, UnableToDecryptInfo, UnsignedDecryptionResult,
        UnsignedEventLocation, VerificationLevel, VerificationState,
    };
    use matrix_sdk_test::{async_test, message_like_event_content, test_json};
    use ruma::{
        api::{
            client::{
                keys::{
                    claim_keys,
                    get_keys::{self, v3::Response as KeyQueryResponse},
                    upload_keys,
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
            room::message::{
                AddMentions, MessageType, Relation, ReplyWithinThread, RoomMessageEventContent,
            },
            AnyMessageLikeEvent, AnyMessageLikeEventContent, AnyTimelineEvent, AnyToDeviceEvent,
            MessageLikeEvent, OriginalMessageLikeEvent,
        },
        room_id,
        serde::Raw,
        to_device::DeviceIdOrAllDevices,
        uint, user_id, DeviceId, DeviceKeyAlgorithm, DeviceKeyId, MilliSecondsSinceUnixEpoch,
        OwnedDeviceKeyId, SecondsSinceUnixEpoch, TransactionId, UserId,
    };
    use serde_json::{json, value::to_raw_value};
    use vodozemac::{
        megolm::{GroupSession, SessionConfig},
        Curve25519PublicKey, Ed25519PublicKey,
    };

    use super::{testing::response_from_file, CrossSigningBootstrapRequests};
    use crate::{
        error::{EventError, SetRoomSettingsError},
        machine::{EncryptionSyncChanges, OlmMachine},
        olm::{
            BackedUpRoomKey, ExportedRoomKey, InboundGroupSession, OutboundGroupSession,
            SenderData, VerifyJson,
        },
        session_manager::CollectStrategy,
        store::{BackupDecryptionKey, Changes, CryptoStore, MemoryStore, RoomSettings},
        types::{
            events::{
                room::encrypted::{EncryptedToDeviceEvent, ToDeviceEncryptedEventContent},
                room_key_withheld::{
                    MegolmV1AesSha2WithheldContent, RoomKeyWithheldContent, WithheldCode,
                },
                ToDeviceEvent,
            },
            CrossSigningKey, DeviceKeys, EventEncryptionAlgorithm, SignedKey, SigningKeys,
        },
        utilities::json_convert,
        verification::tests::{bob_id, outgoing_request_to_event, request_to_event},
        Account, DeviceData, EncryptionSettings, LocalTrust, MegolmError, OlmError,
        OutgoingRequests, ToDeviceRequest, UserIdentities,
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
            .expect("Can't parse the `/keys/upload` response")
    }

    fn keys_query_response() -> get_keys::v3::Response {
        let data = response_from_file(&test_json::KEYS_QUERY);
        get_keys::v3::Response::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
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

    pub(crate) async fn get_prepared_machine_test_helper(
        user_id: &UserId,
        use_fallback_key: bool,
    ) -> (OlmMachine, OneTimeKeys) {
        let machine = OlmMachine::new(user_id, bob_device_id()).await;

        let request = machine
            .store()
            .with_transaction(|mut tr| async {
                let account = tr.account().await.unwrap();
                account.generate_fallback_key_if_needed();
                account.update_uploaded_key_count(0);
                account.generate_one_time_keys_if_needed();
                let request = machine
                    .keys_for_upload(account)
                    .await
                    .expect("Can't prepare initial key upload");
                Ok((tr, request))
            })
            .await
            .unwrap();

        let response = keys_upload_response();
        machine.receive_keys_upload_response(&response).await.unwrap();

        let keys = if use_fallback_key { request.fallback_keys } else { request.one_time_keys };

        (machine, keys)
    }

    async fn get_machine_after_query_test_helper() -> (OlmMachine, OneTimeKeys) {
        let (machine, otk) = get_prepared_machine_test_helper(user_id(), false).await;
        let response = keys_query_response();
        let req_id = TransactionId::new();

        machine.receive_keys_query_response(&req_id, &response).await.unwrap();

        (machine, otk)
    }

    pub async fn get_machine_pair(
        alice: &UserId,
        bob: &UserId,
        use_fallback_key: bool,
    ) -> (OlmMachine, OlmMachine, OneTimeKeys) {
        let (bob, otk) = get_prepared_machine_test_helper(bob, use_fallback_key).await;

        let alice_device = alice_device_id();
        let alice = OlmMachine::new(alice, alice_device).await;

        let alice_device = DeviceData::from_machine_test_helper(&alice).await.unwrap();
        let bob_device = DeviceData::from_machine_test_helper(&bob).await.unwrap();
        alice.store().save_device_data(&[bob_device]).await.unwrap();
        bob.store().save_device_data(&[alice_device]).await.unwrap();

        (alice, bob, otk)
    }

    async fn get_machine_pair_with_session(
        alice: &UserId,
        bob: &UserId,
        use_fallback_key: bool,
    ) -> (OlmMachine, OlmMachine) {
        let (alice, bob, mut one_time_keys) = get_machine_pair(alice, bob, use_fallback_key).await;

        let (device_key_id, one_time_key) = one_time_keys.pop_first().unwrap();

        let one_time_keys = BTreeMap::from([(
            bob.user_id().to_owned(),
            BTreeMap::from([(
                bob.device_id().to_owned(),
                BTreeMap::from([(device_key_id, one_time_key)]),
            )]),
        )]);

        let response = claim_keys::v3::Response::new(one_time_keys);
        alice.inner.session_manager.create_sessions(&response).await.unwrap();

        (alice, bob)
    }

    pub(crate) async fn get_machine_pair_with_setup_sessions_test_helper(
        alice: &UserId,
        bob: &UserId,
        use_fallback_key: bool,
    ) -> (OlmMachine, OlmMachine) {
        let (alice, bob) = get_machine_pair_with_session(alice, bob, use_fallback_key).await;

        let bob_device =
            alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();

        let (session, content) =
            bob_device.encrypt("m.dummy", ToDeviceDummyEventContent::new()).await.unwrap();
        alice.store().save_sessions(&[session]).await.unwrap();

        let event =
            ToDeviceEvent::new(alice.user_id().to_owned(), content.deserialize_as().unwrap());

        let decrypted = bob
            .store()
            .with_transaction(|mut tr| async {
                let res =
                    bob.decrypt_to_device_event(&mut tr, &event, &mut Changes::default()).await?;
                Ok((tr, res))
            })
            .await
            .unwrap();

        bob.store().save_sessions(&[decrypted.session.session()]).await.unwrap();

        (alice, bob)
    }

    #[async_test]
    async fn test_create_olm_machine() {
        let test_start_ts = MilliSecondsSinceUnixEpoch::now();
        let machine = OlmMachine::new(user_id(), alice_device_id()).await;

        let device_creation_time = machine.device_creation_time();
        assert!(device_creation_time <= MilliSecondsSinceUnixEpoch::now());
        assert!(device_creation_time >= test_start_ts);

        let cache = machine.store().cache().await.unwrap();
        let account = cache.account().await.unwrap();
        assert!(!account.shared());

        let own_device = machine
            .get_device(machine.user_id(), machine.device_id(), None)
            .await
            .unwrap()
            .expect("We should always have our own device in the store");

        assert!(own_device.is_locally_trusted(), "Our own device should always be locally trusted");
    }

    #[async_test]
    async fn test_generate_one_time_keys() {
        let machine = OlmMachine::new(user_id(), alice_device_id()).await;

        machine
            .store()
            .with_transaction(|mut tr| async {
                let account = tr.account().await.unwrap();
                assert!(account.generate_one_time_keys_if_needed().is_some());
                Ok((tr, ()))
            })
            .await
            .unwrap();

        let mut response = keys_upload_response();

        machine.receive_keys_upload_response(&response).await.unwrap();

        machine
            .store()
            .with_transaction(|mut tr| async {
                let account = tr.account().await.unwrap();
                assert!(account.generate_one_time_keys_if_needed().is_some());
                Ok((tr, ()))
            })
            .await
            .unwrap();

        response.one_time_key_counts.insert(DeviceKeyAlgorithm::SignedCurve25519, uint!(50));

        machine.receive_keys_upload_response(&response).await.unwrap();

        machine
            .store()
            .with_transaction(|mut tr| async {
                let account = tr.account().await.unwrap();
                assert!(account.generate_one_time_keys_if_needed().is_none());

                Ok((tr, ()))
            })
            .await
            .unwrap();
    }

    #[async_test]
    async fn test_device_key_signing() {
        let machine = OlmMachine::new(user_id(), alice_device_id()).await;

        let (device_keys, identity_keys) = {
            let cache = machine.store().cache().await.unwrap();
            let account = cache.account().await.unwrap();
            let device_keys = account.device_keys();
            let identity_keys = account.identity_keys();
            (device_keys, identity_keys)
        };

        let ed25519_key = identity_keys.ed25519;

        let ret = ed25519_key.verify_json(
            machine.user_id(),
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, machine.device_id()),
            &device_keys,
        );
        ret.unwrap();
    }

    #[async_test]
    async fn test_session_invalidation() {
        let machine = OlmMachine::new(user_id(), alice_device_id()).await;
        let room_id = room_id!("!test:example.org");

        machine.create_outbound_group_session_with_defaults_test_helper(room_id).await.unwrap();
        assert!(machine.inner.group_session_manager.get_outbound_group_session(room_id).is_some());

        machine.discard_room_key(room_id).await.unwrap();

        assert!(machine
            .inner
            .group_session_manager
            .get_outbound_group_session(room_id)
            .unwrap()
            .invalidated());
    }

    #[test]
    fn test_invalid_signature() {
        let account = Account::with_device_id(user_id(), alice_device_id());

        let device_keys = account.device_keys();

        let key = Ed25519PublicKey::from_slice(&[0u8; 32]).unwrap();

        let ret = key.verify_json(
            account.user_id(),
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, account.device_id()),
            &device_keys,
        );
        ret.unwrap_err();
    }

    #[test]
    fn test_one_time_key_signing() {
        let mut account = Account::with_device_id(user_id(), alice_device_id());
        account.update_uploaded_key_count(49);
        account.generate_one_time_keys_if_needed();

        let mut one_time_keys = account.signed_one_time_keys();
        let ed25519_key = account.identity_keys().ed25519;

        let one_time_key: SignedKey = one_time_keys
            .values_mut()
            .next()
            .expect("One time keys should be generated")
            .deserialize_as()
            .unwrap();

        ed25519_key
            .verify_json(
                account.user_id(),
                &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, account.device_id()),
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

        let (ed25519_key, mut request) = {
            let cache = machine.store().cache().await.unwrap();
            let account = cache.account().await.unwrap();
            let ed25519_key = account.identity_keys().ed25519;

            let request =
                machine.keys_for_upload(&account).await.expect("Can't prepare initial key upload");
            (ed25519_key, request)
        };

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

        let response = {
            let cache = machine.store().cache().await.unwrap();
            let account = cache.account().await.unwrap();

            let mut response = keys_upload_response();
            response.one_time_key_counts.insert(
                DeviceKeyAlgorithm::SignedCurve25519,
                account.max_one_time_keys().try_into().unwrap(),
            );

            response
        };

        machine.receive_keys_upload_response(&response).await.unwrap();

        {
            let cache = machine.store().cache().await.unwrap();
            let account = cache.account().await.unwrap();
            let ret = machine.keys_for_upload(&account).await;
            assert!(ret.is_none());
        }
    }

    #[async_test]
    async fn test_keys_query() {
        let (machine, _) = get_prepared_machine_test_helper(user_id(), false).await;
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
        let (machine, _) = get_prepared_machine_test_helper(user_id(), false).await;
        let alice_id = user_id!("@alice:example.org");
        let (_, request) = machine.query_keys_for_users(vec![alice_id]);
        assert!(request.device_keys.contains_key(alice_id));
    }

    #[async_test]
    async fn test_missing_sessions_calculation() {
        let (machine, _) = get_machine_after_query_test_helper().await;

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
        let one_time_keys = BTreeMap::from([(
            user_id.to_owned(),
            BTreeMap::from([(device_id.to_owned(), BTreeMap::from([(key_id, one_time_key)]))]),
        )]);

        let response = claim_keys::v3::Response::new(one_time_keys);
        machine.inner.session_manager.create_sessions(&response).await.unwrap();
    }

    #[async_test]
    async fn test_session_creation() {
        let (alice_machine, bob_machine, mut one_time_keys) =
            get_machine_pair(alice_id(), user_id(), false).await;
        let (key_id, one_time_key) = one_time_keys.pop_first().unwrap();

        create_session(
            &alice_machine,
            bob_machine.user_id(),
            bob_machine.device_id(),
            key_id,
            one_time_key,
        )
        .await;

        let session = alice_machine
            .store()
            .get_sessions(
                &bob_machine.store().static_account().identity_keys().curve25519.to_base64(),
            )
            .await
            .unwrap()
            .unwrap();

        assert!(!session.lock().await.is_empty())
    }

    #[async_test]
    async fn test_getting_most_recent_session() {
        let (alice_machine, bob_machine, mut one_time_keys) =
            get_machine_pair(alice_id(), user_id(), false).await;
        let (key_id, one_time_key) = one_time_keys.pop_first().unwrap();

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
            key_id,
            one_time_key.to_owned(),
        )
        .await;

        for _ in 0..10 {
            let (key_id, one_time_key) = one_time_keys.pop_first().unwrap();

            create_session(
                &alice_machine,
                bob_machine.user_id(),
                bob_machine.device_id(),
                key_id,
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

    async fn olm_encryption_test_helper(use_fallback_key: bool) {
        let (alice, bob) =
            get_machine_pair_with_session(alice_id(), user_id(), use_fallback_key).await;

        let bob_device =
            alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();

        let (_, content) = bob_device
            .encrypt("m.dummy", ToDeviceDummyEventContent::new())
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
            .store()
            .with_transaction(|mut tr| async {
                let res =
                    bob.decrypt_to_device_event(&mut tr, &event, &mut Changes::default()).await?;
                Ok((tr, res))
            })
            .await
            .expect("We should be able to decrypt the event.")
            .result
            .raw_event
            .deserialize()
            .expect("We should be able to deserialize the decrypted event.");

        assert_let!(AnyToDeviceEvent::Dummy(decrypted) = decrypted);
        assert_eq!(&decrypted.sender, alice.user_id());

        // Replaying the event should now result in a decryption failure.
        bob.store()
            .with_transaction(|mut tr| async {
                let res =
                    bob.decrypt_to_device_event(&mut tr, &event, &mut Changes::default()).await?;
                Ok((tr, res))
            })
            .await
            .expect_err(
                "Decrypting a replayed event should not succeed, even if it's a pre-key message",
            );
    }

    #[async_test]
    async fn test_olm_encryption() {
        olm_encryption_test_helper(false).await;
    }

    #[async_test]
    async fn test_olm_encryption_with_fallback_key() {
        olm_encryption_test_helper(true).await;
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
    async fn test_request_missing_secrets() {
        let (alice, _) = get_machine_pair_with_session(alice_id(), bob_id(), false).await;

        let should_query_secrets = alice.query_missing_secrets_from_other_sessions().await.unwrap();

        assert!(should_query_secrets);

        let outgoing_to_device = alice
            .outgoing_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|outgoing| match outgoing.request.as_ref() {
                OutgoingRequests::ToDeviceRequest(request) => {
                    request.event_type.to_string() == "m.secret.request"
                }
                _ => false,
            })
            .collect_vec();

        assert_eq!(outgoing_to_device.len(), 4);

        // The second time, as there are already in-flight requests, it should have no
        // effect.
        let should_query_secrets_now =
            alice.query_missing_secrets_from_other_sessions().await.unwrap();
        assert!(!should_query_secrets_now);
    }

    #[async_test]
    async fn test_request_missing_secrets_cross_signed() {
        let (alice, bob) = get_machine_pair_with_session(alice_id(), bob_id(), false).await;

        setup_cross_signing_for_machine_test_helper(&alice, &bob).await;

        let should_query_secrets = alice.query_missing_secrets_from_other_sessions().await.unwrap();

        assert!(should_query_secrets);

        let outgoing_to_device = alice
            .outgoing_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|outgoing| match outgoing.request.as_ref() {
                OutgoingRequests::ToDeviceRequest(request) => {
                    request.event_type.to_string() == "m.secret.request"
                }
                _ => false,
            })
            .collect_vec();
        assert_eq!(outgoing_to_device.len(), 1);

        // The second time, as there are already in-flight requests, it should have no
        // effect.
        let should_query_secrets_now =
            alice.query_missing_secrets_from_other_sessions().await.unwrap();
        assert!(!should_query_secrets_now);
    }

    #[async_test]
    async fn test_megolm_encryption() {
        let (alice, bob) =
            get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;
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
            .store()
            .with_transaction(|mut tr| async {
                let res =
                    bob.decrypt_to_device_event(&mut tr, &event, &mut Changes::default()).await?;
                Ok((tr, res))
            })
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
        let (alice, bob) =
            get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;
        let room_id = room_id!("!test:example.org");

        let room_keys_withheld_received_stream = bob.store().room_keys_withheld_received_stream();
        pin_mut!(room_keys_withheld_received_stream);

        let encryption_settings = EncryptionSettings::default();
        let encryption_settings = EncryptionSettings {
            sharing_strategy: CollectStrategy::new_device_based(true),
            ..encryption_settings
        };

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

        // We should receive a notification on the room_keys_withheld_received_stream
        let withheld_received = room_keys_withheld_received_stream
            .next()
            .now_or_never()
            .flatten()
            .expect("We should have received a notification of room key being withheld");
        assert_eq!(withheld_received.len(), 1);

        assert_eq!(&withheld_received[0].room_id, room_id);
        assert_matches!(
            &withheld_received[0].withheld_event.content,
            RoomKeyWithheldContent::MegolmV1AesSha2(MegolmV1AesSha2WithheldContent::Unverified(
                unverified_withheld_content
            ))
        );
        assert_eq!(unverified_withheld_content.room_id, room_id);

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

        assert_matches!(&decrypt_result, Err(MegolmError::MissingRoomKey(Some(_))));

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
        let (alice, bob) =
            get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;
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
            .store()
            .with_transaction(|mut tr| async {
                let res =
                    bob.decrypt_to_device_event(&mut tr, &event, &mut Changes::default()).await?;
                Ok((tr, res))
            })
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

        // get_room_event_encryption_info should return the same information
        let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();
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

        let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();

        assert_eq!(
            VerificationState::Unverified(VerificationLevel::UnsignedDevice),
            encryption_info.verification_state
        );
        assert_shield!(encryption_info, Red, Red);

        setup_cross_signing_for_machine_test_helper(&alice, &bob).await;
        let bob_id_from_alice = alice.get_identity(bob.user_id(), None).await.unwrap();
        assert_matches!(bob_id_from_alice, Some(UserIdentities::Other(_)));
        let alice_id_from_bob = bob.get_identity(alice.user_id(), None).await.unwrap();
        assert_matches!(alice_id_from_bob, Some(UserIdentities::Other(_)));

        // we setup cross signing but nothing is signed yet
        let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();

        assert_eq!(
            VerificationState::Unverified(VerificationLevel::UnsignedDevice),
            encryption_info.verification_state
        );
        assert_shield!(encryption_info, Red, Red);

        // Let alice sign her device
        sign_alice_device_for_machine_test_helper(&alice, &bob).await;

        let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();

        assert_eq!(
            VerificationState::Unverified(VerificationLevel::UnverifiedIdentity),
            encryption_info.verification_state
        );

        assert_shield!(encryption_info, Red, None);

        mark_alice_identity_as_verified_test_helper(&alice, &bob).await;
        let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();
        assert_eq!(VerificationState::Verified, encryption_info.verification_state);
        assert_shield!(encryption_info, None, None);

        // Simulate an imported session, to change verification state
        let imported = InboundGroupSession::from_export(&export).unwrap();
        bob.store().save_inbound_group_sessions(&[imported]).await.unwrap();

        let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();

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

    /// Test what happens when we feed an unencrypted event into the decryption
    /// functions
    #[async_test]
    async fn test_decrypt_unencrypted_event() {
        let (bob, _) = get_prepared_machine_test_helper(user_id(), false).await;
        let room_id = room_id!("!test:example.org");

        let event = json!({
            "event_id": "$xxxxx:example.org",
            "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
            "sender": user_id(),
            // it's actually the lack of an `algorithm` that upsets it, rather than the event type.
            "type": "m.room.encrypted",
            "content":  RoomMessageEventContent::text_plain("plain"),
        });

        let event = json_convert(&event).unwrap();

        // decrypt_room_event should return an error
        assert_matches!(
            bob.decrypt_room_event(&event, room_id).await,
            Err(MegolmError::JsonError(..))
        );

        // so should get_room_event_encryption_info
        assert_matches!(
            bob.get_room_event_encryption_info(&event, room_id).await,
            Err(MegolmError::JsonError(..))
        );
    }

    async fn setup_cross_signing_for_machine_test_helper(alice: &OlmMachine, bob: &OlmMachine) {
        let CrossSigningBootstrapRequests { upload_signing_keys_req: alice_upload_signing, .. } =
            alice.bootstrap_cross_signing(false).await.expect("Expect Alice x-signing key request");

        let CrossSigningBootstrapRequests { upload_signing_keys_req: bob_upload_signing, .. } =
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
            .expect("Can't parse the `/keys/upload` response");

        alice.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
        bob.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
    }

    async fn sign_alice_device_for_machine_test_helper(alice: &OlmMachine, bob: &OlmMachine) {
        let CrossSigningBootstrapRequests {
            upload_signing_keys_req: upload_signing,
            upload_signatures_req: upload_signature,
            ..
        } = alice.bootstrap_cross_signing(false).await.expect("Expect Alice x-signing key request");

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
            .expect("Can't parse the `/keys/upload` response");

        alice.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
        bob.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
    }

    async fn mark_alice_identity_as_verified_test_helper(alice: &OlmMachine, bob: &OlmMachine) {
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
            .upload_signing_keys_req
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
            .upload_signing_keys_req;

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
            .expect("Can't parse the `/keys/upload` response");

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
        let (bob, _) = get_prepared_machine_test_helper(user_id(), false).await;

        let other_user_id = user_id!("@web2:localhost:8482");

        let data = response_from_file(&test_json::KEYS_QUERY_TWO_DEVICES_ONE_SIGNED);
        let response = get_keys::v3::Response::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response");

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
            SenderData::unknown(),
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
            SenderData::unknown(),
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
        let (alice, bob) =
            get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;
        let room_id = room_id!("!test:example.org");

        // Need a second bob session to check gossiping
        let bob_id = user_id();
        let bob_other_device = device_id!("OTHERBOB");
        let bob_other_machine = OlmMachine::new(bob_id, bob_other_device).await;
        let bob_other_device =
            DeviceData::from_machine_test_helper(&bob_other_machine).await.unwrap();
        bob.store().save_device_data(&[bob_other_device]).await.unwrap();
        bob.get_device(bob_id, device_id!("OTHERBOB"), None)
            .await
            .unwrap()
            .expect("should exist")
            .set_trust_state(LocalTrust::Verified);

        alice.create_outbound_group_session_with_defaults_test_helper(room_id).await.unwrap();

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
            .store()
            .with_transaction(|mut tr| async {
                let res =
                    bob.decrypt_to_device_event(&mut tr, &event, &mut Changes::default()).await?;
                Ok((tr, res))
            })
            .await
            .unwrap()
            .inbound_group_session;
        bob.store().save_inbound_group_sessions(&[group_session.unwrap()]).await.unwrap();

        let room_event = json_convert(&room_event).unwrap();

        let decrypt_error = bob.decrypt_room_event(&room_event, room_id).await.unwrap_err();

        if let MegolmError::Decryption(vodo_error) = decrypt_error {
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
    async fn test_interactive_verification() {
        let (alice, bob) =
            get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;

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
    async fn test_interactive_verification_started_from_request() {
        let (alice, bob) =
            get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;

        // ----------------------------------------------------------------------------
        // On Alice's device:
        let bob_device =
            alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();

        assert!(!bob_device.is_verified());

        // Alice sends a verification request with her desired methods to Bob
        let (alice_ver_req, request) =
            bob_device.request_verification_with_methods(vec![VerificationMethod::SasV1]);

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
        let msg = &msgs[0];
        let event = outgoing_request_to_event(alice.user_id(), msg);
        alice.inner.verification_machine.mark_request_as_sent(&msg.request_id);

        // ----------------------------------------------------------------------------
        // On Bob's device:

        // And bob receive's it:
        bob.handle_verification_event(&event).await;

        // Now bob sends a key
        let msgs = bob.inner.verification_machine.outgoing_messages();
        assert!(msgs.len() == 1);
        let msg = &msgs[0];
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
    async fn test_room_key_over_megolm() {
        let (alice, bob) =
            get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;
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

        let content = message_like_event_content!({
            "algorithm": "m.megolm.v1.aes-sha2",
            "room_id": room_id,
            "session_id": session_id,
            "session_key": session_key.to_base64(),
        });

        let encrypted_content =
            alice.encrypt_room_event_raw(room_id, "m.room_key", &content).await.unwrap();
        let event = json!({
            "sender": alice.user_id(),
            "content": encrypted_content,
            "type": "m.room.encrypted",
        });

        let event: EncryptedToDeviceEvent = serde_json::from_value(event).unwrap();

        let decrypt_result = bob
            .store()
            .with_transaction(|mut tr| async {
                let res =
                    bob.decrypt_to_device_event(&mut tr, &event, &mut Changes::default()).await?;
                Ok((tr, res))
            })
            .await;

        assert_matches!(
            decrypt_result,
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
    async fn test_room_key_with_fake_identity_keys() {
        let room_id = room_id!("!test:localhost");
        let (alice, _) =
            get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;
        let device = DeviceData::from_machine_test_helper(&alice).await.unwrap();
        alice.store().save_device_data(&[device]).await.unwrap();

        let (outbound, mut inbound) = alice
            .store()
            .static_account()
            .create_group_session_pair(room_id, Default::default(), SenderData::unknown())
            .await
            .unwrap();

        let fake_key = Ed25519PublicKey::from_base64("ee3Ek+J2LkkPmjGPGLhMxiKnhiX//xcqaVL4RP6EypE")
            .unwrap()
            .into();
        let signing_keys = SigningKeys::from([(DeviceKeyAlgorithm::Ed25519, fake_key)]);
        inbound.creator_info.signing_keys = signing_keys.into();

        let content = message_like_event_content!({});
        let content = outbound.encrypt("m.dummy", &content).await;
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
            changes.identities.new.push(crate::UserIdentityData::Own(identity.inner));

            second_machine.store().save_changes(changes).await.unwrap();

            second_machine
        }

        let (alice, bob) =
            get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;
        setup_cross_signing_for_machine_test_helper(&alice, &bob).await;

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

    #[async_test]
    async fn test_wait_on_key_query_doesnt_block_store() {
        // Waiting for a key query shouldn't delay other write attempts to the store.
        // This test will end immediately if it works, and times out after a few seconds
        // if it failed.

        let machine = OlmMachine::new(bob_id(), bob_device_id()).await;

        // Mark Alice as a tracked user, so it gets into the groups of users for which
        // we need to query keys.
        machine.update_tracked_users([alice_id()]).await.unwrap();

        // Start a background task that will wait for the key query to finish silently
        // in the background.
        let machine_cloned = machine.clone();
        let wait = tokio::spawn(async move {
            let machine = machine_cloned;
            let user_devices =
                machine.get_user_devices(alice_id(), Some(Duration::from_secs(10))).await.unwrap();
            assert!(user_devices.devices().next().is_some());
        });

        // Let the background task work first.
        tokio::task::yield_now().await;

        // Create a key upload request and process it back immediately.
        let requests = machine.bootstrap_cross_signing(false).await.unwrap();

        let req = requests.upload_keys_req.expect("upload keys request should be there");
        let response = keys_upload_response();
        let mark_request_as_sent = machine.mark_request_as_sent(&req.request_id, &response);
        tokio::time::timeout(Duration::from_secs(5), mark_request_as_sent)
            .await
            .expect("no timeout")
            .expect("the underlying request has been marked as sent");

        // Answer the key query, so the background task completes immediately?
        let response = keys_query_response();
        let key_queries = machine.inner.identity_manager.users_for_key_query().await.unwrap();

        for (id, _) in key_queries {
            machine.mark_request_as_sent(&id, &response).await.unwrap();
        }

        // The waiting should successfully complete.
        wait.await.unwrap();
    }

    #[async_test]
    async fn room_settings_returns_none_for_unknown_room() {
        let machine = OlmMachine::new(user_id(), alice_device_id()).await;
        let settings = machine.room_settings(room_id!("!test2:localhost")).await.unwrap();
        assert!(settings.is_none());
    }

    #[async_test]
    async fn stores_and_returns_room_settings() {
        let machine = OlmMachine::new(user_id(), alice_device_id()).await;
        let room_id = room_id!("!test:localhost");

        let settings = RoomSettings {
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
            only_allow_trusted_devices: true,
            session_rotation_period: Some(Duration::from_secs(10)),
            session_rotation_period_messages: Some(1234),
        };

        machine.set_room_settings(room_id, &settings).await.unwrap();
        assert_eq!(machine.room_settings(room_id).await.unwrap(), Some(settings));
    }

    #[async_test]
    async fn set_room_settings_rejects_invalid_algorithms() {
        let machine = OlmMachine::new(user_id(), alice_device_id()).await;
        let room_id = room_id!("!test:localhost");

        let err = machine
            .set_room_settings(
                room_id,
                &RoomSettings {
                    algorithm: EventEncryptionAlgorithm::OlmV1Curve25519AesSha2,
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_matches!(err, SetRoomSettingsError::InvalidSettings);
    }

    #[async_test]
    async fn set_room_settings_rejects_changes() {
        let machine = OlmMachine::new(user_id(), alice_device_id()).await;
        let room_id = room_id!("!test:localhost");

        // Initial settings
        machine
            .set_room_settings(
                room_id,
                &RoomSettings { session_rotation_period_messages: Some(100), ..Default::default() },
            )
            .await
            .unwrap();

        // Now, modifying the settings should be rejected
        let err = machine
            .set_room_settings(
                room_id,
                &RoomSettings {
                    session_rotation_period_messages: Some(1000),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();

        assert_matches!(err, SetRoomSettingsError::EncryptionDowngrade);
    }

    #[async_test]
    async fn set_room_settings_accepts_noop_changes() {
        let machine = OlmMachine::new(user_id(), alice_device_id()).await;
        let room_id = room_id!("!test:localhost");

        // Initial settings
        machine
            .set_room_settings(
                room_id,
                &RoomSettings { session_rotation_period_messages: Some(100), ..Default::default() },
            )
            .await
            .unwrap();

        // Same again; should be fine.
        machine
            .set_room_settings(
                room_id,
                &RoomSettings { session_rotation_period_messages: Some(100), ..Default::default() },
            )
            .await
            .unwrap();
    }

    #[async_test]
    async fn test_send_encrypted_to_device() {
        let (alice, bob) = get_machine_pair_with_session(alice_id(), user_id(), false).await;

        let custom_event_type = "m.new_device";

        let custom_content = json!({
                "device_id": "XYZABCDE",
                "rooms": ["!726s6s6q:example.com"]
        });

        let device = alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();
        let raw_encrypted = device
            .encrypt_event_raw(custom_event_type, &custom_content)
            .await
            .expect("Should have encryted the content");

        let request = ToDeviceRequest::new(
            bob.user_id(),
            DeviceIdOrAllDevices::DeviceId(bob_device_id().to_owned()),
            "m.room.encrypted",
            raw_encrypted.cast(),
        );

        assert_eq!("m.room.encrypted", request.event_type.to_string());

        let messages = &request.messages;
        assert_eq!(1, messages.len());
        assert!(messages.get(bob.user_id()).is_some());
        let target_devices = messages.get(bob.user_id()).unwrap();
        assert_eq!(1, target_devices.len());
        assert!(target_devices
            .get(&DeviceIdOrAllDevices::DeviceId(bob_device_id().to_owned()))
            .is_some());

        let event = ToDeviceEvent::new(
            alice.user_id().to_owned(),
            to_device_requests_to_content(vec![request.clone().into()]),
        );

        let event = json_convert(&event).unwrap();

        let sync_changes = EncryptionSyncChanges {
            to_device_events: vec![event],
            changed_devices: &Default::default(),
            one_time_keys_counts: &Default::default(),
            unused_fallback_keys: None,
            next_batch_token: None,
        };

        let (decrypted, _) = bob.receive_sync_changes(sync_changes).await.unwrap();

        assert_eq!(1, decrypted.len());

        let decrypted_event = decrypted[0].deserialize().unwrap();

        assert_eq!(decrypted_event.event_type().to_string(), custom_event_type.to_owned());

        let decrypted_value = to_raw_value(&decrypted[0]).unwrap();
        let decrypted_value = serde_json::to_value(decrypted_value).unwrap();

        assert_eq!(
            decrypted_value.get("content").unwrap().get("device_id").unwrap().as_str().unwrap(),
            custom_content.get("device_id").unwrap().as_str().unwrap(),
        );

        assert_eq!(
            decrypted_value.get("content").unwrap().get("rooms").unwrap().as_array().unwrap(),
            custom_content.get("rooms").unwrap().as_array().unwrap(),
        );
    }

    #[async_test]
    async fn test_send_encrypted_to_device_no_session() {
        let (alice, bob, _) = get_machine_pair(alice_id(), user_id(), false).await;

        let custom_event_type = "m.new_device";

        let custom_content = json!({
                "device_id": "XYZABCDE",
                "rooms": ["!726s6s6q:example.com"]
        });

        let encryption_result = alice
            .get_device(bob.user_id(), bob_device_id(), None)
            .await
            .unwrap()
            .unwrap()
            .encrypt_event_raw(custom_event_type, &custom_content)
            .await;

        assert_matches!(encryption_result, Err(OlmError::MissingSession));
    }

    #[async_test]
    async fn test_fix_incorrect_usage_of_backup_key_causing_decryption_errors() {
        let store = MemoryStore::new();

        let backup_decryption_key = BackupDecryptionKey::new().unwrap();

        store
            .save_changes(Changes {
                backup_decryption_key: Some(backup_decryption_key.clone()),
                backup_version: Some("1".to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();

        // Some valid key data
        let data = json!({
           "algorithm": "m.megolm.v1.aes-sha2",
           "room_id": "!room:id",
           "sender_key": "FOvlmz18LLI3k/llCpqRoKT90+gFF8YhuL+v1YBXHlw",
           "session_id": "/2K+V777vipCxPZ0gpY9qcpz1DYaXwuMRIu0UEP0Wa0",
           "session_key": "AQAAAAAclzWVMeWBKH+B/WMowa3rb4ma3jEl6n5W4GCs9ue65CruzD3ihX+85pZ9hsV9Bf6fvhjp76WNRajoJYX0UIt7aosjmu0i+H+07hEQ0zqTKpVoSH0ykJ6stAMhdr6Q4uW5crBmdTTBIsqmoWsNJZKKoE2+ldYrZ1lrFeaJbjBIY/9ivle++74qQsT2dIKWPanKc9Q2Gl8LjESLtFBD9Fmt",
           "sender_claimed_keys": {
               "ed25519": "F4P7f1Z0RjbiZMgHk1xBCG3KC4/Ng9PmxLJ4hQ13sHA"
           },
           "forwarding_curve25519_key_chain": ["DBPC2zr6c9qimo9YRFK3RVr0Two/I6ODb9mbsToZN3Q", "bBc/qzZFOOKshMMT+i4gjS/gWPDoKfGmETs9yfw9430"]
        });

        let backed_up_room_key: BackedUpRoomKey = serde_json::from_value(data).unwrap();

        // Create the machine using `with_store` and without a call to enable_backup_v1,
        // like regenerate_olm would do
        let alice =
            OlmMachine::with_store(user_id(), alice_device_id(), store, None).await.unwrap();

        let exported_key = ExportedRoomKey::from_backed_up_room_key(
            room_id!("!room:id").to_owned(),
            "/2K+V777vipCxPZ0gpY9qcpz1DYaXwuMRIu0UEP0Wa0".into(),
            backed_up_room_key,
        );

        alice.store().import_exported_room_keys(vec![exported_key], |_, _| {}).await.unwrap();

        let (_, request) = alice.backup_machine().backup().await.unwrap().unwrap();

        let key_backup_data = request.rooms[&room_id!("!room:id").to_owned()]
            .sessions
            .get("/2K+V777vipCxPZ0gpY9qcpz1DYaXwuMRIu0UEP0Wa0")
            .unwrap()
            .deserialize()
            .unwrap();

        let ephemeral = key_backup_data.session_data.ephemeral.encode();
        let ciphertext = key_backup_data.session_data.ciphertext.encode();
        let mac = key_backup_data.session_data.mac.encode();

        // Prior to the fix for GHSA-9ggc-845v-gcgv, this would produce a
        // `Mac(MacError)`
        backup_decryption_key
            .decrypt_v1(&ephemeral, &mac, &ciphertext)
            .expect("The backed up key should be decrypted successfully");
    }

    #[async_test]
    async fn test_olm_machine_with_custom_account() {
        let store = MemoryStore::new();
        let account = vodozemac::olm::Account::new();
        let curve_key = account.identity_keys().curve25519;

        let alice = OlmMachine::with_store(user_id(), alice_device_id(), store, Some(account))
            .await
            .unwrap();

        assert_eq!(
            alice.identity_keys().curve25519,
            curve_key,
            "The Olm machine should have used the Account we provided"
        );
    }

    #[async_test]
    async fn test_unsigned_decryption() {
        let (alice, bob) =
            get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;
        let room_id = room_id!("!test:example.org");

        // Share the room key for the first message.
        let to_device_requests = alice
            .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
            .await
            .unwrap();
        let first_room_key_event = ToDeviceEvent::new(
            alice.user_id().to_owned(),
            to_device_requests_to_content(to_device_requests),
        );

        // Save the first room key.
        let group_session = bob
            .store()
            .with_transaction(|mut tr| async {
                let res = bob
                    .decrypt_to_device_event(
                        &mut tr,
                        &first_room_key_event,
                        &mut Changes::default(),
                    )
                    .await?;
                Ok((tr, res))
            })
            .await
            .unwrap()
            .inbound_group_session;
        bob.store().save_inbound_group_sessions(&[group_session.unwrap()]).await.unwrap();

        // Encrypt first message.
        let first_message_text = "This is the original message";
        let first_message_content = RoomMessageEventContent::text_plain(first_message_text);
        let first_message_encrypted_content =
            alice.encrypt_room_event(room_id, first_message_content).await.unwrap();

        let mut first_message_encrypted_event = json!({
            "event_id": "$message1",
            "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
            "sender": alice.user_id(),
            "type": "m.room.encrypted",
            "content": first_message_encrypted_content,
        });
        let raw_encrypted_event = json_convert(&first_message_encrypted_event).unwrap();

        // Bob has the room key, so first message should be decrypted successfully.
        let raw_decrypted_event =
            bob.decrypt_room_event(&raw_encrypted_event, room_id).await.unwrap();

        let decrypted_event = raw_decrypted_event.event.deserialize().unwrap();
        assert_matches!(
            decrypted_event,
            AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(first_message))
        );

        let first_message = first_message.as_original().unwrap();
        assert_eq!(first_message.content.body(), first_message_text);
        assert!(first_message.unsigned.relations.is_empty());

        assert!(raw_decrypted_event.encryption_info.is_some());
        assert!(raw_decrypted_event.unsigned_encryption_info.is_none());

        // Get a new room key, but don't give it to Bob yet.
        alice.discard_room_key(room_id).await.unwrap();
        let to_device_requests = alice
            .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
            .await
            .unwrap();
        let second_room_key_event = ToDeviceEvent::new(
            alice.user_id().to_owned(),
            to_device_requests_to_content(to_device_requests),
        );

        // Encrypt a second message, an edit.
        let second_message_text = "This is the ~~original~~ edited message";
        let second_message_content = RoomMessageEventContent::text_plain(second_message_text)
            .make_replacement(first_message, None);
        let second_message_encrypted_content =
            alice.encrypt_room_event(room_id, second_message_content).await.unwrap();

        let second_message_encrypted_event = json!({
            "event_id": "$message2",
            "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
            "sender": alice.user_id(),
            "type": "m.room.encrypted",
            "content": second_message_encrypted_content,
        });

        // Bundle the edit in the unsigned object of the first event.
        let relations = json!({
            "m.relations": {
                "m.replace": second_message_encrypted_event,
            },
        });
        first_message_encrypted_event
            .as_object_mut()
            .unwrap()
            .insert("unsigned".to_owned(), relations);
        let raw_encrypted_event = json_convert(&first_message_encrypted_event).unwrap();

        // Bob does not have the second room key, so second message should fail to
        // decrypt.
        let raw_decrypted_event =
            bob.decrypt_room_event(&raw_encrypted_event, room_id).await.unwrap();

        let decrypted_event = raw_decrypted_event.event.deserialize().unwrap();
        assert_matches!(
            decrypted_event,
            AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(first_message))
        );

        let first_message = first_message.as_original().unwrap();
        assert_eq!(first_message.content.body(), first_message_text);
        // Deserialization of the edit failed, but it was here.
        assert!(first_message.unsigned.relations.replace.is_none());
        assert!(first_message.unsigned.relations.has_replacement());

        assert!(raw_decrypted_event.encryption_info.is_some());
        let unsigned_encryption_info = raw_decrypted_event.unsigned_encryption_info.unwrap();
        assert_eq!(unsigned_encryption_info.len(), 1);
        let replace_encryption_result =
            unsigned_encryption_info.get(&UnsignedEventLocation::RelationsReplace).unwrap();
        assert_matches!(
            replace_encryption_result,
            UnsignedDecryptionResult::UnableToDecrypt(UnableToDecryptInfo {
                session_id: Some(second_room_key_session_id)
            })
        );

        // Give Bob the second room key.
        let group_session = bob
            .store()
            .with_transaction(|mut tr| async {
                let res = bob
                    .decrypt_to_device_event(
                        &mut tr,
                        &second_room_key_event,
                        &mut Changes::default(),
                    )
                    .await?;
                Ok((tr, res))
            })
            .await
            .unwrap()
            .inbound_group_session
            .unwrap();
        assert_eq!(group_session.session_id(), second_room_key_session_id);
        bob.store().save_inbound_group_sessions(&[group_session]).await.unwrap();

        // Second message should decrypt now.
        let raw_decrypted_event =
            bob.decrypt_room_event(&raw_encrypted_event, room_id).await.unwrap();

        let decrypted_event = raw_decrypted_event.event.deserialize().unwrap();
        assert_matches!(
            decrypted_event,
            AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(first_message))
        );

        let first_message = first_message.as_original().unwrap();
        assert_eq!(first_message.content.body(), first_message_text);
        let replace = first_message.unsigned.relations.replace.as_ref().unwrap();
        assert_matches!(&replace.content.relates_to, Some(Relation::Replacement(replace_content)));
        assert_eq!(replace_content.new_content.msgtype.body(), second_message_text);

        assert!(raw_decrypted_event.encryption_info.is_some());
        let unsigned_encryption_info = raw_decrypted_event.unsigned_encryption_info.unwrap();
        assert_eq!(unsigned_encryption_info.len(), 1);
        let replace_encryption_result =
            unsigned_encryption_info.get(&UnsignedEventLocation::RelationsReplace).unwrap();
        assert_matches!(replace_encryption_result, UnsignedDecryptionResult::Decrypted(_));

        // Get a new room key again, but don't give it to Bob yet.
        alice.discard_room_key(room_id).await.unwrap();
        let to_device_requests = alice
            .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
            .await
            .unwrap();
        let third_room_key_event = ToDeviceEvent::new(
            alice.user_id().to_owned(),
            to_device_requests_to_content(to_device_requests),
        );

        // Encrypt a third message, a thread event.
        let third_message_text = "This a reply in a thread";
        let third_message_content = RoomMessageEventContent::text_plain(third_message_text)
            .make_for_thread(first_message, ReplyWithinThread::No, AddMentions::No);
        let third_message_encrypted_content =
            alice.encrypt_room_event(room_id, third_message_content).await.unwrap();

        let third_message_encrypted_event = json!({
            "event_id": "$message3",
            "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
            "sender": alice.user_id(),
            "type": "m.room.encrypted",
            "content": third_message_encrypted_content,
            "room_id": room_id,
        });

        // Bundle the edit in the unsigned object of the first event.
        let relations = json!({
            "m.relations": {
                "m.replace": second_message_encrypted_event,
                "m.thread": {
                    "latest_event": third_message_encrypted_event,
                    "count": 1,
                    "current_user_participated": true,
                }
            },
        });
        first_message_encrypted_event
            .as_object_mut()
            .unwrap()
            .insert("unsigned".to_owned(), relations);
        let raw_encrypted_event = json_convert(&first_message_encrypted_event).unwrap();

        // Bob does not have the third room key, so third message should fail to
        // decrypt.
        let raw_decrypted_event =
            bob.decrypt_room_event(&raw_encrypted_event, room_id).await.unwrap();

        let decrypted_event = raw_decrypted_event.event.deserialize().unwrap();
        assert_matches!(
            decrypted_event,
            AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(first_message))
        );

        let first_message = first_message.as_original().unwrap();
        assert_eq!(first_message.content.body(), first_message_text);
        assert!(first_message.unsigned.relations.replace.is_some());
        // Deserialization of the thread event succeeded, but it is still encrypted.
        let thread = first_message.unsigned.relations.thread.as_ref().unwrap();
        assert_matches!(
            thread.latest_event.deserialize(),
            Ok(AnyMessageLikeEvent::RoomEncrypted(_))
        );

        assert!(raw_decrypted_event.encryption_info.is_some());
        let unsigned_encryption_info = raw_decrypted_event.unsigned_encryption_info.unwrap();
        assert_eq!(unsigned_encryption_info.len(), 2);
        let replace_encryption_result =
            unsigned_encryption_info.get(&UnsignedEventLocation::RelationsReplace).unwrap();
        assert_matches!(replace_encryption_result, UnsignedDecryptionResult::Decrypted(_));
        let thread_encryption_result = unsigned_encryption_info
            .get(&UnsignedEventLocation::RelationsThreadLatestEvent)
            .unwrap();
        assert_matches!(
            thread_encryption_result,
            UnsignedDecryptionResult::UnableToDecrypt(UnableToDecryptInfo {
                session_id: Some(third_room_key_session_id)
            })
        );

        // Give Bob the third room key.
        let group_session = bob
            .store()
            .with_transaction(|mut tr| async {
                let res = bob
                    .decrypt_to_device_event(
                        &mut tr,
                        &third_room_key_event,
                        &mut Changes::default(),
                    )
                    .await?;
                Ok((tr, res))
            })
            .await
            .unwrap()
            .inbound_group_session
            .unwrap();
        assert_eq!(group_session.session_id(), third_room_key_session_id);
        bob.store().save_inbound_group_sessions(&[group_session]).await.unwrap();

        // Third message should decrypt now.
        let raw_decrypted_event =
            bob.decrypt_room_event(&raw_encrypted_event, room_id).await.unwrap();

        let decrypted_event = raw_decrypted_event.event.deserialize().unwrap();
        assert_matches!(
            decrypted_event,
            AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(first_message))
        );

        let first_message = first_message.as_original().unwrap();
        assert_eq!(first_message.content.body(), first_message_text);
        assert!(first_message.unsigned.relations.replace.is_some());
        let thread = &first_message.unsigned.relations.thread.as_ref().unwrap();
        assert_matches!(
            thread.latest_event.deserialize(),
            Ok(AnyMessageLikeEvent::RoomMessage(third_message))
        );
        let third_message = third_message.as_original().unwrap();
        assert_eq!(third_message.content.body(), third_message_text);

        assert!(raw_decrypted_event.encryption_info.is_some());
        let unsigned_encryption_info = raw_decrypted_event.unsigned_encryption_info.unwrap();
        assert_eq!(unsigned_encryption_info.len(), 2);
        let replace_encryption_result =
            unsigned_encryption_info.get(&UnsignedEventLocation::RelationsReplace).unwrap();
        assert_matches!(replace_encryption_result, UnsignedDecryptionResult::Decrypted(_));
        let thread_encryption_result = unsigned_encryption_info
            .get(&UnsignedEventLocation::RelationsThreadLatestEvent)
            .unwrap();
        assert_matches!(thread_encryption_result, UnsignedDecryptionResult::Decrypted(_));
    }
}
