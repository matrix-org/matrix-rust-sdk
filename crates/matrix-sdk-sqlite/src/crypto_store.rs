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
    borrow::Cow,
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use deadpool_sqlite::{Object as SqliteConn, Pool as SqlitePool, Runtime};
use matrix_sdk_crypto::{
    olm::{
        InboundGroupSession, OutboundGroupSession, PickledInboundGroupSession,
        PrivateCrossSigningIdentity, Session, StaticAccountData,
    },
    store::{
        caches::SessionStore, BackupKeys, Changes, CryptoStore, PendingChanges, RoomKeyCounts,
        RoomSettings,
    },
    types::events::room_key_withheld::RoomKeyWithheldEvent,
    Account, GossipRequest, GossippedSecret, ReadOnlyDevice, ReadOnlyUserIdentities, SecretInfo,
    TrackedUser,
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{
    events::secret::request::SecretName, DeviceId, MilliSecondsSinceUnixEpoch, OwnedDeviceId,
    RoomId, TransactionId, UserId,
};
use rusqlite::OptionalExtension;
use serde::{de::DeserializeOwned, Serialize};
use tokio::{fs, sync::Mutex};
use tracing::{debug, instrument, warn};

use crate::{
    error::{Error, Result},
    get_or_create_store_cipher,
    utils::{
        load_db_version, Key, SqliteConnectionExt as _, SqliteObjectExt, SqliteObjectStoreExt as _,
    },
    OpenStoreError,
};

/// A sqlite based cryptostore.
#[derive(Clone)]
pub struct SqliteCryptoStore {
    store_cipher: Option<Arc<StoreCipher>>,
    path: Option<PathBuf>,
    pool: SqlitePool,

    // DB values cached in memory
    static_account: Arc<RwLock<Option<StaticAccountData>>>,
    session_cache: SessionStore,
    save_changes_lock: Arc<Mutex<()>>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SqliteCryptoStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = &self.path {
            f.debug_struct("SqliteCryptoStore").field("path", &path).finish()
        } else {
            f.debug_struct("SqliteCryptoStore").field("path", &"memory store").finish()
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
        fs::create_dir_all(path).await.map_err(OpenStoreError::CreateDir)?;
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
        let conn = pool.get().await?;
        let version = load_db_version(&conn).await?;
        run_migrations(&conn, version).await?;
        let store_cipher = match passphrase {
            Some(p) => Some(Arc::new(get_or_create_store_cipher(p, &conn).await?)),
            None => None,
        };

        Ok(SqliteCryptoStore {
            store_cipher,
            path: None,
            pool,
            static_account: Arc::new(RwLock::new(None)),
            session_cache: SessionStore::new(),
            save_changes_lock: Default::default(),
        })
    }

    fn encode_value(&self, value: Vec<u8>) -> Result<Vec<u8>> {
        if let Some(key) = &self.store_cipher {
            let encrypted = key.encrypt_value_data(value)?;
            Ok(rmp_serde::to_vec_named(&encrypted)?)
        } else {
            Ok(value)
        }
    }

    fn decode_value<'a>(&self, value: &'a [u8]) -> Result<Cow<'a, [u8]>> {
        if let Some(key) = &self.store_cipher {
            let encrypted = rmp_serde::from_slice(value)?;
            let decrypted = key.decrypt_value_data(encrypted)?;
            Ok(Cow::Owned(decrypted))
        } else {
            Ok(Cow::Borrowed(value))
        }
    }

    fn serialize_json(&self, value: &impl Serialize) -> Result<Vec<u8>> {
        let serialized = serde_json::to_vec(value)?;
        self.encode_value(serialized)
    }

    fn deserialize_json<T: DeserializeOwned>(&self, data: &[u8]) -> Result<T> {
        let decoded = self.decode_value(data)?;
        Ok(serde_json::from_slice(&decoded)?)
    }

    fn serialize_value(&self, value: &impl Serialize) -> Result<Vec<u8>> {
        let serialized = rmp_serde::to_vec_named(value)?;
        self.encode_value(serialized)
    }

    fn deserialize_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T> {
        let decoded = self.decode_value(value)?;
        Ok(rmp_serde::from_slice(&decoded)?)
    }

    fn deserialize_pickled_inbound_group_session(
        &self,
        value: &[u8],
        backed_up: bool,
    ) -> Result<PickledInboundGroupSession> {
        let mut pickle: PickledInboundGroupSession = self.deserialize_value(value)?;
        // backed_up SQL column is source of truth, backed_up field in pickle
        // needed for other stores though
        pickle.backed_up = backed_up;
        Ok(pickle)
    }

    fn deserialize_key_request(&self, value: &[u8], sent_out: bool) -> Result<GossipRequest> {
        let mut request: GossipRequest = self.deserialize_value(value)?;
        // sent_out SQL column is source of truth, sent_out field in serialized value
        // needed for other stores though
        request.sent_out = sent_out;
        Ok(request)
    }

    fn encode_key(&self, table_name: &str, key: impl AsRef<[u8]>) -> Key {
        let bytes = key.as_ref();
        if let Some(store_cipher) = &self.store_cipher {
            Key::Hashed(store_cipher.hash_key(table_name, bytes))
        } else {
            Key::Plain(bytes.to_owned())
        }
    }

    fn get_static_account(&self) -> Option<StaticAccountData> {
        self.static_account.read().unwrap().clone()
    }

    async fn acquire(&self) -> Result<deadpool_sqlite::Object> {
        Ok(self.pool.get().await?)
    }
}

