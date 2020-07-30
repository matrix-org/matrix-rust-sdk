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

use matrix_sdk_common::instant::{Duration, Instant};
use std::{
    collections::{BTreeMap, HashSet},
    convert::TryFrom,
    path::{Path, PathBuf},
    result::Result as StdResult,
    sync::Arc,
};
use url::Url;

use async_trait::async_trait;
use matrix_sdk_common::locks::Mutex;
use olm_rs::PicklingMode;
use sqlx::{query, query_as, sqlite::SqliteQueryAs, Connect, Executor, SqliteConnection};
use zeroize::Zeroizing;

use super::{CryptoStore, CryptoStoreError, Result};
use crate::{
    device::{Device, TrustState},
    memory_stores::{DeviceStore, GroupSessionStore, SessionStore, UserDevices},
    Account, IdentityKeys, InboundGroupSession, Session,
};
use matrix_sdk_common::{
    api::r0::keys::{AlgorithmAndDeviceId, KeyAlgorithm},
    events::Algorithm,
    identifiers::{DeviceId, RoomId, UserId},
};

/// SQLite based implementation of a `CryptoStore`.
pub struct SqliteStore {
    user_id: Arc<UserId>,
    device_id: Arc<Box<DeviceId>>,
    account_info: Option<AccountInfo>,
    path: PathBuf,

    sessions: SessionStore,
    inbound_group_sessions: GroupSessionStore,
    devices: DeviceStore,
    tracked_users: HashSet<UserId>,
    users_for_key_query: HashSet<UserId>,

