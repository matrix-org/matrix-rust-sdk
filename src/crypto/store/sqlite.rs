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

use std::path::{Path, PathBuf};
use std::result::Result as StdResult;
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

use async_trait::async_trait;
use olm_rs::PicklingMode;
use serde_json;
use sqlx::{query, query_as, sqlite::SqliteQueryAs, Connect, Executor, SqliteConnection};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

use super::{Account, CryptoStore, CryptoStoreError, InboundGroupSession, Result, Session};
use crate::crypto::memory_stores::{GroupSessionStore, SessionStore};

pub struct SqliteStore {
    user_id: Arc<String>,
    device_id: Arc<String>,
    account_id: Option<i64>,
    path: PathBuf,
    sessions: SessionStore,
    inbound_group_sessions: GroupSessionStore,
    connection: Arc<Mutex<SqliteConnection>>,
    pickle_passphrase: Option<Zeroizing<String>>,
}

static DATABASE_NAME: &str = "matrix-sdk-crypto.db";

impl SqliteStore {
    pub async fn open<P: AsRef<Path>>(
        user_id: &str,
        device_id: &str,
        path: P,
    ) -> Result<SqliteStore> {
        SqliteStore::open_helper(user_id, device_id, path, None).await
    }

    pub async fn open_with_passphrase<P: AsRef<Path>>(
        user_id: &str,
        device_id: &str,
        path: P,
        passphrase: String,
    ) -> Result<SqliteStore> {
        SqliteStore::open_helper(user_id, device_id, path, Some(Zeroizing::new(passphrase))).await
    }

    fn path_to_url(path: &Path) -> Result<Url> {
        // TODO this returns an empty error if the path isn't absolute.
        let url = Url::from_directory_path(path).expect("Invalid path");
        Ok(url.join(DATABASE_NAME)?)
    }

    async fn open_helper<P: AsRef<Path>>(
        user_id: &str,
        device_id: &str,
        path: P,
        passphrase: Option<Zeroizing<String>>,
    ) -> Result<SqliteStore> {
        let url = SqliteStore::path_to_url(path.as_ref())?;

        let connection = SqliteConnection::connect(url.as_ref()).await?;
        let store = SqliteStore {
            user_id: Arc::new(user_id.to_owned()),
            device_id: Arc::new(device_id.to_owned()),
            account_id: None,
            sessions: SessionStore::new(),
            inbound_group_sessions: GroupSessionStore::new(),
            path: path.as_ref().to_owned(),
            connection: Arc::new(Mutex::new(connection)),
            pickle_passphrase: passphrase,
        };
        store.create_tables().await?;
        Ok(store)
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

            CREATE INDEX "olmsessions_account_id" ON "sessions" ("account_id");
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

            CREATE INDEX "olm_groups_sessions_account_id" ON "inbound_group_sessions" ("account_id");
        "#,
            )
            .await?;

