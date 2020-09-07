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
    collections::{BTreeMap, HashSet},
    convert::TryFrom,
    path::{Path, PathBuf},
    result::Result as StdResult,
    sync::{Arc, Mutex as SyncMutex},
};

use async_trait::async_trait;
use dashmap::{DashMap, DashSet};
use matrix_sdk_common::{
    api::r0::keys::{CrossSigningKey, KeyUsage},
    identifiers::{
        DeviceId, DeviceKeyAlgorithm, DeviceKeyId, EventEncryptionAlgorithm, RoomId, UserId,
    },
    instant::Duration,
    locks::Mutex,
};
use sqlx::{query, query_as, sqlite::SqliteQueryAs, Connect, Executor, SqliteConnection};
use url::Url;
use zeroize::Zeroizing;

use super::{
    caches::{DeviceStore, GroupSessionStore, ReadOnlyUserDevices, SessionStore},
    CryptoStore, CryptoStoreError, Result,
};
use crate::{
    identities::{LocalTrust, ReadOnlyDevice, UserIdentities},
    olm::{
        Account, AccountPickle, IdentityKeys, InboundGroupSession, InboundGroupSessionPickle,
        PickledAccount, PickledInboundGroupSession, PickledSession, PicklingMode, Session,
        SessionPickle,
    },
};

/// SQLite based implementation of a `CryptoStore`.
#[derive(Clone)]
#[cfg_attr(feature = "docs", doc(cfg(r#sqlite_cryptostore)))]
pub struct SqliteStore {
    user_id: Arc<UserId>,
    device_id: Arc<Box<DeviceId>>,
    account_info: Arc<SyncMutex<Option<AccountInfo>>>,
    path: Arc<PathBuf>,

    sessions: SessionStore,
    inbound_group_sessions: GroupSessionStore,
    devices: DeviceStore,
    tracked_users: Arc<DashSet<UserId>>,
    users_for_key_query: Arc<DashSet<UserId>>,

    connection: Arc<Mutex<SqliteConnection>>,
    pickle_passphrase: Arc<Option<Zeroizing<String>>>,
}

#[derive(Clone)]
struct AccountInfo {
    account_id: i64,
    identity_keys: Arc<IdentityKeys>,
}

static DATABASE_NAME: &str = "matrix-sdk-crypto.db";

impl SqliteStore {
    /// Open a new `SqliteStore`.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user for which the store should be
    /// opened.
    ///
    /// * `device_id` - The unique id of the device for which the store should
    /// be opened.
    ///
    /// * `path` - The path where the database file should reside in.
    pub async fn open<P: AsRef<Path>>(
        user_id: &UserId,
        device_id: &DeviceId,
        path: P,
    ) -> Result<SqliteStore> {
        SqliteStore::open_helper(user_id, device_id, path, None).await
    }

    /// Open a new `SqliteStore`.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user for which the store should be
    /// opened.
    ///
    /// * `device_id` - The unique id of the device for which the store should
    /// be opened.
    ///
    /// * `path` - The path where the database file should reside in.
    ///
    /// * `passphrase` - The passphrase that should be used to securely store
    /// the encryption keys.
    pub async fn open_with_passphrase<P: AsRef<Path>>(
        user_id: &UserId,
        device_id: &DeviceId,
        path: P,
        passphrase: &str,
    ) -> Result<SqliteStore> {
        SqliteStore::open_helper(
            user_id,
            device_id,
            path,
            Some(Zeroizing::new(passphrase.to_owned())),
        )
        .await
    }

    fn path_to_url(path: &Path) -> Result<Url> {
        // TODO this returns an empty error if the path isn't absolute.
        let url = Url::from_directory_path(path).expect("Invalid path");
        Ok(url.join(DATABASE_NAME)?)
    }

    async fn open_helper<P: AsRef<Path>>(
        user_id: &UserId,
        device_id: &DeviceId,
        path: P,
        passphrase: Option<Zeroizing<String>>,
    ) -> Result<SqliteStore> {
        let url = SqliteStore::path_to_url(path.as_ref())?;
        let connection = SqliteConnection::connect(url.as_ref()).await?;

        let store = SqliteStore {
            user_id: Arc::new(user_id.to_owned()),
            device_id: Arc::new(device_id.into()),
            account_info: Arc::new(SyncMutex::new(None)),
            sessions: SessionStore::new(),
            inbound_group_sessions: GroupSessionStore::new(),
            devices: DeviceStore::new(),
            path: Arc::new(path.as_ref().to_owned()),
            connection: Arc::new(Mutex::new(connection)),
            pickle_passphrase: Arc::new(passphrase),
            tracked_users: Arc::new(DashSet::new()),
            users_for_key_query: Arc::new(DashSet::new()),
        };
        store.create_tables().await?;

        Ok(store)
    }

    fn account_id(&self) -> Option<i64> {
        self.account_info
            .lock()
            .unwrap()
            .as_ref()
            .map(|i| i.account_id)
    }

