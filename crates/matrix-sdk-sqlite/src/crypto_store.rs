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
    collections::HashMap,
    fmt,
    path::Path,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use matrix_sdk_base::cross_process_lock::CrossProcessLockGeneration;
use matrix_sdk_crypto::{
    Account, DeviceData, GossipRequest, GossippedSecret, SecretInfo, TrackedUser, UserIdentityData,
    olm::{
        InboundGroupSession, OutboundGroupSession, PickledInboundGroupSession,
        PrivateCrossSigningIdentity, SenderDataType, Session, StaticAccountData,
    },
    store::{
        CryptoStore,
        types::{
            BackupKeys, Changes, DehydratedDeviceKey, PendingChanges, RoomKeyCounts,
            RoomKeyWithheldEntry, RoomSettings, StoredRoomKeyBundleData,
        },
    },
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{
    DeviceId, MilliSecondsSinceUnixEpoch, OwnedDeviceId, RoomId, TransactionId, UserId,
    events::secret::request::SecretName,
};
use rusqlite::{OptionalExtension, named_params, params_from_iter};
use tokio::{fs, sync::Mutex};
use tracing::{debug, instrument, warn};
use vodozemac::Curve25519PublicKey;

use crate::{
    OpenStoreError, Secret, SqliteStoreConfig,
    connection::{Connection as SqliteAsyncConn, Pool as SqlitePool},
    error::{Error, Result},
    utils::{
        EncryptableStore, Key, SqliteAsyncConnExt, SqliteKeyValueStoreAsyncConnExt,
        SqliteKeyValueStoreConnExt, repeat_vars,
    },
};

/// The database name.
const DATABASE_NAME: &str = "matrix-sdk-crypto.sqlite3";

/// An SQLite-based crypto store.
#[derive(Clone)]
pub struct SqliteCryptoStore {
    store_cipher: Option<Arc<StoreCipher>>,
    pool: SqlitePool,

    // DB values cached in memory
    static_account: Arc<RwLock<Option<StaticAccountData>>>,
    save_changes_lock: Arc<Mutex<()>>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SqliteCryptoStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteCryptoStore").finish_non_exhaustive()
    }
}

impl EncryptableStore for SqliteCryptoStore {
    fn get_cypher(&self) -> Option<&StoreCipher> {
        self.store_cipher.as_deref()
    }
}

impl SqliteCryptoStore {
    /// Open the SQLite-based crypto store at the given path using the given
    /// passphrase to encrypt private data.
    pub async fn open(
        path: impl AsRef<Path>,
        passphrase: Option<&str>,
    ) -> Result<Self, OpenStoreError> {
        Self::open_with_config(SqliteStoreConfig::new(path).passphrase(passphrase)).await
    }

    /// Open the SQLite-based crypto store at the given path using the given
    /// key to encrypt private data.
    pub async fn open_with_key(
        path: impl AsRef<Path>,
        key: Option<&[u8; 32]>,
    ) -> Result<Self, OpenStoreError> {
        Self::open_with_config(SqliteStoreConfig::new(path).key(key)).await
    }

    /// Open the SQLite-based crypto store with the config open config.
    pub async fn open_with_config(config: SqliteStoreConfig) -> Result<Self, OpenStoreError> {
        fs::create_dir_all(&config.path).await.map_err(OpenStoreError::CreateDir)?;

        let pool = config.build_pool_of_connections(DATABASE_NAME)?;

        let this = Self::open_with_pool(pool, config.secret).await?;
        this.pool.get().await?.apply_runtime_config(config.runtime_config).await?;

        Ok(this)
    }

    /// Create an SQLite-based crypto store using the given SQLite database
    /// pool. The given secret will be used to encrypt private data.
    async fn open_with_pool(
        pool: SqlitePool,
        secret: Option<Secret>,
    ) -> Result<Self, OpenStoreError> {
        let conn = pool.get().await?;

        let version = conn.db_version().await?;
        debug!("Opened sqlite store with version {}", version);
        run_migrations(&conn, version).await?;

        conn.wal_checkpoint().await;

        let store_cipher = match secret {
            Some(s) => Some(Arc::new(conn.get_or_create_store_cipher(s).await?)),
            None => None,
        };

        Ok(SqliteCryptoStore {
            store_cipher,
            pool,
            static_account: Arc::new(RwLock::new(None)),
            save_changes_lock: Default::default(),
        })
    }

    fn deserialize_and_unpickle_inbound_group_session(
        &self,
        value: Vec<u8>,
        backed_up: bool,
    ) -> Result<InboundGroupSession> {
        let mut pickle: PickledInboundGroupSession = self.deserialize_value(&value)?;

        // The `backed_up` SQL column is the source of truth, because we update it
        // inside `mark_inbound_group_sessions_as_backed_up` and don't update
        // the pickled value inside the `data` column (until now, when we are puling it
        // out of the DB).
        pickle.backed_up = backed_up;

        Ok(InboundGroupSession::from_pickle(pickle)?)
    }

    fn deserialize_key_request(&self, value: &[u8], sent_out: bool) -> Result<GossipRequest> {
        let mut request: GossipRequest = self.deserialize_value(value)?;
        // sent_out SQL column is source of truth, sent_out field in serialized value
        // needed for other stores though
        request.sent_out = sent_out;
        Ok(request)
    }

    fn get_static_account(&self) -> Option<StaticAccountData> {
        self.static_account.read().unwrap().clone()
    }