        Ok(())
    }

    async fn get_sessions_for(
        &mut self,
        sender_key: &str,
    ) -> Result<Option<Arc<Mutex<Vec<Arc<Mutex<Session>>>>>>> {
        let loaded_sessions = self.sessions.get(sender_key).is_some();

        if !loaded_sessions {
            let sessions = self.load_sessions_for(sender_key).await?;

            if !sessions.is_empty() {
                self.sessions.set_for_sender(sender_key, sessions);
            }
        }

        Ok(self.sessions.get(sender_key))
    }

    async fn load_sessions_for(&mut self, sender_key: &str) -> Result<Vec<Arc<Mutex<Session>>>> {
        let account_id = self.account_id.ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let rows: Vec<(String, String, String, String)> = query_as(
            "SELECT pickle, sender_key, creation_time, last_use_time
             FROM sessions WHERE account_id = ? and sender_key = ?",
        )
        .bind(account_id)
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

                Ok(Arc::new(Mutex::new(Session::from_pickle(
                    pickle.to_string(),
                    self.get_pickle_mode(),
                    sender_key.to_string(),
                    creation_time,
                    last_use_time,
                )?)))
            })
            .collect::<Result<Vec<Arc<Mutex<Session>>>>>()?)
    }

    async fn load_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        let account_id = self.account_id.ok_or(CryptoStoreError::AccountUnset)?;
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
                    room_id.to_owned(),
                )?)
            })
            .collect::<Result<Vec<InboundGroupSession>>>()?)
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

        let row: Option<(i64, String, bool)> = query_as(
            "SELECT id, pickle, shared FROM accounts
                      WHERE user_id = ? and device_id = ?",
        )
        .bind(&*self.user_id)
        .bind(&*self.device_id)
        .fetch_optional(&mut *connection)
        .await?;

        let result = match row {
            Some((id, pickle, shared)) => {
                self.account_id = Some(id);
                Some(Account::from_pickle(
                    pickle,
                    self.get_pickle_mode(),
                    shared,
                )?)
            }
            None => None,
        };

        drop(connection);

        let mut sessions = self.load_inbound_group_sessions().await?;

        let _ = sessions
            .drain(..)
            .map(|s| {
                self.inbound_group_sessions.add(s);
            })
            .collect::<()>();

        Ok(result)
    }

    async fn save_account(&mut self, account: Arc<Mutex<Account>>) -> Result<()> {
        let acc = account.lock().await;
        let pickle = acc.pickle(self.get_pickle_mode());
        let mut connection = self.connection.lock().await;

        query(
            "INSERT INTO accounts (
                user_id, device_id, pickle, shared
             ) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(user_id, device_id) DO UPDATE SET
                pickle = ?3,
                shared = ?4
             WHERE user_id = ?1 and
                  device_id = ?2
             ",
        )
        .bind(&*self.user_id)
        .bind(&*self.device_id)
        .bind(&pickle)
        .bind(acc.shared)
        .execute(&mut *connection)
        .await?;

        let account_id: (i64,) =
            query_as("SELECT id FROM accounts WHERE user_id = ? and device_id = ?")
                .bind(&*self.user_id)
                .bind(&*self.device_id)
                .fetch_one(&mut *connection)
                .await?;

        self.account_id = Some(account_id.0);

        Ok(())
    }

    async fn save_session(&mut self, session: Arc<Mutex<Session>>) -> Result<()> {
        let account_id = self.account_id.ok_or(CryptoStoreError::AccountUnset)?;

        let session = session.lock().await;

        let session_id = session.session_id();
        let creation_time = serde_json::to_string(&session.creation_time.elapsed())?;
        let last_use_time = serde_json::to_string(&session.last_use_time.elapsed())?;
        let pickle = session.pickle(self.get_pickle_mode());

        let mut connection = self.connection.lock().await;

        query(
            "REPLACE INTO sessions (
                session_id, account_id, creation_time, last_use_time, sender_key, pickle
             ) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&session_id)
        .bind(&account_id)
        .bind(&creation_time)
        .bind(&last_use_time)
        .bind(&session.sender_key)
        .bind(&pickle)
        .execute(&mut *connection)
        .await?;

        Ok(())
    }

    async fn add_and_save_session(&mut self, session: Session) -> Result<()> {
        let session = self.sessions.add(session).await;
        self.save_session(session).await?;
        Ok(())
    }

    async fn get_sessions(
        &mut self,
        sender_key: &str,
    ) -> Result<Option<Arc<Mutex<Vec<Arc<Mutex<Session>>>>>>> {
        Ok(self.get_sessions_for(sender_key).await?)
    }

    async fn save_inbound_group_session(&mut self, session: InboundGroupSession) -> Result<bool> {
        let account_id = self.account_id.ok_or(CryptoStoreError::AccountUnset)?;
        let pickle = session.pickle(self.get_pickle_mode());
        let mut connection = self.connection.lock().await;
        let session_id = session.session_id();

        query(
            "INSERT INTO inbound_group_sessions (
                session_id, account_id, sender_key, signing_key,
                room_id, pickle
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id) DO UPDATE SET
                pickle = ?6
             WHERE session_id = ?1
             ",
        )
        .bind(session_id)
        .bind(account_id)
        .bind(&session.sender_key)
        .bind(&session.signing_key)
        .bind(&session.room_id)
        .bind(&pickle)
        .execute(&mut *connection)
        .await?;

        Ok(self.inbound_group_sessions.add(session))
    }

    async fn get_inbound_group_session(
        &mut self,
        room_id: &str,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<Arc<Mutex<InboundGroupSession>>>> {
        Ok(self
            .inbound_group_sessions
            .get(room_id, sender_key, session_id))
    }
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> StdResult<(), std::fmt::Error> {
        write!(
            fmt,
            "SqliteStore {{ user_id: {}, device_id: {}, path: {:?} }}",
            self.user_id, self.device_id, self.path
        )
    }
}

#[cfg(test)]
mod test {
    use olm_rs::outbound_group_session::OlmOutboundGroupSession;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    use super::{Account, CryptoStore, InboundGroupSession, Session, SqliteStore};

    static USER_ID: &str = "@example:localhost";
    static DEVICE_ID: &str = "DEVICEID";

