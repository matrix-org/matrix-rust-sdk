// Copyright 2022 The Matrix.org Foundation C.I.C.
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
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use deadpool_sqlite::{Object as SqliteConn, Pool as SqlitePool, Runtime};
use matrix_sdk_common::locks::Mutex;
use matrix_sdk_crypto::{
    olm::{
        IdentityKeys, InboundGroupSession, OutboundGroupSession, PickledInboundGroupSession,
        PrivateCrossSigningIdentity, Session,
    },
    store::{
        caches::SessionStore, BackupKeys, Changes, CryptoStore, CryptoStoreError,
        Result as StoreResult, RoomKeyCounts,
    },
    GossipRequest, ReadOnlyAccount, ReadOnlyDevice, ReadOnlyUserIdentities, SecretInfo,
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, RoomId, TransactionId, UserId};
use rusqlite::OptionalExtension;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::fs;
use tracing::{debug, error};

use crate::{
    get_or_create_store_cipher,
    utils::{Key, SqliteObjectExt},
    OpenStoreError, SqliteObjectStoreExt,
};

#[derive(Clone, Debug)]
pub struct AccountInfo {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceId>,
    identity_keys: Arc<IdentityKeys>,
}

enum Error {
    Crypto(CryptoStoreError),
    Sqlite(rusqlite::Error),
    Pool(deadpool_sqlite::PoolError),
}

impl From<CryptoStoreError> for Error {
    fn from(value: CryptoStoreError) -> Self {
        Self::Crypto(value)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<deadpool_sqlite::PoolError> for Error {
    fn from(value: deadpool_sqlite::PoolError) -> Self {
        Self::Pool(value)
    }
}

impl From<Error> for CryptoStoreError {
    fn from(value: Error) -> Self {
        match value {
            Error::Crypto(c) => c,
            Error::Sqlite(b) => CryptoStoreError::backend(b),
            Error::Pool(b) => CryptoStoreError::backend(b),
        }
    }
}

type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Serialize, Deserialize)]
struct TrackedUser {
    user_id: OwnedUserId,
    dirty: bool,
}

/// A sqlite based cryptostore.
#[derive(Clone)]
pub struct SqliteCryptoStore {
    store_cipher: Option<Arc<StoreCipher>>,
    path: Option<PathBuf>,
    pool: SqlitePool,

    // Extra caches
    session_cache: SessionStore,
    account_info: Arc<RwLock<Option<AccountInfo>>>,
    // tracked_users_cache: Arc<DashSet<OwnedUserId>>,
    // users_for_key_query_cache: Arc<DashSet<OwnedUserId>>,
}

impl std::fmt::Debug for SqliteCryptoStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(path) = &self.path {
            f.debug_struct("SledCryptoStore").field("path", &path).finish()
        } else {
            f.debug_struct("SledCryptoStore").field("path", &"memory store").finish()
        }
    }
}

impl SqliteCryptoStore {
    /// Open the sqlite-based crypto store at the given path using the given
    /// passphrase to encrypt private data.
    pub async fn open(
        path: impl AsRef<Path>,
        passphrase: Option<&str>,
    ) -> Result<Self, OpenStoreError> {
        let path = path.as_ref();
        fs::create_dir_all(path).await.map_err(CryptoStoreError::from)?;
        let cfg = deadpool_sqlite::Config::new(path.join("matrix-sdk-crypto.sqlite3"));
        let pool = cfg.create_pool(Runtime::Tokio1)?;

        Self::open_with_pool(pool, passphrase).await
    }