    connection: Arc<Mutex<SqliteConnection>>,
    pickle_passphrase: Option<Zeroizing<String>>,
}

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
            account_info: None,
            sessions: SessionStore::new(),
            inbound_group_sessions: GroupSessionStore::new(),
            devices: DeviceStore::new(),
            path: path.as_ref().to_owned(),
            connection: Arc::new(Mutex::new(connection)),
            pickle_passphrase: passphrase,
            tracked_users: HashSet::new(),
            users_for_key_query: HashSet::new(),
        };
        store.create_tables().await?;
        Ok(store)
    }

    fn account_id(&self) -> Option<i64> {
        self.account_info.as_ref().map(|i| i.account_id)
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

        Ok(())
    }

    async fn lazy_load_sessions(&mut self, sender_key: &str) -> Result<()> {
        let loaded_sessions = self.sessions.get(sender_key).is_some();

        if !loaded_sessions {
            let sessions = self.load_sessions_for(sender_key).await?;

            if !sessions.is_empty() {
                self.sessions.set_for_sender(sender_key, sessions);
            }
        }

        Ok(())
    }

    async fn get_sessions_for(
        &mut self,
        sender_key: &str,
    ) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        self.lazy_load_sessions(sender_key).await?;
        Ok(self.sessions.get(sender_key))
    }

    async fn load_sessions_for(&mut self, sender_key: &str) -> Result<Vec<Session>> {
        let account_info = self
            .account_info
            .as_ref()
            .ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let rows: Vec<(String, String, String, String)> = query_as(
            "SELECT pickle, sender_key, creation_time, last_use_time
             FROM sessions WHERE account_id = ? and sender_key = ?",
        )
        .bind(account_info.account_id)
        .bind(sender_key)
        .fetch_all(&mut *connection)
        .await?;

        let now = Instant::now();

        Ok(rows
            .iter()
            .map(|row| {
                let pickle = &row.0;
                let sender_key = &row.1;
                let creation_time = now
                    .checked_sub(serde_json::from_str::<Duration>(&row.2)?)
                    .ok_or(CryptoStoreError::SessionTimestampError)?;
                let last_use_time = now
                    .checked_sub(serde_json::from_str::<Duration>(&row.3)?)
                    .ok_or(CryptoStoreError::SessionTimestampError)?;

                Ok(Session::from_pickle(
                    self.user_id.clone(),
                    self.device_id.clone(),
                    account_info.identity_keys.clone(),
                    pickle.to_string(),
                    self.get_pickle_mode(),
                    sender_key.to_string(),
                    creation_time,
                    last_use_time,
                )?)
            })
            .collect::<Result<Vec<Session>>>()?)
    }

    async fn load_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let rows: Vec<(String, String, String, String)> = query_as(
            "SELECT pickle, sender_key, signing_key, room_id
             FROM inbound_group_sessions WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_all(&mut *connection)
        .await?;

        Ok(rows
            .iter()
            .map(|row| {
                let pickle = &row.0;
                let sender_key = &row.1;
                let signing_key = &row.2;
                let room_id = &row.3;

                Ok(InboundGroupSession::from_pickle(
                    pickle.to_string(),
                    self.get_pickle_mode(),
                    sender_key.to_string(),
                    signing_key.to_owned(),
                    RoomId::try_from(room_id.as_str()).unwrap(),
                )?)
            })
            .collect::<Result<Vec<InboundGroupSession>>>()?)
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

    async fn load_tracked_users(&self) -> Result<(HashSet<UserId>, HashSet<UserId>)> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let rows: Vec<(String, bool)> = query_as(
            "SELECT user_id, dirty
             FROM tracked_users WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_all(&mut *connection)
        .await?;

        let mut users = HashSet::new();
        let mut users_for_query = HashSet::new();

        for row in rows {
            let user_id: &str = &row.0;
            let dirty: bool = row.1;

            if let Ok(u) = UserId::try_from(user_id) {
                users.insert(u.clone());
                if dirty {
                    users_for_query.insert(u);
                }
            } else {
                continue;
            };
        }

        Ok((users, users_for_query))
    }

    async fn load_devices(&self) -> Result<DeviceStore> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let rows: Vec<(i64, String, String, Option<String>, i64)> = query_as(
            "SELECT id, user_id, device_id, display_name, trust_state
             FROM devices WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_all(&mut *connection)
        .await?;

        let store = DeviceStore::new();

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
            let trust_state = TrustState::from(row.4);

            let algorithm_rows: Vec<(String,)> =
                query_as("SELECT algorithm FROM algorithms WHERE device_id = ?")
                    .bind(device_row_id)
                    .fetch_all(&mut *connection)
                    .await?;

            let algorithms = algorithm_rows
                .iter()
                .map(|row| {
                    let algorithm: &str = &row.0;
                    Algorithm::from(algorithm)
                })
                .collect::<Vec<Algorithm>>();

            let key_rows: Vec<(String, String)> =
                query_as("SELECT algorithm, key FROM device_keys WHERE device_id = ?")
                    .bind(device_row_id)
                    .fetch_all(&mut *connection)
                    .await?;

            let keys: BTreeMap<AlgorithmAndDeviceId, String> = key_rows
                .into_iter()
                .filter_map(|row| {
                    let algorithm = KeyAlgorithm::try_from(&*row.0).ok()?;
                    let key = row.1;

                    Some((
                        AlgorithmAndDeviceId(algorithm, device_id.as_str().into()),
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

            let mut signatures: BTreeMap<UserId, BTreeMap<AlgorithmAndDeviceId, String>> =
                BTreeMap::new();

            for row in signature_rows {
                let user_id = if let Ok(u) = UserId::try_from(&*row.0) {
                    u
                } else {
                    continue;
                };

                let key_algorithm = if let Ok(k) = KeyAlgorithm::try_from(&*row.1) {
                    k
                } else {
                    continue;
                };

                let signature = row.2;

                if !signatures.contains_key(&user_id) {
                    let _ = signatures.insert(user_id.clone(), BTreeMap::new());
                }
                let user_map = signatures.get_mut(&user_id).unwrap();

                user_map.insert(
                    AlgorithmAndDeviceId(key_algorithm, device_id.as_str().into()),
                    signature.to_owned(),
                );
            }

            let device = Device::new(
                user_id,
                device_id.as_str().into(),
                display_name.clone(),
                trust_state,
                algorithms,
                keys,
                signatures,
            );

            store.add(device);
        }

        Ok(store)
    }

    async fn save_device_helper(&self, device: Device) -> Result<()> {
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

        for (key_algorithm, key) in device.keys() {
            query(
                "INSERT OR IGNORE INTO device_keys (
                    device_id, algorithm, key
                 ) VALUES (?1, ?2, ?3)
                 ",
            )
            .bind(device_row_id)
            .bind(key_algorithm.0.to_string())
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
                .bind(key_id.0.to_string())
                .bind(signature)
                .execute(&mut *connection)
                .await?;
            }
        }

        Ok(())
    }

    fn get_pickle_mode(&self) -> PicklingMode {
        match &self.pickle_passphrase {
            Some(p) => PicklingMode::Encrypted {
                key: p.as_bytes().to_vec(),
            },
            None => PicklingMode::Unencrypted,
        }
    }
}

#[async_trait]
impl CryptoStore for SqliteStore {
    async fn load_account(&mut self) -> Result<Option<Account>> {
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
            let account = Account::from_pickle(
                pickle,
                self.get_pickle_mode(),
                shared,
                uploaded_key_count,
                &self.user_id,
                &self.device_id,
            )?;

            self.account_info = Some(AccountInfo {
                account_id: id,
                identity_keys: account.identity_keys.clone(),
            });

            Some(account)
        } else {
            return Ok(None);
        };

        drop(connection);

        let mut group_sessions = self.load_inbound_group_sessions().await?;

        group_sessions
            .drain(..)
            .map(|s| {
                self.inbound_group_sessions.add(s);
            })
            .for_each(drop);

        let devices = self.load_devices().await?;
        self.devices = devices;

        let (tracked_users, users_for_query) = self.load_tracked_users().await?;
        self.tracked_users = tracked_users;
        self.users_for_key_query = users_for_query;

        Ok(result)
    }

    async fn save_account(&mut self, account: Account) -> Result<()> {
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
        .bind(&*self.user_id.to_string())
        .bind(&*self.device_id.to_string())
        .bind(&pickle)
        .bind(account.shared())
        .bind(account.uploaded_key_count())
        .execute(&mut *connection)
        .await?;

        let account_id: (i64,) =
            query_as("SELECT id FROM accounts WHERE user_id = ? and device_id = ?")
                .bind(&*self.user_id.to_string())
                .bind(&*self.device_id.to_string())
                .fetch_one(&mut *connection)
                .await?;

        self.account_info = Some(AccountInfo {
            account_id: account_id.0,
            identity_keys: account.identity_keys.clone(),
        });

        Ok(())
    }

    async fn save_sessions(&mut self, sessions: &[Session]) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;

        // TODO turn this into a transaction
        for session in sessions {
            self.lazy_load_sessions(&session.sender_key).await?;
            self.sessions.add(session.clone()).await;

            let session_id = session.session_id();
            let creation_time = serde_json::to_string(&session.creation_time.elapsed())?;
            let last_use_time = serde_json::to_string(&session.last_use_time.elapsed())?;
            let pickle = session.pickle(self.get_pickle_mode()).await;

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
            .bind(&*session.sender_key)
            .bind(&pickle)
            .execute(&mut *connection)
            .await?;
        }

        Ok(())
    }

    async fn get_sessions(&mut self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        Ok(self.get_sessions_for(sender_key).await?)
    }

    async fn save_inbound_group_session(&mut self, session: InboundGroupSession) -> Result<bool> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let pickle = session.pickle(self.get_pickle_mode()).await;
        let mut connection = self.connection.lock().await;
        let session_id = session.session_id();

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
        .bind(&*session.sender_key)
        .bind(&*session.signing_key)
        .bind(&*session.room_id.to_string())
        .bind(&pickle)
        .execute(&mut *connection)
        .await?;

        Ok(self.inbound_group_sessions.add(session))
    }

    async fn get_inbound_group_session(
        &mut self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        Ok(self
            .inbound_group_sessions
            .get(room_id, sender_key, session_id))
    }

    fn tracked_users(&self) -> &HashSet<UserId> {
        &self.tracked_users
    }

    fn users_for_key_query(&self) -> &HashSet<UserId> {
        &self.users_for_key_query
    }

    async fn update_tracked_user(&mut self, user: &UserId, dirty: bool) -> Result<bool> {
        let already_added = self.tracked_users.insert(user.clone());

        if dirty {
            self.users_for_key_query.insert(user.clone());
        } else {
            self.users_for_key_query.remove(user);
        }

        self.save_tracked_user(user, dirty).await?;

        Ok(already_added)
    }

    async fn save_devices(&self, devices: &[Device]) -> Result<()> {
        // TODO turn this into a bulk transaction.
        for device in devices {
            self.devices.add(device.clone());
            self.save_device_helper(device.clone()).await?
        }

        Ok(())
    }

    async fn delete_device(&self, device: Device) -> Result<()> {
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

    #[allow(clippy::ptr_arg)]
    async fn get_device(&self, user_id: &UserId, device_id: &DeviceId) -> Result<Option<Device>> {
        Ok(self.devices.get(user_id, device_id))
    }

    async fn get_user_devices(&self, user_id: &UserId) -> Result<UserDevices> {
        Ok(self.devices.user_devices(user_id))
    }
}