    async fn acquire(&self) -> Result<SqliteAsyncConn> {
        Ok(self.pool.get().await?)
    }
}

const DATABASE_VERSION: u8 = 14;

/// key for the dehydrated device pickle key in the key/value table.
const DEHYDRATED_DEVICE_PICKLE_KEY: &str = "dehydrated_device_pickle_key";

/// Run migrations for the given version of the database.
async fn run_migrations(conn: &SqliteAsyncConn, version: u8) -> Result<()> {
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
            txn.execute_batch(include_str!("../migrations/crypto_store/001_init.sql"))?;
            txn.set_db_version(1)
        })
        .await?;
    }

    if version < 2 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/crypto_store/002_reset_olm_hash.sql"))?;
            txn.set_db_version(2)
        })
        .await?;
    }

    if version < 3 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/crypto_store/003_room_settings.sql"))?;
            txn.set_db_version(3)
        })
        .await?;
    }

    if version < 4 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/crypto_store/004_drop_outbound_group_sessions.sql"
            ))?;
            txn.set_db_version(4)
        })
        .await?;
    }

    if version < 5 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/crypto_store/005_withheld_code.sql"))?;
            txn.set_db_version(5)
        })
        .await?;
    }

    if version < 6 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/crypto_store/006_drop_outbound_group_sessions.sql"
            ))?;
            txn.set_db_version(6)
        })
        .await?;
    }

    if version < 7 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/crypto_store/007_lock_leases.sql"))?;
            txn.set_db_version(7)
        })
        .await?;
    }

    if version < 8 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/crypto_store/008_secret_inbox.sql"))?;
            txn.set_db_version(8)
        })
        .await?;
    }

    if version < 9 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/crypto_store/009_inbound_group_session_sender_key_sender_data_type.sql"
            ))?;
            txn.set_db_version(9)
        })
        .await?;
    }

    if version < 10 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/crypto_store/010_received_room_key_bundles.sql"
            ))?;
            txn.set_db_version(10)
        })
        .await?;
    }

    if version < 11 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/crypto_store/011_received_room_key_bundles_with_curve_key.sql"
            ))?;
            txn.set_db_version(11)
        })
        .await?;
    }

    if version < 12 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/crypto_store/012_withheld_code_by_room.sql"
            ))?;
            txn.set_db_version(12)
        })
        .await?;
    }

    if version < 13 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/crypto_store/013_lease_locks_with_generation.sql"
            ))?;
            txn.set_db_version(13)
        })
        .await?;
    }

    if version < 14 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/crypto_store/014_room_key_backups_fully_downloaded.sql"
            ))?;
            txn.set_db_version(14)
        })
        .await?;
    }

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
        sender_key: Option<&[u8]>,
        sender_data_type: Option<u8>,
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

    fn set_received_room_key_bundle(
        &self,
        room_id: &[u8],
        user_id: &[u8],
        data: &[u8],
    ) -> rusqlite::Result<()>;

    fn set_has_downloaded_all_room_keys(&self, room_id: &[u8]) -> rusqlite::Result<()>;
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
        sender_key: Option<&[u8]>,
        sender_data_type: Option<u8>,
    ) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO inbound_group_session (session_id, room_id, data, backed_up, sender_key, sender_data_type) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (session_id) DO UPDATE SET data = ?3, backed_up = ?4, sender_key = ?5, sender_data_type = ?6",
            (session_id, room_id, data, backed_up, sender_key, sender_data_type),
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

    fn set_received_room_key_bundle(
        &self,
        room_id: &[u8],
        sender_user_id: &[u8],
        data: &[u8],
    ) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO received_room_key_bundle(room_id, sender_user_id, bundle_data)
            VALUES (?1, ?2, ?3)
            ON CONFLICT (room_id, sender_user_id) DO UPDATE SET bundle_data = ?3",
            (room_id, sender_user_id, data),
        )?;
        Ok(())
    }

    fn set_has_downloaded_all_room_keys(&self, room_id: &[u8]) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO room_key_backups_fully_downloaded(room_id)
             VALUES (?1)
             ON CONFLICT(room_id) DO NOTHING",
            (room_id,),
        )?;
        Ok(())
    }
}

