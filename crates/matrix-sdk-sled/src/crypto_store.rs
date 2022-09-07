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
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use dashmap::DashSet;
use matrix_sdk_common::locks::Mutex;
use matrix_sdk_crypto::{
    olm::{
        IdentityKeys, InboundGroupSession, OutboundGroupSession, PickledInboundGroupSession,
        PrivateCrossSigningIdentity, Session,
    },
    store::{
        caches::SessionStore, BackupKeys, Changes, CryptoStore, CryptoStoreError, Result,
        RoomKeyCounts,
    },
    types::{events::room_key_request::SupportedKeyInfo, EventEncryptionAlgorithm},
    GossipRequest, ReadOnlyAccount, ReadOnlyDevice, ReadOnlyUserIdentities, SecretInfo,
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, RoomId, TransactionId, UserId};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
pub use sled::Error;
use sled::{
    transaction::{ConflictableTransactionError, TransactionError},
    Batch, Config, Db, IVec, Transactional, Tree,
};
use tracing::debug;

use super::OpenStoreError;
use crate::encode_key::{EncodeKey, ENCODE_SEPARATOR};

const DATABASE_VERSION: u8 = 5;

// Table names that are used to derive a separate key for each tree. This ensure
// that user ids encoded for different trees won't end up as the same byte
// sequence. This prevents corelation attacks on our tree metadata.
const DEVICE_TABLE_NAME: &str = "crypto-store-devices";
const IDENTITIES_TABLE_NAME: &str = "crypto-store-identities";
const SESSIONS_TABLE_NAME: &str = "crypto-store-sessions";
const INBOUND_GROUP_TABLE_NAME: &str = "crypto-store-inbound-group-sessions";
const OUTBOUND_GROUP_TABLE_NAME: &str = "crypto-store-outbound-group-sessions";
const SECRET_REQUEST_BY_INFO_TABLE: &str = "crypto-store-secret-request-by-info";
const TRACKED_USERS_TABLE: &str = "crypto-store-secret-tracked-users";

impl EncodeKey for InboundGroupSession {
    fn encode(&self) -> Vec<u8> {
        (self.room_id(), self.sender_key().to_base64(), self.session_id()).encode()
    }

    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        (self.room_id(), self.sender_key().to_base64(), self.session_id())
            .encode_secure(table_name, store_cipher)
    }
}

impl EncodeKey for OutboundGroupSession {
    fn encode(&self) -> Vec<u8> {
        self.room_id().encode()
    }
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        self.room_id().encode_secure(table_name, store_cipher)
    }
}

impl EncodeKey for Session {
    fn encode(&self) -> Vec<u8> {
        let sender_key = self.sender_key().to_base64();
        let session_id = self.session_id();

        [sender_key.as_bytes(), &[ENCODE_SEPARATOR], session_id.as_bytes(), &[ENCODE_SEPARATOR]]
            .concat()
    }

    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        let sender_key =
            store_cipher.hash_key(table_name, self.sender_key().to_base64().as_bytes());
        let session_id = store_cipher.hash_key(table_name, self.session_id().as_bytes());

        [sender_key.as_slice(), &[ENCODE_SEPARATOR], session_id.as_slice(), &[ENCODE_SEPARATOR]]
            .concat()
    }
}

impl EncodeKey for SecretInfo {
    fn encode(&self) -> Vec<u8> {
        match self {
            SecretInfo::KeyRequest(k) => k.encode(),
            SecretInfo::SecretRequest(s) => s.encode(),
        }
    }
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        match self {
            SecretInfo::KeyRequest(k) => k.encode_secure(table_name, store_cipher),
            SecretInfo::SecretRequest(s) => s.encode_secure(table_name, store_cipher),
        }
    }
}

impl EncodeKey for EventEncryptionAlgorithm {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        let s: &str = self.as_ref();
        s.as_bytes().into()
    }
}