const DATABASE_VERSION: u8 = 8;

/// Run migrations for the given version of the database.
async fn run_migrations(conn: &SqliteConn, version: u8) -> Result<()> {
    if version == 0 {
        debug!("Creating database");
    } else if version < DATABASE_VERSION {
        debug!(version, new_version = DATABASE_VERSION, "Upgrading database");
    } else {
        return Ok(());
    }

    if version < 1 {
        // First turn on WAL mode, this can't be done in the transaction, it fails with
        // the error message: "cannot change into wal mode from within a transaction".
        conn.execute_batch("PRAGMA journal_mode = wal;").await?;
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/crypto_store/001_init.sql"))
        })
        .await?;
    }

    if version < 2 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/crypto_store/002_reset_olm_hash.sql"))
        })
        .await?;
    }

    if version < 3 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/crypto_store/003_room_settings.sql"))
        })
        .await?;
    }

    if version < 4 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/crypto_store/004_drop_outbound_group_sessions.sql"
            ))
        })
        .await?;
    }

    if version < 5 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/crypto_store/005_withheld_code.sql"))
        })
        .await?;
    }

    if version < 6 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/crypto_store/006_drop_outbound_group_sessions.sql"
            ))
        })
        .await?;
    }

    if version < 7 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/crypto_store/007_lock_leases.sql"))
        })
        .await?;
    }

    if version < 8 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/crypto_store/008_secret_inbox.sql"))
        })
        .await?;
    }

    conn.set_kv("version", vec![DATABASE_VERSION]).await?;

    Ok(())
}

trait SqliteConnectionExt {
    fn set_session(
        &self,
        session_id: &[u8],
        sender_key: &[u8],
        data: &[u8],
    ) -> rusqlite::Result<()>;

    fn set_inbound_group_session(
        &self,
        room_id: &[u8],
        session_id: &[u8],
        data: &[u8],
        backed_up: bool,
    ) -> rusqlite::Result<()>;

    fn set_outbound_group_session(&self, room_id: &[u8], data: &[u8]) -> rusqlite::Result<()>;

    fn set_device(&self, user_id: &[u8], device_id: &[u8], data: &[u8]) -> rusqlite::Result<()>;
    fn delete_device(&self, user_id: &[u8], device_id: &[u8]) -> rusqlite::Result<()>;

    fn set_identity(&self, user_id: &[u8], data: &[u8]) -> rusqlite::Result<()>;

    fn add_olm_hash(&self, data: &[u8]) -> rusqlite::Result<()>;

    fn set_key_request(
        &self,
        request_id: &[u8],
        sent_out: bool,
        data: &[u8],
    ) -> rusqlite::Result<()>;

    fn set_direct_withheld(
        &self,
        session_id: &[u8],
        room_id: &[u8],
        data: &[u8],
    ) -> rusqlite::Result<()>;

    fn set_room_settings(&self, room_id: &[u8], data: &[u8]) -> rusqlite::Result<()>;

    fn set_secret(&self, request_id: &[u8], data: &[u8]) -> rusqlite::Result<()>;
}