    /// Create a sqlite-based crypto store using the given sqlite database pool.
    /// The given passphrase will be used to encrypt private data.
    pub async fn open_with_pool(
        pool: SqlitePool,
        passphrase: Option<&str>,
    ) -> Result<Self, OpenStoreError> {
        let conn = pool.get().await.map_err(CryptoStoreError::backend)?;
        run_migrations(&conn).await?;
        let store_cipher = match passphrase {
            Some(p) => Some(Arc::new(get_or_create_store_cipher(p, &conn).await?)),
            None => None,
        };

        Ok(SqliteCryptoStore {
            store_cipher,
            path: None,
            pool,
            account_info: Arc::new(RwLock::new(None)),
            session_cache: SessionStore::new(),
        })
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

    fn encode_key(&self, table_name: &str, key: impl AsRef<[u8]>) -> Key {
        let bytes = key.as_ref();
        if let Some(store_cipher) = &self.store_cipher {
            Key::Hashed(store_cipher.hash_key(table_name, bytes))
        } else {
            Key::Plain(bytes.to_owned())
        }
    }

    fn get_account_info(&self) -> Option<AccountInfo> {
        self.account_info.read().unwrap().clone()
    }

    async fn acquire(&self) -> Result<deadpool_sqlite::Object> {
        Ok(self.pool.get().await?)
    }

    /* async fn reset_backup_state(&self) -> Result<()> {
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
    } */
}

const DATABASE_VERSION: u8 = 1;

async fn run_migrations(conn: &SqliteConn) -> Result<(), CryptoStoreError> {
    let metadata_exists = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'metadata'",
            (),
            |row| row.get::<_, u32>(0),
        )
        .await
        .map_err(CryptoStoreError::backend)?
        > 0;

    let version = if metadata_exists {
        match conn.get_metadata("version").await?.as_deref() {
            Some([v]) => *v,
            Some(_) => {
                error!("version database field has multiple bytes");
                return Ok(());
            }
            None => {
                error!("version database field is missing");
                return Ok(());
            }
        }
    } else {
        0
    };

    if version == 0 {
        debug!("Creating database");
    } else if version < DATABASE_VERSION {
        debug!(version, new_version = DATABASE_VERSION, "Upgrading database");
    }

    if version < 1 {
        conn.with_transaction(|txn| txn.execute_batch(include_str!("../migrations/001_init.sql")))
            .await
            .map_err(CryptoStoreError::backend)?;
    }

    Ok(())
}

trait SqliteConnectionExt {
    fn set_account_data(&self, key: &str, value: &[u8]) -> rusqlite::Result<()>;
    fn set_session(
        &self,
        session_id: &[u8],
        sender_key: &[u8],
        session_data: &[u8],
    ) -> rusqlite::Result<()>;
}

impl SqliteConnectionExt for rusqlite::Connection {
    fn set_account_data(&self, key: &str, value: &[u8]) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO account_data VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = ?2",
            (key, value),
        )?;
        Ok(())
    }

    fn set_session(
        &self,
        session_id: &[u8],
        sender_key: &[u8],
        session_data: &[u8],
    ) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO session \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT (session_id) DO UPDATE SET session_data = ?3",
            (session_id, sender_key, session_data),
        )?;
        Ok(())
    }
}

#[async_trait]
trait SqliteObjectCryptoStoreExt: SqliteObjectExt {
    async fn get_account_data(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let key = key.to_owned();
        Ok(self
            .query_row("SELECT value FROM account_data WHERE key = ?", (key,), |row| row.get(0))
            .await
            .optional()?)
    }

    async fn set_account_data(&self, key: &str, value: Vec<u8>) -> Result<()>;

    async fn get_sessions_for_sender_key(&self, sender_key: Key) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .prepare("SELECT session_data FROM sessions WHERE sender_key = ?", |mut stmt| {
                stmt.query((sender_key,))?.mapped(|row| row.get(0)).collect()
            })
            .await?)
    }
}

#[async_trait]
impl SqliteObjectCryptoStoreExt for deadpool_sqlite::Object {
    async fn set_account_data(&self, key: &str, value: Vec<u8>) -> Result<()> {
        let key = key.to_owned();
        self.interact(move |conn| conn.set_account_data(&key, &value))
            .await
            .unwrap()
            .map_err(CryptoStoreError::backend)?;

        Ok(())
    }
}