impl EncodeKey for SupportedKeyInfo {
    fn encode(&self) -> Vec<u8> {
        (self.room_id(), &self.algorithm(), self.session_id()).encode()
    }
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        let room_id = store_cipher.hash_key(table_name, self.room_id().as_bytes());
        let algorithm = store_cipher.hash_key(table_name, self.algorithm().as_ref().as_bytes());
        let session_id = store_cipher.hash_key(table_name, self.session_id().as_bytes());

        [
            room_id.as_slice(),
            &[ENCODE_SEPARATOR],
            algorithm.as_slice(),
            &[ENCODE_SEPARATOR],
            session_id.as_slice(),
            &[ENCODE_SEPARATOR],
        ]
        .concat()
    }
}

impl EncodeKey for ReadOnlyDevice {
    fn encode(&self) -> Vec<u8> {
        (self.user_id(), self.device_id()).encode()
    }
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        (self.user_id(), self.device_id()).encode_secure(table_name, store_cipher)
    }
}

#[derive(Clone, Debug)]
pub struct AccountInfo {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceId>,
    identity_keys: Arc<IdentityKeys>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TrackedUser {
    user_id: OwnedUserId,
    dirty: bool,
}

/// A [sled] based cryptostore.
///
/// [sled]: https://github.com/spacejam/sled#readme
#[derive(Clone)]
pub struct SledCryptoStore {
    account_info: Arc<RwLock<Option<AccountInfo>>>,
    store_cipher: Option<Arc<StoreCipher>>,
    path: Option<PathBuf>,
    inner: Db,

    session_cache: SessionStore,
    tracked_users_cache: Arc<DashSet<OwnedUserId>>,
    users_for_key_query_cache: Arc<DashSet<OwnedUserId>>,

    account: Tree,
    private_identity: Tree,

    olm_hashes: Tree,
    sessions: Tree,
    inbound_group_sessions: Tree,
    outbound_group_sessions: Tree,

    outgoing_secret_requests: Tree,
    unsent_secret_requests: Tree,
    secret_requests_by_info: Tree,

    devices: Tree,
    identities: Tree,

    tracked_users: Tree,
}

impl std::fmt::Debug for SledCryptoStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(path) = &self.path {
            f.debug_struct("SledCryptoStore").field("path", &path).finish()
        } else {
            f.debug_struct("SledCryptoStore").field("path", &"memory store").finish()
        }
    }
}

impl SledCryptoStore {
    /// Open the sled-based crypto store at the given path using the given
    /// passphrase to encrypt private data.
    pub fn open_with_passphrase(
        path: impl AsRef<Path>,
        passphrase: Option<&str>,
    ) -> Result<Self, OpenStoreError> {
        let path = path.as_ref().join("matrix-sdk-crypto");
        let db =
            Config::new().temporary(false).path(&path).open().map_err(CryptoStoreError::backend)?;

        let store_cipher = passphrase
            .map(|p| Self::get_or_create_store_cipher(p, &db))
            .transpose()?
            .map(Into::into);

        SledCryptoStore::open_helper(db, Some(path), store_cipher)
    }

    /// Create a sled-based crypto store using the given sled database.
    /// The given passphrase will be used to encrypt private data.
    pub fn open_with_database(db: Db, passphrase: Option<&str>) -> Result<Self, OpenStoreError> {
        let store_cipher = passphrase
            .map(|p| Self::get_or_create_store_cipher(p, &db))
            .transpose()?
            .map(Into::into);

        SledCryptoStore::open_helper(db, None, store_cipher)
    }

    fn get_account_info(&self) -> Option<AccountInfo> {
        self.account_info.read().unwrap().clone()
    }

    fn serialize_value(&self, event: &impl Serialize) -> Result<Vec<u8>, CryptoStoreError> {
        if let Some(key) = &self.store_cipher {
            key.encrypt_value(event).map_err(CryptoStoreError::backend)
        } else {
            Ok(serde_json::to_vec(event)?)
        }
    }