impl SqliteConnectionExt for rusqlite::Connection {
    fn set_session(
        &self,
        session_id: &[u8],
        sender_key: &[u8],
        data: &[u8],
    ) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO session (session_id, sender_key, data)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (session_id) DO UPDATE SET data = ?3",
            (session_id, sender_key, data),
        )?;
        Ok(())
    }

    fn set_inbound_group_session(
        &self,
        room_id: &[u8],
        session_id: &[u8],
        data: &[u8],
        backed_up: bool,
    ) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO inbound_group_session (session_id, room_id, data, backed_up) \
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (session_id) DO UPDATE SET data = ?3, backed_up = ?4",
            (session_id, room_id, data, backed_up),
        )?;
        Ok(())
    }

    fn set_outbound_group_session(&self, room_id: &[u8], data: &[u8]) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO outbound_group_session (room_id, data) \
             VALUES (?1, ?2)
             ON CONFLICT (room_id) DO UPDATE SET data = ?2",
            (room_id, data),
        )?;
        Ok(())
    }

    fn set_device(&self, user_id: &[u8], device_id: &[u8], data: &[u8]) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO device (user_id, device_id, data) \
             VALUES (?1, ?2, ?3)
             ON CONFLICT (user_id, device_id) DO UPDATE SET data = ?3",
            (user_id, device_id, data),
        )?;
        Ok(())
    }

    fn delete_device(&self, user_id: &[u8], device_id: &[u8]) -> rusqlite::Result<()> {
        self.execute(
            "DELETE FROM device WHERE user_id = ? AND device_id = ?",
            (user_id, device_id),
        )?;
        Ok(())
    }

    fn set_identity(&self, user_id: &[u8], data: &[u8]) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO identity (user_id, data) \
             VALUES (?1, ?2)
             ON CONFLICT (user_id) DO UPDATE SET data = ?2",
            (user_id, data),
        )?;
        Ok(())
    }

    fn add_olm_hash(&self, data: &[u8]) -> rusqlite::Result<()> {
        self.execute("INSERT INTO olm_hash (data) VALUES (?) ON CONFLICT DO NOTHING", (data,))?;
        Ok(())
    }

    fn set_key_request(
        &self,
        request_id: &[u8],
        sent_out: bool,
        data: &[u8],
    ) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO key_requests (request_id, sent_out, data)
            VALUES (?1, ?2, ?3)
            ON CONFLICT (request_id) DO UPDATE SET sent_out = ?2, data = ?3",
            (request_id, sent_out, data),
        )?;
        Ok(())
    }

    fn set_direct_withheld(
        &self,
        session_id: &[u8],
        room_id: &[u8],
        data: &[u8],
    ) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO direct_withheld_info (session_id, room_id, data)
            VALUES (?1, ?2, ?3)
            ON CONFLICT (session_id) DO UPDATE SET room_id = ?2, data = ?3",
            (session_id, room_id, data),
        )?;
        Ok(())
    }

    fn set_room_settings(&self, room_id: &[u8], data: &[u8]) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO room_settings (room_id, data)
            VALUES (?1, ?2)
            ON CONFLICT (room_id) DO UPDATE SET data = ?2",
            (room_id, data),
        )?;
        Ok(())
    }

    fn set_secret(&self, secret_name: &[u8], data: &[u8]) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO secrets (secret_name, data)
            VALUES (?1, ?2)",
            (secret_name, data),
        )?;

        Ok(())
    }
}