    async fn create_tables(&self) -> Result<()> {
        let mut connection = self.connection.lock().await;
        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS accounts (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "user_id" TEXT NOT NULL,
                "device_id" TEXT NOT NULL,
                "pickle" BLOB NOT NULL,
                "shared" INTEGER NOT NULL,
                "uploaded_key_count" INTEGER NOT NULL,
                UNIQUE(user_id,device_id)
            );
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS sessions (
                "session_id" TEXT NOT NULL PRIMARY KEY,
                "account_id" INTEGER NOT NULL,
                "creation_time" TEXT NOT NULL,
                "last_use_time" TEXT NOT NULL,
                "sender_key" TEXT NOT NULL,
                "pickle" BLOB NOT NULL,
                FOREIGN KEY ("account_id") REFERENCES "accounts" ("id")
                    ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS "olmsessions_account_id" ON "sessions" ("account_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS tracked_users (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "account_id" INTEGER NOT NULL,
                "user_id" TEXT NOT NULL,
                "dirty" INTEGER NOT NULL,
                FOREIGN KEY ("account_id") REFERENCES "accounts" ("id")
                    ON DELETE CASCADE
                UNIQUE(account_id,user_id)
            );

            CREATE INDEX IF NOT EXISTS "tracked_users_account_id" ON "tracked_users" ("account_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS inbound_group_sessions (
                "session_id" TEXT NOT NULL PRIMARY KEY,
                "account_id" INTEGER NOT NULL,
                "sender_key" TEXT NOT NULL,
                "signing_key" TEXT NOT NULL,
                "room_id" TEXT NOT NULL,
                "pickle" BLOB NOT NULL,
                FOREIGN KEY ("account_id") REFERENCES "accounts" ("id")
                    ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS "olm_groups_sessions_account_id" ON "inbound_group_sessions" ("account_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS devices (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "account_id" INTEGER NOT NULL,
                "user_id" TEXT NOT NULL,
                "device_id" TEXT NOT NULL,
                "display_name" TEXT,
                "trust_state" INTEGER NOT NULL,
                FOREIGN KEY ("account_id") REFERENCES "accounts" ("id")
                    ON DELETE CASCADE
                UNIQUE(account_id,user_id,device_id)
            );

            CREATE INDEX IF NOT EXISTS "devices_account_id" ON "devices" ("account_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS algorithms (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "device_id" INTEGER NOT NULL,
                "algorithm" TEXT NOT NULL,
                FOREIGN KEY ("device_id") REFERENCES "devices" ("id")
                    ON DELETE CASCADE
                UNIQUE(device_id, algorithm)
            );

            CREATE INDEX IF NOT EXISTS "algorithms_device_id" ON "algorithms" ("device_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS device_keys (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "device_id" INTEGER NOT NULL,
                "algorithm" TEXT NOT NULL,
                "key" TEXT NOT NULL,
                FOREIGN KEY ("device_id") REFERENCES "devices" ("id")
                    ON DELETE CASCADE
                UNIQUE(device_id, algorithm)
            );

            CREATE INDEX IF NOT EXISTS "device_keys_device_id" ON "device_keys" ("device_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS device_signatures (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "device_id" INTEGER NOT NULL,
                "user_id" TEXT NOT NULL,
                "key_algorithm" TEXT NOT NULL,
                "signature" TEXT NOT NULL,
                FOREIGN KEY ("device_id") REFERENCES "devices" ("id")
                    ON DELETE CASCADE
                UNIQUE(device_id, user_id, key_algorithm)
            );

            CREATE INDEX IF NOT EXISTS "device_keys_device_id" ON "device_keys" ("device_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS users (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "account_id" INTEGER NOT NULL,
                "user_id" TEXT NOT NULL,
                FOREIGN KEY ("account_id") REFERENCES "accounts" ("id")
                    ON DELETE CASCADE
                UNIQUE(account_id,user_id)
            );

            CREATE INDEX IF NOT EXISTS "users_account_id" ON "users" ("account_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS user_keys (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "key" TEXT NOT NULL,
                "key_id" TEXT NOT NULL,
                "key_type" TEXT NOT NULL,
                "usage" TEXT NOT NULL,
                "user_id" INTEGER NOT NULL,
                FOREIGN KEY ("user_id") REFERENCES "users" ("id") ON DELETE CASCADE
                UNIQUE(user_id, key_id, key_type)
            );

            CREATE INDEX IF NOT EXISTS "user_keys_user_id" ON "users" ("user_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS user_key_signatures (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "user_id" TEXT NOT NULL,
                "key_id" INTEGER NOT NULL,
                "signature" TEXT NOT NULL,
                "user_key" INTEGER NOT NULL,
                FOREIGN KEY ("user_key") REFERENCES "user_keys" ("id")
                    ON DELETE CASCADE
                UNIQUE(user_id, key_id, user_key)
            );

            CREATE INDEX IF NOT EXISTS "user_keys_device_id" ON "device_keys" ("device_id");
        "#,
            )
            .await?;

        Ok(())
    }

    async fn lazy_load_sessions(&self, sender_key: &str) -> Result<()> {
        let loaded_sessions = self.sessions.get(sender_key).is_some();

        if !loaded_sessions {
            let sessions = self.load_sessions_for(sender_key).await?;

            if !sessions.is_empty() {
                self.sessions.set_for_sender(sender_key, sessions);
            }
        }

        Ok(())
    }

    async fn get_sessions_for(&self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        self.lazy_load_sessions(sender_key).await?;
        Ok(self.sessions.get(sender_key))
    }

    async fn load_sessions_for(&self, sender_key: &str) -> Result<Vec<Session>> {
        let account_info = self
            .account_info
            .lock()
            .unwrap()
            .clone()
            .ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let mut rows: Vec<(String, String, String, String)> = query_as(
            "SELECT pickle, sender_key, creation_time, last_use_time
             FROM sessions WHERE account_id = ? and sender_key = ?",
        )
        .bind(account_info.account_id)
        .bind(sender_key)
        .fetch_all(&mut *connection)
        .await?;

        Ok(rows
            .drain(..)
            .map(|row| {
                let pickle = row.0;
                let sender_key = row.1;
                let creation_time = serde_json::from_str::<Duration>(&row.2)?;
                let last_use_time = serde_json::from_str::<Duration>(&row.3)?;

                let pickle = PickledSession {
                    pickle: SessionPickle::from(pickle),
                    last_use_time,
                    creation_time,
                    sender_key,
                };

                Ok(Session::from_pickle(
                    self.user_id.clone(),
                    self.device_id.clone(),
                    account_info.identity_keys.clone(),
                    pickle,
                    self.get_pickle_mode(),
                )?)
            })
            .collect::<Result<Vec<Session>>>()?)
    }

    async fn load_inbound_group_sessions(&self) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let mut rows: Vec<(String, String, String, String)> = query_as(
            "SELECT pickle, sender_key, signing_key, room_id
             FROM inbound_group_sessions WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_all(&mut *connection)
        .await?;

        let mut group_sessions = rows
            .drain(..)
            .map(|row| {
                let pickle = row.0;
                let sender_key = row.1;
                let signing_key = row.2;
                let room_id = row.3;

                let pickle = PickledInboundGroupSession {
                    pickle: InboundGroupSessionPickle::from(pickle),
                    sender_key,
                    signing_key,
                    room_id: RoomId::try_from(room_id)?,
                    // Fixme we need to store/restore these once we get support
                    // for key requesting/forwarding.
                    forwarding_chains: None,
                };

                Ok(InboundGroupSession::from_pickle(
                    pickle,
                    self.get_pickle_mode(),
                )?)
            })
            .collect::<Result<Vec<InboundGroupSession>>>()?;

        group_sessions
            .drain(..)
            .map(|s| {
                self.inbound_group_sessions.add(s);
            })
            .for_each(drop);

        Ok(())
    }

    async fn save_tracked_user(&self, user: &UserId, dirty: bool) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        query(
            "INSERT INTO tracked_users (
                account_id, user_id, dirty
             ) VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id, user_id) DO UPDATE SET
                dirty = excluded.dirty
             ",
        )
        .bind(account_id)
        .bind(user.to_string())
        .bind(dirty)
        .execute(&mut *connection)
        .await?;

        Ok(())
    }

    async fn load_tracked_users(&self) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let rows: Vec<(String, bool)> = query_as(
            "SELECT user_id, dirty
             FROM tracked_users WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_all(&mut *connection)
        .await?;

        for row in rows {
            let user_id: &str = &row.0;
            let dirty: bool = row.1;

            if let Ok(u) = UserId::try_from(user_id) {
                self.tracked_users.insert(u.clone());
                if dirty {
                    self.users_for_key_query.insert(u);
                }
            } else {
                continue;
            };
        }

        Ok(())
    }

    async fn load_devices(&self) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let rows: Vec<(i64, String, String, Option<String>, i64)> = query_as(
            "SELECT id, user_id, device_id, display_name, trust_state
             FROM devices WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_all(&mut *connection)
        .await?;

        for row in rows {
            let device_row_id = row.0;
            let user_id: &str = &row.1;
            let user_id = if let Ok(u) = UserId::try_from(user_id) {
                u
            } else {
                continue;
            };

            let device_id = &row.2.to_string();
            let display_name = &row.3;
            let trust_state = LocalTrust::from(row.4);

            let algorithm_rows: Vec<(String,)> =
                query_as("SELECT algorithm FROM algorithms WHERE device_id = ?")
                    .bind(device_row_id)
                    .fetch_all(&mut *connection)
                    .await?;

            let algorithms = algorithm_rows
                .iter()
                .map(|row| {
                    let algorithm: &str = &row.0;
                    EventEncryptionAlgorithm::from(algorithm)
                })
                .collect::<Vec<EventEncryptionAlgorithm>>();

            let key_rows: Vec<(String, String)> =
                query_as("SELECT algorithm, key FROM device_keys WHERE device_id = ?")
                    .bind(device_row_id)
                    .fetch_all(&mut *connection)
                    .await?;

            let keys: BTreeMap<DeviceKeyId, String> = key_rows
                .into_iter()
                .filter_map(|row| {
                    let algorithm = row.0.parse::<DeviceKeyAlgorithm>().ok()?;
                    let key = row.1;

                    Some((
                        DeviceKeyId::from_parts(algorithm, device_id.as_str().into()),
                        key,
                    ))
                })
                .collect();

            let signature_rows: Vec<(String, String, String)> = query_as(
                "SELECT user_id, key_algorithm, signature
                         FROM device_signatures WHERE device_id = ?",
            )
            .bind(device_row_id)
            .fetch_all(&mut *connection)
            .await?;

            let mut signatures: BTreeMap<UserId, BTreeMap<DeviceKeyId, String>> = BTreeMap::new();

            for row in signature_rows {
                let user_id = if let Ok(u) = UserId::try_from(&*row.0) {
                    u
                } else {
                    continue;
                };

                let key_algorithm = if let Ok(k) = row.1.parse::<DeviceKeyAlgorithm>() {
                    k
                } else {
                    continue;
                };

                let signature = row.2;

                signatures
                    .entry(user_id)
                    .or_insert_with(BTreeMap::new)
                    .insert(
                        DeviceKeyId::from_parts(key_algorithm, device_id.as_str().into()),
                        signature.to_owned(),
                    );
            }

            let device = ReadOnlyDevice::new(
                user_id,
                device_id.as_str().into(),
                display_name.clone(),
                trust_state,
                algorithms,
                keys,
                signatures,
            );

            self.devices.add(device);
        }

        Ok(())
    }

    async fn save_device_helper(&self, device: ReadOnlyDevice) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;

        let mut connection = self.connection.lock().await;

        query(
            "INSERT INTO devices (
                account_id, user_id, device_id,
                display_name, trust_state
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(account_id, user_id, device_id) DO UPDATE SET
                display_name = excluded.display_name,
                trust_state = excluded.trust_state
             ",
        )
        .bind(account_id)
        .bind(device.user_id().as_str())
        .bind(device.device_id().as_str())
        .bind(device.display_name())
        .bind(device.trust_state() as i64)
        .execute(&mut *connection)
        .await?;

        let row: (i64,) = query_as(
            "SELECT id FROM devices
                      WHERE user_id = ? and device_id = ?",
        )
        .bind(device.user_id().as_str())
        .bind(device.device_id().as_str())
        .fetch_one(&mut *connection)
        .await?;

        let device_row_id = row.0;

        for algorithm in device.algorithms() {
            query(
                "INSERT OR IGNORE INTO algorithms (
                    device_id, algorithm
                 ) VALUES (?1, ?2)
                 ",
            )
            .bind(device_row_id)
            .bind(algorithm.to_string())
            .execute(&mut *connection)
            .await?;
        }

        for (key_id, key) in device.keys() {
            query(
                "INSERT OR IGNORE INTO device_keys (
                    device_id, algorithm, key
                 ) VALUES (?1, ?2, ?3)
                 ",
            )
            .bind(device_row_id)
            .bind(key_id.algorithm().to_string())
            .bind(key)
            .execute(&mut *connection)
            .await?;
        }

        for (user_id, signature_map) in device.signatures() {
            for (key_id, signature) in signature_map {
                query(
                    "INSERT OR IGNORE INTO device_signatures (
                        device_id, user_id, key_algorithm, signature
                     ) VALUES (?1, ?2, ?3, ?4)
                     ",
                )
                .bind(device_row_id)
                .bind(user_id.as_str())
                .bind(key_id.algorithm().to_string())
                .bind(signature)
                .execute(&mut *connection)
                .await?;
            }
        }

        Ok(())
    }

    fn get_pickle_mode(&self) -> PicklingMode {
        match &*self.pickle_passphrase {
            Some(p) => PicklingMode::Encrypted {
                key: p.as_bytes().to_vec(),
            },
            None => PicklingMode::Unencrypted,
        }
    }

    async fn load_user(&self, user_id: &UserId) -> Result<Option<UserIdentities>> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let row: Option<(i64,)> =
            query_as("SELECT id FROM users WHERE account_id = ? and user_id = ?")
                .bind(account_id)
                .bind(user_id.as_str())
                .fetch_optional(&mut *connection)
                .await?;

        let user_row_id = if let Some(row) = row {
            row.0
        } else {
            return Ok(None);
        };

        let key_rows: Vec<(i64, String, String, String)> = query_as(
            "SELECT id, key_id, key, usage FROM user_keys WHERE user_id = ? and key_type = ?",
        )
        .bind(user_row_id)
        .bind("master_key")
        .fetch_all(&mut *connection)
        .await?;

        let mut keys = BTreeMap::new();
        let mut signatures = BTreeMap::new();
        let mut key_usage = HashSet::new();

        for row in key_rows {
            let key_row_id = row.0;
            let key_id = row.1;
            let key = row.2;
            let usage: Vec<String> = serde_json::from_str(&row.3)?;

            keys.insert(key_id, key);
            key_usage.extend(usage);

            let mut signature_rows: Vec<(String, String, String)> = query_as(
                "SELECT user_id, key_id, signature, FROM user_key_signatures WHERE user_key = ?",
            )
            .bind(user_row_id)
            .bind("master_key")
            .fetch_all(&mut *connection)
            .await?;

            for row in signature_rows.drain(..) {
                let user_id = if let Ok(u) = UserId::try_from(row.0) {
                    u
                } else {
                    continue;
                };

                let key_id = row.1;
                let signature = row.2;

                signatures
                    .entry(user_id)
                    .or_insert_with(BTreeMap::new)
                    .insert(key_id, signature);
            }
        }

        let usage: Vec<KeyUsage> = key_usage
            .iter()
            .filter_map(|u| serde_json::from_str(u).ok())
            .collect();

        let key = CrossSigningKey {
            user_id: user_id.to_owned(),
            usage,
            keys,
            signatures,
        };

        Ok(None)
    }

    async fn save_user_helper(&self, user: &UserIdentities) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;

        let mut connection = self.connection.lock().await;

        query(
            "INSERT OR IGNORE INTO users (
                account_id, user_id
             ) VALUES (?1, ?2)
             ",
        )
        .bind(account_id)
        .bind(user.user_id().as_str())
        .execute(&mut *connection)
        .await?;

        let row: (i64,) = query_as(
            "SELECT id FROM users
                WHERE account_id = ? and user_id = ?",
        )
        .bind(account_id)
        .bind(user.user_id().as_str())
        .fetch_one(&mut *connection)
        .await?;

        let user_row_id = row.0;

        for (key_id, key) in user.master_key() {
            query(
                "INSERT OR IGNORE INTO user_keys (
                    user_id, key_type, key_id, key, usage
                 ) VALUES (?1, ?2, ?3, ?4, ?5)
                 ",
            )
            .bind(user_row_id)
            .bind("master_key")
            .bind(key_id.as_str())
            .bind(key)
            .bind(serde_json::to_string(user.master_key().usage())?)
            .execute(&mut *connection)
            .await?;

            let row: (i64,) = query_as(
                "SELECT id FROM user_keys
                    WHERE user_id = ? and key_id = ? and key_type = ?",
            )
            .bind(user_row_id)
            .bind(key_id.as_str())
            .bind("master_key")
            .fetch_one(&mut *connection)
            .await?;

            let key_row_id = row.0;

            for (user_id, signature_map) in user.master_key().signatures() {
                for (key_id, signature) in signature_map {
                    query(
                        "INSERT OR IGNORE INTO user_key_signatures (
                            user_key, user_id, key_id, signature
                         ) VALUES (?1, ?2, ?3, ?4)
                         ",
                    )
                    .bind(key_row_id)
                    .bind(user_id.as_str())
                    .bind(key_id.as_str())
                    .bind(signature)
                    .execute(&mut *connection)
                    .await?;
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl CryptoStore for SqliteStore {
    async fn load_account(&self) -> Result<Option<Account>> {
        let mut connection = self.connection.lock().await;

        let row: Option<(i64, String, bool, i64)> = query_as(
            "SELECT id, pickle, shared, uploaded_key_count FROM accounts
                      WHERE user_id = ? and device_id = ?",
        )
        .bind(self.user_id.as_str())
        .bind(self.device_id.as_str())
        .fetch_optional(&mut *connection)
        .await?;

        let result = if let Some((id, pickle, shared, uploaded_key_count)) = row {
            let pickle = PickledAccount {
                user_id: (&*self.user_id).clone(),
                device_id: (&*self.device_id).clone(),
                pickle: AccountPickle::from(pickle),
                shared,
                uploaded_signed_key_count: uploaded_key_count,
            };

            let account = Account::from_pickle(pickle, self.get_pickle_mode())?;

            *self.account_info.lock().unwrap() = Some(AccountInfo {
                account_id: id,
                identity_keys: account.identity_keys.clone(),
            });

            Some(account)
        } else {
            return Ok(None);
        };

        drop(connection);

        self.load_inbound_group_sessions().await?;
        self.load_devices().await?;
        self.load_tracked_users().await?;

        Ok(result)
    }

    async fn save_account(&self, account: Account) -> Result<()> {
        let pickle = account.pickle(self.get_pickle_mode()).await;
        let mut connection = self.connection.lock().await;

        query(
            "INSERT INTO accounts (
                user_id, device_id, pickle, shared, uploaded_key_count
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(user_id, device_id) DO UPDATE SET
                pickle = excluded.pickle,
                shared = excluded.shared
             ",
        )
        .bind(pickle.user_id.as_str())
        .bind(pickle.device_id.as_str())
        .bind(pickle.pickle.as_str())
        .bind(pickle.shared)
        .bind(pickle.uploaded_signed_key_count)
        .execute(&mut *connection)
        .await?;

        let account_id: (i64,) =
            query_as("SELECT id FROM accounts WHERE user_id = ? and device_id = ?")
                .bind(self.user_id.as_str())
                .bind(self.device_id.as_str())
                .fetch_one(&mut *connection)
                .await?;

        *self.account_info.lock().unwrap() = Some(AccountInfo {
            account_id: account_id.0,
            identity_keys: account.identity_keys.clone(),
        });

        Ok(())
    }

    async fn save_sessions(&self, sessions: &[Session]) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;

        // TODO turn this into a transaction
        for session in sessions {
            self.lazy_load_sessions(&session.sender_key).await?;
            self.sessions.add(session.clone()).await;

            let pickle = session.pickle(self.get_pickle_mode()).await;

            let session_id = session.session_id();
            let creation_time = serde_json::to_string(&pickle.creation_time)?;
            let last_use_time = serde_json::to_string(&pickle.last_use_time)?;

            let mut connection = self.connection.lock().await;

            query(
                "REPLACE INTO sessions (
                    session_id, account_id, creation_time, last_use_time, sender_key, pickle
                 ) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&session_id)
            .bind(&account_id)
            .bind(&*creation_time)
            .bind(&*last_use_time)
            .bind(&pickle.sender_key)
            .bind(&pickle.pickle.as_str())
            .execute(&mut *connection)
            .await?;
        }

        Ok(())
    }

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        Ok(self.get_sessions_for(sender_key).await?)
    }

    async fn save_inbound_group_session(&self, session: InboundGroupSession) -> Result<bool> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let pickle = session.pickle(self.get_pickle_mode()).await;
        let mut connection = self.connection.lock().await;
        let session_id = session.session_id();

        // FIXME we need to store/restore the forwarding chains.
        // FIXME this should be converted so it accepts an array of sessions for
        // the key import feature.

        query(
            "INSERT INTO inbound_group_sessions (
                session_id, account_id, sender_key, signing_key,
                room_id, pickle
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id) DO UPDATE SET
                pickle = excluded.pickle
             ",
        )
        .bind(session_id)
        .bind(account_id)
        .bind(pickle.sender_key)
        .bind(pickle.signing_key)
        .bind(pickle.room_id.as_str())
        .bind(pickle.pickle.as_str())
        .execute(&mut *connection)
        .await?;

        Ok(self.inbound_group_sessions.add(session))
    }

    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        Ok(self
            .inbound_group_sessions
            .get(room_id, sender_key, session_id))
    }

    fn is_user_tracked(&self, user_id: &UserId) -> bool {
        self.tracked_users.contains(user_id)
    }

    fn has_users_for_key_query(&self) -> bool {
        !self.users_for_key_query.is_empty()
    }

    fn users_for_key_query(&self) -> HashSet<UserId> {
        #[allow(clippy::map_clone)]
        self.users_for_key_query.iter().map(|u| u.clone()).collect()
    }

    async fn update_tracked_user(&self, user: &UserId, dirty: bool) -> Result<bool> {
        let already_added = self.tracked_users.insert(user.clone());

        if dirty {
            self.users_for_key_query.insert(user.clone());
        } else {
            self.users_for_key_query.remove(user);
        }

        self.save_tracked_user(user, dirty).await?;

        Ok(already_added)
    }

    async fn save_devices(&self, devices: &[ReadOnlyDevice]) -> Result<()> {
        // TODO turn this into a bulk transaction.
        for device in devices {
            self.devices.add(device.clone());
            self.save_device_helper(device.clone()).await?
        }

        Ok(())
    }

    async fn delete_device(&self, device: ReadOnlyDevice) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        query(
            "DELETE FROM devices
             WHERE account_id = ?1 and user_id = ?2 and device_id = ?3
             ",
        )
        .bind(account_id)
        .bind(&device.user_id().to_string())
        .bind(device.device_id().as_str())
        .execute(&mut *connection)
        .await?;

        Ok(())
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>> {
        Ok(self.devices.get(user_id, device_id))
    }

    async fn get_user_devices(&self, user_id: &UserId) -> Result<ReadOnlyUserDevices> {
        Ok(self.devices.user_devices(user_id))
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<UserIdentities>> {
        self.load_user(user_id).await
    }

    async fn save_user_identities(&self, users: &[UserIdentities]) -> Result<()> {
        for user in users {
            self.save_user_helper(user).await?;
        }

        Ok(())
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> StdResult<(), std::fmt::Error> {
        fmt.debug_struct("SqliteStore")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .field("path", &self.path)
            .finish()
    }
}

#[cfg(test)]
mod test {
    use crate::{
        identities::{device::test::get_device, user::test::get_own_identity},
        olm::{Account, GroupSessionKey, InboundGroupSession, Session},
    };
    use matrix_sdk_common::{
        api::r0::keys::SignedKey,
        identifiers::{room_id, user_id, DeviceId, UserId},
    };
    use olm_rs::outbound_group_session::OlmOutboundGroupSession;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    use super::{CryptoStore, SqliteStore};

    fn alice_id() -> UserId {
        user_id!("@alice:example.org")
    }

    fn alice_device_id() -> Box<DeviceId> {
        "ALICEDEVICE".into()
    }

    fn bob_id() -> UserId {
        user_id!("@bob:example.org")
    }

    fn bob_device_id() -> Box<DeviceId> {
        "BOBDEVICE".into()
    }

    async fn get_store(passphrase: Option<&str>) -> (SqliteStore, tempfile::TempDir) {
        let tmpdir = tempdir().unwrap();
        let tmpdir_path = tmpdir.path().to_str().unwrap();

        let store = if let Some(passphrase) = passphrase {
            SqliteStore::open_with_passphrase(
                &alice_id(),
                &alice_device_id(),
                tmpdir_path,
                passphrase,
            )
            .await
            .expect("Can't create a passphrase protected store")
        } else {
            SqliteStore::open(&alice_id(), &alice_device_id(), tmpdir_path)
                .await
                .expect("Can't create store")
        };

        (store, tmpdir)
    }

    async fn get_loaded_store() -> (Account, SqliteStore, tempfile::TempDir) {
        let (store, dir) = get_store(None).await;
        let account = get_account();
        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        (account, store, dir)
    }

    fn get_account() -> Account {
        Account::new(&alice_id(), &alice_device_id())
    }

    async fn get_account_and_session() -> (Account, Session) {
        let alice = Account::new(&alice_id(), &alice_device_id());
        let bob = Account::new(&bob_id(), &bob_device_id());

        bob.generate_one_time_keys_helper(1).await;
        let one_time_key = bob
            .one_time_keys()
            .await
            .curve25519()
            .iter()
            .next()
            .unwrap()
            .1
            .to_owned();
        let one_time_key = SignedKey {
            key: one_time_key,
            signatures: BTreeMap::new(),
        };
        let sender_key = bob.identity_keys().curve25519().to_owned();
        let session = alice
            .create_outbound_session_helper(&sender_key, &one_time_key)
            .await
            .unwrap();

        (alice, session)
    }

    #[tokio::test]
    async fn create_store() {
        let tmpdir = tempdir().unwrap();
        let tmpdir_path = tmpdir.path().to_str().unwrap();
        let _ = SqliteStore::open(&alice_id(), &alice_device_id(), tmpdir_path)
            .await
            .expect("Can't create store");
    }

    #[tokio::test]
    async fn save_account() {
        let (store, _dir) = get_store(None).await;
        assert!(store.load_account().await.unwrap().is_none());
        let account = get_account();

        store
            .save_account(account)
            .await
            .expect("Can't save account");
    }

    #[tokio::test]
    async fn load_account() {
        let (store, _dir) = get_store(None).await;
        let account = get_account();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(account, loaded_account);
    }

    #[tokio::test]
    async fn load_account_with_passphrase() {
        let (store, _dir) = get_store(Some("secret_passphrase")).await;
        let account = get_account();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(account, loaded_account);
    }

    #[tokio::test]
    async fn save_and_share_account() {
        let (store, _dir) = get_store(None).await;
        let account = get_account();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        account.mark_as_shared();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(account, loaded_account);
    }

    #[tokio::test]
    async fn save_session() {
        let (store, _dir) = get_store(None).await;
        let (account, session) = get_account_and_session().await;

        assert!(store.save_sessions(&[session.clone()]).await.is_err());

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        store.save_sessions(&[session]).await.unwrap();
    }

    #[tokio::test]
    async fn load_sessions() {
        let (store, _dir) = get_store(None).await;
        let (account, session) = get_account_and_session().await;
        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");
        store.save_sessions(&[session.clone()]).await.unwrap();

        let sessions = store
            .load_sessions_for(&session.sender_key)
            .await
            .expect("Can't load sessions");
        let loaded_session = &sessions[0];

        assert_eq!(&session, loaded_session);
    }

    #[tokio::test]
    async fn add_and_save_session() {
        let (store, dir) = get_store(None).await;
        let (account, session) = get_account_and_session().await;
        let sender_key = session.sender_key.to_owned();
        let session_id = session.session_id().to_owned();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");
        store.save_sessions(&[session]).await.unwrap();

        let sessions = store.get_sessions(&sender_key).await.unwrap().unwrap();
        let sessions_lock = sessions.lock().await;
        let session = &sessions_lock[0];

        assert_eq!(session_id, session.session_id());

        drop(store);

        let store = SqliteStore::open(&alice_id(), &alice_device_id(), dir.path())
            .await
            .expect("Can't create store");

        let loaded_account = store.load_account().await.unwrap().unwrap();
        assert_eq!(account, loaded_account);

        let sessions = store.get_sessions(&sender_key).await.unwrap().unwrap();
        let sessions_lock = sessions.lock().await;
        let session = &sessions_lock[0];

        assert_eq!(session_id, session.session_id());
    }

    #[tokio::test]
    async fn save_inbound_group_session() {
        let (account, store, _dir) = get_loaded_store().await;

        let identity_keys = account.identity_keys();
        let outbound_session = OlmOutboundGroupSession::new();
        let session = InboundGroupSession::new(
            identity_keys.curve25519(),
            identity_keys.ed25519(),
            &room_id!("!test:localhost"),
            GroupSessionKey(outbound_session.session_key()),
        )
        .expect("Can't create session");

        store
            .save_inbound_group_session(session)
            .await
            .expect("Can't save group session");
    }

    #[tokio::test]
    async fn load_inbound_group_session() {
        let (account, store, _dir) = get_loaded_store().await;

        let identity_keys = account.identity_keys();
        let outbound_session = OlmOutboundGroupSession::new();
        let session = InboundGroupSession::new(
            identity_keys.curve25519(),
            identity_keys.ed25519(),
            &room_id!("!test:localhost"),
            GroupSessionKey(outbound_session.session_key()),
        )
        .expect("Can't create session");

        store
            .save_inbound_group_session(session.clone())
            .await
            .expect("Can't save group session");

        store.load_inbound_group_sessions().await.unwrap();

        let loaded_session = store
            .get_inbound_group_session(&session.room_id, &session.sender_key, session.session_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session, loaded_session);
    }

    #[tokio::test]
    async fn test_tracked_users() {
        let (_account, store, dir) = get_loaded_store().await;
        let device = get_device();

        assert!(store
            .update_tracked_user(device.user_id(), false)
            .await
            .unwrap());
        assert!(!store
            .update_tracked_user(device.user_id(), false)
            .await
            .unwrap());

        assert!(store.is_user_tracked(device.user_id()));
        assert!(!store.users_for_key_query().contains(device.user_id()));
        assert!(!store
            .update_tracked_user(device.user_id(), true)
            .await
            .unwrap());
        assert!(store.users_for_key_query().contains(device.user_id()));
        drop(store);

        let store = SqliteStore::open(&alice_id(), &alice_device_id(), dir.path())
            .await
            .expect("Can't create store");

        store.load_account().await.unwrap();

        assert!(store.is_user_tracked(device.user_id()));
        assert!(store.users_for_key_query().contains(device.user_id()));

        store
            .update_tracked_user(device.user_id(), false)
            .await
            .unwrap();
        assert!(!store.users_for_key_query().contains(device.user_id()));

        let store = SqliteStore::open(&alice_id(), &alice_device_id(), dir.path())
            .await
            .expect("Can't create store");

        store.load_account().await.unwrap();

        assert!(!store.users_for_key_query().contains(device.user_id()));
    }

    #[tokio::test]
    async fn device_saving() {
        let (_account, store, dir) = get_loaded_store().await;
        let device = get_device();

        store.save_devices(&[device.clone()]).await.unwrap();

        drop(store);

        let store = SqliteStore::open(&alice_id(), &alice_device_id(), dir.path())
            .await
            .expect("Can't create store");

        store.load_account().await.unwrap();

        let loaded_device = store
            .get_device(device.user_id(), device.device_id())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(device, loaded_device);

        for algorithm in loaded_device.algorithms() {
            assert!(device.algorithms().contains(algorithm));
        }
        assert_eq!(device.algorithms().len(), loaded_device.algorithms().len());
        assert_eq!(device.keys(), loaded_device.keys());

        let user_devices = store.get_user_devices(device.user_id()).await.unwrap();
        assert_eq!(user_devices.keys().next().unwrap(), device.device_id());
        assert_eq!(user_devices.devices().next().unwrap(), &device);
    }

    #[tokio::test]
    async fn device_deleting() {
        let (_account, store, dir) = get_loaded_store().await;
        let device = get_device();

        store.save_devices(&[device.clone()]).await.unwrap();
        store.delete_device(device.clone()).await.unwrap();

        let store = SqliteStore::open(&alice_id(), &alice_device_id(), dir.path())
            .await
            .expect("Can't create store");

        store.load_account().await.unwrap();

        let loaded_device = store
            .get_device(device.user_id(), device.device_id())
            .await
            .unwrap();

        assert!(loaded_device.is_none());
    }

    #[tokio::test]
    async fn user_saving() {
        let (_account, store, dir) = get_loaded_store().await;
        let own_identity = get_own_identity();

        store
            .save_user_identities(&[own_identity.into()])
            .await
            .expect("Can't save identity");

        drop(store);

        // let store = SqliteStore::open(&alice_id(), &alice_device_id(), dir.path())
        //     .await
        //     .expect("Can't create store");

        // store.load_account().await.unwrap();

        // let loaded_device = store
        //     .get_device(device.user_id(), device.device_id())
        //     .await
        //     .unwrap()
        //     .unwrap();

        // assert_eq!(device, loaded_device);

        // for algorithm in loaded_device.algorithms() {
        //     assert!(device.algorithms().contains(algorithm));
        // }
        // assert_eq!(device.algorithms().len(), loaded_device.algorithms().len());
        // assert_eq!(device.keys(), loaded_device.keys());

        // let user_devices = store.get_user_devices(device.user_id()).await.unwrap();
        // assert_eq!(user_devices.keys().next().unwrap(), device.device_id());
        // assert_eq!(user_devices.devices().next().unwrap(), &device);
    }
}