#[async_trait]
trait SqliteObjectCryptoStoreExt: SqliteAsyncConnExt {
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
    ) -> Result<Option<(Vec<u8>, Vec<u8>, bool)>> {
        Ok(self
            .query_row(
                "SELECT room_id, data, backed_up FROM inbound_group_session WHERE session_id = ?",
                (session_id,),
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
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

    async fn get_inbound_group_session_counts(
        &self,
        _backup_version: Option<&str>,
    ) -> Result<RoomKeyCounts> {
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

    async fn get_inbound_group_sessions_by_room_id(
        &self,
        room_id: Key,
    ) -> Result<Vec<(Vec<u8>, bool)>> {
        Ok(self
            .prepare(
                "SELECT data, backed_up FROM inbound_group_session WHERE room_id = :room_id",
                move |mut stmt| {
                    stmt.query(named_params! {
                        ":room_id": room_id,
                    })?
                    .mapped(|row| Ok((row.get(0)?, row.get(1)?)))
                    .collect()
                },
            )
            .await?)
    }

    async fn get_inbound_group_sessions_for_device_batch(
        &self,
        sender_key: Key,
        sender_data_type: SenderDataType,
        after_session_id: Option<Key>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, bool)>> {
        Ok(self
            .prepare(
                "
                SELECT data, backed_up
                FROM inbound_group_session
                WHERE sender_key = :sender_key
                    AND sender_data_type = :sender_data_type
                    AND session_id > :after_session_id
                ORDER BY session_id
                LIMIT :limit
                ",
                move |mut stmt| {
                    let sender_data_type = sender_data_type as u8;

                    // If we are not provided with an `after_session_id`, use a key which will sort
                    // before all real keys: the empty string.
                    let after_session_id = after_session_id.unwrap_or(Key::Plain(Vec::new()));

                    stmt.query(named_params! {
                        ":sender_key": sender_key,
                        ":sender_data_type": sender_data_type,
                        ":after_session_id": after_session_id,
                        ":limit": limit,
                    })?
                    .mapped(|row| Ok((row.get(0)?, row.get(1)?)))
                    .collect()
                },
            )
            .await?)
    }

    async fn get_inbound_group_sessions_for_backup(&self, limit: usize) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .prepare(
                "SELECT data FROM inbound_group_session WHERE backed_up = FALSE LIMIT ?",
                move |mut stmt| stmt.query((limit,))?.mapped(|row| row.get(0)).collect(),
            )
            .await?)
    }

    async fn mark_inbound_group_sessions_as_backed_up(&self, session_ids: Vec<Key>) -> Result<()> {
        if session_ids.is_empty() {
            // We are not expecting to be called with an empty list of sessions
            warn!("No sessions to mark as backed up!");
            return Ok(());
        }

        let session_ids_len = session_ids.len();

        self.chunk_large_query_over(session_ids, None, move |txn, session_ids| {
            // Safety: placeholders is not generated using any user input except the number
            // of session IDs, so it is safe from injection.
            let sql_params = repeat_vars(session_ids_len);
            let query = format!("UPDATE inbound_group_session SET backed_up = TRUE where session_id IN ({sql_params})");
            txn.prepare(&query)?.execute(params_from_iter(session_ids.iter()))?;
            Ok(Vec::<()>::new())
        }).await?;

        Ok(())
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

    async fn get_withheld_sessions_by_room_id(&self, room_id: Key) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .prepare("SELECT data FROM direct_withheld_info WHERE room_id = ?1", |mut stmt| {
                stmt.query((room_id,))?.mapped(|row| row.get(0)).collect()
            })
            .await?)
    }

    async fn get_room_settings(&self, room_id: Key) -> Result<Option<Vec<u8>>> {
        Ok(self
            .query_row("SELECT data FROM room_settings WHERE room_id = ?", (room_id,), |row| {
                row.get(0)
            })
            .await
            .optional()?)
    }

    async fn get_received_room_key_bundle(
        &self,
        room_id: Key,
        sender_user: Key,
    ) -> Result<Option<Vec<u8>>> {
        Ok(self
            .query_row(
                "SELECT bundle_data FROM received_room_key_bundle WHERE room_id = ? AND sender_user_id = ?",
                (room_id, sender_user),
                |row| { row.get(0) },
            )
            .await
            .optional()?)
    }

    async fn has_downloaded_all_room_keys(&self, room_id: Key) -> Result<bool> {
        Ok(self
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM room_key_backups_fully_downloaded WHERE room_id = ?)",
                (room_id,),
                |row| row.get(0),
            )
            .await?)
    }
}