#[async_trait]
trait SqliteObjectCryptoStoreExt: SqliteObjectExt {
    async fn get_sessions_for_sender_key(&self, sender_key: Key) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .prepare("SELECT data FROM session WHERE sender_key = ?", |mut stmt| {
                stmt.query((sender_key,))?.mapped(|row| row.get(0)).collect()
            })
            .await?)
    }

    async fn get_inbound_group_session(
        &self,
        session_id: Key,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        Ok(self
            .query_row(
                "SELECT room_id, data FROM inbound_group_session WHERE session_id = ?",
                (session_id,),
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .await
            .optional()?)
    }

    async fn get_inbound_group_sessions(&self) -> Result<Vec<(Vec<u8>, bool)>> {
        Ok(self
            .prepare("SELECT data, backed_up FROM inbound_group_session", |mut stmt| {
                stmt.query(())?.mapped(|row| Ok((row.get(0)?, row.get(1)?))).collect()
            })
            .await?)
    }

    async fn get_inbound_group_session_counts(&self) -> Result<RoomKeyCounts> {
        let total = self
            .query_row("SELECT count(*) FROM inbound_group_session", (), |row| row.get(0))
            .await?;
        let backed_up = self
            .query_row(
                "SELECT count(*) FROM inbound_group_session WHERE backed_up = TRUE",
                (),
                |row| row.get(0),
            )
            .await?;
        Ok(RoomKeyCounts { total, backed_up })
    }

    async fn get_inbound_group_sessions_for_backup(&self, limit: usize) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .prepare(
                "SELECT data FROM inbound_group_session WHERE backed_up = FALSE LIMIT ?",
                move |mut stmt| stmt.query((limit,))?.mapped(|row| row.get(0)).collect(),
            )
            .await?)
    }

    async fn reset_inbound_group_session_backup_state(&self) -> Result<()> {
        self.execute("UPDATE inbound_group_session SET backed_up = FALSE", ()).await?;
        Ok(())
    }

    async fn get_outbound_group_session(&self, room_id: Key) -> Result<Option<Vec<u8>>> {
        Ok(self
            .query_row(
                "SELECT data FROM outbound_group_session WHERE room_id = ?",
                (room_id,),
                |row| row.get(0),
            )
            .await
            .optional()?)
    }

    async fn get_device(&self, user_id: Key, device_id: Key) -> Result<Option<Vec<u8>>> {
        Ok(self
            .query_row(
                "SELECT data FROM device WHERE user_id = ? AND device_id = ?",
                (user_id, device_id),
                |row| row.get(0),
            )
            .await
            .optional()?)
    }

    async fn get_user_devices(&self, user_id: Key) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .prepare("SELECT data FROM device WHERE user_id = ?", |mut stmt| {
                stmt.query((user_id,))?.mapped(|row| row.get(0)).collect()
            })
            .await?)
    }

    async fn get_user_identity(&self, user_id: Key) -> Result<Option<Vec<u8>>> {
        Ok(self
            .query_row("SELECT data FROM identity WHERE user_id = ?", (user_id,), |row| row.get(0))
            .await
            .optional()?)
    }

    async fn has_olm_hash(&self, data: Vec<u8>) -> Result<bool> {
        Ok(self
            .query_row("SELECT count(*) FROM olm_hash WHERE data = ?", (data,), |row| {
                row.get::<_, i32>(0)
            })
            .await?
            > 0)
    }

    async fn get_tracked_users(&self) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .prepare("SELECT data FROM tracked_user", |mut stmt| {
                stmt.query(())?.mapped(|row| row.get(0)).collect()
            })
            .await?)
    }

    async fn add_tracked_users(&self, users: Vec<(Key, Vec<u8>)>) -> Result<()> {
        Ok(self
            .prepare(
                "INSERT INTO tracked_user (user_id, data) \
                 VALUES (?1, ?2) \
                 ON CONFLICT (user_id) DO UPDATE SET data = ?2",
                |mut stmt| {
                    for (user_id, data) in users {
                        stmt.execute((user_id, data))?;
                    }

                    Ok(())
                },
            )
            .await?)
    }

    async fn get_outgoing_secret_request(
        &self,
        request_id: Key,
    ) -> Result<Option<(Vec<u8>, bool)>> {
        Ok(self
            .query_row(
                "SELECT data, sent_out FROM key_requests WHERE request_id = ?",
                (request_id,),
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .await
            .optional()?)
    }

    async fn get_outgoing_secret_requests(&self) -> Result<Vec<(Vec<u8>, bool)>> {
        Ok(self
            .prepare("SELECT data, sent_out FROM key_requests", |mut stmt| {
                stmt.query(())?.mapped(|row| Ok((row.get(0)?, row.get(1)?))).collect()
            })
            .await?)
    }

    async fn get_unsent_secret_requests(&self) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .prepare("SELECT data FROM key_requests WHERE sent_out = FALSE", |mut stmt| {
                stmt.query(())?.mapped(|row| row.get(0)).collect()
            })
            .await?)
    }

    async fn delete_key_request(&self, request_id: Key) -> Result<()> {
        self.execute("DELETE FROM key_requests WHERE request_id = ?", (request_id,)).await?;
        Ok(())
    }

    async fn get_secrets_from_inbox(&self, secret_name: Key) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .prepare("SELECT data FROM secrets WHERE secret_name = ?", |mut stmt| {
                stmt.query((secret_name,))?.mapped(|row| row.get(0)).collect()
            })
            .await?)
    }

    async fn delete_secrets_from_inbox(&self, secret_name: Key) -> Result<()> {
        self.execute("DELETE FROM secrets WHERE secret_name = ?", (secret_name,)).await?;
        Ok(())
    }

    async fn get_direct_withheld_info(
        &self,
        session_id: Key,
        room_id: Key,
    ) -> Result<Option<Vec<u8>>> {
        Ok(self
            .query_row(
                "SELECT data FROM direct_withheld_info WHERE session_id = ?1 AND room_id = ?2",
                (session_id, room_id),
                |row| row.get(0),
            )
            .await
            .optional()?)
    }

    async fn get_room_settings(&self, room_id: Key) -> Result<Option<Vec<u8>>> {
        Ok(self
            .query_row("SELECT data FROM room_settings WHERE room_id = ?", (room_id,), |row| {
                row.get(0)
            })
            .await
            .optional()?)
    }
}