    fn deserialize_value<T: DeserializeOwned>(&self, event: &[u8]) -> Result<T, CryptoStoreError> {
        if let Some(key) = &self.store_cipher {
            key.decrypt_value(event).map_err(CryptoStoreError::backend)
        } else {
            Ok(serde_json::from_slice(event)?)
        }
    }

    fn encode_key<T: EncodeKey>(&self, table_name: &str, key: T) -> Vec<u8> {
        if let Some(store_cipher) = &self.store_cipher {
            key.encode_secure(table_name, store_cipher).to_vec()
        } else {
            key.encode()
        }
    }

    async fn reset_backup_state(&self) -> Result<()> {
        let mut pickles: Vec<(IVec, PickledInboundGroupSession)> = self
            .inbound_group_sessions
            .iter()
            .map(|p| {
                let item = p.map_err(CryptoStoreError::backend)?;
                Ok((item.0, self.deserialize_value(&item.1)?))
            })
            .collect::<Result<_>>()?;

        for (_, pickle) in &mut pickles {
            pickle.backed_up = false;
        }

        let ret: Result<(), TransactionError<CryptoStoreError>> =
            self.inbound_group_sessions.transaction(|inbound_sessions| {
                for (key, pickle) in &pickles {
                    inbound_sessions.insert(
                        key,
                        self.serialize_value(pickle)
                            .map_err(ConflictableTransactionError::Abort)?,
                    )?;
                }

                Ok(())
            });

        ret.map_err(CryptoStoreError::backend)?;

        self.inner.flush_async().await.map_err(CryptoStoreError::backend)?;

        Ok(())
    }

    fn upgrade(&self) -> Result<()> {
        let version = self
            .inner
            .get("store_version")
            .map_err(CryptoStoreError::backend)?
            .map(|v| {
                let (version_bytes, _) = v.split_at(std::mem::size_of::<u8>());
                u8::from_be_bytes(version_bytes.try_into().unwrap_or_default())
            })
            .unwrap_or(DATABASE_VERSION);

        if version != DATABASE_VERSION {
            debug!(version, new_version = DATABASE_VERSION, "Upgrading the Sled crypto store");
        }

        if version <= 3 {
            return Err(CryptoStoreError::UnsupportedDatabaseVersion(
                version.into(),
                DATABASE_VERSION.into(),
            ));
        }

        if version <= 4 {
            // Room key requests are not that important, if they are needed they
            // will be sent out again. So let's drop all of them since we
            // removed the `sender_key` from the hash key.
            self.outgoing_secret_requests.clear().map_err(CryptoStoreError::backend)?;
            self.unsent_secret_requests.clear().map_err(CryptoStoreError::backend)?;
            self.secret_requests_by_info.clear().map_err(CryptoStoreError::backend)?;
        }

        self.inner
            .insert("store_version", DATABASE_VERSION.to_be_bytes().as_ref())
            .map_err(CryptoStoreError::backend)?;
        self.inner.flush().map_err(CryptoStoreError::backend)?;

        Ok(())
    }

    fn get_or_create_store_cipher(passphrase: &str, database: &Db) -> Result<StoreCipher> {
        let cipher = if let Some(key) =
            database.get("store_cipher".encode()).map_err(CryptoStoreError::backend)?
        {
            StoreCipher::import(passphrase, &key).map_err(|_| CryptoStoreError::UnpicklingError)?
        } else {
            let cipher = StoreCipher::new().map_err(CryptoStoreError::backend)?;
            #[cfg(not(test))]
            let export = cipher.export(passphrase);
            #[cfg(test)]
            let export = cipher._insecure_export_fast_for_testing(passphrase);
            database
                .insert("store_cipher".encode(), export.map_err(CryptoStoreError::backend)?)
                .map_err(CryptoStoreError::backend)?;
            cipher
        };

        Ok(cipher)
    }