#[async_trait]
impl CryptoStore for SqliteCryptoStore {
    async fn load_account(&self) -> StoreResult<Option<ReadOnlyAccount>> {
        let conn = self.acquire().await?;
        if let Some(pickle) = conn.get_account_data("account").await? {
            let pickle = self.deserialize_value(&pickle)?;

            //self.load_tracked_users().await?;
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

    async fn save_account(&self, account: ReadOnlyAccount) -> StoreResult<()> {
        let account_info = AccountInfo {
            user_id: account.user_id.clone(),
            device_id: account.device_id.clone(),
            identity_keys: account.identity_keys.clone(),
        };
        *self.account_info.write().unwrap() = Some(account_info);

        let pickled_account = account.pickle().await;
        let serialized_account = self.serialize_value(&pickled_account)?;
        self.acquire().await?.set_account_data("account", serialized_account).await?;
        Ok(())
    }

    async fn load_identity(&self) -> StoreResult<Option<PrivateCrossSigningIdentity>> {
        let conn = self.acquire().await?;
        if let Some(i) = conn.get_account_data("identity").await? {
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

    async fn save_changes(&self, changes: Changes) -> StoreResult<()> {
        let pickled_account = if let Some(account) = changes.account {
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

        let mut session_changes = Vec::new();
        for session in changes.sessions {
            let session_id = self.encode_key("session", session.session_id());
            let sender_key = self.encode_key("session", session.sender_key().as_bytes());
            let pickle = session.pickle().await;
            session_changes.push((session_id, sender_key, pickle));

            self.session_cache.add(session).await;
        }

        let mut inbound_session_changes = Vec::new();
        for session in changes.inbound_group_sessions {
            let room_id = self.encode_key("inbound_group_session", session.room_id().as_bytes());
            let session_id = self.encode_key("inbound_group_session", session.session_id());
            let pickle = session.pickle().await;
            inbound_session_changes.push((room_id, session_id, pickle));
        }

        let mut outbound_session_changes = Vec::new();
        for session in changes.outbound_group_sessions {
            let room_id = self.encode_key("outbound_group_session", session.room_id().as_bytes());
            let pickle = session.pickle().await;
            outbound_session_changes.push((room_id, pickle));
        }

        let this = self.clone();
        self.acquire()
            .await?
            .with_transaction(move |txn| {
                if let Some(pickled_account) = pickled_account {
                    let serialized_account = this.serialize_value(&pickled_account)?;
                    txn.set_account_data("account", &serialized_account)?;
                }

                /* if let Some(i) = &private_identity_pickle {
                    private_identity.insert(
                        "identity".encode(),
                        self.serialize_value(&i).map_err(ConflictableTransactionError::Abort)?,
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
                } */

                for (session_id, sender_key, pickle) in &session_changes {
                    let serialized_session = this.serialize_value(&pickle)?;
                    txn.set_session(session_id, sender_key, &serialized_session)?;
                }

                /* for (key, session) in &inbound_session_changes {
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
                } */

                Ok::<_, Error>(())
            })
            .await?;

        Ok(())
    }

    async fn get_sessions(
        &self,
        sender_key: &str,
    ) -> StoreResult<Option<Arc<Mutex<Vec<Session>>>>> {
        let account_info = self.get_account_info().ok_or(CryptoStoreError::AccountUnset)?;

        if self.session_cache.get(sender_key).is_none() {
            let conn = self.acquire().await?;
            let sessions = conn
                .get_sessions_for_sender_key(self.encode_key("sessions", sender_key))
                .await?
                .into_iter()
                .map(|bytes| {
                    let pickle = self.deserialize_value(&bytes)?;
                    Ok(Session::from_pickle(
                        account_info.user_id.clone(),
                        account_info.device_id.clone(),
                        account_info.identity_keys.clone(),
                        pickle,
                    ))
                })
                .collect::<Result<_>>()?;

            self.session_cache.set_for_sender(sender_key, sessions);
        }

        Ok(self.session_cache.get(sender_key))
    }

    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> StoreResult<Option<InboundGroupSession>> {
        /* let key = self.encode_key(INBOUND_GROUP_TABLE_NAME, (room_id, session_id));
        let pickle = self
            .inbound_group_sessions
            .get(key)
            .map_err(CryptoStoreError::backend)?
            .map(|p| self.deserialize_value(&p));

        if let Some(pickle) = pickle {
            Ok(Some(InboundGroupSession::from_pickle(pickle?)?))
        } else {
            Ok(None)
        } */
        todo!()
    }

    async fn get_inbound_group_sessions(&self) -> StoreResult<Vec<InboundGroupSession>> {
        /* let pickles: Result<Vec<PickledInboundGroupSession>> = self
            .inbound_group_sessions
            .iter()
            .map(|p| self.deserialize_value(&p.map_err(CryptoStoreError::backend)?.1))
            .collect();

        Ok(pickles?.into_iter().filter_map(|p| InboundGroupSession::from_pickle(p).ok()).collect()) */
        todo!()
    }

    async fn inbound_group_session_counts(&self) -> StoreResult<RoomKeyCounts> {
        /* let pickles: Vec<PickledInboundGroupSession> = self
            .inbound_group_sessions
            .iter()
            .map(|p| {
                let item = p.map_err(CryptoStoreError::backend)?;
                self.deserialize_value(&item.1)
            })
            .collect::<Result<_>>()?;

        let total = pickles.len();
        let backed_up = pickles.into_iter().filter(|p| p.backed_up).count();

        Ok(RoomKeyCounts { total, backed_up }) */
        todo!()
    }

    async fn inbound_group_sessions_for_backup(
        &self,
        limit: usize,
    ) -> StoreResult<Vec<InboundGroupSession>> {
        /* let pickles: Vec<InboundGroupSession> = self
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

        Ok(pickles) */
        todo!()
    }

    async fn reset_backup_state(&self) -> StoreResult<()> {
        todo!()
    }

    async fn get_outbound_group_sessions(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<Option<OutboundGroupSession>> {
        todo!()
    }

    fn is_user_tracked(&self, user_id: &UserId) -> bool {
        /* self.tracked_users_cache.contains(user_id) */
        todo!()
    }

    fn has_users_for_key_query(&self) -> bool {
        /* !self.users_for_key_query_cache.is_empty() */
        todo!()
    }

    fn users_for_key_query(&self) -> HashSet<OwnedUserId> {
        /* self.users_for_key_query_cache.iter().map(|u| u.clone()).collect() */
        todo!()
    }

    fn tracked_users(&self) -> HashSet<OwnedUserId> {
        /* self.tracked_users_cache.to_owned().iter().map(|u| u.clone()).collect() */
        todo!()
    }

    async fn update_tracked_user(&self, user: &UserId, dirty: bool) -> StoreResult<bool> {
        /* let already_added = self.tracked_users_cache.insert(user.to_owned());

        if dirty {
            self.users_for_key_query_cache.insert(user.to_owned());
        } else {
            self.users_for_key_query_cache.remove(user);
        }

        self.save_tracked_users(&[(user, dirty)]).await?;

        Ok(already_added) */
        todo!()
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> StoreResult<Option<ReadOnlyDevice>> {
        /* let key = self.encode_key(DEVICE_TABLE_NAME, (user_id, device_id));

        Ok(self
            .devices
            .get(key)
            .map_err(CryptoStoreError::backend)?
            .map(|d| self.deserialize_value(&d))
            .transpose()?) */
        todo!()
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> StoreResult<HashMap<OwnedDeviceId, ReadOnlyDevice>> {
        /* let key = self.encode_key(DEVICE_TABLE_NAME, user_id);
        self.devices
            .scan_prefix(key)
            .map(|d| self.deserialize_value(&d.map_err(CryptoStoreError::backend)?.1))
            .map(|d| {
                let d: ReadOnlyDevice = d?;
                Ok((d.device_id().to_owned(), d))
            })
            .collect() */
        todo!()
    }

    async fn get_user_identity(
        &self,
        user_id: &UserId,
    ) -> StoreResult<Option<ReadOnlyUserIdentities>> {
        /* let key = self.encode_key(IDENTITIES_TABLE_NAME, user_id);

        Ok(self
            .identities
            .get(key)
            .map_err(CryptoStoreError::backend)?
            .map(|i| self.deserialize_value(&i))
            .transpose()?) */
        todo!()
    }

    async fn is_message_known(
        &self,
        message_hash: &matrix_sdk_crypto::olm::OlmMessageHash,
    ) -> StoreResult<bool> {
        /* Ok(self
        .olm_hashes
        .contains_key(serde_json::to_vec(message_hash)?)
        .map_err(CryptoStoreError::backend)?) */
        todo!()
    }

    async fn get_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> StoreResult<Option<GossipRequest>> {
        /* let request_id = request_id.encode();

        self.get_outgoing_key_request_helper(&request_id).await */
        todo!()
    }

    async fn get_secret_request_by_info(
        &self,
        key_info: &SecretInfo,
    ) -> StoreResult<Option<GossipRequest>> {
        /* let id = self
            .secret_requests_by_info
            .get(self.encode_key(SECRET_REQUEST_BY_INFO_TABLE, key_info))
            .map_err(CryptoStoreError::backend)?;

        if let Some(id) = id {
            self.get_outgoing_key_request_helper(&id).await
        } else {
            Ok(None)
        } */
        todo!()
    }

    async fn get_unsent_secret_requests(&self) -> StoreResult<Vec<GossipRequest>> {
        /* let requests: Result<Vec<GossipRequest>> = self
            .unsent_secret_requests
            .iter()
            .map(|i| self.deserialize_value(&i.map_err(CryptoStoreError::backend)?.1))
            .collect();

        requests */
        todo!()
    }

    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> StoreResult<()> {
        /* let ret: Result<(), TransactionError<CryptoStoreError>> = (
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

        Ok(()) */
        todo!()
    }

    async fn load_backup_keys(&self) -> StoreResult<BackupKeys> {
        /* let key = {
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

        Ok(key) */
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_crypto::cryptostore_integration_tests;
    use once_cell::sync::Lazy;
    use tempfile::{tempdir, TempDir};

    use super::SqliteCryptoStore;

    static TMP_DIR: Lazy<TempDir> = Lazy::new(|| tempdir().unwrap());

    async fn get_store(name: &str, passphrase: Option<&str>) -> SqliteCryptoStore {
        let tmpdir_path = TMP_DIR.path().join(name);

        SqliteCryptoStore::open(tmpdir_path.to_str().unwrap(), passphrase)
            .await
            .expect("Can't create a passphrase protected store")
    }

    cryptostore_integration_tests!();
}

/* #[cfg(test)]
mod encrypted_tests {
    use matrix_sdk_crypto::cryptostore_integration_tests;
    use once_cell::sync::Lazy;
    use tempfile::{tempdir, TempDir};

    use super::SqliteCryptoStore;

    static TMP_DIR: Lazy<TempDir> = Lazy::new(|| tempdir().unwrap());

    async fn get_store(name: &str, passphrase: Option<&str>) -> SqliteCryptoStore {
        let tmpdir_path = TMP_DIR.path().join(name);
        let pass = passphrase.unwrap_or("default_test_password");

        SqliteCryptoStore::open(tmpdir_path.to_str().unwrap(), Some(pass))
            .await
            .expect("Can't create a passphrase protected store")
    }
    cryptostore_integration_tests!();
} */