#[async_trait]
impl SqliteObjectCryptoStoreExt for deadpool_sqlite::Object {}

#[async_trait]
impl CryptoStore for SqliteCryptoStore {
    type Error = Error;

    async fn load_account(&self) -> Result<Option<Account>> {
        let conn = self.acquire().await?;
        if let Some(pickle) = conn.get_kv("account").await? {
            let pickle = self.deserialize_value(&pickle)?;

            let account = Account::from_pickle(pickle).map_err(|_| Error::Unpickle)?;

            *self.static_account.write().unwrap() = Some(account.static_data().clone());

            Ok(Some(account))
        } else {
            Ok(None)
        }
    }

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>> {
        let conn = self.acquire().await?;
        if let Some(i) = conn.get_kv("identity").await? {
            let pickle = self.deserialize_value(&i)?;
            Ok(Some(
                PrivateCrossSigningIdentity::from_pickle(pickle)
                    .await
                    .map_err(|_| Error::Unpickle)?,
            ))
        } else {
            Ok(None)
        }
    }

    async fn save_pending_changes(&self, changes: PendingChanges) -> Result<()> {
        // Serialize calls to `save_pending_changes`; there are multiple await points
        // below, and we're pickling data as we go, so we don't want to
        // invalidate data we've previously read and overwrite it in the store.
        // TODO: #2000 should make this lock go away, or change its shape.
        let _guard = self.save_changes_lock.lock().await;

        let pickled_account = if let Some(account) = changes.account {
            *self.static_account.write().unwrap() = Some(account.static_data().clone());
            Some(account.pickle().await)
        } else {
            None
        };

        let this = self.clone();
        self.acquire()
            .await?
            .with_transaction(move |txn| {
                if let Some(pickled_account) = pickled_account {
                    let serialized_account = this.serialize_value(&pickled_account)?;
                    txn.set_kv("account", &serialized_account)?;
                }

                Ok::<_, Error>(())
            })
            .await?;

        Ok(())
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
        // Serialize calls to `save_changes`; there are multiple await points below, and
        // we're pickling data as we go, so we don't want to invalidate data
        // we've previously read and overwrite it in the store.
        // TODO: #2000 should make this lock go away, or change its shape.
        let _guard = self.save_changes_lock.lock().await;

        let pickled_private_identity =
            if let Some(i) = changes.private_identity { Some(i.pickle().await) } else { None };

        let mut session_changes = Vec::new();
        for session in changes.sessions {
            let session_id = self.encode_key("session", session.session_id());
            let sender_key = self.encode_key("session", session.sender_key().to_base64());
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
                if let Some(pickled_private_identity) = &pickled_private_identity {
                    let serialized_private_identity =
                        this.serialize_value(pickled_private_identity)?;
                    txn.set_kv("identity", &serialized_private_identity)?;
                }

                if let Some(token) = &changes.next_batch_token {
                    let serialized_token = this.serialize_value(token)?;
                    txn.set_kv("next_batch_token", &serialized_token)?;
                }

                if let Some(decryption_key) = &changes.backup_decryption_key {
                    let serialized_decryption_key = this.serialize_value(decryption_key)?;
                    txn.set_kv("recovery_key_v1", &serialized_decryption_key)?;
                }

                if let Some(backup_version) = &changes.backup_version {
                    let serialized_backup_version = this.serialize_value(backup_version)?;
                    txn.set_kv("backup_version_v1", &serialized_backup_version)?;
                }

                for device in changes.devices.new.iter().chain(&changes.devices.changed) {
                    let user_id = this.encode_key("device", device.user_id().as_bytes());
                    let device_id = this.encode_key("device", device.device_id().as_bytes());
                    let data = this.serialize_value(&device)?;
                    txn.set_device(&user_id, &device_id, &data)?;
                }

                for device in &changes.devices.deleted {
                    let user_id = this.encode_key("device", device.user_id().as_bytes());
                    let device_id = this.encode_key("device", device.device_id().as_bytes());
                    txn.delete_device(&user_id, &device_id)?;
                }

                for identity in changes.identities.changed.iter().chain(&changes.identities.new) {
                    let user_id = this.encode_key("identity", identity.user_id().as_bytes());
                    let data = this.serialize_value(&identity)?;
                    txn.set_identity(&user_id, &data)?;
                }

                for (session_id, sender_key, pickle) in &session_changes {
                    let serialized_session = this.serialize_value(&pickle)?;
                    txn.set_session(session_id, sender_key, &serialized_session)?;
                }

                for (room_id, session_id, pickle) in &inbound_session_changes {
                    let serialized_session = this.serialize_value(&pickle)?;
                    txn.set_inbound_group_session(
                        room_id,
                        session_id,
                        &serialized_session,
                        pickle.backed_up,
                    )?;
                }

                for (room_id, pickle) in &outbound_session_changes {
                    let serialized_session = this.serialize_json(&pickle)?;
                    txn.set_outbound_group_session(room_id, &serialized_session)?;
                }

                for hash in &changes.message_hashes {
                    let hash = rmp_serde::to_vec(hash)?;
                    txn.add_olm_hash(&hash)?;
                }

                for request in changes.key_requests {
                    let request_id = this.encode_key("key_requests", request.request_id.as_bytes());
                    let serialized_request = this.serialize_value(&request)?;
                    txn.set_key_request(&request_id, request.sent_out, &serialized_request)?;
                }

                for (room_id, data) in changes.withheld_session_info {
                    for (session_id, event) in data {
                        let session_id = this.encode_key("direct_withheld_info", session_id);
                        let room_id = this.encode_key("direct_withheld_info", &room_id);
                        let serialized_info = this.serialize_json(&event)?;
                        txn.set_direct_withheld(&session_id, &room_id, &serialized_info)?;
                    }
                }

                for (room_id, settings) in changes.room_settings {
                    let room_id = this.encode_key("room_settings", room_id.as_bytes());
                    let value = this.serialize_value(&settings)?;
                    txn.set_room_settings(&room_id, &value)?;
                }

                for secret in changes.secrets {
                    let secret_name = this.encode_key("secrets", secret.secret_name.to_string());
                    let value = this.serialize_json(&secret)?;
                    txn.set_secret(&secret_name, &value)?;
                }

                Ok::<_, Error>(())
            })
            .await?;