// #[cfg_attr(tarpaulin, skip)]
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
    use crate::{device::test::get_device, olm::GroupSessionKey};
    use matrix_sdk_common::{
        api::r0::keys::SignedKey,
        identifiers::{DeviceId, UserId},
    };
    use olm_rs::outbound_group_session::OlmOutboundGroupSession;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    use super::{Account, CryptoStore, InboundGroupSession, RoomId, Session, SqliteStore, TryFrom};

    fn example_user_id() -> UserId {
        UserId::try_from("@example:localhost").unwrap()
    }

    fn example_device_id() -> &'static DeviceId {
        "DEVICEID".into()
    }

    async fn get_store(passphrase: Option<&str>) -> (SqliteStore, tempfile::TempDir) {
        let tmpdir = tempdir().unwrap();
        let tmpdir_path = tmpdir.path().to_str().unwrap();

        let user_id = &example_user_id();
        let device_id = example_device_id();

        let store = if let Some(passphrase) = passphrase {
            SqliteStore::open_with_passphrase(&user_id, device_id, tmpdir_path, passphrase)
                .await
                .expect("Can't create a passphrase protected store")
        } else {
            SqliteStore::open(&user_id, device_id, tmpdir_path)
                .await
                .expect("Can't create store")
        };

        (store, tmpdir)
    }

    async fn get_loaded_store() -> (Account, SqliteStore, tempfile::TempDir) {
        let (mut store, dir) = get_store(None).await;
        let account = get_account();
        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        (account, store, dir)
    }

    fn alice_id() -> UserId {
        UserId::try_from("@alice:example.org").unwrap()
    }

    fn alice_device_id() -> Box<DeviceId> {
        "ALICEDEVICE".into()
    }

    fn bob_id() -> UserId {
        UserId::try_from("@bob:example.org").unwrap()
    }

    fn bob_device_id() -> Box<DeviceId> {
        "BOBDEVICE".into()
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
        let _ = SqliteStore::open(&example_user_id(), "DEVICEID".into(), tmpdir_path)
            .await
            .expect("Can't create store");
    }

    #[tokio::test]
    async fn save_account() {
        let (mut store, _dir) = get_store(None).await;
        assert!(store.load_account().await.unwrap().is_none());
        let account = get_account();

        store
            .save_account(account)
            .await
            .expect("Can't save account");
    }

    #[tokio::test]
    async fn load_account() {
        let (mut store, _dir) = get_store(None).await;
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
        let (mut store, _dir) = get_store(Some("secret_passphrase")).await;
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
        let (mut store, _dir) = get_store(None).await;
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
        let (mut store, _dir) = get_store(None).await;
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
        let (mut store, _dir) = get_store(None).await;
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
        let (mut store, dir) = get_store(None).await;
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

        let mut store = SqliteStore::open(&example_user_id(), example_device_id(), dir.path())
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
        let (account, mut store, _dir) = get_loaded_store().await;

        let identity_keys = account.identity_keys();
        let outbound_session = OlmOutboundGroupSession::new();
        let session = InboundGroupSession::new(
            identity_keys.curve25519(),
            identity_keys.ed25519(),
            &RoomId::try_from("!test:localhost").unwrap(),
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
        let (account, mut store, _dir) = get_loaded_store().await;

        let identity_keys = account.identity_keys();
        let outbound_session = OlmOutboundGroupSession::new();
        let session = InboundGroupSession::new(
            identity_keys.curve25519(),
            identity_keys.ed25519(),
            &RoomId::try_from("!test:localhost").unwrap(),
            GroupSessionKey(outbound_session.session_key()),
        )
        .expect("Can't create session");

        let session_id = session.session_id().to_owned();

        store
            .save_inbound_group_session(session.clone())
            .await
            .expect("Can't save group session");

        let sessions = store.load_inbound_group_sessions().await.unwrap();

        assert_eq!(session_id, sessions[0].session_id());

        let loaded_session = store
            .get_inbound_group_session(&session.room_id, &session.sender_key, session.session_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session, loaded_session);
    }

    #[tokio::test]
    async fn test_tracked_users() {
        let (_account, mut store, dir) = get_loaded_store().await;
        let device = get_device();

        assert!(store
            .update_tracked_user(device.user_id(), false)
            .await
            .unwrap());
        assert!(!store
            .update_tracked_user(device.user_id(), false)
            .await
            .unwrap());

        let tracked_users = store.tracked_users();

        assert!(tracked_users.contains(device.user_id()));
        assert!(!store.users_for_key_query().contains(device.user_id()));
        assert!(!store
            .update_tracked_user(device.user_id(), true)
            .await
            .unwrap());
        assert!(store.users_for_key_query().contains(device.user_id()));
        drop(store);

        let mut store = SqliteStore::open(&example_user_id(), example_device_id(), dir.path())
            .await
            .expect("Can't create store");

        store.load_account().await.unwrap();

        let tracked_users = store.tracked_users();
        assert!(tracked_users.contains(device.user_id()));
        assert!(store.users_for_key_query().contains(device.user_id()));

        store
            .update_tracked_user(device.user_id(), false)
            .await
            .unwrap();
        assert!(!store.users_for_key_query().contains(device.user_id()));

        let mut store = SqliteStore::open(&example_user_id(), example_device_id(), dir.path())
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

        let mut store = SqliteStore::open(&example_user_id(), example_device_id(), dir.path())
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

        let mut store = SqliteStore::open(&example_user_id(), example_device_id(), dir.path())
            .await
            .expect("Can't create store");

        store.load_account().await.unwrap();

        let loaded_device = store
            .get_device(device.user_id(), device.device_id())
            .await
            .unwrap();

        assert!(loaded_device.is_none());
    }
}