#[async_trait]
impl SqliteObjectCryptoStoreExt for SqliteAsyncConn {}

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
            Ok(Some(PrivateCrossSigningIdentity::from_pickle(pickle).map_err(|_| Error::Unpickle)?))
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
            Some(account.pickle())
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
        }

        let mut inbound_session_changes = Vec::new();
        for session in changes.inbound_group_sessions {
            let room_id = self.encode_key("inbound_group_session", session.room_id().as_bytes());
            let session_id = self.encode_key("inbound_group_session", session.session_id());
            let pickle = session.pickle().await;
            let sender_key =
                self.encode_key("inbound_group_session", session.sender_key().to_base64());
            inbound_session_changes.push((room_id, session_id, pickle, sender_key));
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

                if let Some(pickle_key) = &changes.dehydrated_device_pickle_key {
                    let serialized_pickle_key = this.serialize_value(pickle_key)?;
                    txn.set_kv(DEHYDRATED_DEVICE_PICKLE_KEY, &serialized_pickle_key)?;
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

                for (room_id, session_id, pickle, sender_key) in &inbound_session_changes {
                    let serialized_session = this.serialize_value(&pickle)?;
                    txn.set_inbound_group_session(
                        room_id,
                        session_id,
                        &serialized_session,
                        pickle.backed_up,
                        Some(sender_key),
                        Some(pickle.sender_data.to_type() as u8),
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

                for bundle in changes.received_room_key_bundles {
                    let room_id =
                        this.encode_key("received_room_key_bundle", &bundle.bundle_data.room_id);
                    let user_id = this.encode_key("received_room_key_bundle", &bundle.sender_user);
                    let value = this.serialize_value(&bundle)?;
                    txn.set_received_room_key_bundle(&room_id, &user_id, &value)?;
                }

                for room in changes.room_key_backups_fully_downloaded {
                    let room_id = this.encode_key("room_key_backups_fully_downloaded", &room);
                    txn.set_has_downloaded_all_room_keys(&room_id)?;
                }

                Ok::<_, Error>(())
            })
            .await?;

        Ok(())
    }

    async fn save_inbound_group_sessions(
        &self,
        sessions: Vec<InboundGroupSession>,
        backed_up_to_version: Option<&str>,
    ) -> matrix_sdk_crypto::store::Result<(), Self::Error> {
        // Sanity-check that the data in the sessions corresponds to backed_up_version
        sessions.iter().for_each(|s| {
            let backed_up = s.backed_up();
            if backed_up != backed_up_to_version.is_some() {
                warn!(
                    backed_up,
                    backed_up_to_version,
                    "Session backed-up flag does not correspond to backup version setting",
                );
            }
        });

        // Currently, this store doesn't save the backup version separately, so this
        // just delegates to save_changes.
        self.save_changes(Changes { inbound_group_sessions: sessions, ..Changes::default() }).await
    }

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Vec<Session>>> {
        let device_keys = self.get_own_device().await?.as_device_keys().clone();

        let sessions: Vec<_> = self
            .acquire()
            .await?
            .get_sessions_for_sender_key(self.encode_key("session", sender_key.as_bytes()))
            .await?
            .into_iter()
            .map(|bytes| {
                let pickle = self.deserialize_value(&bytes)?;
                Session::from_pickle(device_keys.clone(), pickle).map_err(|_| Error::AccountUnset)
            })
            .collect::<Result<_>>()?;

        if sessions.is_empty() { Ok(None) } else { Ok(Some(sessions)) }
    }

    #[instrument(skip(self))]
    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        let session_id = self.encode_key("inbound_group_session", session_id);
        let Some((room_id_from_db, value, backed_up)) =
            self.acquire().await?.get_inbound_group_session(session_id).await?
        else {
            return Ok(None);
        };

        let room_id = self.encode_key("inbound_group_session", room_id.as_bytes());
        if *room_id != room_id_from_db {
            warn!("expected room_id for session_id doesn't match what's in the DB");
            return Ok(None);
        }

        Ok(Some(self.deserialize_and_unpickle_inbound_group_session(value, backed_up)?))
    }

    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        self.acquire()
            .await?
            .get_inbound_group_sessions()
            .await?
            .into_iter()
            .map(|(value, backed_up)| {
                self.deserialize_and_unpickle_inbound_group_session(value, backed_up)
            })
            .collect()
    }

    async fn get_inbound_group_sessions_by_room_id(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<InboundGroupSession>> {
        let room_id = self.encode_key("inbound_group_session", room_id.as_bytes());
        self.acquire()
            .await?
            .get_inbound_group_sessions_by_room_id(room_id)
            .await?
            .into_iter()
            .map(|(value, backed_up)| {
                self.deserialize_and_unpickle_inbound_group_session(value, backed_up)
            })
            .collect()
    }

    async fn get_inbound_group_sessions_for_device_batch(
        &self,
        sender_key: Curve25519PublicKey,
        sender_data_type: SenderDataType,
        after_session_id: Option<String>,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>, Self::Error> {
        let after_session_id =
            after_session_id.map(|session_id| self.encode_key("inbound_group_session", session_id));
        let sender_key = self.encode_key("inbound_group_session", sender_key.to_base64());

        self.acquire()
            .await?
            .get_inbound_group_sessions_for_device_batch(
                sender_key,
                sender_data_type,
                after_session_id,
                limit,
            )
            .await?
            .into_iter()
            .map(|(value, backed_up)| {
                self.deserialize_and_unpickle_inbound_group_session(value, backed_up)
            })
            .collect()
    }

    async fn inbound_group_session_counts(
        &self,
        backup_version: Option<&str>,
    ) -> Result<RoomKeyCounts> {
        Ok(self.acquire().await?.get_inbound_group_session_counts(backup_version).await?)
    }

    async fn inbound_group_sessions_for_backup(
        &self,
        _backup_version: &str,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>> {
        self.acquire()
            .await?
            .get_inbound_group_sessions_for_backup(limit)
            .await?
            .into_iter()
            .map(|value| self.deserialize_and_unpickle_inbound_group_session(value, false))
            .collect()
    }

    async fn mark_inbound_group_sessions_as_backed_up(
        &self,
        _backup_version: &str,
        session_ids: &[(&RoomId, &str)],
    ) -> Result<()> {
        Ok(self
            .acquire()
            .await?
            .mark_inbound_group_sessions_as_backed_up(
                session_ids
                    .iter()
                    .map(|(_, s)| self.encode_key("inbound_group_session", s))
                    .collect(),
            )
            .await?)
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

    async fn load_dehydrated_device_pickle_key(&self) -> Result<Option<DehydratedDeviceKey>> {
        let conn = self.acquire().await?;

        conn.get_kv(DEHYDRATED_DEVICE_PICKLE_KEY)
            .await?
            .map(|value| self.deserialize_value(&value))
            .transpose()
    }

    async fn delete_dehydrated_device_pickle_key(&self) -> Result<(), Self::Error> {
        let conn = self.acquire().await?;
        conn.clear_kv(DEHYDRATED_DEVICE_PICKLE_KEY).await?;

        Ok(())
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
    ) -> Result<Option<DeviceData>> {
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
    ) -> Result<HashMap<OwnedDeviceId, DeviceData>> {
        let user_id = self.encode_key("device", user_id.as_bytes());
        self.acquire()
            .await?
            .get_user_devices(user_id)
            .await?
            .into_iter()
            .map(|value| {
                let device: DeviceData = self.deserialize_value(&value)?;
                Ok((device.device_id().to_owned(), device))
            })
            .collect()
    }

    async fn get_own_device(&self) -> Result<DeviceData> {
        let account_info = self.get_static_account().ok_or(Error::AccountUnset)?;

        Ok(self
            .get_device(&account_info.user_id, &account_info.device_id)
            .await?
            .expect("We should be able to find our own device."))
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<UserIdentityData>> {
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
    ) -> Result<Option<RoomKeyWithheldEntry>> {
        let room_id = self.encode_key("direct_withheld_info", room_id);
        let session_id = self.encode_key("direct_withheld_info", session_id);

        self.acquire()
            .await?
            .get_direct_withheld_info(session_id, room_id)
            .await?
            .map(|value| {
                let info = self.deserialize_json::<RoomKeyWithheldEntry>(&value)?;
                Ok(info)
            })
            .transpose()
    }

    async fn get_withheld_sessions_by_room_id(
        &self,
        room_id: &RoomId,
    ) -> matrix_sdk_crypto::store::Result<Vec<RoomKeyWithheldEntry>, Self::Error> {
        let room_id = self.encode_key("direct_withheld_info", room_id);

        self.acquire()
            .await?
            .get_withheld_sessions_by_room_id(room_id)
            .await?
            .into_iter()
            .map(|value| self.deserialize_json(&value))
            .collect()
    }

    async fn get_room_settings(&self, room_id: &RoomId) -> Result<Option<RoomSettings>> {
        let room_id = self.encode_key("room_settings", room_id.as_bytes());
        let Some(value) = self.acquire().await?.get_room_settings(room_id).await? else {
            return Ok(None);
        };

        let settings = self.deserialize_value(&value)?;

        return Ok(Some(settings));
    }

    async fn get_received_room_key_bundle_data(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<StoredRoomKeyBundleData>> {
        let room_id = self.encode_key("received_room_key_bundle", room_id);
        let user_id = self.encode_key("received_room_key_bundle", user_id);
        self.acquire()
            .await?
            .get_received_room_key_bundle(room_id, user_id)
            .await?
            .map(|value| self.deserialize_value(&value))
            .transpose()
    }

    async fn has_downloaded_all_room_keys(&self, room_id: &RoomId) -> Result<bool> {
        let room_id = self.encode_key("room_key_backups_fully_downloaded", room_id);
        self.acquire().await?.has_downloaded_all_room_keys(room_id).await
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

    #[instrument(skip(self))]
    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<Option<CrossProcessLockGeneration>> {
        let key = key.to_owned();
        let holder = holder.to_owned();

        let now: u64 = MilliSecondsSinceUnixEpoch::now().get().into();
        let expiration = now + lease_duration_ms as u64;

        // Learn about the `excluded` keyword in https://sqlite.org/lang_upsert.html.
        let generation = self
            .acquire()
            .await?
            .with_transaction(move |txn| {
                txn.query_row(
                    "INSERT INTO lease_locks (key, holder, expiration)
                    VALUES (?1, ?2, ?3)
                    ON CONFLICT (key)
                    DO
                        UPDATE SET
                            holder = excluded.holder,
                            expiration = excluded.expiration,
                            generation =
                                CASE holder
                                    WHEN excluded.holder THEN generation
                                    ELSE generation + 1
                                END
                        WHERE
                            holder = excluded.holder
                            OR expiration < ?4
                    RETURNING generation
                    ",
                    (key, holder, expiration, now),
                    |row| row.get(0),
                )
                .optional()
            })
            .await?;

        Ok(generation)
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

    async fn get_size(&self) -> Result<Option<usize>, Self::Error> {
        Ok(Some(self.pool.get().await?.get_db_size().await?))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use matrix_sdk_common::deserialized_responses::WithheldCode;
    use matrix_sdk_crypto::{
        cryptostore_integration_tests, cryptostore_integration_tests_time, olm::SenderDataType,
        store::CryptoStore,
    };
    use matrix_sdk_test::async_test;
    use once_cell::sync::Lazy;
    use ruma::{device_id, room_id, user_id};
    use similar_asserts::assert_eq;
    use tempfile::{TempDir, tempdir};
    use tokio::fs;

    use super::SqliteCryptoStore;
    use crate::SqliteStoreConfig;

    static TMP_DIR: Lazy<TempDir> = Lazy::new(|| tempdir().unwrap());

    struct TestDb {
        // Needs to be kept alive because the Drop implementation for TempDir deletes the
        // directory.
        _dir: TempDir,
        database: SqliteCryptoStore,
    }

    fn copy_db(data_path: &str) -> TempDir {
        let db_name = super::DATABASE_NAME;

        let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let database_path = manifest_path.join(data_path).join(db_name);

        let tmpdir = tempdir().unwrap();
        let destination = tmpdir.path().join(db_name);

        // Copy the test database to the tempdir so our test runs are idempotent.
        std::fs::copy(&database_path, destination).unwrap();

        tmpdir
    }

    async fn get_test_db(data_path: &str, passphrase: Option<&str>) -> TestDb {
        let tmpdir = copy_db(data_path);

        let database = SqliteCryptoStore::open(tmpdir.path(), passphrase)
            .await
            .expect("Can't open the test store");

        TestDb { _dir: tmpdir, database }
    }

    #[async_test]
    async fn test_pool_size() {
        let store_open_config =
            SqliteStoreConfig::new(TMP_DIR.path().join("test_pool_size")).pool_max_size(42);

        let store = SqliteCryptoStore::open_with_config(store_open_config).await.unwrap();

        assert_eq!(store.pool.status().max_size, 42);
    }

    /// Test that we didn't regress in our storage layer by loading data from a
    /// pre-filled database, or in other words use a test vector for this.
    #[async_test]
    async fn test_open_test_vector_store() {
        let TestDb { _dir: _, database } = get_test_db("testing/data/storage", None).await;

        let account = database
            .load_account()
            .await
            .unwrap()
            .expect("The test database is prefilled with data, we should find an account");

        let user_id = account.user_id();
        let device_id = account.device_id();

        assert_eq!(
            user_id.as_str(),
            "@pjtest:synapse-oidc.element.dev",
            "The user ID should match to the one we expect."
        );

        assert_eq!(
            device_id.as_str(),
            "v4TqgcuIH6",
            "The device ID should match to the one we expect."
        );

        let device = database
            .get_device(user_id, device_id)
            .await
            .unwrap()
            .expect("Our own device should be found in the store.");

        assert_eq!(device.device_id(), device_id);
        assert_eq!(device.user_id(), user_id);

        assert_eq!(
            device.ed25519_key().expect("The device should have a Ed25519 key.").to_base64(),
            "+cxl1Gl3du5i7UJwfWnoRDdnafFF+xYdAiTYYhYLr8s"
        );

        assert_eq!(
            device.curve25519_key().expect("The device should have a Curve25519 key.").to_base64(),
            "4SL9eEUlpyWSUvjljC5oMjknHQQJY7WZKo5S1KL/5VU"
        );

        let identity = database
            .get_user_identity(user_id)
            .await
            .unwrap()
            .expect("The store should contain an identity.");

        assert_eq!(identity.user_id(), user_id);

        let identity = identity
            .own()
            .expect("The identity should be of the correct type, it should be our own identity.");

        let master_key = identity
            .master_key()
            .get_first_key()
            .expect("Our own identity should have a master key");

        assert_eq!(master_key.to_base64(), "iCUEtB1RwANeqRa5epDrblLk4mer/36sylwQ5hYY3oE");
    }

    /// Test that we didn't regress in our storage layer by loading data from a
    /// pre-filled database, or in other words use a test vector for this.
    #[async_test]
    async fn test_open_test_vector_encrypted_store() {
        let TestDb { _dir: _, database } = get_test_db(
            "testing/data/storage/alice",
            Some(concat!(
                "/rCia2fYAJ+twCZ1Xm2mxFCYcmJdyzkdJjwtgXsziWpYS/UeNxnixuSieuwZXm+x1VsJHmWpl",
                "H+QIQBZpEGZtC9/S/l8xK+WOCesmET0o6yJ/KP73ofDtjBlnNpPwuHLKFpyTbyicpCgQ4UT+5E",
                "UBuJ08TY9Ujdf1D13k5kr5tSZUefDKKCuG1fCRqlU8ByRas1PMQsZxT2W8t7QgBrQiiGmhpo/O",
                "Ti4hfx97GOxncKcxTzppiYQNoHs/f15+XXQD7/oiCcqRIuUlXNsU6hRpFGmbYx2Pi1eyQViQCt",
                "B5dAEiSD0N8U81wXYnpynuTPtnL+hfnOJIn7Sy7mkERQeKg"
            )),
        )
        .await;

        let account = database
            .load_account()
            .await
            .unwrap()
            .expect("The test database is prefilled with data, we should find an account");

        let user_id = account.user_id();
        let device_id = account.device_id();

        assert_eq!(
            user_id.as_str(),
            "@alice:localhost",
            "The user ID should match to the one we expect."
        );

        assert_eq!(
            device_id.as_str(),
            "JVVORTHFXY",
            "The device ID should match to the one we expect."
        );

        let tracked_users =
            database.load_tracked_users().await.expect("Should be tracking some users");

        assert_eq!(tracked_users.len(), 6);

        let known_users = vec![
            user_id!("@alice:localhost"),
            user_id!("@dehydration3:localhost"),
            user_id!("@eve:localhost"),
            user_id!("@bob:localhost"),
            user_id!("@malo:localhost"),
            user_id!("@carl:localhost"),
        ];

        // load the identities
        for user_id in known_users {
            database.get_user_identity(user_id).await.expect("Should load this identity").unwrap();
        }

        let carl_identity =
            database.get_user_identity(user_id!("@carl:localhost")).await.unwrap().unwrap();

        assert_eq!(
            carl_identity.master_key().get_first_key().unwrap().to_base64(),
            "CdhKYYDeBDQveOioXEGWhTPCyzc63Irpar3CNyfun2Q"
        );
        assert!(!carl_identity.was_previously_verified());

        let bob_identity =
            database.get_user_identity(user_id!("@bob:localhost")).await.unwrap().unwrap();

        assert_eq!(
            bob_identity.master_key().get_first_key().unwrap().to_base64(),
            "COh2GYOJWSjem5QPRCaGp9iWV83IELG1IzLKW2S3pFY"
        );
        // Bob is verified so this flag should be set
        assert!(bob_identity.was_previously_verified());

        let known_devices = vec![
            (device_id!("OPXQHCZSKW"), user_id!("@alice:localhost")),
            // a dehydrated one
            (
                device_id!("EvW+9IrGR10KVgVeZP25/KaPfx4R86FofVMcaz7VOho"),
                user_id!("@alice:localhost"),
            ),
            (device_id!("HEEFRFQENV"), user_id!("@alice:localhost")),
            (device_id!("JVVORTHFXY"), user_id!("@alice:localhost")),
            (device_id!("NQUWWSKKHS"), user_id!("@alice:localhost")),
            (device_id!("ORBLPFYCPG"), user_id!("@alice:localhost")),
            (device_id!("YXOWENSEGM"), user_id!("@dehydration3:localhost")),
            (device_id!("VXLFMYCHXC"), user_id!("@bob:localhost")),
            (device_id!("FDGDQAEWOW"), user_id!("@bob:localhost")),
            (device_id!("VXLFMYCHXC"), user_id!("@bob:localhost")),
            (device_id!("FDGDQAEWOW"), user_id!("@bob:localhost")),
            (device_id!("QKUKWJTTQC"), user_id!("@malo:localhost")),
            (device_id!("LOUXJECTFG"), user_id!("@malo:localhost")),
            (device_id!("MKKMAEVLPB"), user_id!("@carl:localhost")),
        ];

        for (device_id, user_id) in known_devices {
            database.get_device(user_id, device_id).await.expect("Should load the device").unwrap();
        }

        let known_sender_key_to_session_count = vec![
            ("FfYcYfDF4nWy+LHdK6CEpIMlFAQDORc30WUkghL06kM", 1),
            ("EvW+9IrGR10KVgVeZP25/KaPfx4R86FofVMcaz7VOho", 1),
            ("hAGsoA4a9M6wwEUX5Q1jux1i+tUngLi01n5AmhDoHTY", 1),
            ("aKqtSJymLzuoglWFwPGk1r/Vm2LE2hFESzXxn4RNjRM", 0),
            ("zHK1psCrgeMn0kaz8hcdvA3INyar9jg1yfrSp0p1pHo", 1),
            ("1QmBA316Wj5jIFRwNOti6N6Xh/vW0bsYCcR4uPfy8VQ", 1),
            ("g5ef2vZF3VXgSPyODIeXpyHIRkuthvLhGvd6uwYggWU", 1),
            ("o7hfupPd1VsNkRIvdlH6ujrEJFSKjFCGbxhAd31XxjI", 1),
            ("Z3RxKQLxY7xpP+ZdOGR2SiNE37SrvmRhW7GPu1UGdm8", 1),
            ("GDomaav8NiY3J+dNEeApJm+O0FooJ3IpVaIyJzCN4w4", 1),
            ("7m7fqkHyEr47V5s/KjaxtJMOr3pSHrrns2q2lWpAQi8", 0),
            ("9psAkPUIF8vNbWbnviX3PlwRcaeO53EHJdNtKpTY1X0", 0),
            ("mqanh+ztw5oRtpqYQgLGW864i6NY2zpoKMIlrcyC+Aw", 0),
            ("fJU/TJdbsv7tVbbpHw1Ke73ziElnM32cNhP2WIg4T10", 0),
            ("sUIeFeFcCZoa5IC6nJ6Vrbvztcyx09m8BBg57XKRClg", 1),
        ];

        for (id, count) in known_sender_key_to_session_count {
            let olm_sessions =
                database.get_sessions(id).await.expect("Should have some olm sessions");

            println!("### Session id: {id:?}");
            assert_eq!(olm_sessions.map_or(0, |v| v.len()), count);
        }

        let inbound_group_sessions = database.get_inbound_group_sessions().await.unwrap();
        assert_eq!(inbound_group_sessions.len(), 15);
        let known_inbound_group_sessions = vec![
            (
                "5hNAxrLai3VI0LKBwfh3wLfksfBFWds0W1a5X5/vSXA",
                room_id!("!SRstFdydzrGwJYtVfm:localhost"),
            ),
            (
                "M6d2eU3y54gaYTbvGSlqa/xc1Az35l56Cp9sxzHWO4g",
                room_id!("!SRstFdydzrGwJYtVfm:localhost"),
            ),
            (
                "IrydwXkRk2N2AqUMIVmLL3oJgMq14R9KId0P/uSD100",
                room_id!("!SRstFdydzrGwJYtVfm:localhost"),
            ),
            (
                "Y74+l9jTo7N5UF+GQwdpgJGe4sn1+QtWITq7BxulHIE",
                room_id!("!SRstFdydzrGwJYtVfm:localhost"),
            ),
            (
                "HpJxQR57WbQGdY6w2Q+C16znVvbXGa+JvQdRoMpWbXg",
                room_id!("!SRstFdydzrGwJYtVfm:localhost"),
            ),
            (
                "Xetvi+ydFkZt8dpONGFbEusQb/Chc2V0XlLByZhsbgE",
                room_id!("!ZIwZcFqZVAYLAqVjfV:localhost"),
            ),
            (
                "wv/WN/39akyerIXczTaIpjAuLnwgXKRtbXFSEHiJqxo",
                room_id!("!ZIwZcFqZVAYLAqVjfV:localhost"),
            ),
            (
                "nA4gQwL//Cm8OdlyjABl/jChbPT/cP5V4Sd8iuE6H0s",
                room_id!("!ZIwZcFqZVAYLAqVjfV:localhost"),
            ),
            (
                "bAAgqFeRDTjfEqL6Qf/c9mk55zoNDCSlboAIRd6b0hw",
                room_id!("!ZIwZcFqZVAYLAqVjfV:localhost"),
            ),
            (
                "exPbsMMdGfAG2qmDdFtpAn+koVprfzS0Zip/RA9QRCE",
                room_id!("!ZIwZcFqZVAYLAqVjfV:localhost"),
            ),
            (
                "h+om7oSw/ZV94fcKaoe8FGXJwQXWOfKQfzbGgNWQILI",
                room_id!("!ZIwZcFqZVAYLAqVjfV:localhost"),
            ),
            (
                "ul3VXonpgk4lO2L3fEWubP/nxsTmLHqu5v8ZM9vHEcw",
                room_id!("!ZIwZcFqZVAYLAqVjfV:localhost"),
            ),
            (
                "JXY15UxC3az2mwg8uX4qwgxfvCM4aygiIWMcdNiVQoc",
                room_id!("!ZIwZcFqZVAYLAqVjfV:localhost"),
            ),
            (
                "OGB9lObr9kWUvha9tB5sMfOF/Mztk24JwQz/nwg3iFQ",
                room_id!("!OgRiTRMaUzLdpCeDBM:localhost"),
            ),
            (
                "SFkHcbxjUOYF7mUAYI/oEMDZFaXszQbCN6Jza7iemj0",
                room_id!("!OgRiTRMaUzLdpCeDBM:localhost"),
            ),
        ];

        // ensure we can load them all
        for (session_id, room_id) in &known_inbound_group_sessions {
            database
                .get_inbound_group_session(room_id, session_id)
                .await
                .expect("Should be able to load inbound group session")
                .unwrap();
        }

        let bob_sender_verified = database
            .get_inbound_group_session(
                room_id!("!ZIwZcFqZVAYLAqVjfV:localhost"),
                "exPbsMMdGfAG2qmDdFtpAn+koVprfzS0Zip/RA9QRCE",
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(bob_sender_verified.sender_data.to_type(), SenderDataType::SenderVerified);
        assert!(bob_sender_verified.backed_up());
        assert!(!bob_sender_verified.has_been_imported());

        let alice_unknown_device = database
            .get_inbound_group_session(
                room_id!("!SRstFdydzrGwJYtVfm:localhost"),
                "IrydwXkRk2N2AqUMIVmLL3oJgMq14R9KId0P/uSD100",
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(alice_unknown_device.sender_data.to_type(), SenderDataType::UnknownDevice);
        assert!(alice_unknown_device.backed_up());
        assert!(alice_unknown_device.has_been_imported());

        let carl_tofu_session = database
            .get_inbound_group_session(
                room_id!("!OgRiTRMaUzLdpCeDBM:localhost"),
                "OGB9lObr9kWUvha9tB5sMfOF/Mztk24JwQz/nwg3iFQ",
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(carl_tofu_session.sender_data.to_type(), SenderDataType::SenderUnverified);
        assert!(carl_tofu_session.backed_up());
        assert!(!carl_tofu_session.has_been_imported());

        // Load outbound sessions
        database
            .get_outbound_group_session(room_id!("!OgRiTRMaUzLdpCeDBM:localhost"))
            .await
            .unwrap()
            .unwrap();
        database
            .get_outbound_group_session(room_id!("!ZIwZcFqZVAYLAqVjfV:localhost"))
            .await
            .unwrap()
            .unwrap();
        database
            .get_outbound_group_session(room_id!("!SRstFdydzrGwJYtVfm:localhost"))
            .await
            .unwrap()
            .unwrap();

        let withheld_info = database
            .get_withheld_info(
                room_id!("!OgRiTRMaUzLdpCeDBM:localhost"),
                "SASgZ+EklvAF4QxJclMlDRlmL0fAMjAJJIKFMdb4Ht0",
            )
            .await
            .expect("This session should be withheld")
            .unwrap();

        assert_eq!(withheld_info.content.withheld_code(), WithheldCode::Unverified);

        let backup_keys = database.load_backup_keys().await.expect("backup key should be cached");
        assert_eq!(backup_keys.backup_version.unwrap(), "6");
        assert!(backup_keys.decryption_key.is_some());
    }

    async fn get_store(
        name: &str,
        passphrase: Option<&str>,
        clear_data: bool,
    ) -> SqliteCryptoStore {
        let tmpdir_path = TMP_DIR.path().join(name);

        if clear_data {
            let _ = fs::remove_dir_all(&tmpdir_path).await;
        }

        SqliteCryptoStore::open(tmpdir_path.to_str().unwrap(), passphrase)
            .await
            .expect("Can't create a secret protected store")
    }

    cryptostore_integration_tests!();
    cryptostore_integration_tests_time!();
}

#[cfg(test)]
mod encrypted_tests {
    use matrix_sdk_crypto::{cryptostore_integration_tests, cryptostore_integration_tests_time};
    use once_cell::sync::Lazy;
    use tempfile::{TempDir, tempdir};
    use tokio::fs;

    use super::SqliteCryptoStore;

    static TMP_DIR: Lazy<TempDir> = Lazy::new(|| tempdir().unwrap());

    async fn get_store(
        name: &str,
        passphrase: Option<&str>,
        clear_data: bool,
    ) -> SqliteCryptoStore {
        let tmpdir_path = TMP_DIR.path().join(name);
        let pass = passphrase.unwrap_or("default_test_password");

        if clear_data {
            let _ = fs::remove_dir_all(&tmpdir_path).await;
        }

        SqliteCryptoStore::open(tmpdir_path.to_str().unwrap(), Some(pass))
            .await
            .expect("Can't create a secret protected store")
    }

    cryptostore_integration_tests!();
    cryptostore_integration_tests_time!();
}