    pub(crate) fn open_helper(
        db: Db,
        path: Option<PathBuf>,
        store_cipher: Option<Arc<StoreCipher>>,
    ) -> Result<Self, OpenStoreError> {
        let account = db.open_tree("account")?;
        let private_identity = db.open_tree("private_identity")?;

        let sessions = db.open_tree("session")?;
        let inbound_group_sessions = db.open_tree("inbound_group_sessions")?;

        let outbound_group_sessions = db.open_tree("outbound_group_sessions")?;

        let tracked_users = db.open_tree("tracked_users")?;
        let olm_hashes = db.open_tree("olm_hashes")?;

        let devices = db.open_tree("devices")?;
        let identities = db.open_tree("identities")?;

        let outgoing_secret_requests = db.open_tree("outgoing_secret_requests")?;
        let unsent_secret_requests = db.open_tree("unsent_secret_requests")?;
        let secret_requests_by_info = db.open_tree("secret_requests_by_info")?;

        let session_cache = SessionStore::new();

        let database = Self {
            account_info: RwLock::new(None).into(),
            path,
            inner: db,
            store_cipher,
            account,
            private_identity,
            sessions,
            session_cache,
            tracked_users_cache: DashSet::new().into(),
            users_for_key_query_cache: DashSet::new().into(),
            inbound_group_sessions,
            outbound_group_sessions,
            outgoing_secret_requests,
            unsent_secret_requests,
            secret_requests_by_info,
            devices,
            tracked_users,
            olm_hashes,
            identities,
        };

        database.upgrade()?;

        Ok(database)
    }

    async fn load_tracked_users(&self) -> Result<()> {
        for value in &self.tracked_users {
            let (_, user) = value.map_err(CryptoStoreError::backend)?;
            let user: TrackedUser = self.deserialize_value(&user)?;

            self.tracked_users_cache.insert(user.user_id.to_owned());

            if user.dirty {
                self.users_for_key_query_cache.insert(user.user_id);
            }
        }

        Ok(())
    }

