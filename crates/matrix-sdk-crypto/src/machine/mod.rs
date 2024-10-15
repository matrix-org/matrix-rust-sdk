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
        AlgorithmInfo, DecryptedRoomEvent, DeviceLinkProblem, EncryptionInfo, UnableToDecryptInfo,
        UnsignedDecryptionResult, UnsignedEventLocation, VerificationLevel, VerificationState,
    },
    executor::BoxFuture,
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
        AnyToDeviceEvent, MessageLikeEventContent,
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
    identities::{user::UserIdentity, Device, IdentityManager, UserDevices},
    olm::{
        Account, CrossSigningStatus, EncryptionSettings, IdentityKeys, InboundGroupSession,
        KnownSenderData, OlmDecryptionInfo, PrivateCrossSigningIdentity, SenderData,
        SenderDataFinder, SessionType, StaticAccountData,
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
    CrossSigningKeyExport, CryptoStoreError, DecryptionSettings, DeviceData, KeysQueryRequest,
    LocalTrust, SignatureError, ToDeviceRequest, TrustRequirement,
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
    const HAS_MIGRATED_VERIFICATION_LATCH: &'static str = "HAS_MIGRATED_VERIFICATION_LATCH";

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

        let store =
            Arc::new(CryptoStoreWrapper::new(self.user_id(), device_id, MemoryStore::new()));
        let device = DeviceData::from_account(&account);
        store.save_pending_changes(PendingChanges { account: Some(account) }).await?;
        store
            .save_changes(Changes {
                devices: DeviceChanges { new: vec![device], ..Default::default() },
                ..Default::default()
            })
            .await?;

        let (verification_machine, store, identity_manager) =
            Self::new_helper_prelude(store, static_account, self.store().private_identity());

        Ok(Self::new_helper(
            device_id,
            store,
            verification_machine,
            identity_manager,
            self.store().private_identity(),
            None,
        ))
    }

    fn new_helper_prelude(
        store_wrapper: Arc<CryptoStoreWrapper>,
        account: StaticAccountData,
        user_identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
    ) -> (VerificationMachine, Store, IdentityManager) {
        let verification_machine =
            VerificationMachine::new(account.clone(), user_identity.clone(), store_wrapper.clone());
        let store = Store::new(account, user_identity, store_wrapper, verification_machine.clone());

        let identity_manager = IdentityManager::new(store.clone());

        (verification_machine, store, identity_manager)
    }

    fn new_helper(
        device_id: &DeviceId,
        store: Store,
        verification_machine: VerificationMachine,
        identity_manager: IdentityManager,
        user_identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
        maybe_backup_key: Option<MegolmV1BackupKey>,
    ) -> Self {
        let group_session_manager = GroupSessionManager::new(store.clone());

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
        let store = Arc::new(CryptoStoreWrapper::new(user_id, device_id, store));

        let (verification_machine, store, identity_manager) =
            Self::new_helper_prelude(store, static_account, identity.clone());

        // FIXME: We might want in the future a more generic high-level data migration
        // mechanism (at the store wrapper layer).
        Self::migration_post_verified_latch_support(&store, &identity_manager).await?;

        Ok(Self::new_helper(
            device_id,
            store,
            verification_machine,
            identity_manager,
            identity,
            maybe_backup_key,
        ))
    }

    // The sdk now support verified identity change detection.
    // This introduces a new local flag (`verified_latch` on
    // `OtherUserIdentityData`). In order to ensure that this flag is up-to-date and
    // for the sake of simplicity we force a re-download of tracked users by marking
    // them as dirty.
    //
    // pub(crate) visibility for testing.
    pub(crate) async fn migration_post_verified_latch_support(
        store: &Store,
        identity_manager: &IdentityManager,
    ) -> Result<(), CryptoStoreError> {
        let maybe_migrate_for_identity_verified_latch =
            store.get_custom_value(Self::HAS_MIGRATED_VERIFICATION_LATCH).await?.is_none();

        if maybe_migrate_for_identity_verified_latch {
            identity_manager.mark_all_tracked_users_as_dirty(store.cache().await?).await?;

            store.set_custom_value(Self::HAS_MIGRATED_VERIFICATION_LATCH, vec![0]).await?
        }
        Ok(())
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
        // Don't hold the lock, otherwise we might deadlock in
        // `bootstrap_cross_signing()` on `account` if a sync task is already
        // running (which locks `account`), or we will deadlock
        // in `upload_device_keys()` which locks private identity again.
        let identity = self.inner.user_identity.lock().await.clone();

        let (upload_signing_keys_req, upload_signatures_req) = if reset || identity.is_empty().await
        {
            info!("Creating new cross signing identity");

            let (identity, upload_signing_keys_req, upload_signatures_req) = {
                let cache = self.inner.store.cache().await?;
                let account = cache.account().await?;
                account.bootstrap_cross_signing().await
            };

            let public = identity.to_public_identity().await.expect(
                "Couldn't create a public version of the identity from a new private identity",
            );

            *self.inner.user_identity.lock().await = identity.clone();

            self.store()
                .save_changes(Changes {
                    identities: IdentityChanges { new: vec![public.into()], ..Default::default() },
                    private_identity: Some(identity),
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
        fields(room_id = ? content.room_id, session_id)
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
            SenderData::unknown(),
            event.content.algorithm(),
            None,
        );

        match session {
            Ok(mut session) => {
                Span::current().record("session_id", session.session_id());

                let sender_data =
                    SenderDataFinder::find_using_event(self.store(), sender_key, event, &session)
                        .await?;

                session.sender_data = sender_data;

                match self.store().compare_group_session(&session).await? {
                    SessionOrdering::Better => {
                        info!("Received a new megolm room key");

                        Ok(Some(session))
                    }
                    comparison_result => {
                        warn!(
                            ?comparison_result,
                            "Received a megolm room key that we already have a better version \
                             of, discarding"
                        );

                        Ok(None)
                    }
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
        debug!(
            sender_device_keys =
                ?decrypted.result.event.sender_device_keys().map(|k| (k.curve25519_key(), k.ed25519_key())).unwrap_or((None, None)),
            "Received a decrypted to-device event",
        );

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

    /// Find whether the supplied session is verified, and provide
    /// explanation of what is missing/wrong if not.
    ///
    /// Checks both the stored verification state of the session and a
    /// recalculated verification state based on our current knowledge, and
    /// returns the more trusted of the two.
    ///
    /// Store the updated [`SenderData`] for this session in the store
    /// if we find an updated value for it.
    async fn get_or_update_verification_state(
        &self,
        session: &InboundGroupSession,
        sender: &UserId,
    ) -> MegolmResult<(VerificationState, Option<OwnedDeviceId>)> {
        /// Whether we should recalculate the Megolm sender's data, given the
        /// current sender data. We only want to recalculate if it might
        /// increase trust and allow us to decrypt messages that we
        /// otherwise might refuse to decrypt.
        ///
        /// We recalculate for all states except:
        ///
        /// - SenderUnverified: the sender is trusted enough that we will
        ///   decrypt their messages in all cases, or
        /// - SenderVerified: the sender is the most trusted they can be.
        fn should_recalculate_sender_data(sender_data: &SenderData) -> bool {
            matches!(
                sender_data,
                SenderData::UnknownDevice { .. }
                    | SenderData::DeviceInfo { .. }
                    | SenderData::VerificationViolation { .. }
            )
        }

        let sender_data = if should_recalculate_sender_data(&session.sender_data) {
            // The session is not sure of the sender yet. Calculate it.
            let calculated_sender_data = SenderDataFinder::find_using_curve_key(
                self.store(),
                session.sender_key(),
                sender,
                session,
            )
            .await?;

            // Is the newly-calculated sender data more trusted?
            if calculated_sender_data.compare_trust_level(&session.sender_data).is_gt() {
                // Yes - save it to the store
                let mut new_session = session.clone();
                new_session.sender_data = calculated_sender_data.clone();
                self.store().save_inbound_group_sessions(&[new_session]).await?;

                // and use it now.
                calculated_sender_data
            } else {
                // No - use the existing data.
                session.sender_data.clone()
            }
        } else {
            session.sender_data.clone()
        };

        Ok(sender_data_to_verification_state(sender_data, session.has_been_imported()))
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
        let (verification_state, device_id) =
            self.get_or_update_verification_state(session, sender).await?;

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
        decryption_settings: &DecryptionSettings,
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

                self.check_sender_trust_requirement(
                    &session,
                    &encryption_info,
                    &decryption_settings.sender_device_trust_requirement,
                )?;

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

    /// Check that the sender of a Megolm session satisfies the trust
    /// requirement from the decryption settings.
    fn check_sender_trust_requirement(
        &self,
        session: &InboundGroupSession,
        encryption_info: &EncryptionInfo,
        trust_requirement: &TrustRequirement,
    ) -> MegolmResult<()> {
        /// Get the error from the encryption information.
        fn encryption_info_to_error(encryption_info: &EncryptionInfo) -> MegolmResult<()> {
            // When this is called, the verification state *must* be unverified,
            // otherwise the sender_data would have been SenderVerified
            let VerificationState::Unverified(verification_level) =
                &encryption_info.verification_state
            else {
                unreachable!("inconsistent verification state");
            };
            Err(MegolmError::SenderIdentityNotTrusted(verification_level.clone()))
        }

        match trust_requirement {
            TrustRequirement::Untrusted => Ok(()),

            TrustRequirement::CrossSignedOrLegacy => match &session.sender_data {
                // Reject if the sender was previously verified, but changed
                // their identity and is not verified any more.
                SenderData::VerificationViolation(..) => Err(
                    MegolmError::SenderIdentityNotTrusted(VerificationLevel::VerificationViolation),
                ),
                SenderData::SenderUnverified(..) => Ok(()),
                SenderData::SenderVerified(..) => Ok(()),
                SenderData::DeviceInfo { legacy_session: true, .. } => Ok(()),
                SenderData::UnknownDevice { legacy_session: true, .. } => Ok(()),
                _ => encryption_info_to_error(encryption_info),
            },

            TrustRequirement::CrossSigned => match &session.sender_data {
                // Reject if the sender was previously verified, but changed
                // their identity and is not verified any more.
                SenderData::VerificationViolation(..) => Err(
                    MegolmError::SenderIdentityNotTrusted(VerificationLevel::VerificationViolation),
                ),
                SenderData::SenderUnverified(..) => Ok(()),
                SenderData::SenderVerified(..) => Ok(()),
                _ => encryption_info_to_error(encryption_info),
            },
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
        decryption_settings: &DecryptionSettings,
    ) -> MegolmResult<DecryptedRoomEvent> {
        self.decrypt_room_event_inner(event, room_id, true, decryption_settings).await
    }

    #[instrument(name = "decrypt_room_event", skip_all, fields(?room_id, event_id, origin_server_ts, sender, algorithm, session_id, message_index, sender_key))]
    async fn decrypt_room_event_inner(
        &self,
        event: &Raw<EncryptedEvent>,
        room_id: &RoomId,
        decrypt_unsigned: bool,
        decryption_settings: &DecryptionSettings,
    ) -> MegolmResult<DecryptedRoomEvent> {
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
        Span::current().record("message_index", content.message_index());

        let result =
            self.decrypt_megolm_events(room_id, &event, &content, decryption_settings).await;

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
            unsigned_encryption_info = self
                .decrypt_unsigned_events(&mut decrypted_event, room_id, decryption_settings)
                .await;
        }

        let event = serde_json::from_value::<Raw<AnyMessageLikeEvent>>(decrypted_event.into())?;

        Ok(DecryptedRoomEvent { event, encryption_info, unsigned_encryption_info })
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
        decryption_settings: &DecryptionSettings,
    ) -> Option<BTreeMap<UnsignedEventLocation, UnsignedDecryptionResult>> {
        let unsigned = main_event.get_mut("unsigned")?.as_object_mut()?;
        let mut unsigned_encryption_info: Option<
            BTreeMap<UnsignedEventLocation, UnsignedDecryptionResult>,
        > = None;

        // Search for an encrypted event in `m.replace`, an edit.
        let location = UnsignedEventLocation::RelationsReplace;
        let replace = location.find_mut(unsigned);
        if let Some(decryption_result) =
            self.decrypt_unsigned_event(replace, room_id, decryption_settings).await
        {
            unsigned_encryption_info
                .get_or_insert_with(Default::default)
                .insert(location, decryption_result);
        }

        // Search for an encrypted event in `latest_event` in `m.thread`, the
        // latest event of a thread.
        let location = UnsignedEventLocation::RelationsThreadLatestEvent;
        let thread_latest_event = location.find_mut(unsigned);
        if let Some(decryption_result) =
            self.decrypt_unsigned_event(thread_latest_event, room_id, decryption_settings).await
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
        decryption_settings: &'a DecryptionSettings,
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
            match self
                .decrypt_room_event_inner(&raw_event, room_id, false, decryption_settings)
                .await
            {
                Ok(decrypted_event) => {
                    // Replace the encrypted event.
                    *event = serde_json::to_value(decrypted_event.event).ok()?;
                    Some(UnsignedDecryptionResult::Decrypted(decrypted_event.encryption_info))
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

    /// Mark all tracked users as dirty.
    ///
    /// All users *whose device lists we are tracking* are flagged as needing a
    /// key query. Users whose devices we are not tracking are ignored.
    pub async fn mark_all_tracked_users_as_dirty(&self) -> StoreResult<()> {
        self.inner
            .identity_manager
            .mark_all_tracked_users_as_dirty(self.inner.store.cache().await?)
            .await
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
    /// Returns a [`UserIdentity`] enum if one is found and the crypto store
    /// didn't throw an error.
    #[instrument(skip(self))]
    pub async fn get_identity(
        &self,
        user_id: &UserId,
        timeout: Option<Duration>,
    ) -> StoreResult<Option<UserIdentity>> {
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

    /// Returns whether this `OlmMachine` is the same another one.
    ///
    /// Useful for testing purposes only.
    #[cfg(any(feature = "testing", test))]
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

    /// Returns the identity manager.
    #[cfg(test)]
    pub(crate) fn identity_manager(&self) -> &IdentityManager {
        &self.inner.identity_manager
    }

    /// Returns a store key, only useful for testing purposes.
    #[cfg(test)]
    pub(crate) fn key_for_has_migrated_verification_latch() -> &'static str {
        Self::HAS_MIGRATED_VERIFICATION_LATCH
    }
}

fn sender_data_to_verification_state(
    sender_data: SenderData,
    session_has_been_imported: bool,
) -> (VerificationState, Option<OwnedDeviceId>) {
    match sender_data {
        SenderData::UnknownDevice { owner_check_failed: false, .. } => {
            let device_link_problem = if session_has_been_imported {
                DeviceLinkProblem::InsecureSource
            } else {
                DeviceLinkProblem::MissingDevice
            };

            (VerificationState::Unverified(VerificationLevel::None(device_link_problem)), None)
        }
        SenderData::UnknownDevice { owner_check_failed: true, .. } => (
            VerificationState::Unverified(VerificationLevel::None(
                DeviceLinkProblem::InsecureSource,
            )),
            None,
        ),
        SenderData::DeviceInfo { device_keys, .. } => (
            VerificationState::Unverified(VerificationLevel::UnsignedDevice),
            Some(device_keys.device_id),
        ),
        SenderData::VerificationViolation(KnownSenderData { device_id, .. }) => {
            (VerificationState::Unverified(VerificationLevel::VerificationViolation), device_id)
        }
        SenderData::SenderUnverified(KnownSenderData { device_id, .. }) => {
            (VerificationState::Unverified(VerificationLevel::UnverifiedIdentity), device_id)
        }
        SenderData::SenderVerified(KnownSenderData { device_id, .. }) => {
            (VerificationState::Verified, device_id)
        }
    }
}

/// A set of requests to be executed when bootstrapping cross-signing using
/// [`OlmMachine::bootstrap_cross_signing`].
#[derive(Debug, Clone)]
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

#[cfg(test)]
pub(crate) mod test_helpers;

#[cfg(test)]
pub(crate) mod tests;