        Ok(())
    }

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        let account_info = self.get_static_account().ok_or(Error::AccountUnset)?;

        if self.session_cache.get(sender_key).is_none() {
            let sessions = self
                .acquire()
                .await?
                .get_sessions_for_sender_key(self.encode_key("session", sender_key.as_bytes()))
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

    #[instrument(skip(self))]
    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        let session_id = self.encode_key("inbound_group_session", session_id);
        let Some((room_id_from_db, value)) =
            self.acquire().await?.get_inbound_group_session(session_id).await?
        else {
            return Ok(None);
        };

        let room_id = self.encode_key("inbound_group_session", room_id.as_bytes());
        if *room_id != room_id_from_db {
            warn!("expected room_id for session_id doesn't match what's in the DB");
            return Ok(None);
        }

        let pickle = self.deserialize_value(&value)?;

        Ok(Some(InboundGroupSession::from_pickle(pickle)?))
    }

    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        self.acquire()
            .await?
            .get_inbound_group_sessions()
            .await?
            .into_iter()
            .map(|(value, backed_up)| {
                let pickle = self.deserialize_pickled_inbound_group_session(&value, backed_up)?;
                Ok(InboundGroupSession::from_pickle(pickle)?)
            })
            .collect()
    }

    async fn inbound_group_session_counts(&self) -> Result<RoomKeyCounts> {
        Ok(self.acquire().await?.get_inbound_group_session_counts().await?)
    }

    async fn inbound_group_sessions_for_backup(
        &self,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>> {
        self.acquire()
            .await?
            .get_inbound_group_sessions_for_backup(limit)
            .await?
            .into_iter()
            .map(|value| {
                let pickle = self.deserialize_pickled_inbound_group_session(&value, false)?;
                Ok(InboundGroupSession::from_pickle(pickle)?)
            })
            .collect()
    }

    async fn reset_backup_state(&self) -> Result<()> {
        Ok(self.acquire().await?.reset_inbound_group_session_backup_state().await?)
    }

    async fn load_backup_keys(&self) -> Result<BackupKeys> {
        let conn = self.acquire().await?;

        let backup_version = conn
            .get_kv("backup_version_v1")
            .await?
            .map(|value| self.deserialize_value(&value))
            .transpose()?;

        let decryption_key = conn
            .get_kv("recovery_key_v1")
            .await?
            .map(|value| self.deserialize_value(&value))
            .transpose()?;

        Ok(BackupKeys { backup_version, decryption_key })
    }

    async fn get_outbound_group_session(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>> {
        let room_id = self.encode_key("outbound_group_session", room_id.as_bytes());
        let Some(value) = self.acquire().await?.get_outbound_group_session(room_id).await? else {
            return Ok(None);
        };

        let account_info = self.get_static_account().ok_or(Error::AccountUnset)?;

        let pickle = self.deserialize_json(&value)?;
        let session = OutboundGroupSession::from_pickle(
            account_info.device_id,
            account_info.identity_keys,
            pickle,
        )
        .map_err(|_| Error::Unpickle)?;

        return Ok(Some(session));
    }

    async fn load_tracked_users(&self) -> Result<Vec<TrackedUser>> {
        self.acquire()
            .await?
            .get_tracked_users()
            .await?
            .iter()
            .map(|value| self.deserialize_value(value))
            .collect()
    }

    async fn save_tracked_users(&self, tracked_users: &[(&UserId, bool)]) -> Result<()> {
        let users: Vec<(Key, Vec<u8>)> = tracked_users
            .iter()
            .map(|(u, d)| {
                let user_id = self.encode_key("tracked_users", u.as_bytes());
                let data =
                    self.serialize_value(&TrackedUser { user_id: (*u).into(), dirty: *d })?;
                Ok((user_id, data))
            })
            .collect::<Result<_>>()?;

        Ok(self.acquire().await?.add_tracked_users(users).await?)
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>> {
        let user_id = self.encode_key("device", user_id.as_bytes());
        let device_id = self.encode_key("device", device_id.as_bytes());
        Ok(self
            .acquire()
            .await?
            .get_device(user_id, device_id)
            .await?
            .map(|value| self.deserialize_value(&value))
            .transpose()?)
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, ReadOnlyDevice>> {
        let user_id = self.encode_key("device", user_id.as_bytes());
        self.acquire()
            .await?
            .get_user_devices(user_id)
            .await?
            .into_iter()
            .map(|value| {
                let device: ReadOnlyDevice = self.deserialize_value(&value)?;
                Ok((device.device_id().to_owned(), device))
            })
            .collect()
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<ReadOnlyUserIdentities>> {
        let user_id = self.encode_key("identity", user_id.as_bytes());
        Ok(self
            .acquire()
            .await?
            .get_user_identity(user_id)
            .await?
            .map(|value| self.deserialize_value(&value))
            .transpose()?)
    }

    async fn is_message_known(
        &self,
        message_hash: &matrix_sdk_crypto::olm::OlmMessageHash,
    ) -> Result<bool> {
        let value = rmp_serde::to_vec(message_hash)?;
        Ok(self.acquire().await?.has_olm_hash(value).await?)
    }

    async fn get_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> Result<Option<GossipRequest>> {
        let request_id = self.encode_key("key_requests", request_id.as_bytes());
        Ok(self
            .acquire()
            .await?
            .get_outgoing_secret_request(request_id)
            .await?
            .map(|(value, sent_out)| self.deserialize_key_request(&value, sent_out))
            .transpose()?)
    }

    async fn get_secret_request_by_info(
        &self,
        key_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>> {
        let requests = self.acquire().await?.get_outgoing_secret_requests().await?;
        for (request, sent_out) in requests {
            let request = self.deserialize_key_request(&request, sent_out)?;
            if request.info == *key_info {
                return Ok(Some(request));
            }
        }
        Ok(None)
    }

    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>> {
        self.acquire()
            .await?
            .get_unsent_secret_requests()
            .await?
            .iter()
            .map(|value| {
                let request = self.deserialize_key_request(value, false)?;
                Ok(request)
            })
            .collect()
    }

    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> Result<()> {
        let request_id = self.encode_key("key_requests", request_id.as_bytes());
        Ok(self.acquire().await?.delete_key_request(request_id).await?)
    }

    async fn get_secrets_from_inbox(
        &self,
        secret_name: &SecretName,
    ) -> Result<Vec<GossippedSecret>> {
        let secret_name = self.encode_key("secrets", secret_name.to_string());

        self.acquire()
            .await?
            .get_secrets_from_inbox(secret_name)
            .await?
            .into_iter()
            .map(|value| self.deserialize_json(value.as_ref()))
            .collect()
    }

    async fn delete_secrets_from_inbox(&self, secret_name: &SecretName) -> Result<()> {
        let secret_name = self.encode_key("secrets", secret_name.to_string());
        self.acquire().await?.delete_secrets_from_inbox(secret_name).await
    }

    async fn get_withheld_info(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<RoomKeyWithheldEvent>> {
        let room_id = self.encode_key("direct_withheld_info", room_id);
        let session_id = self.encode_key("direct_withheld_info", session_id);

        self.acquire()
            .await?
            .get_direct_withheld_info(session_id, room_id)
            .await?
            .map(|value| {
                let info = self.deserialize_json::<RoomKeyWithheldEvent>(&value)?;
                Ok(info)
            })
            .transpose()
    }

    async fn get_room_settings(&self, room_id: &RoomId) -> Result<Option<RoomSettings>> {
        let room_id = self.encode_key("room_settings", room_id.as_bytes());
        let Some(value) = self.acquire().await?.get_room_settings(room_id).await? else {
            return Ok(None);
        };

        let settings = self.deserialize_value(&value)?;

        return Ok(Some(settings));
    }

    async fn get_custom_value(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let Some(serialized) = self.acquire().await?.get_kv(key).await? else {
            return Ok(None);
        };
        let value = if let Some(cipher) = &self.store_cipher {
            let encrypted = rmp_serde::from_slice(&serialized)?;
            cipher.decrypt_value_data(encrypted)?
        } else {
            serialized
        };

        Ok(Some(value))
    }

    async fn set_custom_value(&self, key: &str, value: Vec<u8>) -> Result<()> {
        let serialized = if let Some(cipher) = &self.store_cipher {
            let encrypted = cipher.encrypt_value_data(value)?;
            rmp_serde::to_vec_named(&encrypted)?
        } else {
            value
        };

        self.acquire().await?.set_kv(key, serialized).await?;
        Ok(())
    }

    async fn remove_custom_value(&self, key: &str) -> Result<()> {
        let key = key.to_owned();
        self.acquire()
            .await?
            .interact(move |conn| conn.execute("DELETE FROM kv WHERE key = ?1", (&key,)))
            .await
            .unwrap()?;
        Ok(())
    }

    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<bool> {
        let key = key.to_owned();
        let holder = holder.to_owned();

        let now_ts: u64 = MilliSecondsSinceUnixEpoch::now().get().into();
        let expiration_ts = now_ts + lease_duration_ms as u64;

        let num_touched = self
            .acquire()
            .await?
            .with_transaction(move |txn| {
                txn.execute(
                    "INSERT INTO lease_locks (key, holder, expiration_ts)
                    VALUES (?1, ?2, ?3)
                    ON CONFLICT (key)
                    DO
                        UPDATE SET holder = ?2, expiration_ts = ?3
                        WHERE holder = ?2
                        OR expiration_ts < ?4
                ",
                    (key, holder, expiration_ts, now_ts),
                )
            })
            .await?;

        Ok(num_touched == 1)
    }

    async fn next_batch_token(&self) -> Result<Option<String>, Self::Error> {
        let conn = self.acquire().await?;
        if let Some(token) = conn.get_kv("next_batch_token").await? {
            let maybe_token: Option<String> = self.deserialize_value(&token)?;
            Ok(maybe_token)
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_crypto::{cryptostore_integration_tests, cryptostore_integration_tests_time};
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
    cryptostore_integration_tests_time!();
}

#[cfg(test)]
mod encrypted_tests {
    use matrix_sdk_crypto::{cryptostore_integration_tests, cryptostore_integration_tests_time};
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
    cryptostore_integration_tests_time!();
}