    async fn load_outbound_group_session(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>> {
        let account_info = self.get_account_info().ok_or(CryptoStoreError::AccountUnset)?;

        self.outbound_group_sessions
            .get(self.encode_key(OUTBOUND_GROUP_TABLE_NAME, room_id))
            .map_err(CryptoStoreError::backend)?
            .map(|p| self.deserialize_value(&p))
            .transpose()?
            .map(|p| {
                Ok(OutboundGroupSession::from_pickle(
                    account_info.device_id,
                    account_info.identity_keys,
                    p,
                )?)
            })
            .transpose()
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
        let account_pickle = if let Some(account) = changes.account {
            let account_info = AccountInfo {
                user_id: account.user_id.clone(),
                device_id: account.device_id.clone(),
                identity_keys: account.identity_keys.clone(),
            };

            *self.account_info.write().unwrap() = Some(account_info);
            Some(account.pickle().await)
        } else {
            None
        };

        let private_identity_pickle =
            if let Some(i) = changes.private_identity { Some(i.pickle().await?) } else { None };

        let recovery_key_pickle = changes.recovery_key;

        let device_changes = changes.devices;
        let mut session_changes = HashMap::new();

        for session in changes.sessions {
            let pickle = session.pickle().await;
            let key = self.encode_key(SESSIONS_TABLE_NAME, &session);

            self.session_cache.add(session).await;
            session_changes.insert(key, pickle);
        }

        let mut inbound_session_changes = HashMap::new();

        for session in changes.inbound_group_sessions {
            let key = self.encode_key(INBOUND_GROUP_TABLE_NAME, &session);
            let pickle = session.pickle().await;

            inbound_session_changes.insert(key, pickle);
        }

        let mut outbound_session_changes = HashMap::new();

        for session in changes.outbound_group_sessions {
            let key = self.encode_key(OUTBOUND_GROUP_TABLE_NAME, &session);
            let pickle = session.pickle().await;

            outbound_session_changes.insert(key, pickle);
        }

        let identity_changes = changes.identities;
        let olm_hashes = changes.message_hashes;
        let key_requests = changes.key_requests;
        let backup_version = changes.backup_version;

        let ret: Result<(), TransactionError<CryptoStoreError>> = (
            &self.account,
            &self.private_identity,
            &self.devices,
            &self.identities,
            &self.sessions,
            &self.inbound_group_sessions,
            &self.outbound_group_sessions,
            &self.olm_hashes,
            &self.outgoing_secret_requests,
            &self.unsent_secret_requests,
            &self.secret_requests_by_info,
        )
            .transaction(
                |(
                    account,
                    private_identity,
                    devices,
                    identities,
                    sessions,
                    inbound_sessions,
                    outbound_sessions,
                    hashes,
                    outgoing_secret_requests,
                    unsent_secret_requests,
                    secret_requests_by_info,
                )| {
                    if let Some(a) = &account_pickle {
                        account.insert(
                            "account".encode(),
                            self.serialize_value(a).map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    if let Some(i) = &private_identity_pickle {
                        private_identity.insert(
                            "identity".encode(),
                            self.serialize_value(&i)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    if let Some(r) = &recovery_key_pickle {
                        account.insert(
                            "recovery_key_v1".encode(),
                            self.serialize_value(r).map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    if let Some(b) = &backup_version {
                        account.insert(
                            "backup_version_v1".encode(),
                            self.serialize_value(b).map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for device in device_changes.new.iter().chain(&device_changes.changed) {
                        let key = self.encode_key(DEVICE_TABLE_NAME, device);
                        let device = self
                            .serialize_value(&device)
                            .map_err(ConflictableTransactionError::Abort)?;
                        devices.insert(key, device)?;
                    }

                    for device in &device_changes.deleted {
                        let key = self.encode_key(DEVICE_TABLE_NAME, device);
                        devices.remove(key)?;
                    }

                    for identity in identity_changes.changed.iter().chain(&identity_changes.new) {
                        identities.insert(
                            self.encode_key(IDENTITIES_TABLE_NAME, identity.user_id()),
                            self.serialize_value(&identity)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (key, session) in &session_changes {
                        sessions.insert(
                            key.as_slice(),
                            self.serialize_value(&session)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (key, session) in &inbound_session_changes {
                        inbound_sessions.insert(
                            key.as_slice(),
                            self.serialize_value(&session)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (key, session) in &outbound_session_changes {
                        outbound_sessions.insert(
                            key.as_slice(),
                            self.serialize_value(&session)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for hash in &olm_hashes {
                        hashes.insert(
                            serde_json::to_vec(hash)
                                .map_err(CryptoStoreError::Serialization)
                                .map_err(ConflictableTransactionError::Abort)?,
                            &[0],
                        )?;
                    }

                    for key_request in &key_requests {
                        secret_requests_by_info.insert(
                            self.encode_key(SECRET_REQUEST_BY_INFO_TABLE, &key_request.info),
                            key_request.request_id.encode(),
                        )?;

                        let key_request_id = key_request.request_id.encode();

                        if key_request.sent_out {
                            unsent_secret_requests.remove(key_request_id.clone())?;
                            outgoing_secret_requests.insert(
                                key_request_id,
                                self.serialize_value(&key_request)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        } else {
                            outgoing_secret_requests.remove(key_request_id.clone())?;
                            unsent_secret_requests.insert(
                                key_request_id,
                                self.serialize_value(&key_request)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    Ok(())
                },
            );

        ret.map_err(CryptoStoreError::backend)?;
        self.inner.flush().map_err(CryptoStoreError::backend)?;

        Ok(())
    }

    async fn get_outgoing_key_request_helper(&self, id: &[u8]) -> Result<Option<GossipRequest>> {
        let request = self
            .outgoing_secret_requests
            .get(id)
            .map_err(CryptoStoreError::backend)?
            .map(|r| self.deserialize_value(&r))
            .transpose()?;

        let request = if request.is_none() {
            self.unsent_secret_requests
                .get(id)
                .map_err(CryptoStoreError::backend)?
                .map(|r| self.deserialize_value(&r))
                .transpose()?
        } else {
            request
        };

        Ok(request)
    }

    /// Save a batch of tracked users.
    ///
    /// # Arguments
    ///
    /// * `tracked_users` - A list of tuples. The first element of the tuple is
    /// the user ID, the second element is if the user should be considered to
    /// be dirty.
    pub async fn save_tracked_users(
        &self,
        tracked_users: &[(&UserId, bool)],
    ) -> Result<(), CryptoStoreError> {
        let users: Vec<TrackedUser> = tracked_users
            .iter()
            .map(|(u, d)| TrackedUser { user_id: (*u).into(), dirty: *d })
            .collect();

        let mut batch = Batch::default();

        for user in users {
            batch.insert(
                self.encode_key(TRACKED_USERS_TABLE, user.user_id.as_str()),
                self.serialize_value(&user)?,
            );
        }

        self.tracked_users.apply_batch(batch).map_err(CryptoStoreError::backend)
    }
}

#[async_trait]
impl CryptoStore for SledCryptoStore {
    async fn load_account(&self) -> Result<Option<ReadOnlyAccount>> {
        if let Some(pickle) =
            self.account.get("account".encode()).map_err(CryptoStoreError::backend)?
        {
            let pickle = self.deserialize_value(&pickle)?;

            self.load_tracked_users().await?;
            let account = ReadOnlyAccount::from_pickle(pickle)?;

            let account_info = AccountInfo {
                user_id: account.user_id.clone(),
                device_id: account.device_id.clone(),
                identity_keys: account.identity_keys.clone(),
            };

            *self.account_info.write().unwrap() = Some(account_info);

            Ok(Some(account))
        } else {
            Ok(None)
        }
    }

    async fn save_account(&self, account: ReadOnlyAccount) -> Result<()> {
        self.save_changes(Changes { account: Some(account), ..Default::default() }).await
    }

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>> {
        if let Some(i) =
            self.private_identity.get("identity".encode()).map_err(CryptoStoreError::backend)?
        {
            let pickle = self.deserialize_value(&i)?;
            Ok(Some(
                PrivateCrossSigningIdentity::from_pickle(pickle)
                    .await
                    .map_err(|_| CryptoStoreError::UnpicklingError)?,
            ))
        } else {
            Ok(None)
        }
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
        self.save_changes(changes).await
    }

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        let account_info = self.get_account_info().ok_or(CryptoStoreError::AccountUnset)?;

        if self.session_cache.get(sender_key).is_none() {
            let sessions: Result<Vec<Session>> = self
                .sessions
                .scan_prefix(self.encode_key(SESSIONS_TABLE_NAME, sender_key))
                .map(|s| self.deserialize_value(&s.map_err(CryptoStoreError::backend)?.1))
                .map(|p| {
                    Ok(Session::from_pickle(
                        account_info.user_id.clone(),
                        account_info.device_id.clone(),
                        account_info.identity_keys.clone(),
                        p?,
                    ))
                })
                .collect();

            self.session_cache.set_for_sender(sender_key, sessions?);
        }

        Ok(self.session_cache.get(sender_key))
    }

    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        let key = self.encode_key(INBOUND_GROUP_TABLE_NAME, (room_id, sender_key, session_id));
        let pickle = self
            .inbound_group_sessions
            .get(&key)
            .map_err(CryptoStoreError::backend)?
            .map(|p| self.deserialize_value(&p));

        if let Some(pickle) = pickle {
            Ok(Some(InboundGroupSession::from_pickle(pickle?)?))
        } else {
            Ok(None)
        }
    }

    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        let pickles: Result<Vec<PickledInboundGroupSession>> = self
            .inbound_group_sessions
            .iter()
            .map(|p| self.deserialize_value(&p.map_err(CryptoStoreError::backend)?.1))
            .collect();

        Ok(pickles?.into_iter().filter_map(|p| InboundGroupSession::from_pickle(p).ok()).collect())
    }

    async fn inbound_group_session_counts(&self) -> Result<RoomKeyCounts> {
        let pickles: Vec<PickledInboundGroupSession> = self
            .inbound_group_sessions
            .iter()
            .map(|p| {
                let item = p.map_err(CryptoStoreError::backend)?;
                self.deserialize_value(&item.1)
            })
            .collect::<Result<_>>()?;

        let total = pickles.len();
        let backed_up = pickles.into_iter().filter(|p| p.backed_up).count();

        Ok(RoomKeyCounts { total, backed_up })
    }

    async fn inbound_group_sessions_for_backup(
        &self,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>> {
        let pickles: Vec<InboundGroupSession> = self
            .inbound_group_sessions
            .iter()
            .map(|p| {
                let item = p.map_err(CryptoStoreError::backend)?;
                self.deserialize_value(&item.1)
            })
            .filter_map(|p: Result<PickledInboundGroupSession, CryptoStoreError>| match p {
                Ok(p) => {
                    if !p.backed_up {
                        Some(InboundGroupSession::from_pickle(p).map_err(CryptoStoreError::from))
                    } else {
                        None
                    }
                }

                Err(p) => Some(Err(p)),
            })
            .take(limit)
            .collect::<Result<_>>()?;

        Ok(pickles)
    }

    async fn reset_backup_state(&self) -> Result<()> {
        self.reset_backup_state().await
    }

    async fn get_outbound_group_sessions(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>> {
        self.load_outbound_group_session(room_id).await
    }

    fn is_user_tracked(&self, user_id: &UserId) -> bool {
        self.tracked_users_cache.contains(user_id)
    }

    fn has_users_for_key_query(&self) -> bool {
        !self.users_for_key_query_cache.is_empty()
    }

    fn users_for_key_query(&self) -> HashSet<OwnedUserId> {
        self.users_for_key_query_cache.iter().map(|u| u.clone()).collect()
    }

    fn tracked_users(&self) -> HashSet<OwnedUserId> {
        self.tracked_users_cache.to_owned().iter().map(|u| u.clone()).collect()
    }

    async fn update_tracked_user(&self, user: &UserId, dirty: bool) -> Result<bool> {
        let already_added = self.tracked_users_cache.insert(user.to_owned());

        if dirty {
            self.users_for_key_query_cache.insert(user.to_owned());
        } else {
            self.users_for_key_query_cache.remove(user);
        }

        self.save_tracked_users(&[(user, dirty)]).await?;

        Ok(already_added)
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>> {
        let key = self.encode_key(DEVICE_TABLE_NAME, (user_id, device_id));

        Ok(self
            .devices
            .get(key)
            .map_err(CryptoStoreError::backend)?
            .map(|d| self.deserialize_value(&d))
            .transpose()?)
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, ReadOnlyDevice>> {
        let key = self.encode_key(DEVICE_TABLE_NAME, user_id);
        self.devices
            .scan_prefix(key)
            .map(|d| self.deserialize_value(&d.map_err(CryptoStoreError::backend)?.1))
            .map(|d| {
                let d: ReadOnlyDevice = d?;
                Ok((d.device_id().to_owned(), d))
            })
            .collect()
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<ReadOnlyUserIdentities>> {
        let key = self.encode_key(IDENTITIES_TABLE_NAME, user_id);

        Ok(self
            .identities
            .get(key)
            .map_err(CryptoStoreError::backend)?
            .map(|i| self.deserialize_value(&i))
            .transpose()?)
    }

    async fn is_message_known(
        &self,
        message_hash: &matrix_sdk_crypto::olm::OlmMessageHash,
    ) -> Result<bool> {
        Ok(self
            .olm_hashes
            .contains_key(serde_json::to_vec(message_hash)?)
            .map_err(CryptoStoreError::backend)?)
    }

    async fn get_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> Result<Option<GossipRequest>> {
        let request_id = request_id.encode();

        self.get_outgoing_key_request_helper(&request_id).await
    }

    async fn get_secret_request_by_info(
        &self,
        key_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>> {
        let id = self
            .secret_requests_by_info
            .get(self.encode_key(SECRET_REQUEST_BY_INFO_TABLE, key_info))
            .map_err(CryptoStoreError::backend)?;

        if let Some(id) = id {
            self.get_outgoing_key_request_helper(&id).await
        } else {
            Ok(None)
        }
    }

    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>> {
        let requests: Result<Vec<GossipRequest>> = self
            .unsent_secret_requests
            .iter()
            .map(|i| self.deserialize_value(&i.map_err(CryptoStoreError::backend)?.1))
            .collect();

        requests
    }

    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> Result<()> {
        let ret: Result<(), TransactionError<CryptoStoreError>> = (
            &self.outgoing_secret_requests,
            &self.unsent_secret_requests,
            &self.secret_requests_by_info,
        )
            .transaction(
                |(outgoing_key_requests, unsent_key_requests, key_requests_by_info)| {
                    let sent_request: Option<GossipRequest> = outgoing_key_requests
                        .remove(request_id.encode())?
                        .map(|r| self.deserialize_value(&r))
                        .transpose()
                        .map_err(ConflictableTransactionError::Abort)?;

                    let unsent_request: Option<GossipRequest> = unsent_key_requests
                        .remove(request_id.encode())?
                        .map(|r| self.deserialize_value(&r))
                        .transpose()
                        .map_err(ConflictableTransactionError::Abort)?;

                    if let Some(request) = sent_request {
                        key_requests_by_info
                            .remove(self.encode_key(SECRET_REQUEST_BY_INFO_TABLE, &request.info))?;
                    }

                    if let Some(request) = unsent_request {
                        key_requests_by_info
                            .remove(self.encode_key(SECRET_REQUEST_BY_INFO_TABLE, &request.info))?;
                    }

                    Ok(())
                },
            );

        ret.map_err(CryptoStoreError::backend)?;
        self.inner.flush_async().await.map_err(CryptoStoreError::backend)?;

        Ok(())
    }

    async fn load_backup_keys(&self) -> Result<BackupKeys> {
        let key = {
            let backup_version = self
                .account
                .get("backup_version_v1".encode())
                .map_err(CryptoStoreError::backend)?
                .map(|v| self.deserialize_value(&v))
                .transpose()?;

            let recovery_key = {
                self.account
                    .get("recovery_key_v1".encode())
                    .map_err(CryptoStoreError::backend)?
                    .map(|p| self.deserialize_value(&p))
                    .transpose()?
            };

            BackupKeys { backup_version, recovery_key }
        };

        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_crypto::cryptostore_integration_tests;
    use once_cell::sync::Lazy;
    use tempfile::{tempdir, TempDir};

    use super::SledCryptoStore;

    static TMP_DIR: Lazy<TempDir> = Lazy::new(|| tempdir().unwrap());

    async fn get_store(name: &str, passphrase: Option<&str>) -> SledCryptoStore {
        let tmpdir_path = TMP_DIR.path().join(name);

        let store =
            SledCryptoStore::open_with_passphrase(tmpdir_path.to_str().unwrap(), passphrase)
                .expect("Can't create a passphrase protected store");

        store
    }

    cryptostore_integration_tests!();
}

#[cfg(test)]
mod encrypted_tests {
    use matrix_sdk_crypto::cryptostore_integration_tests;
    use once_cell::sync::Lazy;
    use tempfile::{tempdir, TempDir};

    use super::SledCryptoStore;

    static TMP_DIR: Lazy<TempDir> = Lazy::new(|| tempdir().unwrap());

    async fn get_store(name: &str, passphrase: Option<&str>) -> SledCryptoStore {
        let tmpdir_path = TMP_DIR.path().join(name);
        let pass = passphrase.unwrap_or("default_test_password");

        let store =
            SledCryptoStore::open_with_passphrase(tmpdir_path.to_str().unwrap(), Some(pass))
                .expect("Can't create a passphrase protected store");

        store
    }
    cryptostore_integration_tests!();
}