    async fn get_store() -> SqliteStore {
        let tmpdir = tempdir().unwrap();
        let tmpdir_path = tmpdir.path().to_str().unwrap();
        SqliteStore::open(USER_ID, DEVICE_ID, tmpdir_path)
            .await
            .expect("Can't create store")
    }

    async fn get_loaded_store() -> (Arc<Mutex<Account>>, SqliteStore) {
        let mut store = get_store().await;
        let account = get_account();
        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        (account, store)
    }

    fn get_account() -> Arc<Mutex<Account>> {
        let account = Account::new();
        Arc::new(Mutex::new(account))
    }

    fn get_account_and_session() -> (Arc<Mutex<Account>>, Session) {
        let alice = Account::new();

        let bob = Account::new();

        bob.generate_one_time_keys(1);
        let one_time_key = bob
            .one_time_keys()
            .curve25519()
            .iter()
            .nth(0)
            .unwrap()
            .1
            .to_owned();
        let sender_key = bob.identity_keys().curve25519().to_owned();
        let session = alice
            .create_outbound_session(&sender_key, &one_time_key)
            .unwrap();

        (Arc::new(Mutex::new(alice)), session)
    }

    #[tokio::test]
    async fn create_store() {
        let tmpdir = tempdir().unwrap();
        let tmpdir_path = tmpdir.path().to_str().unwrap();
        let _ = SqliteStore::open("@example:localhost", "DEVICEID", tmpdir_path)
            .await
            .expect("Can't create store");
    }

    #[tokio::test]
    async fn save_account() {
        let mut store = get_store().await;
        let account = get_account();

        store
            .save_account(account)
            .await
            .expect("Can't save account");
    }

    #[tokio::test]
    async fn load_account() {
        let mut store = get_store().await;
        let account = get_account();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        let acc = account.lock().await;
        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(*acc, loaded_account);
    }

    #[tokio::test]
    async fn save_and_share_account() {
        let mut store = get_store().await;
        let account = get_account();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        account.lock().await.shared = true;

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();
        let acc = account.lock().await;

        assert_eq!(*acc, loaded_account);
    }

    #[tokio::test]
    async fn save_session() {
        let mut store = get_store().await;
        let (account, session) = get_account_and_session();
        let session = Arc::new(Mutex::new(session));

        assert!(store.save_session(session.clone()).await.is_err());

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        store.save_session(session).await.unwrap();
    }

    #[tokio::test]
    async fn load_sessions() {
        let mut store = get_store().await;
        let (account, session) = get_account_and_session();
        let session = Arc::new(Mutex::new(session));
        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");
        store.save_session(session.clone()).await.unwrap();

        let sess = session.lock().await;

        let sessions = store
            .load_sessions_for(&sess.sender_key)
            .await
            .expect("Can't load sessions");
        let loaded_session = &sessions[0];

        assert_eq!(*sess, *loaded_session.lock().await);
    }

    #[tokio::test]
    async fn add_and_save_session() {
        let mut store = get_store().await;
        let (account, session) = get_account_and_session();
        let sender_key = session.sender_key.to_owned();
        let session_id = session.session_id();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");
        store.add_and_save_session(session).await.unwrap();

        let sessions = store.get_sessions(&sender_key).await.unwrap().unwrap();
        let sessions_lock = sessions.lock().await;
        let session = &sessions_lock[0];

        assert_eq!(session_id, *session.lock().await.session_id());
    }

    #[tokio::test]
    async fn save_inbound_group_session() {
        let (account, mut store) = get_loaded_store().await;

        let acc = account.lock().await;
        let identity_keys = acc.identity_keys();
        let outbound_session = OlmOutboundGroupSession::new();
        let session = InboundGroupSession::new(
            identity_keys.curve25519(),
            identity_keys.ed25519(),
            "!test:localhost",
            &outbound_session.session_key(),
        )
        .expect("Can't create session");

        store
            .save_inbound_group_session(session)
            .await
            .expect("Can't save group session");
    }

    #[tokio::test]
    async fn load_inbound_group_session() {
        let (account, mut store) = get_loaded_store().await;

        let acc = account.lock().await;
        let identity_keys = acc.identity_keys();
        let outbound_session = OlmOutboundGroupSession::new();
        let session = InboundGroupSession::new(
            identity_keys.curve25519(),
            identity_keys.ed25519(),
            "!test:localhost",
            &outbound_session.session_key(),
        )
        .expect("Can't create session");

        let session_id = session.session_id();

        store
            .save_inbound_group_session(session)
            .await
            .expect("Can't save group session");

        let sessions = store.load_inbound_group_sessions().await.unwrap();

        assert_eq!(session_id, sessions[0].session_id());
    }
}
